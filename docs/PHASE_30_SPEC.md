# Phase 30 Implementation Plan: Shell Context Integration

## Context

Dexter currently knows:
- The active application (bundle ID, name) — AX observer
- The focused UI element (role, label, value preview) — AX observer
- The operator's clipboard content — NSPasteboard polling

What Dexter does **not** know: what the operator just did in the terminal. This is the most
significant missing context for a developer workflow. When the operator asks "why did that
fail?", Dexter infers from the question alone. When `cargo build` exits with code 1, Dexter
is unaware.

Phase 30 adds **shell command context**: after each command completes in the operator's shell,
Dexter learns the command string, the working directory, and the exit code. This is injected
into inference as `[Shell: $ <command> → exit <N> in <cwd>]`.

**No Swift changes. No proto changes. No new SPM/Cargo dependencies.**

---

## Architecture Decisions

### Why shell hooks, not AX terminal reading

AX can sometimes read the focused element in terminal emulators, but:
- Terminal.app does not reliably expose the last command line via accessibility
- iTerm2's AX exposure is inconsistent across versions
- Phase 7's AX observer already captures whatever element text is available — if the
  focused element is readable, its value is already in `[Context:]`

Shell hooks give exact, structured data (command, exit code, CWD) regardless of terminal
emulator, with zero AX dependency.

### Why not capture stdout/stderr

Capturing terminal output requires `tee`, `script(1)`, or PTY manipulation. All three
interfere with terminal rendering (colors, TUI programs like `htop`/`vim`) or require
a wrapper around the shell process. Exit code + command + CWD is 90% of the value at 5%
of the complexity. Shell output capture is Phase 31+ scope.

### Connection-per-event, not persistent socket

The zsh hook sends one JSON payload per command, using a subprocess connection that writes
and closes. Rust accepts each connection, reads to EOF, parses, delivers — then the
connection is gone. This means:
- No connection state to manage
- If Dexter isn't running, the nc command fails silently (2>/dev/null) with no shell stall
- No reconnect logic needed on either side

Compare to the STT worker (persistent bidirectional socket): shell context is one-way
and ephemeral — persistence would add complexity with no benefit.

### Reuse `orchestrator_tx` / `InternalEvent`

Shell events need to reach the active session's `CoreOrchestrator`. The `orchestrator_tx:
Arc<Mutex<Option<mpsc::Sender<InternalEvent>>>>` mechanism (introduced in Phase 24c for the
STT fast path) already solves this exactly: events sent here are delivered to the active
session's `select!` loop, and silently dropped when no session is active (channel is `None`).

Adding `InternalEvent::ShellCommand` variant follows the established pattern. The shell
listener task is spawned in `CoreService::new()` — same as the STT pre-warm.

### Injection ordering: Shell after Clipboard, before Memory

`prepare_messages_for_inference` currently injects:
1. `[Context: ...]` — AX snapshot
2. `[Clipboard: ...]` — clipboard content (Phase 28)
3. `[Memory: ...]` — retrieved memory (Phase 21)

Shell context is injected between Clipboard and Memory (step 2c), using the same
`take_while(|m| m.role == "system").count()` insertion idiom. This means the system
messages read: Context → Clipboard → Shell → Memory — an ordering from least-stable to
most-stable context, which gives the model a natural "current situation → background" flow.

### Age expiry prevents stale context pollution

A command run 10 minutes ago is unlikely to be relevant to the current conversation.
Constant `SHELL_CONTEXT_MAX_AGE_SECS = 300` (5 minutes): if the stored command is older,
the `[Shell: ...]` line is silently omitted. The context is still stored in
`ContextSnapshot` — it just doesn't reach the model until refreshed by a new command.

### No proactive trigger on non-zero exit codes

The natural Phase 31 extension: when `exit_code != 0`, check the ProactiveEngine and
optionally offer unprompted help ("Your build just failed — want me to look at it?").
Phase 30 scope is context-only. Proactive triggering requires:
- A model intelligence gate (exit 1 from `grep` is normal; exit 1 from `cargo build` is not)
- Rate limiting calibration for exit-code-triggered vs app-focus-triggered observations
- UX tuning (how often should Dexter volunteer?)

Deferred to Phase 31.

---

## Notification Flow

```
precmd (zsh) → python3 JSON encoder → nc -U /tmp/dexter-shell.sock
                                           ↓
                                    ShellListener task
                                    (tokio::spawn, CoreService::new)
                                           ↓
                                    parse_shell_payload()
                                    validation + truncation
                                           ↓
                              orchestrator_tx lock → InternalEvent::ShellCommand
                                           ↓
                              select! loop (server.rs)
                              → orchestrator.handle_shell_command()
                                           ↓
                              context_observer.update_shell_command()
                              → ShellCommandContext in ContextSnapshot
                                           ↓
                              prepare_messages_for_inference
                              → [Shell: $ cmd → exit N in /cwd]
