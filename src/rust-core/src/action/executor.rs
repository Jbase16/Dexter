/// Action executors — the layer that actually touches the OS.
///
/// Each function corresponds to one `ActionSpec` variant. All executors:
/// - Are `async` (IO-bound via tokio)
/// - Return a fully-populated `ExecutionResult` — no panics, no unwraps
/// - Enforce a wall-clock timeout via `tokio::time::timeout`
/// - **Never** use `shell=true` — args are passed directly to the OS
///
/// ## Shell safety
///
/// `execute_shell` calls `tokio::process::Command::new(&args[0]).args(&args[1..])`
/// directly. This is structurally different from `shell=true` (which routes through
/// `/bin/sh -c "..."` and enables shell metacharacter injection). With an explicit
/// arg array, the OS sees exactly the tokens Dexter provides — no injection surface.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use tokio::{process::Command, time::timeout};
use tracing::warn;

use crate::{
    browser::coordinator::BrowserCoordinator,
    voice::protocol::msg,
};

use super::engine::BrowserActionKind;

// ── ExecutionResult ───────────────────────────────────────────────────────────

/// The raw result of executing one action. All fields are always populated;
/// the caller (ActionEngine) decides what to log and what to surface.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Process exited 0 / IO succeeded.
    pub success:     bool,
    /// Stdout (shell/AppleScript) or file content (file_read) or bytes-written note (file_write).
    pub output:      String,
    /// Stderr or IO error description. Empty string on success.
    pub error:       String,
    /// Process exit code. `None` for pure IO operations (file_read/file_write), timeouts.
    pub exit_code:   Option<i32>,
    /// Wall-clock duration of the execution in milliseconds.
    pub duration_ms: u64,
}

// ── execute_shell ─────────────────────────────────────────────────────────────

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Expand a leading `~` to the user's home directory.
///
/// `Command::new()` never invokes a shell, so `~/` paths are passed verbatim
/// to the OS and fail with ENOENT. This must be called on every path-bearing
/// arg before it reaches `Command`.
fn expand_home(s: &str) -> String {
    if s.starts_with("~/") || s == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{}{}", home, &s[1..])
    } else {
        s.to_string()
    }
}

fn expand_home_path(path: &PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    PathBuf::from(expand_home(&s))
}

/// Normalize a path the way the policy classifier should see it.
///
/// Phase 38 / Codex finding [3]: `classify_file_write` previously ran on the
/// raw, unexpanded path while `execute_file_write` expanded `~` and the kernel
/// resolved `..`. That meant `~/../../etc/hosts` classified as Cautious (no
/// system prefix) but executed against `/etc/hosts`. This helper is the single
/// source of truth — both classifier and executor call it so the categorization
/// matches the path that actually reaches the filesystem.
///
/// Steps:
///   (1) Expand leading `~` / `~/...` to `$HOME`.
///   (2) Lexically collapse `.` and `..` components without touching the
///       filesystem (equivalent to `os.path.normpath`).
///
/// Does NOT canonicalize symlinks — that requires the path to exist, and the
/// classifier runs BEFORE the file is written. A determined attack via symlink
/// (e.g. `~/link` pointing at `/etc/hosts`) still bypasses the prefix check.
/// That's a smaller surface than the `..` bypass and is out of scope for this
/// helper; future hardening could add a "canonicalize nearest existing parent"
/// step.
pub(crate) fn normalize_for_policy(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let expanded = expand_home_path(&path.to_path_buf());
    let mut out = PathBuf::new();
    for component in expanded.components() {
        match component {
            Component::ParentDir => { out.pop(); }
            Component::CurDir   => {}
            other               => out.push(other.as_os_str()),
        }
    }
    out
}

// ── macOS command normaliser ──────────────────────────────────────────────────

