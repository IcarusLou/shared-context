use std::{fs, path::Path, str::FromStr};

use proptest::prelude::*;
use sctx_event_schema::{
    ArtifactKind, ArtifactLocator, AutoInjectionBlocker, ConflictId, ContextGovernanceStatus,
    ContextId, EngineeringReferenceDraft, Event, EventPayload, ReducerDiagnosticCode, ReducerEvent,
    ReducerPayload, ReferenceRelation, RepoRelativePath, RepositoryId, ResolutionId, ReviewSummary,
    RevisionId, RevisionLifecycle, SemanticConflictOpenReason, SemanticConflictStatus, SpaceId,
    reduce,
};
use serde_json::Value;

fn fixture_events(name: &str) -> Vec<Event> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/reducer/v1")
        .join(name);
    let values: Vec<Value> =
        serde_json::from_slice(&fs::read(path).expect("fixture should be readable"))
            .expect("fixture should be a JSON event array");
    values
        .into_iter()
        .map(|value| serde_json::from_value(value).expect("fixture event should be valid V1"))
        .collect()
}

fn fixture_values(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/reducer/v1")
        .join(name);
    serde_json::from_slice(&fs::read(path).expect("fixture should be readable"))
        .expect("fixture should be a JSON event array")
}

fn values_to_reducer_events(values: Vec<Value>) -> Vec<ReducerEvent> {
    values
        .into_iter()
        .map(|value| {
            serde_json::from_value::<Event>(value)
                .expect("modified fixture must remain valid V1")
                .reducer_event()
                .expect("test input must be inside reducer scope")
        })
        .collect()
}

fn set_revision_relations(values: &mut [Value], revision_id: &str, relations: Value) {
    let revision = values
        .iter_mut()
        .find(|value| value["revision"]["revision_id"] == revision_id)
        .expect("revision fixture exists");
    revision["revision"]["relations"] = relations;
}

fn set_revision_kind(values: &mut [Value], revision_id: &str, kind: &str, topic_key: Option<&str>) {
    let revision = values
        .iter_mut()
        .find(|value| value["revision"]["revision_id"] == revision_id)
        .expect("revision fixture exists");
    revision["revision"]["kind"] = serde_json::json!(kind);
    match topic_key {
        Some(topic_key) => revision["revision"]["topic_key"] = serde_json::json!(topic_key),
        None => {
            revision["revision"]
                .as_object_mut()
                .unwrap()
                .remove("topic_key");
        }
    }
}

fn reducer_events(name: &str) -> Vec<ReducerEvent> {
    fixture_events(name)
        .iter()
        .map(|event| {
            event
                .reducer_event()
                .expect("every V1 fixture event must be reducible")
        })
        .collect()
}

fn all_reducer_events() -> Vec<ReducerEvent> {
    [
        "context-candidates.json",
        "intent-branch-merge.json",
        "context-branch-merge.json",
        "review-summaries.json",
        "publication-lifecycle.json",
        "semantic-conflicts.json",
    ]
    .into_iter()
    .flat_map(reducer_events)
    .collect()
}

#[test]
fn every_candidate_permutation_is_identical_and_remains_outside_context_governance() {
    fn visit(events: &mut [ReducerEvent], index: usize, expected: &[u8], checked: &mut usize) {
        if index == events.len() {
            let projection = reduce(events);
            assert_eq!(serde_json::to_vec(&projection).unwrap(), expected);
            assert!(projection.spaces.is_empty());
            assert!(projection.semantic_conflict_candidates.is_empty());
            assert!(projection.semantic_conflicts.is_empty());
            assert!(
                projection
                    .candidates
                    .values()
                    .all(|candidate| !candidate.candidate.is_auto_injection_eligible())
            );
            *checked += 1;
            return;
        }
        for swap_index in index..events.len() {
            events.swap(index, swap_index);
            visit(events, index + 1, expected, checked);
            events.swap(index, swap_index);
        }
    }

    let mut events = reducer_events("context-candidates.json");
    let expected = serde_json::to_vec(&reduce(&events)).unwrap();
    let mut checked = 0;
    visit(&mut events, 0, &expected, &mut checked);
    assert_eq!(checked, 6);
}

#[test]
fn duplicate_candidate_ids_quarantine_every_definition() {
    let mut values = fixture_values("context-candidates.json");
    let mut duplicate = values[0].clone();
    duplicate["event_id"] = serde_json::json!("evt_00000000-0000-4000-8000-000000000854");
    values.push(duplicate);

    let projection = reduce(&values_to_reducer_events(values));

    assert_eq!(projection.candidates.len(), 2);
    assert!(
        projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::DuplicateCandidateId })
    );
}

fn space(id: &str) -> SpaceId {
    SpaceId::from_str(id).unwrap()
}

fn context(id: &str) -> ContextId {
    ContextId::from_str(id).unwrap()
}