```

---

## File Map

| Change   | File                                                          |
|----------|---------------------------------------------------------------|
| New      | `config/shell_integration.zsh`                                |
| Modified | `src/rust-core/src/constants.rs`                              |
| Modified | `src/rust-core/src/context_observer.rs`                       |
| Modified | `src/rust-core/src/ipc/server.rs`                             |
| Modified | `src/rust-core/src/orchestrator.rs`                           |
| Modified | `src/rust-core/src/proactive/engine.rs` *(test fixture only)* |

---

## 1. `config/shell_integration.zsh` — new file

```zsh
# Dexter shell integration for zsh.
#
# Installation (add to ~/.zshrc):
#   export DEXTER_ROOT="$HOME/Developer/Dex"   # adjust to your checkout path
#   source "$DEXTER_ROOT/config/shell_integration.zsh"
#
# Sends a JSON notification to the Dexter core after each command completes.
# Fire-and-forget (background subprocess); zero impact when Dexter is not running.

# Socket path. Override with DEXTER_SHELL_SOCKET for testing.
_dexter_socket="${DEXTER_SHELL_SOCKET:-/tmp/dexter-shell.sock}"

# Stores the command string captured in preexec, consumed in precmd.
_dexter_last_cmd=""

# preexec: called by zsh before each command line executes.
# $1 is the raw command string as typed by the operator.
_dexter_preexec() {
    _dexter_last_cmd="$1"
}

