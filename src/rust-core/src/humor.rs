//! Humor Engine.
//!
//! This module owns joke-generation request shaping and output filtering. It is
//! deliberately mechanism-based rather than bank-based: Dexter should still ask
//! the model to generate original humor, but the model output must survive
//! simple, deterministic gates before it reaches the operator.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::inference::engine::Message;

const RECENT_HISTORY_LIMIT: usize = 25;
const SIMILARITY_REJECT_THRESHOLD: f32 = 0.60;

const HARD_REJECT_PATTERNS: &[&str] = &[
    "i'm not sure",
    "im not sure",
    "here's one",
    "heres one",
    "here is one",
    "this might",
    "as an ai",
    "if you know what i mean",
    "get it?",
    "ladder",
    "brothel",
    "higher level of service",
    "walks into a bar",
    "walked into a bar",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumorCategory {
    Roast,
    Dirty,
    Dark,
    DadJoke,
    Tech,
    General,
}

impl HumorCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            HumorCategory::Roast => "roast",
            HumorCategory::Dirty => "dirty",
            HumorCategory::Dark => "dark",
            HumorCategory::DadJoke => "dad_joke",
            HumorCategory::Tech => "tech",
            HumorCategory::General => "general",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumorMechanism {
    Misdirection,
    AbsurdSpecificity,
    HostileUnderstatement,
    DarkEscalation,
    FalseAuthority,
    Literalism,
}

impl HumorMechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            HumorMechanism::Misdirection => "misdirection",
            HumorMechanism::AbsurdSpecificity => "absurd_specificity",
            HumorMechanism::HostileUnderstatement => "hostile_understatement",
            HumorMechanism::DarkEscalation => "dark_escalation",
            HumorMechanism::FalseAuthority => "false_authority",
            HumorMechanism::Literalism => "literalism",
        }
    }

    pub fn rule(self) -> &'static str {
        match self {
            HumorMechanism::Misdirection => {
                "Set up one expectation, then make the punchline land by revealing a different adult meaning."
            }
            HumorMechanism::AbsurdSpecificity => {
                "Make the funny part come from a weirdly specific concrete detail instead of a generic insult."
            }
            HumorMechanism::HostileUnderstatement => {
                "Understate something obviously intense with dry, overly calm wording."
            }
            HumorMechanism::DarkEscalation => {
                "Start normal and escalate into a darker implication in the punchline."
            }
            HumorMechanism::FalseAuthority => {
                "Use fake expertise, fake technicality, or official-sounding logic to make the punchline absurd."
            }
            HumorMechanism::Literalism => {
                "Take a phrase too literally, then twist the literal reading into the punchline."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumorPlan {
    pub category: HumorCategory,
    pub mechanism: HumorMechanism,
    pub count: usize,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumorSelection {
    pub final_output: String,
    pub first_reject_reason: Option<String>,
    pub repair_used: bool,
}

pub fn is_joke_request(text: &str) -> bool {
    let t = text.to_lowercase();
    if !(t.contains("joke") || t.contains("roast me")) {
        return false;
    }

    let request_markers = [
        "tell me",
        "give me",
        "write me",
        "make me",
        "hit me with",
        "i want",
        "i'd like",
        "id like",
        "got any",
        "know any",
        "another",
        "different",
        "one more",
        "more ",
    ];
    request_markers.iter().any(|marker| t.contains(marker))
}

pub fn is_generation_followup(text: &str) -> bool {
    let t = text.to_lowercase();
    let identity_variation_markers = [
        "make it gay",
        "make it gayer",
        "make it queer",
        "make it queerer",
        "gay one",
        "queer one",
        "gay version",
        "queer version",
        "more gay",
        "more queer",
        "gayer",
        "queerer",
    ];
    if identity_variation_markers
        .iter()
        .any(|marker| t.contains(marker))
    {
        return true;
    }

    let counted_more_request = t.contains("more")
        && requested_count(&t).is_some()
        && (t.contains("give me")
            || t.contains("give us")
            || t.contains("i want")
            || t.contains("i'd like")
            || t.contains("id like")
            || t.contains("let's hear")
            || t.contains("lets hear"));
    if counted_more_request {
        return true;
    }

    let generation_markers = [
        "another one",
        "another joke",
        "different one",
        "different joke",
        "give me another",
        "tell me another",
        "tell me one then",
        "one more",
        "next one",
        "do better",
        "try again",
        "make it dirtier",
        "make it nastier",
        "make it raunchier",
        "more nsfw",
        "more dirty",
        "more raunchy",
        "less tame",
        "less wholesome",
        "that isn't",
        "that isnt",
        "that wasn't",
        "that wasnt",
        "not 10",
        "not ten",
    ];
    generation_markers.iter().any(|marker| t.contains(marker))
}

pub fn should_handle(text: &str, recent_joke_context: bool) -> bool {
    is_joke_request(text) || (recent_joke_context && is_generation_followup(text))
}

pub fn infer_humor_category(text: &str) -> HumorCategory {
    let t = text.to_lowercase();
    if t.contains("roast") {
        HumorCategory::Roast
    } else if t.contains("dirty")
        || t.contains("nsfw")
        || t.contains("adult")
        || t.contains("filthy")
    {
        HumorCategory::Dirty
    } else if t.contains("dark") {
        HumorCategory::Dark
    } else if t.contains("dad joke") || t.contains("dad-joke") {
        HumorCategory::DadJoke
    } else if t.contains("tech") || t.contains("programmer") || t.contains("software") {
        HumorCategory::Tech
    } else {
        HumorCategory::General
    }
}

pub fn choose_mechanism(text: &str, category: HumorCategory) -> HumorMechanism {
    let t = text.to_lowercase();
    if t.contains("roast") {
        HumorMechanism::AbsurdSpecificity
    } else if t.contains("dirty")
        || t.contains("nsfw")
        || t.contains("adult")
        || t.contains("filthy")
    {
        HumorMechanism::Misdirection
    } else if t.contains("dark") {
        HumorMechanism::DarkEscalation
    } else if t.contains("dad joke") || t.contains("dad-joke") {
        HumorMechanism::Literalism
    } else if t.contains("tech") || t.contains("programmer") || t.contains("software") {
        HumorMechanism::FalseAuthority
    } else {
        match category {
            HumorCategory::Roast => HumorMechanism::AbsurdSpecificity,
            HumorCategory::Dirty => HumorMechanism::Misdirection,
            HumorCategory::Dark => HumorMechanism::DarkEscalation,
            HumorCategory::DadJoke => HumorMechanism::Literalism,
            HumorCategory::Tech => HumorMechanism::FalseAuthority,
            HumorCategory::General => HumorMechanism::HostileUnderstatement,
        }
    }
}

pub fn build_humor_plan(user_text: &str) -> HumorPlan {
    let category = infer_humor_category(user_text);
    let mechanism = choose_mechanism(user_text, category);
    let count = requested_count(user_text).unwrap_or(1).clamp(1, 10);
    let prompt = build_humor_prompt(user_text, category, mechanism, count);
    HumorPlan {
        category,
        mechanism,
        count,
        prompt,
    }
}

pub fn effective_request_for_generation(user_text: &str, history: &[Message]) -> String {
    if is_joke_request(user_text) || !is_generation_followup(user_text) {
        return user_text.to_string();
    }

    let previous_request = history
        .iter()
        .rev()
        .skip_while(|m| m.role == "user" && m.content == user_text)
        .find(|m| m.role == "user" && is_joke_request(&m.content))
        .map(|m| m.content.trim());

    match previous_request {
        Some(prev) if !prev.is_empty() => format!(
            "{user_text}\n\nContinue the same joke request/category as this recent operator request:\n{prev}"
        ),
        _ => user_text.to_string(),
    }
}

pub fn build_humor_prompt(
    user_text: &str,
    category: HumorCategory,
    mechanism: HumorMechanism,
    count: usize,
) -> String {
    let count_rule = if count == 1 {
        "Generate exactly one humor response.".to_string()
    } else {
        format!("Generate exactly {count} jokes in one response. Number them 1-{count}.")
    };

    let category_rule = if category == HumorCategory::Dirty {
        "\nFor dirty/adult requests, every joke must have obvious adult sexual innuendo or an explicit adult double meaning. Use a compact pun structure. Do not write clean motivational humor, generic spouse anecdotes, or generic bedroom anecdotes."
    } else {
        ""
    };

    format!(
        "You are generating humor for Dexter.\n\
         User request:\n\
         {user_text}\n\n\
         Category: {category}\n\
         Comedy mechanism: {mechanism}\n\
         Mechanism rule: {mechanism_rule}\n\
         {count_rule}{category_rule}\n\n\
         Hard rules:\n\
         - Output only the joke or requested numbered jokes.\n\
         - No preamble.\n\
         - No explanation.\n\
         - No apology.\n\
         - No \"here's one.\"\n\
         - No \"I'm not sure.\"\n\
         - No recycled internet templates.\n\
         - Do not use ladder, brothel, higher level of service, or walks-into-a-bar templates.\n\
         - Keep each joke short.\n\
         - Make each joke specific.\n",
        category = category.as_str(),
        mechanism = mechanism.as_str(),
        mechanism_rule = mechanism.rule(),
    )
}

pub fn build_repair_prompt(
    user_text: &str,
    failed_candidate: &str,
    reason: &str,
    category: HumorCategory,
    count: usize,
) -> String {
    let count_rule = if count == 1 {
        "Generate exactly one replacement joke.".to_string()
    } else {
        format!(
            "Generate exactly {count} replacement jokes in one response. Number them 1-{count}."
        )
    };
    let category_rule = if category == HumorCategory::Dirty {
        "\nFor dirty/adult requests, each replacement must include obvious adult sexual innuendo or an explicit adult double meaning. Use compact pun structure, not clean motivational humor, generic spouse anecdotes, or generic bedroom anecdotes."
    } else {
        ""
    };

    format!(
        "The previous candidate failed because: {reason}\n\
         User request:\n\
         {user_text}\n\n\
         Failed candidate:\n\
         {failed_candidate}\n\n\
         {count_rule}{category_rule}\n\
         Use a different joke structure.\n\
         No preamble.\n\
         No explanation.\n\
         No recycled templates.\n\
         Output only the joke or requested numbered jokes."
    )
}

pub fn build_last_chance_repair_prompt(
    user_text: &str,
    reason: &str,
    category: HumorCategory,
    count: usize,
) -> String {
    let count_rule = if count == 1 {
        "Generate exactly one replacement joke.".to_string()
    } else {
        format!(
            "Generate exactly {count} replacement jokes in one response. Number them 1-{count}."
        )
    };
    let category_rule = if category == HumorCategory::Dirty {
        "\nThis is a dirty/adult request: each joke must contain obvious adult sexual innuendo or an explicit adult double meaning."
    } else {
        ""
    };

    format!(
        "Previous humor attempts still failed Dexter's filter because: {reason}\n\
         User request:\n\
         {user_text}\n\n\
         {count_rule}{category_rule}\n\
         Choose a completely different premise, occupation, object, metaphor, and punchline structure from anything already attempted.\n\
         No preamble.\n\
         No explanation.\n\
         No recycled templates.\n\
         Output only the joke or requested numbered jokes."
    )
}

pub fn hard_reject(candidate: &str) -> Option<String> {
    let normalized = normalize_for_pattern(candidate);
    HARD_REJECT_PATTERNS
        .iter()
        .find(|pattern| normalized.contains(**pattern))
        .map(|pattern| format!("hard reject pattern: {pattern}"))
}

pub fn is_too_similar(candidate: &str, recent: &[String]) -> bool {
    let candidate_norm = normalize_joke(candidate);
    recent.iter().any(|prev| {
        jaccard_similarity(&candidate_norm, prev) > SIMILARITY_REJECT_THRESHOLD
            || has_shared_word_shingle(&candidate_norm, prev, 5)
            || has_shared_salient_terms(&candidate_norm, prev, 2)
    })
}

pub fn reject_reason(candidate: &str, recent: &[String]) -> Option<String> {
    hard_reject(candidate).or_else(|| {
        if is_too_similar(candidate, recent) {
            Some("too similar to a recent joke".to_string())
        } else {
            None
        }
    })
}

pub fn reject_reason_for_category(
    candidate: &str,
    recent: &[String],
    category: HumorCategory,
) -> Option<String> {
    reject_reason(candidate, recent).or_else(|| {
        if category == HumorCategory::Dirty && !contains_adult_signal(candidate) {
            Some("missing adult/NSFW innuendo for dirty joke request".to_string())
        } else if category == HumorCategory::Dirty && contains_generic_relationship_setup(candidate)
        {
            Some("generic spouse/relationship anecdote for dirty joke request".to_string())
        } else {
            None
        }
    })
}

pub fn normalize_joke(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_space = false;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

pub fn recent_jokes_from_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == "assistant")
        .take(RECENT_HISTORY_LIMIT)
        .map(|m| normalize_joke(&m.content))
        .filter(|m| !m.is_empty())
        .collect()
}

pub fn recent_joke_outputs_for_prompt(messages: &[Message], limit: usize) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == "assistant")
        .take(limit)
        .map(|m| m.content.trim())
        .filter(|m| !m.is_empty())
        .map(|m| m.chars().take(280).collect::<String>())
        .collect()
}

