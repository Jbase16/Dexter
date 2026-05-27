/// AuditLog — append-only JSONL file recording every action Dexter takes.
///
/// ## Format
///
/// One JSON object per line (`\n`-terminated). Never truncated or rotated.
/// Each line is parseable independently — tools like `jq` work line-by-line.
///
/// ## Why append-only
///
/// An action that was taken cannot be untaken. The audit log is a factual record
/// of what happened, not a working queue. Append-only semantics make it impossible
/// for a bug to silently erase prior entries.
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::constants::{AUDIT_LOG_FILENAME, AUDIT_OUTPUT_PREVIEW_CHARS};

// ── AuditLog ──────────────────────────────────────────────────────────────────

pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Create an AuditLog pointing at `{state_dir}/audit.jsonl`.
    ///
    /// The file is NOT created until the first `append()` call — we don't create
    /// an empty file on startup if no actions are ever taken.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(AUDIT_LOG_FILENAME),
        }
    }

    /// Create an AuditLog wrapped in `Arc<tokio::sync::Mutex<>>` for shared
    /// access from spawned action tasks (Phase 24).
    ///
    /// Uses `tokio::sync::Mutex` because the lock may be held across `.await`
    /// points. The actual IO (`append`) is synchronous and fast (~0.1ms for a
    /// single JSONL line), so blocking the Tokio runtime is negligible.
    pub fn new_shared(state_dir: &Path) -> Arc<tokio::sync::Mutex<Self>> {
        Arc::new(tokio::sync::Mutex::new(Self::new(state_dir)))
    }

    /// Serialize `entry` as a single JSON line and append to the log file.
    ///
    /// Creates the file on first call. Each call is a single `write_all` — no
    /// interleaved seeks, no partial lines on process crash.
    ///
    /// Returns `Err` on serialization or IO failure. Caller must log the error
    /// — the audit failure must not be silent.
    pub fn append(&self, entry: &AuditEntry<'_>) -> Result<(), Box<dyn std::error::Error>> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Truncate `s` to at most `AUDIT_OUTPUT_PREVIEW_CHARS` Unicode scalar values.
    ///
    /// Used for `output_preview` — we record enough to confirm what a command did,
    /// not the full output of `find /` or `cat large_file.txt`.
    pub fn preview(s: &str) -> String {
        if s.chars().count() > AUDIT_OUTPUT_PREVIEW_CHARS {
            s.chars().take(AUDIT_OUTPUT_PREVIEW_CHARS).collect()
        } else {
            s.to_string()
        }
    }

    /// Return the path this log writes to (used in tests and startup logging).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ── AuditEntry ────────────────────────────────────────────────────────────────

/// One record in the audit log. Serializes to JSON with snake_case field names.
///
/// `operator_approved`:
///   - `null`  — SAFE or CAUTIOUS (no approval gate; action executed immediately)
///   - `true`  — DESTRUCTIVE, approved by operator and executed
///   - `false` — DESTRUCTIVE, rejected by operator or abandoned on session end
///
/// `outcome` values:
///   - `"success"` — process exited 0 / IO succeeded
///   - `"failure"` — process exited non-zero / IO error
///   - `"rejected"` — DESTRUCTIVE gate: operator said no (or session ended)
///   - `"timeout"`  — process exceeded ACTION_DEFAULT_TIMEOUT_SECS
#[derive(Serialize)]
pub struct AuditEntry<'a> {
    /// RFC3339 timestamp at the moment the audit entry is written.
    pub timestamp: String,
    /// UUID v4 correlating this entry with the ActionRequest/ActionApproval gRPC messages.
    pub action_id: &'a str,
    /// Action type tag: "shell" | "file_read" | "file_write" | "applescript"
    pub r#type: &'static str,
    /// Classified category: "safe" | "cautious" | "destructive"
    pub category: &'static str,
    /// Sanitized action parameters. FileWrite.content is redacted.
    pub spec_json: serde_json::Value,
    /// Execution outcome: "success" | "failure" | "rejected" | "timeout"
    pub outcome: &'static str,
    /// Process exit code. None for IO operations, timeouts, or rejections.
    pub exit_code: Option<i32>,
    /// First AUDIT_OUTPUT_PREVIEW_CHARS chars of stdout (or file content).
    pub output_preview: Option<String>,
    /// Error description (stderr or IO error message). None on success.
    pub error: Option<String>,
    /// Wall-clock execution time. None for rejections (never executed).
    pub duration_ms: Option<u64>,
    /// Whether the operator explicitly approved this action (DESTRUCTIVE only).
    pub operator_approved: Option<bool>,
}

impl AuditEntry<'_> {
    /// Convenience constructor with the current UTC timestamp pre-filled.
    ///
    /// Phase 9+: used by retrieval pipeline when replaying audit entries.
    #[allow(dead_code)]
    pub fn now(action_id: &str) -> AuditEntryBuilder<'_> {
        AuditEntryBuilder {
            timestamp: Utc::now().to_rfc3339(),
            action_id,
        }
    }
}

