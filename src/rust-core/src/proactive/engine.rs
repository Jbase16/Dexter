/// ProactiveEngine — rate-limited proactive observation initiator.
///
/// Dexter is not a chatbot that only responds when asked. This module governs
/// *when* Dexter initiates a proactive ambient observation based on context changes
/// — without being asked. It is the mechanism that distinguishes a persistent AI
/// entity from a passive question-answering tool.
///
/// ## Design
///
/// The engine tracks when the last proactive response was fired and applies two
/// rate-limiting gates:
/// 1. **Startup grace period**: Dexter stays silent for the first N seconds of a
///    session to let the operator settle in. Default: 30 seconds.
/// 2. **Minimum interval**: After firing, Dexter will not fire again for at least N
///    seconds. Default: 90 seconds. Configurable via `[behavior].proactive_interval_secs`.
///
/// ## [SILENT] opt-out
///
/// The proactive prompt instructs the model to respond with exactly `[SILENT]` if
/// it has nothing useful to add. This is a model-level opt-out that the orchestrator
/// checks after collecting the full response — before any tokens are sent to the UI
/// or TTS. The operator never sees "[SILENT]" on screen.
///
/// ## Conversation history
///
/// Proactive responses are **ephemeral** — they are not added to conversation history
/// or session state. They are ambient observations, like someone glancing at your
/// screen. The operator can always reference what Dexter said by speaking; the model
/// will reconstruct based on current context.
use std::time::{Duration, Instant};

use crate::{
    config::BehaviorConfig,
    constants::PROACTIVE_USER_ACTIVE_WINDOW_SECS,
    context_observer::ContextSnapshot,
};

/// Rate-limited proactive response governor.
///
/// Owned by `CoreOrchestrator`. Updated via `record_fire()` after each successful
/// proactive response. `should_fire()` is the primary gate checked in
/// `handle_system_event()` on `AppFocused` context changes.
pub struct ProactiveEngine {
    enabled:              bool,
    min_interval_secs:    u64,
    startup_grace_secs:   u64,
    last_fired_at:        Option<Instant>,
    session_started_at:   Instant,
    /// Phase 18 — Gate 6: bundle IDs exempt from proactive observations.
    /// Set from `BehaviorConfig.proactive_excluded_bundles` at construction time.
    excluded_bundles:     Vec<String>,
    /// Phase 36 — Gate 7: instant of the operator's most recent turn (voice or typed).
    ///
    /// Updated by `CoreOrchestrator::handle_text_input` via `record_user_turn()`.
    /// When set and within `PROACTIVE_USER_ACTIVE_WINDOW_SECS` of now, Gate 7 blocks
    /// all proactive fires — an app-focus-driven ambient comment dropped between a
    /// user turn and Dexter's response reads as noise during active dialogue.
    last_user_turn_at:    Option<Instant>,
}

impl ProactiveEngine {
    /// Create a new engine from the operator's behavior config.
    ///
    /// `session_started_at` is set to `Instant::now()` so the startup grace
    /// period begins counting from the moment the session opens.
    pub fn new(cfg: &BehaviorConfig) -> Self {
        Self {
            enabled:            cfg.proactive_enabled,
            min_interval_secs:  cfg.proactive_interval_secs,
            startup_grace_secs: cfg.proactive_startup_grace_secs,
            last_fired_at:      None,
            session_started_at: Instant::now(),
            excluded_bundles:   cfg.proactive_excluded_bundles.clone(),
            last_user_turn_at:  None,
        }
    }

    /// Create a new engine whose session start is backdated by `started_ago_secs`.
    ///
    /// Only available in `#[cfg(test)]` — allows unit tests to bypass the startup
    /// grace period without sleeping. Production code always calls `new()`.
    #[cfg(test)]
    pub fn new_backdated(cfg: &BehaviorConfig, started_ago_secs: u64) -> Self {
        Self {
            enabled:            cfg.proactive_enabled,
            min_interval_secs:  cfg.proactive_interval_secs,
            startup_grace_secs: cfg.proactive_startup_grace_secs,
            last_fired_at:      None,
            session_started_at: Instant::now() - Duration::from_secs(started_ago_secs),
            excluded_bundles:   cfg.proactive_excluded_bundles.clone(),
            last_user_turn_at:  None,
        }
    }

