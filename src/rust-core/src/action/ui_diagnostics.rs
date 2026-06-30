use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiFailureKind {
    AppNotRunning,
    NoFrontWindow,
    ControlNotFound,
    AmbiguousControl,
    ControlDisabled,
    NotTypeable,
    NotSelectable,
    OptionNotFound,
    ActionTimeout,
    AccessibilityDenied,
    AppleScriptFailed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRecoveryDirective {
    NoRetrySurfaceToOperator,
    SnapshotThenReplan,
    InspectWindowThenReplan,
    AskForClarification,
}

impl UiRecoveryDirective {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRetrySurfaceToOperator => "no_retry_surface_to_operator",
            Self::SnapshotThenReplan => "snapshot_then_replan",
            Self::InspectWindowThenReplan => "inspect_window_then_replan",
            Self::AskForClarification => "ask_for_clarification",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Self::NoRetrySurfaceToOperator => {
                "Do not retry until the operator fixes permissions, app state, or the underlying AppleScript failure."
            }
            Self::SnapshotThenReplan => {
                "Inspect the current UI snapshot before choosing another control. Do not repeat the same label blindly."
            }
            Self::InspectWindowThenReplan => {
                "Inspect the current app/window state before choosing another target."
            }
            Self::AskForClarification => {
                "Do not guess. Ask which app, window, or matching control the operator means."
            }
        }
    }
}

