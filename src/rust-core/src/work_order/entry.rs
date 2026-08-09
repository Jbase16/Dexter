//! Capability-general turn entry for DEX-03 Slice B.
//!
//! B1 is deliberately decision-only: it can prove that a turn is clearly
//! unrelated to implemented actions or forward a bounded descriptor set to
//! Stage 2. It does not yet create a live [`super::types::WorkOrder`] or change
//! the HUD/TTS response path.

// B1 is exercised as a complete decision-only unit.
// TODO(B2): remove when the first live entry caller is wired.
#![cfg_attr(not(test), allow(dead_code))]

use std::{collections::HashSet, time::Instant};

use serde::Serialize;

use crate::action::engine::{ActionSpec, BrowserActionKind};

use super::types::{EvidenceSource, WorkOrderScope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityId {
    Shell,
    FileRead,
    FileWrite,
    AppleScript,
    MessageSend,
    WindowFocus,
    WindowInspect,
    UiSnapshot,
    UiClick,
    UiType,
    UiSelect,
    UiToggle,
    UiPick,
    BrowserNavigate,
    BrowserClick,
    BrowserType,
    BrowserExtract,
    BrowserScreenshot,
    Shortcut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityRisk {
    ReadsOperatorData,
    WritesOperatorData,
    ExecutesModelSuppliedProgram,
    ExternalCommunication,
    LocalUiMutation,
    BrowserMutation,
    MayRequireApproval,
}

/// Rust-owned description of one action shape already implemented by Dexter.
///
/// String fields are static vocabulary, never model output. Targets such as an
/// app name, URL, path, process, or control label are required inputs and are
/// bound later; they are never encoded as capability-specific phrase rules.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CapabilityDescriptor {
    pub(crate) id: CapabilityId,
    /// One clean sentence reserved for Stage 2 embedding and Stage 3 classifier
    /// context. Stage 1 must never derive lexical anchors from this prose.
    pub(crate) purpose: &'static str,
    /// Explicit canonical concepts used only by the Stage 1 lexical floor.
    pub(crate) match_terms: &'static [&'static str],
    pub(crate) required_inputs: &'static [&'static str],
    pub(crate) possible_outputs: &'static [&'static str],
    pub(crate) expected_effects: &'static [&'static str],
    pub(crate) verification_sources: &'static [EvidenceSource],
    pub(crate) reach: WorkOrderScope,
    pub(crate) risk_properties: &'static [CapabilityRisk],
    pub(crate) preconditions: &'static [&'static str],
    pub(crate) estimated_latency_ms: u32,
    pub(crate) fallback_rank: u8,
}

const ACTION_RECEIPT: &[EvidenceSource] = &[EvidenceSource::ActionReceipt];
const ACTION_AND_CONTEXT: &[EvidenceSource] = &[
    EvidenceSource::ActionReceipt,
    EvidenceSource::ContextSnapshot,
];
const BROWSER_EVIDENCE: &[EvidenceSource] =
    &[EvidenceSource::ActionReceipt, EvidenceSource::BrowserResult];

const SHELL: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::Shell,
    purpose: "Execute a local shell command and return its structured result.",
    match_terms: &["run", "inspect", "delete", "command", "process", "system"],
    required_inputs: &["command arguments"],
    possible_outputs: &["standard output", "standard error", "exit status"],
    expected_effects: &["local process execution"],
    verification_sources: ACTION_RECEIPT,
    reach: WorkOrderScope::Process,
    risk_properties: &[
        CapabilityRisk::ExecutesModelSuppliedProgram,
        CapabilityRisk::MayRequireApproval,
    ],
    preconditions: &["policy permits the exact command"],
    estimated_latency_ms: 1_000,
    fallback_rank: 50,
};

const FILE_READ: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::FileRead,
    purpose: "Read content from a requested local file.",
    match_terms: &["read", "file", "content", "path"],
    required_inputs: &["path"],
    possible_outputs: &["file content"],
    expected_effects: &["local file read"],
    verification_sources: ACTION_RECEIPT,
    reach: WorkOrderScope::Filesystem,
    risk_properties: &[CapabilityRisk::ReadsOperatorData],
    preconditions: &["path resolves locally"],
    estimated_latency_ms: 100,
    fallback_rank: 10,
};

const FILE_WRITE: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::FileWrite,
    purpose: "Create, update, or delete content at a requested local file path.",
    match_terms: &["write", "delete", "file", "content", "path"],
    required_inputs: &["path", "content"],
    possible_outputs: &["written path"],
    expected_effects: &["local file mutation"],
    verification_sources: ACTION_RECEIPT,
    reach: WorkOrderScope::Filesystem,
    risk_properties: &[
        CapabilityRisk::WritesOperatorData,
        CapabilityRisk::MayRequireApproval,
    ],
    preconditions: &["parent path is permitted", "policy permits the write"],
    estimated_latency_ms: 150,
    fallback_rank: 10,
};

const APPLE_SCRIPT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::AppleScript,
    purpose: "Run an AppleScript to automate a local application or macOS service.",
    match_terms: &["run", "applescript", "automation", "application", "system"],
    required_inputs: &["script"],
    possible_outputs: &["script result"],
    expected_effects: &["local application or system automation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[
        CapabilityRisk::ExecutesModelSuppliedProgram,
        CapabilityRisk::LocalUiMutation,
        CapabilityRisk::MayRequireApproval,
    ],
    preconditions: &[
        "policy permits the script",
        "required macOS access is available",
    ],
    estimated_latency_ms: 1_000,
    fallback_rank: 80,
};

const MESSAGE_SEND: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::MessageSend,
    purpose: "Send a message to a recipient resolved from trusted local state.",
    match_terms: &["send", "recipient", "contact", "body"],
    required_inputs: &["recipient", "message body"],
    possible_outputs: &["send status"],
    expected_effects: &["external message delivery"],
    verification_sources: ACTION_RECEIPT,
    reach: WorkOrderScope::External,
    risk_properties: &[
        CapabilityRisk::ExternalCommunication,
        CapabilityRisk::MayRequireApproval,
    ],
    preconditions: &["recipient is resolved by trusted local state"],
    estimated_latency_ms: 1_500,
    fallback_rank: 20,
};

