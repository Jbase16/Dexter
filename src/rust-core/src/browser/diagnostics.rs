use serde::{Deserialize, Serialize};

use crate::voice::worker_client::WorkerError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFailureKind {
    WorkerNotStarted,
    BrowserLaunchFailed,
    PageNotReady,
    NavigationTimeout,
    NavigationFailed,
    SelectorNotFound,
    ClickFailed,
    TypingFailed,
    ExtractionFailed,
    ScreenshotFailed,
    WorkerProtocolError,
    WorkerTimeout,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRecoveryDirective {
    NoRetrySurfaceToOperator,
    RetrySameActionOnce,
    ExtractPageThenReplan,
    RestartWorkerThenRetryOnce,
    AskForClarification,
}

impl BrowserRecoveryDirective {
    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "no_retry_surface_to_operator" => Some(Self::NoRetrySurfaceToOperator),
            "retry_same_action_once" => Some(Self::RetrySameActionOnce),
            "extract_page_then_replan" => Some(Self::ExtractPageThenReplan),
            "restart_worker_then_retry_once" => Some(Self::RestartWorkerThenRetryOnce),
            "ask_for_clarification" => Some(Self::AskForClarification),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRetrySurfaceToOperator => "no_retry_surface_to_operator",
            Self::RetrySameActionOnce => "retry_same_action_once",
            Self::ExtractPageThenReplan => "extract_page_then_replan",
            Self::RestartWorkerThenRetryOnce => "restart_worker_then_retry_once",
            Self::AskForClarification => "ask_for_clarification",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Self::NoRetrySurfaceToOperator => {
                "Do not retry until the operator or recovery command changes browser health."
            }
            Self::RetrySameActionOnce => {
                "Retry at most once only if the action is still relevant and idempotent."
            }
            Self::ExtractPageThenReplan => {
                "Do not repeat the same selector. Inspect the current page, then choose a new selector or ask for clarification."
            }
            Self::RestartWorkerThenRetryOnce => {
                "Restart the browser worker, then retry at most once if the action is idempotent."
            }
            Self::AskForClarification => {
                "Do not guess. Ask for clarification or choose a safer reachable target."
            }
        }
    }
}

impl BrowserFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkerNotStarted => "worker_not_started",
            Self::BrowserLaunchFailed => "browser_launch_failed",
            Self::PageNotReady => "page_not_ready",
            Self::NavigationTimeout => "navigation_timeout",
            Self::NavigationFailed => "navigation_failed",
            Self::SelectorNotFound => "selector_not_found",
            Self::ClickFailed => "click_failed",
            Self::TypingFailed => "typing_failed",
            Self::ExtractionFailed => "extraction_failed",
            Self::ScreenshotFailed => "screenshot_failed",
            Self::WorkerProtocolError => "worker_protocol_error",
            Self::WorkerTimeout => "worker_timeout",
            Self::Unknown => "unknown",
        }
    }

    pub fn recovery_hint(self) -> &'static str {
        match self {
            Self::WorkerNotStarted => {
                "Restart the browser worker with `dexter-cli --restart-component browser`."
            }
            Self::BrowserLaunchFailed => {
                "Install Playwright Chromium with `src/python-workers/.venv/bin/python3 -m playwright install chromium`, then restart the browser worker."
            }
            Self::PageNotReady => "Restart the browser worker, then retry the browser action.",
            Self::NavigationTimeout => {
                "Check network/page availability or retry with a more specific URL."
            }
            Self::NavigationFailed => {
                "Check that the URL exists and is reachable, then retry navigation."
            }
            Self::SelectorNotFound => {
                "Inspect or extract the page before retrying with a selector that exists."
            }
            Self::ClickFailed => "Inspect the page state, then retry with a visible clickable selector.",
            Self::TypingFailed => {
                "Inspect the page state, then retry with a visible editable selector."
            }
            Self::ExtractionFailed => "Retry extraction after the page finishes loading.",
            Self::ScreenshotFailed => "Check screenshot permissions and Desktop write access.",
            Self::WorkerProtocolError => "Restart the browser worker; the worker response was malformed.",
            Self::WorkerTimeout => {
                "Restart the browser worker if the timeout repeats; the previous request may be wedged."
            }
            Self::Unknown => "Check browser worker health with `make status` or `make doctor`.",
        }
    }

    pub fn recovery_directive(self) -> BrowserRecoveryDirective {
        match self {
            Self::WorkerNotStarted | Self::BrowserLaunchFailed => {
                BrowserRecoveryDirective::NoRetrySurfaceToOperator
            }
            Self::PageNotReady | Self::NavigationTimeout => {
                BrowserRecoveryDirective::RetrySameActionOnce
            }
            Self::NavigationFailed => BrowserRecoveryDirective::AskForClarification,
            Self::SelectorNotFound
            | Self::ClickFailed
            | Self::TypingFailed
            | Self::ExtractionFailed => BrowserRecoveryDirective::ExtractPageThenReplan,
            Self::ScreenshotFailed => BrowserRecoveryDirective::NoRetrySurfaceToOperator,
            Self::WorkerProtocolError | Self::WorkerTimeout => {
                BrowserRecoveryDirective::RestartWorkerThenRetryOnce
            }
            Self::Unknown => BrowserRecoveryDirective::AskForClarification,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDiagnostic {
    pub kind: BrowserFailureKind,
    pub detail: String,
    pub recovery_hint: &'static str,
    pub recovery_directive: BrowserRecoveryDirective,
}

impl BrowserDiagnostic {
    pub fn new(kind: BrowserFailureKind, detail: impl Into<String>) -> Self {
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
                "Browser failure [{}]. Recovery: {} {}",
                self.kind.as_str(),
                self.recovery_hint,
                next
            )
        } else {
            format!(
                "Browser failure [{}]: {} Recovery: {} {}",
                self.kind.as_str(),
                self.detail.trim(),
                self.recovery_hint,
                next
            )
        }
    }
}

