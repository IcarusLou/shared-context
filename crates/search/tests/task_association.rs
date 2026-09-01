use std::sync::Arc;

use sctx_domain::{
    Applicability, CandidateConfirmationOperation, CandidateConfirmationPlan,
    CandidateConfirmationPrimaryReference, CandidatePrimarySelection, ConflictParticipant,
    ContextId, ContextKind, ContextRevisionDraft, ContextSpaceAssociationDraft,
    ContextSpaceAssociationOrigin, EvidenceSnapshotDraft, EvidenceType, OptionalCandidateEdits,
    PublicationAction, PublicationDraft, RevisionId, SemanticConflictDraft, SpaceId, SubmissionId,
    TaskId, TaskSessionId, TaskSignal, TaskSignalKind, TaskSpaceAssociation, WorkEpisodeId,
    WorkEpisodeRef, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, CandidateSubmissionRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    AutomaticQueryTokenExplanation, AutomaticQueryTokenFilter, ContextPackDetailLevel,
    ContextPackMode, ContextStatus, IntentConflictActor, IntentConflictDecision,
    IntentConflictHandoffExplanation, IntentConflictKind, IntentConflictSelection,
    IntentConflictValidation, IntentScopeConflictExplanation, IntentScopeConflictKind,
    IntentScopeConflictPolicy, SearchEngine, SpaceAssociationRole, SpaceIntentField,
    TaskAssociationChannel, TaskAssociationFusionExplanation, TaskContextRequest,
    TaskRetrievalPath, WorkingIntentHintField, WorkingIntentHintTarget,
    estimate_task_context_payload_tokens,
};
use tempfile::TempDir;

struct AssociationFixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    feature_spaces: [SpaceId; 4],
    feature_contexts: [ContextId; 3],
    pack_contexts: [ContextId; 4],
    tied_spaces: [SpaceId; 2],
    unsafe_pack_spaces: [SpaceId; 4],
}

struct PolarityFixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    space_id: SpaceId,
    context_id: ContextId,
}

struct FusionCorpusFixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    precise_space_id: SpaceId,
    precise_context_id: ContextId,
    total_spaces: usize,
}

struct IntentHandoffFixture {
    _temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    space_id: SpaceId,
    context_id: ContextId,
    conflicted_head_ids: [RevisionId; 2],
}

fn intent(title: &str, intent_text: &str) -> sctx_domain::IntentSnapshot {
    sctx_domain::IntentSnapshot {
        title: title.to_owned(),
        problem: format!("{intent_text} problem"),
        desired_outcome: format!("{intent_text} outcome"),
        in_scope: vec![intent_text.to_owned()],
        out_of_scope: vec![format!("{title}Excluded")],
        acceptance_conditions: vec![format!("{title}Accepted")],
        domain_terms: vec![format!("{title}Term")],
    }
}

fn context(statement: &str, applicability: Applicability) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Contract,
        problem_view: None,
        hints: Vec::new(),
        topic_key: Some("task/association-fixture".to_owned()),
        statement: statement.to_owned(),
        rationale: "the accepted fixture captures durable engineering behavior".to_owned(),
        applicability,
        assumptions: vec!["fixture inputs remain stable".to_owned()],
        recheck_when: vec!["the fixture contract changes".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the association fixture is safe for retrieval".to_owned(),
            content: serde_json::json!({
                "command": "cargo test -p sctx-search --test task_association",
                "actual": "passed"
            }),
            interpretation: "the Context has complete local evidence".to_owned(),
            limitations: vec!["synthetic fixture".to_owned()],
        }],
    }
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append association fixture Event");
}

fn add_space(store: &GitStore, title: &str, intent_text: &str) -> SpaceId {
    let event = Event::space_created(intent(title, intent_text), None).unwrap();
    let space_id = match event.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(store, event);
    space_id
}

fn add_context(
    store: &GitStore,
    space_id: SpaceId,
    statement: &str,
    applicability: Applicability,
) -> (ContextId, RevisionId) {
    add_context_draft(store, space_id, context(statement, applicability))
}

fn add_context_draft(
    store: &GitStore,
    space_id: SpaceId,
    draft: ContextRevisionDraft,
) -> (ContextId, RevisionId) {
    let event = Event::context_revision_added(space_id, draft, None).unwrap();
    let ids = match event.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    };
    append(store, event);
    ids
}

fn publish(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
    previous_publication_ids: Vec<sctx_domain::PublicationId>,
    action: PublicationAction,
) -> sctx_domain::PublicationId {
    let event = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids,
            action,
            revision_id,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let publication_id = match event.payload() {
        EventPayload::ContextPublicationChanged { publication, .. } => publication.publication_id,
        _ => unreachable!(),
    };
    append(store, event);
    publication_id
}

fn add_accepted_context(
    store: &GitStore,
    space_id: SpaceId,
    statement: &str,
    applicability: Applicability,
) -> (ContextId, RevisionId, sctx_domain::PublicationId) {
    let (context_id, revision_id) = add_context(store, space_id, statement, applicability);
    let publication_id = publish(
        store,
        space_id,
        context_id,
        revision_id,
        Vec::new(),
        PublicationAction::Publish,
    );
    (context_id, revision_id, publication_id)
}

fn applicability(domain: &str, platform: &str, condition: &str) -> Applicability {
    Applicability {
        domains: vec![domain.to_owned()],
        platforms: vec![platform.to_owned()],
        conditions: vec![condition.to_owned()],
    }
}

fn polarity_fixture() -> PolarityFixture {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("polarity-installation")).unwrap();
    let event = Event::space_created(
        sctx_domain::IntentSnapshot {
            title: "RankingRequirement".to_owned(),
            problem: "Search ranking needs stable behavior".to_owned(),
            desired_outcome: "Ranking remains deterministic".to_owned(),
            in_scope: vec!["SearchRankingEngine 搜索排序".to_owned()],
            out_of_scope: vec!["支付迁移 LegacyRouterBoundary".to_owned()],
            acceptance_conditions: vec!["ranking tests remain stable".to_owned()],
            domain_terms: vec!["ranking".to_owned()],
        },
        None,
    )
    .unwrap();
    let space_id = match event.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(&store, event);
    let (context_id, _, _) = add_accepted_context(
        &store,
        space_id,
        "SearchRankingEngine keeps the ranking order stable",
        applicability("ranking", "server", "active"),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    PolarityFixture {
        _temporary: temporary,
        index,
        space_id,
        context_id,
    }
}

fn fusion_corpus_fixture() -> FusionCorpusFixture {
    const GENERIC_SPACE_COUNT: usize = 40;

    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("fusion-installation")).unwrap();
    for index in 0..GENERIC_SPACE_COUNT {
        add_space(
            &store,
            &format!("GenericImplementation{index:02}"),
            "implement shared workflow",
        );
    }
    let precise = Event::space_created(
        sctx_domain::IntentSnapshot {
            title: "PreciseProtocol".to_owned(),
            problem: "implement rare endpoint SearchV9RareEndpoint ExactResultSchema".to_owned(),
            desired_outcome: "implement rare endpoint SearchV9RareEndpoint ExactResultSchema"
                .to_owned(),
            in_scope: vec![
                "implement rare endpoint SearchV9RareEndpoint ExactResultSchema".to_owned(),
            ],
            out_of_scope: vec!["unrelated payments migration".to_owned()],
            acceptance_conditions: vec!["rare endpoint remains exact".to_owned()],
            domain_terms: vec!["precise-protocol".to_owned()],
        },
        None,
    )
    .unwrap();
    let precise_space_id = match precise.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(&store, precise);
    let (precise_context_id, _, _) = add_accepted_context(
        &store,
        precise_space_id,
        "SearchV9RareEndpoint returns ExactResultSchema",
        applicability("precise-search", "server", "active"),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    FusionCorpusFixture {
        _temporary: temporary,
        index,
        precise_space_id,
        precise_context_id,
        total_spaces: GENERIC_SPACE_COUNT + 1,
    }
}

fn handoff_intent(title: &str, outcome: &str) -> sctx_domain::IntentSnapshot {
    sctx_domain::IntentSnapshot {
        title: title.to_owned(),
        problem: "handoffsharedneedle has unresolved alternatives".to_owned(),
        desired_outcome: outcome.to_owned(),
        in_scope: vec!["handoffsharedneedle".to_owned()],
        out_of_scope: vec!["unrelated handoff exclusion".to_owned()],
        acceptance_conditions: vec!["the active Agent validates applicability".to_owned()],
        domain_terms: vec!["intent-handoff".to_owned()],
    }
}

fn intent_handoff_fixture() -> IntentHandoffFixture {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("handoff-installation")).unwrap();
    let base = Event::space_created(
        handoff_intent("HandoffBase", "establish the initial handoff boundary"),
        None,
    )
    .unwrap();
    let (space_id, parent_id) = match base.payload() {
        EventPayload::SpaceCreated {
            space_id,
            intent_revision,
        } => (*space_id, intent_revision.revision_id),
        _ => unreachable!(),
    };
    append(&store, base);
    let left = Event::intent_revision_added(
        space_id,
        vec![parent_id],
        handoff_intent("HandoffLeft", "prefer the left Context alternative"),
        None,
    )
    .unwrap();
    let left_id = match left.payload() {
        EventPayload::SpaceIntentRevisionAdded {
            intent_revision, ..
        } => intent_revision.revision_id,
        _ => unreachable!(),
    };
    append(&store, left);
    let right = Event::intent_revision_added(
        space_id,
        vec![parent_id],
        handoff_intent("HandoffRight", "prefer the right Context alternative"),
        None,
    )
    .unwrap();
    let right_id = match right.payload() {
        EventPayload::SpaceIntentRevisionAdded {
            intent_revision, ..
        } => intent_revision.revision_id,
        _ => unreachable!(),
    };
    append(&store, right);
    let (context_id, _, _) = add_accepted_context(
        &store,
        space_id,
        "contextonlyhandoffneedle Context remains safe but requires Agent validation",
        applicability("handoff", "server", "active"),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    IntentHandoffFixture {
        _temporary: temporary,
        store,
        index,
        space_id,
        context_id,
        conflicted_head_ids: [left_id, right_id],
    }
}