fn revision(id: &str) -> RevisionId {
    RevisionId::from_str(id).unwrap()
}

fn reference_event(context_id: ContextId, revision_id: RevisionId, path: &str) -> Event {
    Event::engineering_reference_recorded(
        context_id,
        revision_id,
        EngineeringReferenceDraft {
            repository_id: RepositoryId::new(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator: ArtifactLocator::File {
                path: RepoRelativePath::new(path).unwrap(),
            },
            supports: "The observed file implements this Context revision".to_owned(),
            limitations: vec!["The path can become stale".to_owned()],
        },
        None,
    )
    .unwrap()
}

#[test]
fn intent_branch_converges_only_through_the_explicit_multi_parent_merge() {
    let mut events = reducer_events("intent-branch-merge.json");
    let merge = events.pop().unwrap();

    let branched = reduce(&events);
    let intent = &branched.spaces[&space("spc_00000000-0000-4000-8000-000000000001")].intent;
    assert_eq!(
        intent.heads,
        [
            revision("rev_00000000-0000-4000-8000-000000000102"),
            revision("rev_00000000-0000-4000-8000-000000000103"),
        ]
        .into_iter()
        .collect()
    );

    events.push(merge);
    let merged = reduce(&events);
    assert_eq!(
        merged.spaces[&space("spc_00000000-0000-4000-8000-000000000001")]
            .intent
            .heads,
        [revision("rev_00000000-0000-4000-8000-000000000104")]
            .into_iter()
            .collect()
    );
    assert!(merged.diagnostics.is_empty());
}

#[test]
fn context_branch_converges_only_through_the_explicit_multi_parent_merge() {
    let mut events = reducer_events("context-branch-merge.json");
    let merge = events.pop().unwrap();
    let space_id = space("spc_00000000-0000-4000-8000-000000000002");
    let context_id = context("ctx_00000000-0000-4000-8000-000000000201");

    let branched = reduce(&events);
    assert_eq!(
        branched.spaces[&space_id].contexts[&context_id].revision_heads,
        [
            revision("rev_00000000-0000-4000-8000-000000000212"),
            revision("rev_00000000-0000-4000-8000-000000000213"),
        ]
        .into_iter()
        .collect()
    );

    events.push(merge);
    let merged = reduce(&events);
    let item = &merged.spaces[&space_id].contexts[&context_id];
    assert_eq!(
        item.revision_heads,
        [revision("rev_00000000-0000-4000-8000-000000000214")]
            .into_iter()
            .collect()
    );
    assert_eq!(
        item.revisions.values().filter(|item| item.is_head).count(),
        1
    );
    assert!(merged.diagnostics.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn cross_space_relations_preserve_cycles_concurrent_heads_and_historical_snapshots() {
    let mut values = fixture_values("context-branch-merge.json");
    values.extend(fixture_values("publication-lifecycle.json"));
    values.extend(fixture_values("semantic-conflicts.json"));
    set_revision_kind(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000211",
        "contract",
        Some("relation/contract"),
    );
    set_revision_kind(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000411",
        "decision",
        Some("relation/decision"),
    );
    set_revision_kind(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000513",
        "validation",
        None,
    );
    set_revision_relations(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000211",
        serde_json::json!([{
            "target_context_id": "ctx_00000000-0000-4000-8000-000000000401",
            "kind": "depends_on",
            "rationale": "The root depends on the lifecycle Contract",
            "supports": ["The lifecycle Context defines the required behavior"]
        }]),
    );
    set_revision_relations(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000411",
        serde_json::json!([{
            "target_context_id": "ctx_00000000-0000-4000-8000-000000000503",
            "kind": "implements",
            "rationale": "The lifecycle Contract implements the visibility Decision",
            "supports": ["Both Contexts describe the same visible behavior"]
        }]),
    );
    set_revision_relations(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000513",
        serde_json::json!([{
            "target_context_id": "ctx_00000000-0000-4000-8000-000000000201",
            "kind": "validated_by",
            "rationale": "The visibility Decision is validated by the branch Context",
            "supports": ["The branch fixture records the validation"]
        }]),
    );
    set_revision_relations(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000212",
        serde_json::json!([{
            "target_context_id": "ctx_00000000-0000-4000-8000-000000000502",
            "kind": "contradicts",
            "rationale": "Branch A contradicts the hidden-result Contract",
            "supports": ["The two statements prescribe opposite visibility"]
        }]),
    );
    set_revision_relations(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000213",
        serde_json::json!([{
            "target_context_id": "ctx_00000000-0000-4000-8000-000000000503",
            "kind": "related_to",
            "rationale": "Branch B is related to the billing-only Context",
            "supports": ["Both branches retain result state"]
        }]),
    );
    set_revision_relations(
        &mut values,
        "rev_00000000-0000-4000-8000-000000000214",
        serde_json::json!([{
            "target_context_id": "ctx_00000000-0000-4000-8000-000000000504",
            "kind": "constrains",
            "rationale": "The merged revision constrains ranking behavior",
            "supports": ["The merged snapshot fixes the allowed rank"]
        }]),
    );
    let projection = reduce(&values_to_reducer_events(values));
    assert!(!projection.diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.code,
            ReducerDiagnosticCode::InvalidContextRelation
                | ReducerDiagnosticCode::InvalidContextRelationTarget
        )
    }));
    let source = &projection.spaces[&space("spc_00000000-0000-4000-8000-000000000002")].contexts
        [&context("ctx_00000000-0000-4000-8000-000000000201")];
    assert_eq!(source.revision_heads.len(), 1);
    for revision_id in [
        "rev_00000000-0000-4000-8000-000000000211",
        "rev_00000000-0000-4000-8000-000000000212",
        "rev_00000000-0000-4000-8000-000000000213",
        "rev_00000000-0000-4000-8000-000000000214",
    ] {
        assert_eq!(
            source.revisions[&revision(revision_id)]
                .revision
                .relations
                .len(),
            1,
            "historical Revision relations must remain traceable"
        );
    }
}

