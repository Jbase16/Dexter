use serde::{Deserialize, Serialize};

use crate::inference::engine::{Message, MessageOrigin};

use super::representation::{estimate_tokens, fingerprint};

const PROMPT_MANIFEST_VERSION: &str = "prompt_manifest_v1";
const PREVIEW_MAX_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSourceKind {
    SystemBase,
    Personality,
    ComedyMode,
    OperatorContext,
    CompiledAmbientContext,
    ConversationHistory,
    RetrievalMemory,
    ActionResult,
    UserTurn,
    ToolSyntheticTurn,
    VisionAttachmentMarker,
    UncertaintyFollowup,
    WallClock,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessageDiagnostic {
    pub index: usize,
    pub role: String,
    pub origin: String,
    pub source_kind: PromptSourceKind,
    pub chars: usize,
    pub estimated_tokens: usize,
    pub image_count: usize,
    pub fingerprint: String,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptAssemblyDiagnostics {
    pub manifest_version: String,
    pub route_category: String,
    pub model: String,
    pub prompt_profile: String,
    pub num_ctx_override: Option<u32>,
    pub message_count: usize,
    pub total_chars: usize,
    pub total_estimated_tokens: usize,
    pub messages: Vec<PromptMessageDiagnostic>,
}

impl PromptAssemblyDiagnostics {
    pub fn from_messages(
        route_category: impl Into<String>,
        model: impl Into<String>,
        prompt_profile: impl Into<String>,
        num_ctx_override: Option<u32>,
        messages: &[Message],
    ) -> Self {
        let diagnostics = messages
            .iter()
            .enumerate()
            .map(|(index, message)| diagnostic_for_message(index, message))
            .collect::<Vec<_>>();
        let total_chars = diagnostics.iter().map(|m| m.chars).sum();
        let total_estimated_tokens = diagnostics.iter().map(|m| m.estimated_tokens).sum();

        Self {
            manifest_version: PROMPT_MANIFEST_VERSION.to_string(),
            route_category: route_category.into(),
            model: model.into(),
            prompt_profile: prompt_profile.into(),
            num_ctx_override,
            message_count: diagnostics.len(),
            total_chars,
            total_estimated_tokens,
            messages: diagnostics,
        }
    }

    pub fn system_operator_estimated_tokens(&self) -> usize {
        self.messages
            .iter()
            .filter(|message| {
                matches!(
                    message.source_kind,
                    PromptSourceKind::SystemBase
                        | PromptSourceKind::Personality
                        | PromptSourceKind::OperatorContext
                )
            })
            .map(|message| message.estimated_tokens)
            .sum()
    }
}

fn diagnostic_for_message(index: usize, message: &Message) -> PromptMessageDiagnostic {
    let source_kind = classify_message(index, message);
    let image_count = message.images.as_ref().map(Vec::len).unwrap_or(0);
    let chars = message.role.chars().count() + message.content.chars().count() + 4;
    let estimated_tokens = estimate_tokens(&message.content);

    PromptMessageDiagnostic {
        index,
        role: message.role.clone(),
        origin: origin_label(message.origin).to_string(),
        source_kind,
        chars,
        estimated_tokens,
        image_count,
        fingerprint: fingerprint(&message.content),
        preview: preview(&message.content),
    }
}

fn classify_message(index: usize, message: &Message) -> PromptSourceKind {
    if message
        .images
        .as_ref()
        .map(|images| !images.is_empty())
        .unwrap_or(false)
    {
        return PromptSourceKind::VisionAttachmentMarker;
    }

    match message.origin {
        MessageOrigin::ToolResult => return classify_tool_result(&message.content),
        MessageOrigin::Retrieval => return PromptSourceKind::RetrievalMemory,
        MessageOrigin::Assistant => return PromptSourceKind::ConversationHistory,
        MessageOrigin::User if message.role == "user" => {
            return classify_user_message(&message.content)
        }
        MessageOrigin::User => return PromptSourceKind::Unknown,
        MessageOrigin::System => {}
    }

    if message.role == "system" {
        classify_system_message(index, &message.content)
    } else {
        PromptSourceKind::Unknown
    }
}

fn classify_system_message(index: usize, content: &str) -> PromptSourceKind {
    let trimmed = content.trim_start();
    let lower = trimmed.to_lowercase();

    if trimmed.starts_with("The current time is ") {
        PromptSourceKind::WallClock
    } else if trimmed.starts_with("COMEDY MODE")
        || lower.contains("comedy mode")
        || lower.contains("humor request")
    {
        PromptSourceKind::ComedyMode
    } else if trimmed.starts_with("[Env")
        || trimmed.starts_with("copied_text.")
        || trimmed.starts_with("focused_app.")
    {
        PromptSourceKind::CompiledAmbientContext
    } else if trimmed.starts_with("[Retrieved")
        || trimmed.starts_with("Reference notes from prior sessions")
        || trimmed.starts_with("Earlier in this conversation:")
    {
        PromptSourceKind::RetrievalMemory
    } else if lower.contains("operator") || lower.contains("jason") || lower.contains("local mac") {
        PromptSourceKind::OperatorContext
    } else if index == 0 {
        PromptSourceKind::Personality
    } else {
        PromptSourceKind::SystemBase
    }
}

fn classify_user_message(content: &str) -> PromptSourceKind {
    if content.starts_with("[Retrieved") {
        PromptSourceKind::RetrievalMemory
    } else if content.starts_with("[Action result") || content.starts_with("[Action FAILED") {
        PromptSourceKind::ActionResult
    } else if content.starts_with("[Env") {
        PromptSourceKind::CompiledAmbientContext
    } else {
        PromptSourceKind::UserTurn
    }
}

fn classify_tool_result(content: &str) -> PromptSourceKind {
    if content.starts_with("[Retrieved") {
        PromptSourceKind::RetrievalMemory
    } else if content.starts_with("[Action result") || content.starts_with("[Action FAILED") {
        PromptSourceKind::ActionResult
    } else {
        PromptSourceKind::ToolSyntheticTurn
    }
}

fn origin_label(origin: MessageOrigin) -> &'static str {
    match origin {
        MessageOrigin::System => "system",
        MessageOrigin::User => "user",
        MessageOrigin::Assistant => "assistant",
        MessageOrigin::ToolResult => "tool_result",
        MessageOrigin::Retrieval => "retrieval",
    }
}

fn preview(content: &str) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut value = compact.chars().take(PREVIEW_MAX_CHARS).collect::<String>();
    if compact.chars().count() > PREVIEW_MAX_CHARS {
        value.push_str("...");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_sums_message_costs() {
        let messages = vec![
            Message::system("You are Dexter."),
            Message::user("explain this code"),
            Message::assistant("Sure."),
        ];
        let manifest = PromptAssemblyDiagnostics::from_messages(
            "Chat",
            "gemma4:26b-mlx",
            "primary_slim",
            Some(8192),
            &messages,
        );

        assert_eq!(manifest.manifest_version, PROMPT_MANIFEST_VERSION);
        assert_eq!(manifest.prompt_profile, "primary_slim");
        assert_eq!(manifest.message_count, 3);
        assert!(manifest.total_chars > 0);
        assert!(manifest.total_estimated_tokens > 0);
        assert_eq!(
            manifest.messages[0].source_kind,
            PromptSourceKind::Personality
        );
        assert_eq!(manifest.messages[1].source_kind, PromptSourceKind::UserTurn);
        assert_eq!(
            manifest.messages[2].source_kind,
            PromptSourceKind::ConversationHistory
        );
    }

    #[test]
    fn manifest_classifies_synthetic_sources() {
        let messages = vec![
            Message::tool_result("[Action result] echo hi -> hi"),
            Message::retrieval("[Retrieved: docs]\nSome context"),
            Message::user("[Env · shell: exit 1]\n\nwhy did that fail?"),
        ];
        let manifest = PromptAssemblyDiagnostics::from_messages(
            "Chat",
            "qwen3:8b",
            "fast_minimal",
            None,
            &messages,
        );

        assert_eq!(
            manifest.messages[0].source_kind,
            PromptSourceKind::ActionResult
        );
        assert_eq!(
            manifest.messages[1].source_kind,
            PromptSourceKind::RetrievalMemory
        );
        assert_eq!(
            manifest.messages[2].source_kind,
            PromptSourceKind::CompiledAmbientContext
        );
    }

    #[test]
    fn manifest_preview_is_bounded_and_fingerprinted() {
        let secret_tail = "secret-token-that-should-not-appear-after-the-preview-window";
        let content = format!("{}{}", "x".repeat(240), secret_tail);
        let messages = vec![Message::user(content.clone())];
        let manifest = PromptAssemblyDiagnostics::from_messages(
            "Chat",
            "qwen3:8b",
            "fast_minimal",
            None,
            &messages,
        );
        let diagnostic = &manifest.messages[0];

        assert!(diagnostic.preview.chars().count() <= PREVIEW_MAX_CHARS + 3);
        assert!(!diagnostic.preview.contains(secret_tail));
        assert_eq!(diagnostic.fingerprint, fingerprint(&content));
    }
}