pub fn append_recent_avoidance(mut prompt: String, recent_outputs: &[String]) -> String {
    if recent_outputs.is_empty() {
        return prompt;
    }

    prompt.push_str("\nRecent jokes/premises to avoid repeating:\n");
    for (idx, output) in recent_outputs.iter().enumerate() {
        prompt.push_str(&format!("{}. {output}\n", idx + 1));
    }
    prompt.push_str(
        "Do not reuse their subject, setup, premise, occupation, object, metaphor, or punchline structure.\n",
    );
    prompt
}

#[allow(dead_code)]
pub fn select_final_candidate(
    first_candidate: &str,
    repair_candidate: Option<&str>,
    recent: &[String],
) -> HumorSelection {
    let first_clean = first_candidate.trim();
    let first_reject_reason = reject_reason(first_clean, recent);
    if first_reject_reason.is_none() {
        return HumorSelection {
            final_output: first_clean.to_string(),
            first_reject_reason,
            repair_used: false,
        };
    }

    if let Some(repair) = repair_candidate {
        let repair_clean = repair.trim();
        if !repair_clean.is_empty() && reject_reason(repair_clean, recent).is_none() {
            return HumorSelection {
                final_output: repair_clean.to_string(),
                first_reject_reason,
                repair_used: true,
            };
        }
        if !repair_clean.is_empty() {
            return HumorSelection {
                final_output: repair_clean.to_string(),
                first_reject_reason,
                repair_used: true,
            };
        }
    }

    HumorSelection {
        final_output: first_clean.to_string(),
        first_reject_reason,
        repair_used: false,
    }
}