// ── Action history receipts ──────────────────────────────────────────────────

/// Operator-facing receipt reconstructed from one audit-log line.
///
/// This is intentionally smaller than `AuditEntry`: the HUD needs a safe,
/// human-readable summary, not raw action parameters or full command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionAuditReceipt {
    pub action_id: String,
    pub action_type: String,
    pub category: String,
    pub description: String,
    pub outcome: String,
    pub summary: String,
}

#[derive(Debug, Deserialize)]
struct AuditEntryRecord {
    action_id: String,
    #[serde(rename = "type")]
    action_type: String,
    category: String,
    spec_json: serde_json::Value,
    outcome: String,
    output_preview: Option<String>,
    error: Option<String>,
}

/// Read the newest action receipts from `{state_dir}/audit.jsonl`.
///
/// Returns newest-first receipts. Missing audit files are treated as an empty
/// history because a fresh Dexter install may not have taken any actions yet.
pub fn recent_action_receipts(
    state_dir: &Path,
    limit: usize,
) -> Result<(PathBuf, Vec<ActionAuditReceipt>), Box<dyn std::error::Error + Send + Sync>> {
    let path = state_dir.join(AUDIT_LOG_FILENAME);
    if limit == 0 || !path.exists() {
        return Ok((path, Vec::new()));
    }

    let content = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = content.lines().collect();
    let mut receipts = Vec::with_capacity(limit.min(lines.len()));
    for (idx, line) in lines.iter().enumerate().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: AuditEntryRecord = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit log line {} is not valid JSON: {e}", idx + 1),
            )
        })?;
        receipts.push(record.to_receipt());
        if receipts.len() >= limit {
            break;
        }
    }

    Ok((path, receipts))
}

impl AuditEntryRecord {
    fn to_receipt(&self) -> ActionAuditReceipt {
        ActionAuditReceipt {
            action_id: self.action_id.clone(),
            action_type: self.action_type.clone(),
            category: self.category.clone(),
            description: describe_audit_spec(&self.action_type, &self.spec_json),
            outcome: audit_receipt_outcome(&self.outcome, self.error.as_deref()),
            summary: audit_receipt_summary(
                &self.outcome,
                self.output_preview.as_deref(),
                self.error.as_deref(),
            ),
        }
    }
}

fn audit_receipt_outcome(outcome: &str, error: Option<&str>) -> String {
    match outcome {
        "success" => "executed",
        "rejected" if error.is_some_and(is_approval_expired_error) => "expired",
        "rejected" => "denied",
        "failure" | "timeout" => "failed",
        _ => "failed",
    }
    .to_string()
}

fn audit_receipt_summary(
    outcome: &str,
    output_preview: Option<&str>,
    error: Option<&str>,
) -> String {
    match outcome {
        "success" => match clean_line(output_preview) {
            Some(output) if output != "Done." => format!("Succeeded: {output}"),
            _ => "Succeeded.".to_string(),
        },
        "rejected" if error.is_some_and(is_approval_expired_error) => {
            "Approval expired before execution.".to_string()
        }
        "rejected" => match clean_line(error) {
            Some(error) if error == "session ended before operator responded" => {
                "Session closed before execution.".to_string()
            }
            Some(error) if error != "operator rejected the action" => {
                format!("Denied before execution: {error}")
            }
            _ => "Denied before execution.".to_string(),
        },
        "timeout" => match clean_line(error) {
            Some(error) => format!("Timed out: {error}"),
            None => "Timed out.".to_string(),
        },
        "failure" => match clean_line(error).or_else(|| clean_line(output_preview)) {
            Some(detail) => format!("Failed: {detail}"),
            None => "Failed.".to_string(),
        },
        _ => match clean_line(error).or_else(|| clean_line(output_preview)) {
            Some(detail) => format!("Failed: {detail}"),
            None => "Failed.".to_string(),
        },
    }
}

fn is_approval_expired_error(error: &str) -> bool {
    error.contains("approval expired before operator response")
}

fn describe_audit_spec(action_type: &str, spec: &serde_json::Value) -> String {
    match action_type {
        "shell" => describe_shell_audit_spec(spec),
        "file_read" => json_string(spec, "path")
            .map(|path| format!("Read file: {path}"))
            .unwrap_or_else(|| "Read file".to_string()),
        "file_write" => json_string(spec, "path")
            .map(|path| format!("Write file: {path}"))
            .unwrap_or_else(|| "Write file".to_string()),
        "applescript" => json_string(spec, "rationale")
            .map(|rationale| format!("AppleScript: {rationale}"))
            .unwrap_or_else(|| "Run AppleScript".to_string()),
        "message_send" => json_string(spec, "recipient")
            .map(|recipient| format!("Send iMessage to: {recipient}"))
            .unwrap_or_else(|| "Send iMessage".to_string()),
        "browser" => describe_browser_audit_spec(spec),
        other => format!("Action: {other}"),
    }
}