#[test]
fn dangling_relation_quarantines_only_owning_revision_and_causal_children() {
    let mut base_values = fixture_values("context-branch-merge.json");
    base_values.extend(fixture_values("publication-lifecycle.json"));
    let baseline = reduce(&values_to_reducer_events(base_values.clone()));
    let source_space = space("spc_00000000-0000-4000-8000-000000000002");
    let unaffected_space = space("spc_00000000-0000-4000-8000-000000000004");
    for invalid_target in [
        "ctx_99999999-9999-4999-8999-999999999999",
        "ctx_00000000-0000-4000-8000-000000000201",
    ] {
        let mut values = base_values.clone();
        set_revision_relations(
            &mut values,
            "rev_00000000-0000-4000-8000-000000000211",
            serde_json::json!([{
                "target_context_id": invalid_target,
                "kind": "depends_on",
                "rationale": "The missing or self target makes this relation invalid",
                "supports": ["The target must be another Context in the current Event set"]
            }]),
        );
        let projection = reduce(&values_to_reducer_events(values));
        assert!(projection.spaces[&source_space].contexts.is_empty());
        assert_eq!(
            projection.spaces[&unaffected_space],
            baseline.spaces[&unaffected_space]
        );
        assert!(projection.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ReducerDiagnosticCode::InvalidContextRelationTarget
                && diagnostic.entity_id == "rev_00000000-0000-4000-8000-000000000211"
        }));
        assert_eq!(
            projection
                .diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.code == ReducerDiagnosticCode::InvalidRevisionReference
                })
                .count(),
            3,
            "concurrent children and merge must follow the invalid parent into quarantine"
        );
    }
}

#[test]
fn every_permutation_of_the_intent_fixture_is_structurally_identical() {
    fn visit(events: &mut [ReducerEvent], index: usize, expected: &[u8], checked: &mut usize) {
        if index == events.len() {
            assert_eq!(serde_json::to_vec(&reduce(events)).unwrap(), expected);
            *checked += 1;
            return;
        }
        for swap_index in index..events.len() {
            events.swap(index, swap_index);
            visit(events, index + 1, expected, checked);
            events.swap(index, swap_index);
        }
    }

    let mut events = reducer_events("intent-branch-merge.json");
    let expected = serde_json::to_vec(&reduce(&events)).unwrap();
    let mut checked = 0;
    visit(&mut events, 0, &expected, &mut checked);
    assert_eq!(checked, 24);
}

#[test]
fn engineering_reference_is_permutation_invariant_and_never_changes_context_governance() {
    let mut events = reducer_events("context-branch-merge.json");
    let baseline = reduce(&events);
    let reference = reference_event(
        context("ctx_00000000-0000-4000-8000-000000000201"),
        revision("rev_00000000-0000-4000-8000-000000000211"),
        "src/path-that-does-not-exist.ts",
    );
    let reference_id = match reference.payload() {
        EventPayload::EngineeringReferenceRecorded { reference, .. } => reference.reference_id,
        _ => unreachable!(),
    };
    events.push(reference.reducer_event().unwrap());
    let forward = reduce(&events);
    events.reverse();
    let reversed = reduce(&events);

    assert_eq!(forward, reversed);
    assert_eq!(forward.spaces, baseline.spaces);
    assert_eq!(forward.semantic_conflicts, baseline.semantic_conflicts);
    assert_eq!(forward.engineering_references.len(), 1);
    assert_eq!(
        forward.engineering_references[&reference_id]
            .reference
            .locator
            .path()
            .as_str(),
        "src/path-that-does-not-exist.ts"
    );
    assert!(forward.diagnostics.is_empty());
}