pub fn select_final_candidate_for_category(
    first_candidate: &str,
    repair_candidate: Option<&str>,
    recent: &[String],
    category: HumorCategory,
) -> HumorSelection {
    let first_clean = first_candidate.trim();
    let first_reject_reason = reject_reason_for_category(first_clean, recent, category);
    if first_reject_reason.is_none() {
        return HumorSelection {
            final_output: first_clean.to_string(),
            first_reject_reason,
            repair_used: false,
        };
    }

    if let Some(repair) = repair_candidate {
        let repair_clean = repair.trim();
        if !repair_clean.is_empty()
            && reject_reason_for_category(repair_clean, recent, category).is_none()
        {
            return HumorSelection {
                final_output: repair_clean.to_string(),
                first_reject_reason,
                repair_used: true,
            };
        }

        if !repair_clean.is_empty() && hard_reject(repair_clean).is_none() {
            return HumorSelection {
                final_output: repair_clean.to_string(),
                first_reject_reason,
                repair_used: true,
            };
        }
    }

    if !first_clean.is_empty() && hard_reject(first_clean).is_none() {
        return HumorSelection {
            final_output: first_clean.to_string(),
            first_reject_reason,
            repair_used: repair_candidate.is_some(),
        };
    }

    HumorSelection {
        final_output: "I had two candidates, but both failed the humor filter. Try me again and I'll take another swing.".to_string(),
        first_reject_reason,
        repair_used: repair_candidate.is_some(),
    }
}

