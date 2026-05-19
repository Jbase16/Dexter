//! dexter-cli — non-interactive gRPC client for the Dexter daemon.
//!
//! Phase 38: built so live-smoke regression tests can run from Bash without an
//! operator at the keyboard, and so future Phase 38b work on structured action
//! types has a way to send synthetic action requests for testing. Sends the
//! same `ClientEvent::TextInput` events Swift's `DexterClient` sends — every
//! test that uses typed input through the HUD can use this CLI instead.
//!
//! ## Usage
//!
//! ```bash
//! # One-shot — send a single typed input, print the response, exit on IDLE
//! cargo run --release --bin dexter-cli -- "explain how a B-tree page split works"
//!
//! # Read commands from stdin, one per line — useful for scripted smoke tests
//! printf "what's the weather in Tokyo and Sacramento?\nexplain how TCP slow-start works\n" | \
//!   cargo run --release --bin dexter-cli
//!
//! # Override the socket path (e.g. for a test daemon on a sandbox socket)
//! dexter-cli --socket /tmp/dexter-test.sock "what time is it"
//!
//! # Auto-respond to destructive-action approval prompts (default: deny)
//! dexter-cli --auto-approve "pkill Slack"
//! dexter-cli --auto-deny "rm -rf /tmp/foo"   # explicit deny (same as default)
//!
//! # Quiet mode — suppress state markers, print only the model's text response
//! dexter-cli --quiet "what's 2 plus 2"
//! ```
//!
//! ## Output format
//!
//! Default mode prints state transitions, action requests, and text responses:
//!
//! ```text
//! [STATE: Thinking]
//! 2 plus 2 equals 4.
//! [STATE: Idle]
//! [DONE]
//! ```
//!
//! `--quiet` suppresses everything except the model text. State events still
//! drive the turn-completion logic — IDLE marks turn end and CLI exits the
//! per-input loop.
//!
//! ## What this is NOT
//!
//! - Not an interactive REPL — input is one-shot or stdin-piped.
//! - Not a voice client — `from_voice` defaults to false (HUD typed-mode behavior).
//!   That means TTS is suppressed (Phase 34: typed-mode is text-only). To exercise
//!   the TTS pipeline use `--from-voice`, but you still won't HEAR audio — the CLI
//!   just sees AudioResponse frames go by.
//! - Not a HUD replacement — markdown rendering, dialog UI, animated entity all
//!   live on the Swift side. CLI prints raw text and structured event JSON.

use std::{
    fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::transport::Endpoint;
use tower::service_fn;
use uuid::Uuid;

mod proto {
    tonic::include_proto!("dexter.v1");
}

use proto::{
    client_event, dexter_service_client::DexterServiceClient, server_event, ActionApproval,
    ActionCategory, ClientEvent, EntityState, HealthRequest, HealthResponse, PingRequest,
    RestartComponent, RestartComponentRequest, SystemEvent, SystemEventType, TextInput, UiAction,
    UiActionType,
};

const DEFAULT_SOCKET: &str = "/tmp/dexter.sock";
const DEFAULT_SHELL_SOCKET: &str = "/tmp/dexter-shell.sock";
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_DEXTER_STATE_DIR: &str = ".dexter/state";
const AUDIT_LOG_FILENAME: &str = "audit.jsonl";
const OLLAMA_TAGS_PATH: &str = "/api/tags";
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;
const DEFAULT_ACTION_RECEIPT_LIMIT: usize = 10;
const DOCTOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DOCTOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalPolicy {
    /// Auto-deny destructive action requests (default — matches "operator was not present").
    Deny,
    /// Auto-approve destructive action requests (use with care; matches `--auto-approve`).
    Approve,
}

#[derive(Debug)]
struct CliConfig {
    socket_path: String,
    shell_socket_path: String,
    inputs: Vec<CliInput>,
    from_voice: bool,
    quiet: bool,
    doctor: bool,
    action_query: Option<ActionQuery>,
    action_limit: usize,
    restart_component: Option<RestartTarget>,
    approval_policy: ApprovalPolicy,
    idle_timeout: Duration,
    interrupt_on_focused_after: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionQuery {
    Last,
    Recent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartTarget {
    Stt,
    Tts,
    Browser,
}

impl RestartTarget {
    fn grpc_component(self) -> RestartComponent {
        match self {
            Self::Stt => RestartComponent::Stt,
            Self::Tts => RestartComponent::Tts,
            Self::Browser => RestartComponent::Browser,
        }
    }

    fn command_arg(self) -> &'static str {
        match self {
            Self::Stt => "stt",
            Self::Tts => "tts",
            Self::Browser => "browser",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Stt => "STT worker",
            Self::Tts => "TTS worker",
            Self::Browser => "browser worker",
        }
    }
}

#[derive(Debug, Clone)]
enum CliInput {
    Text(String),
    ActionJson(String),
    SystemEvent {
        event_type: SystemEventType,
        payload: String,
    },
    ShellCommand {
        command: String,
        cwd: String,
        exit_code: Option<i32>,
    },
}

fn parse_args() -> Result<CliConfig> {
    let mut socket_path = DEFAULT_SOCKET.to_string();
    let mut shell_socket_path = DEFAULT_SHELL_SOCKET.to_string();
    let mut from_voice = false;
    let mut quiet = false;
    let mut doctor = false;
    let mut action_query = None;
    let mut action_limit = DEFAULT_ACTION_RECEIPT_LIMIT;
    let mut restart_component = None;
    let mut approval_policy = ApprovalPolicy::Deny;
    let mut idle_timeout = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS);
    let mut interrupt_on_focused_after = None;
    let mut positional: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" | "-s" => {
                socket_path = args
                    .next()
                    .ok_or_else(|| anyhow!("--socket requires a path argument"))?;
            }
            "--shell-socket" => {
                shell_socket_path = args
                    .next()
                    .ok_or_else(|| anyhow!("--shell-socket requires a path argument"))?;
            }
            "--from-voice" => from_voice = true,
            "--quiet" | "-q" => quiet = true,
            "--doctor" => doctor = true,
            "--actions" => {
                let raw_query = args
                    .next()
                    .ok_or_else(|| anyhow!("--actions requires last or recent"))?;
                action_query = Some(parse_action_query(&raw_query)?);
            }
            "--limit" => {
                action_limit = args
                    .next()
                    .ok_or_else(|| anyhow!("--limit requires a positive integer"))?
                    .parse()
                    .context("--limit: not a valid positive integer")?;
                if action_limit == 0 {
                    return Err(anyhow!("--limit must be greater than zero"));
                }
            }
            "--restart-component" => {
                let raw_component = args
                    .next()
                    .ok_or_else(|| anyhow!("--restart-component requires stt, tts, or browser"))?;
                restart_component = Some(parse_restart_target(&raw_component)?);
            }
            "--auto-approve" | "-y" => approval_policy = ApprovalPolicy::Approve,
            "--auto-deny" | "-n" => approval_policy = ApprovalPolicy::Deny,
            "--idle-timeout" => {
                let secs: u64 = args
                    .next()
                    .ok_or_else(|| anyhow!("--idle-timeout requires a seconds argument"))?
                    .parse()
                    .context("--idle-timeout: not a valid u64 seconds value")?;
                idle_timeout = Duration::from_secs(secs);
            }
            "--interrupt-on-focused-after-ms" => {
                let millis: u64 = args
                    .next()
                    .ok_or_else(|| {
                        anyhow!("--interrupt-on-focused-after-ms requires a millisecond argument")
                    })?
                    .parse()
                    .context(
                        "--interrupt-on-focused-after-ms: not a valid u64 millisecond value",
                    )?;
                interrupt_on_focused_after = Some(Duration::from_millis(millis));
            }
            "--system-event" => {
                let raw_type = args
                    .next()
                    .ok_or_else(|| anyhow!("--system-event requires a type argument"))?;
                let payload = args.next().ok_or_else(|| {
                    anyhow!("--system-event requires a JSON payload argument after the type")
                })?;
                positional.push(format!(
                    "\u{1f}system-event\u{1f}{}\u{1f}{}",
                    raw_type, payload
                ));
            }
            "--action-json" => {
                let raw_json = args
                    .next()
                    .ok_or_else(|| anyhow!("--action-json requires an ActionSpec JSON argument"))?;
                serde_json::from_str::<serde_json::Value>(&raw_json)
                    .context("--action-json: argument is not valid JSON")?;
                positional.push(format!("\u{1f}action-json\u{1f}{raw_json}"));
            }
            "--shell-command" => {
                let command = args
                    .next()
                    .ok_or_else(|| anyhow!("--shell-command requires a command argument"))?;
                let cwd = args
                    .next()
                    .ok_or_else(|| anyhow!("--shell-command requires a cwd argument"))?;
                let raw_exit = args
                    .next()
                    .ok_or_else(|| anyhow!("--shell-command requires an exit-code argument"))?;
                positional.push(format!(
                    "\u{1f}shell-command\u{1f}{}\u{1f}{}\u{1f}{}",
                    command, cwd, raw_exit
                ));
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with('-') => {
                return Err(anyhow!("unknown flag: {other} — try --help"));
            }
            other => positional.push(other.to_string()),
        }
    }

    // If positional args supplied, use those. Otherwise read commands from stdin
    // (one per line). Empty stdin → no inputs → just exit cleanly.
    let inputs = if action_query.is_some() {
        if doctor {
            return Err(anyhow!("--actions cannot be combined with --doctor"));
        }
        if restart_component.is_some() {
            return Err(anyhow!(
                "--actions cannot be combined with --restart-component"
            ));
        }
        if !positional.is_empty() {
            return Err(anyhow!("--actions cannot be combined with input arguments"));
        }
        Vec::new()
    } else if restart_component.is_some() {
        if doctor {
            return Err(anyhow!(
                "--restart-component cannot be combined with --doctor; the restart command prints a post-restart doctor report"
            ));
        }
        if !positional.is_empty() {
            return Err(anyhow!(
                "--restart-component cannot be combined with input arguments"
            ));
        }
        Vec::new()
    } else if doctor {
        if !positional.is_empty() {
            return Err(anyhow!("--doctor cannot be combined with input arguments"));
        }
        Vec::new()
    } else if positional.is_empty() {
        // No positional args. If stdin is a TTY (interactive shell with no
        // piped input) treat as user error and show help. If stdin is piped
        // (file redirect, heredoc, another command's output), drain it.
        // `IsTerminal` is the std-library replacement for the deprecated
        // `atty` crate — stable since Rust 1.70.
        if io::stdin().is_terminal() {
            print_help();
            std::process::exit(0);
        }
        let stdin = io::stdin();
        let mut lines = Vec::new();
        for line in stdin.lock().lines() {
            let line = line.context("reading stdin")?;
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                lines.push(CliInput::Text(trimmed.to_string()));
            }
        }
        lines
    } else {
        positional
            .into_iter()
            .map(|arg| {
                if let Some(rest) = arg.strip_prefix("\u{1f}system-event\u{1f}") {
                    let mut parts = rest.splitn(2, '\u{1f}');
                    let raw_type = parts.next().unwrap_or_default();
                    let payload = parts.next().unwrap_or_default().to_string();
                    Ok(CliInput::SystemEvent {
                        event_type: parse_system_event_type(raw_type)?,
                        payload,
                    })
                } else if let Some(raw_json) = arg.strip_prefix("\u{1f}action-json\u{1f}") {
                    Ok(CliInput::ActionJson(raw_json.to_string()))
                } else if let Some(rest) = arg.strip_prefix("\u{1f}shell-command\u{1f}") {
                    let mut parts = rest.splitn(3, '\u{1f}');
                    let command = parts.next().unwrap_or_default().to_string();
                    let cwd = parts.next().unwrap_or_default().to_string();
                    let raw_exit = parts.next().unwrap_or_default();
                    Ok(CliInput::ShellCommand {
                        command,
                        cwd,
                        exit_code: parse_shell_exit_code(raw_exit)?,
                    })
                } else {
                    Ok(CliInput::Text(arg))
                }
            })
            .collect::<Result<Vec<_>>>()?
    };

    Ok(CliConfig {
        socket_path,
        shell_socket_path,
        inputs,
        from_voice,
        quiet,
        doctor,
        action_query,
        action_limit,
        restart_component,
        approval_policy,
        idle_timeout,
        interrupt_on_focused_after,
    })
}