#[allow(clippy::too_many_lines)]
fn fixture() -> AssociationFixture {
    let temporary = tempfile::tempdir().unwrap();
    let store =
        GitStore::bootstrap_local(temporary.path().join("association-installation")).unwrap();

    let page_space = add_space(
        &store,
        "PageRequirement",
        "pageintentneedle SearchResultsPage.tsx",
    );
    let (page_context, _, _) = add_accepted_context(
        &store,
        page_space,
        "the result page keeps its persistent navigation controls",
        applicability("page-requirement", "browser", "active"),
    );
    let protocol_space = add_space(&store, "ServerProtocol", "protocolintentonly");
    let (protocol_context, _, _) = add_accepted_context(
        &store,
        protocol_space,
        "SearchV2Endpoint returns SearchResponseV2",
        applicability("server-protocol", "server", "active"),
    );
    let compatibility_space = add_space(&store, "Compatibility", "compatibilityintentonly");
    let (compatibility_context, _, _) = add_accepted_context(
        &store,
        compatibility_space,
        "LegacyCompatibilityTest verifies old client behavior",
        applicability("compatibility", "fe", "legacyclient"),
    );
    let analytics_space = add_space(&store, "Analytics", "analyticsintentonly");
    let (analytics_context, _, _) = add_accepted_context(
        &store,
        analytics_space,
        "impression semantics are governed by the analytics contract",
        applicability("analyticsdomain", "server", "production"),
    );

    let tie_alpha = add_space(&store, "TieAlpha", "tiealphaintent");
    add_accepted_context(
        &store,
        tie_alpha,
        "tieassociationneedle",
        applicability("tie-domain", "server", "active"),
    );
    let tie_bravo = add_space(&store, "TieBravo", "tiebravointent");
    add_accepted_context(
        &store,
        tie_bravo,
        "tieassociationneedle",
        applicability("tie-domain", "server", "active"),
    );

    let candidate_space = add_space(
        &store,
        "UnsafeCandidate",
        "candidateintentonly unsafepackintentneedle",
    );
    add_context(
        &store,
        candidate_space,
        "unsafeassociationneedle candidate",
        applicability("unsafe", "fe", "active"),
    );
    let deprecated_space = add_space(
        &store,
        "UnsafeDeprecated",
        "deprecatedintentonly unsafepackintentneedle",
    );
    let (deprecated_context, deprecated_revision, deprecated_publication) = add_accepted_context(
        &store,
        deprecated_space,
        "unsafeassociationneedle deprecated",
        applicability("unsafe", "fe", "active"),
    );
    publish(
        &store,
        deprecated_space,
        deprecated_context,
        deprecated_revision,
        vec![deprecated_publication],
        PublicationAction::Withdraw,
    );
    let conflict_space = add_space(
        &store,
        "UnsafeConflict",
        "conflictintentonly unsafepackintentneedle",
    );
    let (conflict_a, revision_a, publication_a) = add_accepted_context(
        &store,
        conflict_space,
        "unsafeassociationneedle enabled",
        applicability("unsafe", "fe", "active"),
    );
    let (conflict_b, revision_b, publication_b) = add_accepted_context(
        &store,
        conflict_space,
        "unsafeassociationneedle disabled",
        applicability("unsafe", "fe", "active"),
    );
    append(
        &store,
        Event::semantic_conflict_opened(
            conflict_space,
            SemanticConflictDraft {
                participants: vec![
                    ConflictParticipant {
                        context_id: conflict_a,
                        revision_id: revision_a,
                        publication_id: publication_a,
                    },
                    ConflictParticipant {
                        context_id: conflict_b,
                        revision_id: revision_b,
                        publication_id: publication_b,
                    },
                ],
                reason: "the unsafe fixture has contradictory accepted behavior".to_owned(),
                applicability: applicability("unsafe", "fe", "active"),
            },
            None,
        )
        .unwrap(),
    );
    let incomplete_space = add_space(
        &store,
        "IncompleteEvidence",
        "incompleteintentonly unsafepackintentneedle",
    );
    let mut incomplete = context(
        "incomplete evidence must not enter automatic packs",
        applicability("unsafe", "fe", "active"),
    );
    incomplete.evidence[0].limitations.clear();
    let (incomplete_context, incomplete_revision) =
        add_context_draft(&store, incomplete_space, incomplete);
    publish(
        &store,
        incomplete_space,
        incomplete_context,
        incomplete_revision,
        Vec::new(),
        PublicationAction::Publish,
    );

    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    AssociationFixture {
        _temporary: temporary,
        index,
        feature_spaces: [
            page_space,
            protocol_space,
            compatibility_space,
            analytics_space,
        ],
        feature_contexts: [protocol_context, compatibility_context, analytics_context],
        pack_contexts: [
            page_context,
            protocol_context,
            compatibility_context,
            analytics_context,
        ],
        tied_spaces: [tie_alpha, tie_bravo],
        unsafe_pack_spaces: [
            candidate_space,
            deprecated_space,
            conflict_space,
            incomplete_space,
        ],
    }
}

fn task(goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: Some(goal.to_owned()),
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

fn feature_task() -> WorkingIntentSnapshot {
    let mut task = task("pageintentneedle");
    task.current_direction = Some("featureassociationgoal".to_owned());
    task.domains = vec!["analyticsdomain".to_owned()];
    task.platforms = vec!["fe".to_owned()];
    task.constraints = vec!["legacyclient".to_owned()];
    task.acceptance_conditions = vec!["impression".to_owned()];
    task
}

fn feature_signals() -> Vec<TaskSignal> {
    vec![TaskSignal {
        kind: TaskSignalKind::TestOutcome,
        content: "LegacyCompatibilityTest succeeded".to_owned(),
    }]
}

#[test]
fn fe_task_associates_requirement_protocol_compatibility_and_analytics_spaces() {
    let fixture = fixture();
    let task_id = TaskId::new();
    let task = feature_task();
    let signals = feature_signals();
    let response = SearchEngine::new(fixture.index)
        .task_space_associations(task_id, &task, &signals)
        .unwrap();

    assert_eq!(response.task_id, task_id);
    TaskSpaceAssociation::validate_collection(task_id, &response.associations).unwrap();
    assert!(!response.indexed_tree_oid.is_empty());
    assert!(response.projection_generation > 0);
    let actual_spaces = response
        .associations
        .iter()
        .map(|association| association.space_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        actual_spaces,
        [fixture.feature_spaces[3]].into_iter().collect(),
        "weak one-channel text or exact Scope without Context text must not associate"
    );
    for association in &response.associations {
        assert!(association.score > 0.0 && association.score <= 1.0);
        assert!(!association.reasons.is_empty());
        assert!(association.relation_paths.is_empty());
        assert!(
            !association.matched_intent_fields.is_empty()
                || !association.matched_artifacts.is_empty()
                || !association.matched_contexts.is_empty()
        );
    }
    let channels = response
        .associations
        .iter()
        .filter_map(|association| {
            association.reasons.iter().find_map(|reason| {
                serde_json::from_str::<TaskAssociationFusionExplanation>(reason).ok()
            })
        })
        .flat_map(|explanation| explanation.channels)
        .map(|feature| feature.channel)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        channels,
        [
            TaskAssociationChannel::AcceptedContextBm25,
            TaskAssociationChannel::ExactScope,
        ]
        .into_iter()
        .collect()
    );
    let matched_contexts = response
        .associations
        .iter()
        .flat_map(|association| association.matched_contexts.iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        matched_contexts,
        [fixture.feature_contexts[2]].into_iter().collect()
    );
    let matched_artifacts = response
        .associations
        .iter()
        .flat_map(|association| association.matched_artifacts.iter())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        matched_artifacts.is_empty(),
        "textual Task Signals must not masquerade as resolved Engineering Artifacts"
    );
}

#[test]
fn workspace_and_repository_locations_never_add_space_priors() {
    let fixture = fixture();
    let unrelated = task("unrelatedtaskneedle");
    let response = SearchEngine::new(fixture.index)
        .task_space_associations(
            TaskId::new(),
            &unrelated,
            &[
                TaskSignal {
                    kind: TaskSignalKind::Workspace,
                    content: "pageintentneedle SearchV2Endpoint analyticsdomain".to_owned(),
                },
                TaskSignal {
                    kind: TaskSignalKind::Workspace,
                    content: "protocolintentonly LegacyCompatibilityTest".to_owned(),
                },
            ],
        )
        .unwrap();
    assert!(response.associations.is_empty());
}

#[test]
fn a_workspace_signal_that_names_a_file_retrieves_like_the_diff_that_names_one() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);
    let unrelated = task("unrelatedtaskneedle");
    let file = |kind| {
        vec![TaskSignal {
            kind,
            content: "product-catalog:src/pages/SearchResultsPage.tsx".to_owned(),
        }]
    };

    // A file the Agent only opened says what the Task is about exactly as a file it rewrote does.
    let opened = engine
        .task_space_associations(TaskId::new(), &unrelated, &file(TaskSignalKind::Workspace))
        .unwrap();
    let rewritten = engine
        .task_space_associations(TaskId::new(), &unrelated, &file(TaskSignalKind::Diff))
        .unwrap();
    assert_eq!(opened.associations.len(), 1);
    assert_eq!(
        opened
            .associations
            .iter()
            .map(|association| association.space_id)
            .collect::<Vec<_>>(),
        rewritten
            .associations
            .iter()
            .map(|association| association.space_id)
            .collect::<Vec<_>>()
    );

    // The Repository identity in front of the path is a coordinate, not a word to retrieve on, and
    // a bare checkout root is still only a location.
    for content in [
        "product-catalog:",
        "/local/checkout/product-catalog",
        "product-catalog:/absolute/SearchResultsPage.tsx",
    ] {
        let located = engine
            .task_space_associations(
                TaskId::new(),
                &unrelated,
                &[TaskSignal {
                    kind: TaskSignalKind::Workspace,
                    content: content.to_owned(),
                }],
            )
            .unwrap();
        assert!(located.associations.is_empty(), "{content}");
    }
}