    /// Returns `true` if a proactive response should be fired for this context snapshot.
    ///
    /// Gates (all must pass):
    ///   1. `enabled` is true in config.
    ///   2. Screen is not locked.
    ///   3. An app is focused (context is non-trivial).
    ///   4. Startup grace period has elapsed (`startup_grace_secs` since session open).
    ///   5. Minimum interval has elapsed since the last fire (`min_interval_secs`).
    ///   6. (Phase 18) App bundle ID is not in the excluded list.
    ///   7. (Phase 36) Operator has not interacted within `PROACTIVE_USER_ACTIVE_WINDOW_SECS`.
    pub fn should_fire(&self, snapshot: &ContextSnapshot) -> bool {
        if !self.enabled {
            return false;
        }
        if snapshot.is_screen_locked {
            return false;
        }
        if snapshot.app_name.is_none() {
            return false;
        }

        // Gate 6: [Phase 18] per-bundle exclusion list.
        //
        // Bundle IDs are locale-invariant stable identifiers; app names are not.
        // Inserted after Gate 3 (a non-trivial context is confirmed) and before
        // Gate 4 (startup grace) to short-circuit before the elapsed() syscalls.
        //
        // If app_bundle_id is absent (should not occur after Gate 3, but be
        // defensive), allow through — we cannot match against an unknown bundle.
        if let Some(ref bundle_id) = snapshot.app_bundle_id {
            if self.excluded_bundles.iter().any(|ex| ex == bundle_id) {
                return false;
            }
        }

        // Gate 4: startup grace period.
        if self.session_started_at.elapsed() < Duration::from_secs(self.startup_grace_secs) {
            return false;
        }

        // Gate 5: minimum interval since last fire.
        if let Some(last) = self.last_fired_at {
            if last.elapsed() < Duration::from_secs(self.min_interval_secs) {
                return false;
            }
        }

        // Gate 7: [Phase 36] recent-user-activity suppression.
        //
        // During an active exchange the operator is engaged WITH Dexter — an app-
        // focus-driven ambient comment ("It's 2:15 PM") between the operator's turn
        // and Dexter's response reads as non-sequitur. Applied AFTER Gates 4+5 so
        // the costlier timestamp check runs only when other gates have already passed.
        if let Some(last) = self.last_user_turn_at {
            if last.elapsed() < Duration::from_secs(PROACTIVE_USER_ACTIVE_WINDOW_SECS) {
                return false;
            }
        }

        true
    }

    /// Record that the operator just spoke or typed a message.
    ///
    /// Called from `CoreOrchestrator::handle_text_input` at the top of user-turn
    /// handling.  Resets Gate 7's activity clock so proactive observations are
    /// suppressed for the next `PROACTIVE_USER_ACTIVE_WINDOW_SECS` seconds.
    pub fn record_user_turn(&mut self) {
        self.last_user_turn_at = Some(Instant::now());
    }

    /// Record that a proactive response was just fired.
    ///
    /// Resets the minimum-interval clock. Called BEFORE generation so that a
    /// failed inference (Ollama unreachable, model missing) still burns the slot —
    /// preventing rapid re-fire loops on connectivity issues.
    ///
    /// If the model returns `[SILENT]` (a conscious model-level opt-out), the
    /// caller must call `undo_fire()` to refund the slot. The distinction:
    /// - Inference error → slot stays burned (prevents retry storms)
    /// - Model chose `[SILENT]` → slot refunded (nothing happened from the
    ///   operator's perspective; the rate-limit should not apply)
    pub fn record_fire(&mut self) {
        self.last_fired_at = Some(Instant::now());
    }

    /// Refund the rate-limit slot consumed by `record_fire()`.
    ///
    /// Called when the model returns `[SILENT]` — the model consciously decided
    /// it had nothing useful to say. From the operator's perspective, nothing
    /// happened. Penalising the operator's 90-second budget for the model's
    /// deliberate silence is incorrect.
    ///
    /// NOT called on inference errors (Ollama unreachable, timeout) — those
    /// intentionally burn the slot to prevent rapid re-fire loops.
    ///
    /// ## Behavioral consequence: no backoff on consecutive silences
    ///
    /// This sets `last_fired_at = None`, which means Gate 5 passes immediately
    /// on the next `AppFocused` event. Proactive fires on app-switch events (not
    /// on a timer), so this does NOT create a timer-based polling loop. However,
    /// an operator who frequently switches between apps where the model consistently
    /// returns `[SILENT]` will see Ollama called on every unique app switch —
    /// bounded only by the operator's app-switching frequency and the model's
    /// response time (~1–3s for FAST).
    ///
    /// This is an explicit architectural choice: an operator who switches to a
    /// password manager and back should get a fresh proactive attempt on the next
    /// switch, not a 90-second penalty for the model's silence. The expected case
    /// is that consecutive silences are rare (the model has something to say about
    /// *some* apps the operator uses).
    ///
    /// If consecutive-silent backoff becomes necessary in practice, the fix is
    /// minimal: track `consecutive_silent_count: u32` and call a partial-burn
    /// variant — `last_fired_at = Some(Instant::now() - Duration::from_secs(
    /// min_interval_secs - silent_backoff_secs))` — rather than full reset. This
    /// adds Gate 5 expiry in `silent_backoff_secs` without adding a config option.
    pub fn undo_fire(&mut self) {
        self.last_fired_at = None;
    }