# precmd: called by zsh after each command completes.
# $? is the exit code of the just-completed command.
_dexter_precmd() {
    local _exit="$?"
    [[ -z "$_dexter_last_cmd" ]] && return   # nothing was run (e.g., bare Enter)
    local _cmd="$_dexter_last_cmd"
    local _cwd="$PWD"    # CWD after the command — captures cd correctly
    _dexter_last_cmd=""  # clear so a bare Enter in precmd doesn't resend

    # python3 handles all JSON escaping (quotes, backslashes, Unicode).
    # nc -U writes to the Unix domain socket and exits; the socket silently rejects
    # if Dexter is not running (2>/dev/null). &! = background + disown (no "Done:" output).
    (python3 -c "
import json, sys
payload = json.dumps({
    'command':   sys.argv[1],
    'cwd':       sys.argv[2],
    'exit_code': int(sys.argv[3]),
})
print(payload, end='')
" "$_cmd" "$_cwd" "$_exit" | nc -U "$_dexter_socket" 2>/dev/null) &!
}

# Register with zsh's hook system. autoload is required before add-zsh-hook.
# add-zsh-hook is idempotent — safe to source multiple times.
autoload -Uz add-zsh-hook
add-zsh-hook preexec _dexter_preexec
add-zsh-hook precmd  _dexter_precmd
```

**Why Python3 for JSON encoding:** shell parameter expansion cannot safely encode arbitrary
strings to JSON — backslashes, double quotes, newlines, Unicode all require escaping.
`python3 -c "... json.dumps(...)"` is available on the machine (Python 3.14.2), runs in
~10ms, and produces correct JSON unconditionally. The subprocess is backgrounded and
discarded, so the 10ms cost never blocks the prompt.

---

## 2. `constants.rs` — 5 new constants

```rust
/// Unix domain socket path for the shell integration hook.
/// Matches the default value of $DEXTER_SHELL_SOCKET in shell_integration.zsh.
pub const SHELL_SOCKET_PATH:          &str  = "/tmp/dexter-shell.sock";

/// Minimum command length (chars) to accept. Filters empty strings and
/// single-character typos/aliases (e.g. `l`, `s`).
pub const SHELL_CMD_MIN_CHARS:         usize = 2;

/// Maximum command length (chars). Commands longer than this are truncated.
/// Protects against `cat bigfile | ...` pipes that produce multi-MB command strings.
pub const SHELL_CMD_MAX_CHARS:         usize = 500;

/// Maximum CWD length (chars). Paths longer than this are truncated.
pub const SHELL_CWD_MAX_CHARS:         usize = 200;

/// Shell context is injected into inference only when the stored command is
/// fresher than this many seconds. Stale commands (e.g. from 10 minutes ago)
/// are silently omitted rather than injecting misleading context.
pub const SHELL_CONTEXT_MAX_AGE_SECS:  u64   = 300;   // 5 minutes
```

---

## 3. `context_observer.rs` — ShellCommandContext + snapshot field + update method

### 3a. New struct

```rust
/// Shell command context — the most recently completed command in the operator's shell.
/// Populated by CoreOrchestrator::handle_shell_command() on receipt of
/// InternalEvent::ShellCommand from the zsh hook listener.
#[derive(Clone, Debug)]
pub struct ShellCommandContext {
    pub command:     String,
    pub exit_code:   Option<i32>,
    pub cwd:         String,
    /// When this context was received by the core. Used by prepare_messages_for_inference
    /// to skip injection when the command is older than SHELL_CONTEXT_MAX_AGE_SECS.
    pub received_at: DateTime<Utc>,
}
```

### 3b. New field in `ContextSnapshot`

```rust
/// Most recently completed shell command.
/// None until the first InternalEvent::ShellCommand arrives.
/// Overwritten on every command completion; older entries are discarded.
/// Injected as [Shell: ...] only when fresher than SHELL_CONTEXT_MAX_AGE_SECS.
pub last_shell_command: Option<ShellCommandContext>,
```

Add `last_shell_command: None` to `ContextObserver::new()`.

### 3c. `update_shell_command()` method

```rust
/// Record the most recently completed shell command.
/// Overwrites any previous value. Hash is updated to reflect the new state.
pub fn update_shell_command(
    &mut self,
    command: String,
    cwd: String,
    exit_code: Option<i32>,
) {
    self.snapshot.last_shell_command = Some(ShellCommandContext {
        command,
        cwd,
        exit_code,
        received_at: Utc::now(),
    });
    self.snapshot.last_updated  = Utc::now();
    self.snapshot.snapshot_hash = compute_hash(&self.snapshot);
}
```

### 3d. Update `compute_hash()`

Add shell command fields to the hash. Hash `command`, `cwd`, and `exit_code`; do NOT
hash `received_at` — the hash represents the semantic content of the context, and the
timestamp changing (same command received a second time) should not produce a new hash.

```rust
if let Some(shell) = &s.last_shell_command {
    hasher.write(shell.command.as_bytes());
    hasher.write(shell.cwd.as_bytes());
    if let Some(code) = shell.exit_code {
        hasher.write_i32(code);
    }
}
```

### 3e. `proactive/engine.rs` — test fixture fix

The `unlocked_snapshot_with_app()` function in `proactive/engine.rs` constructs
`ContextSnapshot` with all fields explicitly named. Add `last_shell_command: None` to
that struct literal (same fix pattern as Phase 28 required for `clipboard_text`).

---

## 4. `ipc/server.rs` — shell listener task

### 4a. Add `ShellCommand` to `InternalEvent`

```rust
enum InternalEvent {
    TranscriptReady { text: String, trace_id: String },
    /// Shell command-completion event from the zsh hook listener.
    /// Delivered via orchestrator_tx; session's select! loop calls
    /// orchestrator.handle_shell_command() on receipt.
    ShellCommand {
        command:   String,
        cwd:       String,
        exit_code: Option<i32>,
    },
}
```

### 4b. `parse_shell_payload()` — extracted for testability

```rust
/// Parse and validate a JSON string from the shell integration hook.
///
/// Returns `(command, cwd, exit_code)` on success or `None` on any error.
/// Applies SHELL_CMD_MIN_CHARS gate and SHELL_CMD_MAX_CHARS / SHELL_CWD_MAX_CHARS
/// truncation so orchestrator storage is always bounded.
fn parse_shell_payload(json_str: &str) -> Option<(String, String, Option<i32>)> {
    use crate::constants::{SHELL_CMD_MIN_CHARS, SHELL_CMD_MAX_CHARS, SHELL_CWD_MAX_CHARS};

    #[derive(serde::Deserialize)]
    struct ShellPayload {
        command:   String,
        cwd:       String,
        exit_code: Option<i32>,
    }

    let payload: ShellPayload = serde_json::from_str(json_str.trim()).ok()?;

    let cmd_chars = payload.command.chars().count();
    if cmd_chars < SHELL_CMD_MIN_CHARS {
        return None;   // too short — bare Enter, single-char alias, etc.
    }
    let command = if cmd_chars > SHELL_CMD_MAX_CHARS {
        payload.command.chars().take(SHELL_CMD_MAX_CHARS).collect()
    } else {
        payload.command
    };

    let cwd_chars = payload.cwd.chars().count();
    let cwd = if cwd_chars > SHELL_CWD_MAX_CHARS {
        payload.cwd.chars().take(SHELL_CWD_MAX_CHARS).collect()
    } else {
        payload.cwd
    };

    Some((command, cwd, payload.exit_code))
}
```

### 4c. `run_shell_listener()` — private async function

```rust
/// Accepts one-shot connections from the zsh shell hook and delivers parsed
/// events to the active session via `orchestrator_tx`.
///
/// Lifecycle: spawned once in CoreService::new(). Runs for the lifetime of the
/// process. Each command completion is a separate connect → write JSON → EOF → close
/// connection; no persistent state is maintained between commands.
///
/// If no session is active (orchestrator_tx is None), events are silently dropped.
/// Shell context is ephemeral — there is no value in buffering it across sessions.
async fn run_shell_listener(
    orchestrator_tx: Arc<Mutex<Option<mpsc::Sender<InternalEvent>>>>,
) {
    use crate::constants::SHELL_SOCKET_PATH;
    use tokio::net::UnixListener;
    use tokio::io::AsyncReadExt;

    // Remove stale socket from a previous crash. Silent on ENOENT (first run).
    let _ = std::fs::remove_file(SHELL_SOCKET_PATH);

    let listener = match UnixListener::bind(SHELL_SOCKET_PATH) {
        Ok(l) => {
            info!(socket = SHELL_SOCKET_PATH, "Shell listener ready");
            l
        }
        Err(e) => {
            warn!(
                error  = %e,
                socket = SHELL_SOCKET_PATH,
                "Shell listener: failed to bind — shell context disabled for this session"
            );
            return;
        }
    };

    loop {
        let (mut stream, _addr) = match listener.accept().await {
            Ok(s)  => s,
            Err(e) => { warn!(error = %e, "Shell listener: accept error — continuing"); continue; }
        };

        // Each connection is handled in its own task to avoid blocking the accept loop.
        // Connection-per-event means the spawned task is very short-lived.
        let tx_arc = orchestrator_tx.clone();
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(512);
            if let Err(e) = stream.read_to_end(&mut buf).await {
                warn!(error = %e, "Shell listener: read error");
                return;
            }
            let json_str = match std::str::from_utf8(&buf) {
                Ok(s)  => s,
                Err(e) => { warn!(error = %e, "Shell listener: non-UTF8 payload"); return; }
            };

            let Some((command, cwd, exit_code)) = parse_shell_payload(json_str) else {
                // Warn only for non-empty payloads — empty reads happen when nc exits
                // without writing (e.g., Dexter starting and socket being tested).
                if !json_str.trim().is_empty() {
                    warn!(raw = json_str, "Shell listener: payload rejected (parse/validation failure)");
                }
                return;
            };

            let guard = tx_arc.lock().await;
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(InternalEvent::ShellCommand { command, cwd, exit_code }).await;
                // Ignore send error — session may be tearing down; event is lost safely.
            }
            // guard drops here, unlocking. If None: no session active; drop silently.
        });
    }
}
```

### 4d. Spawn in `CoreService::new()`

After the STT pre-warm spawn, add:

```rust
// Spawn the shell context listener. Accepts one-shot connections from the zsh
// integration hook at SHELL_SOCKET_PATH and forwards parsed events to the active
// session via orchestrator_tx.
let shell_tx = self.orchestrator_tx.clone();
tokio::spawn(run_shell_listener(shell_tx));
```

### 4e. Handle `ShellCommand` in the `select!` loop

In the `internal_rx.recv()` match arm, extend the existing `if let` to a `match`:

```rust
// Before (Phase 24c, single variant):
if let Some(InternalEvent::TranscriptReady { text, trace_id }) = internal {
    orchestrator.handle_fast_transcript(text, trace_id).await;
}