#[test]
fn out_of_scope_only_text_or_code_signal_stays_diagnostic_and_never_associates() {
    let fixture = polarity_fixture();
    let engine = SearchEngine::new(fixture.index);
    let excluded_task = task("支付迁移 LegacyRouterBoundary");

    let candidates = engine
        .space_intent_candidates(TaskId::new(), &excluded_task, &[])
        .unwrap();
    assert_eq!(candidates.candidates.len(), 1);
    let candidate = &candidates.candidates[0];
    assert_eq!(candidate.space_id, fixture.space_id);
    assert_eq!(candidate.matched_fields, vec![SpaceIntentField::OutOfScope]);
    assert_eq!(candidate.field_matches.len(), 1);
    assert_eq!(
        candidate.field_matches[0].field,
        SpaceIntentField::OutOfScope
    );
    assert!(!candidate.field_matches[0].matched_tokens.is_empty());

    let associations = engine
        .task_space_associations(TaskId::new(), &excluded_task, &[])
        .unwrap();
    assert!(associations.associations.is_empty());
    let automatic = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            excluded_task,
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(automatic.associations.is_empty());
    assert!(automatic.items.is_empty());

    let code_signal_only = engine
        .task_space_associations(
            TaskId::new(),
            &task("unrelated positive query"),
            &[TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "LegacyRouterBoundary".to_owned(),
            }],
        )
        .unwrap();
    assert!(code_signal_only.associations.is_empty());
}

#[test]
fn shared_vocabulary_in_out_of_scope_never_penalizes_a_positive_association() {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("shared-vocabulary")).unwrap();
    let event = Event::space_created(
        sctx_domain::IntentSnapshot {
            title: "RankingRequirement".to_owned(),
            problem: "SearchRankingEngine needs stable behavior".to_owned(),
            desired_outcome: "Ranking remains deterministic".to_owned(),
            in_scope: vec!["SearchRankingEngine 搜索排序".to_owned()],
            // The exclusion re-uses the Space's own vocabulary to narrow one case; the shared
            // tokens must stay positive.
            out_of_scope: vec!["SearchRankingEngine 支付迁移".to_owned()],
            acceptance_conditions: vec!["ranking tests remain stable".to_owned()],
            domain_terms: vec!["ranking".to_owned()],
        },
        None,
    )
    .unwrap();
    let space_id = match event.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(&store, event);
    add_accepted_context(
        &store,
        space_id,
        "SearchRankingEngine keeps the ranking order stable",
        applicability("ranking", "server", "active"),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let engine = SearchEngine::new(index);

    let shared = engine
        .task_space_associations(TaskId::new(), &task("SearchRankingEngine 搜索排序"), &[])
        .unwrap();
    assert_eq!(shared.associations.len(), 1);
    assert_eq!(shared.associations[0].space_id, space_id);
    assert!(
        shared.associations[0].reasons.iter().all(|reason| {
            serde_json::from_str::<IntentScopeConflictExplanation>(reason).is_err()
        }),
        "a token that also matched a positive Intent field is not an exclusion"
    );

    // A token that only appears in `out_of_scope` still penalizes the association.
    let negative = engine
        .task_space_associations(
            TaskId::new(),
            &task("SearchRankingEngine 搜索排序 支付迁移"),
            &[],
        )
        .unwrap();
    assert_eq!(negative.associations.len(), 1);
    let explanation = negative.associations[0]
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<IntentScopeConflictExplanation>(reason).ok())
        .expect("an exclusive out-of-scope token is still explained as a scope conflict");
    assert_eq!(
        explanation.kind,
        IntentScopeConflictKind::ContextSpaceOutOfScope
    );
    assert!(
        explanation
            .matched_tokens
            .iter()
            .all(|token| !token.contains("search") && !token.contains("ranking"))
    );
    assert!(negative.associations[0].score < shared.associations[0].score);
}

