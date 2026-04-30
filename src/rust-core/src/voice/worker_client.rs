/// WorkerClient — manages a Python voice worker subprocess.
///
/// Handles spawn, handshake validation, frame I/O, health probes, and shutdown.
/// Restart policy is the caller's responsibility (VoiceCoordinator for TTS;
/// stream_audio() creates a fresh client per call for STT).

use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use crate::constants::{VOICE_WORKER_HEALTH_TIMEOUT_SECS, VOICE_WORKER_STARTUP_TIMEOUT_SECS};
use super::protocol::{self, WorkerType, msg};

pub struct WorkerClient {
    #[allow(dead_code)] // read by Phase 13 health-check and restart logic
    pub worker_type: WorkerType,
    stdin:  ChildStdin,
    stdout: BufReader<ChildStdout>,
    _child: Child,
}

#[derive(Debug)]
pub enum WorkerError {
    SpawnFailed(std::io::Error),
    HandshakeTimeout,
    HandshakeFailed(String),
    WrongWorkerType { expected: WorkerType, got: String },
    Io(std::io::Error),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerError::SpawnFailed(e)   => write!(f, "spawn failed: {e}"),
            WorkerError::HandshakeTimeout => write!(f, "handshake timed out"),
            WorkerError::HandshakeFailed(s) => write!(f, "handshake failed: {s}"),
            WorkerError::WrongWorkerType { expected, got }
                => write!(f, "wrong worker type: expected {expected}, got {got}"),
            WorkerError::Io(e) => write!(f, "IO error: {e}"),
        }
    }
}

impl std::error::Error for WorkerError {}

impl WorkerClient {
    /// Spawn `python_exe worker_script`, validate handshake, return ready client.
    pub async fn spawn(
        expected_type: WorkerType,
        python_exe:    &str,
        worker_script: &str,
    ) -> Result<Self, WorkerError> {
        let mut child = Command::new(python_exe)
            .arg(worker_script)
            .env("PYTHONPATH", "src/python-workers")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // inherit() lets Python's print(..., file=sys.stderr) and exception
            // tracebacks flow through to the parent terminal. Keeps diagnostics
            // visible during development without any extra piping overhead.
            .stderr(std::process::Stdio::inherit())
            // Phase 38 / Codex finding [25]: send SIGKILL to the worker if the
            // WorkerClient is dropped without a clean shutdown. Covers handshake
            // failures, mid-session panics, duplicate-spawn races (finding [22]),
            // and the case where shutdown() times out the 3 s wait_with_output.
            // Without this, dropped Tokio Child handles ORPHAN the Python worker.
            .kill_on_drop(true)
            .spawn()
            .map_err(WorkerError::SpawnFailed)?;

        let stdin  = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut client = WorkerClient {
            worker_type: expected_type.clone(),
            stdin,
            stdout,
            _child: child,
        };

        // Read handshake line (newline-terminated JSON) with startup timeout.
        // Uses VOICE_WORKER_STARTUP_TIMEOUT_SECS (30s), not VOICE_WORKER_HEALTH_TIMEOUT_SECS
        // (3s), because workers load heavyweight models before writing their handshake.
        let handshake = tokio::time::timeout(
            Duration::from_secs(VOICE_WORKER_STARTUP_TIMEOUT_SECS),
            client.read_handshake_line(),
        ).await
            .map_err(|_| WorkerError::HandshakeTimeout)?
            .map_err(WorkerError::Io)?;

        let parsed = protocol::parse_handshake(&handshake)
            .map_err(WorkerError::HandshakeFailed)?;

        if parsed.worker_type != expected_type {
            return Err(WorkerError::WrongWorkerType {
                expected: expected_type,
                got: parsed.worker_type.as_str().to_string(),
            });
        }

        Ok(client)
    }

    /// Send HEALTH_PING; wait for HEALTH_PONG within VOICE_WORKER_HEALTH_TIMEOUT_SECS.
    #[allow(dead_code)] // called by VoiceCoordinator::health_check_and_restart (Phase 13)
    pub async fn health_check(&mut self) -> bool {
        if self.write_frame(msg::HEALTH_PING, &[]).await.is_err() { return false; }
        tokio::time::timeout(
            Duration::from_secs(VOICE_WORKER_HEALTH_TIMEOUT_SECS),
            async {
                loop {
                    match self.read_frame().await {
                        Ok(Some((msg::HEALTH_PONG, _))) => return true,
                        Ok(Some(_)) => continue,   // discard non-pong frames
                        _ => return false,
                    }
                }
            }
        ).await.unwrap_or(false)
    }

    pub async fn write_frame(&mut self, msg_type: u8, payload: &[u8]) -> std::io::Result<()> {
        protocol::write_frame(&mut self.stdin, msg_type, payload).await
    }

    pub async fn read_frame(&mut self) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        protocol::read_frame(&mut self.stdout).await
    }

    /// Send SHUTDOWN; wait up to 3 s for process exit.
    ///
    /// Phase 38c: currently unused — daemon shutdown relies on
    /// `kill_on_drop(true)` (Session 1 [25]) which sends SIGKILL via tokio's
    /// Child drop. This method is preserved for the future graceful-shutdown
    /// path that sends SHUTDOWN frames so Python workers exit cleanly.
    #[allow(dead_code)] // Phase 38c: preserved for future graceful-shutdown wiring
    pub async fn shutdown(mut self) {
        let _ = self.write_frame(msg::SHUTDOWN, &[]).await;
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            self._child.wait(),
        ).await;
    }

    async fn read_handshake_line(&mut self) -> std::io::Result<String> {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        self.stdout.read_line(&mut line).await?;
        Ok(line)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_type_stt_as_str_returns_stt() {
        assert_eq!(WorkerType::Stt.as_str(), "stt");
    }

    #[test]
    fn worker_type_tts_as_str_returns_tts() {
        assert_eq!(WorkerType::Tts.as_str(), "tts");
    }

    #[test]
    fn worker_error_display_includes_variant_info() {
        let e = WorkerError::HandshakeFailed("bad version".to_string());
        assert!(e.to_string().contains("bad version"), "Display should include the inner message");

        let e2 = WorkerError::WrongWorkerType {
            expected: WorkerType::Stt,
            got: "tts".to_string(),
        };
        let msg = e2.to_string();
        assert!(msg.contains("stt"), "Display should mention expected type");
        assert!(msg.contains("tts"), "Display should mention got type");
    }
}
