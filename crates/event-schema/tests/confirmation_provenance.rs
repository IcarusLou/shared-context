//! Disposition provenance lives in `annotations` and nowhere else.
//!
//! `schemas/event-v1.schema.json` is byte-frozen and `EventPayload` denies unknown fields, so an
//! installation running an older binary must be able to parse an event this one wrote. These tests
//! pin exactly that: the provenance keys are annotations, an event carrying them parses and
//! projects like any other, and every hash a replay compares is byte-identical with and without
//! them.

use sctx_domain::{
    CandidateConfirmationOperation, CandidateConfirmationPrimaryReference,
    CandidatePrimarySelection, DecisionSource,
};
use sctx_event_schema::{
    Annotations, Applicability, CandidateConfirmationPlan, ConfirmationProvenance,
    ContextCandidate, ContextKind, ContextRevisionDraft, Event, EventPayload, EventType,
    EvidenceSnapshotDraft, EvidenceType, ParsedEvent, SpaceId, SubmissionId, TaskId, TaskSessionId,
    WorkEpisodeId, WorkEpisodeRef, parse_event,
};
use serde_json::Value;

const WRITER_BATCH: &str = "bat_2f1c0e2a-9c4b-4d0e-8f31-6a5b7c8d9e01";

fn candidate() -> ContextCandidate {
    ContextCandidate::from_verified_submission(
        SubmissionId::new(),
        WorkEpisodeRef {
            episode_id: WorkEpisodeId::new(),
            task_session_id: TaskSessionId::new(),
            task_id: TaskId::new(),
        },
        ContextRevisionDraft {
            problem_view: None,
            hints: Vec::new(),
            kind: ContextKind::Discovery,
            topic_key: Some("candidate/confirmation-provenance".to_owned()),
            statement: "provenance belongs in annotations".to_owned(),
            rationale: "the V1 payload schema is frozen and denies unknown fields".to_owned(),
            applicability: Applicability {
                domains: vec!["candidate".to_owned()],
                platforms: Vec::new(),
                conditions: vec!["verified source Episode".to_owned()],
            },
            assumptions: Vec::new(),
            recheck_when: vec!["the schema version changes".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "the annotation boundary was exercised".to_owned(),
                content: serde_json::json!({"operation": "candidate_confirm"}),
                interpretation: "provenance never reaches the payload".to_owned(),
                limitations: vec!["one plan only".to_owned()],
            }],
        },
    )
    .unwrap()
}

fn plan(candidate: &ContextCandidate, space_id: SpaceId) -> CandidateConfirmationPlan {
    CandidateConfirmationPlan::reserve(
        candidate,
        CandidateConfirmationOperation {
            candidate_id: candidate.candidate_id,
            review_parent_version: 1,
            analysis_generation: 1,
            primary: CandidateConfirmationPrimaryReference::ExistingSpace { space_id },
            related_space_ids: Vec::new(),
            edits: sctx_event_schema::OptionalCandidateEdits::default(),
        },
        CandidatePrimarySelection::Existing { space_id },
        Vec::new(),
        Vec::new(),
    )
    .unwrap()
}

fn agent_provenance() -> ConfirmationProvenance {
    ConfirmationProvenance {
        decision_source: DecisionSource::AgentPolicy,
        author: Some("icaruslou".to_owned()),
        external_session_id: Some("xss_2c9f0d3b-5a71-4e62-9b83-1d4e5f607a8c".to_owned()),
    }
}

fn confirmation_event(events: &[Event]) -> &Event {
    events
        .iter()
        .find(|event| event.event_type() == EventType::CandidateConfirmed)
        .expect("a Confirmation closure always carries its Confirmation Event")
}

/// The three provenance keys are annotations, and the payload is untouched.
#[test]
fn provenance_reaches_annotations_and_never_the_payload() {
    let candidate = candidate();
    let plan = plan(&candidate, SpaceId::new());
    let events =
        Event::from_candidate_confirmation_plan(&plan, WRITER_BATCH, &agent_provenance()).unwrap();
    let confirmed = confirmation_event(&events);

    let annotations = confirmed.annotations().expect("annotations are written");
    assert_eq!(
        annotations.additional.get("decision_source"),
        Some(&Value::String("agent_policy".to_owned()))
    );
    assert_eq!(
        annotations.additional.get("author"),
        Some(&Value::String("icaruslou".to_owned()))
    );
    assert_eq!(
        annotations.additional.get("external_session_id"),
        Some(&Value::String(
            "xss_2c9f0d3b-5a71-4e62-9b83-1d4e5f607a8c".to_owned()
        ))
    );

    // The serialized envelope carries them inside `annotations`, which the frozen V1 schema
    // declares `additionalProperties: true`. The payload object beside it is exactly what a
    // Confirmation payload always was, which is what an older binary's `deny_unknown_fields`
    // deserializer reads.
    let serialized = serde_json::to_value(confirmed).unwrap();
    assert_eq!(serialized["annotations"]["decision_source"], "agent_policy");
    assert_eq!(serialized["annotations"]["author"], "icaruslou");
    assert!(
        serialized["confirmation"].get("decision_source").is_none(),
        "provenance must never appear inside the payload"
    );
    assert!(serialized["confirmation"].get("author").is_none());
    assert!(
        serialized["confirmation"]
            .get("external_session_id")
            .is_none()
    );

    // Every other Event of the same closure is left alone: provenance is a fact about the
    // confirmation decision, not about the revision or the publication it produced.
    for event in &events {
        if event.event_type() == EventType::CandidateConfirmed {
            continue;
        }
        let other = event.annotations().expect("writer batch is annotated");
        assert!(!other.additional.contains_key("decision_source"));
        assert!(!other.additional.contains_key("author"));
    }
}