#[test]
fn duplicate_reference_ids_quarantine_only_the_reference_events() {
    let baseline_events = reducer_events("context-branch-merge.json");
    let baseline = reduce(&baseline_events);
    let context_id = context("ctx_00000000-0000-4000-8000-000000000201");
    let revision_id = revision("rev_00000000-0000-4000-8000-000000000211");
    let first = reference_event(context_id, revision_id, "src/first.ts");
    let second = reference_event(context_id, revision_id, "src/second.ts");
    let first_id = match first.payload() {
        EventPayload::EngineeringReferenceRecorded { reference, .. } => reference.reference_id,
        _ => unreachable!(),
    };
    let mut second_json = serde_json::to_value(second).unwrap();
    second_json["reference"]["reference_id"] = serde_json::json!(first_id);
    let second: Event = serde_json::from_value(second_json).unwrap();
    let mut events = baseline_events;
    events.extend([
        first.reducer_event().unwrap(),
        second.reducer_event().unwrap(),
    ]);
    let projection = reduce(&events);

    assert_eq!(projection.spaces, baseline.spaces);
    assert!(projection.engineering_references.is_empty());
    assert!(projection.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == ReducerDiagnosticCode::DuplicateReferenceId
            && diagnostic.event_ids.len() == 2
    }));
}

#[test]
fn invalid_reference_target_locator_and_relation_never_quarantine_context() {
    let baseline_events = reducer_events("publication-lifecycle.json");
    let baseline = reduce(&baseline_events);
    let context_id = context("ctx_00000000-0000-4000-8000-000000000401");
    let valid_revision = revision("rev_00000000-0000-4000-8000-000000000411");

    let missing = reference_event(ContextId::new(), RevisionId::new(), "src/missing.ts")
        .reducer_event()
        .unwrap();
    let cross_context = reference_event(
        context_id,
        revision("rev_00000000-0000-4000-8000-000000000413"),
        "src/cross.ts",
    )
    .reducer_event()
    .unwrap();
    let mut invalid_relation = reference_event(context_id, valid_revision, "src/relation.ts")
        .reducer_event()
        .unwrap();
    let ReducerPayload::EngineeringReferenceRecorded { reference, .. } =
        &mut invalid_relation.payload
    else {
        unreachable!();
    };
    reference.relation = ReferenceRelation::Consumes;

    let mut invalid_locator = reference_event(context_id, valid_revision, "src/locator.ts")
        .reducer_event()
        .unwrap();
    let ReducerPayload::EngineeringReferenceRecorded { reference, .. } =
        &mut invalid_locator.payload
    else {
        unreachable!();
    };
    reference.locator = ArtifactLocator::File {
        path: serde_json::from_value(Value::String("../outside.rs".to_owned())).unwrap(),
    };

    let mut events = baseline_events;
    events.extend([missing, cross_context, invalid_relation, invalid_locator]);
    let projection = reduce(&events);
    assert_eq!(projection.spaces, baseline.spaces);
    assert!(projection.engineering_references.is_empty());
    assert_eq!(
        projection
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == ReducerDiagnosticCode::InvalidEngineeringReferenceTarget
            })
            .count(),
        2
    );
    assert_eq!(
        projection
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == ReducerDiagnosticCode::InvalidEngineeringReference
            })
            .count(),
        2
    );
}

#[test]
fn approve_reject_mixed_and_unreviewed_summaries_retain_all_reviews() {
    let projection = reduce(&reducer_events("review-summaries.json"));
    let item = &projection.spaces[&space("spc_00000000-0000-4000-8000-000000000003")].contexts
        [&context("ctx_00000000-0000-4000-8000-000000000301")];

    assert_eq!(
        item.revisions[&revision("rev_00000000-0000-4000-8000-000000000311")].review_summary,
        ReviewSummary::Approved
    );
    assert_eq!(
        item.revisions[&revision("rev_00000000-0000-4000-8000-000000000312")].review_summary,
        ReviewSummary::Rejected
    );
    let mixed = &item.revisions[&revision("rev_00000000-0000-4000-8000-000000000313")];
    assert_eq!(mixed.review_summary, ReviewSummary::Mixed);
    assert_eq!(mixed.review_event_ids.len(), 2);
    assert_eq!(
        item.revisions[&revision("rev_00000000-0000-4000-8000-000000000314")].review_summary,
        ReviewSummary::Unreviewed
    );
    assert_eq!(item.reviews.len(), 4);
    assert!(projection.diagnostics.is_empty());
}

