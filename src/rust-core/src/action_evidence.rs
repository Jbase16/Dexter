/// Shared operator-facing interpretation of audited action receipts.
///
/// This module is deliberately type-light: both the daemon and `dexter-cli`
/// reconstruct action receipts into slightly different structs, but the
/// operator-facing cause/next-step wording must not drift between them.
pub(crate) trait ActionEvidence {
    fn action_outcome(&self) -> &str;
    fn action_type(&self) -> &str;
    fn action_target(&self) -> &str;
    fn result_summary(&self) -> &str;
}

pub(crate) fn action_receipt_diagnosis(receipt: &impl ActionEvidence) -> String {
    let status = receipt.action_outcome();
    let action_type = receipt.action_type();
    let result = receipt.result_summary().to_ascii_lowercase();

    if status == "denied" {
        return "The action reached the approval gate and was denied before execution.".to_string();
    }
    if status == "expired" {
        return "The action reached the approval gate, but approval expired before execution."
            .to_string();
    }
    if status == "abandoned" {
        return "The action was abandoned before approval, likely because the session ended."
            .to_string();
    }
    if action_type == "message_send" && result.contains("must be resolved by the orchestrator") {
        return "A raw message_send action was blocked because recipient resolution must happen through Rust-side Contacts lookup.".to_string();
    }
    if result.contains("timed out") {
        return "The external tool or worker timed out before Dexter could complete the action."
            .to_string();
    }
    if action_type == "applescript" {
        return "AppleScript execution failed after the action was approved or allowed by policy."
            .to_string();
    }
    if action_type == "browser" {
        return "Browser automation failed after the action was dispatched.".to_string();
    }
    if let Some(kind) = extract_ui_failure_kind(receipt.result_summary()) {
        return format!("UI automation failed after the action was dispatched ({kind}).");
    }
    if action_type == "shell" {
        return "The shell command ran but returned a failure.".to_string();
    }

    "The action reached the engine but did not complete successfully.".to_string()
}

pub(crate) fn action_receipt_next_step(receipt: &impl ActionEvidence) -> &'static str {
    let status = receipt.action_outcome();
    let action_type = receipt.action_type();
    let result = receipt.result_summary().to_ascii_lowercase();

    if status == "denied" {
        return "Re-run the request and approve it only if the shown target and action are correct.";
    }
    if status == "expired" {
        return "Re-run the request and answer the approval prompt before it expires.";
    }
    if status == "abandoned" {
        return "Re-run the request in a fresh session if the action is still needed.";
    }
    if action_type == "message_send" && result.contains("must be resolved by the orchestrator") {
        return "Ask again using the recipient's exact Contacts name so Rust can resolve the handle before approval.";
    }
    if result.contains("timed out") {
        return "Check whether the external tool or worker is hung, then retry with a smaller or more specific action.";
    }
    if action_type == "applescript" {
        return "Check the AppleScript error text and macOS permissions, then retry the smallest safe action.";
    }
    if action_type == "browser" {
        return "Check browser worker health with `make status` or `make doctor`, then retry the browser action.";
    }
    if let Some(kind) = extract_ui_failure_kind(receipt.result_summary()) {
        return ui_failure_next_step(kind);
    }
    if action_type == "shell" {
        return "Inspect the command, working directory, and exit code before retrying.";
    }

    "Inspect the recent action receipt and retry only after the target and inputs are clear."
}

fn ui_failure_next_step(kind: &str) -> &'static str {
    match kind {
        "control_not_found" | "control_disabled" | "not_typeable" | "not_selectable"
        | "option_not_found" => {
            "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly."
        }
        "no_front_window" => "Inspect or focus the target window before retrying the UI action.",
        "ambiguous_control" => {
            "Ask which matching control to use, or collect a UI snapshot and choose a more specific target."
        }
        "app_not_running" => "Open the target app or ask for the correct running app before retrying.",
        "accessibility_denied" => {
            "Grant Accessibility permission for Dexter/Terminal, then retry the UI action."
        }
        "action_timeout" | "applescript_failed" => {
            "Check macOS UI state and permissions before retrying the smallest safe UI action."
        }
        _ => "Inspect the current UI state before retrying the UI action.",
    }
}

fn extract_ui_failure_kind(summary: &str) -> Option<&str> {
    if let Some(rest) = summary.strip_prefix("UI failed (") {
        return rest.split_once(')').map(|(kind, _)| kind);
    }
    if let Some(rest) = summary.strip_prefix("Failed: UI failure [") {
        return rest.split_once(']').map(|(kind, _)| kind);
    }
    if let Some(rest) = summary.strip_prefix("Timed out: UI failure [") {
        return rest.split_once(']').map(|(kind, _)| kind);
    }
    None
}

pub(crate) fn format_failed_action_evidence_block(receipt: &impl ActionEvidence) -> String {
    format!(
        "- {}\n- Evidence: {}\n- Target: {}\n- Next step: {}\n",
        action_receipt_diagnosis(receipt),
        receipt.result_summary(),
        receipt.action_target(),
        action_receipt_next_step(receipt),
    )
}

pub(crate) fn format_success_action_evidence_block(
    receipt: &impl ActionEvidence,
    success_line: &str,
) -> String {
    format!(
        "- {success_line}\n- Evidence: {}\n- Target: {}\n",
        receipt.result_summary(),
        receipt.action_target(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEvidence {
        outcome: &'static str,
        action_type: &'static str,
        summary: &'static str,
    }

    impl ActionEvidence for TestEvidence {
        fn action_outcome(&self) -> &str {
            self.outcome
        }

        fn action_type(&self) -> &str {
            self.action_type
        }

        fn action_target(&self) -> &str {
            "test target"
        }

        fn result_summary(&self) -> &str {
            self.summary
        }
    }

    #[test]
    fn raw_message_send_receipt_has_contacts_specific_copy() {
        let receipt = TestEvidence {
            outcome: "failed",
            action_type: "message_send",
            summary:
                "Failed: message_send actions must be resolved by the orchestrator before execution",
        };

        assert!(action_receipt_diagnosis(&receipt).contains("Rust-side Contacts lookup"));
        assert!(action_receipt_next_step(&receipt).contains("exact Contacts name"));
        assert!(format_failed_action_evidence_block(&receipt).contains("Target: test target"));
    }

    #[test]
    fn denied_receipt_has_approval_copy() {
        let receipt = TestEvidence {
            outcome: "denied",
            action_type: "shell",
            summary: "Action denied before execution.",
        };

        assert!(action_receipt_diagnosis(&receipt).contains("denied before execution"));
        assert!(action_receipt_next_step(&receipt).contains("approve it only if"));
    }

    #[test]
    fn ui_failure_receipt_has_snapshot_guidance() {
        let receipt = TestEvidence {
            outcome: "failed",
            action_type: "ui_click",
            summary: "UI failed (control_not_found): no matching control for 'Save'. Recovery: Capture a UI snapshot and target a visible control from it. Next [snapshot_then_replan]: Inspect the current UI snapshot before choosing another control.",
        };

        assert!(action_receipt_diagnosis(&receipt).contains("control_not_found"));
        assert!(action_receipt_next_step(&receipt).contains("UI snapshot"));
        assert!(action_receipt_next_step(&receipt).contains("Do not repeat"));
    }
}
