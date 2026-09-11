//! Locally derived Context lifecycle: `supersedes`, unresolved semantic conflicts, structured
//! `recheck_when` staleness, and the configured `[context_ttl]` policy.
//!
//! Every state under test is derived on this machine. None of it rewrites an accepted Git fact:
//! the assertions are about retrieval eligibility, ranking demotion, and explanation.

use sctx_domain::{
    Applicability, ConflictParticipant, ConflictResolutionDraft, ConflictResolutionResult,
    ContextId, ContextKind, ContextRelation, ContextRelationKind, ContextRevisionDraft,
    EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft,
    PublicationId, ResolutionOutcome, RevisionId, SpaceId, TaskId, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS, ContextPackDetailLevel, ContextPackMode,
    ContextTtlSettings, STALE_SCORE_MULTIPLIER_BASIS_POINTS, SearchEngine, SearchRequest,
    TaskContextRequest,
};
use tempfile::TempDir;

/// Wall-clock second far past any fixture commit time, used as an explicit TTL evaluation clock.
const FAR_FUTURE_UNIX_SECONDS: i64 = 4_000_000_000;

const NEEDLE: &str = "lifecycleguardneedle";

/// The one file every Context in this fixture is recorded against, and the Session opens it.
///
/// None of these tests is about retrieval: they are about what the derived lifecycle does to a
/// Context that has already been retrieved -- superseded, in an unresolved conflict, stale, or past
/// its configured lifetime. Under ADR-0007 a Context has to be anchored to be retrieved at all, so
/// one shared file is the cheapest way to keep every test pointed at its own subject. It is
/// deliberately one file and not one each: which Context is refused is decided by lifecycle, never
/// by which file the Session happened to open.
const LIFECYCLE_REPOSITORY: &str = "Server";
const LIFECYCLE_FILE: &str = "src/lifecycle/Guard.rs";

struct Fixture {
    _temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    space_id: SpaceId,
}

fn intent() -> IntentSnapshot {
    IntentSnapshot {
        title: "LifecycleGuard".to_owned(),
        problem: format!("{NEEDLE} keeps derived Context lifecycle honest"),
        desired_outcome: format!("{NEEDLE} retrieval stays explainable"),
        in_scope: vec![NEEDLE.to_owned()],
        out_of_scope: vec!["LifecycleGuardExcluded".to_owned()],
        acceptance_conditions: vec!["LifecycleGuardAccepted".to_owned()],
        domain_terms: vec!["lifecycle".to_owned()],
    }
}

fn draft(
    kind: ContextKind,
    statement: &str,
    relations: Vec<ContextRelation>,
) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind,
        topic_key: Some("lifecycle/derived-state".to_owned()),
        problem_view: None,
        statement: statement.to_owned(),
        rationale: "the accepted fixture captures durable engineering behavior".to_owned(),
        applicability: Applicability {
            domains: vec!["lifecycle".to_owned()],
            platforms: vec!["server".to_owned()],
            conditions: vec!["active".to_owned()],
        },
        assumptions: vec!["fixture inputs remain stable".to_owned()],
        recheck_when: vec!["the fixture contract changes".to_owned()],
        hints: Vec::new(),
        relations,
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the lifecycle fixture is safe for retrieval".to_owned(),
            content: serde_json::json!({
                "command": "cargo test -p sctx-search --test context_lifecycle",
                "actual": "passed"
            }),
            interpretation: "the Context has complete local evidence".to_owned(),
            limitations: vec!["synthetic fixture".to_owned()],
        }],
    }
}

