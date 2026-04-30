/// VoiceCoordinator — TTS worker supervisor for CoreOrchestrator.
///
/// Owns the TTS WorkerClient wrapped in Arc<Mutex<>> so the TTS synthesis
/// task spawned by handle_text_input() can access it concurrently with the
/// inference loop without moving self into the task.
///
/// STT workers are NOT managed here — stream_audio() creates them per-call.

use std::sync::{Arc, atomic::{AtomicBool, AtomicU32, Ordering}};
use tokio::sync::Mutex;
use tracing::{info, warn};
use crate::constants::{
    VOICE_PYTHON_EXE, VOICE_TTS_WORKER_PATH,
    VOICE_WORKER_RESTART_MAX_ATTEMPTS, VOICE_WORKER_RESTART_BACKOFF_SECS,
};
use super::worker_client::WorkerClient;
use super::protocol::WorkerType;

/// Phase 38c: VoiceCoordinator is Clone so that one daemon-lifetime instance can
/// be shared across all gRPC sessions. Pre-Phase-38c, each new session created
/// its own VoiceCoordinator and spawned a fresh kokoro worker. That spawn caused
/// memory pressure (Python process startup + model load) which evicted PRIMARY's
/// mmap'd pages — every reconnect ate a 22-second cold-load on the first chat
/// turn. Sharing means: one TTS worker for the daemon's lifetime, no per-session
/// spawn, no eviction trigger.
///
/// All fields are now Arc-wrapped (atomic counters for `restarts` /
/// `permanently_degraded`) so cloning produces independent handles to the same
/// underlying state. Lifecycle methods (`start_tts`, `health_check_and_restart`,
/// `shutdown`) take `&self` and use interior mutation — the daemon-startup
/// supervisor task is the only caller that mutates lifecycle state.
#[derive(Clone)]
pub struct VoiceCoordinator {
    tts:                  Arc<Mutex<Option<WorkerClient>>>,
    tts_ready:            Arc<AtomicBool>,   // lock-free availability check
    python_exe:           String,
    tts_script:           String,
    restarts:             Arc<AtomicU32>,    // Phase 38c: Arc-wrapped for cross-session sharing
    permanently_degraded: Arc<AtomicBool>,   // Phase 38c: Arc-wrapped — set once, read everywhere
}

