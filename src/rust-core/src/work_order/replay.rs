//! Offline founding-failure replay fixtures for DEX-03 Slice A4.
//!
//! Every fixture declares its family explicitly. No code in this module
//! classifies or interprets the operator sentence; language-to-work-order entry
//! remains Slice B work.

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{
    shadow::{ShadowEvent, ShadowTracker, SHADOW_TRACE_FILENAME},
    types::{
        fingerprint, EvidenceRef, EvidenceSource, FreshnessRequirement, Obligation, ObligationKind,
        ObligationSource, SecurityLabel, SucceededWorkOrder, WorkOrder, WorkOrderError,
        WorkOrderKind, WorkOrderScope,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FoundingFamily {
    BrowserTitle,
    FocusWindow,
    MissingLocalControl,
}

#[derive(Debug, Clone)]
struct ReplayFixture {
    id: &'static str,
    family: FoundingFamily,
    operator_text: String,
}

impl ReplayFixture {
    fn work_order(&self) -> WorkOrder {
        let now = replay_time();
        WorkOrder::new(
            format!("replay-{}", self.id),
            "replay-session",
            format!("replay-turn-{}", self.id),
            fingerprint(&self.operator_text),
            now,
            now + Duration::seconds(15),
            WorkOrderKind::Action,
            fixture_goal(self.family),
            fixture_scope(self.family),
            ObligationSource::fixture(self.id).expect("fixture ID is non-empty"),
            fixture_obligations(self.family),
        )
        .expect("hand-authored replay fixture must be valid")
    }
}

#[derive(Debug, Clone, Copy)]
struct ReplayEvidence {
    obligation_id: &'static str,
    source: EvidenceSource,
    source_id: &'static str,
    fact: &'static str,
}

fn corpus() -> Vec<ReplayFixture> {
    let rows = [
        (
            "browser-open-apple-title",
            FoundingFamily::BrowserTitle,
            "Open apple.com in the browser and tell me the page title.",
        ),
        (
            "browser-wikipedia-title",
            FoundingFamily::BrowserTitle,
            "Go to Wikipedia and read me the title.",
        ),
        (
            "browser-example-title",
            FoundingFamily::BrowserTitle,
            "Load example.com; what is the title of the page?",
        ),
        (
            "browser-visit-title",
            FoundingFamily::BrowserTitle,
            "Visit apple.com and report the title shown in the browser.",
        ),
        (
            "browser-navigate-title",
            FoundingFamily::BrowserTitle,
            "Navigate to example.com, then tell me what the page is called.",
        ),
        ("focus-finder", FoundingFamily::FocusWindow, "Focus Finder."),
        (
            "focus-bring-finder-forward",
            FoundingFamily::FocusWindow,
            "Bring Finder forward.",
        ),
        (
            "focus-safari-front",
            FoundingFamily::FocusWindow,
            "Put Safari in front.",
        ),
        (
            "focus-make-finder-frontmost",
            FoundingFamily::FocusWindow,
            "Make the Finder window frontmost.",
        ),
        (
            "focus-switch-safari",
            FoundingFamily::FocusWindow,
            "Switch the active application to Safari.",
        ),
        (
            "missing-finder-control",
            FoundingFamily::MissingLocalControl,
            "Click the Missing Control button in Finder.",
        ),
        (
            "missing-xcode-control",
            FoundingFamily::MissingLocalControl,
            "Press a button called Does Not Exist in Xcode.",
        ),
        (
            "missing-finder-labeled-control",
            FoundingFamily::MissingLocalControl,
            "In Finder, click the button labeled Dexter Manual Missing Control.",
        ),
        (
            "missing-xcode-named-control",
            FoundingFamily::MissingLocalControl,
            "Try the control named No Such Button in the frontmost Xcode window.",
        ),
        (
            "missing-finder-press-control",
            FoundingFamily::MissingLocalControl,
            "Press Finder's Not Present button.",
        ),
    ];
    rows.into_iter()
        .map(|(id, family, operator_text)| ReplayFixture {
            id,
            family,
            operator_text: operator_text.to_string(),
        })
        .collect()
}

fn fixture_goal(family: FoundingFamily) -> &'static str {
    match family {
        FoundingFamily::BrowserTitle => "Navigate, verify the page, and deliver its title",
        FoundingFamily::FocusWindow => "Focus and verify the requested local application",
        FoundingFamily::MissingLocalControl => {
            "Attempt the requested local control and deliver its structured outcome"
        }
    }
}

fn fixture_scope(family: FoundingFamily) -> WorkOrderScope {
    match family {
        FoundingFamily::BrowserTitle => WorkOrderScope::Browser,
        FoundingFamily::FocusWindow | FoundingFamily::MissingLocalControl => {
            WorkOrderScope::LocalUi
        }
    }
}

fn fixture_obligations(family: FoundingFamily) -> Vec<Obligation> {
    match family {
        FoundingFamily::BrowserTitle => vec![
            obligation(
                "navigate",
                ObligationKind::Effect,
                &[],
                EvidenceSource::ActionReceipt,
            ),
            post_action_observation(
                "verify_url",
                ObligationKind::Observation,
                &["navigate"],
                EvidenceSource::BrowserResult,
            ),
            post_action_observation(
                "observe_title",
                ObligationKind::RequestedOutput,
                &["verify_url"],
                EvidenceSource::BrowserResult,
            ),
            obligation(
                "deliver_title",
                ObligationKind::OperatorDelivery,
                &["observe_title"],
                EvidenceSource::SessionEvent,
            ),
        ],
        FoundingFamily::FocusWindow => vec![
            obligation(
                "dispatch_window_focus",
                ObligationKind::Effect,
                &[],
                EvidenceSource::ActionReceipt,
            ),
            post_action_observation(
                "verify_frontmost_app",
                ObligationKind::Observation,
                &["dispatch_window_focus"],
                EvidenceSource::ContextSnapshot,
            ),
            obligation(
                "deliver_focus_result",
                ObligationKind::OperatorDelivery,
                &["verify_frontmost_app"],
                EvidenceSource::SessionEvent,
            ),
        ],
        FoundingFamily::MissingLocalControl => vec![
            obligation(
                "resolve_local_app",
                ObligationKind::Observation,
                &[],
                EvidenceSource::ContextSnapshot,
            ),
            obligation(
                "attempt_control",
                ObligationKind::Effect,
                &["resolve_local_app"],
                EvidenceSource::ActionReceipt,
            ),
            obligation(
                "record_local_result",
                ObligationKind::RequestedOutput,
                &["attempt_control"],
                EvidenceSource::ActionReceipt,
            ),
            obligation(
                "deliver_local_result",
                ObligationKind::OperatorDelivery,
                &["record_local_result"],
                EvidenceSource::SessionEvent,
            ),
        ],
    }
}

fn obligation(
    id: &str,
    kind: ObligationKind,
    dependencies: &[&str],
    source: EvidenceSource,
) -> Obligation {
    Obligation::new(
        id,
        id.replace('_', " "),
        kind,
        dependencies
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        vec![source],
        FreshnessRequirement::Any,
    )
    .expect("fixture obligation must be valid")
}

fn post_action_observation(
    id: &str,
    kind: ObligationKind,
    dependencies: &[&str],
    source: EvidenceSource,
) -> Obligation {
    Obligation::new(
        id,
        id.replace('_', " "),
        kind,
        dependencies
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        vec![source],
        FreshnessRequirement::ObservedAfter(replay_time() + Duration::milliseconds(1)),
    )
    .expect("post-action fixture obligation must be valid")
}

fn apply(order: &mut WorkOrder, evidence: ReplayEvidence) -> Result<(), WorkOrderError> {
    apply_at(order, evidence, replay_time() + Duration::seconds(1))
}

fn apply_at(
    order: &mut WorkOrder,
    evidence: ReplayEvidence,
    observed_at: DateTime<Utc>,
) -> Result<(), WorkOrderError> {
    let obligation_id = evidence.obligation_id;
    let evidence = EvidenceRef::new(
        evidence.source,
        evidence.source_id,
        observed_at,
        evidence.fact,
        SecurityLabel::OperatorPrivate,
    )?;
    let evidence_id = order.record_evidence(evidence)?;
    order.satisfy_obligation(obligation_id, &evidence_id)
}

fn complete(
    mut order: WorkOrder,
    final_delivery_source_id: &str,
) -> Result<SucceededWorkOrder, String> {
    order
        .begin_verification()
        .map_err(|error| error.to_string())?;
    let final_delivery_evidence_id = order
        .evidence_journal()
        .iter()
        .find(|evidence| evidence.source_id() == final_delivery_source_id)
        .map(|evidence| evidence.id().clone())
        .ok_or_else(|| format!("missing delivery evidence: {final_delivery_source_id}"))?;
    let proof = order
        .completion_proof(&final_delivery_evidence_id)
        .map_err(|error| error.to_string())?;
    order
        .succeed(proof)
        .map_err(|error| error.error().to_string())
}

fn fixture(family: FoundingFamily) -> ReplayFixture {
    corpus()
        .into_iter()
        .find(|fixture| fixture.family == family)
        .expect("fixture family exists")
}

fn replay_time() -> DateTime<Utc> {
    Utc.timestamp_opt(1_786_000_000, 0).single().unwrap()
}

fn test_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("dexter-replay-{label}-{}", uuid::Uuid::new_v4()))
}

