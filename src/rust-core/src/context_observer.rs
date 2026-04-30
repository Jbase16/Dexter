/// Context observer — passive machine awareness for the Dexter core.
///
/// `ContextObserver` maintains a point-in-time `ContextSnapshot` of what the operator
/// is doing on their machine. It is updated by `CoreOrchestrator::handle_system_event()`
/// whenever the Swift `EventBridge` sends an APP_FOCUSED, AX_ELEMENT_CHANGED,
/// SCREEN_LOCKED, or SCREEN_UNLOCKED event over the gRPC session stream.
///
/// The snapshot is read-only from outside the orchestrator. Phase 8+ will inject
/// `context_summary()` into the inference system message so the model knows what
/// the operator is looking at when they ask a question.
///
/// Design notes:
/// - No OS dependencies: all parsing is pure Rust (serde_json). Tests run on CI
///   without macOS accessibility permission.
/// - Hash-based change detection: `snapshot_hash` is recomputed over semantic fields
///   on every update. Callers compare the old hash to know whether to log/emit.
/// - Privacy-first: sensitive element values arrive from EventBridge already redacted
///   (`value_preview = ""` when `is_sensitive = true`). This module trusts the Swift
///   side's privacy filter and stores whatever it receives.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::warn;

use crate::constants::{AX_VALUE_PREVIEW_MAX_CHARS, CLIPBOARD_MAX_CHARS, SHELL_CMD_MAX_CHARS, SHELL_CWD_MAX_CHARS};

// ── Public types ──────────────────────────────────────────────────────────────

/// Privacy-filtered snapshot of a focused AXUIElement.
///
/// Cheap to clone; all fields are Option to handle partial AX query failures.
/// `value_preview` is `None` when `is_sensitive = true` — the EventBridge never
/// sends the value of password fields or other sensitive inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct AxElementInfo {
    pub role:          String,
    pub label:         Option<String>,
    /// Truncated to AX_VALUE_PREVIEW_MAX_CHARS. None when is_sensitive=true.
    pub value_preview: Option<String>,
    pub is_sensitive:  bool,
}

/// Shell command context — the most recently completed command in the operator's shell.
///
/// Populated by `CoreOrchestrator::handle_shell_command()` on receipt of
/// `InternalEvent::ShellCommand` from the zsh hook listener. Overwritten on every
/// command completion; the history is not retained.
#[derive(Debug, Clone)]
pub struct ShellCommandContext {
    pub command:     String,
    pub cwd:         String,
    pub exit_code:   Option<i32>,
    /// When this context was received by the core. Used by `prepare_messages_for_inference`
    /// to omit injection when the command is older than `SHELL_CONTEXT_MAX_AGE_SECS`.
    pub received_at: DateTime<Utc>,
}

/// Point-in-time snapshot of machine context.
///
/// `snapshot_hash` changes when any semantic field changes — use it as a cheap
/// change-detection signal before logging or emitting downstream events.
/// `last_updated` is always `Utc::now()` at the moment the snapshot changes.
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub app_bundle_id:    Option<String>,
    pub app_name:         Option<String>,
    pub focused_element:  Option<AxElementInfo>,
    pub is_screen_locked: bool,
    /// Operator clipboard text. None until a CLIPBOARD_CHANGED event arrives.
    /// Bounded to CLIPBOARD_MAX_CHARS. ContextObserver starts fresh every session —
    /// clipboard content from a prior session is never persisted here.
    pub clipboard_text:   Option<String>,
    /// When the clipboard was last updated. Used by prepare_messages_for_inference
    /// to decide whether clipboard content is fresh enough to inject automatically
    /// (< CLIPBOARD_RECENCY_SECS) vs requiring an explicit operator reference.
    pub clipboard_changed_at: Option<DateTime<Utc>>,
    /// Most recently completed shell command. None until the first InternalEvent::ShellCommand
    /// arrives from the zsh hook listener. Overwritten on each command completion.
    /// Injected as [Shell: ...] in prepare_messages_for_inference when fresh.
    pub last_shell_command: Option<ShellCommandContext>,
    /// DefaultHasher over all semantic fields. Recomputed on every update.
    pub snapshot_hash:    u64,
    pub last_updated:     DateTime<Utc>,
}

