use std::{fs, path::Path, str::FromStr};

use proptest::prelude::*;
use sctx_event_schema::{
    ContextGovernanceStatus, ContextId, Event, ReducerDiagnosticCode, ReducerEvent, ReviewSummary,
    RevisionId, RevisionLifecycle, SpaceId, reduce,
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

fn reducer_events(name: &str) -> Vec<ReducerEvent> {
    fixture_events(name)
        .iter()
        .map(|event| {
            event
                .reducer_event()
                .expect("reducer fixture must not contain semantic-conflict events")
        })
        .collect()
}

fn all_reducer_events() -> Vec<ReducerEvent> {
    [
        "intent-branch-merge.json",
        "context-branch-merge.json",
        "review-summaries.json",
        "publication-lifecycle.json",
    ]
    .into_iter()
    .flat_map(reducer_events)
    .collect()
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_event_permutations_have_byte_identical_projection(
        keys in prop::collection::vec(any::<u64>(), 36)
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
        keys in prop::collection::vec(any::<u64>(), 37)
    ) {
        let mut events = all_reducer_events();
        events.push(events[1].clone());
        let expected = serde_json::to_vec(&reduce(&events)).unwrap();
        let mut keyed: Vec<_> = events.into_iter().zip(keys).enumerate().collect();
        keyed.sort_by_key(|(original_index, (_, key))| (*key, *original_index));
        let permuted: Vec<_> = keyed.into_iter().map(|(_, (event, _))| event).collect();

        prop_assert_eq!(serde_json::to_vec(&reduce(&permuted)).unwrap(), expected);
    }
}