fn describe_shell_audit_spec(spec: &serde_json::Value) -> String {
    let Some(args) = spec.get("args").and_then(|value| value.as_array()) else {
        return "Run shell command".to_string();
    };
    let parts: Vec<&str> = args.iter().filter_map(|value| value.as_str()).collect();
    if parts.is_empty() {
        "Run shell command".to_string()
    } else {
        format!("Run: {}", parts.join(" "))
    }
}

fn describe_browser_audit_spec(spec: &serde_json::Value) -> String {
    let action = json_string(spec, "action").unwrap_or("browser action");
    match (
        action,
        json_string(spec, "url"),
        json_string(spec, "selector"),
    ) {
        ("navigate", Some(url), _) => format!("Browser navigate: {url}"),
        (_, _, Some(selector)) => format!("Browser {action}: {selector}"),
        _ => format!("Browser: {action}"),
    }
}

fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn clean_line(value: Option<&str>) -> Option<String> {
    value
        .map(|value| {
            value
                .replace('\r', " ")
                .replace('\n', " ")
                .trim()
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

/// Builder to avoid repeating boilerplate in engine.rs when constructing entries.
///
/// Phase 9+: used by retrieval pipeline and session replay tooling.
#[allow(dead_code)]
pub struct AuditEntryBuilder<'a> {
    pub timestamp: String,
    pub action_id: &'a str,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(action_id: &str) -> AuditEntry<'_> {
        AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            action_id,
            r#type: "shell",
            category: "cautious",
            spec_json: serde_json::json!({"args": ["echo", "hi"]}),
            outcome: "success",
            exit_code: Some(0),
            output_preview: Some("hi".to_string()),
            error: None,
            duration_ms: Some(12),
            operator_approved: None,
        }
    }

    #[test]
    fn audit_log_path_is_state_dir_plus_filename() {
        let tmp = tempdir().unwrap();
        let log = AuditLog::new(tmp.path());
        assert_eq!(log.path(), tmp.path().join(AUDIT_LOG_FILENAME));
    }

    #[test]
    fn audit_log_append_creates_file_and_writes_valid_json() {
        let tmp = tempdir().unwrap();
        let log = AuditLog::new(tmp.path());

        // File does not exist before first append.
        assert!(!log.path().exists());

        log.append(&make_entry("test-001"))
            .expect("append should succeed");

        // File now exists and contains valid JSON.
        let contents = std::fs::read_to_string(log.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed["action_id"], "test-001");
        assert_eq!(parsed["type"], "shell");
        assert_eq!(parsed["outcome"], "success");
    }

    #[test]
    fn audit_log_append_twice_produces_two_lines() {
        let tmp = tempdir().unwrap();
        let log = AuditLog::new(tmp.path());

        log.append(&make_entry("id-1")).unwrap();
        log.append(&make_entry("id-2")).unwrap();

        let contents = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "Two appends must produce exactly two lines");

        // Both lines must be independently valid JSON.
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line).expect("each line must be valid JSON");
        }
    }

    #[test]
    fn audit_log_preview_truncates_at_limit() {
        // Build a string that is AUDIT_OUTPUT_PREVIEW_CHARS + 1 chars long.
        let long: String = "x".repeat(AUDIT_OUTPUT_PREVIEW_CHARS + 1);
        let preview = AuditLog::preview(&long);
        assert_eq!(preview.chars().count(), AUDIT_OUTPUT_PREVIEW_CHARS);
    }

    #[test]
    fn audit_log_preview_short_string_unchanged() {
        let short = "hello";
        assert_eq!(AuditLog::preview(short), short);
    }

    #[test]
    fn recent_action_receipts_returns_newest_first_and_normalizes_outcomes() {
        let tmp = tempdir().unwrap();
        let log = AuditLog::new(tmp.path());

        log.append(&make_entry("id-1")).unwrap();
        let expired = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            action_id: "id-2",
            r#type: "shell",
            category: "destructive",
            spec_json: serde_json::json!({"args": ["rm", "-rf", "/tmp/example"]}),
            outcome: "rejected",
            exit_code: None,
            output_preview: None,
            error: Some("approval expired before operator response".to_string()),
            duration_ms: None,
            operator_approved: Some(false),
        };
        log.append(&expired).unwrap();

        let (path, receipts) = recent_action_receipts(tmp.path(), 10).unwrap();
        assert_eq!(path, log.path());
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].action_id, "id-2");
        assert_eq!(receipts[0].description, "Run: rm -rf /tmp/example");
        assert_eq!(receipts[0].outcome, "expired");
        assert_eq!(receipts[0].summary, "Approval expired before execution.");
        assert_eq!(receipts[1].action_id, "id-1");
        assert_eq!(receipts[1].outcome, "executed");
    }

    #[test]
    fn recent_action_receipts_missing_log_is_empty() {
        let tmp = tempdir().unwrap();
        let (path, receipts) = recent_action_receipts(tmp.path(), 20).unwrap();
        assert_eq!(path, tmp.path().join(AUDIT_LOG_FILENAME));
        assert!(receipts.is_empty());
    }
}
