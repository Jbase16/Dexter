use crate::constants::{MEMORY_HEAVY_WARN_THRESHOLD_GB, VM_STAT_CMD};
/// System memory sampling via vm_stat.
///
/// Provides a non-fatal headroom estimate before heavy model inference.
/// All functions are best-effort: errors are logged but never propagated
/// to callers as fatal — the memory sampler is a diagnostic tool only.
use std::process::Command;
use tracing::warn;

// ── Public API ────────────────────────────────────────────────────────────────

/// Memory snapshot from a single vm_stat invocation.
#[derive(Debug, Clone)]
pub struct MemorySnapshot {
    /// Available memory in GiB: (free + inactive) × page_size.
    /// Conservative: excludes speculative and purgeable pages.
    pub available_gb: f64,
    /// Page size in bytes as reported by vm_stat header.
    pub page_size: u64,
}

/// Error type for memory sampling failures.
#[derive(Debug)]
pub enum MemorySampleError {
    Io(std::io::Error),
    Utf8(std::str::Utf8Error),
    MissingField(&'static str),
    #[allow(dead_code)] // reserved for future numeric parse errors
    ParseError(String),
}

impl std::fmt::Display for MemorySampleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "vm_stat I/O error: {e}"),
            Self::Utf8(e) => write!(f, "vm_stat output is not UTF-8: {e}"),
            Self::MissingField(s) => write!(f, "vm_stat output missing field: {s}"),
            Self::ParseError(s) => write!(f, "vm_stat parse error: {s}"),
        }
    }
}

impl From<std::io::Error> for MemorySampleError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<std::str::Utf8Error> for MemorySampleError {
    fn from(e: std::str::Utf8Error) -> Self {
        Self::Utf8(e)
    }
}

/// Run vm_stat and return a MemorySnapshot.
pub fn sample() -> Result<MemorySnapshot, MemorySampleError> {
    let output = Command::new(VM_STAT_CMD).output()?;
    parse_vm_stat(&output.stdout)
}