#[test]
fn positive_and_out_of_scope_matches_keep_one_penalized_explained_association() {
    let fixture = polarity_fixture();
    let engine = SearchEngine::new(fixture.index);
    let positive_task = task("SearchRankingEngine 搜索排序");
    let positive = engine
        .task_space_associations(TaskId::new(), &positive_task, &[])
        .unwrap();
    assert_eq!(positive.associations.len(), 1);
    let positive_score = positive.associations[0].score;

    let mut task_declares_exclusion = positive_task;
    task_declares_exclusion.out_of_scope = vec!["支付迁移 LegacyRouterBoundary".to_owned()];
    let declared_exclusion = engine
        .task_space_associations(TaskId::new(), &task_declares_exclusion, &[])
        .unwrap();
    assert_eq!(declared_exclusion.associations.len(), 1);
    assert_eq!(
        declared_exclusion.associations[0].score.to_bits(),
        positive_score.to_bits()
    );
    assert!(
        declared_exclusion.associations[0]
            .reasons
            .iter()
            .all(|reason| {
                serde_json::from_str::<IntentScopeConflictExplanation>(reason).is_err()
            })
    );

    let conflict_task = task("SearchRankingEngine 搜索排序 支付迁移 LegacyRouterBoundary");
    let conflict_signals = vec![TaskSignal {
        kind: TaskSignalKind::Diff,
        content: "LegacyRouterBoundary".to_owned(),
    }];
    let conflict_task_id = TaskId::new();
    let conflicted = engine
        .task_space_associations(conflict_task_id, &conflict_task, &conflict_signals)
        .unwrap();
    assert_eq!(conflicted.associations.len(), 1);
    let association = &conflicted.associations[0];
    assert_eq!(association.space_id, fixture.space_id);
    assert_eq!(
        association.score.to_bits(),
        (positive_score / 2.0).to_bits()
    );
    assert!(
        !association
            .matched_intent_fields
            .iter()
            .any(|field| field == "out_of_scope")
    );
    let explanation = association
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<IntentScopeConflictExplanation>(reason).ok())
        .expect("association must expose a typed Intent Scope Conflict");
    assert_eq!(
        explanation.kind,
        IntentScopeConflictKind::ContextSpaceOutOfScope
    );
    assert_eq!(
        explanation.policy,
        IntentScopeConflictPolicy::PenalizeAssociation
    );
    assert_eq!(explanation.score_multiplier_basis_points, 5_000);
    assert!(!explanation.matched_tokens.is_empty());

    let pack = engine
        .task_context_pack(&TaskContextRequest::automatic(
            conflict_task_id,
            conflict_task,
            conflict_signals,
            100_000,
        ))
        .unwrap();
    assert_eq!(pack.associations, conflicted.associations);
    assert_eq!(pack.items.len(), 1);
    assert_eq!(pack.items[0].context.context_id, fixture.context_id);
    assert!(pack.items[0].retrieval_paths.iter().any(|path| {
        matches!(
            path,
            TaskRetrievalPath::IntentFts {
                matched_fields,
                matched_tokens,
            } if matched_fields == &["out_of_scope".to_owned()] && !matched_tokens.is_empty()
        )
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn intent_conflict_handoff_is_visible_without_blocking_and_disappears_after_merge() {
    let fixture = intent_handoff_fixture();
    let engine = SearchEngine::new(fixture.index.clone());
    let query = task("handoffsharedneedle");
    let candidates = engine
        .space_intent_candidates(TaskId::new(), &query, &[])
        .unwrap();
    assert_eq!(candidates.candidates.len(), 1);
    let candidate = &candidates.candidates[0];
    assert!(candidate.intent_conflicted);
    let mut expected_heads = fixture.conflicted_head_ids.to_vec();
    expected_heads.sort();
    assert_eq!(candidate.head_revision_ids, expected_heads);

    let context_only_query = task("contextonlyhandoffneedle");
    let context_only_task_id = TaskId::new();
    let associations = engine
        .task_space_associations(context_only_task_id, &context_only_query, &[])
        .unwrap();
    assert_eq!(associations.associations.len(), 1);
    let warning = associations.associations[0]
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<IntentConflictHandoffExplanation>(reason).ok())
        .expect("conflicted Space must hand the decision to the session Agent");
    assert_eq!(
        warning.kind,
        IntentConflictKind::ContextAndIntentAlternativesConflict
    );
    assert_eq!(warning.head_revision_ids, expected_heads);
    assert_eq!(
        warning.selection,
        IntentConflictSelection::SystemHasNotSelectedWinner
    );
    assert_eq!(warning.required_actor, IntentConflictActor::SessionAgent);
    assert_eq!(
        warning.must_validate,
        vec![
            IntentConflictValidation::CurrentCode,
            IntentConflictValidation::Evidence,
            IntentConflictValidation::TaskApplicability,
        ]
    );
    assert_eq!(
        warning.required_decision,
        IntentConflictDecision::DecideWhichContextIsMoreSuitable
    );

    let before = engine
        .task_context_pack(&TaskContextRequest::automatic(
            context_only_task_id,
            context_only_query.clone(),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(before.associations, associations.associations);
    assert_eq!(
        before.items.len(),
        1,
        "human decision keeps automatic behavior"
    );
    assert_eq!(before.items[0].context.context_id, fixture.context_id);
    let hook_visible_json = serde_json::to_string(&before).unwrap();
    for marker in [
        "context_and_intent_alternatives_conflict",
        "system_has_not_selected_winner",
        "session_agent",
        "current_code",
        "evidence",
        "task_applicability",
        "decide_which_context_is_more_suitable",
    ] {
        assert!(
            hook_visible_json.contains(marker),
            "Hook-serialized Task Pack must expose {marker}"
        );
    }

    append(
        &fixture.store,
        Event::intent_revision_added(
            fixture.space_id,
            fixture.conflicted_head_ids.to_vec(),
            handoff_intent(
                "HandoffResolved",
                "use the validated merged Context alternative",
            ),
            None,
        )
        .unwrap(),
    );
    let resolved_candidates = engine
        .space_intent_candidates(TaskId::new(), &query, &[])
        .unwrap();
    assert_eq!(resolved_candidates.candidates.len(), 1);
    assert!(!resolved_candidates.candidates[0].intent_conflicted);
    assert_eq!(resolved_candidates.candidates[0].head_revision_ids.len(), 1);
    let resolved = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            context_only_query,
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(resolved.items.len(), 1);
    assert_eq!(resolved.items[0].context.context_id, fixture.context_id);
    assert!(resolved.associations[0].reasons.iter().all(|reason| {
        serde_json::from_str::<IntentConflictHandoffExplanation>(reason).is_err()
    }));
    assert!(
        !serde_json::to_string(&resolved)
            .unwrap()
            .contains("system_has_not_selected_winner")
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn forty_space_corpus_is_rrf_ranked_top_k_bounded_and_fully_budgeted() {
    let fixture = fusion_corpus_fixture();
    let engine = SearchEngine::new(fixture.index);
    let signals = vec![TaskSignal {
        kind: TaskSignalKind::Diff,
        content: "SearchV9RareEndpoint".to_owned(),
    }];
    let mut request = TaskContextRequest::automatic(
        TaskId::new(),
        task("implement rare endpoint SearchV9RareEndpoint ExactResultSchema"),
        signals,
        4_000,
    );
    request.max_spaces = 5;

    for invalid_max_spaces in [0, sctx_search::MAX_TASK_MAX_SPACES + 1] {
        let mut invalid = request.clone();
        invalid.max_spaces = invalid_max_spaces;
        assert_eq!(
            engine.task_context_pack(&invalid).unwrap_err().kind(),
            sctx_search::ErrorKind::InvalidInput
        );
    }
    let mut invalid_budget = request.clone();
    invalid_budget.token_budget = sctx_search::MIN_TASK_CONTEXT_TOKEN_BUDGET - 1;
    assert_eq!(
        engine
            .task_context_pack(&invalid_budget)
            .unwrap_err()
            .kind(),
        sctx_search::ErrorKind::InvalidInput
    );

    let first = engine.task_context_pack(&request).unwrap();
    let second = engine.task_context_pack(&request).unwrap();
    assert_eq!(
        first, second,
        "RRF, top-k, omissions, and budget must be stable"
    );
    assert!(!first.associations.is_empty());
    assert!(first.associations.len() <= request.max_spaces);
    assert_eq!(first.associations[0].space_id, fixture.precise_space_id);
    assert!(first.items.iter().any(|item| {
        item.association_space_id == fixture.precise_space_id
            && item.context.context_id == fixture.precise_context_id
    }));
    assert!(first.estimated_tokens <= request.token_budget);
    assert_eq!(
        first.estimated_tokens,
        estimate_task_context_payload_tokens(&first),
        "reported tokens must charge Associations, reasons, item paths, and omissions"
    );
    assert!(
        serde_json::to_string(&first).unwrap().len().div_ceil(4) <= first.estimated_tokens,
        "charged envelope reserve must conservatively cover the complete ASCII fixture response"
    );
    assert!(
        first
            .omitted
            .iter()
            .all(|omitted| omitted.reason != "space_top_k"),
        "generic corpus rows must be filtered before top-k"
    );

    let precise_fusion = first.associations[0]
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<TaskAssociationFusionExplanation>(reason).ok())
        .expect("precise association must expose typed RRF features");
    assert_eq!(
        precise_fusion.algorithm,
        sctx_search::TaskAssociationFusionAlgorithm::ReciprocalRankFusion
    );
    assert!(
        precise_fusion.channels.iter().any(|feature| {
            feature.channel == TaskAssociationChannel::SpaceIntentBm25
                && feature.rank == 1
                && feature.query_token_coverage_basis_points > 0
                && feature.idf_bm25_contribution_micros > 0
                && feature.phrase_match
                && feature.field_weight_points > 0
        }),
        "precise fusion features: {precise_fusion:?}"
    );
    assert!(precise_fusion.channels.iter().any(|feature| {
        feature.channel == TaskAssociationChannel::AcceptedContextBm25
            && feature.rank == 1
            && feature.bm25_micros.is_some()
    }));
    if let Some(generic) = first.associations.get(1) {
        assert!(first.associations[0].score > generic.score);
        let generic_fusion = generic
            .reasons
            .iter()
            .find_map(|reason| {
                serde_json::from_str::<TaskAssociationFusionExplanation>(reason).ok()
            })
            .unwrap();
        let precise_intent = precise_fusion
            .channels
            .iter()
            .find(|feature| feature.channel == TaskAssociationChannel::SpaceIntentBm25)
            .unwrap();
        let generic_intent = generic_fusion
            .channels
            .iter()
            .find(|feature| feature.channel == TaskAssociationChannel::SpaceIntentBm25)
            .unwrap();
        assert!(
            precise_intent.query_token_coverage_basis_points
                > generic_intent.query_token_coverage_basis_points
        );
        assert!(
            precise_intent.idf_bm25_contribution_micros
                > generic_intent.idf_bm25_contribution_micros
        );
    }

    let mut minimum_budget = request.clone();
    minimum_budget.token_budget = sctx_search::MIN_TASK_CONTEXT_TOKEN_BUDGET;
    let bounded = engine.task_context_pack(&minimum_budget).unwrap();
    assert!(bounded.estimated_tokens <= minimum_budget.token_budget);
    assert_eq!(
        bounded.estimated_tokens,
        estimate_task_context_payload_tokens(&bounded)
    );
    assert!(!bounded.omitted.is_empty());

    let mut generic_request =
        TaskContextRequest::automatic(TaskId::new(), task("implement"), Vec::new(), 900);
    generic_request.max_spaces = 4;
    let generic = engine.task_context_pack(&generic_request).unwrap();
    assert!(generic.associations.len() <= generic_request.max_spaces);
    assert!(generic.estimated_tokens <= generic_request.token_budget);
    assert_eq!(
        generic.estimated_tokens,
        estimate_task_context_payload_tokens(&generic)
    );
    assert!(generic.associations.is_empty());
    assert!(generic.items.is_empty());
    assert!(generic.omitted.is_empty());
}

#[test]
fn equal_fused_scores_use_stable_space_id_ties() {
    let fixture = fixture();
    let index = fixture.index.clone();
    let query = task("tieassociationneedle");
    let task_id = TaskId::new();
    let first = SearchEngine::new(fixture.index)
        .task_space_associations(task_id, &query, &[])
        .unwrap();
    assert_eq!(first.associations.len(), 2);
    let mut expected = fixture.tied_spaces.to_vec();
    expected.sort();
    assert_eq!(
        first
            .associations
            .iter()
            .map(|association| association.space_id)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        first.associations[0].score.to_bits(),
        first.associations[1].score.to_bits()
    );

    index.rebuild().unwrap();
    let rebuilt = SearchEngine::new(index)
        .task_space_associations(task_id, &query, &[])
        .unwrap();
    assert_eq!(rebuilt.associations, first.associations);
}

#[test]
fn unsafe_context_states_cannot_supply_association_or_injection_evidence() {
    let fixture = fixture();
    let response = SearchEngine::new(fixture.index)
        .task_space_associations(TaskId::new(), &task("unsafeassociationneedle"), &[])
        .unwrap();
    assert!(
        response.associations.is_empty(),
        "Candidate, Deprecated, and conflicted Context rows must be excluded"
    );
}

#[test]
fn task_context_pack_supports_zero_one_and_many_spaces_with_explicit_m2_paths() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index.clone());
    let zero = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task("unrelatedpackneedle"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(zero.associations.is_empty());
    assert!(zero.items.is_empty());

    let one = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task("pageintentneedle"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(one.associations.len(), 1);
    assert_eq!(one.items.len(), 1);
    assert!(matches!(
        one.items[0].retrieval_paths.as_slice(),
        [TaskRetrievalPath::IntentFts { .. }]
    ));

    let many = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            feature_task(),
            feature_signals(),
            100_000,
        ))
        .unwrap();
    assert_eq!(many.associations.len(), 1);
    assert_eq!(many.items.len(), 1);
    assert_eq!(
        many.items
            .iter()
            .map(|item| item.context.context_id)
            .collect::<std::collections::BTreeSet<_>>(),
        [fixture.pack_contexts[3]].into_iter().collect()
    );
    let association_spaces = many
        .associations
        .iter()
        .map(|association| association.space_id)
        .collect::<std::collections::BTreeSet<_>>();
    for item in &many.items {
        assert_eq!(item.association_space_id, item.context.space_id);
        assert!(association_spaces.contains(&item.association_space_id));
        assert!(!item.retrieval_paths.is_empty());
        assert_eq!(item.context.status, ContextStatus::Accepted);
        assert!(item.context.auto_injection_eligible);
        assert!(!item.context.evidence.is_empty());
        assert!(item.context.conflicts.is_empty());
    }
    let paths = many
        .items
        .iter()
        .flat_map(|item| item.retrieval_paths.iter())
        .collect::<Vec<_>>();
    assert!(
        paths
            .iter()
            .any(|path| matches!(path, TaskRetrievalPath::ContextFts { .. }))
    );
    assert!(
        paths
            .iter()
            .any(|path| matches!(path, TaskRetrievalPath::ExactScope { .. }))
    );
    assert!(paths.iter().all(|path| matches!(
        path,
        TaskRetrievalPath::ContextFts { .. } | TaskRetrievalPath::ExactScope { .. }
    )));
    let metadata = fixture.index.metadata().unwrap();
    assert_eq!(many.indexed_tree_oid, metadata.indexed_tree_oid);
    assert_eq!(many.projection_generation, metadata.projection_generation);
}

#[test]
fn automatic_text_quality_keeps_phrase_and_drops_weak_or_generic_space_inheritance() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);

    let phrase = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task("pageintentneedle SearchResultsPage.tsx"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(phrase.associations.len(), 1);
    assert_eq!(phrase.items.len(), 1);
    assert_eq!(phrase.items[0].context.context_id, fixture.pack_contexts[0]);
    let fusion = phrase.associations[0]
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<TaskAssociationFusionExplanation>(reason).ok())
        .unwrap();
    assert!(fusion.channels.iter().any(|feature| {
        feature.channel == TaskAssociationChannel::SpaceIntentBm25 && feature.phrase_match
    }));

    let generic = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task("the to code file task"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(generic.associations.is_empty());
    assert!(generic.items.is_empty());

    let weak_intent =
        task("pageintentneedle unrelatedalpha unrelatedbravo unrelatedcharlie unrelateddelta");
    let automatic = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            weak_intent.clone(),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(automatic.associations.is_empty());
    assert!(automatic.items.is_empty());

    let mut explicit_request =
        TaskContextRequest::automatic(TaskId::new(), weak_intent, Vec::new(), 100_000);
    explicit_request.mode = ContextPackMode::Explicit;
    let explicit = engine.task_context_pack(&explicit_request).unwrap();
    assert_eq!(explicit.associations.len(), 1);
    assert_eq!(explicit.items.len(), 1);
    assert_eq!(
        explicit.items[0].context.context_id,
        fixture.pack_contexts[0]
    );
}

#[test]
fn strong_context_phrase_does_not_promote_an_unrelated_same_space_sibling() {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("text inheritance gate")).unwrap();
    let space_id = add_space(&store, "WeakSpace", "weakspaceanchor");
    let (strong_context_id, _, _) = add_accepted_context(
        &store,
        space_id,
        "strong context phrase is directly relevant",
        applicability("quality", "server", "active"),
    );
    let (unrelated_context_id, _, _) = add_accepted_context(
        &store,
        space_id,
        "unrelated sibling material must stay private",
        applicability("other", "client", "inactive"),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let mut intent = task("weakspaceanchor absentalpha absentbravo absentcharlie absentdelta");
    intent.current_direction = Some("strong context phrase".to_owned());
    let pack = SearchEngine::new(index)
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            intent,
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(pack.associations.len(), 1);
    assert!(pack.items.iter().any(|item| {
        item.context.context_id == strong_context_id
            && item
                .retrieval_paths
                .iter()
                .any(|path| matches!(path, TaskRetrievalPath::ContextFts { .. }))
    }));
    assert!(
        pack.items
            .iter()
            .all(|item| item.context.context_id != unrelated_context_id)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn related_space_retrieval_preserves_primary_owner_and_exposes_typed_association_path() {
    let temporary = tempfile::tempdir().unwrap();
    let base_store =
        GitStore::bootstrap_local(temporary.path().join("related-space-installation")).unwrap();
    let index = ProjectionIndex::for_store(&base_store);
    let store = base_store
        .with_candidate_submission_index(Arc::new(index.clone()))
        .with_candidate_confirmation_index(Arc::new(index.clone()));
    let primary_space = add_space(&store, "PrimaryRequirement", "primaryonlyneedle");
    let related_space = add_space(&store, "RelatedRequirement", "relatedonlyneedle");
    let alternate_space = add_space(&store, "AlternateRequirement", "alternateonlyneedle");
    let submission = store
        .submit_candidate(CandidateSubmissionRequest {
            submission_id: SubmissionId::new(),
            source_episode: WorkEpisodeRef {
                episode_id: WorkEpisodeId::new(),
                task_session_id: TaskSessionId::new(),
                task_id: TaskId::new(),
            },
            content: context(
                "directcontextneedle remains owned by the Primary Requirement",
                applicability("related-space", "server", "active"),
            ),
        })
        .unwrap();
    let candidate = index.domain_snapshot().unwrap().projection.candidates
        [&submission.record.candidate_id]
        .candidate
        .clone();
    let plan = CandidateConfirmationPlan::reserve(
        &candidate,
        CandidateConfirmationOperation {
            candidate_id: candidate.candidate_id,
            review_parent_version: 1,
            analysis_generation: 1,
            primary: CandidateConfirmationPrimaryReference::ExistingSpace {
                space_id: primary_space,
            },
            related_space_ids: vec![related_space],
            edits: OptionalCandidateEdits::default(),
        },
        CandidatePrimarySelection::Existing {
            space_id: primary_space,
        },
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    store.confirm_candidate(&plan).unwrap();
    let engine = SearchEngine::new(index.clone());
    let request = TaskContextRequest::automatic(
        TaskId::new(),
        task("relatedonlyneedle"),
        Vec::new(),
        100_000,
    );
    let first = engine.task_context_pack(&request).unwrap();
    let second = engine.task_context_pack(&request).unwrap();
    assert_eq!(first, second);
    assert!(first.estimated_tokens <= request.token_budget);
    assert_eq!(
        first.estimated_tokens,
        estimate_task_context_payload_tokens(&first)
    );
    assert_eq!(first.associations.len(), 1);
    assert_eq!(first.associations[0].space_id, related_space);
    assert_eq!(first.items.len(), 1);
    let item = &first.items[0];
    assert_eq!(item.association_space_id, related_space);
    assert_eq!(item.context.space_id, primary_space);
    assert_eq!(item.context.context_id, plan.result_context_id);
    assert!(item.retrieval_paths.iter().any(|path| {
        matches!(
            path,
            TaskRetrievalPath::SpaceAssociation {
                association_id,
                role: SpaceAssociationRole::Related,
                matched_space_id,
            } if *association_id == plan.space_association.association_id
                && *matched_space_id == related_space
        )
    }));
    assert!(
        item.retrieval_paths
            .iter()
            .all(|path| { !matches!(path, TaskRetrievalPath::ContextRelation { .. }) })
    );

    let direct = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task("directcontextneedle"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(direct.items.len(), 1);
    assert_eq!(direct.items[0].context.context_id, plan.result_context_id);
    assert_eq!(
        direct
            .associations
            .iter()
            .map(|association| association.space_id)
            .collect::<std::collections::BTreeSet<_>>(),
        [primary_space, related_space].into_iter().collect()
    );

    for related_space_ids in [vec![related_space], vec![alternate_space]] {
        append(
            &store,
            Event::context_space_association_changed(
                ContextSpaceAssociationDraft {
                    context_id: plan.result_context_id,
                    primary_space_id: primary_space,
                    related_space_ids,
                    previous_association_ids: vec![plan.space_association.association_id],
                    origin: ContextSpaceAssociationOrigin::Correction,
                },
                None,
            )
            .unwrap(),
        );
    }
    index.synchronize().unwrap();
    let conflicted = SearchEngine::new(index)
        .task_context_pack(&request)
        .unwrap();
    assert!(
        conflicted
            .items
            .iter()
            .all(|item| item.context.context_id != plan.result_context_id)
    );
}

#[test]
fn task_context_order_budget_and_fingerprint_are_stable() {
    let fixture = fixture();
    let index = fixture.index.clone();
    let task = feature_task();
    let signals = feature_signals();
    let task_id = TaskId::new();
    let full_request =
        TaskContextRequest::automatic(task_id, task.clone(), signals.clone(), 100_000);
    let full = SearchEngine::new(fixture.index)
        .task_context_pack(&full_request)
        .unwrap();
    assert_eq!(full.task_fingerprint.len(), 64);
    assert!(full.estimated_tokens <= full.token_budget);
    let association_rank = full
        .associations
        .iter()
        .enumerate()
        .map(|(rank, association)| (association.space_id, rank))
        .collect::<std::collections::BTreeMap<_, _>>();
    let item_ranks = full
        .items
        .iter()
        .map(|item| association_rank[&item.association_space_id])
        .collect::<Vec<_>>();
    assert!(item_ranks.windows(2).all(|pair| pair[0] <= pair[1]));

    let limited_request =
        TaskContextRequest::automatic(task_id, task.clone(), signals.clone(), 512);
    let limited_first = SearchEngine::new(index.clone())
        .task_context_pack(&limited_request)
        .unwrap();
    let limited_second = SearchEngine::new(index.clone())
        .task_context_pack(&limited_request)
        .unwrap();
    assert_eq!(limited_first, limited_second);
    assert!(limited_first.estimated_tokens <= 512);
    assert!(!limited_first.omitted.is_empty());

    let mut reordered = signals;
    reordered.reverse();
    reordered.push(TaskSignal {
        kind: TaskSignalKind::Workspace,
        content: "/different/local/checkout".to_owned(),
    });
    reordered.push(TaskSignal {
        kind: TaskSignalKind::Workspace,
        content: "/different/local/repository".to_owned(),
    });
    let reordered_pack = SearchEngine::new(index.clone())
        .task_context_pack(&TaskContextRequest::automatic(
            task_id,
            task.clone(),
            reordered,
            100_000,
        ))
        .unwrap();
    assert_eq!(reordered_pack.task_fingerprint, full.task_fingerprint);
    assert_eq!(reordered_pack.associations, full.associations);
    assert_eq!(reordered_pack.items, full.items);

    index.rebuild().unwrap();
    let rebuilt = SearchEngine::new(index)
        .task_context_pack(&full_request)
        .unwrap();
    assert_eq!(rebuilt.task_fingerprint, full.task_fingerprint);
    assert_eq!(rebuilt.associations, full.associations);
    assert_eq!(rebuilt.items, full.items);
    assert_eq!(rebuilt.indexed_tree_oid, full.indexed_tree_oid);
    assert!(rebuilt.projection_generation > full.projection_generation);
}

#[test]
fn automatic_task_pack_excludes_every_unsafe_state_while_explicit_expands_conflicts() {
    let fixture = fixture();
    let task_id = TaskId::new();
    let task = task("unsafepackintentneedle");
    let automatic = SearchEngine::new(fixture.index.clone())
        .task_context_pack(&TaskContextRequest::automatic(
            task_id,
            task.clone(),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert_eq!(
        automatic
            .associations
            .iter()
            .map(|association| association.space_id)
            .collect::<std::collections::BTreeSet<_>>(),
        fixture.unsafe_pack_spaces.into_iter().collect()
    );
    assert!(automatic.items.is_empty());

    let explicit = SearchEngine::new(fixture.index)
        .task_context_pack(&TaskContextRequest {
            task_id,
            working_intent: task,
            task_signals: Vec::new(),
            resolved_focus: None,
            token_budget: 100_000,
            max_spaces: sctx_search::DEFAULT_TASK_MAX_SPACES,
            candidate_limit: 100,
            mode: ContextPackMode::Explicit,
        })
        .unwrap();
    assert_eq!(explicit.task_fingerprint, automatic.task_fingerprint);
    assert!(
        explicit
            .items
            .iter()
            .any(|item| item.context.status == ContextStatus::Candidate)
    );
    assert!(
        explicit
            .items
            .iter()
            .any(|item| item.context.status == ContextStatus::Deprecated)
    );
    assert!(
        explicit
            .items
            .iter()
            .any(|item| !item.context.conflicts.is_empty()),
        "explicit mode must expand both sides of blocking conflicts"
    );
    assert!(
        explicit
            .items
            .iter()
            .flat_map(|item| item.context.conflicts.iter())
            .all(|conflict| conflict.participants.len() == 2)
    );
    assert!(
        explicit
            .items
            .iter()
            .flat_map(|item| item.context.evidence.iter())
            .any(|evidence| evidence.limitations.is_empty()),
        "explicit mode exposes incomplete Evidence without making it automatic"
    );
}

#[test]
fn a_large_corpus_drops_generic_tokens_only_beyond_the_retained_rarest_floor() {
    let fixture = fusion_corpus_fixture();
    let engine = SearchEngine::new(fixture.index);
    // `implement`, `shared` and `workflow` appear in every generic Space Intent of this corpus;
    // the remaining tokens are rare and carry the actual meaning of the intent.
    let response = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task(
                "SearchV9RareEndpoint ExactResultSchema precise protocol implement shared workflow",
            ),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    let association = response
        .associations
        .iter()
        .find(|association| association.space_id == fixture.precise_space_id)
        .expect("the rare tokens still associate the precise Space");
    // Every Association names the selection it was matched under; the token lists themselves live
    // once at the top level rather than once per Space.
    let projected = association
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<AutomaticQueryTokenExplanation>(reason).ok())
        .expect("automatic query token selection is explained");
    assert!(projected.selected_tokens.is_empty());
    assert!(projected.selected_token_count > 0);
    let explanation = response
        .query_token_explanation
        .clone()
        .expect("the explainable Pack names the automatic query token selection");
    assert_eq!(
        projected.selected_token_count,
        explanation.selected_tokens.len()
    );

    assert!(explanation.document_count >= 20);
    assert!(!explanation.stop_word_fallback_active);
    for generic in ["implement", "shared", "workflow"] {
        let drop = explanation
            .dropped_tokens
            .iter()
            .find(|drop| drop.token == generic)
            .unwrap_or_else(|| panic!("{generic} is dropped: {explanation:?}"));
        assert_eq!(
            drop.filter,
            AutomaticQueryTokenFilter::HighDocumentFrequency
        );
        assert!(
            drop.document_frequency
                .is_some_and(|frequency| frequency > 0)
        );
    }
    // Frequency never removes a rare token, and the retained floor keeps at least the rarest
    // tokens whatever the corpus looks like.
    assert!(explanation.selected_tokens.len() >= 8);
    for rare in ["search", "endpoint", "schema", "precise"] {
        assert!(
            explanation
                .selected_tokens
                .iter()
                .any(|token| token == rare),
            "{rare} is genuine domain vocabulary here"
        );
        assert!(
            explanation
                .dropped_tokens
                .iter()
                .all(|drop| drop.token != rare)
        );
    }
    assert!(
        response
            .items
            .iter()
            .any(|item| item.context.context_id == fixture.precise_context_id)
    );
}

#[test]
fn the_retained_floor_keeps_frequent_tokens_but_never_a_corpus_wide_one() {
    const CARRIER_SPACE_COUNT: usize = 21;
    const MIDDLE_FREQUENCY_SPACE_COUNT: usize = 12;

    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("frequency-floor")).unwrap();
    for index in 0..CARRIER_SPACE_COUNT {
        let mut intent_text = "universalterm".to_owned();
        if index < MIDDLE_FREQUENCY_SPACE_COUNT {
            intent_text.push_str(" middlefrequencyterm");
        }
        if index == 0 {
            intent_text.push_str(" rareneedle");
        }
        add_space(&store, &format!("Carrier{index:02}"), &intent_text);
    }
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();

    let response = SearchEngine::new(index)
        .task_space_associations(
            TaskId::new(),
            &task("rareneedle middlefrequencyterm universalterm"),
            &[],
        )
        .unwrap();
    assert!(!response.associations.is_empty());
    assert!(
        response.associations[0]
            .reasons
            .iter()
            .any(|reason| serde_json::from_str::<AutomaticQueryTokenExplanation>(reason).is_ok())
    );
    let explanation = response
        .query_token_explanation
        .clone()
        .expect("the explainable Pack names the automatic query token selection");

    assert!(explanation.document_count >= 20);
    // 12 of 21 documents: frequent enough to trip the ordinary rule, but the query is short so the
    // retained floor keeps it.
    assert!(
        explanation
            .selected_tokens
            .iter()
            .any(|token| token == "middlefrequencyterm"),
        "{explanation:?}"
    );
    assert!(
        explanation
            .selected_tokens
            .iter()
            .any(|token| token == "rareneedle")
    );
    // Present in every document: it selects the whole corpus, so the floor does not protect it.
    let universal = explanation
        .dropped_tokens
        .iter()
        .find(|drop| drop.token == "universalterm")
        .unwrap_or_else(|| panic!("a corpus-wide token is dropped: {explanation:?}"));
    assert_eq!(
        universal.filter,
        AutomaticQueryTokenFilter::HighDocumentFrequency
    );
    assert_eq!(universal.document_frequency, Some(CARRIER_SPACE_COUNT));
}

#[test]
fn artifact_and_interface_hints_recall_context_only_text_without_graph_semantics() {
    let fixture = fixture();
    let index = fixture.index.clone();
    let mut intent = task("zzzzabsenttaskneedle");
    intent.artifact_hints = vec!["SearchV2Endpoint".to_owned()];
    intent.interface_hints = vec!["SearchResponseV2".to_owned()];
    let request = TaskContextRequest::automatic(TaskId::new(), intent, Vec::new(), 100_000);
    let first = SearchEngine::new(index.clone())
        .task_context_pack(&request)
        .unwrap();

    let protocol_association = first
        .associations
        .iter()
        .find(|association| association.space_id == fixture.feature_spaces[1])
        .unwrap();
    assert_eq!(
        protocol_association.matched_contexts,
        vec![fixture.feature_contexts[0]]
    );
    let item = first
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.feature_contexts[0])
        .unwrap();
    let explanations = item
        .retrieval_paths
        .iter()
        .filter_map(|path| {
            let TaskRetrievalPath::WorkingIntentHintText { explanation } = path else {
                return None;
            };
            Some(explanation)
        })
        .collect::<Vec<_>>();
    assert_eq!(explanations.len(), 2);
    assert_eq!(
        explanations
            .iter()
            .map(|explanation| explanation.source_field)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            WorkingIntentHintField::ArtifactHints,
            WorkingIntentHintField::InterfaceHints,
        ]
        .into_iter()
        .collect()
    );
    assert!(explanations.iter().all(|explanation| {
        explanation.target == WorkingIntentHintTarget::AcceptedContextFts
            && explanation.phrase_match
            && explanation.query_token_coverage_basis_points == 10_000
            && explanation.fusion_contribution_micros > 0
            && !explanation.matched_tokens.is_empty()
    }));
    assert!(
        item.retrieval_paths
            .iter()
            .all(|path| matches!(path, TaskRetrievalPath::WorkingIntentHintText { .. }))
    );
    assert!(first.artifact_generation.is_none());
    assert!(first.graph_context_tree_oid.is_none());

    let second = SearchEngine::new(index.clone())
        .task_context_pack(&request)
        .unwrap();
    assert_eq!(second, first);
    index.rebuild().unwrap();
    let rebuilt = SearchEngine::new(index.clone())
        .task_context_pack(&request)
        .unwrap();
    assert_eq!(rebuilt.task_fingerprint, first.task_fingerprint);
    assert_eq!(rebuilt.associations, first.associations);
    assert_eq!(rebuilt.items, first.items);

    let mut unsafe_intent = task("another absent goal");
    unsafe_intent.artifact_hints = vec!["unsafeassociationneedle".to_owned()];
    unsafe_intent.interface_hints = vec!["incomplete evidence must not enter".to_owned()];
    let unsafe_pack = SearchEngine::new(index)
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            unsafe_intent,
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(unsafe_pack.items.iter().all(|item| {
        !fixture
            .unsafe_pack_spaces
            .contains(&item.association_space_id)
    }));
}

#[test]
fn high_coverage_hint_text_outranks_generic_text_and_remains_budgeted() {
    let fixture = fusion_corpus_fixture();
    let index = fixture.index.clone();
    let mut intent = task("implement shared workflow");
    intent.artifact_hints = vec!["SearchV9RareEndpoint".to_owned()];
    intent.interface_hints = vec!["ExactResultSchema".to_owned()];
    let task_id = TaskId::new();
    let mut request = TaskContextRequest::automatic(task_id, intent, Vec::new(), 100_000);
    request.max_spaces = 5;
    let full = SearchEngine::new(index.clone())
        .task_context_pack(&request)
        .unwrap();
    assert_eq!(full.associations[0].space_id, fixture.precise_space_id);
    assert_eq!(full.items[0].context.context_id, fixture.precise_context_id);
    let channels = full.associations[0]
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str::<TaskAssociationFusionExplanation>(reason).ok())
        .unwrap()
        .channels
        .into_iter()
        .map(|feature| feature.channel)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(channels.contains(&TaskAssociationChannel::ArtifactHintSpaceIntentBm25));
    assert!(channels.contains(&TaskAssociationChannel::ArtifactHintAcceptedContextBm25));
    assert!(channels.contains(&TaskAssociationChannel::InterfaceHintSpaceIntentBm25));
    assert!(channels.contains(&TaskAssociationChannel::InterfaceHintAcceptedContextBm25));
    assert!(full.associations.len() <= request.max_spaces);
    assert!(full.omitted.iter().all(|item| item.reason != "space_top_k"));

    request.token_budget = 512;
    let limited_first = SearchEngine::new(index.clone())
        .task_context_pack(&request)
        .unwrap();
    let limited_second = SearchEngine::new(index)
        .task_context_pack(&request)
        .unwrap();
    assert_eq!(limited_first, limited_second);
    assert!(limited_first.estimated_tokens <= limited_first.token_budget);
    assert!(!limited_first.omitted.is_empty());

    let mut generic_hint = task("zzzzabsentgenerichintgoal");
    generic_hint.artifact_hints = vec!["implement shared workflow".to_owned()];
    let mut generic_request =
        TaskContextRequest::automatic(TaskId::new(), generic_hint, Vec::new(), 100_000);
    generic_request.max_spaces = 4;
    let generic_first = SearchEngine::new(fixture.index.clone())
        .task_context_pack(&generic_request)
        .unwrap();
    let generic_second = SearchEngine::new(fixture.index.clone())
        .task_context_pack(&generic_request)
        .unwrap();
    assert_eq!(generic_first, generic_second);
    assert!(generic_first.associations.is_empty());
    assert!(generic_first.items.is_empty());

    generic_request.mode = ContextPackMode::Explicit;
    let explicit = SearchEngine::new(fixture.index)
        .task_context_pack(&generic_request)
        .unwrap();
    assert_eq!(explicit.associations.len(), generic_request.max_spaces);
    // Every truncated Space is accounted for, and the named ones carry the identity an Agent
    // needs to ask for one of them explicitly.
    let space_top_k = explicit
        .omitted
        .iter()
        .filter(|item| item.reason == "space_top_k")
        .collect::<Vec<_>>();
    assert_eq!(
        space_top_k.iter().map(|item| item.count).sum::<usize>(),
        fixture.total_spaces - generic_request.max_spaces
    );
    assert!(space_top_k.iter().any(|item| item.space_id.is_some()));
}

/// One Space holding eight injection-safe Contexts that all answer the same rare query token.
/// A 2000-token budget cannot carry eight explainable items, so it is the exact shape that made
/// automatic injection return a single Context before compact packing existed.
fn compact_budget_fixture() -> (TempDir, ProjectionIndex, Vec<ContextId>) {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("compact-installation")).unwrap();
    let space_id = add_space(
        &store,
        "CompactBudgetSpace",
        "compactbudgetneedle retrieval payload",
    );
    let mut contexts = Vec::new();
    for index in 0..8_u8 {
        let (context_id, _, _) = add_accepted_context(
            &store,
            space_id,
            &format!(
                "compactbudgetneedle case {index}: the reviewed branch keeps the documented \
                 fallback behavior when the optional module is absent, so the caller observes a \
                 safe degradation instead of an unresolved-service failure"
            ),
            applicability("compactbudget", "server", &format!("case-{index}")),
        );
        contexts.push(context_id);
    }
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    (temporary, index, contexts)
}

#[test]
fn compact_detail_level_fits_more_evidenced_items_in_the_default_budget() {
    let (_temporary, index, contexts) = compact_budget_fixture();
    let engine = SearchEngine::new(index);
    let request = TaskContextRequest::automatic(
        TaskId::new(),
        task("compactbudgetneedle"),
        Vec::new(),
        2_000,
    );

    let full = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Full)
        .unwrap();
    let compact = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Compact)
        .unwrap();

    assert_eq!(full.detail_level, ContextPackDetailLevel::Full);
    assert!(full.compact_items.is_empty());
    assert_eq!(compact.detail_level, ContextPackDetailLevel::Compact);
    assert!(compact.items.is_empty());
    assert!(compact.associations.is_empty());
    assert_eq!(compact.compact_associations.len(), 1);

    assert!(
        compact.compact_items.len() >= 6,
        "compact packing must carry at least six Contexts in the default budget, got {}",
        compact.compact_items.len()
    );
    assert!(
        compact.compact_items.len() > full.items.len(),
        "compact must carry strictly more than the explainable shape at the same budget"
    );
    assert!(compact.estimated_tokens <= request.token_budget);
    assert_eq!(
        compact.estimated_tokens,
        estimate_task_context_payload_tokens(&compact)
    );
    assert!(
        serde_json::to_string(&compact.compact_items)
            .unwrap()
            .len()
            .div_ceil(4)
            <= compact.estimated_tokens
    );

    for item in &compact.compact_items {
        assert!(contexts.contains(&item.context_id));
        assert_eq!(item.status, ContextStatus::Accepted);
        assert!(
            !item.evidence.is_empty(),
            "every compact item keeps its Evidence: {item:?}"
        );
        assert!(item.evidence.iter().all(|evidence| {
            !evidence.summary.is_empty() && evidence.summary.chars().count() <= 201
        }));
        assert!(!item.conditions.is_empty());
        assert!(!item.why.is_empty() && item.why.len() <= 3);
        assert!(item.conflicts.is_empty());
    }

    // Every omitted Context is named, so the Agent can fetch exactly what the budget dropped.
    for omitted in &compact.omitted {
        if omitted.context_id.is_some() {
            assert!(omitted.title.is_some());
            assert_eq!(omitted.count, 1);
        }
    }
    let carried = compact
        .compact_items
        .iter()
        .map(|item| item.context_id)
        .chain(
            compact
                .omitted
                .iter()
                .filter_map(|omitted| omitted.context_id),
        )
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        carried,
        contexts
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        "compact packing accounts for every retrieved Context, returned or named as omitted"
    );
}