    /// Build the messages list for a proactive observation request.
    ///
    /// The caller applies personality via `PersonalityLayer::apply_to_messages()`.
    /// This method produces the proactive user-turn prompt that follows personality
    /// + context injection.
    ///
    /// ## Phase 37.9: inverted default
    ///
    /// Previously this prompt framed `[SILENT]` as an opt-out — "give a brief
    /// observation OR say [SILENT]". In practice the FAST model (qwen3:8b) read
    /// that as "observation is the default" and filled dead context windows with
    /// safe/generic content, most commonly the current time or date. Operators
    /// reported: *"He keeps randomly opening up the HUD and speaking the date and
    /// time. unprompted."*
    ///
    /// The inversion: `[SILENT]` is now the default; an observation requires a
    /// concrete, named trigger (a specific problem visible on screen, a change
    /// worth flagging, an actionable next step). Bare time/date/day-of-week
    /// announcements are explicitly forbidden — they almost never contain
    /// information the operator doesn't already have, and they fire the HUD for
    /// no reason.
    ///
    /// A regex-based post-filter (`is_low_value_response`) catches cases where
    /// the model emits a bare time/date anyway. Those get demoted to `[SILENT]`
    /// and refund the rate-limit slot, so the operator never sees them.
    pub fn build_proactive_prompt(context_summary: &str) -> String {
        format!(
            "[Proactive] Current context: {}.\n\
             \n\
             DEFAULT: respond with exactly [SILENT].\n\
             \n\
             Only break silence if the operator's visible activity reveals a \
             specific, named hook you can help with — a concrete error you can \
             see, a change in what they're doing that's worth naming, or a \
             precise next action. If nothing specific comes to mind, respond \
             [SILENT]. Silence is the correct answer most of the time.\n\
             \n\
             FORBIDDEN outputs (always respond [SILENT] instead):\n\
             - The current time, date, or day of the week.\n\
             - Weather summaries unless the operator is clearly planning travel/outdoor activity.\n\
             - Generic 'you are working in <app>' restatements with no added insight.\n\
             - Greetings, check-ins, or 'how can I help?' prompts.\n\
             \n\
             If you do speak: one brief sentence, grounded in a specific thing visible on screen.",
            context_summary
        )
    }

    /// Returns `true` if the model's response is bare low-value filler that
    /// should be suppressed even though it isn't literal `[SILENT]`.
    ///
    /// ## Why this exists
    ///
    /// Prompt inversion (`build_proactive_prompt`) reduces but does not eliminate
    /// bare time/date proactive outputs. Small models occasionally ignore the
    /// `FORBIDDEN` list and emit "It's 3:42 PM" or "Today is Tuesday, April 22"
    /// anyway. This function is the deterministic backstop: any response that
    /// matches a low-value pattern is treated exactly like `[SILENT]` — the
    /// orchestrator refunds the rate-limit slot and suppresses UI output.
    ///
    /// ## Patterns caught
    ///
    /// - Bare clock: "3:42 PM", "It's 3:42", "The time is 3:42 PM", "15:42"
    /// - Bare date: "April 22, 2026", "It's April 22", "Today is April 22"
    /// - Bare day: "Today is Tuesday", "It's Tuesday"
    /// - Combined: "It's Tuesday, April 22 at 3:42 PM"
    ///
    /// A response containing any of the above AS WELL AS substantive content
    /// (e.g. "You just hit 3pm — still on track for the 4pm deploy window")
    /// is NOT filtered; the heuristic requires the low-value pattern to account
    /// for essentially the entire response (length-gated).
    pub fn is_low_value_response(response: &str) -> bool {
        let trimmed = response.trim();
        if trimmed.is_empty() {
            return false; // handled by is_silent_response
        }

        // Length gate: substantive responses are never filtered, even if they
        // happen to mention the time. A bare time/date announcement from a
        // small model is almost always short — under ~80 chars.
        if trimmed.chars().count() > 80 {
            return false;
        }

        let lower = trimmed.to_ascii_lowercase();

        // Strip common non-alphanumeric punctuation at ends so "3:42 PM." and
        // "It's 3:42 PM!" land on the same pattern.
        let stripped = lower.trim_matches(|c: char| !c.is_alphanumeric());

        // 1. Bare clock: optional "it's" / "the time is" / "currently" prefix
        //    + H(H):MM + optional AM/PM, OR 24-hour HH:MM.
        //    Also catches "it is 3 pm", "currently 3pm".
        let clock_patterns: &[&str] = &[
            "it's ", "it is ", "the time is ", "time is ", "currently ",
            "right now it's ", "right now the time is ",
        ];
        let has_clock_lead = clock_patterns.iter().any(|p| stripped.starts_with(p))
            || stripped
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false);
        let has_clocky_token = stripped
            .split_whitespace()
            .any(|w| is_clocky_token(w));
        if has_clock_lead && has_clocky_token && !has_substantive_verb(&lower) {
            return true;
        }

