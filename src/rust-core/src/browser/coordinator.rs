/// BrowserCoordinator — lifecycle manager for the Playwright browser worker.
///
/// Mirrors VoiceCoordinator: long-lived process, Arc<Mutex> client slot,
/// AtomicBool availability flag, restart policy with backoff.
///
/// Threading invariant: all methods take &self (or &mut self for shutdown).
/// The tokio::sync::Mutex<Option<WorkerClient>> provides interior mutability
/// for async I/O across await points — required because WorkerClient's frame
/// I/O borrows stdin/stdout across await.
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use tracing::{error, info, warn};

use crate::{
    constants::{
        BROWSER_WORKER_PATH, BROWSER_WORKER_RESULT_TIMEOUT_SECS,
        VOICE_PYTHON_EXE, VOICE_WORKER_RESTART_BACKOFF_SECS, VOICE_WORKER_RESTART_MAX_ATTEMPTS,
    },
    voice::{
        protocol::{msg, WorkerType},
        worker_client::WorkerClient,
    },
};

/// Phase 24: `Clone` is derived because all fields are `Arc`-wrapped.
/// `ExecutorHandle` clones this to run actions in background tasks.
#[derive(Clone)]
pub struct BrowserCoordinator {
    // Same Arc<Mutex> pattern as VoiceCoordinator: allows &self usage from
    // execute_and_log (which borrows &ActionEngine) without &mut constraints.
    client:        Arc<tokio::sync::Mutex<Option<WorkerClient>>>,
    is_available:  Arc<AtomicBool>,
    restart_count: Arc<AtomicU32>,
}

impl BrowserCoordinator {
    /// Create in degraded mode — worker slot is empty, is_available=false.
    /// Always succeeds. Caller must call start() to spawn the actual process.
    pub fn new_degraded() -> Self {
        Self {
            client:        Arc::new(tokio::sync::Mutex::new(None)),
            is_available:  Arc::new(AtomicBool::new(false)),
            restart_count: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Spawn browser_worker.py. Sets is_available=true on success.
    /// Called once from ActionEngine::start_browser().
    pub async fn start(&self) {
        match WorkerClient::spawn(WorkerType::Browser, VOICE_PYTHON_EXE, BROWSER_WORKER_PATH).await {
            Ok(client) => {
                *self.client.lock().await = Some(client);
                self.is_available.store(true, Ordering::Relaxed);
                self.restart_count.store(0, Ordering::Relaxed);
                info!("Browser worker started");
            }
            Err(e) => {
                error!(error = %e, "Browser worker failed to start — browser actions degraded");
            }
        }
    }

    #[allow(dead_code)] // used in unit tests; available for future callers (e.g. degraded-mode UI feedback)
    pub fn is_available(&self) -> bool {
        self.is_available.load(Ordering::Relaxed)
    }

    /// True when the browser worker has exceeded restart limits and will not be retried.
    /// The orchestrator uses this to surface a one-time TextResponse to the UI.
    pub fn is_permanently_degraded(&self) -> bool {
        self.restart_count.load(Ordering::Relaxed) >= VOICE_WORKER_RESTART_MAX_ATTEMPTS
    }

    /// Send a browser command frame and await MSG_BROWSER_RESULT within timeout.
    ///
    /// Returns the JSON payload of the BROWSER_RESULT frame as a String.
    /// Returns Err if the worker is unavailable, the frame write fails, or timeout fires.
    ///
    /// Holds the tokio::sync::Mutex across all await points for this call —
    /// this is intentional and safe: browser commands are sequential per-session.
    pub async fn execute(
        &self,
        msg_type: u8,
        payload:  &[u8],
    ) -> Result<String, crate::voice::worker_client::WorkerError> {
        if !self.is_available.load(Ordering::Relaxed) {
            return Err(crate::voice::worker_client::WorkerError::Io(
                std::io::Error::new(std::io::ErrorKind::NotConnected, "browser worker unavailable")
            ));
        }

        // Run write+read inside an inner block so the lock guard is released
        // before we (potentially) re-acquire it on the timeout path below.
        let read_result = {
            let mut guard = self.client.lock().await;
            let client = guard.as_mut().ok_or_else(|| {
                crate::voice::worker_client::WorkerError::Io(
                    std::io::Error::new(std::io::ErrorKind::NotConnected, "browser worker slot is None")
                )
            })?;

            client.write_frame(msg_type, payload).await
                .map_err(crate::voice::worker_client::WorkerError::Io)?;

            tokio::time::timeout(
                std::time::Duration::from_secs(BROWSER_WORKER_RESULT_TIMEOUT_SECS),
                async {
                    loop {
                        match client.read_frame().await {
                            Ok(Some((t, data))) if t == msg::BROWSER_RESULT => {
                                return String::from_utf8(data).map_err(|_| {
                                    crate::voice::worker_client::WorkerError::Io(
                                        std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            "non-UTF8 browser result payload",
                                        )
                                    )
                                });
                            }
                            Ok(Some(_)) => continue, // discard non-result frames (e.g. stray HEALTH_PONG)
                            Ok(None) | Err(_) => return Err(
                                crate::voice::worker_client::WorkerError::Io(
                                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "browser worker closed")
                                )
                            ),
                        }
                    }
                }
            ).await
        };
        // Lock guard dropped here.

        match read_result {
            Ok(inner) => inner,
            Err(_elapsed) => {
                // Phase 38 / Codex finding [15]: timed-out browser commands leave
                // a stale request in the worker's queue. The next execute() call
                // would write a new command, then read from a stdout buffer that
                // contains the OLD command's result first — accepting it as the
                // NEW command's result. Dropping the client breaks that chain:
                // the WorkerClient's `kill_on_drop(true)` (Session 1 [25])
                // SIGKILLs the Python worker, the next health check sees it
                // dead, restart_count gates a fresh spawn. Subsequent execute()
                // calls return `NotConnected` (clean error) until the restart
                // completes — better than silent result poisoning.
                warn!(
                    timeout_secs = BROWSER_WORKER_RESULT_TIMEOUT_SECS,
                    "Browser worker command timed out — dropping client to prevent stale-result poisoning",
                );
                self.is_available.store(false, Ordering::Relaxed);
                *self.client.lock().await = None;
                Err(crate::voice::worker_client::WorkerError::HandshakeTimeout)
            }
        }
    }