#[test]
fn publication_fixture_covers_withdraw_supersede_conflict_and_causal_merge() {
    let projection = reduce(&reducer_events("publication-lifecycle.json"));
    let space = &projection.spaces[&space("spc_00000000-0000-4000-8000-000000000004")];

    let deprecated = &space.contexts[&context("ctx_00000000-0000-4000-8000-000000000401")];
    assert!(matches!(
        deprecated.governance,
        ContextGovernanceStatus::Deprecated { .. }
    ));
    assert_eq!(
        deprecated.revisions[&revision("rev_00000000-0000-4000-8000-000000000411")].lifecycle,
        RevisionLifecycle::Superseded
    );
    assert_eq!(
        deprecated.revisions[&revision("rev_00000000-0000-4000-8000-000000000412")].lifecycle,
        RevisionLifecycle::Deprecated
    );

    let conflicted = &space.contexts[&context("ctx_00000000-0000-4000-8000-000000000402")];
    assert!(matches!(
        conflicted.governance,
        ContextGovernanceStatus::GovernanceConflict { .. }
    ));
    assert_eq!(conflicted.publication_heads.len(), 2);
    assert_eq!(
        conflicted.revisions[&revision("rev_00000000-0000-4000-8000-000000000414")].lifecycle,
        RevisionLifecycle::Candidate
    );

    let merged = &space.contexts[&context("ctx_00000000-0000-4000-8000-000000000403")];
    assert!(matches!(
        merged.governance,
        ContextGovernanceStatus::Accepted {
            revision_id,
            ..
        } if revision_id == revision("rev_00000000-0000-4000-8000-000000000417")
    ));
    assert_eq!(merged.publication_heads.len(), 1);
    assert_eq!(
        merged.revisions[&revision("rev_00000000-0000-4000-8000-000000000415")].lifecycle,
        RevisionLifecycle::Superseded
    );
    assert_eq!(
        merged.revisions[&revision("rev_00000000-0000-4000-8000-000000000416")].lifecycle,
        RevisionLifecycle::Superseded
    );
    assert_eq!(
        merged.revisions[&revision("rev_00000000-0000-4000-8000-000000000417")].lifecycle,
        RevisionLifecycle::Accepted
    );
    assert!(projection.diagnostics.is_empty());
}

#[test]
fn duplicate_event_ids_are_quarantined_as_a_set() {
    let mut events = reducer_events("intent-branch-merge.json");
    events.push(events[1].clone());
    let projection = reduce(&events);

    assert!(
        projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::DuplicateEventId })
    );
    let intent = &projection.spaces[&space("spc_00000000-0000-4000-8000-000000000001")].intent;
    assert!(
        !intent
            .revisions
            .contains_key(&revision("rev_00000000-0000-4000-8000-000000000102"))
    );
    assert!(
        !intent
            .revisions
            .contains_key(&revision("rev_00000000-0000-4000-8000-000000000104"))
    );
}

#[test]
fn dangling_revision_edges_propagate_quarantine_to_dependants() {
    let mut values = fixture_values("context-branch-merge.json");
    values[2]["revision"]["parent_revision_ids"] =
        serde_json::json!(["rev_00000000-0000-4000-8000-000000000299"]);
    let projection = reduce(&values_to_reducer_events(values));
    let item = &projection.spaces[&space("spc_00000000-0000-4000-8000-000000000002")].contexts
        [&context("ctx_00000000-0000-4000-8000-000000000201")];

    assert!(
        !item
            .revisions
            .contains_key(&revision("rev_00000000-0000-4000-8000-000000000212"))
    );
    assert!(
        !item
            .revisions
            .contains_key(&revision("rev_00000000-0000-4000-8000-000000000214"))
    );
    assert!(
        projection.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ReducerDiagnosticCode::InvalidRevisionReference
        })
    );
}

#[test]
fn revision_and_publication_cycles_never_produce_heads() {
    let mut revision_values = fixture_values("context-branch-merge.json");
    revision_values[1]["revision"]["parent_revision_ids"] =
        serde_json::json!(["rev_00000000-0000-4000-8000-000000000212"]);
    let revision_projection = reduce(&values_to_reducer_events(revision_values));
    assert!(
        revision_projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::RevisionCycle })
    );
    assert!(
        revision_projection.spaces[&space("spc_00000000-0000-4000-8000-000000000002")]
            .contexts
            .is_empty()
    );

    let mut publication_values = fixture_values("publication-lifecycle.json");
    publication_values[3]["publication"]["previous_publication_ids"] =
        serde_json::json!(["pub_00000000-0000-4000-8000-000000000423"]);
    let publication_projection = reduce(&values_to_reducer_events(publication_values));
    let item = &publication_projection.spaces[&space("spc_00000000-0000-4000-8000-000000000004")]
        .contexts[&context("ctx_00000000-0000-4000-8000-000000000401")];
    assert!(matches!(
        item.governance,
        ContextGovernanceStatus::Unpublished
    ));
    assert!(item.publication_heads.is_empty());
    assert!(
        publication_projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::PublicationCycle })
    );
}