/// Stateful aggregator for macOS context signals.
///
/// Owned by `CoreOrchestrator`. Updated via `update_from_app_focused()`,
/// `update_from_element_changed()`, and `set_screen_locked()`. The snapshot
/// is read via `snapshot()` and used for logging and (Phase 8+) inference injection.
pub struct ContextObserver {
    snapshot: ContextSnapshot,
}

// ── Private payload types (serde) ─────────────────────────────────────────────

/// Deserialized from CLIPBOARD_CHANGED event payload JSON.
#[derive(Deserialize)]
struct ClipboardPayload {
    text: String,
}

/// Deserialized from APP_FOCUSED event payload JSON.
#[derive(Deserialize)]
struct AppFocusedPayload {
    bundle_id:  String,
    name:       String,
    ax_element: Option<AxElementPayload>,
}

/// Deserialized from AX_ELEMENT_CHANGED event payload JSON, and from the
/// optional `ax_element` sub-object inside an APP_FOCUSED payload.
#[derive(Deserialize)]
struct AxElementPayload {
    role:          String,
    label:         Option<String>,
    value_preview: Option<String>,
    is_sensitive:  bool,
}

// ── ContextObserver implementation ────────────────────────────────────────────

impl ContextObserver {
    /// Create a new observer with an empty, unlocked initial snapshot.
    pub fn new() -> Self {
        let snapshot = ContextSnapshot {
            app_bundle_id:     None,
            app_name:          None,
            focused_element:   None,
            is_screen_locked:  false,
            clipboard_text:    None,
            clipboard_changed_at: None,
            last_shell_command: None,
            snapshot_hash:     0,
            last_updated:      Utc::now(),
        };
        Self { snapshot }
    }