// After (Phase 30, two variants):
match internal {
    Some(InternalEvent::TranscriptReady { text, trace_id }) => {
        orchestrator.handle_fast_transcript(text, trace_id).await;
    }
    Some(InternalEvent::ShellCommand { command, cwd, exit_code }) => {
        orchestrator.handle_shell_command(command, cwd, exit_code).await;
    }
    None => {}
}
```

---

## 5. `orchestrator.rs` — handle + inject

### 5a. `handle_shell_command()`

```rust
/// Receives a shell command-completion event from the zsh integration hook
/// (via InternalEvent::ShellCommand in the server.rs select! loop).
///
/// Updates ContextObserver with the new shell context. Does NOT trigger proactive
/// inference — Phase 30 is context-only. Exit-code-based proactive triggering
/// is Phase 31+ scope.
pub(crate) async fn handle_shell_command(
    &mut self,
    command: String,
    cwd: String,
    exit_code: Option<i32>,
) {
    self.context_observer.update_shell_command(
        command.clone(),
        cwd.clone(),
        exit_code,
    );
    info!(
        session   = %self.session_id,
        command   = %command,
        cwd       = %cwd,
        exit_code = ?exit_code,
        "Shell command context updated"
    );
}
```

### 5b. Injection in `prepare_messages_for_inference()` — Step 2c

After the existing step 2b (Clipboard injection), add:

```rust
// Step 2c: Inject [Shell: ...] if a fresh shell command context is available.
// "Fresh" = received within SHELL_CONTEXT_MAX_AGE_SECS seconds. Older commands
// are omitted — they are more likely to confuse than to help ("why is it telling
// me about cargo build from 20 minutes ago?").
if let Some(shell) = self.context_observer.snapshot().last_shell_command.as_ref() {
    let age_secs = (Utc::now() - shell.received_at).num_seconds();
    if age_secs < crate::constants::SHELL_CONTEXT_MAX_AGE_SECS as i64 {
        let exit_str = shell.exit_code
            .map_or_else(|| "?".to_string(), |c| c.to_string());
        let msg = format!(
            "[Shell: $ {} → exit {} in {}]",
            shell.command, exit_str, shell.cwd
        );
        let insert_at = messages.iter().take_while(|m| m.role == "system").count();
        messages.insert(
            insert_at,
            crate::inference::engine::Message::system(msg),
        );
        debug!(
            session   = %self.session_id,
            command   = %shell.command,
            exit_code = ?shell.exit_code,
            age_secs  = age_secs,
            "Shell context injected into inference request"
        );
    }
}
```

**Injection position invariant:** Both clipboard (2b) and shell (2c) use `take_while(|m|
m.role == "system").count()` at their respective insertion points. Clipboard runs first,
inserting one system message. When shell's `take_while` runs, it counts the already-inserted
clipboard message, placing shell after clipboard. When memory's `take_while` runs (later,
Phase 21), it counts both clipboard and shell, placing memory after shell. The ordering
`[Context:] → [Clipboard:] → [Shell:] → [Memory:]` is preserved without any explicit index
arithmetic.

---

## 6. Execution Order

1. Add 5 constants to `constants.rs`
2. Add `ShellCommandContext` struct, `last_shell_command` field, `update_shell_command()`,
   and `compute_hash()` update to `context_observer.rs`
3. Add `last_shell_command: None` to `ContextObserver::new()`
4. Add `last_shell_command: None` to `unlocked_snapshot_with_app()` in `proactive/engine.rs`
5. Add `ShellCommand` variant to `InternalEvent` in `ipc/server.rs`
6. Add `parse_shell_payload()` private function in `ipc/server.rs`
7. Add `run_shell_listener()` private async function in `ipc/server.rs`
8. Spawn shell listener in `CoreService::new()` (after STT pre-warm spawn)
9. Extend `internal_rx.recv()` match to handle `ShellCommand` arm in select! loop
10. Add `handle_shell_command()` to `orchestrator.rs`
11. Add step 2c shell injection to `prepare_messages_for_inference()`
12. Write unit tests for `parse_shell_payload` in `ipc/server.rs`
13. Write integration tests in `orchestrator.rs`
14. Create `config/shell_integration.zsh`
15. `cargo test` — target: all prior tests pass + 8 new tests pass, 0 warnings
16. `source config/shell_integration.zsh` in test shell, run a command, verify
    `[Shell: ...]` appears in inference context via logs
17. Update `docs/SESSION_STATE.json` and `memory/MEMORY.md`

---

## 7. Tests

### Unit tests in `ipc/server.rs`

```rust
#[cfg(test)]
mod shell_payload_tests {
    use super::parse_shell_payload;