#[test]
fn duplicate_revision_and_ambiguous_context_owner_invalidate_every_definition() {
    let mut duplicate_values = fixture_values("context-branch-merge.json");
    let mut duplicate = duplicate_values[1].clone();
    duplicate["event_id"] = serde_json::json!("evt_00000000-0000-4000-8000-000000000099");
    duplicate["revision"]["evidence"][0]["evidence_id"] =
        serde_json::json!("evd_00000000-0000-4000-8000-000000000299");
    duplicate_values.push(duplicate);
    let duplicate_projection = reduce(&values_to_reducer_events(duplicate_values));
    assert!(
        duplicate_projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::DuplicateRevisionId })
    );
    assert!(
        duplicate_projection.spaces[&space("spc_00000000-0000-4000-8000-000000000002")]
            .contexts
            .is_empty()
    );

    let mut owner_values = fixture_values("review-summaries.json");
    let mut other_space = fixture_values("publication-lifecycle.json");
    owner_values.push(other_space.remove(0));
    owner_values[5]["space_id"] = serde_json::json!("spc_00000000-0000-4000-8000-000000000004");
    let owner_projection = reduce(&values_to_reducer_events(owner_values));
    assert!(
        owner_projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::AmbiguousContextOwner })
    );
    assert!(
        owner_projection.spaces[&space("spc_00000000-0000-4000-8000-000000000003")]
            .contexts
            .is_empty()
    );
}

#[test]
fn annotations_do_not_participate_in_reduction() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/reducer/v1/publication-lifecycle.json");
    let mut values: Vec<Value> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let plain: Vec<_> = values
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value::<Event>(value)
                .unwrap()
                .reducer_event()
                .unwrap()
        })
        .collect();
    for (index, value) in values.iter_mut().enumerate() {
        value["annotations"] = serde_json::json!({
            "created_at": format!("2099-12-31T23:59:{index:02}Z"),
            "producer": "arbitrary-producer",
            "origin_hint": {"path": format!("arbitrary/{index}")}
        });
    }
    let annotated: Vec<_> = values
        .into_iter()
        .map(|value| {
            serde_json::from_value::<Event>(value)
                .unwrap()
                .reducer_event()
                .unwrap()
        })
        .collect();

    assert_eq!(
        serde_json::to_vec(&reduce(&plain)).unwrap(),
        serde_json::to_vec(&reduce(&annotated)).unwrap()
    );
}

#[test]
fn topic_and_scope_overlap_produce_only_the_expected_candidate() {
    let projection = reduce(&reducer_events("semantic-conflicts.json"));

    assert_eq!(projection.semantic_conflict_candidates.len(), 1);
    let candidate = &projection.semantic_conflict_candidates[0];
    assert_eq!(candidate.topic_key, "search/result-visibility");
    assert_eq!(
        candidate
            .participants
            .iter()
            .map(|participant| participant.context_id)
            .collect::<Vec<_>>(),
        vec![
            context("ctx_00000000-0000-4000-8000-000000000501"),
            context("ctx_00000000-0000-4000-8000-000000000502"),
        ]
    );
    assert!(projection.diagnostics.is_empty());
}

#[test]
fn confirmed_open_conflict_blocks_both_participants_until_one_resolution_head() {
    let mut events = reducer_events("semantic-conflicts.json");
    events.pop();
    let projection = reduce(&events);
    let conflict_id = ConflictId::from_str("cnf_00000000-0000-4000-8000-000000000531").unwrap();
    let conflict = &projection.semantic_conflicts[&conflict_id];

    assert!(matches!(
        &conflict.status,
        SemanticConflictStatus::Open { reasons }
            if reasons.contains(&SemanticConflictOpenReason::NoResolution)
    ));
    for context_id in [
        context("ctx_00000000-0000-4000-8000-000000000501"),
        context("ctx_00000000-0000-4000-8000-000000000502"),
    ] {
        let eligibility = &projection.spaces[&space("spc_00000000-0000-4000-8000-000000000005")]
            .contexts[&context_id]
            .auto_injection;
        assert!(!eligibility.eligible);
        assert!(
            eligibility
                .blockers
                .contains(&AutoInjectionBlocker::UnresolvedSemanticConflict(
                    conflict_id
                ))
        );
    }

    let resolved = reduce(&reducer_events("semantic-conflicts.json"));
    assert!(matches!(
        resolved.semantic_conflicts[&conflict_id].status,
        SemanticConflictStatus::Resolved { resolution_id }
            if resolution_id
                == ResolutionId::from_str("rsl_00000000-0000-4000-8000-000000000541").unwrap()
    ));
    assert!(
        resolved.spaces[&space("spc_00000000-0000-4000-8000-000000000005")].contexts
            [&context("ctx_00000000-0000-4000-8000-000000000501")]
            .auto_injection
            .eligible
    );
}