pub fn output_hash(output: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalize_joke(output).hash(&mut hasher);
    hasher.finish()
}

fn normalize_for_pattern(text: &str) -> String {
    text.to_lowercase()
        .replace('\u{2019}', "'")
        .replace('\u{2018}', "'")
        .replace('\u{201c}', "\"")
        .replace('\u{201d}', "\"")
}

fn requested_count(text: &str) -> Option<usize> {
    let t = text.to_lowercase();
    for word in t.split(|c: char| !c.is_ascii_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if let Ok(n) = word.parse::<usize>() {
            return Some(n);
        }
    }

    if t.contains("ten") {
        Some(10)
    } else if t.contains("nine") {
        Some(9)
    } else if t.contains("eight") {
        Some(8)
    } else if t.contains("seven") {
        Some(7)
    } else if t.contains("six") {
        Some(6)
    } else if t.contains("five") {
        Some(5)
    } else if t.contains("four") {
        Some(4)
    } else if t.contains("three") {
        Some(3)
    } else if t.contains("two") || t.contains("couple") {
        Some(2)
    } else {
        None
    }
}

fn contains_adult_signal(text: &str) -> bool {
    let normalized = normalize_joke(text);
    let markers = [
        "adult",
        "ass",
        "bedroom",
        "blow",
        "boob",
        "climax",
        "cock",
        "cream",
        "creamy",
        "dick",
        "dough",
        "drill",
        "erection",
        "fill",
        "fuck",
        "grind",
        "hard",
        "handle",
        "horny",
        "kink",
        "knead",
        "loaded",
        "nail",
        "naked",
        "nude",
        "orgasm",
        "penetrat",
        "penis",
        "pipe",
        "pound",
        "pressure",
        "pump",
        "pussy",
        "rise",
        "rising",
        "screw",
        "sex",
        "shaft",
        "spread",
        "stroke",
        "stuffed",
        "suck",
        "throbbing",
        "thrust",
        "toy",
        "vibrat",
        "wet",
        "wood",
    ];
    markers.iter().any(|marker| normalized.contains(marker))
}