fn print_help() {
    eprintln!("dexter-cli — non-interactive gRPC client for the Dexter daemon.");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("  dexter-cli [FLAGS] [INPUT ...]");
    eprintln!();
    eprintln!("With no INPUT arguments, reads commands from stdin (one per line).");
    eprintln!();
    eprintln!("FLAGS:");
    eprintln!("  -s, --socket <PATH>      Override gRPC socket path (default: {DEFAULT_SOCKET})");
    eprintln!("      --shell-socket <PATH>");
    eprintln!("                           Override shell-context socket path (default: {DEFAULT_SHELL_SOCKET}).");
    eprintln!("      --from-voice         Set TextInput.from_voice = true (enables TTS).");
    eprintln!("                           Default false matches HUD typed-input mode.");
    eprintln!("  -q, --quiet              Suppress state markers — only print model text.");
    eprintln!("      --doctor             Run lightweight diagnostics without opening a");
    eprintln!("                           session stream or generating model output.");
    eprintln!("      --actions <last|recent>");
    eprintln!("                           Print action receipts from the local audit log.");
    eprintln!(
        "      --limit <N>          Limit --actions recent output (default: {DEFAULT_ACTION_RECEIPT_LIMIT})."
    );
    eprintln!("      --restart-component <stt|tts|browser>");
    eprintln!("                           Ask the daemon to restart one shared worker, then");
    eprintln!("                           print a post-restart doctor report.");
    eprintln!("  -y, --auto-approve       Auto-approve destructive action requests.");
    eprintln!("  -n, --auto-deny          Auto-deny destructive action requests (default).");
    eprintln!("      --idle-timeout <S>   Wait at most S seconds for IDLE before next turn (default: {DEFAULT_IDLE_TIMEOUT_SECS}).");
    eprintln!("      --interrupt-on-focused-after-ms <MS>");
    eprintln!("                           After a turn reaches FOCUSED, send HotkeyActivated");
    eprintln!("                           after MS milliseconds and finish the turn on LISTENING.");
    eprintln!("                           Intended for action-cancellation smoke tests.");
    eprintln!("      --system-event <TYPE> <JSON>");
    eprintln!("                           Send a synthetic SystemEvent before/among text inputs.");
    eprintln!(
        "                           TYPE examples: connected, app_focused, ax_element_changed,"
    );
    eprintln!("                           clipboard_changed, app_unfocused, screen_locked.");
    eprintln!("      --action-json <JSON> Send an exact ActionSpec through the dev-only");
    eprintln!("                           synthetic action path; useful for deterministic");
    eprintln!("                           action approval smoke tests.");
    eprintln!("      --shell-command <COMMAND> <CWD> <EXIT_CODE|null>");
    eprintln!("                           Send a synthetic shell-completion event through the");
    eprintln!("                           same /tmp/dexter-shell.sock path as the zsh hook.");
    eprintln!("  -h, --help               Show this help and exit.");
    eprintln!();
    eprintln!("EXAMPLES:");
    eprintln!("  dexter-cli \"what's 2 plus 2\"");
    eprintln!("  dexter-cli --doctor");
    eprintln!("  dexter-cli --actions last");
    eprintln!("  dexter-cli --actions recent --limit 20");
    eprintln!("  dexter-cli --restart-component tts");
    eprintln!("  dexter-cli --quiet \"explain how TCP slow-start works\"");
    eprintln!("  printf \"q1\\nq2\\n\" | dexter-cli");
    eprintln!("  dexter-cli --auto-deny \"rm -rf /tmp/foo\"");
    eprintln!("  dexter-cli --action-json '{{\"type\":\"shell\",\"args\":[\"echo\",\"hi\"]}}'");
    eprintln!("  dexter-cli --system-event clipboard_changed '{{\"text\":\"copied\"}}' \"summarize clipboard\"");
    eprintln!("  dexter-cli --shell-command \"cargo test\" /Users/me/project 0 \"what happened?\"");
}

fn parse_action_query(raw: &str) -> Result<ActionQuery> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "last" => Ok(ActionQuery::Last),
        "recent" => Ok(ActionQuery::Recent),
        other => Err(anyhow!(
            "unknown action receipt query: {other}; expected last or recent"
        )),
    }
}

fn parse_restart_target(raw: &str) -> Result<RestartTarget> {
    let normalized = raw.trim().replace(['-', ' '], "_").to_ascii_lowercase();
    match normalized.as_str() {
        "stt" | "speech_to_text" | "speech_text" => Ok(RestartTarget::Stt),
        "tts" | "text_to_speech" | "voice" => Ok(RestartTarget::Tts),
        "browser" | "browser_worker" | "playwright" => Ok(RestartTarget::Browser),
        other => Err(anyhow!(
            "unknown restart component: {other}; expected stt, tts, or browser"
        )),
    }
}

