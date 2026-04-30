/// Structured error type for all InferenceEngine operations.
///
/// Each variant captures the information an operator actually needs to diagnose the
/// failure: the URL that was unreachable, which model was missing, which phase of
/// streaming failed, etc. Generic `String` error variants are avoided — every variant
/// is typed to its causal category so callers can pattern-match and respond differently
/// (e.g., `ModelNotFound` triggers a pull-or-fail decision; `OllamaUnavailable` triggers
/// a startup-mode health-check warning; `RequestTimeout` is a transient retry candidate).
///
/// `From<InferenceError> for tonic::Status` converts these at the gRPC boundary so
/// server.rs never needs to import this module directly.
use std::fmt;

// ── Error enum ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum InferenceError {
    /// Ollama HTTP server is not reachable at the configured base URL.
    /// Carries the URL so the operator sees exactly which address failed.
    OllamaUnavailable { url: String, source: String },

    /// A requested model is not present on disk (not in `ollama list` output).
    /// Carries the model name tag as it was requested.
    ModelNotFound(String),

    /// A streaming generation was aborted because Ollama went silent for longer
    /// than `stream_inactivity_timeout_secs`. Carries a human-readable description
    /// of where in the stream the silence was detected.
    StreamInterrupted(String),

    /// A non-streaming request (embed, list, pull) exceeded `request_timeout_secs`.
    /// Distinct from `StreamInterrupted` — different timeout primitives, different
    /// recovery paths.
    RequestTimeout,

    /// Ollama returned a non-2xx HTTP response.
    /// Carries both the status code and the body text so operators can distinguish
    /// "403 model requires auth" from "500 internal error" without grepping logs.
    ApiError { status: u16, message: String },

    /// A JSON (de)serialization failure in the NDJSON streaming decoder or request
    /// body builder. Carries the `serde_json` error description.
    /// These indicate a protocol mismatch between the expected Ollama API shape and
    /// what the server actually sent — usually a version skew.
    SerializationError(String),
}

// ── Display ───────────────────────────────────────────────────────────────────

impl fmt::Display for InferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InferenceError::OllamaUnavailable { url, source } => {
                write!(f, "Ollama unreachable at {url}: {source}")
            }
            InferenceError::ModelNotFound(name) => {
                write!(f, "Model not found: '{name}' — run `ollama pull {name}` or set auto_pull_missing_models = true")
            }
            InferenceError::StreamInterrupted(detail) => {
                write!(f, "Streaming generation interrupted: {detail}")
            }
            InferenceError::RequestTimeout => {
                write!(f, "Ollama request timed out (non-streaming)")
            }
            InferenceError::ApiError { status, message } => {
                write!(f, "Ollama API error {status}: {message}")
            }
            InferenceError::SerializationError(detail) => {
                write!(f, "NDJSON serialization error: {detail}")
            }
        }
    }
}

// ── std::error::Error ─────────────────────────────────────────────────────────

impl std::error::Error for InferenceError {}

// ── From<InferenceError> for tonic::Status ────────────────────────────────────
//
// Maps inference failures to the closest semantically correct gRPC status code.
// The intent is that a caller receiving a gRPC error can understand *why* the
// request failed without needing to parse the message string.
//
// Mapping rationale:
//   OllamaUnavailable  → UNAVAILABLE   (service is down; retry may succeed)
//   ModelNotFound      → NOT_FOUND     (resource does not exist)
//   StreamInterrupted  → UNAVAILABLE   (transient; the stream died mid-flight)
//   RequestTimeout     → DEADLINE_EXCEEDED (operation took too long)
//   ApiError           → INTERNAL      (Ollama itself returned an error)
//   SerializationError → INTERNAL      (protocol-level unexpected shape)

impl From<InferenceError> for tonic::Status {
    fn from(e: InferenceError) -> tonic::Status {
        match &e {
            InferenceError::OllamaUnavailable { .. } => {
                tonic::Status::unavailable(e.to_string())
            }
            InferenceError::ModelNotFound(_) => {
                tonic::Status::not_found(e.to_string())
            }
            InferenceError::StreamInterrupted(_) => {
                tonic::Status::unavailable(e.to_string())
            }
            InferenceError::RequestTimeout => {
                tonic::Status::deadline_exceeded(e.to_string())
            }
            InferenceError::ApiError { .. } => {
                tonic::Status::internal(e.to_string())
            }
            InferenceError::SerializationError(_) => {
                tonic::Status::internal(e.to_string())
            }
        }
    }
}

// ── Conversions from external error types ────────────────────────────────────
//
// These let `?` work ergonomically in engine.rs without littering map_err() calls.

impl From<reqwest::Error> for InferenceError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            InferenceError::RequestTimeout
        } else if e.is_connect() {
            // Capture the URL from the reqwest error if available; fall back to
            // a generic description. The engine will have already logged the URL
            // in context before propagating.
            let url = e.url()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            InferenceError::OllamaUnavailable {
                url,
                source: e.to_string(),
            }
        } else {
            // Covers status errors, body errors, decode errors etc.
            InferenceError::ApiError {
                status: e.status().map(|s| s.as_u16()).unwrap_or(0),
                message: e.to_string(),
            }
        }
    }
}

impl From<serde_json::Error> for InferenceError {
    fn from(e: serde_json::Error) -> Self {
        InferenceError::SerializationError(e.to_string())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn ollama_unavailable_maps_to_grpc_unavailable() {
        let err = InferenceError::OllamaUnavailable {
            url:    "http://localhost:11434".to_string(),
            source: "connection refused".to_string(),
        };
        let status = tonic::Status::from(err);
        assert_eq!(status.code(), Code::Unavailable);
        assert!(status.message().contains("connection refused"));
    }

    #[test]
    fn model_not_found_maps_to_grpc_not_found() {
        let err = InferenceError::ModelNotFound("phi3:mini".to_string());
        let status = tonic::Status::from(err);
        assert_eq!(status.code(), Code::NotFound);
        assert!(status.message().contains("phi3:mini"));
    }

    #[test]
    fn request_timeout_maps_to_grpc_deadline_exceeded() {
        let status = tonic::Status::from(InferenceError::RequestTimeout);
        assert_eq!(status.code(), Code::DeadlineExceeded);
    }

    #[test]
    fn api_error_maps_to_grpc_internal() {
        let err = InferenceError::ApiError { status: 500, message: "boom".to_string() };
        let status = tonic::Status::from(err);
        assert_eq!(status.code(), Code::Internal);
        assert!(status.message().contains("500"));
    }

    #[test]
    fn serialization_error_maps_to_grpc_internal() {
        let err = InferenceError::SerializationError("unexpected field".to_string());
        let status = tonic::Status::from(err);
        assert_eq!(status.code(), Code::Internal);
    }

    #[test]
    fn stream_interrupted_maps_to_grpc_unavailable() {
        let err = InferenceError::StreamInterrupted("inactivity timeout after 30s".to_string());
        let status = tonic::Status::from(err);
        assert_eq!(status.code(), Code::Unavailable);
    }

    #[test]
    fn display_includes_actionable_hint_for_model_not_found() {
        let err = InferenceError::ModelNotFound("llama3:8b".to_string());
        let msg = err.to_string();
        // Operator should see the pull command in the error message.
        assert!(msg.contains("ollama pull"));
        assert!(msg.contains("llama3:8b"));
    }
}