#[test]
fn corpus_has_five_explicit_paraphrases_per_founding_family() {
    let fixtures = corpus();
    for family in [
        FoundingFamily::BrowserTitle,
        FoundingFamily::FocusWindow,
        FoundingFamily::MissingLocalControl,
    ] {
        assert_eq!(
            fixtures
                .iter()
                .filter(|fixture| fixture.family == family)
                .count(),
            5
        );
    }
    for fixture in fixtures {
        let order = fixture.work_order();
        assert!(matches!(
            order.obligation_source(),
            ObligationSource::Fixture { fixture_id } if fixture_id == fixture.id
        ));
        assert_eq!(order.scope(), fixture_scope(fixture.family));
    }
}

#[tokio::test]
async fn navigation_only_trace_keeps_title_observation_and_delivery_pending() {
    let mut fixture = fixture(FoundingFamily::BrowserTitle);
    fixture.operator_text =
        "Open apple.com and tell me the title. DEXTER-REPLAY-CANARY fake-password=swordfish"
            .to_string();
    let mut order = fixture.work_order();
    order.activate().unwrap();
    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "navigate",
            source: EvidenceSource::ActionReceipt,
            source_id: "browser-action-1",
            fact: "browser navigation completed",
        },
    )
    .unwrap();
    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "verify_url",
            source: EvidenceSource::BrowserResult,
            source_id: "browser-action-1",
            fact: "page_url=https://apple.com/?password=swordfish DEXTER-REPLAY-CANARY",
        },
    )
    .unwrap();

    assert_eq!(
        order.pending_obligation_ids(),
        vec!["observe_title", "deliver_title"]
    );
    assert!(complete(order.clone(), "delivery-title").is_err());

    let state_dir = test_dir("navigation-only");
    let tracker = ShadowTracker::spawn(&state_dir);
    tracker.observe(ShadowEvent::WorkOrderSnapshot(order));
    tracker.flush_for_test().await;
    let trace = std::fs::read_to_string(state_dir.join(SHADOW_TRACE_FILENAME)).unwrap();
    let row: serde_json::Value = serde_json::from_str(trace.trim()).unwrap();
    let obligations = row["work_order"]["obligations"].as_array().unwrap();
    assert!(obligations.iter().any(|obligation| {
        obligation["id"] == "observe_title" && obligation["status"] == "pending"
    }));
    assert!(obligations.iter().any(|obligation| {
        obligation["id"] == "deliver_title" && obligation["status"] == "pending"
    }));
    assert!(!trace.contains("DEXTER-REPLAY-CANARY"));
    assert!(!trace.contains("swordfish"));
    std::fs::remove_dir_all(state_dir).unwrap();
}