fn parse_system_event_type(raw: &str) -> Result<SystemEventType> {
    let normalized = raw
        .trim()
        .trim_start_matches("SYSTEM_EVENT_TYPE_")
        .replace(['-', ' '], "_")
        .to_ascii_lowercase();
    match normalized.as_str() {
        "connected" => Ok(SystemEventType::Connected),
        "app_focused" => Ok(SystemEventType::AppFocused),
        "app_unfocused" => Ok(SystemEventType::AppUnfocused),
        "screen_locked" => Ok(SystemEventType::ScreenLocked),
        "ax_element_changed" => Ok(SystemEventType::AxElementChanged),
        "screen_unlocked" => Ok(SystemEventType::ScreenUnlocked),
        "hotkey_activated" => Ok(SystemEventType::HotkeyActivated),
        "audio_playback_complete" => Ok(SystemEventType::AudioPlaybackComplete),
        "clipboard_changed" => Ok(SystemEventType::ClipboardChanged),
        other => Err(anyhow!("unknown SystemEventType: {other}")),
    }
}

fn parse_shell_exit_code(raw: &str) -> Result<Option<i32>> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("null") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let exit_code = trimmed
        .parse::<i32>()
        .with_context(|| format!("invalid shell exit code: {trimmed:?}"))?;
    Ok(Some(exit_code))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "OK",
            DoctorStatus::Warn => "WARN",
            DoctorStatus::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorCheck {
    status: DoctorStatus,
    name: String,
    detail: String,
}

impl DoctorCheck {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: DoctorStatus::Ok,
            name: name.into(),
            detail: detail.into(),
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: DoctorStatus::Warn,
            name: name.into(),
            detail: detail.into(),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: DoctorStatus::Fail,
            name: name.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct DoctorFileConfig {
    #[serde(default)]
    core: DoctorCoreFileConfig,
    #[serde(default)]
    inference: DoctorInferenceFileConfig,
}

#[derive(Debug, Deserialize, Default)]
struct DoctorCoreFileConfig {
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
struct DoctorInferenceFileConfig {
    ollama_base_url: Option<String>,
}

async fn run_doctor(cfg: &CliConfig) -> Result<i32> {
    let mut checks = Vec::new();

    let (config_check, ollama_base_url) = load_doctor_config();
    checks.push(config_check);
    checks.extend(check_binary_neighbors());
    checks.push(check_path_exists(
        "core socket file",
        Path::new(&cfg.socket_path),
        DoctorStatus::Fail,
    ));
    checks.push(check_path_exists(
        "shell socket file",
        Path::new(&cfg.shell_socket_path),
        DoctorStatus::Warn,
    ));
    checks.push(check_daemon_ping(&cfg.socket_path).await);
    checks.extend(check_daemon_health(&cfg.socket_path).await);
    checks.push(check_ollama(ollama_base_url.as_deref()).await);

    print_doctor_report(&checks);
    Ok(doctor_exit_code(&checks))
}

async fn run_restart_component(cfg: &CliConfig, target: RestartTarget) -> Result<i32> {
    let mut client = match connect(&cfg.socket_path).await {
        Ok(client) => client,
        Err(error) => {
            print_daemon_connection_hint(&cfg.socket_path, "restart a component", &error);
            return Ok(2);
        }
    };
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        client.restart_component(RestartComponentRequest {
            trace_id: Uuid::new_v4().to_string(),
            component: target.grpc_component() as i32,
        }),
    )
    .await
    .context("restart_component RPC timed out")?
    .context("restart_component RPC failed")?
    .into_inner();

    println!("Dexter Component Restart");
    println!();
    let restart_check = if response.success {
        DoctorCheck::ok(
            format!("restart {}", target.command_arg()),
            response.message.clone(),
        )
    } else {
        DoctorCheck::fail(
            format!("restart {}", target.command_arg()),
            response.message.clone(),
        )
    };
    println!("{}", format_doctor_check(&restart_check));
    println!();

    let mut checks = match response.health {
        Some(health) => daemon_health_checks(health),
        None => vec![DoctorCheck::fail(
            "daemon health",
            "restart response did not include post-restart health snapshot",
        )],
    };
    let (_config_check, ollama_base_url) = load_doctor_config();
    checks.push(check_ollama(ollama_base_url.as_deref()).await);

    println!("Post-Restart Doctor");
    println!();
    for check in &checks {
        println!("{}", format_doctor_check(check));
    }
    let suggestions = suggested_recovery_commands(&checks);
    if !suggestions.is_empty() {
        println!();
        print_recovery_suggestions(&suggestions);
    }
    println!();

    let post_restart_exit = doctor_exit_code(&checks);
    if response.success && post_restart_exit == 0 {
        println!("Result: OK - {} recovered.", target.label());
        Ok(0)
    } else {
        println!(
            "Result: FAIL - {} did not recover cleanly; inspect daemon logs.",
            target.label()
        );
        Ok(1)
    }
}

fn run_action_receipts(cfg: &CliConfig, query: ActionQuery) -> Result<i32> {
    let state_dir = load_action_state_dir()?;
    let audit_path = state_dir.join(AUDIT_LOG_FILENAME);
    let limit = match query {
        ActionQuery::Last => 1,
        ActionQuery::Recent => cfg.action_limit,
    };
    let receipts = read_action_receipts(&audit_path, limit)?;
    print_action_receipts(&audit_path, &receipts);
    Ok(0)
}

#[derive(Debug, Deserialize)]
struct AuditEntryOwned {
    timestamp: String,
    action_id: String,
    #[serde(rename = "type")]
    action_type: String,
    category: String,
    spec_json: serde_json::Value,
    outcome: String,
    exit_code: Option<i32>,
    output_preview: Option<String>,
    error: Option<String>,
    duration_ms: Option<u64>,
    operator_approved: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionReceipt {
    timestamp: String,
    action_id: String,
    action_type: String,
    category: String,
    target: String,
    status: String,
    approval: String,
    result: String,
    duration_ms: Option<u64>,
}

fn read_action_receipts(audit_path: &Path, limit: usize) -> Result<Vec<ActionReceipt>> {
    if !audit_path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(audit_path)
        .with_context(|| format!("failed to read {}", audit_path.display()))?;
    let mut entries = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: AuditEntryOwned = serde_json::from_str(trimmed).with_context(|| {
            format!(
                "failed to parse action audit line {} in {}",
                index + 1,
                audit_path.display()
            )
        })?;
        entries.push(action_receipt_from_audit(entry));
    }

