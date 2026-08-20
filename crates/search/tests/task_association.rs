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
    ContextPackMode, ContextStatus, SearchEngine, TaskContextRequest, TaskRetrievalPath,
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
        fixture.feature_spaces.into_iter().collect(),
        "one Task must retain every independently evidenced Space"
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
    let matched_contexts = response
        .associations
        .iter()
        .flat_map(|association| association.matched_contexts.iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        matched_contexts,
        fixture.feature_contexts.into_iter().collect()
    );
    let matched_artifacts = response
        .associations
        .iter()
        .flat_map(|association| association.matched_artifacts.iter())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        matched_artifacts,
        [
            "file:SearchResultsPage.tsx".to_owned(),
            "api:SearchV2Endpoint".to_owned(),
            "schema:SearchResponseV2".to_owned(),
            "test:LegacyCompatibilityTest".to_owned(),
        ]
        .into_iter()
        .collect(),
        "only exact textual engineering hints may be reported"
    );
}

#[test]
fn workspace_location_never_adds_a_space_prior_and_unrelated_task_returns_zero() {
    let fixture = fixture();
    let unrelated = task("unrelatedtaskneedle");
    let response = SearchEngine::new(fixture.index)
        .task_space_associations(
            &unrelated,
            &[TaskSignal {
                kind: TaskSignalKind::Workspace,
                content: "pageintentneedle SearchV2Endpoint analyticsdomain".to_owned(),
            }],
        )
        .unwrap();
    assert!(response.associations.is_empty());
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
    assert_eq!(many.associations.len(), 4);
    assert_eq!(many.items.len(), 4);
    assert_eq!(
        many.items
            .iter()
            .map(|item| item.context.context_id)
            .collect::<std::collections::BTreeSet<_>>(),
        fixture.pack_contexts.into_iter().collect()
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
    assert!(
        paths
            .iter()
            .any(|path| matches!(path, TaskRetrievalPath::ExactTaskSignal { .. }))
    );
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

    let limited_request = TaskContextRequest::automatic(task.clone(), signals.clone(), 64);
    let limited_first = SearchEngine::new(index.clone())
        .task_context_pack(&limited_request)
        .unwrap();
    let limited_second = SearchEngine::new(index.clone())
        .task_context_pack(&limited_request)
        .unwrap();
    assert_eq!(limited_first, limited_second);
    assert!(limited_first.estimated_tokens <= 64);
    assert!(!limited_first.omitted.is_empty());

    let mut reordered = signals;
    reordered.reverse();
    reordered.push(TaskSignal {
        kind: TaskSignalKind::Workspace,
        content: "/different/local/checkout".to_owned(),
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
