//! Identity of the exact Dexter core process serving a turn.
//!
//! Semantic version alone cannot distinguish a freshly rebuilt daemon from an
//! older process that still owns the Unix socket. Persist a cheap, deterministic
//! identity derived from the running executable's path and metadata so live
//! acceptance receipts can prove which binary handled the request.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAttestation {
    pub core_version: String,
    pub process_id: u32,
    pub executable_path: String,
    pub executable_size_bytes: Option<u64>,
    pub executable_modified_unix_ms: Option<u64>,
    #[serde(default)]
    pub executable_blake3: Option<String>,
    pub identity: String,
}

impl RuntimeAttestation {
    pub fn current() -> Self {
        let executable = std::env::current_exe().ok();
        let executable_path = executable
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unresolved".to_string());
        let metadata = executable
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok());
        let executable_size_bytes = metadata.as_ref().map(std::fs::Metadata::len);
        let executable_modified_unix_ms = metadata
            .as_ref()
            .and_then(|value| value.modified().ok())
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis() as u64);
        let executable_blake3 = executable
            .as_deref()
            .and_then(|path| hash_file_blake3(path).ok());
        let process_id = std::process::id();
        let identity_material = executable_blake3.clone().unwrap_or_else(|| {
            format!(
                "{}|{}|{}|{}",
                crate::constants::CORE_VERSION,
                executable_path,
                executable_size_bytes.unwrap_or_default(),
                executable_modified_unix_ms.unwrap_or_default(),
            )
        });

        Self {
            core_version: crate::constants::CORE_VERSION.to_string(),
            process_id,
            executable_path,
            executable_size_bytes,
            executable_modified_unix_ms,
            executable_blake3,
            identity: blake3::hash(identity_material.as_bytes()).to_hex()[..16].to_string(),
        }
    }
}

fn hash_file_blake3(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::RuntimeAttestation;

    #[test]
    fn current_runtime_attestation_identifies_this_process() {
        let attestation = RuntimeAttestation::current();
        assert_eq!(attestation.process_id, std::process::id());
        assert_eq!(attestation.core_version, crate::constants::CORE_VERSION);
        assert!(!attestation.executable_path.is_empty());
        assert_eq!(
            attestation.executable_blake3.as_deref().map(str::len),
            Some(64)
        );
        assert_eq!(attestation.identity.len(), 16);
    }
}
