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

use crate::{browser::coordinator::BrowserCoordinator, voice::protocol::msg};

use super::engine::BrowserActionKind;

// ── ExecutionResult ───────────────────────────────────────────────────────────

/// The raw result of executing one action. All fields are always populated;
/// the caller (ActionEngine) decides what to log and what to surface.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Process exited 0 / IO succeeded.
    pub success: bool,
    /// Stdout (shell/AppleScript) or file content (file_read) or bytes-written note (file_write).
    pub output: String,
    /// Stderr or IO error description. Empty string on success.
    pub error: String,
    /// Process exit code. `None` for pure IO operations (file_read/file_write), timeouts.
    pub exit_code: Option<i32>,
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
///   (3) Canonicalize the nearest existing parent, then re-append any missing
///       suffix. This resolves symlinked parents before policy classification
///       without requiring the final target file to exist.
pub(crate) fn normalize_for_policy(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let expanded = expand_home_path(&path.to_path_buf());
    let mut out = PathBuf::new();
    for component in expanded.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    canonicalize_nearest_existing_parent(&out)
}

fn canonicalize_nearest_existing_parent(path: &std::path::Path) -> PathBuf {
    let mut probe = path.to_path_buf();
    let mut suffix = Vec::new();

    loop {
        if probe.as_os_str().is_empty() {
            if let Ok(mut resolved) = std::env::current_dir() {
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                return resolved;
            }
            return path.to_path_buf();
        }

        if probe.exists() {
            match std::fs::canonicalize(&probe) {
                Ok(mut resolved) => {
                    for part in suffix.iter().rev() {
                        resolved.push(part);
                    }
                    return resolved;
                }
                Err(_) => return path.to_path_buf(),
            }
        }

        let Some(name) = probe.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(PathBuf::from(name.to_os_string()));
        if !probe.pop() {
            return path.to_path_buf();
        }
    }
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
        a.starts_with("--sort")
            || a.starts_with("--format")
            || a.starts_with("--no-header")
            || a == "--deselect"
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
                return vec!["bash".to_string(), "-c".to_string(), pipeline.to_string()];
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
    args: &[String],
    working_dir: Option<&PathBuf>,
    timeout_secs: u64,
) -> ExecutionResult {
    let start = Instant::now();

    if args.is_empty() {
        return ExecutionResult {
            success: false,
            output: String::new(),
            error: "empty args — no command to execute".to_string(),
            exit_code: None,
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
                success: false,
                output: String::new(),
                error: format!(
                    "working_dir does not exist: {}. \
                     Refusing to silently use the daemon's working directory; \
                     either omit working_dir or supply a path that exists.",
                    expanded_dir.display()
                ),
                exit_code: None,
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
                success: false,
                output: String::new(),
                error: format!("timed out after {}s", timeout_secs),
                exit_code: None,
                duration_ms,
            }
        }
        Ok(Err(io_err)) => {
            // spawn or wait failed (command not found, permission denied, etc.)
            ExecutionResult {
                success: false,
                output: String::new(),
                error: io_err.to_string(),
                exit_code: None,
                duration_ms,
            }
        }
        Ok(Ok(output)) => ExecutionResult {
            success: output.status.success(),
            output: String::from_utf8_lossy(&output.stdout).to_string(),
            error: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
            duration_ms,
        },
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
            success: false,
            output: String::new(),
            error: e.to_string(),
            exit_code: None,
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Ok(bytes) => match String::from_utf8(bytes.clone()) {
            Ok(content) => ExecutionResult {
                success: true,
                output: content,
                error: String::new(),
                exit_code: Some(0),
                duration_ms: start.elapsed().as_millis() as u64,
            },
            Err(_) => ExecutionResult {
                success: false,
                output: String::new(),
                error: format!(
                    "binary file ({} bytes) — cannot display as text. \
                     Use `shell: ls ~/Desktop/` to verify existence, or `shell: file <path>` \
                     to inspect type.",
                    bytes.len()
                ),
                exit_code: None,
                duration_ms: start.elapsed().as_millis() as u64,
            },
        },
    }
}

// ── execute_file_write ────────────────────────────────────────────────────────