const WINDOW_FOCUS: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::WindowFocus,
    purpose: "Bring a local application window to the foreground.",
    match_terms: &["focus", "application", "window"],
    required_inputs: &["application name", "optional window title"],
    possible_outputs: &["frontmost application"],
    expected_effects: &["local window focus change"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::LocalUiMutation],
    preconditions: &["application exists locally"],
    estimated_latency_ms: 300,
    fallback_rank: 5,
};

const WINDOW_INSPECT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::WindowInspect,
    purpose: "Inspect visible windows belonging to a local application.",
    match_terms: &["inspect", "application", "window", "title"],
    required_inputs: &["optional application name"],
    possible_outputs: &["visible windows", "window titles"],
    expected_effects: &["local window observation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::ReadsOperatorData],
    preconditions: &["accessibility observation is available"],
    estimated_latency_ms: 250,
    fallback_rank: 5,
};

const UI_SNAPSHOT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::UiSnapshot,
    purpose: "Capture the visible accessibility controls of a local application.",
    match_terms: &[
        "snapshot",
        "inspect",
        "application",
        "control",
        "button",
        "field",
        "menu",
        "row",
    ],
    required_inputs: &["optional application name"],
    possible_outputs: &["accessibility control tree"],
    expected_effects: &["local user interface observation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::ReadsOperatorData],
    preconditions: &["accessibility permission is available"],
    estimated_latency_ms: 350,
    fallback_rank: 10,
};

const UI_CLICK: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::UiClick,
    purpose: "Activate a named button, link, or control in a local application.",
    match_terms: &["click", "application", "button", "control", "link"],
    required_inputs: &["control label", "optional application", "optional role"],
    possible_outputs: &["structured control result"],
    expected_effects: &["local control activation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::LocalUiMutation],
    preconditions: &["target control is visible and selectable"],
    estimated_latency_ms: 400,
    fallback_rank: 10,
};

const UI_TYPE: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::UiType,
    purpose: "Enter text into a named editable field in a local application.",
    match_terms: &["type", "application", "field", "control"],
    required_inputs: &["text", "optional field label", "optional application"],
    possible_outputs: &["structured typing result"],
    expected_effects: &["local field content mutation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[
        CapabilityRisk::LocalUiMutation,
        CapabilityRisk::WritesOperatorData,
    ],
    preconditions: &["target field is visible and editable"],
    estimated_latency_ms: 400,
    fallback_rank: 10,
};

const UI_SELECT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::UiSelect,
    purpose: "Select an option from a menu or selection control in a local application.",
    match_terms: &[
        "select",
        "application",
        "option",
        "menu",
        "popup",
        "control",
    ],
    required_inputs: &["control label", "option", "optional application"],
    possible_outputs: &["selected option"],
    expected_effects: &["local selection mutation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::LocalUiMutation],
    preconditions: &["target selection control is visible"],
    estimated_latency_ms: 450,
    fallback_rank: 10,
};

const UI_TOGGLE: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::UiToggle,
    purpose: "Set a checkbox, switch, or toggle control to a requested state.",
    match_terms: &["toggle", "application", "checkbox", "control", "state"],
    required_inputs: &["control label", "desired state", "optional application"],
    possible_outputs: &["resulting toggle state"],
    expected_effects: &["local toggle state mutation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::LocalUiMutation],
    preconditions: &["target toggle is visible"],
    estimated_latency_ms: 400,
    fallback_rank: 10,
};

const UI_PICK: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::UiPick,
    purpose: "Pick a visible row or item from a named local container.",
    match_terms: &[
        "pick",
        "select",
        "application",
        "row",
        "item",
        "option",
        "container",
    ],
    required_inputs: &["item label", "optional container", "optional application"],
    possible_outputs: &["picked item"],
    expected_effects: &["local item selection mutation"],
    verification_sources: ACTION_AND_CONTEXT,
    reach: WorkOrderScope::LocalUi,
    risk_properties: &[CapabilityRisk::LocalUiMutation],
    preconditions: &["target item is visible and selectable"],
    estimated_latency_ms: 450,
    fallback_rank: 10,
};

const BROWSER_NAVIGATE: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::BrowserNavigate,
    purpose: "Navigate the browser to a requested website or URL.",
    match_terms: &["navigate", "browser", "website", "page", "url"],
    required_inputs: &["URL"],
    possible_outputs: &["resulting page URL", "page title"],
    expected_effects: &["browser page navigation"],
    verification_sources: BROWSER_EVIDENCE,
    reach: WorkOrderScope::Browser,
    risk_properties: &[CapabilityRisk::BrowserMutation],
    preconditions: &["browser worker is ready"],
    estimated_latency_ms: 1_500,
    fallback_rank: 5,
};

const BROWSER_CLICK: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::BrowserClick,
    purpose: "Activate a button, link, or selector on the current browser page.",
    match_terms: &[
        "click", "browser", "page", "selector", "button", "control", "link",
    ],
    required_inputs: &["selector"],
    possible_outputs: &["structured browser result", "resulting page URL"],
    expected_effects: &["browser page control activation"],
    verification_sources: BROWSER_EVIDENCE,
    reach: WorkOrderScope::Browser,
    risk_properties: &[CapabilityRisk::BrowserMutation],
    preconditions: &["browser worker is ready", "target selector exists"],
    estimated_latency_ms: 700,
    fallback_rank: 5,
};

const BROWSER_TYPE: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::BrowserType,
    purpose: "Enter text into a field on the current browser page.",
    match_terms: &["type", "browser", "page", "selector", "field"],
    required_inputs: &["selector", "text"],
    possible_outputs: &["structured browser result"],
    expected_effects: &["browser field content mutation"],
    verification_sources: BROWSER_EVIDENCE,
    reach: WorkOrderScope::Browser,
    risk_properties: &[
        CapabilityRisk::BrowserMutation,
        CapabilityRisk::WritesOperatorData,
    ],
    preconditions: &["browser worker is ready", "target selector exists"],
    estimated_latency_ms: 500,
    fallback_rank: 5,
};