fn append(store: &GitStore, event: Event) {
    store.append_event(AppendRequest::event(event)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let store =
            GitStore::bootstrap_local(temporary.path().join("lifecycle-installation")).unwrap();
        let created = Event::space_created(intent(), None).unwrap();
        let EventPayload::SpaceCreated { space_id, .. } = created.payload() else {
            unreachable!()
        };
        let space_id = *space_id;
        append(&store, created);
        let index = ProjectionIndex::for_store(&store);
        Self {
            _temporary: temporary,
            store,
            index,
            space_id,
        }
    }

    fn accept(&self, revision: ContextRevisionDraft) -> (ContextId, RevisionId, PublicationId) {
        let added = Event::context_revision_added(self.space_id, revision, None).unwrap();
        let EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } = added.payload()
        else {
            unreachable!()
        };
        let (context_id, revision_id) = (*context_id, revision.revision_id);
        append(&self.store, added);
        let published = Event::publication_changed(
            self.space_id,
            context_id,
            PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: PublicationAction::Publish,
                revision_id,
                review_event_ids: Vec::new(),
            },
            None,
        )
        .unwrap();
        let EventPayload::ContextPublicationChanged { publication, .. } = published.payload()
        else {
            unreachable!()
        };
        let publication_id = publication.publication_id;
        append(&self.store, published);
        append(
            &self.store,
            Event::engineering_reference_recorded(
                context_id,
                revision_id,
                sctx_domain::EngineeringReferenceDraft {
                    repository_id: LIFECYCLE_REPOSITORY.parse().unwrap(),
                    artifact_kind: sctx_domain::ArtifactKind::File,
                    relation: sctx_domain::ReferenceRelation::Implements,
                    locator: sctx_domain::ArtifactLocator::File {
                        path: sctx_domain::RepoRelativePath::new(LIFECYCLE_FILE).unwrap(),
                    },
                    supports: "the lifecycle fixture anchors every Context to one file".to_owned(),
                    limitations: vec!["synthetic fixture".to_owned()],
                },
                None,
            )
            .unwrap(),
        );
        (context_id, revision_id, publication_id)
    }

    fn engine(&self) -> SearchEngine {
        self.index.synchronize().unwrap();
        SearchEngine::new(self.index.clone())
    }
}

fn pack(engine: &SearchEngine, mode: ContextPackMode) -> Vec<sctx_search::TaskContextItem> {
    let mut request = TaskContextRequest::automatic(TaskId::new(), task(), Vec::new(), 8_000);
    request.mode = mode;
    request.signal_history = vec![sctx_domain::TaskSignal {
        kind: sctx_domain::TaskSignalKind::Workspace,
        content: format!("{LIFECYCLE_REPOSITORY}:{LIFECYCLE_FILE}"),
    }];
    engine.task_context_pack(&request).unwrap().items
}

fn task() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: NEEDLE.to_owned(),
        current_direction: Some(NEEDLE.to_owned()),
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn search(engine: &SearchEngine) -> Vec<sctx_search::SearchResult> {
    engine
        .search(&SearchRequest {
            query: NEEDLE.to_owned(),
            ..SearchRequest::default()
        })
        .unwrap()
        .results
}

#[test]
fn supersedes_relation_excludes_the_target_from_injection_but_keeps_it_searchable() {
    let fixture = Fixture::new();
    let (superseded, _, _) = fixture.accept(draft(
        ContextKind::Decision,
        &format!("{NEEDLE} legacy routing decision"),
        Vec::new(),
    ));
    let (superseding, _, _) = fixture.accept(draft(
        ContextKind::Decision,
        &format!("{NEEDLE} current routing decision"),
        vec![ContextRelation {
            target_context_id: superseded,
            kind: ContextRelationKind::Supersedes,
            rationale: "the current decision replaces the legacy routing decision".to_owned(),
            supports: vec!["the legacy decision no longer describes the shipped code".to_owned()],
        }],
    ));

    let engine = fixture.engine();
    let injected = pack(&engine, ContextPackMode::AutomaticInjection)
        .into_iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        injected.contains(&superseding),
        "the superseding Context stays injectable"
    );
    assert!(
        !injected.contains(&superseded),
        "a superseded Context is never injected automatically"
    );

    let results = search(&engine);
    let target = results
        .iter()
        .find(|result| result.context_id == superseded)
        .expect("a superseded Context stays searchable");
    assert_eq!(target.derived_state.superseded_by, Some(superseding));
    assert!(!target.auto_injection_eligible);
    let winner = results
        .iter()
        .find(|result| result.context_id == superseding)
        .expect("the superseding Context is searchable");
    assert_eq!(winner.derived_state.superseded_by, None);
    assert!(winner.auto_injection_eligible);
}