impl VoiceCoordinator {
    /// Synchronous constructor — no subprocess spawned. Always succeeds.
    /// Call start_tts().await afterwards to attempt worker spawn.
    pub fn new_degraded() -> Self {
        Self {
            tts:                  Arc::new(Mutex::new(None)),
            tts_ready:            Arc::new(AtomicBool::new(false)),
            python_exe:           VOICE_PYTHON_EXE.to_string(),
            tts_script:           VOICE_TTS_WORKER_PATH.to_string(),
            restarts:             Arc::new(AtomicU32::new(0)),
            permanently_degraded: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Attempt to spawn the TTS worker. Updates tts_ready. Non-fatal.
    ///
    /// Phase 38c: takes `&self` (was `&mut self`) — `restarts` is now an
    /// `Arc<AtomicU32>` so the reset uses interior mutation. Allows the daemon-
    /// startup task to call this without owning the coordinator exclusively.
    pub async fn start_tts(&self) {
        match WorkerClient::spawn(WorkerType::Tts, &self.python_exe, &self.tts_script).await {
            Ok(client) => {
                *self.tts.lock().await = Some(client);
                self.tts_ready.store(true, Ordering::Relaxed);
                // Phase 38 / Codex finding [38]: reset the consecutive-restart
                // counter on a successful spawn. The constant doc says
                // `VOICE_WORKER_RESTART_MAX_ATTEMPTS` counts CONSECUTIVE failures,
                // but without this reset the counter persists across recoveries —
                // three NON-consecutive crashes over the lifetime of the daemon
                // would permanently degrade voice. Resetting here matches the
                // documented semantics.
                self.restarts.store(0, Ordering::Relaxed);
                info!("TTS worker ready");
            }
            Err(e) => {
                warn!(error = %e, "TTS worker spawn failed — voice degraded (text-only)");
            }
        }
    }

    /// True if the TTS worker is alive and ready. Lock-free.
    pub fn is_tts_available(&self) -> bool {
        self.tts_ready.load(Ordering::Relaxed)
    }

    /// True when the worker has exceeded restart limits and will not be retried.
    /// The orchestrator uses this to surface a one-time TextResponse to the UI.
    ///
    /// Phase 38c: reads `Arc<AtomicBool>` instead of bare `bool` so all session
    /// clones see the same lifecycle state (set once by the daemon-startup
    /// supervisor when restart limits are exceeded).
    pub fn is_permanently_degraded(&self) -> bool {
        self.permanently_degraded.load(Ordering::Relaxed)
    }

    /// Clone of the Arc<Mutex<>> for use in spawned TTS synthesis tasks.
    pub fn tts_arc(&self) -> Arc<Mutex<Option<WorkerClient>>> {
        self.tts.clone()
    }

    /// Health-check the TTS worker; restart on failure up to the configured limit.
    /// Called by `CoreOrchestrator::voice_health_check()` on a periodic timer (Phase 13).
    ///
    /// Phase 38c: takes `&self` (was `&mut self`) — `restarts` and
    /// `permanently_degraded` are interior-mutable atomics. The daemon-startup
    /// supervisor task is the canonical caller; per-session orchestrators may
    /// also call it (the timer fires per session today, but operations are
    /// idempotent so concurrent calls just observe the same atomic state).
    pub async fn health_check_and_restart(&self) {
        let alive = {
            let mut guard = self.tts.lock().await;
            match guard.as_mut() {
                Some(c) => c.health_check().await,
                None    => false,
            }
        };
        let restarts_now = self.restarts.load(Ordering::Relaxed);
        if !alive && restarts_now < VOICE_WORKER_RESTART_MAX_ATTEMPTS {
            let delay = VOICE_WORKER_RESTART_BACKOFF_SECS << restarts_now;  // exponential
            warn!(
                attempt    = restarts_now + 1,
                delay_secs = delay,
                "TTS worker dead — restarting"
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            self.tts_ready.store(false, Ordering::Relaxed);
            *self.tts.lock().await = None;
            self.restarts.fetch_add(1, Ordering::Relaxed);
            self.start_tts().await;
        } else if !alive && !self.permanently_degraded.load(Ordering::Relaxed) {
            // Transition into permanent degradation — log exactly once via
            // compare-and-swap so concurrent callers don't double-log.
            if self.permanently_degraded
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                warn!(
                    max_attempts = VOICE_WORKER_RESTART_MAX_ATTEMPTS,
                    "TTS worker permanently unavailable — entering text-only mode"
                );
            }
            self.tts_ready.store(false, Ordering::Relaxed);
            *self.tts.lock().await = None;
        }
        // If permanently_degraded is already true, subsequent ticks are silent no-ops.
    }

    /// Send SHUTDOWN to the TTS worker and drop the client.
    ///
    /// Phase 38c: takes `&self` (was `self`) so the daemon-level supervisor can
    /// call shutdown without owning the coordinator exclusively. Idempotent —
    /// `Option::take()` returns None on second call, so repeated invocations
    /// are no-ops.
    ///
    /// Currently unused: daemon shutdown relies on `kill_on_drop(true)` from
    /// Session 1. Retained for the future graceful-shutdown path.
    #[allow(dead_code)] // Phase 38c: preserved for future graceful-shutdown wiring
    pub async fn shutdown(&self) {
        if let Some(client) = self.tts.lock().await.take() {
            client.shutdown().await;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_degraded_is_not_available() {
        let vc = VoiceCoordinator::new_degraded();
        assert!(!vc.is_tts_available(), "Degraded coordinator must report unavailable");
    }

    #[test]
    fn new_degraded_restart_count_is_zero() {
        let vc = VoiceCoordinator::new_degraded();
        assert_eq!(vc.restarts.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tts_arc_clone_points_to_same_allocation() {
        let vc = VoiceCoordinator::new_degraded();
        let arc1 = vc.tts_arc();
        let arc2 = vc.tts_arc();
        assert!(Arc::ptr_eq(&arc1, &arc2), "Both Arc clones must point to the same allocation");
    }

    #[test]
    fn is_tts_available_reflects_atomic_state() {
        let vc = VoiceCoordinator::new_degraded();
        assert!(!vc.is_tts_available());
        vc.tts_ready.store(true, Ordering::Relaxed);
        assert!(vc.is_tts_available());
        vc.tts_ready.store(false, Ordering::Relaxed);
        assert!(!vc.is_tts_available());
    }

    #[test]
    fn is_permanently_degraded_false_initially() {
        let vc = VoiceCoordinator::new_degraded();
        assert!(!vc.is_permanently_degraded());
    }

    #[test]
    fn is_permanently_degraded_field_set_directly() {
        let vc = VoiceCoordinator::new_degraded();
        vc.permanently_degraded.store(true, Ordering::Relaxed);
        assert!(vc.is_permanently_degraded());
    }

    #[test]
    fn voice_coordinator_clone_shares_state() {
        // Phase 38c regression guard: cloning must produce independent handles
        // to the same underlying state — sessions using the same daemon-level
        // VoiceCoordinator must observe each other's lifecycle changes.
        let vc = VoiceCoordinator::new_degraded();
        let vc2 = vc.clone();

        assert!(!vc.is_tts_available());
        assert!(!vc2.is_tts_available());

        // Mutate via vc; observe via vc2.
        vc.tts_ready.store(true, Ordering::Relaxed);
        assert!(vc2.is_tts_available(),
                "clone must observe tts_ready changes via shared Arc");

        vc.permanently_degraded.store(true, Ordering::Relaxed);
        assert!(vc2.is_permanently_degraded(),
                "clone must observe permanently_degraded changes via shared Arc");

        vc.restarts.store(7, Ordering::Relaxed);
        assert_eq!(vc2.restarts.load(Ordering::Relaxed), 7,
                   "clone must observe restart count via shared Arc");
    }

    #[test]
    fn voice_coordinator_tts_arc_shares_across_clones() {
        // Phase 38c: the inner tts: Arc<Mutex<Option<WorkerClient>>> must be
        // shared between clones — that's the whole mechanism by which sessions
        // talk to the same TTS worker. tts_arc() returns clones of the same Arc.
        let vc = VoiceCoordinator::new_degraded();
        let vc2 = vc.clone();
        let arc1 = vc.tts_arc();
        let arc2 = vc2.tts_arc();
        assert!(Arc::ptr_eq(&arc1, &arc2),
                "tts_arc() across clones must point to the same allocation");
    }
}