        // 2. Bare date / day announcements.
        let date_leads: &[&str] = &[
            "today is ",
            "it's ",
            "it is ",
            "the date is ",
            "the day is ",
        ];
        let months: &[&str] = &[
            "january", "february", "march", "april", "may", "june", "july",
            "august", "september", "october", "november", "december",
            "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept",
            "oct", "nov", "dec",
        ];
        let days: &[&str] = &[
            "monday", "tuesday", "wednesday", "thursday", "friday",
            "saturday", "sunday",
        ];
        let has_date_lead = date_leads.iter().any(|p| stripped.starts_with(p));
        let mentions_month = months.iter().any(|m| {
            // Whole-word match: "apr" must not match "april" inside longer text
            // — split on whitespace/punctuation and compare the token directly.
            stripped
                .split(|c: char| !c.is_alphabetic())
                .any(|tok| tok == *m)
        });
        let mentions_day = days.iter().any(|d| {
            stripped
                .split(|c: char| !c.is_alphabetic())
                .any(|tok| tok == *d)
        });
        if has_date_lead && (mentions_month || mentions_day) && !has_substantive_verb(&lower) {
            return true;
        }
        // "Today is Tuesday" / "It's Wednesday" with no other verb is low-value
        // even without an explicit date lead if the response is essentially just
        // the day name.
        if (mentions_day || mentions_month) && trimmed.chars().count() < 35 && !has_substantive_verb(&lower) {
            return true;
        }

        false
    }

    /// Returns `true` if the response is the silence opt-out OR a low-value
    /// proactive output that should be treated as silence. Callers that want
    /// both behaviors should use this aggregate rather than chaining checks.
    pub fn should_suppress_proactive(response: &str) -> bool {
        Self::is_silent_response(response) || Self::is_low_value_response(response)
    }

    /// Build the user-turn prompt for a shell command failure proactive observation.
    ///
    /// Used by CoreOrchestrator when a command exits non-zero and all rate-limit gates
    /// pass. Produces a failure-specific, actionable prompt — as opposed to
    /// `build_proactive_prompt` which requests a general ambient observation.
    ///
    /// The model also receives `[Context:]` and `[Shell:]` system messages before this
    /// user turn, giving it: what the operator is doing right now AND what just failed.
    pub fn build_shell_error_prompt(command: &str, exit_code: i32, cwd: &str) -> String {
        format!(
            "[Proactive] The operator just ran `{}` in {} and it failed with exit code {}. \
             If you can identify the likely cause or suggest a fix, give one brief targeted \
             observation. Otherwise respond with exactly [SILENT].",
            command, cwd, exit_code
        )
    }

    /// Returns `true` if the model's response is the silence opt-out.
    ///
    /// Checks trimmed response against `[SILENT]` (case-insensitive) and empty
    /// string. Used by the orchestrator to suppress proactive output before
    /// any tokens are sent to the UI.
    pub fn is_silent_response(response: &str) -> bool {
        let t = response.trim();
        t.is_empty() || t.eq_ignore_ascii_case("[silent]")
    }
}