#[test]
#[allow(clippy::too_many_lines)]
fn unresolved_semantic_conflict_demotes_a_participant_and_resolution_restores_it() {
    let fixture = Fixture::new();
    let (left, left_revision, left_publication) = fixture.accept(draft(
        ContextKind::Decision,
        &format!("{NEEDLE} retries are safe to repeat"),
        Vec::new(),
    ));
    let (right, right_revision, right_publication) = fixture.accept(draft(
        ContextKind::Decision,
        &format!("{NEEDLE} retries must never repeat"),
        Vec::new(),
    ));
    let opened = Event::semantic_conflict_opened(
        fixture.space_id,
        sctx_domain::SemanticConflictDraft {
            participants: vec![
                ConflictParticipant {
                    context_id: left,
                    revision_id: left_revision,
                    publication_id: left_publication,
                },
                ConflictParticipant {
                    context_id: right,
                    revision_id: right_revision,
                    publication_id: right_publication,
                },
            ],
            reason: "the two accepted decisions disagree about retry safety".to_owned(),
            applicability: Applicability {
                domains: vec!["lifecycle".to_owned()],
                platforms: vec!["server".to_owned()],
                conditions: vec!["active".to_owned()],
            },
        },
        None,
    )
    .unwrap();
    let EventPayload::SemanticConflictOpened { conflict, .. } = opened.payload() else {
        unreachable!()
    };
    let conflict_id = conflict.conflict_id;
    append(&fixture.store, opened);

    let engine = fixture.engine();
    let conflicted = pack(&engine, ContextPackMode::Explicit)
        .into_iter()
        .find(|item| item.context.context_id == left)
        .expect("an explicit Pack still returns a conflict participant");
    assert_eq!(
        conflicted.context.derived_state.demotion_basis_points,
        Some(CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS)
    );
    assert!(
        pack(&engine, ContextPackMode::AutomaticInjection).is_empty(),
        "an unresolved semantic conflict still blocks automatic injection outright"
    );

    let mut request = TaskContextRequest::automatic(TaskId::new(), task(), Vec::new(), 8_000);
    request.mode = ContextPackMode::Explicit;
    let compact = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Compact)
        .unwrap();
    let compact_item = compact
        .compact_items
        .iter()
        .find(|item| item.context_id == left)
        .expect("the compact projection keeps the conflict participant");
    assert!(
        compact_item.why.iter().any(
            |reason| reason.contains("Unresolved semantic conflict with")
                && reason.contains(&right.to_string())
        ),
        "the compact reason names the other side: {:?}",
        compact_item.why
    );

    append(
        &fixture.store,
        Event::semantic_conflict_resolution_added(
            fixture.space_id,
            conflict_id,
            ConflictResolutionDraft {
                previous_resolution_ids: Vec::new(),
                related_publication_ids: vec![left_publication, right_publication],
                results: vec![
                    ConflictResolutionResult {
                        context_id: left,
                        revision_id: left_revision,
                        outcome: ResolutionOutcome::Retained,
                    },
                    ConflictResolutionResult {
                        context_id: right,
                        revision_id: right_revision,
                        outcome: ResolutionOutcome::Retained,
                    },
                ],
                rationale: "both decisions apply to disjoint retry classes".to_owned(),
            },
            None,
        )
        .unwrap(),
    );

    let engine = fixture.engine();
    let restored = pack(&engine, ContextPackMode::Explicit)
        .into_iter()
        .find(|item| item.context.context_id == left)
        .expect("the resolved participant is still returned");
    assert_eq!(restored.context.derived_state.demotion_basis_points, None);
    assert!(restored.context.conflicts.is_empty());
    let injected = pack(&engine, ContextPackMode::AutomaticInjection)
        .into_iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        injected.contains(&left) && injected.contains(&right),
        "resolution restores automatic injection for both sides"
    );
}