const BROWSER_EXTRACT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::BrowserExtract,
    purpose: "Extract a requested value or text from the current browser page.",
    match_terms: &[
        "extract", "read", "browser", "page", "content", "value", "title",
    ],
    required_inputs: &["optional selector"],
    possible_outputs: &["page text", "page content", "page title", "requested value"],
    expected_effects: &["browser page observation"],
    verification_sources: BROWSER_EVIDENCE,
    reach: WorkOrderScope::Browser,
    risk_properties: &[CapabilityRisk::ReadsOperatorData],
    preconditions: &["browser worker is ready", "resulting page is available"],
    estimated_latency_ms: 400,
    fallback_rank: 5,
};

const BROWSER_SCREENSHOT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::BrowserScreenshot,
    purpose: "Capture an image of the current browser page.",
    match_terms: &["screenshot", "browser", "page", "image"],
    required_inputs: &[],
    possible_outputs: &["browser screenshot"],
    expected_effects: &["browser visual observation"],
    verification_sources: BROWSER_EVIDENCE,
    reach: WorkOrderScope::Browser,
    risk_properties: &[CapabilityRisk::ReadsOperatorData],
    preconditions: &["browser worker is ready"],
    estimated_latency_ms: 500,
    fallback_rank: 5,
};

const SHORTCUT: CapabilityDescriptor = CapabilityDescriptor {
    id: CapabilityId::Shortcut,
    purpose: "Run a named macOS Shortcut installed by the operator.",
    match_terms: &["shortcut", "run", "automation", "workflow"],
    required_inputs: &[
        "shortcut name",
        "optional input path",
        "optional output path",
    ],
    possible_outputs: &["shortcut result", "optional output path"],
    expected_effects: &["operator-installed shortcut execution"],
    verification_sources: ACTION_RECEIPT,
    reach: WorkOrderScope::External,
    risk_properties: &[
        CapabilityRisk::ExecutesModelSuppliedProgram,
        CapabilityRisk::MayRequireApproval,
    ],
    preconditions: &["named shortcut exists", "policy permits execution"],
    estimated_latency_ms: 2_000,
    fallback_rank: 30,
};

const ALL_DESCRIPTORS: [&CapabilityDescriptor; 19] = [
    &SHELL,
    &FILE_READ,
    &FILE_WRITE,
    &APPLE_SCRIPT,
    &MESSAGE_SEND,
    &WINDOW_FOCUS,
    &WINDOW_INSPECT,
    &UI_SNAPSHOT,
    &UI_CLICK,
    &UI_TYPE,
    &UI_SELECT,
    &UI_TOGGLE,
    &UI_PICK,
    &BROWSER_NAVIGATE,
    &BROWSER_CLICK,
    &BROWSER_TYPE,
    &BROWSER_EXTRACT,
    &BROWSER_SCREENSHOT,
    &SHORTCUT,
];

pub(crate) fn all_descriptors() -> &'static [&'static CapabilityDescriptor] {
    &ALL_DESCRIPTORS
}

