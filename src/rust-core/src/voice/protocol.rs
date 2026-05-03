/// Binary IPC framing + handshake validation for Python voice worker processes.
///
/// Frame: [1 byte msg_type][4 bytes payload_len LE u32][payload]
/// Handshake: worker writes one '\n'-terminated JSON line before entering binary mode.
use crate::constants::VOICE_PROTOCOL_VERSION;

pub const HEADER_SIZE: usize = 5;

pub mod msg {
    #[allow(dead_code)] // sent by Phase 13 health-check loop
    pub const HEALTH_PING: u8 = 0x01;
    pub const HEALTH_PONG: u8 = 0x02;
    pub const AUDIO_CHUNK: u8 = 0x03;
    pub const AUDIO_END: u8 = 0x04;
    pub const TRANSCRIPT: u8 = 0x05;
    pub const TEXT_INPUT: u8 = 0x06;
    pub const TTS_AUDIO: u8 = 0x07;
    pub const TTS_DONE: u8 = 0x08;
    #[allow(dead_code)] // Phase 38c: WorkerClient::shutdown is currently unused (kill_on_drop path); preserved for graceful-shutdown wiring
    pub const SHUTDOWN: u8 = 0x09;
    #[allow(dead_code)] // received in error-handling paths (Phase 13+)
    pub const ERROR: u8 = 0x0A;

    // ── Browser worker (Phase 14) — must stay in sync with protocol.py ──────
    pub const BROWSER_NAVIGATE: u8 = 0x0B; // payload: JSON {"url": "..."}
    pub const BROWSER_CLICK: u8 = 0x0C; // payload: JSON {"selector": "..."}
    pub const BROWSER_TYPE: u8 = 0x0D; // payload: JSON {"selector": "...", "text": "..."}
    pub const BROWSER_EXTRACT: u8 = 0x0E; // payload: JSON {"selector": null | "..."}
    pub const BROWSER_SCREENSHOT: u8 = 0x0F; // payload: empty — worker saves path, returns in result
    pub const BROWSER_RESULT: u8 = 0x10; // payload: JSON {"success": bool, "output": "...", "error": "..."}

    /// Sent by stt_worker after all TRANSCRIPT frames for one utterance (Phase 23).
    /// Allows the persistent STT worker to remain alive across multiple utterances
    /// without closing stdout. Must stay in sync with protocol.py MSG_TRANSCRIPT_DONE.
    pub const TRANSCRIPT_DONE: u8 = 0x11; // payload: empty
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerType {
    Stt,
    Tts,
    Browser,
}

impl WorkerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkerType::Stt => "stt",
            WorkerType::Tts => "tts",
            WorkerType::Browser => "browser",
        }
    }
}

impl std::fmt::Display for WorkerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validated handshake sent by the worker on stdout before binary mode.
#[derive(Debug)]
pub struct WorkerHandshake {
    #[allow(dead_code)] // verified inside parse_handshake; field exposed for test assertions
    pub protocol_version: u32,
    pub worker_type: WorkerType,
}

/// Parse the '\n'-terminated JSON handshake line from a worker's stdout.
/// Err if: malformed JSON, version != VOICE_PROTOCOL_VERSION, unknown worker_type.
pub fn parse_handshake(line: &str) -> Result<WorkerHandshake, String> {
    let json: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("Handshake JSON parse error: {e}"))?;
    let version = json["protocol_version"]
        .as_u64()
        .ok_or("Missing protocol_version")? as u32;
    if version != VOICE_PROTOCOL_VERSION {
        return Err(format!(
            "Protocol version mismatch: expected {VOICE_PROTOCOL_VERSION}, got {version}"
        ));
    }
    let wt = match json["worker_type"].as_str().ok_or("Missing worker_type")? {
        "stt" => WorkerType::Stt,
        "tts" => WorkerType::Tts,
        "browser" => WorkerType::Browser,
        other => return Err(format!("Unknown worker_type: {other}")),
    };
    Ok(WorkerHandshake {
        protocol_version: version,
        worker_type: wt,
    })
}

/// Write one binary frame to an AsyncWrite target.
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let len = payload.len() as u32;
    let mut header = [0u8; HEADER_SIZE];
    header[0] = msg_type;
    header[1..5].copy_from_slice(&len.to_le_bytes());
    writer.write_all(&header).await?;
    if !payload.is_empty() {
        writer.write_all(payload).await?;
    }
    writer.flush().await
}

/// Read one binary frame. Returns Ok(None) on clean EOF.
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    use tokio::io::{AsyncReadExt, ErrorKind};
    let mut header = [0u8; HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let msg_type = header[0];
    let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
    let payload = if len > 0 {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        buf
    } else {
        vec![]
    };
    Ok(Some((msg_type, payload)))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handshake_stt_valid() {
        let line = r#"{"protocol_version":1,"worker_type":"stt"}"#;
        let hs = parse_handshake(line).unwrap();
        assert_eq!(hs.worker_type, WorkerType::Stt);
        assert_eq!(hs.protocol_version, 1);
    }

    #[test]
    fn parse_handshake_tts_valid() {
        let line = r#"{"protocol_version":1,"worker_type":"tts"}"#;
        let hs = parse_handshake(line).unwrap();
        assert_eq!(hs.worker_type, WorkerType::Tts);
    }

    #[test]
    fn parse_handshake_version_mismatch_returns_err() {
        let line = r#"{"protocol_version":99,"worker_type":"stt"}"#;
        let result = parse_handshake(line);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("version mismatch"));
    }

    #[test]
    fn parse_handshake_unknown_worker_type_returns_err() {
        let line = r#"{"protocol_version":1,"worker_type":"vision"}"#;
        let result = parse_handshake(line);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown worker_type"));
    }

    #[test]
    fn parse_handshake_malformed_json_returns_err() {
        let result = parse_handshake("not json at all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("parse error"));
    }

    #[test]
    fn parse_handshake_browser_valid() {
        let line = r#"{"protocol_version":1,"worker_type":"browser"}"#;
        let hs = parse_handshake(line).unwrap();
        assert_eq!(hs.worker_type, WorkerType::Browser);
        assert_eq!(hs.protocol_version, 1);
    }
}