/// An event carrying provenance parses strictly and deserializes its payload unchanged.
#[test]
fn an_annotated_confirmation_event_parses_and_projects() {
    let candidate = candidate();
    let plan = plan(&candidate, SpaceId::new());
    let events =
        Event::from_candidate_confirmation_plan(&plan, WRITER_BATCH, &agent_provenance()).unwrap();
    let confirmed = confirmation_event(&events);
    let bytes = serde_json::to_vec(confirmed).unwrap();

    let ParsedEvent::Known(parsed) = parse_event(&bytes).unwrap() else {
        panic!("an annotated Confirmation Event stays a known V1 Event");
    };
    assert_eq!(parsed.event_id(), confirmed.event_id());
    match parsed.payload() {
        EventPayload::CandidateConfirmed { confirmation } => {
            assert_eq!(**confirmation, plan.confirmation);
        }
        other => panic!("unexpected payload: {other:?}"),
    }
    assert_eq!(
        serde_json::to_vec(&*parsed).unwrap(),
        bytes,
        "parsing and re-serializing an annotated Event is byte-identical"
    );
}

/// Provenance changes no identity a retry or a replay compares.
#[test]
fn provenance_leaves_every_replay_identity_byte_identical() {
    let candidate = candidate();
    let space_id = SpaceId::new();
    let plan = plan(&candidate, space_id);

    let human = Event::from_candidate_confirmation_plan(
        &plan,
        WRITER_BATCH,
        &ConfirmationProvenance::human(),
    )
    .unwrap();
    let agent =
        Event::from_candidate_confirmation_plan(&plan, WRITER_BATCH, &agent_provenance()).unwrap();

    let human_confirmed = confirmation_event(&human);
    let agent_confirmed = confirmation_event(&agent);

    let (human_operation, human_plan) = human_confirmed.confirmation_hashes().unwrap();
    let (agent_operation, agent_plan) = agent_confirmed.confirmation_hashes().unwrap();
    assert_eq!(human_operation, agent_operation);
    assert_eq!(human_plan, agent_plan);
    assert_eq!(human_operation, plan.operation.operation_hash());
    assert_eq!(human_plan, plan.plan_hash());

    // `semantic_hash` excludes annotations by construction; this pins that the exclusion still
    // holds for the keys added here.
    assert_eq!(
        human_confirmed.semantic_hash(),
        agent_confirmed.semantic_hash(),
        "provenance is not part of an Event's semantics"
    );
    assert_eq!(
        serde_json::to_vec(human_confirmed.payload()).unwrap(),
        serde_json::to_vec(agent_confirmed.payload()).unwrap(),
        "provenance is not part of an Event's payload"
    );
}

/// A `human` confirmation records its source and nothing it could not resolve.
#[test]
fn a_human_confirmation_records_only_the_keys_it_resolved() {
    let candidate = candidate();
    let plan = plan(&candidate, SpaceId::new());
    let events = Event::from_candidate_confirmation_plan(
        &plan,
        WRITER_BATCH,
        &ConfirmationProvenance::human(),
    )
    .unwrap();
    let annotations = confirmation_event(&events).annotations().unwrap();

    assert_eq!(
        annotations.additional.get("decision_source"),
        Some(&Value::String("human".to_owned()))
    );
    assert!(
        !annotations.additional.contains_key("author"),
        "an unresolvable author is omitted, never guessed or written empty"
    );
    assert!(!annotations.additional.contains_key("external_session_id"));
}

/// An event written before provenance existed still round-trips byte-for-byte.
///
/// The keys are additional annotation members, so an event with no `annotations` at all — and one
/// with only the members that always existed — must be unaffected by this change.
#[test]
fn an_event_without_provenance_annotations_round_trips_unchanged() {
    for annotations in [
        None,
        Some(Annotations::default()),
        Some(Annotations {
            created_at: Some("2026-09-04T00:00:00Z".to_owned()),
            producer: Some("sctx/0.1.0".to_owned()),
            origin_hint: None,
            additional: [(
                "writer_batch_id".to_owned(),
                Value::String(WRITER_BATCH.to_owned()),
            )]
            .into_iter()
            .collect(),
        }),
    ] {
        let event = Event::space_created(
            sctx_event_schema::IntentSnapshot {
                title: "Provenance boundary".to_owned(),
                problem: "old events must stay readable".to_owned(),
                desired_outcome: "byte-identical round trips".to_owned(),
                in_scope: vec!["annotations".to_owned()],
                out_of_scope: Vec::new(),
                acceptance_conditions: vec!["the round trip is byte-identical".to_owned()],
                domain_terms: Vec::new(),
            },
            annotations,
        )
        .unwrap();
        let bytes = serde_json::to_vec(&event).unwrap();
        let ParsedEvent::Known(parsed) = parse_event(&bytes).unwrap() else {
            panic!("a V1 Event stays a known V1 Event");
        };
        assert_eq!(
            serde_json::to_vec(&*parsed).unwrap(),
            bytes,
            "an Event written before provenance existed is unchanged by it"
        );
        let reparsed = parsed.annotations().cloned();
        assert!(
            reparsed
                .as_ref()
                .is_none_or(|values| !values.additional.contains_key("decision_source")),
            "nothing invents provenance on an Event that never carried it"
        );
    }
}
