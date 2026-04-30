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

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hyper_util::rt::TokioIo;
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
    client_event,
    dexter_service_client::DexterServiceClient,
    server_event,
    ActionApproval, ActionCategory, ClientEvent, EntityState, PingRequest,
    TextInput,
};

const DEFAULT_SOCKET: &str = "/tmp/dexter.sock";
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalPolicy {
    /// Auto-deny destructive action requests (default — matches "operator was not present").
    Deny,
    /// Auto-approve destructive action requests (use with care; matches `--auto-approve`).
    Approve,
}

#[derive(Debug)]
struct CliConfig {
    socket_path:      String,
    inputs:           Vec<String>,
    from_voice:       bool,
    quiet:            bool,
    approval_policy:  ApprovalPolicy,
    idle_timeout:     Duration,
}

fn parse_args() -> Result<CliConfig> {
    let mut socket_path     = DEFAULT_SOCKET.to_string();
    let mut from_voice      = false;
    let mut quiet           = false;
    let mut approval_policy = ApprovalPolicy::Deny;
    let mut idle_timeout    = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS);
    let mut positional: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" | "-s" => {
                socket_path = args.next()
                    .ok_or_else(|| anyhow!("--socket requires a path argument"))?;
            }
            "--from-voice" => from_voice = true,
            "--quiet" | "-q" => quiet = true,
            "--auto-approve" | "-y" => approval_policy = ApprovalPolicy::Approve,
            "--auto-deny"    | "-n" => approval_policy = ApprovalPolicy::Deny,
            "--idle-timeout" => {
                let secs: u64 = args.next()
                    .ok_or_else(|| anyhow!("--idle-timeout requires a seconds argument"))?
                    .parse()
                    .context("--idle-timeout: not a valid u64 seconds value")?;
                idle_timeout = Duration::from_secs(secs);
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
    let inputs = if positional.is_empty() {
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
                lines.push(trimmed.to_string());
            }
        }
        lines
    } else {
        positional
    };

    Ok(CliConfig { socket_path, inputs, from_voice, quiet, approval_policy, idle_timeout })
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
    eprintln!("      --from-voice         Set TextInput.from_voice = true (enables TTS).");
    eprintln!("                           Default false matches HUD typed-input mode.");
    eprintln!("  -q, --quiet              Suppress state markers — only print model text.");
    eprintln!("  -y, --auto-approve       Auto-approve destructive action requests.");
    eprintln!("  -n, --auto-deny          Auto-deny destructive action requests (default).");
    eprintln!("      --idle-timeout <S>   Wait at most S seconds for IDLE before next turn (default: {DEFAULT_IDLE_TIMEOUT_SECS}).");
    eprintln!("  -h, --help               Show this help and exit.");
    eprintln!();
    eprintln!("EXAMPLES:");
    eprintln!("  dexter-cli \"what's 2 plus 2\"");
    eprintln!("  dexter-cli --quiet \"explain how TCP slow-start works\"");
    eprintln!("  printf \"q1\\nq2\\n\" | dexter-cli");
    eprintln!("  dexter-cli --auto-deny \"rm -rf /tmp/foo\"");
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = parse_args()?;

    if cfg.inputs.is_empty() {
        eprintln!("dexter-cli: no inputs provided (positional args empty AND stdin empty)");
        std::process::exit(2);
    }

    let mut client = match connect(&cfg.socket_path).await {
        Ok(c) => c,
        Err(e) => {
            // Detect the canonical "daemon isn't running" case (UDS connect
            // returns ENOENT when no socket file exists). Give a hint instead
            // of the bare tonic→transport→ENOENT error chain.
            //
            // Phase 38 / dexter-cli: this is THE single failure mode operators
            // will hit most often (forgot to `make run` first). Worth detecting
            // explicitly because the default error chain reads as "the CLI is
            // broken" when actually the daemon is just absent.
            let chain = format!("{e:#}");
            let socket_missing = !std::path::Path::new(&cfg.socket_path).exists()
                || chain.contains("No such file or directory");
            if socket_missing {
                eprintln!(
                    "dexter-cli: cannot connect to {} — daemon not running.\n\
                     \n\
                     Start it in another terminal:\n\
                       make run 2>&1 | tee /tmp/dexter-verify.log\n\
                     \n\
                     Wait for the \"Ready.\" TTS, then re-run this command.",
                    cfg.socket_path,
                );
                std::process::exit(2);
            }
            return Err(e).with_context(|| format!("failed to connect to {}", cfg.socket_path));
        }
    };

    // Liveness probe — same Ping the Swift client does on connect. Also confirms
    // the proto schema matches (mismatched .proto = different field IDs = decode fail).
    let pong = client.ping(PingRequest {
        trace_id: Uuid::new_v4().to_string(),
    }).await.context("Ping failed — daemon may not be running, or socket path is wrong")?;
    if !cfg.quiet {
        eprintln!("[connected — core version: {}]", pong.into_inner().core_version);
    }

    // Stable session ID for this CLI run — same lifecycle as Swift's
    // `currentSessionID` (set on session open, cleared on close).
    let session_id = Uuid::new_v4().to_string();

    // Open the bidirectional Session stream. Channel capacity matches Swift's
    // approach — a small buffered queue is enough since we drain the response
    // stream synchronously between sending events.
    let (tx, rx) = tokio::sync::mpsc::channel::<ClientEvent>(16);
    let response = client.session(ReceiverStream::new(rx)).await
        .context("session() RPC failed")?;
    let mut response_stream = response.into_inner();

    // Drive each input to completion (IDLE state) before sending the next.
    for (i, input) in cfg.inputs.iter().enumerate() {
        let trace_id = Uuid::new_v4().to_string();
        if !cfg.quiet {
            eprintln!("[turn {} — sending: {input:?}]", i + 1);
        }

        let event = ClientEvent {
            trace_id:   trace_id.clone(),
            session_id: session_id.clone(),
            event: Some(client_event::Event::TextInput(TextInput {
                content:    input.clone(),
                from_voice: cfg.from_voice,
            })),
        };
        tx.send(event).await
            .map_err(|_| anyhow!("session stream closed before TextInput could be sent"))?;

        // Drain server events until we see IDLE (turn complete) or hit the timeout.
        run_turn(&mut response_stream, &tx, &session_id, &cfg).await?;
    }

    // Close the writer half cleanly so the daemon's session task exits its loop
    // normally. Without this drop, the daemon waits for either the next event or
    // the gRPC stream EOF — `tx.drop()` triggers EOF on the read side.
    drop(tx);
    Ok(())
}