/// Rewrite shell arg lists that contain GNU/Linux-only flags to macOS equivalents.
///
/// qwen3:8b was trained predominantly on Linux documentation and consistently
/// generates GNU-style flags (`--sort`, `--format`, `--no-header` for ps;
/// `--time-style` for ls) that are illegal on macOS BSD utilities. Prompting the
/// model to use different flags reliably fails — the training-data prior is too
/// strong. This normaliser catches the bad patterns at the execution boundary and
/// substitutes working macOS commands before the OS ever sees the args.
/// Returns a human-readable macOS-correct command string for a shell arg list
/// that may contain GNU/Linux-only flags. Used by the command-query interceptor
/// in orchestrator.rs so the displayed command is always valid BSD syntax even
/// when the model generated GNU-style args.
///
/// Returns `None` if the args are already macOS-safe (no rewrite needed).
pub fn describe_normalized_shell_command(args: &[String]) -> Option<String> {
    if args.first().map(|s| s.as_str()) != Some("ps") {
        return None;
    }
    let has_gnu = args[1..].iter().any(|a| {
        a.starts_with("--sort") || a.starts_with("--format")
            || a.starts_with("--no-header") || a == "--deselect"
    });
    // Also catch -eo which is Linux procps syntax (macOS uses -Ao or -A -o).
    let has_eo = args[1..].iter().any(|a| a == "-eo");
    if !has_gnu && !has_eo {
        return None;
    }
    // Determine intent: memory-focused or CPU-focused.
    // If any arg mentions mem/pmem/rss → sort by %MEM (col 4 in ps aux).
    // Otherwise default to CPU (col 3).
    let wants_memory = args.iter().any(|a| {
        let lc = a.to_lowercase();
        lc.contains("mem") || lc.contains("rss") || lc.contains("pmem")
    });
    if wants_memory {
        Some("ps -Acro pid,pmem,comm".to_string())
    } else {
        Some("ps -Acro pid,pcpu,comm".to_string())
    }
}

fn normalize_shell_args(args: &[String]) -> Vec<String> {
    if args.is_empty() {
        return args.to_vec();
    }
    match args[0].as_str() {
        "ps" => {
            // Detect any GNU-only flag. macOS ps uses BSD syntax; --sort, --format,
            // --no-header, --deselect are all GNU procps extensions.
            // Also catch -eo (Linux procps format specifier; BSD uses -Ao or -A -o).
            let has_gnu = args[1..].iter().any(|a| {
                a.starts_with("--sort")
                    || a.starts_with("--format")
                    || a.starts_with("--no-header")
                    || a == "--deselect"
                    || a == "-eo"
            });
            if has_gnu {
                // Determine intent: memory-focused → sort by %MEM (col 4), else CPU (col 3).
                let wants_memory = args.iter().any(|a| {
                    let lc = a.to_lowercase();
                    lc.contains("mem") || lc.contains("rss") || lc.contains("pmem")
                });
                let pipeline = if wants_memory {
                    "ps aux | sort -rk4 | head -20"
                } else {
                    "ps aux | sort -rk3 | head -20"
                };
                warn!(
                    original = ?args,
                    pipeline = pipeline,
                    "ps: GNU-only flags detected — rewriting to macOS pipeline"
                );
                // Route through bash -c so the pipe is handled by the shell.
                // This is the one place we intentionally use a shell pipeline;
                // the string is hardcoded, not operator-supplied, so injection is N/A.
                return vec![
                    "bash".to_string(),
                    "-c".to_string(),
                    pipeline.to_string(),
                ];
            }
        }
        "ls" => {
            // --time-style=... is a GNU coreutils extension; BSD ls ignores it
            // but some versions print "ls: illegal option" instead.
            let filtered: Vec<String> = args
                .iter()
                .filter(|a| !a.starts_with("--time-style"))
                .cloned()
                .collect();
            if filtered.len() != args.len() {
                warn!(
                    original = ?args,
                    "ls: removed GNU-only --time-style flag"
                );
                return filtered;
            }
        }
        _ => {}
    }
    args.to_vec()
}

// ── execute_shell ─────────────────────────────────────────────────────────────

