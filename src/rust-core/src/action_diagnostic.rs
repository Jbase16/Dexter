use std::path::{Path, PathBuf};

use crate::{
    action::audit::{recent_action_receipts, ActionAuditReceipt},
    action_evidence::{
        action_receipt_diagnosis, format_failed_action_evidence_block,
        format_success_action_evidence_block, ActionEvidence,
    },
    session::{state::HistoryEntry, SessionStateManager},
};

impl ActionEvidence for ActionAuditReceipt {
    fn action_outcome(&self) -> &str {
        &self.outcome
    }

    fn action_type(&self) -> &str {
        &self.action_type
    }

    fn action_target(&self) -> &str {
        &self.description
    }

    fn result_summary(&self) -> &str {
        &self.summary
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionActionClue {
    pub(crate) session_id: String,
    pub(crate) session_start: String,
    pub(crate) session_end: Option<String>,
    pub(crate) user_text: Option<String>,
    pub(crate) assistant_text: Option<String>,
    pub(crate) diagnosis: String,
    pub(crate) evidence: String,
    pub(crate) operator_next_step: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionDiagnosticReport {
    pub(crate) markdown: String,
    pub(crate) cause: String,
    pub(crate) audit_log_path: PathBuf,
    pub(crate) receipts: Vec<ActionAuditReceipt>,
    pub(crate) has_session_clue: bool,
    pub(crate) has_diagnostic: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ActionDiagnosticInput<'a> {
    pub(crate) state_dir: &'a Path,
    pub(crate) limit: usize,
    pub(crate) current_user_text: Option<String>,
    pub(crate) current_assistant_text: Option<String>,
    pub(crate) health_warnings: Vec<String>,
    pub(crate) only_if_clue: bool,
    pub(crate) ignore_action_receipts: bool,
}

pub(crate) fn build_action_diagnostic(
    input: ActionDiagnosticInput<'_>,
) -> Result<ActionDiagnosticReport, Box<dyn std::error::Error + Send + Sync>> {
    let limit = if input.limit == 0 {
        3
    } else {
        input.limit.min(100)
    };
    let (audit_log_path, receipts) = if input.ignore_action_receipts {
        (
            input.state_dir.join(crate::constants::AUDIT_LOG_FILENAME),
            Vec::new(),
        )
    } else {
        recent_action_receipts(input.state_dir, limit)?
    };

    let current_clue =
        analyze_current_turn_for_action_clue(input.current_user_text, input.current_assistant_text);
    let latest_clue = if current_clue.is_none() {
        load_latest_session_clue(input.state_dir)
    } else {
        None
    };
    let session_clue = current_clue.as_ref().or(latest_clue.as_ref());

    let mut has_diagnostic = true;
    let cause = if let Some(receipt) = receipts
        .first()
        .filter(|receipt| receipt.outcome != "executed")
    {
        action_receipt_diagnosis(receipt)
    } else if let Some(clue) = session_clue {
        clue.diagnosis.clone()
    } else if receipts.first().is_some() {
        "The most recent audited action executed successfully.".to_string()
    } else {
        has_diagnostic = false;
        "No recent action receipt or known refusal clue was found.".to_string()
    };

    if input.only_if_clue
        && receipts
            .first()
            .is_none_or(|receipt| receipt.outcome == "executed")
        && session_clue.is_none()
    {
        has_diagnostic = false;
    }

    let markdown = format_action_diagnostic_markdown(
        &audit_log_path,
        &receipts,
        session_clue,
        &input.health_warnings,
        &cause,
        has_diagnostic,
    );

    Ok(ActionDiagnosticReport {
        markdown,
        cause,
        audit_log_path,
        receipts,
        has_session_clue: session_clue.is_some(),
        has_diagnostic,
    })
}

fn load_latest_session_clue(state_dir: &Path) -> Option<SessionActionClue> {
    let state = SessionStateManager::load_latest(state_dir)?;
    analyze_history_for_action_clue(
        state.session_id,
        state.session_start,
        state.session_end,
        state.conversation_history,
    )
}

fn analyze_current_turn_for_action_clue(
    user_text: Option<String>,
    assistant_text: Option<String>,
) -> Option<SessionActionClue> {
    let assistant_text = clean_line_owned(assistant_text?)?;
    let user_text = user_text.and_then(clean_line_owned);
    let (diagnosis, operator_next_step) =
        action_refusal_diagnosis(user_text.as_deref(), &assistant_text)?;

    Some(SessionActionClue {
        session_id: "current".to_string(),
        session_start: "in-flight".to_string(),
        session_end: None,
        user_text,
        assistant_text: Some(assistant_text.clone()),
        diagnosis,
        evidence: assistant_text,
        operator_next_step,
    })
}

fn analyze_history_for_action_clue(
    session_id: String,
    session_start: String,
    session_end: Option<String>,
    history: Vec<HistoryEntry>,
) -> Option<SessionActionClue> {
    let mut last_user: Option<String> = None;
    let mut last_assistant: Option<String> = None;

    for entry in history.iter().rev() {
        match entry.role.as_str() {
            "assistant" if last_assistant.is_none() => {
                last_assistant = clean_line_owned(entry.content.clone());
            }
            "user" if last_user.is_none() => {
                last_user = clean_line_owned(entry.content.clone());
            }
            _ => {}
        }
        if last_user.is_some() && last_assistant.is_some() {
            break;
        }
    }

    let assistant = last_assistant.as_deref()?;
    let (diagnosis, operator_next_step) =
        action_refusal_diagnosis(last_user.as_deref(), assistant)?;

    Some(SessionActionClue {
        session_id,
        session_start,
        session_end,
        user_text: last_user,
        assistant_text: last_assistant.clone(),
        diagnosis,
        evidence: assistant.to_string(),
        operator_next_step,
    })
}

fn action_refusal_diagnosis(user: Option<&str>, assistant: &str) -> Option<(String, String)> {
    let lower = assistant.to_ascii_lowercase();
    let (diagnosis, operator_next_step) = if lower.contains("different machine")
        || lower.contains("only run it here")
    {
        (
            "Dexter refused to execute a shell action locally because the request looked off-host.",
            "Run the surfaced command on the target machine, or explicitly say it should run on this Mac.",
        )
    } else if user
        .map(|text| is_explicit_web_action_request(text) && is_tool_capability_denial(&lower))
        .unwrap_or(false)
    {
        (
            "Dexter answered with a tool-capability denial even though the operator asked for web/search/download work. No action was emitted, so browser, shell, or download tooling never ran.",
            "Fix the routing path so explicit web/search/download requests enter tool handling before normal chat, then retry the task.",
        )
    } else if lower.contains("couldn't determine the exact contacts recipient") {
        (
            "Dexter refused a message send because the request did not name an exact Contacts recipient.",
            "Retry with the recipient's Contacts name exactly as it appears in Contacts.",
        )
    } else if lower.contains("couldn't verify the recipient in contacts") {
        (
            "Dexter refused a message send because the generated send action did not contain a verifiable Contacts recipient.",
            "Retry with the contact name exactly as it appears in Contacts so Rust can resolve the handle.",
        )
    } else if lower.contains("contacts lookup failed") {
        (
            "Dexter refused a message send because Contacts lookup failed before the recipient could be verified.",
            "Check Contacts access, then retry with the contact name exactly as it appears in Contacts.",
        )
    } else if lower.contains("couldn't find that imessage recipient handle in contacts") {
        (
            "Dexter refused a message send because the proposed iMessage handle was not found in Contacts.",
            "Add or correct the contact in Contacts.app, then retry with the contact's exact name.",
        )
    } else if lower.contains("belongs to")
        && lower.contains("not ")
        && lower.contains("i didn't send it")
        && lower.contains("contacts")
    {
        (
            "Dexter refused a message send because the proposed iMessage handle belonged to a different Contacts entry.",
            "Retry with the exact Contacts name; Dexter will resolve the recipient handle itself.",
        )
    } else if lower.contains("i couldn't find") && lower.contains("contacts") {
        (
            "Dexter refused a message send because Contacts did not contain the requested recipient.",
            "Add or correct the Contacts entry, then retry using the contact's exact name.",
        )
    } else if lower.contains("more than one contacts match") {
        (
            "Dexter refused a message send because Contacts resolution was ambiguous.",
            "Retry with the exact Contacts name or enough detail to disambiguate.",
        )
    } else if lower.contains("isn't a phone number")
        || lower.contains("or imessage email i can use")
    {
        (
            "Dexter found the Contacts entry but refused to send because it had no reachable phone or iMessage handle.",
            "Add a phone number or iMessage email to the Contacts entry, then retry.",
        )
    } else if lower.contains("i don't have your imessage handle configured") {
        (
            "Dexter refused a self-send because operator_self_handle is not configured.",
            "Set behavior.operator_self_handle in ~/.dexter/config.toml or name a concrete Contacts recipient.",
        )
    } else if lower.contains("i need the contacts name") {
        (
            "Dexter refused a message send because the recipient was missing.",
            "Retry with the recipient's Contacts name.",
        )
    } else if lower.contains("what would you like to say") {
        (
            "Dexter refused a message send because the message body was missing.",
            "Retry with both the recipient and the exact message body.",
        )
    } else if lower.starts_with("here's the command:") {
        (
            "Dexter displayed a command instead of executing because the request looked like a command question.",
            "Copy and run the command yourself, or explicitly ask Dexter to execute it.",
        )
    } else if lower.contains("action denied before execution") {
        (
            "Dexter did not execute because the operator denied the approval request.",
            "Approve the action next time if the target and command are correct.",
        )
    } else {
        return None;
    };

    Some((diagnosis.to_string(), operator_next_step.to_string()))
}

fn is_explicit_web_action_request(user: &str) -> bool {
    let lower = user.to_ascii_lowercase();
    [
        "go online",
        "search the web",
        "search online",
        "look online",
        "browse",
        "open the website",
        "open this site",
        "find me",
        "find a ",
        "find an ",
        "download",
        "save to my desktop",
        "save it to my desktop",
        "grab the video",
        "rip the video",
        "yt-dlp",
        "full stream",
        "direct stream",
        "magnet",
        "torrent",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

fn is_tool_capability_denial(assistant_lower: &str) -> bool {
    [
        "i don't have a way to browse",
        "i do not have a way to browse",
        "i don't have the ability to browse",
        "i do not have the ability to browse",
        "i can't browse",
        "i cannot browse",
        "i can't access the web",
        "i cannot access the web",
        "i don't have the ability to access",
        "i do not have the ability to access",
        "my browsing is limited",
        "i'm stuck looking",
        "i am stuck looking",
        "if you find a direct",
        "if you give me a specific site",
        "if you have a specific link",
        "i don't \"find\"",
        "i do not \"find\"",
    ]
    .iter()
    .any(|pattern| assistant_lower.contains(pattern))
}

fn format_action_diagnostic_markdown(
    audit_log_path: &Path,
    receipts: &[ActionAuditReceipt],
    session_clue: Option<&SessionActionClue>,
    health_warnings: &[String],
    cause: &str,
    has_diagnostic: bool,
) -> String {
    let mut out = String::new();
    out.push_str("### Action Diagnostic\n\n");

    out.push_str("Most Likely Cause\n");
    if !has_diagnostic {
        out.push_str("- No recent action receipt or known refusal clue was found.\n");
        out.push_str("- This usually means the last turn was normal chat, the model never emitted an action, or the session did not persist yet.\n");
    } else if let Some(receipt) = receipts
        .first()
        .filter(|receipt| receipt.outcome != "executed")
    {
        let block = format_failed_action_evidence_block(receipt);
        debug_assert!(block.contains(cause));
        out.push_str(&block);
    } else if let Some(clue) = session_clue {
        out.push_str(&format!("- {}\n", clue.diagnosis));
        out.push_str(&format!("- Evidence: {}\n", clue.evidence));
        out.push_str(&format!("- Next step: {}\n", clue.operator_next_step));
    } else if let Some(receipt) = receipts.first() {
        out.push_str(&format_success_action_evidence_block(
            receipt,
            "The most recent audited action executed successfully.",
        ));
    }

    if let Some(clue) = session_clue {
        out.push('\n');
        out.push_str("Latest Session Clue\n");
        out.push_str(&format!("- Session: {}\n", clue.session_id));
        out.push_str(&format!("- Started: {}\n", clue.session_start));
        if let Some(end) = &clue.session_end {
            out.push_str(&format!("- Ended: {end}\n"));
        }
        if let Some(user_text) = &clue.user_text {
            out.push_str(&format!(
                "- User: {}\n",
                truncate_for_report(user_text, 220)
            ));
        }
        if let Some(assistant_text) = &clue.assistant_text {
            out.push_str(&format!(
                "- Assistant: {}\n",
                truncate_for_report(assistant_text, 280)
            ));
        }
    }

    out.push('\n');
    out.push_str("Recent Action Evidence\n");
    out.push_str(&format!("source: `{}`\n\n", audit_log_path.display()));
    if receipts.is_empty() {
        out.push_str("No action receipts found.\n");
    } else {
        for receipt in receipts {
            out.push_str(&format!(
                "- `{}` {} [{}]: {} - {}\n",
                receipt.outcome,
                receipt.action_type,
                action_review_label(&receipt.category),
                receipt.description,
                receipt.summary
            ));
        }
    }

    if !health_warnings.is_empty() {
        out.push('\n');
        out.push_str("Health Warnings That May Explain It\n");
        for warning in health_warnings {
            out.push_str(&format!("- {warning}\n"));
        }
    }

    out
}

fn action_review_label(category: &str) -> &'static str {
    match category {
        "safe" => "no approval required",
        "cautious" => "reviewed by policy",
        "destructive" => "approval required",
        _ => "approval required",
    }
}

fn clean_line_owned(value: String) -> Option<String> {
    let clean = value.replace('\r', " ").replace('\n', " ");
    let clean = clean.trim();
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

fn truncate_for_report(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        value.to_string()
    } else {
        let mut truncated: String = chars.into_iter().take(max_chars).collect();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::audit::{AuditEntry, AuditLog};
    use chrono::Utc;
    use tempfile::tempdir;

    fn write_audit_entry(
        state_dir: &Path,
        outcome: &'static str,
        error: Option<String>,
        operator_approved: Option<bool>,
    ) {
        let log = AuditLog::new(state_dir);
        let entry = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            action_id: "diag-action",
            r#type: "message_send",
            category: "cautious",
            spec_json: serde_json::json!({
                "recipient": "555-0100",
                "body": "hello",
                "rationale": "test"
            }),
            outcome,
            exit_code: None,
            output_preview: None,
            error,
            duration_ms: Some(1),
            operator_approved,
        };
        log.append(&entry).unwrap();
    }

    #[test]
    fn diagnostic_prefers_failed_receipt() {
        let tmp = tempdir().unwrap();
        write_audit_entry(
            tmp.path(),
            "failure",
            Some(
                "message_send actions must be resolved by the orchestrator before execution"
                    .to_string(),
            ),
            None,
        );

        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: None,
            current_assistant_text: None,
            health_warnings: Vec::new(),
            only_if_clue: false,
            ignore_action_receipts: false,
        })
        .unwrap();

        assert!(report.has_diagnostic);
        assert!(report.cause.contains("raw message_send action was blocked"));
        assert!(report.markdown.contains("Send iMessage to: 555-0100"));
        assert!(report
            .markdown
            .contains("Next step: Ask again using the recipient's exact Contacts name"));
    }

    #[test]
    fn diagnostic_detects_current_contacts_refusal_without_receipts() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some("text Nope Person hello".to_string()),
            current_assistant_text: Some(
                "I couldn't find Nope Person in Contacts, so I didn't send it.".to_string(),
            ),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(report.has_diagnostic);
        assert!(report.has_session_clue);
        assert!(report
            .cause
            .contains("Contacts did not contain the requested recipient"));
        assert!(report.markdown.contains("Latest Session Clue"));
    }

    #[test]
    fn diagnostic_detects_exact_contact_name_refusal_without_receipts() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some("send that to him".to_string()),
            current_assistant_text: Some(
                "I couldn't determine the exact Contacts recipient from that request, so I didn't send it. Ask again with the contact name exactly as it appears in Contacts.".to_string(),
            ),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(report.has_diagnostic);
        assert!(report
            .cause
            .contains("did not name an exact Contacts recipient"));
        assert!(report
            .markdown
            .contains("Retry with the recipient's Contacts name"));
    }