#[test]
fn concurrent_resolution_heads_keep_the_conflict_open() {
    let mut values = fixture_values("semantic-conflicts.json");
    let mut concurrent = values.last().unwrap().clone();
    concurrent["event_id"] = serde_json::json!("evt_00000000-0000-4000-8000-000000000062");
    concurrent["resolution"]["resolution_id"] =
        serde_json::json!("rsl_00000000-0000-4000-8000-000000000542");
    values.push(concurrent);
    let projection = reduce(&values_to_reducer_events(values));
    let conflict = &projection.semantic_conflicts
        [&ConflictId::from_str("cnf_00000000-0000-4000-8000-000000000531").unwrap()];

    assert_eq!(conflict.resolution_heads.len(), 2);
    assert!(matches!(
        &conflict.status,
        SemanticConflictStatus::Open { reasons }
            if reasons.contains(&SemanticConflictOpenReason::ResolutionConflict)
    ));
}

#[test]
fn unconverged_publications_block_resolution_even_when_all_heads_are_cited() {
    let mut values = fixture_values("semantic-conflicts.json");
    for (event_id, publication_id) in [
        (
            "evt_00000000-0000-4000-8000-000000000063",
            "pub_00000000-0000-4000-8000-000000000525",
        ),
        (
            "evt_00000000-0000-4000-8000-000000000064",
            "pub_00000000-0000-4000-8000-000000000526",
        ),
    ] {
        values.push(serde_json::json!({
            "schema_version": "1",
            "event_id": event_id,
            "event_type": "context.publication_changed",
            "space_id": "spc_00000000-0000-4000-8000-000000000005",
            "context_id": "ctx_00000000-0000-4000-8000-000000000501",
            "publication": {
                "publication_id": publication_id,
                "previous_publication_ids": ["pub_00000000-0000-4000-8000-000000000521"],
                "action": "publish",
                "revision_id": "rev_00000000-0000-4000-8000-000000000511",
                "review_event_ids": []
            }
        }));
    }
    values.push(serde_json::json!({
        "schema_version": "1",
        "event_id": "evt_00000000-0000-4000-8000-000000000065",
        "event_type": "semantic_conflict.resolution_added",
        "space_id": "spc_00000000-0000-4000-8000-000000000005",
        "conflict_id": "cnf_00000000-0000-4000-8000-000000000531",
        "resolution": {
            "resolution_id": "rsl_00000000-0000-4000-8000-000000000543",
            "previous_resolution_ids": ["rsl_00000000-0000-4000-8000-000000000541"],
            "related_publication_ids": [
                "pub_00000000-0000-4000-8000-000000000525",
                "pub_00000000-0000-4000-8000-000000000526",
                "pub_00000000-0000-4000-8000-000000000522"
            ],
            "results": [
                {"context_id":"ctx_00000000-0000-4000-8000-000000000501","revision_id":"rev_00000000-0000-4000-8000-000000000511","outcome":"retained"},
                {"context_id":"ctx_00000000-0000-4000-8000-000000000502","revision_id":"rev_00000000-0000-4000-8000-000000000512","outcome":"revised"}
            ],
            "rationale": "Cite all current heads without hiding governance divergence"
        }
    }));
    let projection = reduce(&values_to_reducer_events(values));
    let conflict = &projection.semantic_conflicts
        [&ConflictId::from_str("cnf_00000000-0000-4000-8000-000000000531").unwrap()];

    assert_eq!(conflict.resolution_heads.len(), 1);
    assert!(matches!(
        &conflict.status,
        SemanticConflictStatus::Open { reasons }
            if reasons == &[
                SemanticConflictOpenReason::PublicationHeadsNotConverged
            ].into_iter().collect()
    ));
}

#[test]
fn quarantine_closure_reaches_publications_conflict_and_resolution() {
    let mut values = fixture_values("semantic-conflicts.json");
    values[3]["revision"]["evidence"][0]["evidence_id"] =
        values[1]["revision"]["evidence"][0]["evidence_id"].clone();
    let projection = reduce(&values_to_reducer_events(values));

    for code in [
        ReducerDiagnosticCode::DuplicateEvidenceId,
        ReducerDiagnosticCode::InvalidPublicationReference,
        ReducerDiagnosticCode::InvalidSemanticConflictReference,
        ReducerDiagnosticCode::InvalidResolutionReference,
    ] {
        assert!(
            projection
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == code)
        );
    }
    for event_id in [52, 53, 54, 55, 60, 61] {
        let id = format!("evt_00000000-0000-4000-8000-{event_id:012}");
        assert!(
            projection
                .quarantined_event_ids
                .contains(&sctx_event_schema::EventId::from_str(&id).unwrap())
        );
    }
    assert!(projection.semantic_conflicts.is_empty());
}