impl UiFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AppNotRunning => "app_not_running",
            Self::NoFrontWindow => "no_front_window",
            Self::ControlNotFound => "control_not_found",
            Self::AmbiguousControl => "ambiguous_control",
            Self::ControlDisabled => "control_disabled",
            Self::NotTypeable => "not_typeable",
            Self::NotSelectable => "not_selectable",
            Self::OptionNotFound => "option_not_found",
            Self::ActionTimeout => "action_timeout",
            Self::AccessibilityDenied => "accessibility_denied",
            Self::AppleScriptFailed => "applescript_failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn recovery_hint(self) -> &'static str {
        match self {
            Self::AppNotRunning => "Open the target app or ask Dexter to target a running app.",
            Self::NoFrontWindow => "Inspect or focus the target window before retrying.",
            Self::ControlNotFound => "Capture a UI snapshot and target a visible control from it.",
            Self::AmbiguousControl => "Disambiguate the target label, role, window, or container.",
            Self::ControlDisabled => {
                "Inspect the current UI state; the requested control is present but disabled."
            }
            Self::NotTypeable => "Target a visible editable text field or text area.",
            Self::NotSelectable => "Target a selectable popup, menu, combo box, row, or item.",
            Self::OptionNotFound => "Inspect the available options before selecting again.",
            Self::ActionTimeout => {
                "Check whether the UI or AppleScript runner is hung before retrying."
            }
            Self::AccessibilityDenied => {
                "Grant Accessibility permission for Dexter/Terminal, then retry."
            }
            Self::AppleScriptFailed => {
                "Inspect the AppleScript error and macOS permissions before retrying."
            }
            Self::Unknown => "Inspect the action receipt and current UI state before retrying.",
        }
    }

    pub fn recovery_directive(self) -> UiRecoveryDirective {
        match self {
            Self::AppNotRunning | Self::AmbiguousControl | Self::Unknown => {
                UiRecoveryDirective::AskForClarification
            }
            Self::NoFrontWindow => UiRecoveryDirective::InspectWindowThenReplan,
            Self::ControlNotFound
            | Self::ControlDisabled
            | Self::NotTypeable
            | Self::NotSelectable
            | Self::OptionNotFound => UiRecoveryDirective::SnapshotThenReplan,
            Self::ActionTimeout | Self::AccessibilityDenied | Self::AppleScriptFailed => {
                UiRecoveryDirective::NoRetrySurfaceToOperator
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiDiagnostic {
    pub kind: UiFailureKind,
    pub detail: String,
    pub recovery_hint: &'static str,
    pub recovery_directive: UiRecoveryDirective,
}

impl UiDiagnostic {
    pub fn new(kind: UiFailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            recovery_hint: kind.recovery_hint(),
            recovery_directive: kind.recovery_directive(),
        }
    }

    pub fn operator_message(&self) -> String {
        let next = format!(
            "Next [{}]: {}",
            self.recovery_directive.as_str(),
            self.recovery_directive.instruction()
        );
        if self.detail.trim().is_empty() {
            format!(
                "UI failure [{}]. Recovery: {} {}",
                self.kind.as_str(),
                self.recovery_hint,
                next
            )
        } else {
            format!(
                "UI failure [{}]: Recovery: {} {} Detail: {}",
                self.kind.as_str(),
                self.recovery_hint,
                next,
                self.detail.trim()
            )
        }
    }
}

pub fn is_ui_or_window_action(action_type: &str) -> bool {
    matches!(
        action_type,
        "window_focus"
            | "window_inspect"
            | "ui_snapshot"
            | "ui_click"
            | "ui_type"
            | "ui_select"
            | "ui_toggle"
            | "ui_pick"
    )
}

pub fn classify_ui_error(action_type: &str, detail: &str) -> UiDiagnostic {
    let kind = classify_ui_error_text(action_type, detail);
    UiDiagnostic::new(kind, compact_detail(detail))
}

pub fn classify_ui_error_text(action_type: &str, detail: &str) -> UiFailureKind {
    let lower = detail.to_ascii_lowercase();

    if lower.contains("not authorized to send apple events")
        || lower.contains("not allowed assistive access")
        || lower.contains("not authorized for accessibility")
        || lower.contains("not permitted to send keystrokes")
        || lower.contains("operation is not permitted")
        || lower.contains("not permitted")
    {
        return UiFailureKind::AccessibilityDenied;
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return UiFailureKind::ActionTimeout;
    }
    if lower.contains("app not running")
        || lower.contains("can't get application process")
        || lower.contains("can’t get application process")
    {
        return UiFailureKind::AppNotRunning;
    }
    if lower.contains("no front window") || lower.contains("front window: (none)") {
        return UiFailureKind::NoFrontWindow;
    }
    if lower.contains("ambiguous") {
        return UiFailureKind::AmbiguousControl;
    }
    if lower.contains("matched control is disabled")
        || lower.contains("matched item is disabled")
        || lower.contains("requested control is present but disabled")
    {
        return UiFailureKind::ControlDisabled;
    }
    if lower.contains("no exact option") || lower.contains("ambiguous option") {
        return UiFailureKind::OptionNotFound;
    }
    if lower.contains("no matching text control")
        || lower.contains("could not set text value")
        || lower.contains("not typeable")
    {
        return UiFailureKind::NotTypeable;
    }
    if lower.contains("no matching selectable control")
        || lower.contains("no matching toggle control")
        || lower.contains("no matching visible item")
        || lower.contains("could not select item")
        || lower.contains("selected state remained false")
        || lower.contains("unsupported toggle value")
        || lower.contains("has no readable value")
        || lower.contains("value is missing")
    {
        return UiFailureKind::NotSelectable;
    }
    if lower.contains("no matching control") || lower.contains("no matching container") {
        return UiFailureKind::ControlNotFound;
    }
    if is_ui_or_window_action(action_type)
        && (lower.contains("execution error")
            || lower.contains("osascript")
            || lower.contains("apple event"))
    {
        return UiFailureKind::AppleScriptFailed;
    }

    UiFailureKind::Unknown
}

pub fn ui_failure_summary(error: &str) -> Option<String> {
    let prefix = "UI failure [";
    let rest = error.strip_prefix(prefix)?;
    let (kind, detail) = rest.split_once("]: ")?;
    Some(format_compact_ui_failure_summary(kind, detail))
}

fn format_compact_ui_failure_summary(kind: &str, detail: &str) -> String {
    const SUMMARY_MAX_CHARS: usize = 900;
    const CAUSE_MAX_CHARS: usize = 220;
    const TARGET_MAX_CHARS: usize = 280;
    const EVIDENCE_MAX_CHARS: usize = 360;

    let detail_segment = marker_segment(detail, "Detail:");
    let cause_source = detail_segment.as_deref().unwrap_or(detail);
    let cause = segment_until_any(
        cause_source,
        &["Target:", "Evidence:", "Recovery:", "Next ["],
    )
    .map(|value| compact_for_receipt(value, CAUSE_MAX_CHARS));
    let recovery = marker_segment(detail, "Recovery:")
        .map(|value| compact_for_receipt(&format!("Recovery: {value}"), CAUSE_MAX_CHARS));
    let next = marker_segment(detail, "Next [")
        .map(|value| compact_for_receipt(&format!("Next [{value}"), CAUSE_MAX_CHARS));
    let target = marker_segment(detail, "Target:")
        .map(|value| compact_for_receipt(&format!("Target: {value}"), TARGET_MAX_CHARS));
    let evidence = marker_segment(detail, "Evidence:")
        .map(|value| compact_for_receipt(&format!("Evidence: {value}"), EVIDENCE_MAX_CHARS));

    let mut parts = Vec::new();
    if let Some(cause) = cause {
        parts.push(format!("UI failed ({kind}): {cause}"));
    } else {
        parts.push(format!("UI failed ({kind})."));
    }
    if let Some(recovery) = recovery {
        parts.push(recovery);
    }
    if let Some(next) = next {
        parts.push(next);
    }
    if let Some(target) = target {
        parts.push(target);
    }
    if let Some(evidence) = evidence {
        parts.push(evidence);
    }

    compact_for_receipt(&parts.join(" "), SUMMARY_MAX_CHARS)
}

fn marker_segment(detail: &str, marker: &str) -> Option<String> {
    let (_, rest) = detail.split_once(marker)?;
    segment_until_any(
        rest,
        &["Recovery:", "Next [", "Detail:", "Target:", "Evidence:"],
    )
    .map(ToOwned::to_owned)
}

fn segment_until_any<'a>(value: &'a str, markers: &[&str]) -> Option<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let end = markers
        .iter()
        .filter_map(|marker| trimmed.find(marker))
        .filter(|idx| *idx > 0)
        .min()
        .unwrap_or(trimmed.len());
    let segment = trimmed[..end].trim().trim_end_matches('.').trim();
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

fn compact_for_receipt(value: &str, max_chars: usize) -> String {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let mut truncated: String = cleaned.chars().take(max_chars.saturating_sub(3)).collect();
    truncated.push_str("...");
    truncated
}

fn compact_detail(detail: &str) -> String {
    let cleaned = detail
        .replace('\r', " ")
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned = strip_osascript_boilerplate(&cleaned);
    cleaned.chars().take(1200).collect()
}

fn strip_osascript_boilerplate(detail: &str) -> String {
    let Some(idx) = detail.find("execution error:") else {
        return detail.to_string();
    };
    detail[idx + "execution error:".len()..]
        .trim()
        .trim_end_matches("(-1728)")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_missing_control() {
        let diagnostic = classify_ui_error(
            "ui_click",
            "execution error: ui control press failed: no matching control for 'Save' (-1728)",
        );

        assert_eq!(diagnostic.kind, UiFailureKind::ControlNotFound);
        assert_eq!(
            diagnostic.recovery_directive,
            UiRecoveryDirective::SnapshotThenReplan
        );
        assert!(diagnostic.operator_message().contains("control_not_found"));
    }

    #[test]
    fn classifies_ambiguous_control() {
        let diagnostic = classify_ui_error(
            "ui_click",
            "execution error: ui control press failed: ambiguous exact match for 'OK' (-1728)",
        );

        assert_eq!(diagnostic.kind, UiFailureKind::AmbiguousControl);
        assert_eq!(
            diagnostic.recovery_directive,
            UiRecoveryDirective::AskForClarification
        );
    }

    #[test]
    fn classifies_disabled_control() {
        let diagnostic = classify_ui_error(
            "ui_toggle",
            "execution error: ui toggle failed: matched control is disabled: AXCheckBox | name='Sync' (-1728)",
        );

        assert_eq!(diagnostic.kind, UiFailureKind::ControlDisabled);
        assert_eq!(
            diagnostic.recovery_directive,
            UiRecoveryDirective::SnapshotThenReplan
        );
    }

    #[test]
    fn disabled_candidate_evidence_does_not_override_missing_control_cause() {
        let diagnostic = classify_ui_error(
            "ui_click",
            "execution error: ui control press failed: no matching control for 'Save'. Target: action=ui_click app=Fixture window='Fixture' role=AXButton label='Save' container=<none>. Evidence: match_count=0 nearest_safe_candidates: AXButton | name='Disabled action' | enabled=false (-1728)",
        );

        assert_eq!(diagnostic.kind, UiFailureKind::ControlNotFound);
        assert_eq!(
            diagnostic.recovery_directive,
            UiRecoveryDirective::SnapshotThenReplan
        );
    }

    #[test]
    fn classifies_type_and_pick_failures() {
        assert_eq!(
            classify_ui_error_text(
                "ui_type",
                "ui type failed: no matching text control for 'Name'"
            ),
            UiFailureKind::NotTypeable
        );
        assert_eq!(
            classify_ui_error_text(
                "ui_pick",
                "ui pick failed: no matching visible item for 'Invoices'"
            ),
            UiFailureKind::NotSelectable
        );
    }

    #[test]
    fn classifies_accessibility_denial() {
        let diagnostic = classify_ui_error(
            "ui_snapshot",
            "System Events got an error: osascript is not allowed assistive access.",
        );

        assert_eq!(diagnostic.kind, UiFailureKind::AccessibilityDenied);
        assert_eq!(
            diagnostic.recovery_directive,
            UiRecoveryDirective::NoRetrySurfaceToOperator
        );
    }

    #[test]
    fn parses_ui_failure_summary_kind() {
        let summary = ui_failure_summary(
            "UI failure [control_not_found]: no matching control for 'Save'. Recovery: inspect",
        )
        .expect("summary should parse");

        assert!(summary.starts_with("UI failed (control_not_found):"));
        assert!(summary.contains("Recovery: inspect"));
    }

    #[test]
    fn ui_failure_summary_prioritizes_target_and_evidence() {
        let long_filler = "background-noise ".repeat(80);
        let summary = ui_failure_summary(&format!(
            "UI failure [control_disabled]: Recovery: Inspect the current UI state; the requested control is present but disabled. Next [snapshot_then_replan]: Inspect the current UI snapshot before choosing another control. Do not repeat the same label blindly. Detail: ui type failed: matched control is disabled. {long_filler} Target: action=ui_type app=DexterHUDUIFailureFixture window='Dexter HUD UI Failure Fixture' role=AXTextField label='Disabled secret field' container=<none> text=<redacted>. Evidence: matched_control: AXTextField | name='Disabled secret field' | enabled=false | frame={{x=1,y=2,w=3,h=4}}"
        ))
        .expect("summary should parse");

        assert!(summary.chars().count() <= 900);
        assert!(summary.contains("UI failed (control_disabled):"));
        assert!(summary.contains("Next [snapshot_then_replan]"));
        assert!(summary.contains("Target: action=ui_type"));
        assert!(summary.contains("text=<redacted>"));
        assert!(summary.contains("Evidence: matched_control:"));
        assert!(summary.contains("enabled=false"));
        assert!(!summary.contains("TOP_SECRET_TYPED_VALUE"));
    }

    #[test]
    fn ui_failure_summary_keeps_nearest_candidates_visible() {
        let summary = ui_failure_summary(
            "UI failure [control_not_found]: Recovery: Capture a UI snapshot and target a visible control from it. Next [snapshot_then_replan]: Inspect the current UI snapshot before choosing another control. Do not repeat the same label blindly. Detail: ui control press failed: no matching control for 'Save'. Target: action=ui_click app=Finder window='Preferences' role=AXButton label='Save' container=<none>. Evidence: match_count=0 nearest_safe_candidates: AXButton | name='OK' | identifier='firstOK' | enabled=true | frame={x=74,y=56,w=110,h=34}; AXButton | name='Cancel' | identifier='cancel' | enabled=true | frame={x=236,y=56,w=110,h=34}",
        )
        .expect("summary should parse");

        assert!(summary.contains("Target: action=ui_click"));
        assert!(summary.contains("Evidence: match_count=0"));
        assert!(summary.contains("nearest_safe_candidates"));
        assert!(summary.contains("identifier='firstOK'"));
    }

    #[test]
    fn preserves_ui_replan_evidence_in_operator_message() {
        let diagnostic = classify_ui_error(
            "ui_click",
            "execution error: ui control press failed: no matching control for 'Save'. Target: action=ui_click app=DexterHUDUIFailureFixture window='Dexter HUD UI Failure Fixture' role=AXButton label='Save' container=<none>. Evidence: match_count=0 nearest_safe_candidates: AXButton | name='OK' | identifier='firstOK' | enabled=true | frame={x=74,y=56,w=110,h=34}; AXButton | name='OK' | identifier='secondOK' | enabled=true | frame={x=236,y=56,w=110,h=34} (-1728)",
        );
        let message = diagnostic.operator_message();

        assert!(message.contains("control_not_found"));
        assert!(message.contains("Target: action=ui_click"));
        assert!(message.contains("Evidence: match_count=0"));
        assert!(message.contains("identifier='firstOK'"));
        assert!(message.contains("Next [snapshot_then_replan]"));
    }
}
