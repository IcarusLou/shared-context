use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use sctx_event_schema::{
    Annotations, Applicability, ConflictParticipant, ConflictResolutionDraft,
    ConflictResolutionResult, ContextId, ContextKind, ContextRevisionDraft, DiagnosticCode, Event,
    EventPayload, EventType, EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, OriginHint,
    ParsedEvent, PublicationAction, PublicationDraft, PublicationId, ResolutionOutcome,
    ReviewDraft, ReviewVerdict, SemanticConflictDraft, V1_JSON_SCHEMA, V1_SCHEMA_ID, WorkEpisodeId,
    parse_event,
};
use serde_json::{Value, json};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn json_files(path: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(path)
        .expect("fixture directory should exist")
        .map(|entry| entry.expect("fixture should be readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    paths
}

fn parse_known(bytes: &[u8]) -> Event {
    match parse_event(bytes).expect("fixture should parse") {
        ParsedEvent::Known(event) => *event,
        ParsedEvent::UnknownSchema(_) => panic!("V1 fixture must not be quarantined"),
    }
}

fn intent(title: &str) -> IntentSnapshot {
    IntentSnapshot {
        title: title.to_owned(),
        problem: "Repeated rediscovery".to_owned(),
        desired_outcome: "Durable context".to_owned(),
        in_scope: vec!["event contract".to_owned()],
        out_of_scope: vec!["SQLite reducer".to_owned()],
        acceptance_conditions: vec!["round trip".to_owned()],
        domain_terms: vec!["ContextSpace".to_owned()],
    }
}

fn context_draft() -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Decision,
        topic_key: Some("event-schema/compatibility".to_owned()),
        statement: "Unknown schemas are quarantined".to_owned(),
        rationale: "Unsupported semantics cannot be projected safely".to_owned(),
        applicability: Applicability {
            domains: vec!["event-schema".to_owned()],
            platforms: vec!["macos".to_owned()],
            conditions: vec!["unsupported version".to_owned()],
        },
        assumptions: vec!["raw JSON remains available".to_owned()],
        recheck_when: vec!["parser support changes".to_owned()],
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::SourceSnapshot,
            supports: "Parser checks the version first".to_owned(),
            content: json!({"projectable": false}),
            interpretation: "The input is diagnostic-only".to_owned(),
            limitations: vec!["Future payload is not validated".to_owned()],
        }],
    }
}

#[test]
fn all_eight_v1_fixtures_round_trip_without_semantic_loss() {
    let paths = json_files(&fixture_root().join("events/v1/valid"));
    assert_eq!(paths.len(), 8);

    let mut event_types = Vec::new();
    for path in paths {
        let input = fs::read(&path).expect("fixture should be readable");
        let event = parse_known(&input);
        event_types.push(event.event_type().as_str());

        let original: Value = serde_json::from_slice(&input).expect("fixture JSON should parse");
        let serialized = serde_json::to_value(&event).expect("event should serialize");
        assert_eq!(
            serialized,
            original,
            "round-trip mismatch for {}",
            path.display()
        );
    }

    event_types.sort_unstable();
    assert_eq!(
        event_types,
        vec![
            "context.publication_changed",
            "context.reviewed",
            "context.revision_added",
            "context_candidate.created",
            "semantic_conflict.opened",
            "semantic_conflict.resolution_added",
            "space.created",
            "space.intent_revision_added",
        ]
    );
}

#[test]
fn every_invalid_fixture_is_rejected_as_v1_input() {
    let paths = json_files(&fixture_root().join("events/v1/invalid"));
    assert!(paths.len() >= 8);

    for path in paths {
        let input = fs::read(&path).expect("fixture should be readable");
        let error = parse_event(&input)
            .map(|_| ())
            .expect_err(&format!("{} should be invalid", path.display()));
        assert_eq!(error.kind(), sctx_event_schema::ErrorKind::InvalidInput);

        let direct: Result<Event, _> = serde_json::from_slice(&input);
        assert!(
            direct.is_err(),
            "direct Event deserialization must also reject {}",
            path.display()
        );
    }
}