fn contains_generic_relationship_setup(text: &str) -> bool {
    let normalized = normalize_joke(text);
    let markers = [
        "i told my wife",
        "i told my girlfriend",
        "i told my partner",
        "my date said",
        "my date told",
        "my wife",
        "my wife told me",
        "my wife asked",
        "my wife said",
        "my girlfriend",
        "my girlfriend told me",
        "my girlfriend asked",
        "my girlfriend said",
        "my partner",
        "my partner told me",
        "my partner asked",
        "my partner said",
    ];
    markers.iter().any(|marker| normalized.contains(marker))
}

fn jaccard_similarity(a: &str, b: &str) -> f32 {
    let a_words = word_set(a);
    let b_words = word_set(b);
    if a_words.is_empty() || b_words.is_empty() {
        return 0.0;
    }
    let intersection = a_words.intersection(&b_words).count() as f32;
    let union = a_words.union(&b_words).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn has_shared_word_shingle(a: &str, b: &str, width: usize) -> bool {
    let a_words: Vec<&str> = a.split_whitespace().collect();
    let b_words: Vec<&str> = b.split_whitespace().collect();
    if a_words.len() < width || b_words.len() < width {
        return false;
    }

    let a_shingles = a_words
        .windows(width)
        .map(|w| w.join(" "))
        .collect::<HashSet<_>>();
    b_words
        .windows(width)
        .map(|w| w.join(" "))
        .any(|shingle| a_shingles.contains(&shingle))
}

fn has_shared_salient_terms(a: &str, b: &str, threshold: usize) -> bool {
    let a_terms = salient_word_set(a);
    if a_terms.len() < threshold {
        return false;
    }
    let b_terms = salient_word_set(b);
    a_terms.intersection(&b_terms).take(threshold).count() >= threshold
}

fn salient_word_set(text: &str) -> HashSet<&str> {
    const STOPWORDS: &[&str] = &[
        "about", "after", "again", "because", "before", "being", "could", "dirty", "enough",
        "every", "focus", "getting", "going", "handle", "handled", "hard", "heard", "joke",
        "looking", "make", "more", "really", "spent", "that", "there", "through", "wanted", "what",
        "when", "where", "which", "with", "work", "working", "would",
    ];
    text.split_whitespace()
        .filter(|w| w.len() >= 4 && !STOPWORDS.contains(w))
        .collect::<HashSet<_>>()
}

fn word_set(text: &str) -> HashSet<&str> {
    text.split_whitespace()
        .filter(|w| w.len() > 2)
        .collect::<HashSet<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_hedge_preamble() {
        let reason = hard_reject("I'm not sure if this qualifies as a dirty joke, but here goes.")
            .expect("hedge preamble should reject");
        assert!(reason.contains("i'm not sure"));
    }

    #[test]
    fn rejects_ladder_brothel_template() {
        assert!(hard_reject(
            "Why did the man bring a ladder to the brothel? Higher level of service."
        )
        .is_some());
    }

    #[test]
    fn rejects_repeated_punchline() {
        let recent = vec![normalize_joke(
            "I told my wife she was drawing her eyebrows too high. She looked surprised.",
        )];
        assert!(is_too_similar(
            "I told my wife her eyebrows were drawn too high. She looked surprised.",
            &recent,
        ));
    }

    #[test]
    fn rejects_repeated_setup_phrase() {
        let recent = vec![normalize_joke(
            "I used to be a baker, but I couldn't make enough dough, so now I just focus on my buns.",
        )];
        assert!(is_too_similar(
            "I used to be a baker, but I couldn't make enough dough, so now I work somewhere else.",
            &recent,
        ));
    }

    #[test]
    fn rejects_repeated_premise_terms() {
        let recent = vec![normalize_joke(
            "I used to be a baker, but I couldn't make enough dough, so now I just focus on my buns.",
        )];
        assert!(is_too_similar(
            "Why did the baker have such a dirty job? He spent all day kneading the dough and getting his buns well-handled.",
            &recent,
        ));
    }

    #[test]
    fn mechanism_prompt_has_no_preamble_permission() {
        let plan = build_humor_plan("tell me a dirty joke");
        assert!(plan.prompt.contains("No preamble"));
        assert!(plan.prompt.contains("No explanation"));
        assert!(plan.prompt.contains("No \"here's one.\""));
        assert!(plan.prompt.contains("No \"I'm not sure.\""));
    }

    #[test]
    fn stepdad_joke_prompt_keeps_literal_request() {
        let plan = build_humor_plan("give me 10 more step-dad jokes");
        assert_eq!(plan.count, 10);
        assert_eq!(plan.category, HumorCategory::DadJoke);
        assert_eq!(plan.mechanism, HumorMechanism::Literalism);
        assert!(plan.prompt.contains("Generate exactly 10 jokes"));
        assert!(plan.prompt.contains("give me 10 more step-dad jokes"));
        assert!(!plan.prompt.contains("adult/NSFW dad-joke-style pun"));
        assert!(!plan.prompt.contains("not about stepdads"));
    }

    #[test]
    fn followup_inherits_previous_joke_category() {
        let history = vec![
            Message::user("tell me a dirty dad joke"),
            Message::assistant("A prior joke."),
            Message::user("another one"),
        ];
        let effective = effective_request_for_generation("another one", &history);
        let plan = build_humor_plan(&effective);

        assert!(effective.contains("tell me a dirty dad joke"));
        assert_eq!(plan.category, HumorCategory::Dirty);
        assert_eq!(plan.mechanism, HumorMechanism::Misdirection);
        assert!(plan.prompt.contains("dirty/adult requests"));
    }

    #[test]
    fn counted_more_followup_inherits_previous_joke_category() {
        let history = vec![
            Message::user("tell me a dirty dad joke"),
            Message::assistant("A prior dirty joke."),
            Message::user("give me 2 more"),
        ];
        let effective = effective_request_for_generation("give me 2 more", &history);
        let plan = build_humor_plan(&effective);

        assert!(should_handle("give me 2 more", true));
        assert!(effective.contains("tell me a dirty dad joke"));
        assert_eq!(plan.count, 2);
        assert_eq!(plan.category, HumorCategory::Dirty);
    }

    #[test]
    fn identity_variation_followup_stays_in_humor_engine() {
        let history = vec![
            Message::user("tell me a gay joke"),
            Message::assistant("A prior gay joke."),
            Message::user("make it gayer"),
        ];
        let effective = effective_request_for_generation("make it gayer", &history);

        assert!(should_handle("make it gayer", true));
        assert!(effective.contains("tell me a gay joke"));
    }

    #[test]
    fn repair_attempt_runs_once_in_selection_logic() {
        let recent: Vec<String> = vec![];
        let selection = select_final_candidate(
            "I'm not sure if this qualifies, but here goes.",
            Some("My therapist says I avoid intimacy, which is unfair because I named my Wi-Fi 'Commitment Issues' and connect to it every night."),
            &recent,
        );

        assert!(selection.repair_used);
        assert!(selection.first_reject_reason.is_some());
        assert!(selection.final_output.contains("Commitment Issues"));
    }

    #[test]
    fn repair_prompt_preserves_count_and_dirty_category() {
        let prompt = build_repair_prompt(
            "give me 3 more dirty dad jokes",
            "1. Clean joke\n2. Clean joke\n3. Clean joke",
            "missing adult/NSFW innuendo for dirty joke request",
            HumorCategory::Dirty,
            3,
        );

        assert!(prompt.contains("Generate exactly 3 replacement jokes"));
        assert!(prompt.contains("Number them 1-3"));
        assert!(prompt.contains("each replacement must include obvious adult sexual innuendo"));
    }

    #[test]
    fn last_chance_repair_prompt_demands_different_premise() {
        let prompt = build_last_chance_repair_prompt(
            "another one",
            "too similar to a recent joke",
            HumorCategory::Dirty,
            1,
        );

        assert!(prompt.contains("Generate exactly one replacement joke"));
        assert!(prompt.contains("completely different premise"));
        assert!(prompt.contains("obvious adult sexual innuendo"));
    }

    #[test]
    fn dirty_category_rejects_clean_joke() {
        let reason = reject_reason_for_category(
            "I told my wife she should embrace her mistakes, so she hugged me.",
            &[],
            HumorCategory::Dirty,
        )
        .expect("clean candidate should fail dirty request");

        assert!(reason.contains("missing adult/NSFW"));
    }

    #[test]
    fn dirty_category_rejects_generic_relationship_setup() {
        let reason = reject_reason_for_category(
            "My wife told me she wanted something new in the bedroom, so I brought a toolbox.",
            &[],
            HumorCategory::Dirty,
        )
        .expect("generic relationship setup should fail dirty request");

        assert!(reason.contains("generic spouse"));
    }

    #[test]
    fn hard_rejected_repair_cannot_be_final_output() {
        let selection = select_final_candidate_for_category(
            "I'm not sure if this qualifies as a dirty joke, but here goes.",
            Some("Why did the man bring a ladder to the brothel? Higher level of service."),
            &[],
            HumorCategory::Dirty,
        );

        assert!(!selection.final_output.to_lowercase().contains("ladder"));
        assert!(selection.final_output.contains("failed the humor filter"));
    }

    #[test]
    fn soft_dirty_quality_failure_returns_non_hard_rejected_candidate() {
        let selection = select_final_candidate_for_category(
            "I told my wife she should embrace her mistakes, so she hugged me.",
            Some("I tried to write a dirty joke about calendars, but the dates never lined up."),
            &[],
            HumorCategory::Dirty,
        );

        assert!(selection.repair_used);
        assert!(!selection.final_output.contains("failed the humor filter"));
        assert!(selection.final_output.contains("calendars"));
    }

    #[test]
    fn recent_avoidance_prompt_includes_recent_outputs() {
        let prompt = append_recent_avoidance(
            "Generate a joke.".to_string(),
            &["I used to be a baker, but I couldn't make enough dough.".to_string()],
        );

        assert!(prompt.contains("Recent jokes/premises to avoid repeating"));
        assert!(prompt.contains("baker"));
        assert!(prompt.contains("Do not reuse their subject"));
    }

    #[test]
    fn joke_request_short_circuit_predicate_ignores_explanation_followup() {
        assert!(should_handle("tell me a dad joke", false));
        assert!(should_handle("another one", true));
        assert!(!should_handle("explain the joke", true));
        assert!(!should_handle("why is that funny", true));
    }
}