/// Log a warning if available memory is below MEMORY_HEAVY_WARN_THRESHOLD_GB.
///
/// Called by the orchestrator before routing to the HEAVY model tier.
/// Non-fatal: always returns, even if vm_stat fails.
pub fn warn_if_low_for_heavy() {
    match sample() {
        Ok(snap) => {
            if snap.available_gb < MEMORY_HEAVY_WARN_THRESHOLD_GB {
                warn!(
                    available_gb  = snap.available_gb,
                    threshold_gb  = MEMORY_HEAVY_WARN_THRESHOLD_GB,
                    page_size     = snap.page_size,
                    "Available memory below HEAVY model threshold — system may experience swap pressure"
                );
            } else {
                tracing::info!(
                    available_gb = snap.available_gb,
                    "Memory headroom OK for HEAVY model"
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to sample memory before HEAVY inference — proceeding anyway");
        }
    }
}

// ── Internal parser ───────────────────────────────────────────────────────────

/// Parse raw vm_stat stdout bytes into a MemorySnapshot.
///
/// Public for unit testing. Not part of the stable API — callers should use `sample()`.
///
/// Expected vm_stat output format:
/// ```text
/// Mach Virtual Memory Statistics: (page size of 16384 bytes)
/// Pages free:                               12345.
/// Pages active:                            234567.
/// Pages inactive:                          345678.
/// ...
/// ```
pub fn parse_vm_stat(data: &[u8]) -> Result<MemorySnapshot, MemorySampleError> {
    let text = std::str::from_utf8(data)?;
    let mut page_size: Option<u64> = None;
    let mut free: Option<u64> = None;
    let mut inactive: Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        // Parse page size from the header line.
        // Format: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
        if page_size.is_none() {
            if let Some(ps) = extract_page_size(trimmed) {
                page_size = Some(ps);
                continue;
            }
        }

        if let Some(n) = parse_pages_line("Pages free:", trimmed) {
            free = Some(n);
        } else if let Some(n) = parse_pages_line("Pages inactive:", trimmed) {
            inactive = Some(n);
        }
    }

    let page_size = page_size.ok_or(MemorySampleError::MissingField("page size header"))?;
    let free = free.ok_or(MemorySampleError::MissingField("Pages free"))?;
    let inactive = inactive.ok_or(MemorySampleError::MissingField("Pages inactive"))?;

    let available_bytes = (free + inactive) * page_size;
    let available_gb = available_bytes as f64 / 1_073_741_824.0; // 1 GiB = 2^30 bytes

    Ok(MemorySnapshot {
        available_gb,
        page_size,
    })
}

/// Extract the page size (in bytes) from the vm_stat header line.
///
/// Returns None if the line is not the header or cannot be parsed.
fn extract_page_size(line: &str) -> Option<u64> {
    // Target: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let start = line.find("page size of")?;
    let rest = &line[start + "page size of".len()..];
    let rest = rest.trim();
    let end = rest.find(' ')?;
    rest[..end].parse().ok()
}

/// Parse a "Pages <label>: <digits>." line, returning the digit value.
///
/// vm_stat uses trailing periods as field terminators. This parser strips them.
/// Returns None if the line does not start with `label` or cannot be parsed.
fn parse_pages_line(label: &str, line: &str) -> Option<u64> {
    if !line.starts_with(label) {
        return None;
    }
    let rest = line[label.len()..].trim().trim_end_matches('.');
    rest.parse().ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               10000.
Pages active:                            200000.
Pages inactive:                           50000.
Pages speculative:                          500.
Pages throttled:                              0.
Pages wired down:                         40000.
Pages purgeable:                             10.
";

    #[test]
    fn parse_vm_stat_valid_returns_correct_gb() {
        let snap = parse_vm_stat(FIXTURE_VM_STAT.as_bytes()).unwrap();
        // (10000 + 50000) × 16384 = 60000 × 16384 = 983_040_000 bytes ≈ 0.915 GiB
        let expected = (10_000u64 + 50_000) * 16_384;
        let expected_gb = expected as f64 / 1_073_741_824.0;
        assert!(
            (snap.available_gb - expected_gb).abs() < 1e-6,
            "available_gb mismatch: got {}, expected {}",
            snap.available_gb,
            expected_gb
        );
    }

    #[test]
    fn parse_vm_stat_page_size_parsed_from_header() {
        let snap = parse_vm_stat(FIXTURE_VM_STAT.as_bytes()).unwrap();
        assert_eq!(
            snap.page_size, 16_384,
            "Page size must be extracted from the vm_stat header"
        );
    }

    #[test]
    fn parse_vm_stat_non_apple_silicon_page_size() {
        // Intel Macs use 4096-byte pages. Parser must handle any page size.
        let data = "\
Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free:                               10000.
Pages inactive:                           50000.
";
        let snap = parse_vm_stat(data.as_bytes()).unwrap();
        assert_eq!(snap.page_size, 4_096);
        let expected_gb = (10_000u64 + 50_000) * 4_096;
        let expected_gb = expected_gb as f64 / 1_073_741_824.0;
        assert!((snap.available_gb - expected_gb).abs() < 1e-6);
    }

    #[test]
    fn parse_vm_stat_missing_pages_free_returns_err() {
        let data = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages inactive:                           50000.
";
        let err = parse_vm_stat(data.as_bytes()).unwrap_err();
        assert!(matches!(err, MemorySampleError::MissingField("Pages free")));
    }

    #[test]
    fn parse_vm_stat_missing_header_returns_err() {
        let data = "Pages free: 10000.\nPages inactive: 50000.\n";
        let err = parse_vm_stat(data.as_bytes()).unwrap_err();
        assert!(matches!(
            err,
            MemorySampleError::MissingField("page size header")
        ));
    }

    /// Calls the real `vm_stat` binary. Run with: make test-e2e
    #[tokio::test]
    #[ignore = "requires live Apple Silicon — run with: make test-e2e"]
    async fn memory_sample_positive_on_live_machine() {
        let snap = sample().expect("vm_stat must succeed on live machine");
        assert!(snap.available_gb > 0.0, "Available GB must be positive");
        assert!(snap.page_size > 0, "Page size must be positive");
        // Apple Silicon always uses 16384-byte pages.
        assert_eq!(
            snap.page_size, 16_384,
            "Expected 16KB pages on Apple Silicon"
        );
    }
}