    Ok(entries.into_iter().rev().take(limit).collect())
}

fn action_receipt_from_audit(entry: AuditEntryOwned) -> ActionReceipt {
    let target = action_target(&entry.action_type, &entry.spec_json);
    let status = action_status(&entry.outcome, entry.operator_approved);
    let approval = action_approval_label(&entry.category, entry.operator_approved);
    let result = action_result_summary(
        &entry.outcome,
        entry.operator_approved,
        entry.exit_code,
        entry.output_preview.as_deref(),
        entry.error.as_deref(),
    );

    ActionReceipt {
        timestamp: empty_to_unknown(entry.timestamp),
        action_id: empty_to_unknown(entry.action_id),
        action_type: empty_to_unknown(entry.action_type),
        category: empty_to_unknown(entry.category),
        target,
        status,
        approval,
        result,
        duration_ms: entry.duration_ms,
    }
}

fn action_target(action_type: &str, spec: &serde_json::Value) -> String {
    match action_type {
        "shell" => {
            let args = spec
                .get("args")
                .and_then(|value| value.as_array())
                .map(|args| {
                    args.iter()
                        .filter_map(|arg| arg.as_str())
                        .map(shell_display_arg)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "shell command".to_string());
            match spec.get("working_dir").and_then(|value| value.as_str()) {
                Some(dir) if !dir.trim().is_empty() => format!("{args}  (cwd: {dir})"),
                _ => args,
            }
        }
        "file_read" | "file_write" => spec
            .get("path")
            .and_then(|value| value.as_str())
            .map(one_line)
            .unwrap_or_else(|| "file path unavailable".to_string()),
        "applescript" => spec
            .get("rationale")
            .and_then(|value| value.as_str())
            .map(one_line)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "AppleScript".to_string()),
        "message_send" => spec
            .get("recipient")
            .and_then(|value| value.as_str())
            .map(|recipient| format!("iMessage to {}", one_line(recipient)))
            .unwrap_or_else(|| "iMessage recipient unavailable".to_string()),
        "browser" => browser_action_target(spec),
        _ => "target unavailable".to_string(),
    }
}

fn browser_action_target(spec: &serde_json::Value) -> String {
    let Some(browser) = spec.get("browser") else {
        return "browser action".to_string();
    };
    let action = browser
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or("browser");
    match action {
        "navigate" => browser
            .get("url")
            .and_then(|value| value.as_str())
            .map(|url| format!("navigate {}", one_line(url)))
            .unwrap_or_else(|| "navigate".to_string()),
        "click" => browser
            .get("selector")
            .and_then(|value| value.as_str())
            .map(|selector| format!("click {}", one_line(selector)))
            .unwrap_or_else(|| "click".to_string()),
        "type" => browser
            .get("selector")
            .and_then(|value| value.as_str())
            .map(|selector| format!("type into {}", one_line(selector)))
            .unwrap_or_else(|| "type".to_string()),
        "extract" => browser
            .get("selector")
            .and_then(|value| value.as_str())
            .map(|selector| format!("extract {}", one_line(selector)))
            .unwrap_or_else(|| "extract page".to_string()),
        "screenshot" => "screenshot".to_string(),
        other => one_line(other),
    }
}

fn action_status(outcome: &str, operator_approved: Option<bool>) -> String {
    match (outcome, operator_approved) {
        ("success", _) => "executed".to_string(),
        ("timeout", _) => "failed".to_string(),
        ("failure", _) => "failed".to_string(),
        ("rejected", Some(false)) => "denied".to_string(),
        ("rejected", None) => "abandoned".to_string(),
        ("rejected", Some(true)) => "failed".to_string(),
        (other, _) => one_line(other),
    }
}

fn action_approval_label(category: &str, operator_approved: Option<bool>) -> String {
    match operator_approved {
        Some(true) => "approved".to_string(),
        Some(false) => "denied".to_string(),
        None if category == "destructive" => "not recorded".to_string(),
        None => "not required".to_string(),
    }
}

fn action_result_summary(
    outcome: &str,
    operator_approved: Option<bool>,
    exit_code: Option<i32>,
    output_preview: Option<&str>,
    error: Option<&str>,
) -> String {
    match (outcome, operator_approved) {
        ("success", _) => match output_preview.map(one_line).filter(|s| !s.is_empty()) {
            Some(output) => format!("Succeeded: {output}"),
            None => "Succeeded.".to_string(),
        },
        ("rejected", Some(false)) => "Denied before execution.".to_string(),
        ("rejected", None) => match error.map(one_line).filter(|s| !s.is_empty()) {
            Some(error) => format!("Abandoned before approval: {error}"),
            None => "Abandoned before approval.".to_string(),
        },
        ("timeout", _) => match error.map(one_line).filter(|s| !s.is_empty()) {
            Some(error) => format!("Timed out: {error}"),
            None => "Timed out.".to_string(),
        },
        ("failure", _) | ("rejected", Some(true)) => {
            let prefix = match exit_code {
                Some(code) => format!("Failed with exit code {code}"),
                None => "Failed".to_string(),
            };
            match error.map(one_line).filter(|s| !s.is_empty()) {
                Some(error) => format!("{prefix}: {error}"),
                None => format!("{prefix}."),
            }
        }
        (other, _) => one_line(other),
    }
}

fn print_action_receipts(audit_path: &Path, receipts: &[ActionReceipt]) {
    println!("Dexter Action Receipts");
    println!("source: {}", audit_path.display());
    println!();

    if receipts.is_empty() {
        println!("No action receipts found.");
        return;
    }

    for receipt in receipts {
        println!("{}", format_action_receipt(receipt));
    }
}

fn format_action_receipt(receipt: &ActionReceipt) -> String {
    let duration = receipt
        .duration_ms
        .map(|ms| format!(" | duration: {ms}ms"))
        .unwrap_or_default();
    format!(
        "{time}  {status}  {kind}\n  id: {id}\n  target: {target}\n  category: {category} | approval: {approval}{duration}\n  result: {result}\n",
        time = receipt.timestamp,
        status = receipt.status.to_ascii_uppercase(),
        kind = receipt.action_type,
        id = receipt.action_id,
        target = receipt.target,
        category = receipt.category,
        approval = receipt.approval,
        duration = duration,
        result = receipt.result,
    )
}

fn load_action_state_dir() -> Result<PathBuf> {
    let config_path = doctor_config_path();
    if !config_path.exists() {
        return Ok(default_action_state_dir());
    }

    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    parse_action_state_dir(&raw)
}

fn parse_action_state_dir(raw: &str) -> Result<PathBuf> {
    let parsed: DoctorFileConfig = toml::from_str(raw).context("invalid TOML")?;
    Ok(parsed
        .core
        .state_dir
        .map(expand_home_path)
        .unwrap_or_else(default_action_state_dir))
}

fn default_action_state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(DEFAULT_DEXTER_STATE_DIR)
}

fn expand_home_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(rest);
    }
    path
}

fn empty_to_unknown(value: String) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shell_display_arg(value: &str) -> String {
    if value.is_empty() {
        "''".to_string()
    } else if value.chars().any(char::is_whitespace) {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

fn load_doctor_config() -> (DoctorCheck, Option<String>) {
    let config_path = doctor_config_path();
    if !config_path.exists() {
        return (
            DoctorCheck::ok(
                "config",
                format!(
                    "{} absent; using defaults, including Ollama {}",
                    config_path.display(),
                    DEFAULT_OLLAMA_BASE_URL
                ),
            ),
            Some(DEFAULT_OLLAMA_BASE_URL.to_string()),
        );
    }

    let raw = match fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(error) => {
            return (
                DoctorCheck::fail(
                    "config",
                    format!("failed to read {}: {error}", config_path.display()),
                ),
                None,
            );
        }
    };

    match parse_doctor_ollama_base_url(&raw) {
        Ok(ollama_base_url) => (
            DoctorCheck::ok(
                "config",
                format!(
                    "{} loaded; Ollama {}",
                    config_path.display(),
                    ollama_base_url
                ),
            ),
            Some(ollama_base_url),
        ),
        Err(error) => (
            DoctorCheck::fail(
                "config",
                format!("failed to parse {}: {error}", config_path.display()),
            ),
            None,
        ),
    }
}

fn parse_doctor_ollama_base_url(raw: &str) -> Result<String> {
    let parsed: DoctorFileConfig = toml::from_str(raw).context("invalid TOML")?;
    let base_url = parsed
        .inference
        .ollama_base_url
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return Err(anyhow!("inference.ollama_base_url is empty"));
    }
    Ok(base_url.to_string())
}

fn doctor_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".dexter")
        .join("config.toml")
}

fn check_binary_neighbors() -> Vec<DoctorCheck> {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return vec![DoctorCheck::warn(
                "cli binary",
                format!("could not resolve current executable: {error}"),
            )];
        }
    };

    let mut checks = Vec::new();
    checks.push(if is_executable_file(&current_exe) {
        DoctorCheck::ok("cli binary", current_exe.display().to_string())
    } else {
        DoctorCheck::warn(
            "cli binary",
            format!("{} exists but is not executable", current_exe.display()),
        )
    });

    let core_path = current_exe
        .parent()
        .map(|parent| parent.join("dexter-core"))
        .unwrap_or_else(|| PathBuf::from("dexter-core"));
    checks.push(if is_executable_file(&core_path) {
        DoctorCheck::ok("core binary", core_path.display().to_string())
    } else {
        DoctorCheck::warn(
            "core binary",
            format!(
                "{} not found next to dexter-cli; build with `cargo build --release --bin dexter-core`",
                core_path.display()
            ),
        )
    });

    checks
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn check_path_exists(name: &str, path: &Path, missing_status: DoctorStatus) -> DoctorCheck {
    if path.exists() {
        return DoctorCheck::ok(name, path.display().to_string());
    }

    match missing_status {
        DoctorStatus::Fail => DoctorCheck::fail(
            name,
            format!("{} missing; start the daemon first", path.display()),
        ),
        DoctorStatus::Warn => DoctorCheck::warn(
            name,
            format!(
                "{} missing; shell context may be unavailable",
                path.display()
            ),
        ),
        DoctorStatus::Ok => DoctorCheck::ok(name, path.display().to_string()),
    }
}

async fn check_daemon_ping(socket_path: &str) -> DoctorCheck {
    let connect_result = tokio::time::timeout(DOCTOR_CONNECT_TIMEOUT, connect(socket_path)).await;
    let mut client = match connect_result {
        Err(_) => {
            return DoctorCheck::fail(
                "daemon ping",
                format!("timed out connecting to {socket_path}"),
            );
        }
        Ok(Err(error)) => {
            return DoctorCheck::fail("daemon ping", format!("connect failed: {error:#}"));
        }
        Ok(Ok(client)) => client,
    };

    let ping_result = tokio::time::timeout(
        DOCTOR_REQUEST_TIMEOUT,
        client.ping(PingRequest {
            trace_id: Uuid::new_v4().to_string(),
        }),
    )
    .await;

    match ping_result {
        Err(_) => DoctorCheck::fail("daemon ping", "ping timed out"),
        Ok(Err(status)) => DoctorCheck::fail("daemon ping", format!("ping failed: {status}")),
        Ok(Ok(response)) => DoctorCheck::ok(
            "daemon ping",
            format!("core version {}", response.into_inner().core_version),
        ),
    }
}