/// Exhaustive bridge from executable action shape to its Rust-owned contract.
/// Adding an `ActionSpec` or `BrowserActionKind` variant cannot compile until
/// this match declares its descriptor.
pub(crate) fn descriptor_for(spec: &ActionSpec) -> &'static CapabilityDescriptor {
    match spec {
        ActionSpec::Shell { .. } => &SHELL,
        ActionSpec::FileRead { .. } => &FILE_READ,
        ActionSpec::FileWrite { .. } => &FILE_WRITE,
        ActionSpec::AppleScript { .. } => &APPLE_SCRIPT,
        ActionSpec::MessageSend { .. } => &MESSAGE_SEND,
        ActionSpec::WindowFocus { .. } => &WINDOW_FOCUS,
        ActionSpec::WindowInspect { .. } => &WINDOW_INSPECT,
        ActionSpec::UiSnapshot { .. } => &UI_SNAPSHOT,
        ActionSpec::UiClick { .. } => &UI_CLICK,
        ActionSpec::UiType { .. } => &UI_TYPE,
        ActionSpec::UiSelect { .. } => &UI_SELECT,
        ActionSpec::UiToggle { .. } => &UI_TOGGLE,
        ActionSpec::UiPick { .. } => &UI_PICK,
        ActionSpec::Browser { action, .. } => match action {
            BrowserActionKind::Navigate { .. } => &BROWSER_NAVIGATE,
            BrowserActionKind::Click { .. } => &BROWSER_CLICK,
            BrowserActionKind::Type { .. } => &BROWSER_TYPE,
            BrowserActionKind::Extract { .. } => &BROWSER_EXTRACT,
            BrowserActionKind::Screenshot => &BROWSER_SCREENSHOT,
        },
        ActionSpec::Shortcut { .. } => &SHORTCUT,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageOneDisposition {
    /// No implemented capability vocabulary matched an actionable concept.
    ObviousChat,
    /// Only these bounded capabilities may proceed to semantic Stage 2.
    CapabilityCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CapabilityCandidate {
    pub(crate) id: CapabilityId,
    pub(crate) score: u16,
    pub(crate) matched_descriptor_terms: u16,
    pub(crate) matched_local_entities: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StageOneDecision {
    pub(crate) disposition: StageOneDisposition,
    pub(crate) candidates: Vec<CapabilityCandidate>,
    pub(crate) elapsed_micros: u64,
}

/// Stage 1 forwarding measures Stage 2 cost, not final classification
/// correctness. Final entry false positives are recorded separately in
/// [`EntryEvaluationMetrics`] after B2 can make a terminal entry decision.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct StageOneForwardingMetrics {
    pub(crate) total_turns: u64,
    pub(crate) forwarded_to_stage_2: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryStage {
    Stage1,
    Stage2,
    Stage3,
}

/// Raw termination counts are retained alongside any derived rate so the
/// Slice B checkpoint cannot hide an expensive Stage 3 middle band inside an
/// aggregate latency percentile.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct EntryStageDistribution {
    pub(crate) stage_1: u64,
    pub(crate) stage_2: u64,
    pub(crate) stage_3: u64,
}

impl EntryStageDistribution {
    pub(crate) fn record(&mut self, stage: EntryStage) {
        match stage {
            EntryStage::Stage1 => self.stage_1 = self.stage_1.saturating_add(1),
            EntryStage::Stage2 => self.stage_2 = self.stage_2.saturating_add(1),
            EntryStage::Stage3 => self.stage_3 = self.stage_3.saturating_add(1),
        }
    }

    pub(crate) fn total(&self) -> u64 {
        self.stage_1
            .saturating_add(self.stage_2)
            .saturating_add(self.stage_3)
    }
}

/// Named evaluation counters required by the Slice B stop/go report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct EntryEvaluationMetrics {
    pub(crate) stages: EntryStageDistribution,
    pub(crate) actionable_turns: u64,
    pub(crate) entry_false_negatives: u64,
    pub(crate) non_actionable_turns: u64,
    pub(crate) entry_false_positives: u64,
}

impl EntryEvaluationMetrics {
    pub(crate) fn entry_false_negative_rate(&self) -> f64 {
        rate(self.entry_false_negatives, self.actionable_turns)
    }

    pub(crate) fn entry_false_positive_rate(&self) -> f64 {
        rate(self.entry_false_positives, self.non_actionable_turns)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StageOneMatcher;

// These are explicit tuning knobs, pinned by the clean-chat, adversarial-chat,
// cross-capability, compound-request, and founding-paraphrase corpora below.
const ACTION_TERM_WEIGHT: u16 = 5;
const DESCRIPTOR_TERM_WEIGHT: u16 = 2;
const PRIMARY_MATCH_THRESHOLD: u16 = 5;
const COMPOUND_MATCH_THRESHOLD: u16 = 4;
const COMPOUND_MIN_MATCHED_TERMS: u16 = 2;
const LOCAL_ENTITY_BONUS_CAP: u16 = 2;

impl StageOneMatcher {
    pub(crate) fn evaluate(&self, text: &str, local_entities: &[&str]) -> StageOneDecision {
        let started_at = Instant::now();
        let request_tokens = normalized_tokens(text);
        let action_tokens = request_tokens
            .iter()
            .filter(|token| is_action_concept(token))
            .cloned()
            .collect::<HashSet<_>>();
        let entity_tokens = local_entities
            .iter()
            .flat_map(|entity| normalized_tokens(entity))
            .collect::<HashSet<_>>();
        let matched_local_entities = request_tokens.intersection(&entity_tokens).count() as u16;
        let has_local_scope = matched_local_entities > 0
            || request_tokens.contains("app")
            || request_tokens.contains("application")
            || request_tokens.contains("window");
        let has_browser_scope = request_tokens.contains("browser")
            || request_tokens.contains("web")
            || request_tokens.contains("website")
            || request_tokens.contains("page")
            || request_tokens.contains("url")
            || text.contains("://");

        let mut candidates = if action_tokens.is_empty() {
            Vec::new()
        } else {
            all_descriptors()
                .iter()
                .filter_map(|descriptor| {
                    score_descriptor(
                        descriptor,
                        &request_tokens,
                        &action_tokens,
                        matched_local_entities,
                        has_local_scope,
                        has_browser_scope,
                    )
                })
                .collect::<Vec<_>>()
        };
        candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        candidates.truncate(5);

        StageOneDecision {
            disposition: if candidates.is_empty() {
                StageOneDisposition::ObviousChat
            } else {
                StageOneDisposition::CapabilityCandidate
            },
            candidates,
            elapsed_micros: u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
        }
    }
}

impl CapabilityId {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::FileRead => "file_read",
            Self::FileWrite => "file_write",
            Self::AppleScript => "apple_script",
            Self::MessageSend => "message_send",
            Self::WindowFocus => "window_focus",
            Self::WindowInspect => "window_inspect",
            Self::UiSnapshot => "ui_snapshot",
            Self::UiClick => "ui_click",
            Self::UiType => "ui_type",
            Self::UiSelect => "ui_select",
            Self::UiToggle => "ui_toggle",
            Self::UiPick => "ui_pick",
            Self::BrowserNavigate => "browser_navigate",
            Self::BrowserClick => "browser_click",
            Self::BrowserType => "browser_type",
            Self::BrowserExtract => "browser_extract",
            Self::BrowserScreenshot => "browser_screenshot",
            Self::Shortcut => "shortcut",
        }
    }
}

fn score_descriptor(
    descriptor: &CapabilityDescriptor,
    request_tokens: &HashSet<String>,
    request_action_tokens: &HashSet<String>,
    matched_local_entities: u16,
    has_local_scope: bool,
    has_browser_scope: bool,
) -> Option<CapabilityCandidate> {
    // Local application evidence must not silently widen into browser work.
    // The reverse is intentionally not suppressed: a browser request may also
    // require local-window capabilities, and Stage 2 will resolve that compound.
    if has_local_scope && !has_browser_scope && descriptor.reach == WorkOrderScope::Browser {
        return None;
    }
    let matched_terms = descriptor
        .match_terms
        .iter()
        .filter(|term| request_tokens.contains(**term))
        .count() as u16;
    let matched_actions = descriptor
        .match_terms
        .iter()
        .filter(|term| request_action_tokens.contains(**term))
        .count() as u16;
    let scope_entity_bonus = if descriptor.reach == WorkOrderScope::LocalUi {
        matched_local_entities.min(LOCAL_ENTITY_BONUS_CAP)
    } else {
        0
    };
    let score = matched_actions
        .saturating_mul(ACTION_TERM_WEIGHT)
        .saturating_add(
            matched_terms
                .saturating_sub(matched_actions)
                .saturating_mul(DESCRIPTOR_TERM_WEIGHT),
        )
        .saturating_add(scope_entity_bonus);

    // A primary match owns an action concept. A secondary compound match may
    // share the turn's action verb but must match at least two descriptor terms.
    let is_primary = matched_actions > 0 && score >= PRIMARY_MATCH_THRESHOLD;
    let is_compound_secondary = !request_action_tokens.is_empty()
        && matched_terms >= COMPOUND_MIN_MATCHED_TERMS
        && score >= COMPOUND_MATCH_THRESHOLD;
    (is_primary || is_compound_secondary).then_some(CapabilityCandidate {
        id: descriptor.id,
        score,
        matched_descriptor_terms: matched_terms,
        matched_local_entities,
    })
}

fn rate(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn normalized_tokens(text: &str) -> HashSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter_map(normalize_token)
        .collect()
}

fn normalize_token(raw: &str) -> Option<String> {
    let lower = raw.to_lowercase();
    let token = lower.trim();
    if token.is_empty() || is_stopword(token) {
        return None;
    }
    let inflection_folded = fold_inflection(token);
    Some(semantic_alias(inflection_folded).to_string())
}

/// Linguistically safe folding of inflected forms to their base token.
fn fold_inflection(token: &str) -> &str {
    match token {
        "focused" | "focusing" => "focus",
        "opened" | "opening" => "open",
        "visited" => "visit",
        "loaded" => "load",
        "browsed" => "browse",
        "pressed" => "press",
        "tapped" => "tap",
        "activated" => "activate",
        "typed" | "typing" => "type",
        "entered" => "enter",
        "filled" => "fill",
        "chosen" => "choose",
        "selected" | "selection" => "select",
        "picked" => "pick",
        "enabled" => "enable",
        "disabled" => "disable",
        "checked" => "check",
        "unchecked" => "uncheck",
        "inspected" => "inspect",
        "observed" => "observe",
        "listed" => "list",
        "viewed" => "view",
        "saved" => "save",
        "created" => "create",
        "edited" => "edit",
        "updated" => "update",
        "ran" => "run",
        "executed" => "execute",
        "launched" => "launch",
        "started" => "start",
        "sent" => "send",
        "messaged" => "message",
        "texted" => "text",
        "captured" => "capture",
        "extracted" => "extract",
        "reported" => "report",
        "removed" => "remove",
        "deleted" => "delete",
        value => value,
    }
}

/// Recall-biased semantic aliases for Stage 1. These mappings are
/// context-dependent by design: B1 prefers forwarding an ambiguous turn to
/// Stage 2 over losing an actionable imperative as chat.
fn semantic_alias(token: &str) -> &str {
    match token {
        "front" | "frontmost" | "forward" | "foreground" | "active" | "switch" | "bring"
        | "put" => "focus",
        "open" | "visit" | "load" | "browse" | "go" => "navigate",
        "press" | "tap" | "activate" => "click",
        "enter" | "fill" | "input" => "type",
        "choose" => "select",
        "enable" | "disable" | "check" | "uncheck" | "turn" => "toggle",
        "observe" | "show" | "list" => "inspect",
        "view" => "read",
        "save" | "create" | "edit" | "update" => "write",
        "execute" | "launch" | "start" | "try" => "run",
        "message" | "text" => "send",
        "capture" => "screenshot",
        "report" => "extract",
        "remove" => "delete",
        value => value,
    }
}

fn is_action_concept(token: &str) -> bool {
    matches!(
        token,
        "run"
            | "read"
            | "write"
            | "delete"
            | "send"
            | "focus"
            | "inspect"
            | "snapshot"
            | "click"
            | "type"
            | "select"
            | "toggle"
            | "pick"
            | "navigate"
            | "extract"
            | "screenshot"
            | "shortcut"
    )
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "are"
            | "at"
            | "be"
            | "can"
            | "could"
            | "for"
            | "from"
            | "i"
            | "in"
            | "is"
            | "it"
            | "me"
            | "my"
            | "of"
            | "on"
            | "please"
            | "the"
            | "this"
            | "to"
            | "what"
            | "would"
            | "you"
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf};

    use super::*;

    fn ids(decision: &StageOneDecision) -> HashSet<CapabilityId> {
        decision
            .candidates
            .iter()
            .map(|candidate| candidate.id)
            .collect()
    }

    fn assert_candidate(
        matcher: &StageOneMatcher,
        text: &str,
        entities: &[&str],
        expected: CapabilityId,
    ) {
        let decision = matcher.evaluate(text, entities);
        assert_eq!(
            decision.disposition,
            StageOneDisposition::CapabilityCandidate,
            "actionable turn exited as chat: {text}"
        );
        assert!(
            ids(&decision).contains(&expected),
            "expected {expected:?} for {text:?}, got {:?}",
            decision.candidates
        );
    }

    #[test]
    fn descriptor_registry_is_complete_unique_and_target_agnostic() {
        assert_eq!(all_descriptors().len(), 19);
        let unique = all_descriptors()
            .iter()
            .map(|descriptor| descriptor.id)
            .collect::<HashSet<_>>();
        assert_eq!(unique.len(), all_descriptors().len());

        for descriptor in all_descriptors() {
            assert!(!descriptor.purpose.trim().is_empty());
            assert!(!descriptor.match_terms.is_empty());
            assert!(descriptor
                .required_inputs
                .iter()
                .all(|input| !input.trim().is_empty()));
            assert!(!descriptor.possible_outputs.is_empty());
            assert!(!descriptor.expected_effects.is_empty());
            assert!(!descriptor.verification_sources.is_empty());
            assert!(!descriptor.risk_properties.is_empty());
            assert!(!descriptor.preconditions.is_empty());
            assert!(descriptor.estimated_latency_ms > 0);
            assert!(descriptor.fallback_rank > 0);
            let serialized = format!("{descriptor:?}").to_lowercase();
            for forbidden_target in ["finder", "safari", "xcode", "preview", "apple.com"] {
                assert!(
                    !serialized.contains(forbidden_target),
                    "descriptor {:?} hard-coded target {forbidden_target}",
                    descriptor.id
                );
            }
        }
    }

    #[test]
    fn golden_stage_one_lexicon_is_explicit_canonical_unique_and_actionable() {
        let expected: &[(CapabilityId, &[&str])] = &[
            (
                CapabilityId::Shell,
                &["run", "inspect", "delete", "command", "process", "system"],
            ),
            (CapabilityId::FileRead, &["read", "file", "content", "path"]),
            (
                CapabilityId::FileWrite,
                &["write", "delete", "file", "content", "path"],
            ),
            (
                CapabilityId::AppleScript,
                &["run", "applescript", "automation", "application", "system"],
            ),
            (
                CapabilityId::MessageSend,
                &["send", "recipient", "contact", "body"],
            ),
            (
                CapabilityId::WindowFocus,
                &["focus", "application", "window"],
            ),
            (
                CapabilityId::WindowInspect,
                &["inspect", "application", "window", "title"],
            ),
            (
                CapabilityId::UiSnapshot,
                &[
                    "snapshot",
                    "inspect",
                    "application",
                    "control",
                    "button",
                    "field",
                    "menu",
                    "row",
                ],
            ),
            (
                CapabilityId::UiClick,
                &["click", "application", "button", "control", "link"],
            ),
            (
                CapabilityId::UiType,
                &["type", "application", "field", "control"],
            ),
            (
                CapabilityId::UiSelect,
                &[
                    "select",
                    "application",
                    "option",
                    "menu",
                    "popup",
                    "control",
                ],
            ),
            (
                CapabilityId::UiToggle,
                &["toggle", "application", "checkbox", "control", "state"],
            ),
            (
                CapabilityId::UiPick,
                &[
                    "pick",
                    "select",
                    "application",
                    "row",
                    "item",
                    "option",
                    "container",
                ],
            ),
            (
                CapabilityId::BrowserNavigate,
                &["navigate", "browser", "website", "page", "url"],
            ),
            (
                CapabilityId::BrowserClick,
                &[
                    "click", "browser", "page", "selector", "button", "control", "link",
                ],
            ),
            (
                CapabilityId::BrowserType,
                &["type", "browser", "page", "selector", "field"],
            ),
            (
                CapabilityId::BrowserExtract,
                &[
                    "extract", "read", "browser", "page", "content", "value", "title",
                ],
            ),
            (
                CapabilityId::BrowserScreenshot,
                &["screenshot", "browser", "page", "image"],
            ),
            (
                CapabilityId::Shortcut,
                &["shortcut", "run", "automation", "workflow"],
            ),
        ];

        assert_eq!(expected.len(), all_descriptors().len());
        for (descriptor, (expected_id, expected_terms)) in
            all_descriptors().iter().zip(expected.iter())
        {
            assert_eq!(descriptor.id, *expected_id);
            assert_eq!(descriptor.match_terms, *expected_terms);
            let mut unique_terms = HashSet::new();
            for term in descriptor.match_terms {
                assert_eq!(fold_inflection(term), *term);
                assert_eq!(semantic_alias(term), *term);
                assert_eq!(normalize_token(term).as_deref(), Some(*term));
                assert!(unique_terms.insert(*term), "duplicate term {term:?}");
            }
            assert!(
                descriptor
                    .match_terms
                    .iter()
                    .any(|term| is_action_concept(term)),
                "descriptor {:?} can never win a primary match",
                descriptor.id
            );
        }
    }

    #[test]
    fn descriptor_for_is_exhaustive_across_every_executable_shape() {
        let fixtures = [
            (
                ActionSpec::Shell {
                    args: vec!["true".into()],
                    working_dir: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::Shell,
            ),
            (
                ActionSpec::FileRead {
                    path: PathBuf::from("/tmp/a"),
                },
                CapabilityId::FileRead,
            ),
            (
                ActionSpec::FileWrite {
                    path: PathBuf::from("/tmp/a"),
                    content: "x".into(),
                    create_dirs: false,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::FileWrite,
            ),
            (
                ActionSpec::AppleScript {
                    script: "return 1".into(),
                    rationale: None,
                },
                CapabilityId::AppleScript,
            ),
            (
                ActionSpec::MessageSend {
                    recipient: "operator".into(),
                    body: "hello".into(),
                    rationale: None,
                },
                CapabilityId::MessageSend,
            ),
            (
                ActionSpec::WindowFocus {
                    app_name: "fixture-app".into(),
                    title_contains: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::WindowFocus,
            ),
            (
                ActionSpec::WindowInspect {
                    app_name: None,
                    rationale: None,
                },
                CapabilityId::WindowInspect,
            ),
            (
                ActionSpec::UiSnapshot {
                    app_name: None,
                    max_depth: None,
                    rationale: None,
                },
                CapabilityId::UiSnapshot,
            ),
            (
                ActionSpec::UiClick {
                    app_name: None,
                    role: None,
                    label: "fixture".into(),
                    max_depth: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::UiClick,
            ),
            (
                ActionSpec::UiType {
                    app_name: None,
                    role: None,
                    label: None,
                    text: "fixture".into(),
                    max_depth: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::UiType,
            ),
            (
                ActionSpec::UiSelect {
                    app_name: None,
                    role: None,
                    label: "fixture".into(),
                    option: "value".into(),
                    max_depth: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::UiSelect,
            ),
            (
                ActionSpec::UiToggle {
                    app_name: None,
                    role: None,
                    label: "fixture".into(),
                    state: true,
                    max_depth: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::UiToggle,
            ),
            (
                ActionSpec::UiPick {
                    app_name: None,
                    role: None,
                    label: "fixture".into(),
                    container_label: None,
                    max_depth: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::UiPick,
            ),
            (
                ActionSpec::Browser {
                    action: BrowserActionKind::Navigate {
                        url: "https://example.invalid".into(),
                    },
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::BrowserNavigate,
            ),
            (
                ActionSpec::Browser {
                    action: BrowserActionKind::Click {
                        selector: "body".into(),
                    },
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::BrowserClick,
            ),
            (
                ActionSpec::Browser {
                    action: BrowserActionKind::Type {
                        selector: "input".into(),
                        text: "fixture".into(),
                    },
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::BrowserType,
            ),
            (
                ActionSpec::Browser {
                    action: BrowserActionKind::Extract { selector: None },
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::BrowserExtract,
            ),
            (
                ActionSpec::Browser {
                    action: BrowserActionKind::Screenshot,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::BrowserScreenshot,
            ),
            (
                ActionSpec::Shortcut {
                    name: "fixture".into(),
                    input_path: None,
                    output_path: None,
                    rationale: None,
                    category_override: None,
                },
                CapabilityId::Shortcut,
            ),
        ];
        for (spec, expected) in fixtures {
            assert_eq!(descriptor_for(&spec).id, expected);
        }
    }

    #[test]
    fn stage_one_generalizes_targets_across_capability_families() {
        let matcher = StageOneMatcher;
        let entities = ["Finder", "Safari", "Xcode", "Preview", "Notes"];
        for (text, expected) in [
            ("Focus Finder.", CapabilityId::WindowFocus),
            ("Bring Safari forward.", CapabilityId::WindowFocus),
            ("Switch to Xcode.", CapabilityId::WindowFocus),
            (
                "Make Preview the active application.",
                CapabilityId::WindowFocus,
            ),
            (
                "Open apple.com in the browser.",
                CapabilityId::BrowserNavigate,
            ),
            ("Go to Wikipedia.", CapabilityId::BrowserNavigate),
            ("Load example.com.", CapabilityId::BrowserNavigate),
            ("Click the Export button in Preview.", CapabilityId::UiClick),
            ("Press the Build control in Xcode.", CapabilityId::UiClick),
            (
                "Type hello into the search field in Notes.",
                CapabilityId::UiType,
            ),
            ("Choose PDF from the Format menu.", CapabilityId::UiSelect),
            ("Turn the Sync checkbox on.", CapabilityId::UiToggle),
            ("Read /tmp/report.txt.", CapabilityId::FileRead),
            (
                "Write these notes to /tmp/notes.txt.",
                CapabilityId::FileWrite,
            ),
            ("Run ps and show the process list.", CapabilityId::Shell),
            ("Send Alex the status update.", CapabilityId::MessageSend),
            ("Run my Archive Project shortcut.", CapabilityId::Shortcut),
            (
                "Take a screenshot of the browser page.",
                CapabilityId::BrowserScreenshot,
            ),
        ] {
            assert_candidate(&matcher, text, &entities, expected);
        }
    }

    #[test]
    fn founding_compound_browser_request_keeps_effect_and_requested_output_candidates() {
        let decision = StageOneMatcher.evaluate(
            "Open apple.com in the browser and tell me the page title.",
            &[],
        );
        let candidates = ids(&decision);
        assert!(candidates.contains(&CapabilityId::BrowserNavigate));
        assert!(candidates.contains(&CapabilityId::BrowserExtract));
    }

    #[test]
    fn founding_paraphrase_corpus_has_zero_stage_one_false_negatives() {
        let matcher = StageOneMatcher;
        let entities = ["Finder", "Safari", "Xcode"];
        let corpus = [
            (
                "Open apple.com in the browser and tell me the page title.",
                CapabilityId::BrowserNavigate,
            ),
            (
                "Go to Wikipedia and read me the title.",
                CapabilityId::BrowserNavigate,
            ),
            (
                "Load example.com; what is the title of the page?",
                CapabilityId::BrowserNavigate,
            ),
            (
                "Visit apple.com and report the title shown in the browser.",
                CapabilityId::BrowserNavigate,
            ),
            (
                "Navigate to example.com, then tell me what the page is called.",
                CapabilityId::BrowserNavigate,
            ),
            ("Focus Finder.", CapabilityId::WindowFocus),
            ("Bring Finder forward.", CapabilityId::WindowFocus),
            ("Put Safari in front.", CapabilityId::WindowFocus),
            (
                "Make the Finder window frontmost.",
                CapabilityId::WindowFocus,
            ),
            (
                "Switch the active application to Safari.",
                CapabilityId::WindowFocus,
            ),
            (
                "Click the Missing Control button in Finder.",
                CapabilityId::UiClick,
            ),
            (
                "Press a button called Does Not Exist in Xcode.",
                CapabilityId::UiClick,
            ),
            (
                "In Finder, click the button labeled Dexter Manual Missing Control.",
                CapabilityId::UiClick,
            ),
            (
                "Try the control named No Such Button in the frontmost Xcode window.",
                CapabilityId::UiClick,
            ),
            ("Press Finder's Not Present button.", CapabilityId::UiClick),
        ];
        let false_negatives = corpus
            .iter()
            .filter(|(text, expected)| {
                let decision = matcher.evaluate(text, &entities);
                decision.disposition != StageOneDisposition::CapabilityCandidate
                    || !ids(&decision).contains(expected)
            })
            .count();

        assert_eq!(
            false_negatives,
            0,
            "entry false-negative rate must be 0/{total} on the founding corpus",
            total = corpus.len()
        );
    }

    #[test]
    fn missing_local_control_stays_in_local_capability_space() {
        let decision = StageOneMatcher.evaluate(
            "Click a button named Dexter Manual Missing Control in the frontmost app.",
            &["Finder", "Preview"],
        );
        let candidates = ids(&decision);
        assert!(candidates.contains(&CapabilityId::UiClick));
        for browser_capability in [
            CapabilityId::BrowserNavigate,
            CapabilityId::BrowserClick,
            CapabilityId::BrowserType,
            CapabilityId::BrowserExtract,
            CapabilityId::BrowserScreenshot,
        ] {
            assert!(!candidates.contains(&browser_capability));
        }
    }

    #[test]
    fn obvious_chat_exits_stage_one_without_capability_candidates() {
        let matcher = StageOneMatcher;
        let corpus = [
            "Good morning.",
            "Tell me a joke.",
            "Why is the sky blue?",
            "What is a closure in Rust?",
            "Help me brainstorm names for a dog.",
            "That was an interesting answer.",
            "How are you today?",
            "Explain quantum entanglement simply.",
            "What do you think about this idea?",
            "Thanks, that makes sense.",
            "Who wrote The Left Hand of Darkness?",
            "When was the first moon landing?",
            "Summarize the concept we discussed.",
            "I am not sure yet.",
            "Let's think about the tradeoffs.",
            "What color is closest to teal?",
            "Could that approach be simpler?",
            "Give me an analogy.",
            "What's two plus two?",
            "That joke was funny.",
        ];
        let stage_one_chat = corpus
            .iter()
            .filter(|text| {
                matcher.evaluate(text, &[]).disposition == StageOneDisposition::ObviousChat
            })
            .count();
        assert_eq!(stage_one_chat, corpus.len());
    }

    #[test]
    fn adversarial_chat_reports_raw_stage_two_forwarding_without_calling_it_false_positive() {
        let matcher = StageOneMatcher;
        let corpus = [
            "Let's go over the tradeoffs.",
            "What type of dog is that?",
            "That text you wrote was helpful.",
            "My run this morning was great.",
            "Put that idea aside for now.",
            "Can we focus on the larger question?",
            "I selected this because it looked better.",
            "The page in that book was confusing.",
            "I deleted that paragraph from my draft.",
            "The open question remains difficult.",
            "She pressed the issue during the meeting.",
        ];
        let forwarded_to_stage_2 = corpus
            .iter()
            .filter(|text| {
                matcher.evaluate(text, &[]).disposition == StageOneDisposition::CapabilityCandidate
            })
            .count();
        let report = StageOneForwardingMetrics {
            total_turns: corpus.len() as u64,
            forwarded_to_stage_2: forwarded_to_stage_2 as u64,
        };

        // This corpus documents the recall-biased Stage 1 cost surface. Add a
        // maximum forwarding budget after the corpus is large enough for a
        // percentage to be meaningful; never add a minimum-error invariant.
        eprintln!(
            "adversarial_stage_one_forwarding={}/{}",
            report.forwarded_to_stage_2, report.total_turns
        );
        assert_eq!(report.total_turns, corpus.len() as u64);
        assert!(report.forwarded_to_stage_2 <= report.total_turns);
    }

    #[test]
    fn destructive_intent_survives_normalization_and_reaches_claiming_capabilities() {
        for raw in ["delete", "deleted", "remove", "removed"] {
            assert_eq!(normalize_token(raw).as_deref(), Some("delete"));
        }
        let candidates = ids(&StageOneMatcher.evaluate("Delete the build folder.", &[]));
        assert!(candidates.contains(&CapabilityId::Shell));
        assert!(candidates.contains(&CapabilityId::FileWrite));
    }

    #[test]
    fn candidate_telemetry_keeps_the_real_entity_match_count() {
        let decision =
            StageOneMatcher.evaluate("Focus Finder and Safari.", &["Finder", "Safari", "Preview"]);
        let candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.id == CapabilityId::WindowFocus)
            .unwrap();
        assert_eq!(candidate.matched_local_entities, 2);
    }

    #[test]
    fn stage_one_report_names_raw_counts_and_false_negatives() {
        let matcher = StageOneMatcher;
        let actionable = [
            "Focus Finder",
            "Bring Safari forward",
            "Open example.com",
            "Press the Save button",
            "Write this to /tmp/a",
        ];
        let obvious_chat = ["Good morning", "Tell me a story", "Why is water wet?"];
        let actionable_forwarded = actionable
            .iter()
            .filter(|text| {
                matcher.evaluate(text, &["Finder", "Safari"]).disposition
                    == StageOneDisposition::CapabilityCandidate
            })
            .count();
        let stage_one_terminated = obvious_chat
            .iter()
            .filter(|text| {
                matcher.evaluate(text, &[]).disposition == StageOneDisposition::ObviousChat
            })
            .count();
        let entry_false_negatives = actionable.len() - actionable_forwarded;
        let mut metrics = EntryEvaluationMetrics {
            actionable_turns: actionable.len() as u64,
            entry_false_negatives: entry_false_negatives as u64,
            non_actionable_turns: obvious_chat.len() as u64,
            // Stage 1 forwarding is not a final false positive. B2+ owns this
            // correctness metric once it can make a terminal entry decision.
            entry_false_positives: 0,
            ..Default::default()
        };
        let forwarding = StageOneForwardingMetrics {
            total_turns: actionable.len() as u64,
            forwarded_to_stage_2: actionable_forwarded as u64,
        };
        for _ in 0..stage_one_terminated {
            metrics.stages.record(EntryStage::Stage1);
        }

        assert_eq!(actionable_forwarded, actionable.len());
        assert_eq!(stage_one_terminated, obvious_chat.len());
        assert_eq!(entry_false_negatives, 0);
        assert_eq!(metrics.stages.stage_1, 3);
        assert_eq!(metrics.stages.stage_2, 0);
        assert_eq!(metrics.stages.stage_3, 0);
        assert_eq!(metrics.stages.total(), 3);
        assert_eq!(forwarding.total_turns, 5);
        assert_eq!(forwarding.forwarded_to_stage_2, 5);
        assert_eq!(metrics.entry_false_negative_rate(), 0.0);
        assert_eq!(metrics.entry_false_positive_rate(), 0.0);
    }

    #[test]
    fn stage_distribution_retains_each_terminal_stage_independently() {
        let mut distribution = EntryStageDistribution::default();
        distribution.record(EntryStage::Stage1);
        distribution.record(EntryStage::Stage2);
        distribution.record(EntryStage::Stage3);

        assert_eq!(distribution.stage_1, 1);
        assert_eq!(distribution.stage_2, 1);
        assert_eq!(distribution.stage_3, 1);
        assert_eq!(distribution.total(), 3);
    }
}