pub fn classify_worker_error(error: &WorkerError) -> BrowserDiagnostic {
    match error {
        WorkerError::SpawnFailed(inner) => {
            BrowserDiagnostic::new(BrowserFailureKind::WorkerNotStarted, inner.to_string())
        }
        WorkerError::HandshakeTimeout => {
            BrowserDiagnostic::new(BrowserFailureKind::BrowserLaunchFailed, error.to_string())
        }
        WorkerError::HandshakeFailed(detail) => {
            let kind = classify_error_text(detail, BrowserFailureKind::WorkerProtocolError);
            BrowserDiagnostic::new(kind, detail.clone())
        }
        WorkerError::WrongWorkerType { .. } | WorkerError::Io(_) => {
            let kind =
                classify_error_text(&error.to_string(), BrowserFailureKind::WorkerProtocolError);
            BrowserDiagnostic::new(kind, error.to_string())
        }
    }
}

pub fn classify_browser_result_error(action_label: &str, detail: &str) -> BrowserDiagnostic {
    let fallback = match action_label {
        "navigate" => BrowserFailureKind::NavigationTimeout,
        "click" => BrowserFailureKind::ClickFailed,
        "type" => BrowserFailureKind::TypingFailed,
        "extract" => BrowserFailureKind::ExtractionFailed,
        "screenshot" => BrowserFailureKind::ScreenshotFailed,
        _ => BrowserFailureKind::Unknown,
    };
    let kind = classify_error_text(detail, fallback);
    BrowserDiagnostic::new(kind, detail.to_string())
}

pub fn classify_worker_error_kind(kind: &str) -> Option<BrowserFailureKind> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "worker_not_started" => Some(BrowserFailureKind::WorkerNotStarted),
        "browser_launch_failed" => Some(BrowserFailureKind::BrowserLaunchFailed),
        "page_not_ready" => Some(BrowserFailureKind::PageNotReady),
        "navigation_timeout" => Some(BrowserFailureKind::NavigationTimeout),
        "navigation_failed" => Some(BrowserFailureKind::NavigationFailed),
        "selector_not_found" => Some(BrowserFailureKind::SelectorNotFound),
        "click_failed" => Some(BrowserFailureKind::ClickFailed),
        "typing_failed" => Some(BrowserFailureKind::TypingFailed),
        "extraction_failed" => Some(BrowserFailureKind::ExtractionFailed),
        "screenshot_failed" => Some(BrowserFailureKind::ScreenshotFailed),
        "worker_protocol_error" => Some(BrowserFailureKind::WorkerProtocolError),
        "worker_timeout" => Some(BrowserFailureKind::WorkerTimeout),
        "unknown" => Some(BrowserFailureKind::Unknown),
        _ => None,
    }
}