#[test]
fn compact_detail_level_drops_machine_channels_and_full_keeps_the_token_explanation() {
    let (_temporary, index, _contexts) = compact_budget_fixture();
    let engine = SearchEngine::new(index);
    let request = TaskContextRequest::automatic(
        TaskId::new(),
        task("compactbudgetneedle"),
        Vec::new(),
        100_000,
    );

    let full = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Full)
        .unwrap();
    let compact = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Compact)
        .unwrap();

    // A Task with no dropped tokens still reports its selection at the top level in `full`.
    let explanation = full
        .query_token_explanation
        .as_ref()
        .expect("full packs expose the automatic query token selection at the top level");
    assert!(
        explanation
            .selected_tokens
            .contains(&"compactbudgetneedle".to_owned())
    );
    // Compact keeps the projection, never the lists: the reader who most needs to know how much
    // of the question this Tree could answer is the one whose Pack came back empty.
    let compact_explanation = compact
        .query_token_explanation
        .as_ref()
        .expect("compact packs keep the projected automatic query token selection");
    assert!(compact_explanation.selected_tokens.is_empty());
    assert!(compact_explanation.answerable_tokens.is_empty());
    assert_eq!(
        compact_explanation.selected_token_count,
        explanation.selected_token_count
    );
    assert_eq!(
        compact_explanation.answerable_token_count,
        explanation.answerable_token_count
    );
    assert!(compact_explanation.dropped_tokens.len() <= 8);

    assert!(
        full.associations.iter().any(|association| association
            .reasons
            .iter()
            .any(|reason| reason.starts_with('{'))),
        "the explainable shape keeps the machine-readable fusion reason"
    );
    assert!(
        compact
            .compact_associations
            .iter()
            .all(|association| association
                .reasons
                .iter()
                .all(|reason| !reason.starts_with('{'))),
        "compact associations keep only human-readable sentences"
    );

    let encoded = serde_json::to_value(&compact.compact_items).unwrap();
    let encoded = serde_json::to_string(&encoded).unwrap();
    for dropped in [
        "match_reason",
        "safety_source",
        "retrieval_paths",
        "rationale",
        "bm25",
        "auto_injection_eligible",
    ] {
        assert!(
            !encoded.contains(dropped),
            "compact items must not carry `{dropped}`"
        );
    }
    assert!(
        serde_json::to_string(&full.items)
            .unwrap()
            .contains("match_reason"),
        "the explainable shape keeps its match reasons"
    );

    // Identical requests remain byte-stable in both shapes.
    assert_eq!(
        compact,
        engine
            .task_context_pack_with_detail(&request, ContextPackDetailLevel::Compact)
            .unwrap()
    );
}

