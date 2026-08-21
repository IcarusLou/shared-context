use sctx_domain::{
    Applicability, ConflictParticipant, ContextId, ContextKind, ContextRevisionDraft,
    EvidenceSnapshotDraft, EvidenceType, PublicationAction, PublicationDraft, RevisionId,
    SemanticConflictDraft, SpaceId, TaskId, TaskIntent, TaskSignal, TaskSignalKind,
    TaskSpaceAssociation,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    ContextPackMode, ContextStatus, IntentConflictActor, IntentConflictDecision,
    IntentConflictHandoffExplanation, IntentConflictKind, IntentConflictSelection,
    IntentConflictValidation, IntentScopeConflictExplanation, IntentScopeConflictKind,
    IntentScopeConflictPolicy, SearchEngine, SpaceIntentField, TaskAssociationChannel,
    TaskAssociationFusionExplanation, TaskContextRequest, TaskRetrievalPath,
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
    let store = GitStore::initialize(temporary.path().join("polarity-installation")).unwrap();
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
    let store = GitStore::initialize(temporary.path().join("fusion-installation")).unwrap();
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
    let store = GitStore::initialize(temporary.path().join("handoff-installation")).unwrap();
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
    let store = GitStore::initialize(temporary.path().join("association-installation")).unwrap();

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

fn task(goal: &str) -> TaskIntent {
    TaskIntent {
        task_id: TaskId::new(),
        goal: goal.to_owned(),
        desired_change: goal.to_owned(),
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifacts: Vec::new(),
        interfaces: Vec::new(),
        unknowns: Vec::new(),
    }
}

fn feature_task() -> TaskIntent {
    let mut task = task("pageintentneedle");
    "featureassociationgoal".clone_into(&mut task.desired_change);
    task.domains = vec!["analyticsdomain".to_owned()];
    task.platforms = vec!["fe".to_owned()];
    task.constraints = vec!["legacyclient".to_owned()];
    task.acceptance_conditions = vec!["impression".to_owned()];
    task
}

fn feature_signals() -> Vec<TaskSignal> {
    vec![
        TaskSignal {
            kind: TaskSignalKind::File,
            content: "SearchResultsPage.tsx".to_owned(),
        },
        TaskSignal {
            kind: TaskSignalKind::Api,
            content: "SearchV2Endpoint".to_owned(),
        },
        TaskSignal {
            kind: TaskSignalKind::Schema,
            content: "SearchResponseV2".to_owned(),
        },
        TaskSignal {
            kind: TaskSignalKind::Test,
            content: "LegacyCompatibilityTest".to_owned(),
        },
    ]
}

#[test]
fn fe_task_associates_requirement_protocol_compatibility_and_analytics_spaces() {
    let fixture = fixture();
    let task = feature_task();
    let signals = feature_signals();
    let response = SearchEngine::new(fixture.index)
        .task_space_associations(&task, &signals)
        .unwrap();

    assert_eq!(response.task_id, task.task_id);
    TaskSpaceAssociation::validate_collection(task.task_id, &response.associations).unwrap();
    assert!(!response.indexed_tree_oid.is_empty());
    assert!(response.projection_generation > 0);
    let actual_spaces = response
        .associations
        .iter()
        .map(|association| association.space_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        actual_spaces,
        [
            fixture.feature_spaces[0],
            fixture.feature_spaces[2],
            fixture.feature_spaces[3],
        ]
        .into_iter()
        .collect(),
        "only Intent, Context BM25, and Scope evidence may associate without an Engineering Graph"
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
            TaskAssociationChannel::SpaceIntentBm25,
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
        [fixture.feature_contexts[1], fixture.feature_contexts[2]]
            .into_iter()
            .collect()
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
            &unrelated,
            &[
                TaskSignal {
                    kind: TaskSignalKind::Workspace,
                    content: "pageintentneedle SearchV2Endpoint analyticsdomain".to_owned(),
                },
                TaskSignal {
                    kind: TaskSignalKind::Repository,
                    content: "protocolintentonly LegacyCompatibilityTest".to_owned(),
                },
            ],
        )
        .unwrap();
    assert!(response.associations.is_empty());
}

#[test]
fn out_of_scope_only_text_or_code_signal_stays_diagnostic_and_never_associates() {
    let fixture = polarity_fixture();
    let engine = SearchEngine::new(fixture.index);
    let excluded_task = task("支付迁移 LegacyRouterBoundary");

    let candidates = engine.space_intent_candidates(&excluded_task, &[]).unwrap();
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

    let associations = engine.task_space_associations(&excluded_task, &[]).unwrap();
    assert!(associations.associations.is_empty());
    let automatic = engine
        .task_context_pack(&TaskContextRequest::automatic(
            excluded_task,
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(automatic.associations.is_empty());
    assert!(automatic.items.is_empty());

    let code_signal_only = engine
        .task_space_associations(
            &task("unrelated positive query"),
            &[TaskSignal {
                kind: TaskSignalKind::Symbol,
                content: "LegacyRouterBoundary".to_owned(),
            }],
        )
        .unwrap();
    assert!(code_signal_only.associations.is_empty());
}

#[test]
fn positive_and_out_of_scope_matches_keep_one_penalized_explained_association() {
    let fixture = polarity_fixture();
    let engine = SearchEngine::new(fixture.index);
    let positive_task = task("SearchRankingEngine 搜索排序");
    let positive = engine.task_space_associations(&positive_task, &[]).unwrap();
    assert_eq!(positive.associations.len(), 1);
    let positive_score = positive.associations[0].score;

    let mut task_declares_exclusion = positive_task;
    task_declares_exclusion.out_of_scope = vec!["支付迁移 LegacyRouterBoundary".to_owned()];
    let declared_exclusion = engine
        .task_space_associations(&task_declares_exclusion, &[])
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
        kind: TaskSignalKind::Symbol,
        content: "LegacyRouterBoundary".to_owned(),
    }];
    let conflicted = engine
        .task_space_associations(&conflict_task, &conflict_signals)
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
    assert!(explanation.matched_task_signals.is_empty());

    let pack = engine
        .task_context_pack(&TaskContextRequest::automatic(
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
    let candidates = engine.space_intent_candidates(&query, &[]).unwrap();
    assert_eq!(candidates.candidates.len(), 1);
    let candidate = &candidates.candidates[0];
    assert!(candidate.intent_conflicted);
    let mut expected_heads = fixture.conflicted_head_ids.to_vec();
    expected_heads.sort();
    assert_eq!(candidate.head_revision_ids, expected_heads);

    let context_only_query = task("contextonlyhandoffneedle");
    let associations = engine
        .task_space_associations(&context_only_query, &[])
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
    let resolved_candidates = engine.space_intent_candidates(&query, &[]).unwrap();
    assert_eq!(resolved_candidates.candidates.len(), 1);
    assert!(!resolved_candidates.candidates[0].intent_conflicted);
    assert_eq!(resolved_candidates.candidates[0].head_revision_ids.len(), 1);
    let resolved = engine
        .task_context_pack(&TaskContextRequest::automatic(
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
        kind: TaskSignalKind::Api,
        content: "SearchV9RareEndpoint".to_owned(),
    }];
    let mut request = TaskContextRequest::automatic(
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
    let top_k = first
        .omitted
        .iter()
        .find(|omitted| omitted.reason == "space_top_k")
        .expect("fixed corpus must report top-k Space omissions");
    assert_eq!(
        top_k.count,
        fixture.total_spaces.saturating_sub(request.max_spaces)
    );
    assert!(top_k.estimated_tokens > 0);

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

    let mut generic_request = TaskContextRequest::automatic(task("implement"), Vec::new(), 900);
    generic_request.max_spaces = 4;
    let generic = engine.task_context_pack(&generic_request).unwrap();
    assert!(generic.associations.len() <= generic_request.max_spaces);
    assert!(generic.estimated_tokens <= generic_request.token_budget);
    assert_eq!(
        generic.estimated_tokens,
        estimate_task_context_payload_tokens(&generic)
    );
    assert!(
        generic
            .omitted
            .iter()
            .any(|item| item.reason == "space_top_k")
    );
}

#[test]
fn equal_fused_scores_use_stable_space_id_ties() {
    let fixture = fixture();
    let index = fixture.index.clone();
    let query = task("tieassociationneedle");
    let first = SearchEngine::new(fixture.index)
        .task_space_associations(&query, &[])
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
        .task_space_associations(&query, &[])
        .unwrap();
    assert_eq!(rebuilt.associations, first.associations);
}

#[test]
fn unsafe_context_states_cannot_supply_association_or_injection_evidence() {
    let fixture = fixture();
    let response = SearchEngine::new(fixture.index)
        .task_space_associations(&task("unsafeassociationneedle"), &[])
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
            task("unrelatedpackneedle"),
            Vec::new(),
            100_000,
        ))
        .unwrap();
    assert!(zero.associations.is_empty());
    assert!(zero.items.is_empty());

    let one = engine
        .task_context_pack(&TaskContextRequest::automatic(
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
            feature_task(),
            feature_signals(),
            100_000,
        ))
        .unwrap();
    assert_eq!(many.associations.len(), 3);
    assert_eq!(many.items.len(), 3);
    assert_eq!(
        many.items
            .iter()
            .map(|item| item.context.context_id)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            fixture.pack_contexts[0],
            fixture.pack_contexts[2],
            fixture.pack_contexts[3],
        ]
        .into_iter()
        .collect()
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
            .any(|path| matches!(path, TaskRetrievalPath::IntentFts { .. }))
    );
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
    assert!(paths.iter().all(|path| {
        matches!(
            path,
            TaskRetrievalPath::IntentFts { .. }
                | TaskRetrievalPath::ContextFts { .. }
                | TaskRetrievalPath::ExactScope { .. }
        )
    }));
    let metadata = fixture.index.metadata().unwrap();
    assert_eq!(many.indexed_tree_oid, metadata.indexed_tree_oid);
    assert_eq!(many.projection_generation, metadata.projection_generation);
}

#[test]
fn task_context_order_budget_and_fingerprint_are_stable() {
    let fixture = fixture();
    let index = fixture.index.clone();
    let task = feature_task();
    let signals = feature_signals();
    let full_request = TaskContextRequest::automatic(task.clone(), signals.clone(), 100_000);
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

    let limited_request = TaskContextRequest::automatic(task.clone(), signals.clone(), 512);
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
        kind: TaskSignalKind::Repository,
        content: "/different/local/repository".to_owned(),
    });
    let reordered_pack = SearchEngine::new(index.clone())
        .task_context_pack(&TaskContextRequest::automatic(
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
    let task = task("unsafepackintentneedle");
    let automatic = SearchEngine::new(fixture.index.clone())
        .task_context_pack(&TaskContextRequest::automatic(
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
            task_intent: task,
            task_signals: Vec::new(),
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
