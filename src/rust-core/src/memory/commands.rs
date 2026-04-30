/// A memory management command detected in the operator's raw input.
///
/// Detected by `detect_memory_command()` before routing. On a match, the orchestrator
/// handles the operation directly and returns without going through inference.
#[derive(Debug, PartialEq)]
pub enum MemoryCommand {
    /// "remember [that] X" — upsert X as an operator fact.
    Remember(String),
    /// "forget [that] X" — delete the fact identified by slug_id(X).
    Forget(String),
    /// "what do you know about me?" and variants — list all operator facts.
    List,
}

/// Detect whether `text` is a memory management command.
///
/// Pattern matching only — no inference. Returns `None` for regular queries.
/// Matching is case-insensitive; the payload is returned with original casing
/// (the operator's phrasing is stored verbatim in the VectorStore content field).
pub fn detect_memory_command(text: &str) -> Option<MemoryCommand> {
    let lower = text.trim().to_lowercase();

    // Remember — "remember [that] X"
    if let Some(_) = lower.strip_prefix("remember that ") {
        let original = &text.trim()[("remember that ".len())..];
        return Some(MemoryCommand::Remember(original.trim().to_string()));
    }
    if lower.starts_with("remember ") {
        let original = &text.trim()[("remember ".len())..];
        // Guard: question forms ("remember when...", "remember the time...") are NOT commands.
        let payload_lower = original.trim().to_lowercase();
        if payload_lower.starts_with("when ")
            || payload_lower.starts_with("the ")
            || payload_lower.starts_with("how ")
            || payload_lower.starts_with("what ")
        {
            return None;
        }
        return Some(MemoryCommand::Remember(original.trim().to_string()));
    }

    // Forget — "forget [that] X"
    if let Some(_) = lower.strip_prefix("forget that ") {
        let original = &text.trim()[("forget that ".len())..];
        return Some(MemoryCommand::Forget(original.trim().to_string()));
    }
    if lower.starts_with("forget ") {
        let original = &text.trim()[("forget ".len())..];
        return Some(MemoryCommand::Forget(original.trim().to_string()));
    }

    // List — exact known phrasings only.
    const LIST_TRIGGERS: &[&str] = &[
        "what do you know about me",
        "what do you know about me?",
        "what do you remember about me",
        "what do you remember about me?",
        "list what you know",
        "list what you know about me",
        "show me what you know",
        "show me what you know about me",
        "what do you remember",
    ];
    if LIST_TRIGGERS.iter().any(|t| lower == *t) {
        return Some(MemoryCommand::List);
    }

    None
}

/// Generate a stable identifier for a memory fact from its content.
///
/// Algorithm: lowercase → replace non-alphanumeric with space → split on whitespace
/// → join with '_' → truncate to 48 characters.
pub fn slug_id(content: &str) -> String {
    content
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .chars()
        .take(48)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_remember_command_with_that_prefix() {
        let cmd = detect_memory_command("remember that my setup uses Apple Silicon");
        assert_eq!(cmd, Some(MemoryCommand::Remember("my setup uses Apple Silicon".to_string())));
    }

    #[test]
    fn detect_remember_command_without_that() {
        let cmd = detect_memory_command("remember I prefer dark mode");
        assert_eq!(cmd, Some(MemoryCommand::Remember("I prefer dark mode".to_string())));
    }

    #[test]
    fn detect_forget_command_strips_that_prefix() {
        let cmd = detect_memory_command("forget that I'm using Python 3.14");
        assert_eq!(cmd, Some(MemoryCommand::Forget("I'm using Python 3.14".to_string())));
    }

    #[test]
    fn detect_list_command_what_do_you_know() {
        assert_eq!(detect_memory_command("what do you know about me?"), Some(MemoryCommand::List));
        assert_eq!(detect_memory_command("what do you remember about me"), Some(MemoryCommand::List));
    }

    #[test]
    fn detect_no_memory_command_for_question_form() {
        assert_eq!(detect_memory_command("remember the time we deployed that server?"), None);
        assert_eq!(detect_memory_command("remember when this broke?"), None);
        assert_eq!(detect_memory_command("how do you forget things?"), None);
        assert_eq!(detect_memory_command("what is your favorite programming language?"), None);
    }

    #[test]
    fn slug_id_is_deterministic_and_lowercase() {
        assert_eq!(slug_id("I'm building Dexter"), "i_m_building_dexter");
        assert_eq!(slug_id("I'm building Dexter"), slug_id("I'm building Dexter"));
        assert_eq!(slug_id(""), "");
        assert!(slug_id("ANY CAPS INPUT").chars().all(|c| !c.is_uppercase()),
            "slug_id output must be all-lowercase");
    }
}