    /// Parse an APP_FOCUSED payload JSON string and update the snapshot.
    ///
    /// Updates `app_bundle_id`, `app_name`, and optionally `focused_element`
    /// (if `ax_element` is present in the payload). Returns `true` if
    /// `snapshot_hash` changed — i.e., this event carried new information.
    /// Returns `false` if the app focus didn't actually change state (e.g.,
    /// re-focus of the same app with the same element).
    ///
    /// On JSON parse failure, logs a warning and returns `false` — a single
    /// malformed payload should never crash the orchestrator.
    pub fn update_from_app_focused(&mut self, payload_json: &str) -> bool {
        let payload: AppFocusedPayload = match serde_json::from_str(payload_json) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, payload = payload_json, "Failed to parse APP_FOCUSED payload — snapshot unchanged");
                return false;
            }
        };

        let old_hash = self.snapshot.snapshot_hash;

        let bundle_id = payload.bundle_id;
        let is_terminal = is_terminal_bundle(&bundle_id);

        self.snapshot.app_bundle_id   = Some(bundle_id);
        self.snapshot.app_name        = Some(payload.name);
        // Phase 36: terminal-emulator scrollback leaks our own log output back into
        // the model's context ("the screen shows a terminal window with a SQLite
        // query being executed..."). Strip value_preview for any terminal bundle;
        // keep role + label so context_summary() still identifies the app.
        self.snapshot.focused_element = payload.ax_element
            .map(ax_payload_to_info)
            .map(|mut el| { if is_terminal { el.value_preview = None; } el });
        self.snapshot.last_updated    = Utc::now();
        self.snapshot.snapshot_hash   = compute_hash(&self.snapshot);

        self.snapshot.snapshot_hash != old_hash
    }

    /// Parse an AX_ELEMENT_CHANGED payload JSON string and update `focused_element`.
    ///
    /// Preserves `app_bundle_id` and `app_name` — element changes happen within
    /// the same app, so app identity is not updated. Returns `true` if
    /// `snapshot_hash` changed (element info meaningfully changed).
    ///
    /// On JSON parse failure, logs a warning and returns `false`.
    pub fn update_from_element_changed(&mut self, payload_json: &str) -> bool {
        let payload: AxElementPayload = match serde_json::from_str(payload_json) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, payload = payload_json, "Failed to parse AX_ELEMENT_CHANGED payload — snapshot unchanged");
                return false;
            }
        };

        let old_hash = self.snapshot.snapshot_hash;

        // Phase 36: If the currently-focused app is a terminal emulator, the element's
        // value_preview is almost always scrollback (including our own log output) —
        // scrub it. The element update doesn't carry app identity, so we consult the
        // stored app_bundle_id for the decision.
        let is_terminal = self
            .snapshot
            .app_bundle_id
            .as_deref()
            .map(is_terminal_bundle)
            .unwrap_or(false);

        let mut info = ax_payload_to_info(payload);
        if is_terminal {
            info.value_preview = None;
        }
        self.snapshot.focused_element = Some(info);
        self.snapshot.last_updated    = Utc::now();
        self.snapshot.snapshot_hash   = compute_hash(&self.snapshot);

        self.snapshot.snapshot_hash != old_hash
    }

    /// Update `is_screen_locked`.
    ///
    /// Returns `true` if the value actually changed (false→true or true→false).
    /// Returns `false` on redundant calls (e.g., two consecutive lock events).
    pub fn set_screen_locked(&mut self, locked: bool) -> bool {
        if self.snapshot.is_screen_locked == locked {
            return false;
        }
        self.snapshot.is_screen_locked = locked;
        self.snapshot.last_updated     = Utc::now();
        self.snapshot.snapshot_hash    = compute_hash(&self.snapshot);
        true
    }

    /// Parse a CLIPBOARD_CHANGED payload JSON string and update `clipboard_text`.
    ///
    /// Content is truncated to CLIPBOARD_MAX_CHARS as a secondary guard (EventBridge
    /// performs the primary truncation before sending). Returns `true` if the stored
    /// clipboard content actually changed (new text differs from previously stored text).
    ///
    /// On JSON parse failure, logs a warning and returns `false` — a single malformed
    /// payload should never crash the orchestrator.
    pub fn update_from_clipboard_changed(&mut self, payload_json: &str) -> bool {
        let payload: ClipboardPayload = match serde_json::from_str(payload_json) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    error   = %e,
                    payload = payload_json,
                    "Failed to parse CLIPBOARD_CHANGED payload — clipboard unchanged"
                );
                return false;
            }
        };

        // Secondary truncation guard (EventBridge truncates first; this guards against
        // future callers that bypass EventBridge's truncation).
        let text = if payload.text.chars().count() > CLIPBOARD_MAX_CHARS {
            payload.text.chars().take(CLIPBOARD_MAX_CHARS).collect()
        } else {
            payload.text
        };

        // Skip no-op updates (same content arrived twice — guard defensively).
        if self.snapshot.clipboard_text.as_deref() == Some(text.as_str()) {
            return false;
        }

        let old_hash = self.snapshot.snapshot_hash;

        self.snapshot.clipboard_text = Some(text);
        self.snapshot.clipboard_changed_at = Some(Utc::now());
        self.snapshot.last_updated   = Utc::now();
        self.snapshot.snapshot_hash  = compute_hash(&self.snapshot);

        self.snapshot.snapshot_hash != old_hash
    }

    /// Returns the current clipboard text for injection into the inference message stack.
    ///
    /// Returns `None` when no clipboard content has arrived this session.
    /// The full stored text (up to CLIPBOARD_MAX_CHARS) is returned; the caller
    /// (`prepare_messages_for_inference`) injects it as a `[Clipboard: ...]` system message.
    pub fn clipboard_summary(&self) -> Option<&str> {
        self.snapshot.clipboard_text.as_deref()
    }

    /// Record the most recently completed shell command.
    ///
    /// Applies secondary truncation (SHELL_CMD_MAX_CHARS / SHELL_CWD_MAX_CHARS) as a
    /// defence-in-depth guard — the `parse_shell_payload` function in `ipc/server.rs`
    /// performs primary truncation, but this guards against future callers that bypass it.
    /// Overwrites any previous shell context. Hash is recomputed.
    pub fn update_shell_command(
        &mut self,
        command: String,
        cwd: String,
        exit_code: Option<i32>,
    ) {
        let command = if command.chars().count() > SHELL_CMD_MAX_CHARS {
            command.chars().take(SHELL_CMD_MAX_CHARS).collect()
        } else {
            command
        };
        let cwd = if cwd.chars().count() > SHELL_CWD_MAX_CHARS {
            cwd.chars().take(SHELL_CWD_MAX_CHARS).collect()
        } else {
            cwd
        };
        self.snapshot.last_shell_command = Some(ShellCommandContext {
            command,
            cwd,
            exit_code,
            received_at: Utc::now(),
        });
        self.snapshot.last_updated  = Utc::now();
        self.snapshot.snapshot_hash = compute_hash(&self.snapshot);
    }

    /// Borrow the current snapshot.
    pub fn snapshot(&self) -> &ContextSnapshot {
        &self.snapshot
    }

    /// Human-readable one-liner describing the current context.
    ///
    /// Format: `"Xcode — Source Editor: let x = 5"` (app — label: value_preview).
    /// When no element is focused: `"Xcode"`.
    /// Returns `None` when no app is focused.
    ///
    /// Phase 8+: injected into the inference system message so the model knows
    /// what the operator is looking at when they ask a question.
    pub fn context_summary(&self) -> Option<String> {
        let app_name = self.snapshot.app_name.as_deref()?;

        let element_part = self.snapshot.focused_element.as_ref().and_then(|el| {
            // Skip the element suffix when there's nothing useful to show.
            let label = el.label.as_deref().unwrap_or("");
            let value = el.value_preview.as_deref().unwrap_or("");

            match (label.is_empty(), value.is_empty()) {
                (true,  true)  => None,
                (false, true)  => Some(label.to_string()),
                (true,  false) => Some(value.to_string()),
                (false, false) => Some(format!("{label}: {value}")),
            }
        });

        Some(match element_part {
            Some(part) => format!("{app_name} \u{2014} {part}"),
            None       => app_name.to_string(),
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Phase 36: bundle identifiers for known terminal emulators whose scrollback
/// should NEVER be injected into the inference context.
///
/// Terminals routinely display the operator's own command output — including
/// Dexter's log lines when `RUST_LOG=debug`. A generic AXTextArea value_preview
/// capture would feed that back into the model, which faithfully describes its
/// own logs as "what's on screen". This list is consulted by
/// `update_from_app_focused` and `update_from_element_changed` to strip
/// `value_preview` while preserving app identity.
///
/// Add new emulators here; the check is a straight equality match on bundle ID.
const TERMINAL_BUNDLE_IDS: &[&str] = &[
    "com.apple.Terminal",            // Terminal.app
    "com.googlecode.iterm2",         // iTerm2 (classic)
    "com.googlecode.iterm2.nightly", // iTerm2 (nightly builds)
    "io.alacritty",                  // Alacritty
    "net.kovidgoyal.kitty",          // kitty
    "dev.warp.Warp-Stable",          // Warp
    "com.mitchellh.ghostty",         // Ghostty
    "co.zeit.hyper",                 // Hyper
    "org.wezfurlong.wezterm",        // wezterm
];

/// Phase 36: returns true if `bundle_id` identifies a terminal emulator whose
/// AX value_preview content should be scrubbed before injection into the
/// inference context.
pub(crate) fn is_terminal_bundle(bundle_id: &str) -> bool {
    TERMINAL_BUNDLE_IDS.iter().any(|t| *t == bundle_id)
}

/// Convert a deserialized `AxElementPayload` into the public `AxElementInfo`.
///
/// When `is_sensitive` is true, `value_preview` is explicitly set to `None`
/// regardless of what the payload says — defence-in-depth in case EventBridge
/// incorrectly sends a non-empty value for a sensitive field.
fn ax_payload_to_info(p: AxElementPayload) -> AxElementInfo {
    let value_preview = if p.is_sensitive {
        None
    } else {
        // EventBridge already truncates to AX_VALUE_PREVIEW_MAX_CHARS before sending.
        // Guard here too in case a future caller path skips EventBridge's truncation.
        p.value_preview.filter(|v| !v.is_empty()).map(|v| {
            if v.chars().count() > AX_VALUE_PREVIEW_MAX_CHARS {
                v.chars().take(AX_VALUE_PREVIEW_MAX_CHARS).collect()
            } else {
                v
            }
        })
    };

    AxElementInfo {
        role:  p.role,
        label: p.label.filter(|l| !l.is_empty()),
        value_preview,
        is_sensitive: p.is_sensitive,
    }
}

/// Compute a `u64` hash over the semantic fields of a `ContextSnapshot`.
///
/// `DefaultHasher` is sufficient — this hash is only used as a cheap local
/// change-detection signal, never stored or transmitted. Stability across
/// process restarts is not required.
fn compute_hash(s: &ContextSnapshot) -> u64 {
    let mut h = DefaultHasher::new();
    s.app_bundle_id.as_deref().unwrap_or("").hash(&mut h);
    s.app_name.as_deref().unwrap_or("").hash(&mut h);
    if let Some(el) = &s.focused_element {
        el.role.hash(&mut h);
        el.label.as_deref().unwrap_or("").hash(&mut h);
        el.value_preview.as_deref().unwrap_or("").hash(&mut h);
        el.is_sensitive.hash(&mut h);
    }
    s.is_screen_locked.hash(&mut h);
    s.clipboard_text.as_deref().unwrap_or("").hash(&mut h);
    // Hash shell command content (not received_at — timestamp changes don't represent
    // a semantic state change; only a different command/cwd/exit_code does).
    if let Some(shell) = &s.last_shell_command {
        shell.command.hash(&mut h);
        shell.cwd.hash(&mut h);
        if let Some(code) = shell.exit_code {
            code.hash(&mut h);
        }
    }
    h.finish()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn app_focused_payload(bundle_id: &str, name: &str) -> String {
        format!(r#"{{"bundle_id":"{bundle_id}","name":"{name}"}}"#)
    }

    fn app_focused_payload_with_element(bundle_id: &str, name: &str, role: &str, label: &str, value: &str, sensitive: bool) -> String {
        format!(
            r#"{{"bundle_id":"{bundle_id}","name":"{name}","ax_element":{{"role":"{role}","label":"{label}","value_preview":"{value}","is_sensitive":{sensitive}}}}}"#
        )
    }

    fn element_payload(role: &str, label: &str, value: &str, sensitive: bool) -> String {
        format!(r#"{{"role":"{role}","label":"{label}","value_preview":"{value}","is_sensitive":{sensitive}}}"#)
    }

    // ── Initial state ────────────────────────────────────────────────────────

    #[test]
    fn snapshot_starts_unlocked_and_empty() {
        let obs = ContextObserver::new();
        let snap = obs.snapshot();
        assert!(snap.app_bundle_id.is_none());
        assert!(snap.app_name.is_none());
        assert!(snap.focused_element.is_none());
        assert!(!snap.is_screen_locked);
        // Hash is 0 for an empty snapshot (no update has run compute_hash yet).
        assert_eq!(snap.snapshot_hash, 0);
    }

    // ── APP_FOCUSED updates ───────────────────────────────────────────────────

    #[test]
    fn update_app_focused_sets_app_info() {
        let mut obs = ContextObserver::new();
        let changed = obs.update_from_app_focused(&app_focused_payload("com.apple.Xcode", "Xcode"));
        assert!(changed, "First update must report a change");
        let snap = obs.snapshot();
        assert_eq!(snap.app_bundle_id.as_deref(), Some("com.apple.Xcode"));
        assert_eq!(snap.app_name.as_deref(), Some("Xcode"));
        assert!(snap.focused_element.is_none());
    }

    #[test]
    fn update_same_app_payload_twice_returns_false() {
        let mut obs = ContextObserver::new();
        let payload = app_focused_payload("com.apple.Terminal", "Terminal");
        obs.update_from_app_focused(&payload);
        let changed = obs.update_from_app_focused(&payload);
        assert!(!changed, "Identical payload must report no change (hash unchanged)");
    }

    #[test]
    fn update_different_app_returns_true() {
        let mut obs = ContextObserver::new();
        obs.update_from_app_focused(&app_focused_payload("com.apple.Finder", "Finder"));
        let changed = obs.update_from_app_focused(&app_focused_payload("com.apple.Terminal", "Terminal"));
        assert!(changed, "Switching apps must report a change");
        assert_eq!(obs.snapshot().app_name.as_deref(), Some("Terminal"));
    }

    // ── AX_ELEMENT_CHANGED updates ────────────────────────────────────────────

    #[test]
    fn ax_element_changed_preserves_app_identity() {
        let mut obs = ContextObserver::new();
        obs.update_from_app_focused(&app_focused_payload("com.apple.Xcode", "Xcode"));
        obs.update_from_element_changed(&element_payload("AXTextField", "Source Editor", "let x = 5", false));

        let snap = obs.snapshot();
        // App identity must survive the element update.
        assert_eq!(snap.app_bundle_id.as_deref(), Some("com.apple.Xcode"));
        assert_eq!(snap.app_name.as_deref(), Some("Xcode"));
        // Element should now be populated.
        let el = snap.focused_element.as_ref().expect("element must be present");
        assert_eq!(el.role, "AXTextField");
        assert_eq!(el.label.as_deref(), Some("Source Editor"));
        assert_eq!(el.value_preview.as_deref(), Some("let x = 5"));
    }

    #[test]
    fn sensitive_element_clears_value_preview() {
        let mut obs = ContextObserver::new();
        // EventBridge sends is_sensitive=true for password fields.
        // value_preview should be None regardless of what the payload says.
        obs.update_from_element_changed(&element_payload("AXSecureTextField", "Password", "", true));

        let el = obs.snapshot().focused_element.as_ref().expect("element must be present");
        assert!(el.is_sensitive);
        assert!(el.value_preview.is_none(), "value_preview must be None for sensitive elements");
    }

    #[test]
    fn element_value_preview_truncated_at_200_chars() {
        let mut obs = ContextObserver::new();
        // Send a 201-char value — Rust must truncate it to 200 chars as a secondary
        // guard even if EventBridge somehow sends an overlong value.
        let value_201: String = "a".repeat(201);
        obs.update_from_element_changed(&element_payload("AXTextField", "Editor", &value_201, false));

        let el = obs.snapshot().focused_element.as_ref().expect("element must be present");
        assert_eq!(
            el.value_preview.as_deref().map(|v| v.chars().count()),
            Some(200),
            "value_preview must be truncated to AX_VALUE_PREVIEW_MAX_CHARS (200)"
        );
    }

    // ── Screen lock ──────────────────────────────────────────────────────────

    #[test]
    fn screen_locked_true_returns_true_first_time() {
        let mut obs = ContextObserver::new();
        let changed = obs.set_screen_locked(true);
        assert!(changed, "false→true must return true");
        assert!(obs.snapshot().is_screen_locked);
    }

    #[test]
    fn screen_locked_redundant_call_returns_false() {
        let mut obs = ContextObserver::new();
        obs.set_screen_locked(true);
        let changed = obs.set_screen_locked(true); // second lock event
        assert!(!changed, "Redundant lock must return false");
    }

    // ── context_summary ───────────────────────────────────────────────────────

    #[test]
    fn context_summary_none_when_no_app() {
        let obs = ContextObserver::new();
        assert!(obs.context_summary().is_none());
    }

    #[test]
    fn context_summary_formats_app_and_element() {
        let mut obs = ContextObserver::new();
        obs.update_from_app_focused(&app_focused_payload_with_element(
            "com.apple.Xcode", "Xcode",
            "AXTextField", "Source Editor", "let x = 5", false,
        ));

        let summary = obs.context_summary().expect("summary must be present");
        assert_eq!(summary, "Xcode \u{2014} Source Editor: let x = 5");
    }

    // ── Clipboard updates ─────────────────────────────────────────────────────

    fn clipboard_payload(text: &str) -> String {
        serde_json::json!({"text": text}).to_string()
    }

    #[test]
    fn update_from_clipboard_changed_sets_clipboard_text() {
        let mut obs = ContextObserver::new();
        let changed = obs.update_from_clipboard_changed(&clipboard_payload("fn main() {}"));
        assert!(changed, "First clipboard update must report a change");
        assert_eq!(
            obs.snapshot().clipboard_text.as_deref(),
            Some("fn main() {}")
        );
    }

    #[test]
    fn update_from_clipboard_changed_updates_hash() {
        let mut obs = ContextObserver::new();
        obs.update_from_clipboard_changed(&clipboard_payload("first content"));
        let hash_after_first = obs.snapshot().snapshot_hash;
        obs.update_from_clipboard_changed(&clipboard_payload("second content"));
        assert_ne!(
            obs.snapshot().snapshot_hash, hash_after_first,
            "Hash must change when clipboard content changes"
        );
    }

    #[test]
    fn update_from_clipboard_same_content_returns_false() {
        let mut obs = ContextObserver::new();
        obs.update_from_clipboard_changed(&clipboard_payload("same text"));
        let changed = obs.update_from_clipboard_changed(&clipboard_payload("same text"));
        assert!(!changed, "Identical clipboard content must return false");
    }

    #[test]
    fn update_from_clipboard_changed_truncates_at_max() {
        let mut obs = ContextObserver::new();
        let long_text: String = "x".repeat(CLIPBOARD_MAX_CHARS + 100);
        obs.update_from_clipboard_changed(&clipboard_payload(&long_text));

        let stored_len = obs.snapshot().clipboard_text.as_deref()
            .map(|t| t.chars().count())
            .unwrap_or(0);
        assert_eq!(
            stored_len, CLIPBOARD_MAX_CHARS,
            "Clipboard text must be truncated to CLIPBOARD_MAX_CHARS"
        );
    }

    #[test]
    fn clipboard_summary_none_when_empty() {
        let obs = ContextObserver::new();
        assert!(
            obs.clipboard_summary().is_none(),
            "clipboard_summary must be None until a CLIPBOARD_CHANGED event arrives"
        );
    }

    #[test]
    fn clipboard_summary_returns_stored_text() {
        let mut obs = ContextObserver::new();
        obs.update_from_clipboard_changed(&clipboard_payload("let answer = 42;"));
        assert_eq!(
            obs.clipboard_summary(),
            Some("let answer = 42;"),
            "clipboard_summary must return the stored clipboard text"
        );
    }

    #[test]
    fn clipboard_parse_failure_returns_false_and_leaves_state_unchanged() {
        let mut obs = ContextObserver::new();
        obs.update_from_clipboard_changed(&clipboard_payload("original"));
        let changed = obs.update_from_clipboard_changed("not valid json {{{");
        assert!(!changed, "Parse failure must return false");
        assert_eq!(
            obs.clipboard_summary(),
            Some("original"),
            "State must be unchanged after parse failure"
        );
    }

    // ── Phase 36: terminal-bundle AX sanitization ────────────────────────────

    #[test]
    fn is_terminal_bundle_identifies_known_emulators() {
        assert!(is_terminal_bundle("com.apple.Terminal"));
        assert!(is_terminal_bundle("com.googlecode.iterm2"));
        assert!(is_terminal_bundle("com.googlecode.iterm2.nightly"));
        assert!(is_terminal_bundle("io.alacritty"));
        assert!(is_terminal_bundle("net.kovidgoyal.kitty"));
        assert!(is_terminal_bundle("dev.warp.Warp-Stable"));
        assert!(is_terminal_bundle("com.mitchellh.ghostty"));
        assert!(is_terminal_bundle("co.zeit.hyper"));
        assert!(is_terminal_bundle("org.wezfurlong.wezterm"));
    }

    #[test]
    fn is_terminal_bundle_rejects_non_terminals() {
        assert!(!is_terminal_bundle("com.apple.Safari"));
        assert!(!is_terminal_bundle("com.apple.Xcode"));
        assert!(!is_terminal_bundle("com.tinyspeck.slackmacgap"));
        assert!(!is_terminal_bundle(""));
    }

    #[test]
    fn app_focused_scrubs_value_preview_for_terminal_bundles() {
        // Fix X1: iTerm2 scrollback can contain our own log output fed back as
        // "what's on screen." When the focused app is a terminal, value_preview
        // must be stripped before the snapshot is queried for context injection.
        let mut obs = ContextObserver::new();
        let payload = app_focused_payload_with_element(
            "com.googlecode.iterm2",
            "iTerm2",
            "AXTextArea",
            "Terminal",
            "SQLite query returning results for contact handles",
            false,
        );
        obs.update_from_app_focused(&payload);

        let snap = obs.snapshot();
        let el = snap.focused_element.as_ref().expect("element present");
        assert_eq!(el.role, "AXTextArea", "role is preserved (non-leaky)");
        assert_eq!(
            el.label.as_deref(),
            Some("Terminal"),
            "label is preserved (non-leaky)"
        );
        assert!(
            el.value_preview.is_none(),
            "value_preview MUST be stripped for terminal bundles — got: {:?}",
            el.value_preview
        );
    }

    #[test]
    fn app_focused_preserves_value_preview_for_non_terminal_bundles() {
        // Regression guard for the terminal scrub: ordinary apps must still
        // surface value_preview so the model has meaningful context.
        let mut obs = ContextObserver::new();
        let payload = app_focused_payload_with_element(
            "com.apple.Xcode",
            "Xcode",
            "AXTextField",
            "Source Editor",
            "let x = 5;",
            false,
        );
        obs.update_from_app_focused(&payload);

        let el = obs
            .snapshot()
            .focused_element
            .as_ref()
            .expect("element present");
        assert_eq!(
            el.value_preview.as_deref(),
            Some("let x = 5;"),
            "value_preview must pass through for non-terminal apps"
        );
    }

    #[test]
    fn ax_element_changed_scrubs_value_preview_when_app_is_terminal() {
        // The AX change arrives separately from the app-focus event; the scrub
        // must consult the stored app_bundle_id (not the payload) to decide.
        let mut obs = ContextObserver::new();
        obs.update_from_app_focused(&app_focused_payload("com.apple.Terminal", "Terminal"));
        obs.update_from_element_changed(&element_payload(
            "AXTextArea",
            "Terminal",
            "2026-04-03T12:34:56 INFO orchestrator: handle_text_input …",
            false,
        ));

        let el = obs.snapshot().focused_element.as_ref().expect("element present");
        assert_eq!(el.role, "AXTextArea");
        assert!(
            el.value_preview.is_none(),
            "ax_element_changed must honour stored terminal bundle — got: {:?}",
            el.value_preview
        );
    }

    #[test]
    fn ax_element_changed_keeps_value_preview_when_app_is_not_terminal() {
        let mut obs = ContextObserver::new();
        obs.update_from_app_focused(&app_focused_payload("com.apple.Finder", "Finder"));
        obs.update_from_element_changed(&element_payload(
            "AXList",
            "Column View",
            "Documents",
            false,
        ));

        let el = obs.snapshot().focused_element.as_ref().expect("element present");
        assert_eq!(el.value_preview.as_deref(), Some("Documents"));
    }
}