pub async fn execute_file_write(
    path: &PathBuf,
    content: &str,
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
                    success: false,
                    output: String::new(),
                    error: format!("create_dir_all failed: {e}"),
                    exit_code: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                };
            }
        }
    }

    let byte_count = content.len();
    match tokio::fs::write(path, content).await {
        Ok(()) => ExecutionResult {
            success: true,
            output: format!("wrote {} bytes to {}", byte_count, path.display()),
            error: String::new(),
            exit_code: Some(0),
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Err(e) => ExecutionResult {
            success: false,
            output: String::new(),
            error: e.to_string(),
            exit_code: None,
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

// ── execute_window_focus ─────────────────────────────────────────────────────

/// Focus an application, optionally raising the first window whose title contains
/// a requested substring.
///
/// This is intentionally narrower than arbitrary AppleScript: the model supplies
/// structured app/window labels, and Rust builds the script with literal escaping.
/// No clicks, keystrokes, or text entry happen here.
pub async fn execute_window_focus(
    app_name: &str,
    title_contains: Option<&str>,
    timeout_secs: u64,
) -> ExecutionResult {
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return ExecutionResult {
            success: false,
            output: String::new(),
            error: "window_focus app_name must not be empty".to_string(),
            exit_code: None,
            duration_ms: 0,
        };
    }

    let title_contains = title_contains
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let script = build_window_focus_script(app_name, title_contains);
    execute_applescript(&script, timeout_secs).await
}

fn build_window_focus_script(app_name: &str, title_contains: &str) -> String {
    let app = escape_applescript_literal(app_name);
    let title = escape_applescript_literal(title_contains);
    format!(
        r#"set targetAppName to "{app}"
set wantedTitle to "{title}"

tell application targetAppName to activate

tell application "System Events"
    set targetProcess to first process whose name is targetAppName
    set frontmost of targetProcess to true
    delay 0.05

    if wantedTitle is not "" then
        repeat with candidateWindow in windows of targetProcess
            set candidateTitle to ""
            try
                set candidateTitle to name of candidateWindow as text
            end try
            if candidateTitle contains wantedTitle then
                try
                    perform action "AXRaise" of candidateWindow
                end try
                try
                    set focused of candidateWindow to true
                end try
                return "focused " & targetAppName & " window: " & candidateTitle
            end if
        end repeat
        return "focused " & targetAppName & "; no visible window title contained: " & wantedTitle
    end if

    if (count of windows of targetProcess) > 0 then
        try
            perform action "AXRaise" of window 1 of targetProcess
        end try
        return "focused " & targetAppName
    end if

    return "focused " & targetAppName & "; no visible windows reported"
end tell"#
    )
}

// ── execute_window_inspect ───────────────────────────────────────────────────

/// Inspect the current frontmost app/window, or a named app's visible windows.
///
/// This is read-only System Events observation. It does not activate apps,
/// raise windows, click, type, or mutate UI state. The output is line-oriented
/// so it can be injected back into the model context and shown in receipts.
pub async fn execute_window_inspect(app_name: Option<&str>, timeout_secs: u64) -> ExecutionResult {
    let app_name = app_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let script = build_window_inspect_script(app_name);
    execute_applescript(&script, timeout_secs).await
}

fn build_window_inspect_script(app_name: &str) -> String {
    let app = escape_applescript_literal(app_name);
    format!(
        r#"set requestedAppName to "{app}"

tell application "System Events"
    if requestedAppName is "" then
        set targetProcess to first application process whose frontmost is true
    else
        set matchingProcesses to application processes whose name is requestedAppName
        if (count of matchingProcesses) is 0 then
            return "window inspection failed: app not running: " & requestedAppName
        end if
        set targetProcess to item 1 of matchingProcesses
    end if

    set targetAppName to name of targetProcess as text
    set isFrontmost to frontmost of targetProcess
    set frontWindowTitle to ""
    try
        set frontWindowTitle to name of front window of targetProcess as text
    end try

    set windowTitles to {{}}
    repeat with candidateWindow in windows of targetProcess
        set candidateTitle to ""
        try
            set candidateTitle to name of candidateWindow as text
        end try
        if candidateTitle is not "" then
            set end of windowTitles to candidateTitle
        end if
    end repeat

    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to linefeed
    set windowText to windowTitles as text
    set AppleScript's text item delimiters to oldDelimiters
    if windowText is "" then
        set windowText to "(none)"
    end if
    if frontWindowTitle is "" then
        set frontWindowTitle to "(none)"
    end if

    return "inspected app: " & targetAppName & linefeed & "frontmost: " & (isFrontmost as text) & linefeed & "front window: " & frontWindowTitle & linefeed & "visible windows:" & linefeed & windowText
end tell"#
    )
}

// ── execute_ui_snapshot ──────────────────────────────────────────────────────

/// Capture a bounded, read-only snapshot of actionable controls in a window.
///
/// This is the next step after `window_inspect`: it does not activate, raise,
/// click, type, or mutate UI state. It only reads Accessibility metadata for the
/// front window of the requested app, or for the frontmost app when `app_name`
/// is absent. Secure text field values are never read.
pub async fn execute_ui_snapshot(
    app_name: Option<&str>,
    max_depth: Option<u8>,
    timeout_secs: u64,
) -> ExecutionResult {
    let app_name = app_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let depth = max_depth.unwrap_or(2).clamp(1, 4);
    let script = build_ui_snapshot_script(app_name, depth);
    execute_applescript(&script, timeout_secs).await
}

fn build_ui_snapshot_script(app_name: &str, max_depth: u8) -> String {
    let app = escape_applescript_literal(app_name);
    format!(
        r#"set requestedAppName to "{app}"
set maxDepth to {max_depth}
set maxRows to 80

on cleanText(rawValue)
    try
        set textValue to rawValue as text
    on error
        return ""
    end try
    set textValue to my replaceText(textValue, return, " ")
    set textValue to my replaceText(textValue, linefeed, " ")
    set textValue to my replaceText(textValue, tab, " ")
    repeat while textValue contains "  "
        set textValue to my replaceText(textValue, "  ", " ")
    end repeat
    if (length of textValue) > 120 then
        return (text 1 thru 120 of textValue) & "..."
    end if
    return textValue
end cleanText

on replaceText(sourceText, searchText, replacementText)
    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to searchText
    set textItems to text items of sourceText
    set AppleScript's text item delimiters to replacementText
    set joinedText to textItems as text
    set AppleScript's text item delimiters to oldDelimiters
    return joinedText
end replaceText

on safeProperty(uiElement, propertyName)
    try
        tell application "System Events"
            if propertyName is "role" then
                set rawValue to role of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "name" then
                set rawValue to name of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "description" then
                set rawValue to description of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "value" then
                try
                    if (role of uiElement as text) is "AXSecureTextField" then return ""
                end try
                set rawValue to value of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
        end tell
    end try
    return ""
end safeProperty

on isInterestingRole(roleName)
    if roleName is "" then return false
    set interestingRoles to {{"AXButton", "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXMenuButton", "AXTextField", "AXTextArea", "AXComboBox", "AXSearchField", "AXLink", "AXTabGroup", "AXTable", "AXOutline", "AXList", "AXScrollArea", "AXGroup", "AXToolbar"}}
    return interestingRoles contains roleName
end isInterestingRole

on elementLine(uiElement)
    set roleName to my safeProperty(uiElement, "role")
    if not my isInterestingRole(roleName) then return ""
    set labelParts to {{}}
    set elementName to my safeProperty(uiElement, "name")
    set elementDescription to my safeProperty(uiElement, "description")
    set elementValue to my safeProperty(uiElement, "value")
    if elementName is not "" then set end of labelParts to "name=" & quoted form of elementName
    if elementDescription is not "" and elementDescription is not elementName then set end of labelParts to "description=" & quoted form of elementDescription
    if elementValue is not "" and elementValue is not elementName and elementValue is not elementDescription then set end of labelParts to "value=" & quoted form of elementValue

    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to " | "
    set labelsText to labelParts as text
    set AppleScript's text item delimiters to oldDelimiters
    if labelsText is "" then return "- " & roleName
    return "- " & roleName & " | " & labelsText
end elementLine

on collectControls(uiElement, currentDepth, allowedDepth)
    set rows to {{}}
    if currentDepth > allowedDepth then return rows
    try
        tell application "System Events"
            set childElements to UI elements of uiElement
        end tell
    on error
        return rows
    end try
    repeat with childElement in childElements
        set rowText to my elementLine(childElement)
        if rowText is not "" then set end of rows to rowText
        if currentDepth < allowedDepth then
            set nestedRows to my collectControls(childElement, currentDepth + 1, allowedDepth)
            repeat with nestedRow in nestedRows
                set end of rows to nestedRow as text
            end repeat
        end if
    end repeat
    return rows
end collectControls

tell application "System Events"
    if requestedAppName is "" then
        set targetProcess to first application process whose frontmost is true
    else
        set matchingProcesses to application processes whose name is requestedAppName
        if (count of matchingProcesses) is 0 then
            return "ui snapshot failed: app not running: " & requestedAppName
        end if
        set targetProcess to item 1 of matchingProcesses
    end if

    set targetAppName to name of targetProcess as text
    set isFrontmost to frontmost of targetProcess
    set frontWindowTitle to "(none)"
    try
        set targetWindow to front window of targetProcess
        set frontWindowTitle to my cleanText(name of targetWindow)
    on error
        return "ui snapshot app: " & targetAppName & linefeed & "frontmost: " & (isFrontmost as text) & linefeed & "front window: (none)" & linefeed & "controls:" & linefeed & "(no front window)"
    end try

    set focusedLine to "(none)"
    try
        set focusedElement to value of attribute "AXFocusedUIElement" of targetProcess
        set focusedLine to my elementLine(focusedElement)
        if focusedLine is "" then set focusedLine to my safeProperty(focusedElement, "role")
        if focusedLine is "" then set focusedLine to "(unavailable)"
    end try

    set controlRows to my collectControls(targetWindow, 1, maxDepth)
    set boundedRows to {{}}
    repeat with rowText in controlRows
        if (count of boundedRows) is greater than or equal to maxRows then exit repeat
        set end of boundedRows to rowText as text
    end repeat

    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to linefeed
    set controlsText to boundedRows as text
    set AppleScript's text item delimiters to oldDelimiters
    if controlsText is "" then set controlsText to "(no actionable controls found)"

    return "ui snapshot app: " & targetAppName & linefeed & "frontmost: " & (isFrontmost as text) & linefeed & "front window: " & frontWindowTitle & linefeed & "focused element: " & focusedLine & linefeed & "controls:" & linefeed & controlsText
end tell"#
    )
}

// ── execute_ui_click ─────────────────────────────────────────────────────────

/// Press one visible Accessibility control by role/label.
///
/// This is intentionally narrower than raw AppleScript or coordinate clicking:
/// it targets the front window of a named app, or the current frontmost app, and
/// presses exactly one unambiguous control found by Accessibility metadata.
pub async fn execute_ui_click(
    app_name: Option<&str>,
    role: Option<&str>,
    label: &str,
    max_depth: Option<u8>,
    timeout_secs: u64,
) -> ExecutionResult {
    let app_name = app_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let role = role
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let label = label.trim();
    if label.is_empty() {
        return ExecutionResult {
            success: false,
            output: String::new(),
            error: "ui_click label must not be empty".to_string(),
            exit_code: None,
            duration_ms: 0,
        };
    }
    let depth = max_depth.unwrap_or(2).clamp(1, 4);
    let script = build_ui_click_script(app_name, role, label, depth);
    execute_applescript(&script, timeout_secs).await
}

fn build_ui_click_script(app_name: &str, role: &str, label: &str, max_depth: u8) -> String {
    let app = escape_applescript_literal(app_name);
    let role = escape_applescript_literal(role);
    let label = escape_applescript_literal(label);
    format!(
        r#"set requestedAppName to "{app}"
set requestedRole to "{role}"
set requestedLabel to "{label}"
set maxDepth to {max_depth}
set maxRows to 80

on cleanText(rawValue)
    try
        set textValue to rawValue as text
    on error
        return ""
    end try
    set textValue to my replaceText(textValue, return, " ")
    set textValue to my replaceText(textValue, linefeed, " ")
    set textValue to my replaceText(textValue, tab, " ")
    repeat while textValue contains "  "
        set textValue to my replaceText(textValue, "  ", " ")
    end repeat
    if (length of textValue) > 120 then
        return (text 1 thru 120 of textValue) & "..."
    end if
    return textValue
end cleanText

on replaceText(sourceText, searchText, replacementText)
    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to searchText
    set textItems to text items of sourceText
    set AppleScript's text item delimiters to replacementText
    set joinedText to textItems as text
    set AppleScript's text item delimiters to oldDelimiters
    return joinedText
end replaceText

on safeProperty(uiElement, propertyName)
    try
        tell application "System Events"
            if propertyName is "role" then
                set rawValue to role of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "name" then
                set rawValue to name of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "description" then
                set rawValue to description of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "value" then
                try
                    if (role of uiElement as text) is "AXSecureTextField" then return ""
                end try
                set rawValue to value of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
        end tell
    end try
    return ""
end safeProperty

on isInterestingRole(roleName)
    if roleName is "" then return false
    set interestingRoles to {{"AXButton", "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXMenuButton", "AXComboBox", "AXLink", "AXTabGroup", "AXToolbar"}}
    return interestingRoles contains roleName
end isInterestingRole

on isEnabledControl(uiElement)
    try
        tell application "System Events"
            set rawEnabled to enabled of uiElement
            if rawEnabled is missing value then return true
            return rawEnabled as boolean
        end tell
    end try
    return true
end isEnabledControl

on labelMatches(uiElement, wantedLabel, exactMatch)
    set candidateLabels to {{}}
    set elementName to my safeProperty(uiElement, "name")
    set elementDescription to my safeProperty(uiElement, "description")
    set elementValue to my safeProperty(uiElement, "value")
    if elementName is not "" then set end of candidateLabels to elementName
    if elementDescription is not "" then set end of candidateLabels to elementDescription
    if elementValue is not "" then set end of candidateLabels to elementValue

    repeat with candidateLabel in candidateLabels
        set candidateText to candidateLabel as text
        ignoring case
            if exactMatch then
                if candidateText is wantedLabel then return true
            else
                if candidateText contains wantedLabel then return true
            end if
        end ignoring
    end repeat
    return false
end labelMatches

on elementSummary(uiElement)
    set roleName to my safeProperty(uiElement, "role")
    set elementName to my safeProperty(uiElement, "name")
    set elementDescription to my safeProperty(uiElement, "description")
    set elementValue to my safeProperty(uiElement, "value")
    set labelParts to {{}}
    if elementName is not "" then set end of labelParts to "name=" & quoted form of elementName
    if elementDescription is not "" and elementDescription is not elementName then set end of labelParts to "description=" & quoted form of elementDescription
    if elementValue is not "" and elementValue is not elementName and elementValue is not elementDescription then set end of labelParts to "value=" & quoted form of elementValue
    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to " | "
    set labelsText to labelParts as text
    set AppleScript's text item delimiters to oldDelimiters
    if labelsText is "" then return roleName
    return roleName & " | " & labelsText
end elementSummary

on collectMatches(uiElement, currentDepth, allowedDepth, wantedRole, wantedLabel, exactMatch, rowLimit)
    set matches to {{}}
    if currentDepth > allowedDepth then return matches
    try
        tell application "System Events"
            set childElements to UI elements of uiElement
        end tell
    on error
        return matches
    end try
    repeat with childElement in childElements
        set roleName to my safeProperty(childElement, "role")
        set roleOk to true
        if wantedRole is not "" and roleName is not wantedRole then set roleOk to false
        if roleOk and my isInterestingRole(roleName) and my labelMatches(childElement, wantedLabel, exactMatch) then
            set end of matches to childElement
        end if
        if currentDepth < allowedDepth then
            set nestedMatches to my collectMatches(childElement, currentDepth + 1, allowedDepth, wantedRole, wantedLabel, exactMatch, rowLimit)
            repeat with nestedMatch in nestedMatches
                set end of matches to nestedMatch
            end repeat
        end if
        if (count of matches) is greater than rowLimit then exit repeat
    end repeat
    return matches
end collectMatches

tell application "System Events"
    if requestedAppName is "" then
        set targetProcess to first application process whose frontmost is true
    else
        set matchingProcesses to application processes whose name is requestedAppName
        if (count of matchingProcesses) is 0 then
            error "ui control press failed: app not running: " & requestedAppName number 1728
        end if
        set targetProcess to item 1 of matchingProcesses
    end if

    set targetAppName to name of targetProcess as text
    set frontWindowTitle to "(none)"
    try
        set targetWindow to front window of targetProcess
        set frontWindowTitle to my cleanText(name of targetWindow)
    on error
        set targetWindow to targetProcess
        set frontWindowTitle to "(process root)"
    end try
end tell

set exactMatches to my collectMatches(targetWindow, 1, maxDepth, requestedRole, requestedLabel, true, maxRows)
if (count of exactMatches) is 1 then
    set targetElement to item 1 of exactMatches
else if (count of exactMatches) is greater than 1 then
    error "ui control press failed: ambiguous exact match for " & quoted form of requestedLabel number 1728
else
    set fuzzyMatches to my collectMatches(targetWindow, 1, maxDepth, requestedRole, requestedLabel, false, maxRows)
    if (count of fuzzyMatches) is 0 then
        error "ui control press failed: no matching control for " & quoted form of requestedLabel number 1728
    else if (count of fuzzyMatches) is greater than 1 then
        error "ui control press failed: ambiguous partial match for " & quoted form of requestedLabel number 1728
    end if
    set targetElement to item 1 of fuzzyMatches
end if

if not my isEnabledControl(targetElement) then
    error "ui control press failed: matched control is disabled: " & my elementSummary(targetElement) number 1728
end if

set targetSummary to my elementSummary(targetElement)
tell application "System Events"
    perform action "AXPress" of targetElement
end tell
return "pressed UI control: " & targetSummary & linefeed & "app: " & targetAppName & linefeed & "front window: " & frontWindowTitle"#
    )
}

// ── execute_ui_type ──────────────────────────────────────────────────────────

/// Set the value of one visible text-entry Accessibility control.
///
/// The target must resolve to exactly one typeable control. The typed text is
/// passed only to the executor script; audit surfaces record its length instead
/// of the content.
pub async fn execute_ui_type(
    app_name: Option<&str>,
    role: Option<&str>,
    label: Option<&str>,
    text: &str,
    max_depth: Option<u8>,
    timeout_secs: u64,
) -> ExecutionResult {
    let app_name = app_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let role = role
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let label = label
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    if role.is_empty() && label.is_empty() {
        return ExecutionResult {
            success: false,
            output: String::new(),
            error: "ui_type requires a role or label".to_string(),
            exit_code: None,
            duration_ms: 0,
        };
    }
    let depth = max_depth.unwrap_or(2).clamp(1, 4);
    let script = build_ui_type_script(app_name, role, label, text, depth);
    execute_applescript(&script, timeout_secs).await
}

fn build_ui_type_script(
    app_name: &str,
    role: &str,
    label: &str,
    text: &str,
    max_depth: u8,
) -> String {
    let app = escape_applescript_literal(app_name);
    let role = escape_applescript_literal(role);
    let label = escape_applescript_literal(label);
    let text = escape_applescript_literal(text);
    format!(
        r#"set requestedAppName to "{app}"
set requestedRole to "{role}"
set requestedLabel to "{label}"
set requestedText to "{text}"
set maxDepth to {max_depth}
set maxRows to 80

on cleanText(rawValue)
    try
        set textValue to rawValue as text
    on error
        return ""
    end try
    set textValue to my replaceText(textValue, return, " ")
    set textValue to my replaceText(textValue, linefeed, " ")
    set textValue to my replaceText(textValue, tab, " ")
    repeat while textValue contains "  "
        set textValue to my replaceText(textValue, "  ", " ")
    end repeat
    if (length of textValue) > 120 then
        return (text 1 thru 120 of textValue) & "..."
    end if
    return textValue
end cleanText

on replaceText(sourceText, searchText, replacementText)
    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to searchText
    set textItems to text items of sourceText
    set AppleScript's text item delimiters to replacementText
    set joinedText to textItems as text
    set AppleScript's text item delimiters to oldDelimiters
    return joinedText
end replaceText

on safeProperty(uiElement, propertyName)
    try
        tell application "System Events"
            if propertyName is "role" then
                set rawValue to role of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "name" then
                set rawValue to name of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "description" then
                set rawValue to description of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
            if propertyName is "value" then
                try
                    if (role of uiElement as text) is "AXSecureTextField" then return ""
                end try
                set rawValue to value of uiElement
                if rawValue is missing value then return ""
                return my cleanText(rawValue)
            end if
        end tell
    end try
    return ""
end safeProperty

on isTypeableRole(roleName)
    if roleName is "" then return false
    set typeableRoles to {{"AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"}}
    return typeableRoles contains roleName
end isTypeableRole

on labelMatches(uiElement, wantedLabel, exactMatch)
    if wantedLabel is "" then return true
    set candidateLabels to {{}}
    set elementName to my safeProperty(uiElement, "name")
    set elementDescription to my safeProperty(uiElement, "description")
    set elementValue to my safeProperty(uiElement, "value")
    if elementName is not "" then set end of candidateLabels to elementName
    if elementDescription is not "" then set end of candidateLabels to elementDescription
    if elementValue is not "" then set end of candidateLabels to elementValue

    repeat with candidateLabel in candidateLabels
        set candidateText to candidateLabel as text
        ignoring case
            if exactMatch then
                if candidateText is wantedLabel then return true
            else
                if candidateText contains wantedLabel then return true
            end if
        end ignoring
    end repeat
    return false
end labelMatches

on elementSummary(uiElement)
    set roleName to my safeProperty(uiElement, "role")
    set elementName to my safeProperty(uiElement, "name")
    set elementDescription to my safeProperty(uiElement, "description")
    set elementValue to my safeProperty(uiElement, "value")
    set labelParts to {{}}
    if elementName is not "" then set end of labelParts to "name=" & quoted form of elementName
    if elementDescription is not "" and elementDescription is not elementName then set end of labelParts to "description=" & quoted form of elementDescription
    if elementValue is not "" and elementValue is not elementName and elementValue is not elementDescription then set end of labelParts to "value=" & quoted form of elementValue
    set oldDelimiters to AppleScript's text item delimiters
    set AppleScript's text item delimiters to " | "
    set labelsText to labelParts as text
    set AppleScript's text item delimiters to oldDelimiters
    if labelsText is "" then return roleName
    return roleName & " | " & labelsText
end elementSummary

on collectTypeTargets(uiElement, currentDepth, allowedDepth, wantedRole, wantedLabel, exactMatch, rowLimit)
    set matches to {{}}
    if currentDepth > allowedDepth then return matches
    try
        tell application "System Events"
            set childElements to UI elements of uiElement
        end tell
    on error
        return matches
    end try
    repeat with childElement in childElements
        set roleName to my safeProperty(childElement, "role")
        set roleOk to true
        if wantedRole is not "" and roleName is not wantedRole then set roleOk to false
        if roleOk and my isTypeableRole(roleName) and my labelMatches(childElement, wantedLabel, exactMatch) then
            set end of matches to childElement
        end if
        if currentDepth < allowedDepth then
            set nestedMatches to my collectTypeTargets(childElement, currentDepth + 1, allowedDepth, wantedRole, wantedLabel, exactMatch, rowLimit)
            repeat with nestedMatch in nestedMatches
                set end of matches to nestedMatch
            end repeat
        end if
        if (count of matches) is greater than rowLimit then exit repeat
    end repeat
    return matches
end collectTypeTargets

tell application "System Events"
    if requestedAppName is "" then
        set targetProcess to first application process whose frontmost is true
    else
        set matchingProcesses to application processes whose name is requestedAppName
        if (count of matchingProcesses) is 0 then
            error "ui type failed: app not running: " & requestedAppName number 1728
        end if
        set targetProcess to item 1 of matchingProcesses
    end if

    set targetAppName to name of targetProcess as text
    set frontWindowTitle to "(none)"
    try
        set targetWindow to front window of targetProcess
        set frontWindowTitle to my cleanText(name of targetWindow)
    on error
        set targetWindow to targetProcess
        set frontWindowTitle to "(process root)"
    end try
end tell

set exactMatches to my collectTypeTargets(targetWindow, 1, maxDepth, requestedRole, requestedLabel, true, maxRows)
if (count of exactMatches) is 1 then
    set targetElement to item 1 of exactMatches
else if (count of exactMatches) is greater than 1 then
    error "ui type failed: ambiguous exact match for " & quoted form of requestedLabel number 1728
else
    if requestedLabel is "" then
        error "ui type failed: no matching text control for role " & quoted form of requestedRole number 1728
    end if
    set fuzzyMatches to my collectTypeTargets(targetWindow, 1, maxDepth, requestedRole, requestedLabel, false, maxRows)
    if (count of fuzzyMatches) is 0 then
        error "ui type failed: no matching text control for " & quoted form of requestedLabel number 1728
    else if (count of fuzzyMatches) is greater than 1 then
        error "ui type failed: ambiguous partial match for " & quoted form of requestedLabel number 1728
    end if
    set targetElement to item 1 of fuzzyMatches
end if

set targetSummary to my elementSummary(targetElement)
tell application "System Events"
    try
        set value of targetElement to requestedText
    on error errMsg number errNum
        error "ui type failed: could not set text value: " & errMsg number errNum
    end try
end tell
return "typed into UI control: " & targetSummary & linefeed & "app: " & targetAppName & linefeed & "front window: " & frontWindowTitle & linefeed & "text: <" & ((length of requestedText) as text) & " chars>"
"#
    )
}

fn escape_applescript_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push(' '),
            ch if ch.is_control() => out.push(' '),
            ch => out.push(ch),
        }
    }
    out
}