/// Execute a shell command. Never uses `shell=true`.
///
/// `timeout_secs` caps wall-clock execution time. On timeout the process is killed
/// before returning, preventing zombie subprocesses from accumulating.
///
/// All args have `~` expanded before being passed to the OS — the kernel does not
/// process tilde expansion (that is a shell convenience, not an OS feature).
pub async fn execute_shell(
    args:         &[String],
    working_dir:  Option<&PathBuf>,
    timeout_secs: u64,
) -> ExecutionResult {
    let start = Instant::now();

    if args.is_empty() {
        return ExecutionResult {
            success:     false,
            output:      String::new(),
            error:       "empty args — no command to execute".to_string(),
            exit_code:   None,
            duration_ms: 0,
        };
    }

    let normalized = normalize_shell_args(args);
    let expanded: Vec<String> = normalized.iter().map(|a| expand_home(a)).collect();
    let mut cmd = Command::new(&expanded[0]);
    // Phase 38 / Codex finding [4]: kill the subprocess if our future is dropped
    // (timeout fires, caller cancels). Without this, Tokio's default behavior is
    // to ORPHAN the child — meaning a timed-out `osascript`, `curl`, etc. continues
    // running while we report failure. The previous comment claiming "process was
    // already killed by tokio on timeout" was factually wrong; kill_on_drop(true)
    // is what makes it true.
    cmd.kill_on_drop(true);
    cmd.args(&expanded[1..]);
    if let Some(dir) = working_dir {
        // Expand ~ in working_dir (args are expanded above, but working_dir is a
        // separate field that the model may emit with a ~-prefix or a hallucinated path).
        let expanded_dir = expand_home_path(dir);
        if !expanded_dir.exists() {
            // Phase 38 / Codex finding [5]: previously this silently fell back to
            // the daemon cwd with a `warn!` log. That meant a relative command
            // like `rm -rf build` or `mv file target` could execute against an
            // unintended directory if the model supplied a bad `working_dir` —
            // the operator approved one execution context, the system used
            // another. Failure-fast surfaces the bad path back to the model so
            // it can correct on the continuation turn rather than mutating the
            // wrong tree.
            warn!(
                path = %expanded_dir.display(),
                "working_dir does not exist — refusing to fall back to daemon cwd. \
                 Either omit working_dir or supply a path that exists."
            );
            return ExecutionResult {
                success:     false,
                output:      String::new(),
                error:       format!(
                    "working_dir does not exist: {}. \
                     Refusing to silently use the daemon's working directory; \
                     either omit working_dir or supply a path that exists.",
                    expanded_dir.display()
                ),
                exit_code:   None,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
        cmd.current_dir(&expanded_dir);
    }

    let result = timeout(Duration::from_secs(timeout_secs), cmd.output()).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Err(_elapsed) => {
            // timeout fired — Tokio drops the `cmd.output()` future, which drops
            // the inner Child, which sends SIGKILL via kill_on_drop(true) above.
            ExecutionResult {
                success:     false,
                output:      String::new(),
                error:       format!("timed out after {}s", timeout_secs),
                exit_code:   None,
                duration_ms,
            }
        }
        Ok(Err(io_err)) => {
            // spawn or wait failed (command not found, permission denied, etc.)
            ExecutionResult {
                success:     false,
                output:      String::new(),
                error:       io_err.to_string(),
                exit_code:   None,
                duration_ms,
            }
        }
        Ok(Ok(output)) => {
            ExecutionResult {
                success:     output.status.success(),
                output:      String::from_utf8_lossy(&output.stdout).to_string(),
                error:       String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code:   output.status.code(),
                duration_ms,
            }
        }
    }
}

// ── execute_file_read ─────────────────────────────────────────────────────────

pub async fn execute_file_read(path: &PathBuf) -> ExecutionResult {
    let start = Instant::now();
    let resolved = expand_home_path(path);
    // Read raw bytes first so we can detect binary files gracefully instead of
    // returning a cryptic "stream did not contain valid UTF-8" error that causes
    // the model to loop. Binary files (webm, mp4, etc.) must not be read this way.
    match tokio::fs::read(&resolved).await {
        Err(e) => ExecutionResult {
            success:     false,
            output:      String::new(),
            error:       e.to_string(),
            exit_code:   None,
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Ok(bytes) => match String::from_utf8(bytes.clone()) {
            Ok(content) => ExecutionResult {
                success:     true,
                output:      content,
                error:       String::new(),
                exit_code:   Some(0),
                duration_ms: start.elapsed().as_millis() as u64,
            },
            Err(_) => ExecutionResult {
                success:     false,
                output:      String::new(),
                error:       format!(
                    "binary file ({} bytes) — cannot display as text. \
                     Use `shell: ls ~/Desktop/` to verify existence, or `shell: file <path>` \
                     to inspect type.",
                    bytes.len()
                ),
                exit_code:   None,
                duration_ms: start.elapsed().as_millis() as u64,
            },
        },
    }
}

// ── execute_file_write ────────────────────────────────────────────────────────

pub async fn execute_file_write(
    path:        &PathBuf,
    content:     &str,
    create_dirs: bool,
) -> ExecutionResult {
    let start = Instant::now();
    // Phase 38 / Codex finding [3]: use the same normalizer the policy
    // classifier uses, so the path the OS sees matches the path that was
    // categorized. Without this, `~/../../etc/hosts` was classified as
    // Cautious (raw path didn't match `/etc/`) but written to `/etc/hosts`
    // (kernel resolved `..` after `expand_home_path`).
    let path = &normalize_for_policy(path);

    if create_dirs {
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ExecutionResult {
                    success:     false,
                    output:      String::new(),
                    error:       format!("create_dir_all failed: {e}"),
                    exit_code:   None,
                    duration_ms: start.elapsed().as_millis() as u64,
                };
            }
        }
    }

    let byte_count = content.len();
    match tokio::fs::write(path, content).await {
        Ok(()) => ExecutionResult {
            success:     true,
            output:      format!("wrote {} bytes to {}", byte_count, path.display()),
            error:       String::new(),
            exit_code:   Some(0),
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Err(e) => ExecutionResult {
            success:     false,
            output:      String::new(),
            error:       e.to_string(),
            exit_code:   None,
            duration_ms: start.elapsed().as_millis() as u64,
        },
    }
}

// ── execute_applescript ───────────────────────────────────────────────────────

/// Execute an AppleScript via `osascript -e "..."`. Reuses `execute_shell`
/// since osascript is just a subprocess — same timeout and error handling applies.
pub async fn execute_applescript(script: &str, timeout_secs: u64) -> ExecutionResult {
    let args: Vec<String> = vec![
        "osascript".to_string(),
        "-e".to_string(),
        script.to_string(),
    ];
    execute_shell(&args, None, timeout_secs).await
}

// ── execute_browser ───────────────────────────────────────────────────────────

/// Execute a browser action via the long-lived BrowserCoordinator.
///
/// Translates `BrowserActionKind` → msg_type + JSON payload, calls
/// `coordinator.execute()`, and parses the JSON result into ExecutionResult.
///
/// Returns a failed ExecutionResult (no panic) if:
/// - The coordinator is unavailable (worker not started or permanently crashed)
/// - The command times out (BROWSER_WORKER_RESULT_TIMEOUT_SECS)
/// - The worker returns {"success": false, "error": "..."}
pub async fn execute_browser(
    coordinator:  &BrowserCoordinator,
    action:       &BrowserActionKind,
    _timeout_secs: u64,   // timeout is enforced inside coordinator.execute()
) -> ExecutionResult {
    let start = Instant::now();

    let (msg_type, payload) = build_browser_frame(action);
    let result = coordinator.execute(msg_type, &payload).await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Err(e) => ExecutionResult {
            success:     false,
            output:      String::new(),
            error:       format!("Browser worker error: {e}"),
            exit_code:   None,
            duration_ms,
        },
        Ok(json_str) => {
            match serde_json::from_str::<serde_json::Value>(&json_str) {
                Err(e) => ExecutionResult {
                    success:     false,
                    output:      String::new(),
                    error:       format!("Browser result parse error: {e}"),
                    exit_code:   None,
                    duration_ms,
                },
                Ok(val) => ExecutionResult {
                    success:     val["success"].as_bool().unwrap_or(false),
                    output:      val["output"].as_str().unwrap_or("").to_string(),
                    error:       val["error"].as_str().unwrap_or("").to_string(),
                    exit_code:   None,  // browser actions have no process exit code
                    duration_ms,
                },
            }
        }
    }
}