/// Connect to the Dexter daemon's gRPC socket using the same UDS-over-tonic
/// pattern as the integration tests in `src/ipc/server.rs`. The
/// `Endpoint::from_static("http://localhost")` URI is a placeholder — tonic
/// requires a valid HTTP/2 :authority header but doesn't use it for routing
/// when the connector returns a UnixStream directly.
async fn connect(
    socket_path: &str,
) -> Result<DexterServiceClient<tonic::transport::Channel>> {
    let path = socket_path.to_string();
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(service_fn(move |_: tonic::transport::Uri| {
            let p = path.clone();
            async move {
                UnixStream::connect(p)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await
        .context("tonic Channel connect failed")?;
    Ok(DexterServiceClient::new(channel))
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
    response_stream:    &mut tonic::Streaming<proto::ServerEvent>,
    tx:                 &tokio::sync::mpsc::Sender<ClientEvent>,
    session_id:         &str,
    cfg:                &CliConfig,
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

    loop {
        let next = tokio::time::timeout(cfg.idle_timeout, response_stream.next()).await;
        let event = match next {
            Err(_elapsed) => {
                eprintln!("[idle timeout {}s — giving up on this turn]", cfg.idle_timeout.as_secs());
                return Ok(());
            }
            Ok(None) => {
                if !cfg.quiet { eprintln!("[server closed session stream]"); }
                return Ok(());
            }
            Ok(Some(Err(status))) => {
                return Err(anyhow!("session stream error: {status}"));
            }
            Ok(Some(Ok(evt))) => evt,
        };

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
                let state = EntityState::try_from(s.state)
                    .unwrap_or(EntityState::Unspecified);
                if !cfg.quiet {
                    writeln!(stdout_lock, "[STATE: {state:?}]")?;
                }
                let is_active = !matches!(state, EntityState::Idle | EntityState::Unspecified);
                if is_active {
                    activity_seen = true;
                }
                if state == EntityState::Idle && activity_seen {
                    if !cfg.quiet {
                        writeln!(stdout_lock, "[DONE]")?;
                    }
                    return Ok(());
                }
            }

            // Audio frames — note arrival but discard (CLI can't play audio).
            // Prints a single-character signal in non-quiet mode so test
            // scripts that grep for audio activity have a signal.
            Some(server_event::Event::AudioResponse(audio)) => {
                if !cfg.quiet {
                    if audio.is_final {
                        writeln!(stdout_lock, "[AUDIO: is_final sentinel after {} bytes streamed]", audio.data.len())?;
                    } else {
                        write!(stdout_lock, ".")?;
                        stdout_lock.flush()?;
                    }
                }
            }

            // Action approval flow. Print the request, send back ActionApproval
            // per the configured policy. Without this, the daemon would wait
            // for a Swift dialog response that never arrives.
            Some(server_event::Event::ActionRequest(req)) => {
                activity_seen = true;
                let cat = ActionCategory::try_from(req.category)
                    .unwrap_or(ActionCategory::Unspecified);
                if !cfg.quiet {
                    writeln!(
                        stdout_lock,
                        "[ACTION REQUEST id={} category={cat:?}]\n  description: {}\n  payload: {}",
                        req.action_id, req.description, req.payload,
                    )?;
                }
                let approved = matches!(cfg.approval_policy, ApprovalPolicy::Approve);
                let approval = ClientEvent {
                    trace_id:   Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    event: Some(client_event::Event::ActionApproval(ActionApproval {
                        action_id:     req.action_id.clone(),
                        approved,
                        operator_note: format!(
                            "dexter-cli auto-{} (policy: {:?})",
                            if approved { "approved" } else { "denied" },
                            cfg.approval_policy,
                        ),
                    })),
                };
                tx.send(approval).await
                    .map_err(|_| anyhow!("session stream closed before ActionApproval could be sent"))?;
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
    }
}