// ── execute_shortcut ─────────────────────────────────────────────────────────

/// Execute a macOS Shortcut via `/usr/bin/shortcuts run`.
///
/// Arguments are passed as argv entries, never through a shell, so shortcut names
/// and paths cannot introduce shell metacharacter injection.
pub async fn execute_shortcut(
    name: &str,
    input_path: Option<&PathBuf>,
    output_path: Option<&PathBuf>,
    timeout_secs: u64,
) -> ExecutionResult {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return ExecutionResult {
            success: false,
            output: String::new(),
            error: "shortcut name must not be empty".to_string(),
            exit_code: None,
            duration_ms: 0,
        };
    }

    let mut args = vec![
        "/usr/bin/shortcuts".to_string(),
        "run".to_string(),
        trimmed_name.to_string(),
    ];
    if let Some(path) = input_path {
        args.push("--input-path".to_string());
        args.push(path.to_string_lossy().to_string());
    }
    if let Some(path) = output_path {
        args.push("--output-path".to_string());
        args.push(path.to_string_lossy().to_string());
    }

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
    coordinator: &BrowserCoordinator,
    action: &BrowserActionKind,
    _timeout_secs: u64, // timeout is enforced inside coordinator.execute()
) -> ExecutionResult {
    let start = Instant::now();

    let (msg_type, payload) = build_browser_frame(action);
    let result = coordinator.execute(msg_type, &payload).await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Err(e) => ExecutionResult {
            success: false,
            output: String::new(),
            error: format!("Browser worker error: {e}"),
            exit_code: None,
            duration_ms,
        },
        Ok(json_str) => {
            match serde_json::from_str::<serde_json::Value>(&json_str) {
                Err(e) => ExecutionResult {
                    success: false,
                    output: String::new(),
                    error: format!("Browser result parse error: {e}"),
                    exit_code: None,
                    duration_ms,
                },
                Ok(val) => ExecutionResult {
                    success: val["success"].as_bool().unwrap_or(false),
                    output: val["output"].as_str().unwrap_or("").to_string(),
                    error: val["error"].as_str().unwrap_or("").to_string(),
                    exit_code: None, // browser actions have no process exit code
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
            serde_json::json!({"selector": selector})
                .to_string()
                .into_bytes(),
        ),
        BrowserActionKind::Type { selector, text } => (
            msg::BROWSER_TYPE,
            serde_json::json!({"selector": selector, "text": text})
                .to_string()
                .into_bytes(),
        ),
        BrowserActionKind::Extract { selector } => (
            msg::BROWSER_EXTRACT,
            serde_json::json!({"selector": selector})
                .to_string()
                .into_bytes(),
        ),
        BrowserActionKind::Screenshot => (msg::BROWSER_SCREENSHOT, vec![]),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// These tests are NOT gated #[ignore] — they use only safe system commands
// (echo, osascript return value) and temp file operations.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::ACTION_DEFAULT_TIMEOUT_SECS;
    use tempfile::tempdir;

    #[tokio::test]
    async fn execute_shell_echo_succeeds() {
        let args: Vec<String> = vec!["echo".to_string(), "hello-dexter".to_string()];
        let result = execute_shell(&args, None, ACTION_DEFAULT_TIMEOUT_SECS).await;
        assert!(result.success, "echo should succeed: {:?}", result.error);
        assert!(
            result.output.contains("hello-dexter"),
            "stdout must contain the echoed string"
        );
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
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("test.txt");
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
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("out.txt");
        let data = "written by dexter phase 8";

        let wr = execute_file_write(&path, data, false).await;
        assert!(wr.success, "write should succeed: {:?}", wr.error);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, data);
    }

    #[tokio::test]
    async fn execute_file_write_with_create_dirs() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested/dir/out.txt");
        let data = "nested write";

        let wr = execute_file_write(&path, data, true).await;
        assert!(
            wr.success,
            "write with create_dirs should succeed: {:?}",
            wr.error
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), data);
    }

    #[tokio::test]
    async fn execute_applescript_return_value() {
        // osascript -e 'return "dexter_ok"' → stdout: "dexter_ok\n"
        let result =
            execute_applescript(r#"return "dexter_ok""#, ACTION_DEFAULT_TIMEOUT_SECS).await;
        assert!(
            result.success,
            "osascript should succeed: {:?}",
            result.error
        );
        assert!(
            result.output.contains("dexter_ok"),
            "osascript stdout must contain return value, got: {:?}",
            result.output
        );
    }

    #[tokio::test]
    async fn execute_applescript_timeout_reports_failure() {
        let result = execute_applescript(r#"delay 5"#, 1).await;
        assert!(
            !result.success,
            "timed-out AppleScript must not be reported as success: {:?}",
            result
        );
        assert_eq!(result.error, "timed out after 1s");
        assert_eq!(result.exit_code, None);
    }

    #[tokio::test]
    async fn execute_shortcut_empty_name_fails_closed() {
        let result = execute_shortcut("", None, None, ACTION_DEFAULT_TIMEOUT_SECS).await;

        assert!(!result.success);
        assert_eq!(result.error, "shortcut name must not be empty");
        assert_eq!(result.exit_code, None);
    }

    #[tokio::test]
    async fn execute_window_focus_empty_app_fails_closed() {
        let result = execute_window_focus("  ", None, ACTION_DEFAULT_TIMEOUT_SECS).await;

        assert!(!result.success);
        assert_eq!(result.error, "window_focus app_name must not be empty");
        assert_eq!(result.exit_code, None);
    }

    #[test]
    fn build_window_focus_script_escapes_literals() {
        let script = build_window_focus_script("Bad \" App", "Docs\\Thing\nNew");
        assert!(
            script.contains("set targetAppName to \"Bad \\\" App\""),
            "app literal must be escaped: {script}"
        );
        assert!(
            script.contains("set wantedTitle to \"Docs\\\\Thing New\""),
            "title literal must be escaped and line-normalized: {script}"
        );
    }

    #[test]
    fn build_window_inspect_script_escapes_literals_and_stays_read_only() {
        let script = build_window_inspect_script("Bad \" App\\Name\nNew");
        assert!(
            script.contains("set requestedAppName to \"Bad \\\" App\\\\Name New\""),
            "app literal must be escaped and line-normalized: {script}"
        );
        assert!(script.contains("first application process whose frontmost is true"));
        assert!(script.contains("visible windows:"));
        assert!(!script.contains(" to activate"));
        assert!(!script.contains("AXRaise"));
    }

    #[test]
    fn build_ui_snapshot_script_escapes_literals_and_stays_read_only() {
        let script = build_ui_snapshot_script("Bad \" App\\Name\nNew", 2);
        assert!(
            script.contains("set requestedAppName to \"Bad \\\" App\\\\Name New\""),
            "app literal must be escaped and line-normalized: {script}"
        );
        assert!(script.contains("set maxDepth to 2"));
        assert!(script.contains("AXSecureTextField"));
        assert!(script.contains("controls:"));
        assert!(!script.contains(" to activate"));
        assert!(!script.contains("AXRaise"));
        assert!(!script.contains("keystroke"));
        assert!(!script.contains("click "));
    }

    #[test]
    fn build_ui_click_script_escapes_literals_and_uses_axpress_only() {
        let script = build_ui_click_script("Bad \" App\\Name\nNew", "AXButton", "OK \"Now\"", 2);
        assert!(
            script.contains("set requestedAppName to \"Bad \\\" App\\\\Name New\""),
            "app literal must be escaped and line-normalized: {script}"
        );
        assert!(
            script.contains("set requestedLabel to \"OK \\\"Now\\\"\""),
            "label literal must be escaped: {script}"
        );
        assert!(script.contains("set requestedRole to \"AXButton\""));
        assert!(script.contains("set maxDepth to 2"));
        assert!(script.contains("perform action \"AXPress\" of targetElement"));
        assert!(!script.contains(" to activate"));
        assert!(!script.contains("AXRaise"));
        assert!(!script.contains("keystroke"));
    }

    #[tokio::test]
    async fn execute_ui_click_blank_label_fails_closed() {
        let result = execute_ui_click(None, Some("AXButton"), "   ", Some(2), 1).await;

        assert!(!result.success);
        assert_eq!(result.error, "ui_click label must not be empty");
        assert_eq!(result.exit_code, None);
    }

    #[test]
    fn build_ui_type_script_escapes_text_and_sets_ax_value() {
        let script = build_ui_type_script(
            "Bad \" App\\Name\nNew",
            "AXTextField",
            "Search \"Field\"",
            "hello \"Dexter\"\\line\nnext",
            2,
        );
        assert!(
            script.contains("set requestedAppName to \"Bad \\\" App\\\\Name New\""),
            "app literal must be escaped and line-normalized: {script}"
        );
        assert!(
            script.contains("set requestedLabel to \"Search \\\"Field\\\"\""),
            "label literal must be escaped: {script}"
        );
        assert!(
            script.contains("set requestedText to \"hello \\\"Dexter\\\"\\\\line next\""),
            "typed text literal must be escaped and line-normalized: {script}"
        );
        assert!(script.contains("set value of targetElement to requestedText"));
        assert!(script.contains("AXTextField"));
        assert!(script.contains("AXTextArea"));
        assert!(!script.contains("keystroke"));
        assert!(!script.contains("perform action \"AXPress\""));
        assert!(!script.contains("click "));
    }

    #[tokio::test]
    async fn execute_ui_type_without_role_or_label_fails_closed() {
        let result = execute_ui_type(None, None, None, "hello", Some(2), 1).await;

        assert!(!result.success);
        assert_eq!(result.error, "ui_type requires a role or label");
        assert_eq!(result.exit_code, None);
    }

    // ── Phase 38 / Codex finding [5]: working_dir failure-fast ────────────────

    #[tokio::test]
    async fn execute_shell_missing_working_dir_returns_error() {
        // Pre-Phase-38 behavior: silently fall back to daemon cwd with a warn!,
        // potentially mutating an unrelated tree. Now we fail explicitly so the
        // model gets the bad-path back and can correct on the continuation turn.
        let bad_dir = PathBuf::from("/tmp/dexter_phase38_no_such_dir_xyz");
        let args = vec!["echo".to_string(), "hi".to_string()];
        let result = execute_shell(&args, Some(&bad_dir), ACTION_DEFAULT_TIMEOUT_SECS).await;

        assert!(!result.success, "missing working_dir must fail the action");
        assert!(
            result.error.contains("working_dir does not exist"),
            "error must name the failure mode, got: {:?}",
            result.error
        );
        assert!(
            result.error.contains("dexter_phase38_no_such_dir_xyz"),
            "error must include the bad path so the model can correct it, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn execute_shell_existing_working_dir_succeeds() {
        // Regression guard: a valid working_dir must still work normally.
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let args = vec!["echo".to_string(), "hi".to_string()];
        let result = execute_shell(&args, Some(&dir), ACTION_DEFAULT_TIMEOUT_SECS).await;

        assert!(
            result.success,
            "valid working_dir should succeed: {:?}",
            result.error
        );
        assert!(result.output.contains("hi"));
    }

    // ── Phase 38 / Codex finding [3]: normalize_for_policy unit coverage ─────

    fn is_system_hosts_path(path: &PathBuf) -> bool {
        let s = path.to_string_lossy();
        s == "/etc/hosts" || s == "/private/etc/hosts"
    }

    #[test]
    fn normalize_for_policy_collapses_dotdot() {
        let normalized = normalize_for_policy(std::path::Path::new("/Users/jason/../../etc/hosts"));
        assert!(
            is_system_hosts_path(&normalized),
            "dotdot path must normalize to the real system hosts path, got {}",
            normalized.display()
        );
    }

    #[test]
    fn normalize_for_policy_collapses_curdir() {
        let normalized =
            normalize_for_policy(std::path::Path::new("/Users/jason/./notes/./file.txt"));
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
        assert!(
            s.ends_with("/file.txt"),
            "filename must be preserved, got {s}"
        );
    }

    #[test]
    fn normalize_for_policy_tilde_and_dotdot_combine() {
        // The exact attack: ~/../../etc/hosts. Normalize must collapse both.
        let normalized = normalize_for_policy(std::path::Path::new("~/../../etc/hosts"));
        assert!(
            is_system_hosts_path(&normalized),
            "tilde + dotdot must normalize to a system path so policy can flag it, got {}",
            normalized.display()
        );
    }

    #[test]
    fn normalize_for_policy_resolves_symlinked_parent() {
        let tmp = tempdir().unwrap();
        let link = tmp.path().join("looks-local");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        let normalized = normalize_for_policy(&link.join("hosts"));

        assert!(
            is_system_hosts_path(&normalized),
            "symlinked parent must resolve before policy classification, got {}",
            normalized.display()
        );
    }
}