pub fn classify_error_text(detail: &str, fallback: BrowserFailureKind) -> BrowserFailureKind {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("executable doesn't exist")
        || lower.contains("playwright install")
        || lower.contains("browser.launch")
        || lower.contains("chromium.launch")
    {
        BrowserFailureKind::BrowserLaunchFailed
    } else if lower.contains("browser worker unavailable")
        || lower.contains("worker slot is none")
        || lower.contains("notconnected")
        || lower.contains("not connected")
    {
        BrowserFailureKind::WorkerNotStarted
    } else if lower.contains("handshake timed out") || lower.contains("timed out") {
        BrowserFailureKind::WorkerTimeout
    } else if lower.contains("non-utf8")
        || lower.contains("parse error")
        || lower.contains("wrong worker type")
    {
        BrowserFailureKind::WorkerProtocolError
    } else if lower.contains("element not found") || lower.contains("no elements found") {
        BrowserFailureKind::SelectorNotFound
    } else if lower.contains("page") && lower.contains("closed") {
        BrowserFailureKind::PageNotReady
    } else if lower.contains("navigation") && lower.contains("timeout") {
        BrowserFailureKind::NavigationTimeout
    } else if lower.contains("net::err_")
        || lower.contains("err_file_not_found")
        || lower.contains("err_name_not_resolved")
        || lower.contains("err_connection_refused")
    {
        BrowserFailureKind::NavigationFailed
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_missing_playwright_executable_as_launch_failed() {
        let diagnostic = classify_error_text(
            "BrowserType.launch: Executable doesn't exist at /Users/jason/Library/Caches/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell",
            BrowserFailureKind::Unknown,
        );

        assert_eq!(diagnostic, BrowserFailureKind::BrowserLaunchFailed);
    }

    #[test]
    fn browser_diagnostic_message_contains_kind_detail_and_recovery() {
        let diagnostic = BrowserDiagnostic::new(
            BrowserFailureKind::SelectorNotFound,
            "element not found: '#submit'",
        );
        let message = diagnostic.operator_message();

        assert!(message.contains("selector_not_found"));
        assert!(message.contains("#submit"));
        assert!(message.contains("Inspect or extract"));
        assert!(message.contains("extract_page_then_replan"));
    }

    #[test]
    fn classifies_worker_error_kind_from_structured_browser_result() {
        assert_eq!(
            classify_worker_error_kind("selector_not_found"),
            Some(BrowserFailureKind::SelectorNotFound)
        );
        assert_eq!(
            classify_worker_error_kind("navigation_failed"),
            Some(BrowserFailureKind::NavigationFailed)
        );
    }

    #[test]
    fn classifies_chromium_network_errors_as_navigation_failed() {
        let diagnostic = classify_error_text(
            "page.goto: net::ERR_FILE_NOT_FOUND at file:///tmp/missing.html",
            BrowserFailureKind::Unknown,
        );

        assert_eq!(diagnostic, BrowserFailureKind::NavigationFailed);
    }

    #[test]
    fn recovery_directive_maps_selector_failure_to_extract_replan() {
        assert_eq!(
            BrowserFailureKind::SelectorNotFound.recovery_directive(),
            BrowserRecoveryDirective::ExtractPageThenReplan
        );
        assert_eq!(
            BrowserFailureKind::BrowserLaunchFailed.recovery_directive(),
            BrowserRecoveryDirective::NoRetrySurfaceToOperator
        );
        assert_eq!(
            BrowserFailureKind::WorkerTimeout.recovery_directive(),
            BrowserRecoveryDirective::RestartWorkerThenRetryOnce
        );
    }
}