#[test]
fn browser_title_replay_cannot_complete_until_observed_value_is_delivered() {
    let mut order = fixture(FoundingFamily::BrowserTitle).work_order();
    order.activate().unwrap();
    for evidence in [
        ReplayEvidence {
            obligation_id: "navigate",
            source: EvidenceSource::ActionReceipt,
            source_id: "browser-action-2",
            fact: "navigation completed",
        },
        ReplayEvidence {
            obligation_id: "verify_url",
            source: EvidenceSource::BrowserResult,
            source_id: "browser-action-2",
            fact: "page_url=https://apple.com/",
        },
        ReplayEvidence {
            obligation_id: "observe_title",
            source: EvidenceSource::BrowserResult,
            source_id: "browser-title-2",
            fact: "page_title=Apple",
        },
    ] {
        apply(&mut order, evidence).unwrap();
    }
    assert_eq!(order.pending_obligation_ids(), vec!["deliver_title"]);
    assert!(complete(order.clone(), "delivery-title-2").is_err());

    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "deliver_title",
            source: EvidenceSource::SessionEvent,
            source_id: "delivery-title-2",
            fact: "operator_message page_title=Apple",
        },
    )
    .unwrap();
    assert!(complete(order, "delivery-title-2").is_ok());
}

#[test]
fn focus_replay_requires_fresh_frontmost_app_observation() {
    let mut order = fixture(FoundingFamily::FocusWindow).work_order();
    order.activate().unwrap();
    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "dispatch_window_focus",
            source: EvidenceSource::ActionReceipt,
            source_id: "focus-action-1",
            fact: "WindowFocus dispatched",
        },
    )
    .unwrap();
    assert_eq!(
        order.pending_obligation_ids(),
        vec!["verify_frontmost_app", "deliver_focus_result"]
    );
    let stale_observation = apply_at(
        &mut order,
        ReplayEvidence {
            obligation_id: "verify_frontmost_app",
            source: EvidenceSource::ContextSnapshot,
            source_id: "context-before-focus",
            fact: "frontmost_app_bundle_id=com.apple.finder",
        },
        replay_time(),
    )
    .unwrap_err();
    assert_eq!(
        stale_observation,
        WorkOrderError::StaleEvidence("verify_frontmost_app".to_string())
    );
    let wrong_source = apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "verify_frontmost_app",
            source: EvidenceSource::BrowserResult,
            source_id: "online-result",
            fact: "Finder information from the web",
        },
    )
    .unwrap_err();
    assert!(matches!(
        wrong_source,
        WorkOrderError::UnacceptableEvidence { .. }
    ));

    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "verify_frontmost_app",
            source: EvidenceSource::ContextSnapshot,
            source_id: "context-after-focus",
            fact: "frontmost_app_bundle_id=com.apple.finder",
        },
    )
    .unwrap();
    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "deliver_focus_result",
            source: EvidenceSource::SessionEvent,
            source_id: "delivery-focus-1",
            fact: "operator_message focus verified",
        },
    )
    .unwrap();
    assert!(complete(order, "delivery-focus-1").is_ok());
}