    /// Send HEALTH_PING; restart if no HEALTH_PONG. Respects restart_count limit.
    ///
    /// Called periodically from CoreOrchestrator via ActionEngine::browser_health_check().
    pub async fn health_check_and_restart(&self) {
        let healthy = {
            let mut guard = self.client.lock().await;
            match guard.as_mut() {
                None         => false,
                Some(client) => client.health_check().await,
            }
        };
        if healthy { return; }

        let count = self.restart_count.fetch_add(1, Ordering::Relaxed);
        if count >= VOICE_WORKER_RESTART_MAX_ATTEMPTS {
            if count == VOICE_WORKER_RESTART_MAX_ATTEMPTS {
                // Log only once — avoid log spam on every tick after max restarts.
                error!("Browser worker reached max restart attempts — browser actions permanently degraded");
            }
            return;
        }

        warn!(restart_count = count + 1, "Browser worker unhealthy — restarting");
        self.is_available.store(false, Ordering::Relaxed);
        *self.client.lock().await = None;

        // Exponential backoff: 1s, 2s, 4s, … (doubles each failed attempt).
        let backoff = VOICE_WORKER_RESTART_BACKOFF_SECS << count;
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;

        self.start().await;
    }

    /// Send SHUTDOWN frame and wait for process exit.
    ///
    /// Phase 38c: no longer called from session shutdown (the browser worker is
    /// shared across sessions). Retained for the future graceful-daemon-shutdown
    /// path. Daemon exit currently relies on `kill_on_drop(true)` (Session 1
    /// [25]) which sends SIGKILL via tokio Child's drop — functionally correct,
    /// just less polite than a clean SHUTDOWN frame.
    #[allow(dead_code)] // Phase 38c: preserved for future graceful-shutdown wiring
    pub async fn shutdown(&mut self) {
        self.is_available.store(false, Ordering::Relaxed);
        let client = self.client.lock().await.take();
        if let Some(c) = client {
            c.shutdown().await;
            info!("Browser worker shut down");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_degraded_is_not_available() {
        let c = BrowserCoordinator::new_degraded();
        assert!(!c.is_available());
    }

    #[test]
    fn new_degraded_restart_count_is_zero() {
        let c = BrowserCoordinator::new_degraded();
        assert_eq!(c.restart_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn browser_arc_clone_points_to_same_allocation() {
        let c = BrowserCoordinator::new_degraded();
        let a = Arc::clone(&c.is_available);
        a.store(true, Ordering::Relaxed);
        assert!(c.is_available());
    }

    #[tokio::test]
    async fn execute_returns_err_when_unavailable() {
        let c = BrowserCoordinator::new_degraded();
        // Worker slot is None and is_available=false — must return Err without panic.
        let result = c.execute(msg::BROWSER_NAVIGATE, b"{}").await;
        assert!(result.is_err());
    }

    #[test]
    fn is_permanently_degraded_false_initially() {
        let c = BrowserCoordinator::new_degraded();
        assert!(!c.is_permanently_degraded());
    }

    #[test]
    fn is_permanently_degraded_true_when_count_at_max() {
        let c = BrowserCoordinator::new_degraded();
        c.restart_count.store(VOICE_WORKER_RESTART_MAX_ATTEMPTS, Ordering::Relaxed);
        assert!(c.is_permanently_degraded());
    }
}