async fn check_daemon_health(socket_path: &str) -> Vec<DoctorCheck> {
    let connect_result = tokio::time::timeout(DOCTOR_CONNECT_TIMEOUT, connect(socket_path)).await;
    let mut client = match connect_result {
        Err(_) => {
            return vec![DoctorCheck::fail(
                "daemon health",
                format!("timed out connecting to {socket_path}"),
            )];
        }
        Ok(Err(error)) => {
            return vec![DoctorCheck::fail(
                "daemon health",
                format!("connect failed: {error:#}"),
            )];
        }
        Ok(Ok(client)) => client,
    };

    let health_result = tokio::time::timeout(
        DOCTOR_REQUEST_TIMEOUT,
        client.health(HealthRequest {
            trace_id: Uuid::new_v4().to_string(),
        }),
    )
    .await;

    match health_result {
        Err(_) => vec![DoctorCheck::fail("daemon health", "health RPC timed out")],
        Ok(Err(status)) => vec![DoctorCheck::fail(
            "daemon health",
            format!("health RPC failed: {status}"),
        )],
        Ok(Ok(response)) => daemon_health_checks(response.into_inner()),
    }
}

fn daemon_health_checks(health: HealthResponse) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    let degraded = if health.degraded_components.is_empty() {
        "none".to_string()
    } else {
        health.degraded_components.join(",")
    };
    let status = doctor_status_for_daemon_health(&health.status);
    checks.push(DoctorCheck {
        status,
        name: "daemon health".to_string(),
        detail: format!(
            "status {}; degraded components {}",
            empty_as_unknown(&health.status),
            degraded
        ),
    });
    checks.push(model_warm_check(
        "fast model",
        &health.fast_model,
        health.fast_model_warm,
    ));
    checks.push(model_warm_check(
        "primary model",
        &health.primary_model,
        health.primary_model_warm,
    ));
    checks.push(model_warm_check(
        "embed model",
        &health.embed_model,
        health.embed_model_warm,
    ));
    checks.push(component_health_check("STT worker", &health.stt_worker));
    checks.push(component_health_check("TTS worker", &health.tts_worker));
    checks.push(component_health_check(
        "browser worker",
        &health.browser_worker,
    ));
    checks.push(DoctorCheck::ok(
        "daemon config",
        format!(
            "state {}; personality {}; Ollama {}",
            empty_as_unknown(&health.state_dir),
            empty_as_unknown(&health.personality_path),
            empty_as_unknown(&health.ollama_url)
        ),
    ));
    checks
}

fn doctor_status_for_daemon_health(status: &str) -> DoctorStatus {
    match status.trim().to_ascii_lowercase().as_str() {
        "ready" => DoctorStatus::Ok,
        "degraded" => DoctorStatus::Fail,
        "pending" => DoctorStatus::Warn,
        _ => DoctorStatus::Warn,
    }
}

fn doctor_status_for_component_status(status: &str) -> DoctorStatus {
    match status.trim().to_ascii_lowercase().as_str() {
        "ready" => DoctorStatus::Ok,
        "pending" => DoctorStatus::Warn,
        "degraded" => DoctorStatus::Fail,
        _ => DoctorStatus::Warn,
    }
}

fn model_warm_check(name: &str, model: &str, warm: bool) -> DoctorCheck {
    let detail = format!(
        "{} {}",
        empty_as_unknown(model),
        if warm { "warm" } else { "not warm" }
    );
    if warm {
        DoctorCheck::ok(name, detail)
    } else {
        DoctorCheck::fail(name, detail)
    }
}

fn component_health_check(name: &str, status: &str) -> DoctorCheck {
    DoctorCheck {
        status: doctor_status_for_component_status(status),
        name: name.to_string(),
        detail: empty_as_unknown(status).to_string(),
    }
}

fn empty_as_unknown(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "unknown"
    } else {
        trimmed
    }
}

async fn check_ollama(base_url: Option<&str>) -> DoctorCheck {
    let Some(base_url) = base_url else {
        return DoctorCheck::fail("ollama", "skipped because config did not parse");
    };
    let tags_url = match ollama_tags_url(base_url) {
        Ok(url) => url,
        Err(error) => return DoctorCheck::fail("ollama", error.to_string()),
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(DOCTOR_CONNECT_TIMEOUT)
        .timeout(DOCTOR_REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => return DoctorCheck::fail("ollama", format!("client build failed: {error}")),
    };

    let response = match client.get(&tags_url).send().await {
        Ok(response) => response,
        Err(error) => {
            return DoctorCheck::fail("ollama", format!("{base_url} unreachable: {error}"));
        }
    };

    let status = response.status();
    if !status.is_success() {
        return DoctorCheck::fail("ollama", format!("{tags_url} returned HTTP {status}"));
    }

    match response.json::<serde_json::Value>().await {
        Ok(body) => {
            let model_count = body
                .get("models")
                .and_then(|models| models.as_array())
                .map(|models| models.len());
            match model_count {
                Some(count) => {
                    DoctorCheck::ok("ollama", format!("{base_url} reachable; {count} models"))
                }
                None => DoctorCheck::warn(
                    "ollama",
                    format!("{base_url} reachable; /api/tags payload shape was unexpected"),
                ),
            }
        }
        Err(error) => DoctorCheck::warn(
            "ollama",
            format!("{base_url} reachable; failed to parse /api/tags JSON: {error}"),
        ),
    }
}

fn ollama_tags_url(base_url: &str) -> Result<String> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        return Err(anyhow!("Ollama base URL is empty"));
    }
    Ok(format!("{base_url}{OLLAMA_TAGS_PATH}"))
}

fn print_doctor_report(checks: &[DoctorCheck]) {
    println!("Dexter Doctor");
    println!();
    for check in checks {
        println!("{}", format_doctor_check(check));
    }
    let suggestions = suggested_recovery_commands(checks);
    if !suggestions.is_empty() {
        println!();
        print_recovery_suggestions(&suggestions);
    }
    println!();
    if checks
        .iter()
        .any(|check| check.status == DoctorStatus::Fail)
    {
        println!("Result: FAIL - fix failed checks before relying on Dexter.");
    } else {
        println!("Result: OK - no failed checks.");
    }
}