/// Map a BrowserActionKind to (msg_type, JSON payload bytes).
///
/// The coordinator sends this frame to the Python worker, which dispatches
/// on msg_type to the appropriate handler.
fn build_browser_frame(action: &BrowserActionKind) -> (u8, Vec<u8>) {
    match action {
        BrowserActionKind::Navigate { url } => (
            msg::BROWSER_NAVIGATE,
            serde_json::json!({"url": url}).to_string().into_bytes(),
        ),
        BrowserActionKind::Click { selector } => (
            msg::BROWSER_CLICK,
            serde_json::json!({"selector": selector}).to_string().into_bytes(),
        ),
        BrowserActionKind::Type { selector, text } => (
            msg::BROWSER_TYPE,
            serde_json::json!({"selector": selector, "text": text}).to_string().into_bytes(),
        ),
        BrowserActionKind::Extract { selector } => (
            msg::BROWSER_EXTRACT,
            serde_json::json!({"selector": selector}).to_string().into_bytes(),
        ),
        BrowserActionKind::Screenshot => (
            msg::BROWSER_SCREENSHOT,
            vec![],
        ),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// These tests are NOT gated #[ignore] — they use only safe system commands
// (echo, osascript return value) and temp file operations.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use crate::constants::ACTION_DEFAULT_TIMEOUT_SECS;

    #[tokio::test]
    async fn execute_shell_echo_succeeds() {
        let args: Vec<String> = vec!["echo".to_string(), "hello-dexter".to_string()];
        let result = execute_shell(&args, None, ACTION_DEFAULT_TIMEOUT_SECS).await;
        assert!(result.success, "echo should succeed: {:?}", result.error);
        assert!(result.output.contains("hello-dexter"), "stdout must contain the echoed string");
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn execute_shell_nonexistent_command_fails() {
        let args: Vec<String> = vec!["dexter_no_such_binary_xyz_phase8".to_string()];
        let result = execute_shell(&args, None, ACTION_DEFAULT_TIMEOUT_SECS).await;
        assert!(!result.success, "unknown command must fail");
        assert!(!result.error.is_empty(), "error field must explain why");
    }

    #[tokio::test]
    async fn execute_shell_empty_args_returns_error() {
        let result = execute_shell(&[], None, ACTION_DEFAULT_TIMEOUT_SECS).await;
        assert!(!result.success);
        assert!(!result.error.is_empty());
    }

    #[tokio::test]
    async fn execute_file_read_reads_temp_file() {
        let tmp    = tempdir().unwrap();
        let path   = tmp.path().join("test.txt");
        let expect = "dexter phase 8 test content";
        std::fs::write(&path, expect).unwrap();

        let result = execute_file_read(&path).await;
        assert!(result.success, "read should succeed: {:?}", result.error);
        assert_eq!(result.output.trim(), expect);
    }

    #[tokio::test]
    async fn execute_file_read_nonexistent_fails() {
        let path = PathBuf::from("/tmp/dexter_phase8_nonexistent_xyz.txt");
        let result = execute_file_read(&path).await;
        assert!(!result.success);
        assert!(!result.error.is_empty());
    }

    #[tokio::test]
    async fn execute_file_write_creates_and_reads_back() {
        let tmp   = tempdir().unwrap();
        let path  = tmp.path().join("out.txt");
        let data  = "written by dexter phase 8";

        let wr = execute_file_write(&path, data, false).await;
        assert!(wr.success, "write should succeed: {:?}", wr.error);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, data);
    }

    #[tokio::test]
    async fn execute_file_write_with_create_dirs() {
        let tmp  = tempdir().unwrap();
        let path = tmp.path().join("nested/dir/out.txt");
        let data = "nested write";

        let wr = execute_file_write(&path, data, true).await;
        assert!(wr.success, "write with create_dirs should succeed: {:?}", wr.error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), data);
    }

    #[tokio::test]
    async fn execute_applescript_return_value() {
        // osascript -e 'return "dexter_ok"' → stdout: "dexter_ok\n"
        let result = execute_applescript(r#"return "dexter_ok""#, ACTION_DEFAULT_TIMEOUT_SECS).await;
        assert!(result.success, "osascript should succeed: {:?}", result.error);
        assert!(
            result.output.contains("dexter_ok"),
            "osascript stdout must contain return value, got: {:?}", result.output
        );
    }

    // ── Phase 38 / Codex finding [5]: working_dir failure-fast ────────────────

    #[tokio::test]
    async fn execute_shell_missing_working_dir_returns_error() {
        // Pre-Phase-38 behavior: silently fall back to daemon cwd with a warn!,
        // potentially mutating an unrelated tree. Now we fail explicitly so the
        // model gets the bad-path back and can correct on the continuation turn.
        let bad_dir  = PathBuf::from("/tmp/dexter_phase38_no_such_dir_xyz");
        let args     = vec!["echo".to_string(), "hi".to_string()];
        let result   = execute_shell(&args, Some(&bad_dir), ACTION_DEFAULT_TIMEOUT_SECS).await;

        assert!(!result.success, "missing working_dir must fail the action");
        assert!(
            result.error.contains("working_dir does not exist"),
            "error must name the failure mode, got: {:?}", result.error
        );
        assert!(
            result.error.contains("dexter_phase38_no_such_dir_xyz"),
            "error must include the bad path so the model can correct it, got: {:?}", result.error
        );
    }

    #[tokio::test]
    async fn execute_shell_existing_working_dir_succeeds() {
        // Regression guard: a valid working_dir must still work normally.
        let tmp  = tempdir().unwrap();
        let dir  = tmp.path().to_path_buf();
        let args = vec!["echo".to_string(), "hi".to_string()];
        let result = execute_shell(&args, Some(&dir), ACTION_DEFAULT_TIMEOUT_SECS).await;

        assert!(result.success, "valid working_dir should succeed: {:?}", result.error);
        assert!(result.output.contains("hi"));
    }

    // ── Phase 38 / Codex finding [3]: normalize_for_policy unit coverage ─────

    #[test]
    fn normalize_for_policy_collapses_dotdot() {
        let normalized = normalize_for_policy(std::path::Path::new("/Users/jason/../../etc/hosts"));
        assert_eq!(normalized, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn normalize_for_policy_collapses_curdir() {
        let normalized = normalize_for_policy(std::path::Path::new("/Users/jason/./notes/./file.txt"));
        assert_eq!(normalized, PathBuf::from("/Users/jason/notes/file.txt"));
    }

    #[test]
    fn normalize_for_policy_expands_tilde() {
        // Use a path with a known-shape result regardless of $HOME's actual value.
        // We can at least assert the leading ~ is gone and the result is absolute.
        let normalized = normalize_for_policy(std::path::Path::new("~/file.txt"));
        let s = normalized.to_string_lossy().to_string();
        assert!(!s.starts_with("~"), "tilde must be expanded, got {s}");
        assert!(s.starts_with("/"), "result must be absolute, got {s}");
        assert!(s.ends_with("/file.txt"), "filename must be preserved, got {s}");
    }

    #[test]
    fn normalize_for_policy_tilde_and_dotdot_combine() {
        // The exact attack: ~/../../etc/hosts. Normalize must collapse both.
        let normalized = normalize_for_policy(std::path::Path::new("~/../../etc/hosts"));
        let s = normalized.to_string_lossy().to_string();
        assert!(s.ends_with("/etc/hosts") || s == "/etc/hosts",
                "tilde + dotdot must normalize to a system path so policy can flag it, got {s}");
    }
}
