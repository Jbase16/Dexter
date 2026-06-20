use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationKind {
    Raw,
    Summary,
    KeyValue,
    Diff,
    FingerprintOnly,
    CapabilityOnly,
    ErrorOnly,
    CommandStatus,
    MetadataOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRepresentation {
    pub kind: RepresentationKind,
    pub payload: String,
    pub estimated_tokens: usize,
    pub utility_multiplier: f64,
}

impl CandidateRepresentation {
    pub fn new(
        kind: RepresentationKind,
        payload: impl Into<String>,
        utility_multiplier: f64,
    ) -> Self {
        let payload = payload.into();
        let estimated_tokens = estimate_tokens(&payload);
        Self {
            kind,
            payload,
            estimated_tokens,
            utility_multiplier,
        }
    }
}

pub fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }

    let structured = text.contains('{')
        || text.contains('}')
        || text.contains(';')
        || text.contains("=>")
        || text.lines().count() > 8;
    let divisor = if structured { 3 } else { 4 };
    ((chars + divisor - 1) / divisor).max(1)
}

pub fn fingerprint(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

pub fn clipboard_representations(raw: &str, force_raw: bool) -> Vec<CandidateRepresentation> {
    if force_raw {
        return vec![CandidateRepresentation::new(
            RepresentationKind::Raw,
            raw.to_string(),
            1.0,
        )];
    }

    let summary = summarize_text(raw);
    vec![
        CandidateRepresentation::new(RepresentationKind::Summary, summary, 0.72),
        CandidateRepresentation::new(RepresentationKind::Raw, raw.to_string(), 1.0),
    ]
}

pub fn summarize_text(raw: &str) -> String {
    let kind = if raw.trim_start().starts_with('{') || raw.trim_start().starts_with('[') {
        "structured data"
    } else if raw.contains("func ") || raw.contains("struct ") || raw.contains("class ") {
        "code"
    } else if raw.lines().count() > 6 {
        "multi-line text"
    } else {
        "text"
    };

    let mut preview = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ");
    if preview.chars().count() > 240 {
        preview = preview.chars().take(240).collect::<String>();
        preview.push_str("...");
    }

    if preview.is_empty() {
        format!("copied_text.kind={kind}")
    } else {
        format!("copied_text.kind={kind}; preview=\"{preview}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_blake3_hex() {
        assert_eq!(
            fingerprint("dexter-context"),
            "280dfe89eadf90b9fe4f42ed6ff76ae2ab6518e1a31a6555751ca19c5badd383"
        );
    }

    #[test]
    fn clipboard_summary_uses_metadata_language() {
        let summary = summarize_text("func parseThing() {}");
        assert!(summary.starts_with("copied_text.kind=code"));
        assert!(!summary.contains("Clipboard contains"));
    }
}