#[test]
fn a_condition_matches_the_stated_task_scope_rather_than_its_constraint_list() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);

    // The Task never lists a constraint: it states the situation in its goal and in-scope items,
    // which is how an Agent actually writes a Working Intent.
    let mut stated = task("legacyclient compatibility fallback");
    stated.in_scope = vec!["legacyclient".to_owned()];
    let matched = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            stated,
            Vec::new(),
            100_000,
        ))
        .unwrap();
    let conditions = matched
        .items
        .iter()
        .flat_map(|item| item.retrieval_paths.iter())
        .filter_map(|path| match path {
            TaskRetrievalPath::ExactScope { dimension, value } if dimension == "condition" => {
                Some(value.clone())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        conditions.contains("legacyclient"),
        "an in-scope item must be able to match a Context condition: {conditions:?}"
    );

    // A Task that never states the situation still must not match the condition.
    let silent = engine
        .task_context_pack(&TaskContextRequest::automatic(
            TaskId::new(),
            task("compatibility fallback"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(
        !silent
            .items
            .iter()
            .flat_map(|item| item.retrieval_paths.iter())
            .any(|path| matches!(
                path,
                TaskRetrievalPath::ExactScope { dimension, value }
                    if dimension == "condition" && value == "legacyclient"
            )),
        "an unstated condition must stay unmatched"
    );
}

/// Six accepted Contexts written the way a real review writes them: a long Chinese statement, a
/// Chinese Evidence summary, and one cross-Space Relation whose rationale is a whole Chinese
/// paragraph. This is the payload shape that made a real 2000-token injection return two
/// Contexts and name seven more as omitted.
fn real_shape_compact_fixture() -> (TempDir, ProjectionIndex, SpaceId) {
    let temporary = tempfile::tempdir().unwrap();
    let store =
        GitStore::bootstrap_local(temporary.path().join("real-shape-installation")).unwrap();
    let space_id = add_space(
        &store,
        "分支功能等价性评审",
        "realshapeneedle 等价性 降级 注入",
    );
    let neighbour_space = add_space(&store, "构建与编译", "realshapeneedle 构建 编译");
    let (neighbour_context, neighbour_revision) = add_context(
        &store,
        neighbour_space,
        "realshapeneedle 直接改动的库模块都能通过 Debug Kotlin 任务完成编译，构建产物可用于回归验证。",
        applicability("realshape", "android", "debug-build"),
    );
    publish(
        &store,
        neighbour_space,
        neighbour_context,
        neighbour_revision,
        Vec::new(),
        PublicationAction::Publish,
    );

    let statements = [
        "realshapeneedle 当垂类实现模块缺席时，分支把未解析服务的失败行为有意改成安全降级，因此严格意义上的全配置功能等价并不成立，评审需要按配置分别给出结论，而不是笼统地宣称行为不变；两套配置的差异点集中在服务解析失败之后的兜底分支上。",
        "realshapeneedle 当直播入口服务没有真实实现或参数拼装返回空时，商品锚点点击回调会提前返回，跳过配置分发与进入直播间导航，基线仍会导航，因此该路径不功能等价；复现步骤是在缺少垂类模块的调试包里点击商品锚点，观察不到任何页面跳转。",
        "realshapeneedle 底栏的空保护发生得过晚：当垂类领域服务返回空且展示判定为真时，入口组件已注册到优先级管理器，随后以空容器抢占底栏槽位，阻止默认评论栏兜底，用户看到的是一条没有输入框的空白底栏，且没有任何错误提示。",
        "realshapeneedle 无真实实现时，占位实现与旧动态代理在基本类型、空返回与可空返回值上大多等价，只有日志方法由空值变为空映射，影响范围限于埋点；这一差异不会改变调用方的控制流，但会让下游统计把缺失事件记成空事件。",
        "realshapeneedle 引入真实垂类实现后，评审分支保留了运行期服务解析能力，调试包可以正常构建并进入直播间，说明降级路径只在模块缺席时生效，完整配置下的行为与基线一致。",
        "realshapeneedle 评审范围内的所有直接改动模块都保留了原有的公开接口签名，调用方无需同步修改，二进制兼容性由接口快照比对确认。",
    ];
    for (index, statement) in statements.iter().enumerate() {
        let mut draft = context(statement, applicability("realshape", "android", "review"));
        "评审需要逐条区分配置差异，避免把安全降级误读成功能回归，因此每条结论都单独沉淀。"
            .clone_into(&mut draft.rationale);
        draft.evidence[0].content = serde_json::json!({
            "summary": format!(
                "第 {index} 条结论的证据：对照基线逐行比较调用链，记录了进入直播间导航与底栏兜底两条路径的实际行为差异，并附带缺少垂类模块与包含垂类模块两种配置下的构建与运行日志摘要。"
            ),
        });
        if index == 0 {
            draft.relations = vec![sctx_domain::ContextRelation {
                kind: sctx_domain::ContextRelationKind::RelatedTo,
                target_context_id: neighbour_context,
                rationale:
                    "编译结论与等价性结论互为前提：只有在直接改动模块全部编译通过的前提下，才能把行为差异归因于分支改动而不是构建失败，因此两条结论必须一起阅读。"
                        .to_owned(),
                supports: vec![
                    "编译通过是等价性判断的前置条件".to_owned(),
                    "行为差异不能归因于构建失败".to_owned(),
                ],
            }];
        }
        let (context_id, revision_id) = add_context_draft(&store, space_id, draft);
        publish(
            &store,
            space_id,
            context_id,
            revision_id,
            Vec::new(),
            PublicationAction::Publish,
        );
    }
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    (temporary, index, space_id)
}

#[test]
fn compact_packing_keeps_the_top_ranked_chinese_contexts_in_the_default_budget() {
    let (_temporary, index, _space_id) = real_shape_compact_fixture();
    let engine = SearchEngine::new(index);
    let request =
        TaskContextRequest::automatic(TaskId::new(), task("realshapeneedle"), Vec::new(), 2_000);

    let reference = engine
        .task_context_pack_with_detail(
            &TaskContextRequest {
                token_budget: 100_000,
                ..request.clone()
            },
            ContextPackDetailLevel::Compact,
        )
        .unwrap();
    let compact = engine
        .task_context_pack_with_detail(&request, ContextPackDetailLevel::Compact)
        .unwrap();

    let item_tokens = reference
        .compact_items
        .iter()
        .map(|item| serde_json::to_string(item).unwrap().chars().count())
        .collect::<Vec<_>>();
    println!(
        "real-shape compact: {} items at 2000 tokens ({} estimated), unbudgeted item chars {:?}",
        compact.compact_items.len(),
        compact.estimated_tokens,
        item_tokens
    );

    assert!(compact.estimated_tokens <= request.token_budget);
    assert!(
        compact.compact_items.len() >= 4,
        "a 2000 token budget must carry at least four real-shape Contexts, got {}",
        compact.compact_items.len()
    );
    let carried = compact
        .compact_items
        .iter()
        .map(|item| item.context_id)
        .collect::<std::collections::BTreeSet<_>>();
    for expected in reference.compact_items.iter().take(3) {
        assert!(
            carried.contains(&expected.context_id),
            "the three highest ranked Contexts are never omitted: {:?}",
            expected.context_id
        );
    }

    // The Space explanation no longer carries a Relation rationale paragraph.
    let associations = serde_json::to_string(&compact.compact_associations).unwrap();
    assert!(!associations.contains("relation_paths"));
    assert!(!associations.contains("matched_intent_fields"));
    assert!(!associations.contains("编译结论与等价性结论互为前提"));
    for association in &compact.compact_associations {
        assert!(association.reasons.len() <= 2);
        assert!(association.title.is_some());
    }

    // Omissions name at most five Contexts; anything beyond that is one counted entry.
    let named = compact
        .omitted
        .iter()
        .filter(|omitted| omitted.context_id.is_some())
        .count();
    assert!(named <= 5, "at most five omissions are named, got {named}");
    for omitted in &compact.omitted {
        if omitted.context_id.is_some() {
            assert!(omitted.revision_id.is_none());
            assert!(
                omitted
                    .title
                    .as_ref()
                    .is_some_and(|title| title.chars().count() <= 41)
            );
        }
    }
}

/// A Chinese-first knowledge base plus filler, so document frequency rather than the small-corpus
/// stop-word fallback governs automatic query-token selection.
///
/// The one interesting Context states its fact in Chinese but spells `ProductAnchorAssem`
/// verbatim, which is the only channel an English question has into it.
fn cross_language_identifier_fixture() -> (TempDir, ProjectionIndex, ContextId) {
    let temporary = tempfile::tempdir().unwrap();
    let store =
        GitStore::bootstrap_local(temporary.path().join("identifier-installation")).unwrap();
    let space_id = add_space(&store, "商品锚点导航", "复查商品锚点进入直播间的导航缺口");
    let (context_id, _, _) = add_accepted_context(
        &store,
        space_id,
        "当 ProductAnchorAssem 在商品锚点点击回调中提前 return 时，跳过配置分发与进入直播间导航（ProductAnchorAssem.kt:202）。",
        applicability("anchordomain", "android", "无真实实现"),
    );
    for filler in 0..6_u8 {
        let filler_space = add_space(
            &store,
            &format!("无关空间{filler}"),
            &format!("无关意图{filler} 发布清单"),
        );
        add_accepted_context(
            &store,
            filler_space,
            &format!("无关事实{filler}：发布清单会在发版之前校验版本号。"),
            applicability("fillerdomain", "server", "active"),
        );
    }
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    (temporary, index, context_id)
}

/// WP-L8: identifier-derived query tokens carry extra weight in the automatic coverage gate.
///
/// Two of six English tokens match a Chinese Context, which reads as 33% coverage and used to sit
/// below `AUTOMATIC_TEXT_COVERAGE_THRESHOLD_BASIS_POINTS` with only one text channel behind it, so
/// automatic injection returned nothing at all for a question explicit search answers first.
#[test]
fn identifier_coverage_carries_an_english_question_into_a_chinese_knowledge_base() {
    let (_temporary, index, context_id) = cross_language_identifier_fixture();
    let engine = SearchEngine::new(index);

    let request = TaskContextRequest::automatic(
        TaskId::new(),
        task("product anchor click does not navigate"),
        Vec::new(),
        100_000,
    );
    let pack = engine.task_context_pack(&request).unwrap();
    assert_eq!(
        pack.items
            .iter()
            .map(|item| item.context.context_id)
            .collect::<Vec<_>>(),
        vec![context_id],
        "the query names two words of one identifier the Context spells verbatim"
    );
    assert!(
        pack.associations[0]
            .reasons
            .iter()
            .any(|reason| reason.starts_with("Identifier coverage:")),
        "the weighting explains itself: {:?}",
        pack.associations[0].reasons
    );

    // One shared word is vocabulary, not a named identifier, so the weighting stays off.
    let single_word = TaskContextRequest::automatic(
        TaskId::new(),
        task("anchor rollout timeline review"),
        Vec::new(),
        100_000,
    );
    let single = engine.task_context_pack(&single_word).unwrap();
    assert!(
        single.items.is_empty(),
        "a single incidental identifier word must not buy an association: {:?}",
        single.items
    );
}

/// A Space that wins on Intent text but offers a Context sharing a single query token, a Space
/// that offers the Context answering most of the query, and filler that crowds the accepted-Context
/// channel so the two Spaces fuse to close but distinct scores.
fn near_duplicate_coverage_fixture() -> (TempDir, ProjectionIndex, ContextId, ContextId) {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("coverage-installation")).unwrap();
    let short_space = add_space(
        &store,
        "SlotSurface",
        "zulpraneedle bantiqneedle korvethneedle mildapneedle glavikneedle",
    );
    let (short_context, _, _) = add_accepted_context(
        &store,
        short_space,
        "zulpraneedle",
        applicability("surfacedomain", "android", "active"),
    );
    for filler in 0..6_u8 {
        let filler_space = add_space(
            &store,
            &format!("Filler{filler}"),
            "unrelated filler intent",
        );
        add_accepted_context(
            &store,
            filler_space,
            "bantiqneedle korvethneedle mildapneedle glavikneedle",
            applicability("fillerdomain", "server", "active"),
        );
    }
    let long_space = add_space(
        &store,
        "SlotCause",
        "zulpraneedle bantiqneedle korvethneedle mildapneedle glavikneedle plus a much longer \
         intent body whose extra words push this Space one rank down on the Intent channel",
    );
    let (long_context, _, _) = add_accepted_context(
        &store,
        long_space,
        "bantiqneedle korvethneedle mildapneedle glavikneedle: the registration happens before \
         the null guard, so the empty container keeps the slot and the default fallback never \
         runs for the surface the shorter Context only names",
        applicability("causedomain", "android", "active"),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    (temporary, index, short_context, long_context)
}

/// WP-L8: the injection ranking scales each Space's fused score by how much of the query the
/// Context itself answered.
///
/// The fused score is a property of the Space, so every Context a Space contributes carries the
/// same one. Ordering by it alone let the Space that won on Intent text put forward whichever of
/// its Contexts shared a phrase with the query, ahead of the Context that actually covers the
/// question.
#[test]
fn coverage_weighted_rank_puts_the_covering_context_ahead_of_a_short_near_duplicate() {
    let (_temporary, index, short_context, long_context) = near_duplicate_coverage_fixture();
    let mut request = TaskContextRequest::automatic(
        TaskId::new(),
        task("zulpraneedle bantiqneedle korvethneedle mildapneedle glavikneedle"),
        Vec::new(),
        100_000,
    );
    request.max_spaces = 10;
    let pack = SearchEngine::new(index)
        .task_context_pack(&request)
        .unwrap();
    let position = |context_id: ContextId| {
        pack.items
            .iter()
            .position(|item| item.context.context_id == context_id)
    };
    let short_position = position(short_context).expect("the near duplicate is still an answer");
    let long_position = position(long_context).expect("the covering Context is an answer");
    assert!(
        long_position < short_position,
        "the Context that answered more of the query must lead: long={long_position} \
         short={short_position}"
    );
    assert_eq!(long_position, 0);
    assert!(
        pack.items[long_position]
            .context
            .match_reason
            .coverage_basis_points
            > pack.items[short_position]
                .context
                .match_reason
                .coverage_basis_points
    );
}