#[test]
fn missing_control_replay_stays_local_and_delivers_structured_failure() {
    let mut order = fixture(FoundingFamily::MissingLocalControl).work_order();
    assert_eq!(order.scope(), WorkOrderScope::LocalUi);
    order.activate().unwrap();
    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "resolve_local_app",
            source: EvidenceSource::ContextSnapshot,
            source_id: "finder-context",
            fact: "frontmost_app_bundle_id=com.apple.finder",
        },
    )
    .unwrap();
    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "attempt_control",
            source: EvidenceSource::ActionReceipt,
            source_id: "ui-click-action",
            fact: "outcome=failure kind=control_not_found",
        },
    )
    .unwrap();
    let action_receipt_id = order
        .evidence_journal()
        .iter()
        .find(|evidence| evidence.source_id() == "ui-click-action")
        .map(|evidence| evidence.id().clone())
        .unwrap();
    order
        .satisfy_obligation("record_local_result", &action_receipt_id)
        .unwrap();
    assert_eq!(order.pending_obligation_ids(), vec!["deliver_local_result"]);
    assert!(complete(order.clone(), "delivery-ui-failure").is_err());

    apply(
        &mut order,
        ReplayEvidence {
            obligation_id: "deliver_local_result",
            source: EvidenceSource::SessionEvent,
            source_id: "delivery-ui-failure",
            fact: "operator_message structured local control failure",
        },
    )
    .unwrap();
    assert!(complete(order, "delivery-ui-failure").is_ok());
}