/// Detects whether a whitespace-split token looks like a clock reading.
///
/// Accepts:
/// - `3:42`, `3:42pm`, `3:42am`, `3:42:08`
/// - `15:42`, `15:42:08`
/// - `3pm`, `3am`, `3p.m.`, `3a.m.`
///
/// This deliberately does NOT require a colon — "3pm" and "3 pm" are both
/// common model outputs when announcing the time.
fn is_clocky_token(token: &str) -> bool {
    // Strip common trailing punctuation on a single token ("3:42," / "pm.").
    let t = token.trim_matches(|c: char| c == ',' || c == '.' || c == '!' || c == '?');
    if t.is_empty() {
        return false;
    }
    // AM/PM markers on their own.
    let lower = t.to_ascii_lowercase();
    if matches!(lower.as_str(), "am" | "pm" | "a.m" | "p.m" | "a.m." | "p.m.") {
        return true;
    }
    // "3pm" / "3am" — digit(s) followed by am/pm.
    if let Some(idx) = lower.find(|c: char| c.is_ascii_alphabetic()) {
        let (nums, suffix) = lower.split_at(idx);
        if !nums.is_empty()
            && nums.chars().all(|c| c.is_ascii_digit())
            && matches!(suffix, "am" | "pm" | "a.m" | "p.m" | "a.m." | "p.m.")
        {
            return true;
        }
    }
    // "3:42" or "15:42" or "3:42:08" — digits separated by colons.
    if lower.contains(':') {
        let segments: Vec<&str> = lower.split(':').collect();
        if (2..=3).contains(&segments.len())
            && segments
                .iter()
                .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        {
            return true;
        }
        // "3:42pm" — digits:digits + am/pm suffix on the last segment.
        if segments.len() == 2 {
            let a = segments[0];
            let b = segments[1];
            if !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()) {
                if let Some(idx) = b.find(|c: char| c.is_ascii_alphabetic()) {
                    let (bn, bs) = b.split_at(idx);
                    if !bn.is_empty()
                        && bn.chars().all(|c| c.is_ascii_digit())
                        && matches!(bs, "am" | "pm" | "a.m" | "p.m" | "a.m." | "p.m.")
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Returns `true` if the response contains a verb that implies substantive
/// content (an observation, a suggestion, a question) rather than a bare
/// time/date announcement.
///
/// This is intentionally narrow — we only want to AVOID demoting responses
/// that are clearly more than a clock readout. A bare "It's 3:42 PM" has no
/// such verb and SHOULD be demoted; "You just hit 3pm — still on track for
/// the 4pm deploy window" contains "hit" / "track" and must survive.
///
/// The list is keyword-based (no full POS tagging); false negatives here are
/// safer than false positives — a missed verb means we demote a response that
/// might have been real, costing at most one ambient observation. A false
/// positive means a bare date/time leaks through.
fn has_substantive_verb(lower: &str) -> bool {
    // Split on non-alphabetic chars so "hit," and "hit" both tokenize to "hit".
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphabetic())
        .filter(|s| !s.is_empty())
        .collect();

    const SUBSTANTIVE_VERBS: &[&str] = &[
        "working", "debugging", "writing", "editing", "reading", "running",
        "building", "testing", "deploying", "fixing", "refactoring", "reviewing",
        "noticed", "saw", "see", "seeing", "spotted", "looks", "looking",
        "seems", "appears", "suggest", "suggests", "try", "consider",
        "check", "verify", "confirm", "note", "remember", "watch",
        "hit", "track", "tracking", "missed", "passed", "approaching",
        "remind", "reminded", "reminder", "schedule", "scheduled", "due",
        "still", "already", "just", "about", "close",
        "error", "errors", "failed", "failing", "broken", "crash", "crashed",
    ];

    tokens.iter().any(|t| SUBSTANTIVE_VERBS.contains(t))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BehaviorConfig;

    fn default_behavior_cfg() -> BehaviorConfig {
        BehaviorConfig::default()
    }

    fn unlocked_snapshot_with_app() -> ContextSnapshot {
        use crate::context_observer::ContextSnapshot;
        ContextSnapshot {
            app_bundle_id:      Some("com.apple.Xcode".to_string()),
            app_name:           Some("Xcode".to_string()),
            focused_element:    None,
            is_screen_locked:   false,
            clipboard_text:     None,
            clipboard_changed_at: None,
            last_shell_command: None,
            snapshot_hash:      1,
            last_updated:       chrono::Utc::now(),
        }
    }

    // ── Gate 1: enabled flag ─────────────────────────────────────────────────

    #[test]
    fn proactive_engine_disabled_never_fires() {
        let cfg = BehaviorConfig {
            proactive_enabled: false,
            ..BehaviorConfig::default()
        };
        // Backdate past both grace and interval so only the enabled flag blocks.
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "disabled engine must never fire"
        );
    }

    // ── Gate 2: screen locked ────────────────────────────────────────────────

    #[test]
    fn proactive_engine_does_not_fire_when_screen_locked() {
        let cfg = default_behavior_cfg();
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        let mut snap = unlocked_snapshot_with_app();
        snap.is_screen_locked = true;
        assert!(
            !engine.should_fire(&snap),
            "must not fire when screen is locked"
        );
    }

    // ── Gate 3: app name ─────────────────────────────────────────────────────

    #[test]
    fn proactive_engine_does_not_fire_with_no_app() {
        let cfg = default_behavior_cfg();
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        let mut snap = unlocked_snapshot_with_app();
        snap.app_name = None;
        assert!(
            !engine.should_fire(&snap),
            "must not fire when no app is focused"
        );
    }

    // ── Gate 4: startup grace ────────────────────────────────────────────────

    #[test]
    fn proactive_engine_does_not_fire_during_startup_grace() {
        let cfg = BehaviorConfig {
            proactive_startup_grace_secs: 60,
            ..BehaviorConfig::default()
        };
        // Session started only 5 seconds ago — within the 60s grace period.
        let engine = ProactiveEngine::new_backdated(&cfg, 5);
        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "must not fire during startup grace period"
        );
    }

    // ── Gate 5: minimum interval ─────────────────────────────────────────────

    #[test]
    fn proactive_engine_does_not_fire_before_min_interval() {
        let cfg = BehaviorConfig {
            proactive_interval_secs: 90,
            ..BehaviorConfig::default()
        };
        let mut engine = ProactiveEngine::new_backdated(&cfg, 200);
        // Simulate a fire that happened 10 seconds ago — within the 90s window.
        engine.last_fired_at = Some(Instant::now() - Duration::from_secs(10));
        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "must not fire within min_interval_secs of last fire"
        );
    }

    // ── Happy path ───────────────────────────────────────────────────────────

    #[test]
    fn proactive_engine_fires_when_all_gates_pass() {
        let cfg = BehaviorConfig {
            proactive_enabled:          true,
            proactive_interval_secs:    90,
            proactive_startup_grace_secs: 30,
            proactive_excluded_bundles: vec![],
            ..BehaviorConfig::default()
        };
        // Backdate past both grace and interval.
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "all gates clear — engine must fire"
        );
    }

    // ── record_fire ──────────────────────────────────────────────────────────

    #[test]
    fn proactive_engine_record_fire_blocks_immediate_repeat() {
        let cfg = default_behavior_cfg();
        let mut engine = ProactiveEngine::new_backdated(&cfg, 200);
        assert!(engine.should_fire(&unlocked_snapshot_with_app()), "pre-condition");
        engine.record_fire();
        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "record_fire must prevent immediate re-fire"
        );
    }

    // ── is_silent_response ───────────────────────────────────────────────────

    #[test]
    fn is_silent_response_detects_variants() {
        assert!(ProactiveEngine::is_silent_response("[SILENT]"));
        assert!(ProactiveEngine::is_silent_response("[silent]"));
        assert!(ProactiveEngine::is_silent_response("  [Silent]  "));
        assert!(ProactiveEngine::is_silent_response(""));
        assert!(ProactiveEngine::is_silent_response("   "));
    }

    #[test]
    fn is_silent_response_does_not_suppress_real_responses() {
        assert!(!ProactiveEngine::is_silent_response("You're in Xcode — working on the metal shader."));
        assert!(!ProactiveEngine::is_silent_response("Looks like you're debugging a concurrency issue."));
    }

    // ── undo_fire ────────────────────────────────────────────────────────────

    #[test]
    fn proactive_engine_undo_fire_restores_firing() {
        // After record_fire(), the engine should be blocked by the rate limit.
        // After undo_fire(), it should be allowed to fire again immediately —
        // as if record_fire() was never called.
        let cfg = default_behavior_cfg();
        let mut engine = ProactiveEngine::new_backdated(&cfg, 200);

        // Verify pre-condition: all gates clear.
        assert!(engine.should_fire(&unlocked_snapshot_with_app()), "pre-condition: should fire");

        // Burn the slot.
        engine.record_fire();
        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "record_fire must block immediate re-fire"
        );

        // Refund the slot (model returned [SILENT]).
        engine.undo_fire();
        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "undo_fire must restore firing eligibility"
        );
    }

    // ── Gate 6: per-bundle exclusion list (Phase 18) ─────────────────────────

    #[test]
    fn proactive_engine_excluded_bundle_does_not_fire() {
        let cfg = BehaviorConfig {
            proactive_excluded_bundles: vec![
                "com.agilebits.onepassword-osx".to_string(),
                "com.apple.dt.Xcode".to_string(),
            ],
            ..BehaviorConfig::default()
        };
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        // The snapshot fixture uses "com.apple.Xcode" as the bundle ID.
        // Gate 6 checks for an exact match, so this bundle is not in the exclusion list.
        // But let's also test with a matching bundle:
        let mut snap = unlocked_snapshot_with_app();
        snap.app_bundle_id = Some("com.apple.dt.Xcode".to_string());
        assert!(
            !engine.should_fire(&snap),
            "Gate 6: bundle in exclusion list must prevent firing"
        );
    }

    #[test]
    fn proactive_engine_non_excluded_bundle_fires() {
        let cfg = BehaviorConfig {
            proactive_excluded_bundles: vec!["com.agilebits.onepassword-osx".to_string()],
            ..BehaviorConfig::default()
        };
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        // Fixture uses "com.apple.Xcode" — not in the exclusion list.
        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "Gate 6: bundle NOT in exclusion list — all other gates clear — must fire"
        );
    }

    #[test]
    fn proactive_engine_empty_exclusion_list_fires_all_bundles() {
        // Default config has proactive_excluded_bundles = [] (Phase 17 parity).
        let cfg = BehaviorConfig::default();
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "Empty exclusion list must never block proactive firing (Phase 17 parity)"
        );
    }

    // ── build_proactive_prompt ───────────────────────────────────────────────

    #[test]
    fn build_proactive_prompt_contains_context_summary() {
        let prompt = ProactiveEngine::build_proactive_prompt("Xcode — Source Editor: func parseVmStat");
        assert!(
            prompt.contains("Xcode — Source Editor: func parseVmStat"),
            "prompt must embed the context summary"
        );
        assert!(
            prompt.contains("[SILENT]"),
            "prompt must mention [SILENT] opt-out so model knows the convention"
        );
    }

    // ── build_shell_error_prompt ─────────────────────────────────────────────

    #[test]
    fn build_shell_error_prompt_contains_command_exit_and_cwd() {
        let prompt = ProactiveEngine::build_shell_error_prompt(
            "cargo build", 1, "/Users/jason/Developer/Dex",
        );
        assert!(prompt.contains("cargo build"),                "prompt must contain command");
        assert!(prompt.contains("exit code 1"),                "prompt must contain exit code");
        assert!(prompt.contains("/Users/jason/Developer/Dex"), "prompt must contain cwd");
        assert!(prompt.contains("[SILENT]"),                   "prompt must mention [SILENT] opt-out");
    }

    #[test]
    fn build_shell_error_prompt_distinct_from_ambient_prompt() {
        // Regression guard: the two prompt styles must remain distinct.
        // If they collapse, the model loses the signal that one is about
        // an explicit failure vs. an app-focus ambient observation.
        let error_prompt   = ProactiveEngine::build_shell_error_prompt("make", 2, "/tmp");
        let ambient_prompt = ProactiveEngine::build_proactive_prompt("Terminal — make");
        assert_ne!(error_prompt, ambient_prompt,
            "shell error and ambient proactive prompts must be distinct");
    }

    // ── Gate 7: recent user-activity suppression (Phase 36) ──────────────────

    #[test]
    fn proactive_engine_suppresses_after_recent_user_turn() {
        // Fix C1/C3: once the operator speaks or types, proactive observations
        // must be silenced for `PROACTIVE_USER_ACTIVE_WINDOW_SECS`. Without this
        // gate, an observation fires mid-conversation and overlaps the response.
        let cfg = default_behavior_cfg();
        let mut engine = ProactiveEngine::new_backdated(&cfg, 200);
        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "pre-condition: all earlier gates clear"
        );

        engine.record_user_turn();

        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "Gate 7: a recent user turn must suppress proactive firing"
        );
    }

    #[test]
    fn proactive_engine_unsuppressed_once_window_elapses() {
        // The active window is small (60s default) — simulate expiry by
        // back-dating last_user_turn_at past the window.
        let cfg = default_behavior_cfg();
        let mut engine = ProactiveEngine::new_backdated(&cfg, 200);
        engine.record_user_turn();
        assert!(
            !engine.should_fire(&unlocked_snapshot_with_app()),
            "pre-condition: suppressed right after record_user_turn"
        );

        // Simulate the window elapsing by overwriting the timestamp.
        let window = Duration::from_secs(
            crate::constants::PROACTIVE_USER_ACTIVE_WINDOW_SECS,
        );
        engine.last_user_turn_at = Some(Instant::now() - window - Duration::from_secs(1));

        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "Gate 7: once the active window expires, firing resumes"
        );
    }

    #[test]
    fn proactive_engine_no_user_turn_history_allows_firing() {
        // Gate 7 must be a NOP on a fresh engine (no user turns yet). Guards
        // against a bug where last_user_turn_at = None is treated as "just now."
        let cfg = default_behavior_cfg();
        let engine = ProactiveEngine::new_backdated(&cfg, 200);
        assert!(engine.last_user_turn_at.is_none(), "fresh engine has no turns");
        assert!(
            engine.should_fire(&unlocked_snapshot_with_app()),
            "No prior user turn must not suppress firing"
        );
    }

    // ── Phase 37.9: prompt inversion + low-value filter ──────────────────────

    #[test]
    fn proactive_prompt_is_silent_by_default() {
        // Regression guard for the date/time spam bug.
        // The prompt must frame [SILENT] as the default answer, not as an
        // opt-out. If this test fails because someone softened the DEFAULT
        // wording, they're reintroducing the bug.
        let prompt = ProactiveEngine::build_proactive_prompt("Xcode — Source Editor");
        let lower = prompt.to_ascii_lowercase();
        assert!(
            lower.contains("default: respond with exactly [silent]"),
            "prompt must state [SILENT] is the default, not an opt-out"
        );
        assert!(
            lower.contains("silence is the correct answer"),
            "prompt must tell the model silence is usually correct"
        );
    }

    #[test]
    fn proactive_prompt_forbids_bare_time_and_date() {
        let prompt = ProactiveEngine::build_proactive_prompt("Safari — news.ycombinator.com");
        let lower = prompt.to_ascii_lowercase();
        assert!(lower.contains("forbidden"), "prompt must have a FORBIDDEN section");
        assert!(lower.contains("time"),      "prompt must forbid bare time");
        assert!(lower.contains("date"),      "prompt must forbid bare date");
        assert!(lower.contains("day of the week"), "prompt must forbid bare day of the week");
    }

    // ── is_low_value_response: clock patterns ────────────────────────────────

    #[test]
    fn low_value_catches_bare_clock_with_lead() {
        let cases = [
            "It's 3:42 PM.",
            "it's 3:42pm",
            "The time is 3:42 PM.",
            "Time is 3:42 PM",
            "Currently 3:42 PM.",
            "Right now it's 3:42 PM.",
            "It is 3 PM.",
            "It's 3pm.",
            "The time is 15:42.",
            "It's 15:42:08.",
        ];
        for c in &cases {
            assert!(
                ProactiveEngine::is_low_value_response(c),
                "expected {c:?} to be low-value"
            );
        }
    }

    #[test]
    fn low_value_catches_digit_led_clock() {
        // "3:42 PM" with no leading "it's" — model sometimes drops the lead.
        assert!(ProactiveEngine::is_low_value_response("3:42 PM"));
        assert!(ProactiveEngine::is_low_value_response("3:42 PM."));
        assert!(ProactiveEngine::is_low_value_response("15:42."));
    }

    // ── is_low_value_response: date patterns ─────────────────────────────────

    #[test]
    fn low_value_catches_bare_date() {
        let cases = [
            "Today is April 22, 2026.",
            "It's April 22.",
            "It is April 22, 2026.",
            "The date is April 22, 2026.",
            "Today is Tuesday, April 22.",
        ];
        for c in &cases {
            assert!(
                ProactiveEngine::is_low_value_response(c),
                "expected {c:?} to be low-value"
            );
        }
    }

    #[test]
    fn low_value_catches_bare_day_of_week() {
        assert!(ProactiveEngine::is_low_value_response("Today is Tuesday."));
        assert!(ProactiveEngine::is_low_value_response("It's Wednesday."));
        assert!(ProactiveEngine::is_low_value_response("Tuesday."));
    }

    // ── is_low_value_response: negative cases — must NOT demote ──────────────

    #[test]
    fn low_value_does_not_demote_substantive_observations() {
        // These are exactly the kinds of observations we WANT to keep. If the
        // filter starts demoting these, proactive becomes mute.
        let cases = [
            "You're debugging a concurrency issue in CoreOrchestrator.",
            "Looks like the build failed — missing `serde_json` import in router.rs.",
            "That Xcode error is the missing async marker on the actor method.",
            "You just hit 3pm — still on track for the 4pm deploy window.",
            "Noticed you're editing the AppleScript — remember the body needs escaping.",
            "The test at line 412 looks like it's checking the wrong field.",
        ];
        for c in &cases {
            assert!(
                !ProactiveEngine::is_low_value_response(c),
                "expected {c:?} to SURVIVE the filter (substantive observation)"
            );
        }
    }

    #[test]
    fn low_value_length_gate_protects_substantive_responses() {
        // A response over the length threshold (~80 chars) is never demoted —
        // even if it starts with "It's".
        let long = "It's 3:42 PM and you're about to miss the standup — Slack notification just fired in the sidebar.";
        assert!(long.chars().count() > 80);
        assert!(
            !ProactiveEngine::is_low_value_response(long),
            "long response must not be demoted even if it mentions the time"
        );
    }

    #[test]
    fn low_value_empty_string_is_not_demoted() {
        // is_silent_response handles empty/whitespace — low_value must NOT
        // double-claim it, otherwise we'd log the wrong reason ("demoted") for
        // a silent response.
        assert!(!ProactiveEngine::is_low_value_response(""));
        assert!(!ProactiveEngine::is_low_value_response("   "));
    }

    // ── should_suppress_proactive: aggregate ─────────────────────────────────

    #[test]
    fn should_suppress_catches_both_silent_and_low_value() {
        assert!(ProactiveEngine::should_suppress_proactive("[SILENT]"));
        assert!(ProactiveEngine::should_suppress_proactive("It's 3:42 PM."));
        assert!(ProactiveEngine::should_suppress_proactive("Today is Tuesday."));
        assert!(!ProactiveEngine::should_suppress_proactive(
            "You're debugging a concurrency issue in CoreOrchestrator."
        ));
    }

    // ── is_clocky_token: unit coverage for the token classifier ──────────────

    #[test]
    fn clocky_token_detects_common_clock_shapes() {
        assert!(is_clocky_token("3:42"));
        assert!(is_clocky_token("3:42pm"));
        assert!(is_clocky_token("15:42"));
        assert!(is_clocky_token("15:42:08"));
        assert!(is_clocky_token("3pm"));
        assert!(is_clocky_token("3am"));
        assert!(is_clocky_token("pm"));
        assert!(is_clocky_token("am"));
        assert!(is_clocky_token("p.m."));
    }

    #[test]
    fn clocky_token_rejects_non_clocks() {
        assert!(!is_clocky_token("working"));
        assert!(!is_clocky_token("Xcode"));
        assert!(!is_clocky_token("42")); // bare number is not a clock
        assert!(!is_clocky_token("3"));  // bare number is not a clock
        assert!(!is_clocky_token(""));
        // "router:rs" has a colon but non-digit segments → not a clock.
        assert!(!is_clocky_token("router:rs"));
    }

    // ── has_substantive_verb: protects real observations ─────────────────────

    #[test]
    fn substantive_verb_preserves_technical_observations() {
        assert!(has_substantive_verb("you're debugging a race condition"));
        assert!(has_substantive_verb("the test failed at line 412"));
        assert!(has_substantive_verb("you just hit the deploy window"));
        assert!(has_substantive_verb("still on track for 4pm"));
    }

    #[test]
    fn substantive_verb_correctly_absent_for_bare_clock() {
        assert!(!has_substantive_verb("it's 3:42 pm"));
        assert!(!has_substantive_verb("today is tuesday"));
        assert!(!has_substantive_verb("april 22 2026"));
    }
}