#[test]
fn recorded_stale_reason_demotes_an_item_and_explains_itself() {
    let fixture = Fixture::new();
    let (context_id, _, _) = fixture.accept(draft(
        ContextKind::Decision,
        &format!("{NEEDLE} the release branch pins the router contract"),
        Vec::new(),
    ));
    fixture.index.synchronize().unwrap();
    fixture
        .index
        .record_stale_reasons(&[(
            context_id.to_string(),
            Some("branch_advanced: main moved from abc1234 to def5678".to_owned()),
        )])
        .unwrap();

    let engine = SearchEngine::new(fixture.index.clone());
    let item = pack(&engine, ContextPackMode::AutomaticInjection)
        .into_iter()
        .find(|item| item.context.context_id == context_id)
        .expect("a stale Context is demoted, not withheld");
    assert_eq!(
        item.context.derived_state.demotion_basis_points,
        Some(STALE_SCORE_MULTIPLIER_BASIS_POINTS)
    );
    assert!(item.context.auto_injection_eligible);

    let mut request = TaskContextRequest::automatic(TaskId::new(), task(), Vec::new(), 8_000);
    request.mode = ContextPackMode::AutomaticInjection;
    request.signal_history = vec![sctx_domain::TaskSignal {
        kind: sctx_domain::TaskSignalKind::Workspace,
        content: format!("{LIFECYCLE_REPOSITORY}:{LIFECYCLE_FILE}"),
    }];
    let compact = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Compact)
        .unwrap();
    let compact_item = compact
        .compact_items
        .iter()
        .find(|item| item.context_id == context_id)
        .expect("the compact projection keeps the stale Context");
    assert!(
        compact_item
            .why
            .iter()
            .any(|reason| reason.contains("Possibly stale") && reason.contains("branch_advanced")),
        "the compact reason explains the staleness: {:?}",
        compact_item.why
    );
    assert_eq!(
        compact_item.derived_state.stale_reason.as_deref(),
        Some("branch_advanced: main moved from abc1234 to def5678")
    );
}

#[test]
fn expired_context_ttl_marks_a_validation_historical_and_keeps_it_searchable() {
    let fixture = Fixture::new();
    let (validation, _, _) = fixture.accept(draft(
        ContextKind::Validation,
        &format!("{NEEDLE} the debug build passed on the release branch"),
        Vec::new(),
    ));
    let (decision, _, _) = fixture.accept(draft(
        ContextKind::Decision,
        &format!("{NEEDLE} the router contract stays versioned"),
        Vec::new(),
    ));

    fixture.index.synchronize().unwrap();
    let engine = SearchEngine::new(fixture.index.clone()).with_context_ttl(ContextTtlSettings {
        validation_seconds: Some(30 * 86_400),
        progress_seconds: Some(14 * 86_400),
        now_unix_seconds: Some(FAR_FUTURE_UNIX_SECONDS),
    });

    let injected = pack(&engine, ContextPackMode::AutomaticInjection)
        .into_iter()
        .map(|item| item.context.context_id)
        .collect::<Vec<_>>();
    assert!(
        injected.contains(&decision),
        "no configured lifetime applies to a decision"
    );
    assert!(
        !injected.contains(&validation),
        "an expired validation is never injected automatically"
    );

    let results = search(&engine);
    let expired = results
        .iter()
        .find(|result| result.context_id == validation)
        .expect("a historical Context stays searchable");
    assert!(
        expired
            .derived_state
            .historical_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("context_ttl")),
        "search explains why the Context is historical: {:?}",
        expired.derived_state.historical_reason
    );
    assert!(!expired.auto_injection_eligible);

    let unset = SearchEngine::new(fixture.index.clone());
    assert!(
        pack(&unset, ContextPackMode::AutomaticInjection)
            .iter()
            .any(|item| item.context.context_id == validation),
        "an unset [context_ttl] expires nothing"
    );
}