#[test]
fn duplicate_conflict_and_resolution_ids_quarantine_every_definition() {
    let mut conflict_values = fixture_values("semantic-conflicts.json");
    let mut duplicate_conflict = conflict_values[9].clone();
    duplicate_conflict["event_id"] = serde_json::json!("evt_00000000-0000-4000-8000-000000000066");
    conflict_values.push(duplicate_conflict);
    let conflict_projection = reduce(&values_to_reducer_events(conflict_values));
    assert!(conflict_projection.semantic_conflicts.is_empty());
    assert!(
        conflict_projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::DuplicateConflictId })
    );
    assert!(conflict_projection.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == ReducerDiagnosticCode::InvalidResolutionReference
    }));

    let mut resolution_values = fixture_values("semantic-conflicts.json");
    let mut duplicate_resolution = resolution_values[10].clone();
    duplicate_resolution["event_id"] =
        serde_json::json!("evt_00000000-0000-4000-8000-000000000067");
    resolution_values.push(duplicate_resolution);
    let resolution_projection = reduce(&values_to_reducer_events(resolution_values));
    let conflict = &resolution_projection.semantic_conflicts
        [&ConflictId::from_str("cnf_00000000-0000-4000-8000-000000000531").unwrap()];
    assert!(conflict.resolution_heads.is_empty());
    assert!(matches!(
        &conflict.status,
        SemanticConflictStatus::Open { reasons }
            if reasons.contains(&SemanticConflictOpenReason::NoResolution)
    ));
    assert!(
        resolution_projection
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ReducerDiagnosticCode::DuplicateResolutionId })
    );
}

#[test]
fn resolution_cycles_and_their_dependants_never_become_heads() {
    let mut values = fixture_values("semantic-conflicts.json");
    values[10]["resolution"]["previous_resolution_ids"] =
        serde_json::json!(["rsl_00000000-0000-4000-8000-000000000542"]);
    let mut second = values[10].clone();
    second["event_id"] = serde_json::json!("evt_00000000-0000-4000-8000-000000000068");
    second["resolution"]["resolution_id"] =
        serde_json::json!("rsl_00000000-0000-4000-8000-000000000542");
    second["resolution"]["previous_resolution_ids"] =
        serde_json::json!(["rsl_00000000-0000-4000-8000-000000000541"]);
    values.push(second);
    let projection = reduce(&values_to_reducer_events(values));
    let conflict = &projection.semantic_conflicts
        [&ConflictId::from_str("cnf_00000000-0000-4000-8000-000000000531").unwrap()];

    assert!(conflict.resolutions.is_empty());
    assert!(conflict.resolution_heads.is_empty());
    assert!(
        projection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ReducerDiagnosticCode::ResolutionCycle)
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_event_permutations_have_byte_identical_projection(
        keys in prop::collection::vec(any::<u64>(), 50)
    ) {
        let events = all_reducer_events();
        let expected = serde_json::to_vec(&reduce(&events)).unwrap();
        let mut keyed: Vec<_> = events.into_iter().zip(keys).enumerate().collect();
        keyed.sort_by_key(|(original_index, (_, key))| (*key, *original_index));
        let permuted: Vec<_> = keyed.into_iter().map(|(_, (event, _))| event).collect();

        prop_assert_eq!(serde_json::to_vec(&reduce(&permuted)).unwrap(), expected);
    }

    #[test]
    fn quarantine_is_also_permutation_invariant(
        keys in prop::collection::vec(any::<u64>(), 51)
    ) {
        let mut events = all_reducer_events();
        events.push(events[1].clone());
        let expected = serde_json::to_vec(&reduce(&events)).unwrap();
        let mut keyed: Vec<_> = events.into_iter().zip(keys).enumerate().collect();
        keyed.sort_by_key(|(original_index, (_, key))| (*key, *original_index));
        let permuted: Vec<_> = keyed.into_iter().map(|(_, (event, _))| event).collect();

        prop_assert_eq!(serde_json::to_vec(&reduce(&permuted)).unwrap(), expected);
    }

    #[test]
    fn full_conflict_quarantine_closure_is_permutation_invariant(
        keys in prop::collection::vec(any::<u64>(), 50)
    ) {
        let mut values = fixture_values("semantic-conflicts.json");
        let duplicate_evidence = values[1]["revision"]["evidence"][0]["evidence_id"].clone();
        values[3]["revision"]["evidence"][0]["evidence_id"] = duplicate_evidence;
        let mut events = all_reducer_events();
        events.splice(39.., values_to_reducer_events(values));
        let expected = serde_json::to_vec(&reduce(&events)).unwrap();
        let mut keyed: Vec<_> = events.into_iter().zip(keys).enumerate().collect();
        keyed.sort_by_key(|(original_index, (_, key))| (*key, *original_index));
        let permuted: Vec<_> = keyed.into_iter().map(|(_, (event, _))| event).collect();

        prop_assert_eq!(serde_json::to_vec(&reduce(&permuted)).unwrap(), expected);
    }
}
