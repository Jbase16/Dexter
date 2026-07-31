//! Rust-owned security labels for content included in model requests.
//!
//! These labels are local metadata. They are never serialized to Ollama and
//! never derived from model text.

/// Highest sensitivity of data visible to, read by, or produced by a model
/// request. `Unknown` is deliberately the most conservative value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DataSensitivity {
    Public,
    OperatorPrivate,
    Restricted,
    #[default]
    Unknown,
}

impl DataSensitivity {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::OperatorPrivate => "operator_private",
            Self::Restricted => "restricted",
            Self::Unknown => "unknown",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Public => 0,
            Self::OperatorPrivate => 1,
            Self::Restricted => 2,
            Self::Unknown => 3,
        }
    }

    pub(crate) const fn max(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    pub(crate) const fn is_private_or_unknown(self) -> bool {
        !matches!(self, Self::Public)
    }
}

/// Trust assigned by Rust to model-visible content. `ExternalUntrusted` wins
/// aggregation so an external instruction remains visible to policy even when
/// other conservative `Unknown` content is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ContentTrust {
    Operator,
    LocalTrusted,
    LocalObserved,
    ModelGenerated,
    #[default]
    Unknown,
    ExternalUntrusted,
}

impl ContentTrust {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::LocalTrusted => "local_trusted",
            Self::LocalObserved => "local_observed",
            Self::ExternalUntrusted => "external_untrusted",
            Self::ModelGenerated => "model_generated",
            Self::Unknown => "unknown",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Operator => 0,
            Self::LocalTrusted => 1,
            Self::LocalObserved => 2,
            Self::ModelGenerated => 3,
            Self::Unknown => 4,
            Self::ExternalUntrusted => 5,
        }
    }

    pub(crate) const fn max(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    pub(crate) const fn is_untrusted_for_action(self) -> bool {
        matches!(self, Self::ExternalUntrusted | Self::Unknown)
    }
}

/// Aggregate security labels for the exact messages in one generation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PromptSecurity {
    pub(crate) sensitivity: DataSensitivity,
    pub(crate) trust: ContentTrust,
}

impl PromptSecurity {
    pub(crate) const fn new(sensitivity: DataSensitivity, trust: ContentTrust) -> Self {
        Self { sensitivity, trust }
    }

    pub(crate) const fn public_local() -> Self {
        Self::new(DataSensitivity::Public, ContentTrust::LocalTrusted)
    }

    pub(crate) const fn combine(self, other: Self) -> Self {
        Self {
            sensitivity: self.sensitivity.max(other.sensitivity),
            trust: self.trust.max(other.trust),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_security_uses_maximum_sensitivity_and_untrusted_source() {
        let local = PromptSecurity::new(
            DataSensitivity::OperatorPrivate,
            ContentTrust::LocalObserved,
        );
        let external =
            PromptSecurity::new(DataSensitivity::Public, ContentTrust::ExternalUntrusted);

        let combined = local.combine(external);
        assert_eq!(combined.sensitivity, DataSensitivity::OperatorPrivate);
        assert_eq!(combined.trust, ContentTrust::ExternalUntrusted);
    }
}