#[test]
fn unknown_schema_is_preserved_reported_and_never_projectable() {
    let path = fixture_root().join("events/unknown/schema-v2.json");
    let input = fs::read(&path).expect("fixture should be readable");

    let parsed = parse_event(&input).expect("unknown schema should be accepted diagnostically");
    assert!(parsed.known().is_none());
    let ParsedEvent::UnknownSchema(unknown) = parsed else {
        panic!("unknown schema must not become a V1 event");
    };

    assert_eq!(unknown.raw_json().as_bytes(), input);
    assert_eq!(unknown.schema_version(), "2");
    assert_eq!(unknown.event_type(), Some("context.reimagined"));
    assert_eq!(
        unknown.event_id(),
        Some("evt_future-format-is-not-v1-validated")
    );
    assert_eq!(
        unknown.diagnostic().code,
        DiagnosticCode::UnknownSchemaVersion
    );
    assert_eq!(unknown.diagnostic().code.as_str(), "UNKNOWN_SCHEMA_VERSION");
}

#[test]
fn unknown_schema_payload_is_not_validated_as_v1() {
    let input = br#"{
        "schema_version":"future",
        "event_id":{"future":"shape"},
        "event_type":42,
        "payload":null
    }"#;

    let parsed = parse_event(input).expect("future payload must remain diagnostic input");
    let ParsedEvent::UnknownSchema(unknown) = parsed else {
        panic!("future payload must not become a projectable event");
    };
    assert_eq!(unknown.raw_json().as_bytes(), input);
    assert_eq!(unknown.event_id(), None);
    assert_eq!(unknown.event_type(), None);
}

#[test]
#[allow(clippy::too_many_lines)]
fn generation_api_assigns_new_ids_and_all_generated_events_parse() {
    let annotations = Some(Annotations {
        producer: Some("test".to_owned()),
        origin_hint: Some(OriginHint {
            values: [("path".to_owned(), json!("not-authoritative"))]
                .into_iter()
                .collect(),
        }),
        ..Annotations::default()
    });
    let created = Event::space_created(intent("Initial"), annotations.clone()).unwrap();
    let (space_id, initial_revision_id) = match created.payload() {
        EventPayload::SpaceCreated {
            space_id,
            intent_revision,
        } => (*space_id, intent_revision.revision_id),
        _ => panic!("wrong payload"),
    };
    let intent_added =
        Event::intent_revision_added(space_id, vec![initial_revision_id], intent("Revised"), None)
            .unwrap();
    let candidate =
        Event::context_candidate_created(WorkEpisodeId::new(), context_draft(), None).unwrap();
    let revision_added =
        Event::context_revision_added(space_id, context_draft(), annotations).unwrap();
    let (context_id, revision_id) = match revision_added.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => panic!("wrong payload"),
    };
    let reviewed = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "Self-contained evidence".to_owned(),
        },
        None,
    )
    .unwrap();
    let publication = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids: Vec::new(),
            action: PublicationAction::Publish,
            revision_id,
            review_event_ids: vec![reviewed.event_id()],
        },
        None,
    )
    .unwrap();
    let publication_id = match publication.payload() {
        EventPayload::ContextPublicationChanged { publication, .. } => publication.publication_id,
        _ => panic!("wrong payload"),
    };
    let other_context = ContextId::new();
    let other_revision = sctx_event_schema::RevisionId::new();
    let other_publication = PublicationId::new();
    let conflict = Event::semantic_conflict_opened(
        space_id,
        SemanticConflictDraft {
            participants: vec![
                ConflictParticipant {
                    context_id,
                    revision_id,
                    publication_id,
                },
                ConflictParticipant {
                    context_id: other_context,
                    revision_id: other_revision,
                    publication_id: other_publication,
                },
            ],
            reason: "Incompatible accepted statements".to_owned(),
            applicability: Applicability::default(),
        },
        None,
    )
    .unwrap();
    let conflict_id = match conflict.payload() {
        EventPayload::SemanticConflictOpened { conflict, .. } => conflict.conflict_id,
        _ => panic!("wrong payload"),
    };
    let resolution = Event::semantic_conflict_resolution_added(
        space_id,
        conflict_id,
        ConflictResolutionDraft {
            previous_resolution_ids: Vec::new(),
            related_publication_ids: vec![publication_id, other_publication],
            results: vec![
                ConflictResolutionResult {
                    context_id,
                    revision_id,
                    outcome: ResolutionOutcome::Retained,
                },
                ConflictResolutionResult {
                    context_id: other_context,
                    revision_id: other_revision,
                    outcome: ResolutionOutcome::Revised,
                },
            ],
            rationale: "Retain the behavior backed by evidence".to_owned(),
        },
        None,
    )
    .unwrap();

    let events = [
        candidate,
        created,
        intent_added,
        revision_added,
        reviewed,
        publication,
        conflict,
        resolution,
    ];
    let event_ids: HashSet<_> = events.iter().map(Event::event_id).collect();
    assert_eq!(event_ids.len(), events.len());

    for event in events {
        assert!(event.event_id().to_string().starts_with("evt_"));
        let serialized = serde_json::to_vec(&event).unwrap();
        assert!(matches!(
            parse_event(&serialized).unwrap(),
            ParsedEvent::Known(parsed) if *parsed == event
        ));
    }
}