fn suggested_recovery_commands(checks: &[DoctorCheck]) -> Vec<String> {
    let mut commands = Vec::new();
    for check in checks {
        if check.status != DoctorStatus::Fail {
            continue;
        }
        let target = match check.name.as_str() {
            "STT worker" => RestartTarget::Stt,
            "TTS worker" => RestartTarget::Tts,
            "browser worker" => RestartTarget::Browser,
            _ => continue,
        };
        let command = format!("dexter-cli --restart-component {}", target.command_arg());
        if !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands
}

fn print_recovery_suggestions(commands: &[String]) {
    println!("Suggested fixes:");
    for command in commands {
        println!("  {command}");
    }
}

fn format_doctor_check(check: &DoctorCheck) -> String {
    format!(
        "{:<4} {:<18} {}",
        check.status.label(),
        check.name,
        check.detail
    )
}

fn doctor_exit_code(checks: &[DoctorCheck]) -> i32 {
    if checks
        .iter()
        .any(|check| check.status == DoctorStatus::Fail)
    {
        1
    } else {
        0
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = parse_args()?;

    if let Some(query) = cfg.action_query {
        let exit_code = run_action_receipts(&cfg, query)?;
        std::process::exit(exit_code);
    }

    if cfg.doctor {
        let exit_code = run_doctor(&cfg).await?;
        std::process::exit(exit_code);
    }

    if let Some(target) = cfg.restart_component {
        let exit_code = run_restart_component(&cfg, target).await?;
        std::process::exit(exit_code);
    }

    if cfg.inputs.is_empty() {
        eprintln!("dexter-cli: no inputs provided (positional args empty AND stdin empty)");
        std::process::exit(2);
    }

    let mut client = match connect(&cfg.socket_path).await {
        Ok(c) => c,
        Err(e) => {
            if print_daemon_connection_hint(&cfg.socket_path, "send input", &e) {
                std::process::exit(2);
            }
            return Err(e).with_context(|| format!("failed to connect to {}", cfg.socket_path));
        }
    };

    // Liveness probe — same Ping the Swift client does on connect. Also confirms
    // the proto schema matches (mismatched .proto = different field IDs = decode fail).
    let pong = client
        .ping(PingRequest {
            trace_id: Uuid::new_v4().to_string(),
        })
        .await
        .context("Ping failed — daemon may not be running, or socket path is wrong")?;
    if !cfg.quiet {
        eprintln!(
            "[connected — core version: {}]",
            pong.into_inner().core_version
        );
    }

    // Stable session ID for this CLI run — same lifecycle as Swift's
    // `currentSessionID` (set on session open, cleared on close).
    let session_id = Uuid::new_v4().to_string();

    // Open the bidirectional Session stream. Channel capacity matches Swift's
    // approach — a small buffered queue is enough since we drain the response
    // stream synchronously between sending events.
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientEvent>(16);
    let response = client
        .session(ReceiverStream::new(rx))
        .await
        .context("session() RPC failed")?;
    let mut response_stream = response.into_inner();

    // Drive each input to completion (IDLE state) before sending the next.
    for (i, input) in cfg.inputs.iter().enumerate() {
        let trace_id = Uuid::new_v4().to_string();
        match input {
            CliInput::Text(text) => {
                if !cfg.quiet {
                    eprintln!("[turn {} — sending text: {text:?}]", i + 1);
                }

                let event = ClientEvent {
                    trace_id: trace_id.clone(),
                    session_id: session_id.clone(),
                    event: Some(client_event::Event::TextInput(TextInput {
                        content: text.clone(),
                        from_voice: cfg.from_voice,
                    })),
                };
                tx.send(event)
                    .await
                    .map_err(|_| anyhow!("session stream closed before TextInput could be sent"))?;

                // Drain server events until we see IDLE (turn complete) or hit the timeout.
                run_turn(&mut response_stream, &tx, &session_id, &cfg).await?;
            }
            CliInput::ActionJson(raw_json) => {
                if !cfg.quiet {
                    eprintln!("[turn {} — sending action JSON]", i + 1);
                }

                let payload = json!({
                    "source": "dexter-cli",
                    "kind": "action_json",
                    "action_json": serde_json::from_str::<serde_json::Value>(raw_json)
                        .context("--action-json: argument is not valid JSON")?,
                })
                .to_string();
                let event = ClientEvent {
                    trace_id: trace_id.clone(),
                    session_id: session_id.clone(),
                    event: Some(client_event::Event::UiAction(UiAction {
                        r#type: UiActionType::Unspecified as i32,
                        payload,
                    })),
                };
                tx.send(event)
                    .await
                    .map_err(|_| anyhow!("session stream closed before UIAction could be sent"))?;

                run_turn(&mut response_stream, &tx, &session_id, &cfg).await?;
            }
            CliInput::SystemEvent {
                event_type,
                payload,
            } => {
                if !cfg.quiet {
                    eprintln!("[event {} — sending system event: {:?}]", i + 1, event_type);
                }
                let event = ClientEvent {
                    trace_id,
                    session_id: session_id.clone(),
                    event: Some(client_event::Event::SystemEvent(SystemEvent {
                        r#type: *event_type as i32,
                        payload: payload.clone(),
                    })),
                };
                tx.send(event).await.map_err(|_| {
                    anyhow!("session stream closed before SystemEvent could be sent")
                })?;
                // System events normally do not produce a full turn. Yield briefly so the
                // daemon's select loop can ingest this context before the next TextInput.
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
            CliInput::ShellCommand {
                command,
                cwd,
                exit_code,
            } => {
                if !cfg.quiet {
                    eprintln!(
                        "[event {} — sending shell command context: {:?} exit {:?}]",
                        i + 1,
                        command,
                        exit_code
                    );
                }
                send_shell_command_event(&cfg.shell_socket_path, command, cwd, *exit_code)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to send shell context event to {}",
                            cfg.shell_socket_path
                        )
                    })?;
                // Give the daemon's shell listener task and active session select loop
                // a brief chance to ingest the event before the next TextInput.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    // Close the writer half cleanly so the daemon's session task exits its loop
    // normally. Without this drop, the daemon waits for either the next event or
    // the gRPC stream EOF — `tx.drop()` triggers EOF on the read side.
    drop(tx);
    Ok(())
}

async fn send_shell_command_event(
    socket_path: &str,
    command: &str,
    cwd: &str,
    exit_code: Option<i32>,
) -> Result<()> {
    let payload = json!({
        "command": command,
        "cwd": cwd,
        "exit_code": exit_code,
    })
    .to_string();
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect {socket_path}"))?;
    stream
        .write_all(payload.as_bytes())
        .await
        .context("write shell payload")?;
    stream.shutdown().await.context("shutdown shell socket")?;
    Ok(())
}

/// Connect to the Dexter daemon's gRPC socket using the same UDS-over-tonic
/// pattern as the integration tests in `src/ipc/server.rs`. The
/// `Endpoint::from_static("http://localhost")` URI is a placeholder — tonic
/// requires a valid HTTP/2 :authority header but doesn't use it for routing
/// when the connector returns a UnixStream directly.
async fn connect(socket_path: &str) -> Result<DexterServiceClient<tonic::transport::Channel>> {
    let path = socket_path.to_string();
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(service_fn(move |_: tonic::transport::Uri| {
            let p = path.clone();
            async move { UnixStream::connect(p).await.map(TokioIo::new) }
        }))
        .await
        .context("tonic Channel connect failed")?;
    Ok(DexterServiceClient::new(channel))
}

fn print_daemon_connection_hint(socket_path: &str, operation: &str, error: &anyhow::Error) -> bool {
    let chain = format!("{error:#}");
    let socket_missing =
        !std::path::Path::new(socket_path).exists() || chain.contains("No such file or directory");
    if !socket_missing {
        return false;
    }

    eprintln!(
        "dexter-cli: cannot {operation}; {socket_path} is unavailable — daemon not running.\n\
         \n\
         Start it in another terminal:\n\
           make run 2>&1 | tee /tmp/dexter-verify.log\n\
         \n\
         Wait for the \"Ready.\" TTS, then re-run this command."
    );
    true
}

fn audio_playback_complete_payload(audio_trace_id: &str) -> String {
    let audio_trace_id = audio_trace_id.trim();
    if audio_trace_id.is_empty() {
        "{}".to_string()
    } else {
        json!({ "audio_trace_id": audio_trace_id }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_exit_code_accepts_integer_and_null() {
        assert_eq!(parse_shell_exit_code("0").unwrap(), Some(0));
        assert_eq!(parse_shell_exit_code("130").unwrap(), Some(130));
        assert_eq!(parse_shell_exit_code("null").unwrap(), None);
        assert_eq!(parse_shell_exit_code("None").unwrap(), None);
    }

    #[test]
    fn parse_shell_exit_code_rejects_invalid_text() {
        assert!(parse_shell_exit_code("oops").is_err());
        assert!(parse_shell_exit_code("").is_err());
    }

    #[test]
    fn parse_system_event_type_accepts_common_spellings() {
        assert_eq!(
            parse_system_event_type("app-focused").unwrap(),
            SystemEventType::AppFocused
        );
        assert_eq!(
            parse_system_event_type("SYSTEM_EVENT_TYPE_CLIPBOARD_CHANGED").unwrap(),
            SystemEventType::ClipboardChanged
        );
    }

    #[test]
    fn parse_action_query_accepts_last_and_recent() {
        assert_eq!(parse_action_query("last").unwrap(), ActionQuery::Last);
        assert_eq!(parse_action_query(" RECENT ").unwrap(), ActionQuery::Recent);
    }

    #[test]
    fn parse_action_query_rejects_unknown_query() {
        assert!(parse_action_query("all").is_err());
        assert!(parse_action_query("").is_err());
    }

    #[test]
    fn parse_restart_target_accepts_worker_names() {
        assert_eq!(parse_restart_target("stt").unwrap(), RestartTarget::Stt);
        assert_eq!(
            parse_restart_target("text-to-speech").unwrap(),
            RestartTarget::Tts
        );
        assert_eq!(
            parse_restart_target("browser_worker").unwrap(),
            RestartTarget::Browser
        );
    }

    #[test]
    fn parse_restart_target_rejects_unknown_component() {
        assert!(parse_restart_target("primary").is_err());
        assert!(parse_restart_target("").is_err());
    }

    #[test]
    fn audio_playback_complete_payload_tags_audio_trace_id() {
        assert_eq!(
            audio_playback_complete_payload("trace-123"),
            r#"{"audio_trace_id":"trace-123"}"#
        );
    }

    #[test]
    fn audio_playback_complete_payload_preserves_legacy_empty_payload() {
        assert_eq!(audio_playback_complete_payload("   "), "{}");
    }

    #[test]
    fn parse_doctor_ollama_base_url_uses_default_when_absent() {
        assert_eq!(
            parse_doctor_ollama_base_url("[core]\nsocket_path = \"/tmp/custom.sock\"\n").unwrap(),
            DEFAULT_OLLAMA_BASE_URL
        );
    }

    #[test]
    fn parse_doctor_ollama_base_url_reads_inference_override() {
        assert_eq!(
            parse_doctor_ollama_base_url(
                "[inference]\nollama_base_url = \"http://127.0.0.1:11435\"\n"
            )
            .unwrap(),
            "http://127.0.0.1:11435"
        );
    }

    #[test]
    fn parse_doctor_ollama_base_url_rejects_empty_override() {
        assert!(parse_doctor_ollama_base_url("[inference]\nollama_base_url = \"   \"\n").is_err());
    }

    #[test]
    fn parse_action_state_dir_reads_core_override() {
        assert_eq!(
            parse_action_state_dir("[core]\nstate_dir = \"/tmp/dexter-action-state\"\n").unwrap(),
            PathBuf::from("/tmp/dexter-action-state")
        );
    }

    #[test]
    fn parse_action_state_dir_expands_home_override() {
        let parsed =
            parse_action_state_dir("[core]\nstate_dir = \"~/dexter-action-state\"\n").unwrap();
        assert!(parsed.ends_with("dexter-action-state"));
        assert!(parsed.is_absolute());
    }

    #[test]
    fn ollama_tags_url_trims_trailing_slash() {
        assert_eq!(
            ollama_tags_url("http://localhost:11434/").unwrap(),
            "http://localhost:11434/api/tags"
        );
    }

    #[test]
    fn action_receipt_from_audit_formats_shell_success() {
        let receipt = action_receipt_from_audit(AuditEntryOwned {
            timestamp: "2026-05-18T12:00:00Z".to_string(),
            action_id: "act-1".to_string(),
            action_type: "shell".to_string(),
            category: "safe".to_string(),
            spec_json: serde_json::json!({
                "args": ["echo", "hello world"],
                "working_dir": "/tmp",
                "rationale": "smoke",
            }),
            outcome: "success".to_string(),
            exit_code: Some(0),
            output_preview: Some("hello world\n".to_string()),
            error: None,
            duration_ms: Some(17),
            operator_approved: None,
        });

        assert_eq!(receipt.status, "executed");
        assert_eq!(receipt.approval, "not required");
        assert_eq!(receipt.target, "echo \"hello world\"  (cwd: /tmp)");
        assert_eq!(receipt.result, "Succeeded: hello world");
        assert_eq!(receipt.duration_ms, Some(17));
    }

    #[test]
    fn action_receipt_from_audit_formats_denied_destructive_action() {
        let receipt = action_receipt_from_audit(AuditEntryOwned {
            timestamp: "2026-05-18T12:00:00Z".to_string(),
            action_id: "act-2".to_string(),
            action_type: "applescript".to_string(),
            category: "destructive".to_string(),
            spec_json: serde_json::json!({
                "rationale": "send iMessage to Jason Phillips",
            }),
            outcome: "rejected".to_string(),
            exit_code: None,
            output_preview: None,
            error: None,
            duration_ms: None,
            operator_approved: Some(false),
        });

        assert_eq!(receipt.status, "denied");
        assert_eq!(receipt.approval, "denied");
        assert_eq!(receipt.target, "send iMessage to Jason Phillips");
        assert_eq!(receipt.result, "Denied before execution.");
    }

    #[test]
    fn read_action_receipts_returns_newest_first_with_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_path = tmp.path().join("audit.jsonl");
        std::fs::write(
            &audit_path,
            [
                r#"{"timestamp":"old","action_id":"act-old","type":"shell","category":"safe","spec_json":{"args":["echo","old"],"working_dir":null,"rationale":null},"outcome":"success","exit_code":0,"output_preview":"old","error":null,"duration_ms":1,"operator_approved":null}"#,
                r#"{"timestamp":"new","action_id":"act-new","type":"file_read","category":"safe","spec_json":{"path":"/tmp/new.txt"},"outcome":"failure","exit_code":null,"output_preview":null,"error":"permission denied","duration_ms":2,"operator_approved":null}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let receipts = read_action_receipts(&audit_path, 1).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].action_id, "act-new");
        assert_eq!(receipts[0].target, "/tmp/new.txt");
        assert_eq!(receipts[0].result, "Failed: permission denied");
    }

    #[test]
    fn format_action_receipt_includes_receipt_fields() {
        let receipt = ActionReceipt {
            timestamp: "2026-05-18T12:00:00Z".to_string(),
            action_id: "act-3".to_string(),
            action_type: "browser".to_string(),
            category: "cautious".to_string(),
            target: "navigate https://example.com".to_string(),
            status: "executed".to_string(),
            approval: "not required".to_string(),
            result: "Succeeded: loaded".to_string(),
            duration_ms: Some(42),
        };

        let formatted = format_action_receipt(&receipt);
        assert!(formatted.contains("EXECUTED  browser"));
        assert!(formatted.contains("id: act-3"));
        assert!(formatted.contains("target: navigate https://example.com"));
        assert!(formatted.contains("category: cautious | approval: not required | duration: 42ms"));
        assert!(formatted.contains("result: Succeeded: loaded"));
    }

    #[test]
    fn doctor_exit_code_fails_only_on_failed_checks() {
        assert_eq!(
            doctor_exit_code(&[DoctorCheck::ok("a", "ok"), DoctorCheck::warn("b", "warn")]),
            0
        );
        assert_eq!(
            doctor_exit_code(&[DoctorCheck::ok("a", "ok"), DoctorCheck::fail("b", "fail")]),
            1
        );
    }

    #[test]
    fn format_doctor_check_is_stable() {
        let line = format_doctor_check(&DoctorCheck::ok("daemon ping", "core version 0.1.0"));
        assert_eq!(line, "OK   daemon ping        core version 0.1.0");
    }

    #[test]
    fn suggested_recovery_commands_map_failed_workers() {
        let commands = suggested_recovery_commands(&[
            DoctorCheck::fail("STT worker", "degraded"),
            DoctorCheck::fail("TTS worker", "degraded"),
            DoctorCheck::fail("browser worker", "degraded"),
        ]);
        assert_eq!(
            commands,
            vec![
                "dexter-cli --restart-component stt".to_string(),
                "dexter-cli --restart-component tts".to_string(),
                "dexter-cli --restart-component browser".to_string(),
            ]
        );
    }

    #[test]
    fn suggested_recovery_commands_ignore_non_workers_and_dedupe() {
        let commands = suggested_recovery_commands(&[
            DoctorCheck::fail("TTS worker", "degraded"),
            DoctorCheck::warn("browser worker", "pending"),
            DoctorCheck::fail("primary model", "not warm"),
            DoctorCheck::fail("TTS worker", "degraded"),
        ]);
        assert_eq!(
            commands,
            vec!["dexter-cli --restart-component tts".to_string()]
        );
    }

    #[test]
    fn daemon_health_checks_expand_ready_snapshot() {
        let checks = daemon_health_checks(HealthResponse {
            trace_id: "trace".to_string(),
            core_version: "0.1.0".to_string(),
            status: "ready".to_string(),
            degraded_components: Vec::new(),
            socket: "/tmp/dexter.sock".to_string(),
            shell_socket: "/tmp/dexter-shell.sock".to_string(),
            config_path: "/Users/jason/.dexter/config.toml".to_string(),
            state_dir: "/Users/jason/.dexter/state".to_string(),
            personality_path: "config/personality/default.yaml".to_string(),
            ollama_url: "http://localhost:11434".to_string(),
            fast_model: "qwen3:8b".to_string(),
            primary_model: "gemma4:26b".to_string(),
            embed_model: "mxbai-embed-large".to_string(),
            fast_model_warm: true,
            primary_model_warm: true,
            embed_model_warm: true,
            stt_worker: "ready".to_string(),
            tts_worker: "ready".to_string(),
            browser_worker: "ready".to_string(),
        });

        assert_eq!(checks[0].status, DoctorStatus::Ok);
        assert!(checks
            .iter()
            .all(|check| check.status != DoctorStatus::Fail));
        assert!(checks
            .iter()
            .any(|check| check.name == "primary model" && check.detail == "gemma4:26b warm"));
    }

    #[test]
    fn daemon_health_checks_fail_on_degraded_snapshot() {
        let checks = daemon_health_checks(HealthResponse {
            trace_id: "trace".to_string(),
            core_version: "0.1.0".to_string(),
            status: "degraded".to_string(),
            degraded_components: vec!["primary_model".to_string(), "tts_worker".to_string()],
            socket: "/tmp/dexter.sock".to_string(),
            shell_socket: "/tmp/dexter-shell.sock".to_string(),
            config_path: "/Users/jason/.dexter/config.toml".to_string(),
            state_dir: "/Users/jason/.dexter/state".to_string(),
            personality_path: "config/personality/default.yaml".to_string(),
            ollama_url: "http://localhost:11434".to_string(),
            fast_model: "qwen3:8b".to_string(),
            primary_model: "gemma4:26b".to_string(),
            embed_model: "mxbai-embed-large".to_string(),
            fast_model_warm: true,
            primary_model_warm: false,
            embed_model_warm: true,
            stt_worker: "ready".to_string(),
            tts_worker: "degraded".to_string(),
            browser_worker: "ready".to_string(),
        });

        assert_eq!(checks[0].status, DoctorStatus::Fail);
        assert!(checks
            .iter()
            .any(|check| check.name == "primary model" && check.status == DoctorStatus::Fail));
        assert!(checks
            .iter()
            .any(|check| check.name == "TTS worker" && check.status == DoctorStatus::Fail));
    }
}

/// Drain server events for one turn — returns when:
///   - EntityStateChange with state=IDLE arrives (turn complete), OR
///   - the response stream ends (server closed session), OR
///   - the per-turn idle_timeout fires.
///
/// Side effects: prints text responses to stdout, prints state markers and
/// action requests to stdout (unless `--quiet`), and replies to ActionRequests
/// with an ActionApproval whose decision matches `cfg.approval_policy`.
async fn run_turn(
    response_stream: &mut tonic::Streaming<proto::ServerEvent>,
    tx: &tokio::sync::mpsc::Sender<ClientEvent>,
    session_id: &str,
    cfg: &CliConfig,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();

    // Phase 38 / dexter-cli bugfix: distinguish the initial-state IDLE
    // (sent by `Session opened — IDLE sent` in ipc/server.rs) from the
    // turn-complete IDLE that fires after the model finishes responding.
    //
    // Without this gate, the CLI sees the first IDLE within milliseconds of
    // session open, exits run_turn, and drops the gRPC stream — even though
    // the daemon hasn't started processing the TextInput yet. The daemon
    // then tries to deliver tokens 40s later (after warmup) and finds the
    // channel closed, logging "Startup greeting failed — gRPC session
    // channel closed" + "Orchestrator event handler failed".
    //
    // Turn-complete = IDLE seen AFTER at least one signal that the daemon
    // actually processed our input: any non-IDLE state, any text response,
    // OR an action request. Either of those proves "Dexter saw the input
    // and started working."
    let mut activity_seen = false;
    let mut focused_interrupt_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut focused_interrupt_armed = false;

    let turn_result: Result<()> = loop {
        let next = tokio::time::timeout(cfg.idle_timeout, response_stream.next()).await;
        let event = match next {
            Err(_elapsed) => {
                eprintln!(
                    "[idle timeout {}s — giving up on this turn]",
                    cfg.idle_timeout.as_secs()
                );
                break Ok(());
            }
            Ok(None) => {
                if !cfg.quiet {
                    eprintln!("[server closed session stream]");
                }
                break Ok(());
            }
            Ok(Some(Err(status))) => {
                break Err(anyhow!("session stream error: {status}"));
            }
            Ok(Some(Ok(evt))) => evt,
        };

        let event_trace_id = event.trace_id.clone();
        match event.event {
            // Streaming text from the model. Print directly without newlines so
            // the response builds up in the terminal the way it streams.
            Some(server_event::Event::TextResponse(text)) => {
                if !text.content.is_empty() {
                    activity_seen = true;
                    write!(stdout_lock, "{}", text.content)?;
                    stdout_lock.flush()?;
                }
                if text.is_final {
                    // Mark the end of the model's reply with a newline so
                    // subsequent lines (state markers etc.) start fresh.
                    writeln!(stdout_lock)?;
                }
            }

            // State transition. Drives turn-completion logic — IDLE after
            // any activity = turn done. IDLE BEFORE activity = the
            // session-open initial state, ignore it and keep listening.
            Some(server_event::Event::EntityState(s)) => {
                let state = EntityState::try_from(s.state).unwrap_or(EntityState::Unspecified);
                if !cfg.quiet {
                    writeln!(stdout_lock, "[STATE: {state:?}]")?;
                }
                let is_active = !matches!(state, EntityState::Idle | EntityState::Unspecified);
                if is_active {
                    activity_seen = true;
                }
                if state == EntityState::Focused && !focused_interrupt_armed {
                    let Some(delay) = cfg.interrupt_on_focused_after else {
                        continue;
                    };
                    focused_interrupt_armed = true;
                    let tx_interrupt = tx.clone();
                    let session_id_interrupt = session_id.to_string();
                    if !cfg.quiet {
                        writeln!(
                            stdout_lock,
                            "[INTERRUPT armed after focused: {}ms]",
                            delay.as_millis()
                        )?;
                    }
                    focused_interrupt_task = Some(tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let event = ClientEvent {
                            trace_id: Uuid::new_v4().to_string(),
                            session_id: session_id_interrupt,
                            event: Some(client_event::Event::SystemEvent(SystemEvent {
                                r#type: SystemEventType::HotkeyActivated as i32,
                                payload: "{}".to_string(),
                            })),
                        };
                        let _ = tx_interrupt.send(event).await;
                    }));
                }
                if state == EntityState::Listening && focused_interrupt_armed {
                    if !cfg.quiet {
                        writeln!(stdout_lock, "[INTERRUPTED]")?;
                    }
                    break Ok(());
                }
                if state == EntityState::Idle && activity_seen {
                    if !cfg.quiet {
                        writeln!(stdout_lock, "[DONE]")?;
                    }
                    break Ok(());
                }
            }

            // Audio frames — note arrival but discard (CLI can't play audio).
            // Prints a single-character signal in non-quiet mode so test
            // scripts that grep for audio activity have a signal.
            Some(server_event::Event::AudioResponse(audio)) => {
                if !cfg.quiet {
                    if audio.is_final {
                        writeln!(
                            stdout_lock,
                            "[AUDIO: is_final sentinel after {} bytes streamed]",
                            audio.data.len()
                        )?;
                    } else {
                        write!(stdout_lock, ".")?;
                        stdout_lock.flush()?;
                    }
                }
                if audio.is_final {
                    let event = ClientEvent {
                        trace_id: Uuid::new_v4().to_string(),
                        session_id: session_id.to_string(),
                        event: Some(client_event::Event::SystemEvent(SystemEvent {
                            r#type: SystemEventType::AudioPlaybackComplete as i32,
                            payload: audio_playback_complete_payload(&event_trace_id),
                        })),
                    };
                    tx.send(event).await.map_err(|_| {
                        anyhow!(
                            "session stream closed before AUDIO_PLAYBACK_COMPLETE could be sent"
                        )
                    })?;
                }
            }

            // Action approval flow. Print the request, send back ActionApproval
            // per the configured policy. Without this, the daemon would wait
            // for a Swift dialog response that never arrives.
            Some(server_event::Event::ActionRequest(req)) => {
                activity_seen = true;
                let cat =
                    ActionCategory::try_from(req.category).unwrap_or(ActionCategory::Unspecified);
                if !cfg.quiet {
                    writeln!(
                        stdout_lock,
                        "[ACTION REQUEST id={} category={cat:?}]\n  description: {}\n  payload: {}",
                        req.action_id, req.description, req.payload,
                    )?;
                }
                let approved = matches!(cfg.approval_policy, ApprovalPolicy::Approve);
                let approval = ClientEvent {
                    trace_id: Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    event: Some(client_event::Event::ActionApproval(ActionApproval {
                        action_id: req.action_id.clone(),
                        approved,
                        operator_note: format!(
                            "dexter-cli auto-{} (policy: {:?})",
                            if approved { "approved" } else { "denied" },
                            cfg.approval_policy,
                        ),
                    })),
                };
                tx.send(approval).await.map_err(|_| {
                    anyhow!("session stream closed before ActionApproval could be sent")
                })?;
                if !cfg.quiet {
                    writeln!(
                        stdout_lock,
                        "[ACTION REPLY → action_id={} approved={approved}]",
                        req.action_id,
                    )?;
                }
            }

            // ConfigSync at session open — Swift uses it for hotkey config; CLI
            // doesn't care. Print a compact marker in non-quiet mode.
            Some(server_event::Event::ConfigSync(_)) => {
                if !cfg.quiet {
                    writeln!(stdout_lock, "[CONFIG_SYNC received]")?;
                }
            }

            // VadHint sets the next utterance's silence threshold — voice-only
            // signal, irrelevant to CLI. Discard.
            Some(server_event::Event::VadHint(_)) => {}

            None => {
                // Malformed event with no variant — log and continue.
                if !cfg.quiet {
                    writeln!(stdout_lock, "[unrecognized server event with no variant]")?;
                }
            }
        }
    };

    if let Some(handle) = focused_interrupt_task {
        handle.abort();
    }

    turn_result
}