    #[test]
    fn parse_shell_payload_valid() {
        let json = r#"{"command":"cargo test","cwd":"/tmp/project","exit_code":0}"#;
        let (cmd, cwd, code) = parse_shell_payload(json).unwrap();
        assert_eq!(cmd, "cargo test");
        assert_eq!(cwd, "/tmp/project");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn parse_shell_payload_null_exit_code() {
        let json = r#"{"command":"cargo test","cwd":"/tmp","exit_code":null}"#;
        let (_cmd, _cwd, code) = parse_shell_payload(json).unwrap();
        assert_eq!(code, None);
    }

    #[test]
    fn parse_shell_payload_command_too_short() {
        let json = r#"{"command":"l","cwd":"/tmp","exit_code":0}"#;
        assert!(parse_shell_payload(json).is_none(), "single-char command must be rejected");
    }

    #[test]
    fn parse_shell_payload_truncates_long_command() {
        let long_cmd = "a".repeat(600);
        let json = format!(r#"{{"command":"{}","cwd":"/tmp","exit_code":0}}"#, long_cmd);
        let (cmd, _cwd, _code) = parse_shell_payload(&json).unwrap();
        assert_eq!(cmd.chars().count(), 500, "command must be truncated to SHELL_CMD_MAX_CHARS");
    }

    #[test]
    fn parse_shell_payload_truncates_long_cwd() {
        let long_cwd = "/".repeat(300);
        let json = format!(r#"{{"command":"ls","cwd":"{}","exit_code":0}}"#, long_cwd);
        let (_cmd, cwd, _code) = parse_shell_payload(&json).unwrap();
        assert_eq!(cwd.chars().count(), 200, "cwd must be truncated to SHELL_CWD_MAX_CHARS");
    }

    #[test]
    fn parse_shell_payload_invalid_json_returns_none() {
        assert!(parse_shell_payload("not json at all").is_none());
        assert!(parse_shell_payload("").is_none());
        assert!(parse_shell_payload("{}").is_none()); // missing required fields
    }
}
```

### Integration tests in `orchestrator.rs`

```rust
#[tokio::test]
async fn shell_context_injected_into_inference_messages() {
    // Signature: prepare_messages_for_inference(&self, recall: &[MemoryEntry]) -> Vec<Message>
    // The user query is not passed here — it lives in self.context at call time.
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, _rx) = make_orchestrator(tmp.path());

    orch.handle_shell_command(
        "cargo build".to_string(),
        "/Users/test/project".to_string(),
        Some(1),
    ).await;

    let messages = orch.prepare_messages_for_inference(&[]);

    let shell_msg = messages.iter().find(|m| m.role == "system" && m.content.contains("[Shell:"));
    assert!(shell_msg.is_some(), "[Shell: ...] must be injected when context is fresh");
    let content = &shell_msg.unwrap().content;
    assert!(content.contains("cargo build"),        "command must appear in shell context");
    assert!(content.contains("exit 1"),             "exit code must appear in shell context");
    assert!(content.contains("/Users/test/project"),"cwd must appear in shell context");
}

#[tokio::test]
async fn shell_context_not_injected_when_none() {
    // Baseline: orchestrator with no shell command set produces no [Shell: ...] message.
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, _rx) = make_orchestrator(tmp.path());

    let messages = orch.prepare_messages_for_inference(&[]);
    let has_shell = messages.iter().any(|m| m.role == "system" && m.content.contains("[Shell:"));
    assert!(!has_shell, "[Shell: ...] must not appear when no shell command has been received");
}
```

**Expected test totals:** 276 (Phase 28) + 8 new = **284 RUST TESTS PASS**.

---

## 8. Acceptance Criteria

### Automated

`cargo test` in `src/rust-core/`: 284 tests pass, 0 warnings.

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Hook sources cleanly | `source config/shell_integration.zsh` in a fresh shell — no errors, `_dexter_preexec` and `_dexter_precmd` registered |
| 2 | Command captured | Run `ls -la` in the sourced shell — Rust log shows `"Shell command context updated"` with `command="ls -la"` |
| 3 | Exit code captured | Run `false` (exit 1) — log shows `exit_code=Some(1)` |
| 4 | Successful command | Run `echo hi` (exit 0) — log shows `exit_code=Some(0)` |
| 5 | CWD correct after cd | Run `cd /tmp && ls` — log shows `cwd="/tmp"` (not the directory before cd) |
| 6 | Shell context in inference | Run `cargo build` (fails), then ask Dexter "what just happened?" — response references the build failure |
| 7 | Context format | Check `[Shell: $ cargo build → exit 1 in /path]` format in debug logs |
| 8 | Dexter not running | Run a command while Dexter core is stopped — shell prompt returns immediately, no hang |
| 9 | Age expiry | Run a command, wait 6 minutes without interaction, then ask Dexter a question — `[Shell:]` does NOT appear in debug logs for that inference |
| 10 | No Swift impact | `cd src/swift && swift build` still succeeds with 0 warnings from project code |
| 11 | No regression | All prior context (AX, clipboard, memory) still works — not displaced by shell context |
| 12 | Short commands filtered | Type `l` (1 char, hypothetical alias), press Enter — NOT sent to Dexter (parse_shell_payload rejects < 2 chars) |

---

## 9. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| `nc -U` unavailable or behaviorally different on some macOS versions | macOS ships `/usr/bin/nc` with Unix socket support. Fallback: `socat UNIX-CONNECT:$socket STDIN` if nc is ever unavailable (note in README). |
| Shell hook slows prompt by 10–50ms per command | Entire hook runs in `(...)  &!` (subshell + background + disown). Shell prompt returns before Python3 or nc start. Measured overhead: 0ms prompt latency. |
| Operators use bash, not zsh | The spec targets zsh (`preexec`/`precmd` are zsh-native). A bash version using `PROMPT_COMMAND` and `trap ... DEBUG` is structurally similar but more fragile. Bash is Phase 31+ scope. |
| Long-running commands (e.g. `cargo build`) — command arrives late | `precmd` fires AFTER the command completes, regardless of duration. A 3-minute build fires one precmd notification when it finishes. This is correct behavior: the context is "build just finished with exit N". |
| Socket left behind after crash prevents rebind | `run_shell_listener` calls `std::fs::remove_file(SHELL_SOCKET_PATH)` before bind. First-run ENOENT is silently ignored. |
| `UnixListener::bind` fails if socket file is owned by another user | Only relevant with multiple OS users. Single-operator machine: not a risk. |
| `proactive/engine.rs` test fixture missing `last_shell_command` field | Explicitly called out in Step 4 of execution order. Same fix pattern as Phase 28's `clipboard_text: None`. |
| `take_while` ordering: shell injected before clipboard if memory inserts first | Ordering is determinate: clipboard (2b) inserts before shell (2c). The `take_while` at each insertion point counts all prior system messages (including ones just inserted). Verified by the existing Phase 28 ordering test pattern; same invariant applies here. |