#[test]
fn generated_id_randomness_is_observable_without_a_caller_supplied_id() {
    let mut event_ids = HashSet::new();
    let mut space_ids = HashSet::new();
    let mut revision_ids = HashSet::new();

    for index in 0..256 {
        let event = Event::space_created(intent(&format!("Space {index}")), None).unwrap();
        event_ids.insert(event.event_id());
        let EventPayload::SpaceCreated {
            space_id,
            intent_revision,
        } = event.payload()
        else {
            unreachable!();
        };
        space_ids.insert(*space_id);
        revision_ids.insert(intent_revision.revision_id);
    }

    assert_eq!(event_ids.len(), 256);
    assert_eq!(space_ids.len(), 256);
    assert_eq!(revision_ids.len(), 256);
    assert!(
        event_ids
            .iter()
            .all(|id| id.to_string().starts_with("evt_"))
    );
    assert!(
        space_ids
            .iter()
            .all(|id| id.to_string().starts_with("spc_"))
    );
    assert!(
        revision_ids
            .iter()
            .all(|id| id.to_string().starts_with("rev_"))
    );
}

#[test]
fn semantic_hash_excludes_annotations_origin_hints_and_production_envelope() {
    let path = fixture_root().join("events/v1/valid/context-revision-added.json");
    let input = fs::read(&path).unwrap();
    let baseline = parse_known(&input);
    let mut changed_metadata: Value = serde_json::from_slice(&input).unwrap();
    changed_metadata["event_id"] = json!("evt_99999999-9999-4999-8999-999999999999");
    changed_metadata["annotations"] = json!({
        "created_at": "2099-12-31T23:59:59Z",
        "producer": "different-agent",
        "origin_hint": {
            "workspace_alias": "different-workspace",
            "path": "different/path.rs",
            "development_commit": "different-commit"
        },
        "session_id": "non-authoritative"
    });
    let metadata_variant = parse_known(&serde_json::to_vec(&changed_metadata).unwrap());

    assert_eq!(baseline.semantic_hash(), metadata_variant.semantic_hash());

    changed_metadata["revision"]["statement"] = json!("A different authoritative statement");
    let semantic_variant = parse_known(&serde_json::to_vec(&changed_metadata).unwrap());
    assert_ne!(baseline.semantic_hash(), semantic_variant.semantic_hash());
}

#[test]
fn bundled_schema_is_versioned_and_matches_the_exported_id() {
    let schema: Value = serde_json::from_str(V1_JSON_SCHEMA).expect("bundled schema is valid JSON");
    assert_eq!(schema["$id"], V1_SCHEMA_ID);
    assert_eq!(schema["properties"]["schema_version"]["const"], "1");
    assert_eq!(schema["unevaluatedProperties"], false);
}

#[test]
fn non_object_missing_or_non_string_schema_version_is_not_an_unknown_event() {
    for input in [
        br"[]".as_slice(),
        br"{}".as_slice(),
        br#"{"schema_version": 2}"#.as_slice(),
    ] {
        assert!(parse_event(input).is_err());
    }
}

#[test]
fn known_schema_with_unknown_event_type_is_rejected_not_quarantined() {
    let input = br#"{
        "schema_version":"1",
        "event_id":"evt_00000000-0000-4000-8000-000000000099",
        "event_type":"future.event"
    }"#;
    assert!(parse_event(input).is_err());
}

#[test]
fn event_type_getter_matches_payload_variant() {
    let event = Event::space_created(intent("Type"), None).unwrap();
    assert_eq!(event.event_type(), EventType::SpaceCreated);
}

#[test]
fn candidate_event_has_source_content_but_no_space_route() {
    let event = Event::context_candidate_created(WorkEpisodeId::new(), context_draft(), None)
        .expect("valid Candidate event");
    let EventPayload::ContextCandidateCreated { candidate } = event.payload() else {
        panic!("expected context_candidate.created");
    };

    assert!(!candidate.is_auto_injection_eligible());
    let serialized = serde_json::to_value(event).expect("serialize Candidate event");
    assert!(serialized.get("space_id").is_none());
    assert!(serialized["candidate"].get("space_id").is_none());
    assert!(serialized["candidate"].get("source_episode_id").is_some());
    assert!(serialized["candidate"].get("content").is_some());
}