    #[test]
    fn diagnostic_detects_contact_handle_mismatch_without_receipts() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some("text Jason Phillips saying smoke".to_string()),
            current_assistant_text: Some(
                "I found that iMessage handle in Contacts, but it belongs to Jane Phillips, not Jason Phillips. I didn't send it.".to_string(),
            ),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(report.has_diagnostic);
        assert!(report
            .cause
            .contains("belonged to a different Contacts entry"));
        assert!(report.markdown.contains("Latest Session Clue"));
    }

    #[test]
    fn diagnostic_detects_contacts_lookup_failure_without_receipts() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some("text Jason Phillips saying smoke".to_string()),
            current_assistant_text: Some(
                "Contacts lookup failed while resolving Jason Phillips, so I didn't send it. Check Contacts access and try again.".to_string(),
            ),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(report.has_diagnostic);
        assert!(report.has_session_clue);
        assert!(report.cause.contains("Contacts lookup failed"));
        assert!(report.markdown.contains("Check Contacts access"));
    }

    #[test]
    fn diagnostic_detects_web_tool_capability_denial_without_receipts() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some(
                "go online and find me a public domain movie download".to_string(),
            ),
            current_assistant_text: Some(
                "I don't have the ability to browse the web for streaming links. If you have a specific link, I can inspect it.".to_string(),
            ),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(report.has_diagnostic);
        assert!(report.has_session_clue);
        assert!(report.cause.contains("tool-capability denial"));
        assert!(report
            .cause
            .contains("browser, shell, or download tooling never ran"));
        assert!(report.markdown.contains(
            "Fix the routing path so explicit web/search/download requests enter tool handling"
        ));
    }

    #[test]
    fn diagnostic_does_not_treat_capability_denial_as_action_clue_without_tool_request() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some("what are your limits".to_string()),
            current_assistant_text: Some(
                "I don't have the ability to browse the web unless you ask me to use tools."
                    .to_string(),
            ),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(!report.has_diagnostic);
        assert!(!report.has_session_clue);
    }

    #[test]
    fn only_if_clue_returns_false_for_normal_chat() {
        let tmp = tempdir().unwrap();
        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: tmp.path(),
            limit: 3,
            current_user_text: Some("what is two plus two".to_string()),
            current_assistant_text: Some("Two plus two is four.".to_string()),
            health_warnings: Vec::new(),
            only_if_clue: true,
            ignore_action_receipts: true,
        })
        .unwrap();

        assert!(!report.has_diagnostic);
        assert!(!report.has_session_clue);
    }
}
