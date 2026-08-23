use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, thread};

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ConflictParticipant, ContextId,
    ContextKind, ContextRelation, ContextRelationKind, ContextRevisionDraft, EngineeringArtifact,
    EngineeringReference, EngineeringReferenceDraft, EvidenceSnapshotDraft, EvidenceType,
    PublicationAction, PublicationDraft, ReferenceId, ReferenceRelation, RepoRelativePath,
    RepositoryId, RepositoryIdentity, ResolvedFocus, RevisionId, SemanticConflictDraft, SpaceId,
    SubmissionId, TaskId, TaskSessionId, WorkEpisodeId, WorkEpisodeRef, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, GraphContextSnapshot, ProjectedEngineeringReference,
    RepositoryScanOutcome, RepositorySnapshot, SnapshotArtifact, SnapshotSourcePolicy,
    SourceLanguage, build_graph_context_snapshots,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, CandidateSubmissionRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    ContextPackMode, ContextSafetySource, SearchEngine, TaskAssociationChannel,
    TaskAssociationFusionExplanation, TaskContextRequest, TaskGraphDiagnosticKind,
    TaskRetrievalPath, estimate_task_context_payload_tokens,
};
use tempfile::TempDir;

struct GraphFixture {
    _temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    graph_store: EngineeringProjectionStore,
    source_space: SpaceId,
    contract_space: SpaceId,
    validation_space: SpaceId,
    generic_space: SpaceId,
    source_context: ContextId,
    contract_context: ContextId,
    validation_context: ContextId,
    source_revision: RevisionId,
    source_publication: sctx_domain::PublicationId,
    repository: RepositoryIdentity,
    reference: ProjectedEngineeringReference,
    context_snapshots: Vec<GraphContextSnapshot>,
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append Graph retrieval fixture Event");
}

fn add_space(store: &GitStore, title: &str, intent: &str) -> SpaceId {
    let event = Event::space_created(
        sctx_domain::IntentSnapshot {
            title: title.to_owned(),
            problem: format!("{intent} problem"),
            desired_outcome: format!("{intent} outcome"),
            in_scope: vec![intent.to_owned()],
            out_of_scope: vec![format!("{title} excluded")],
            acceptance_conditions: vec![format!("{title} accepted")],
            domain_terms: vec![format!("{title} term")],
        },
        None,
    )
    .unwrap();
    let EventPayload::SpaceCreated { space_id, .. } = event.payload() else {
        unreachable!();
    };
    let space_id = *space_id;
    append(store, event);
    space_id
}

fn draft(
    kind: ContextKind,
    statement: &str,
    platform: &str,
    relations: Vec<ContextRelation>,
) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind,
        topic_key: Some("graph/retrieval".to_owned()),
        statement: statement.to_owned(),
        rationale: "the Graph retrieval fixture captures durable cross-end behavior".to_owned(),
        applicability: Applicability {
            domains: vec!["search".to_owned()],
            platforms: vec![platform.to_owned()],
            conditions: vec!["active".to_owned()],
        },
        assumptions: vec!["the fixture remains deterministic".to_owned()],
        recheck_when: vec!["the fixture contract changes".to_owned()],
        relations,
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "Graph retrieval is safe for automatic injection".to_owned(),
            content: serde_json::json!({
                "command": "cargo test -p sctx-search --test graph_retrieval",
                "actual": "passed"
            }),
            interpretation: "the accepted Context has complete evidence".to_owned(),
            limitations: vec!["synthetic fixture".to_owned()],
        }],
    }
}

fn add_context(
    store: &GitStore,
    space_id: SpaceId,
    draft: ContextRevisionDraft,
) -> (ContextId, RevisionId, sctx_domain::PublicationId) {
    let event = Event::context_revision_added(space_id, draft, None).unwrap();
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision,
        ..
    } = event.payload()
    else {
        unreachable!();
    };
    let context_id = *context_id;
    let revision_id = revision.revision_id;
    append(store, event);
    let publication = publish(store, space_id, context_id, revision_id, Vec::new());
    (context_id, revision_id, publication)
}

fn add_unpublished_context(
    store: &GitStore,
    space_id: SpaceId,
    draft: ContextRevisionDraft,
) -> (ContextId, RevisionId) {
    let event = Event::context_revision_added(space_id, draft, None).unwrap();
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision,
        ..
    } = event.payload()
    else {
        unreachable!();
    };
    let ids = (*context_id, revision.revision_id);
    append(store, event);
    ids
}

fn revise_context(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    parent_revision_id: RevisionId,
    previous_publication_id: sctx_domain::PublicationId,
    draft: ContextRevisionDraft,
) -> (RevisionId, sctx_domain::PublicationId) {
    let event = Event::context_revised(space_id, context_id, vec![parent_revision_id], draft, None)
        .unwrap();
    let EventPayload::ContextRevisionAdded { revision, .. } = event.payload() else {
        unreachable!();
    };
    let revision_id = revision.revision_id;
    append(store, event);
    let publication = publish(
        store,
        space_id,
        context_id,
        revision_id,
        vec![previous_publication_id],
    );
    (revision_id, publication)
}

fn publish(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
    previous_publication_ids: Vec<sctx_domain::PublicationId>,
) -> sctx_domain::PublicationId {
    let event = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids,
            action: PublicationAction::Publish,
            revision_id,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextPublicationChanged { publication, .. } = event.payload() else {
        unreachable!();
    };
    let publication_id = publication.publication_id;
    append(store, event);
    publication_id
}

fn withdraw(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
    previous_publication_id: sctx_domain::PublicationId,
) {
    append(
        store,
        Event::publication_changed(
            space_id,
            context_id,
            PublicationDraft {
                previous_publication_ids: vec![previous_publication_id],
                action: PublicationAction::Withdraw,
                revision_id,
                review_event_ids: Vec::new(),
            },
            None,
        )
        .unwrap(),
    );
}

fn relation(target_context_id: ContextId, kind: ContextRelationKind) -> ContextRelation {
    ContextRelation {
        target_context_id,
        kind,
        rationale: format!("{kind:?} is required for the cross-end behavior"),
        supports: vec!["accepted integration evidence supports this hop".to_owned()],
    }
}

fn repository() -> RepositoryIdentity {
    RepositoryIdentity {
        repository_id: RepositoryId::new(),
        canonical_name: "web-search".to_owned(),
    }
}

fn symbol_locator(name: &str) -> ArtifactLocator {
    ArtifactLocator::Symbol {
        path: RepoRelativePath::new("src/search.ts").unwrap(),
        language: "typescript".to_owned(),
        module: "fe::search".to_owned(),
        enclosing_type: None,
        symbol_name: name.to_owned(),
        signature: format!("{name}()"),
    }
}

fn symbol_artifact(repository: &RepositoryIdentity, name: &str) -> SnapshotArtifact {
    let artifact_key = ArtifactKey::derive(repository.repository_id, symbol_locator(name)).unwrap();
    SnapshotArtifact {
        artifact: EngineeringArtifact {
            repository: repository.clone(),
            artifact_key,
            display_name: name.to_owned(),
        },
        snapshot_generation: "repo-current".to_owned(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        observations: vec![ArtifactObservation {
            path: "src/search.ts".to_owned(),
            line: Some(10),
            language: SourceLanguage::TypeScriptJavaScript,
            source_state: ArtifactSourceState::TrackedHead,
        }],
    }
}

fn reference(
    repository: &RepositoryIdentity,
    context_id: ContextId,
    revision_id: RevisionId,
    symbol: &str,
) -> ProjectedEngineeringReference {
    ProjectedEngineeringReference {
        context_id,
        revision_id,
        reference: EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: repository.repository_id,
            artifact_kind: ArtifactKind::Symbol,
            relation: ReferenceRelation::Implements,
            locator: symbol_locator(symbol),
            supports: "the FE Symbol implements this Context".to_owned(),
            limitations: vec!["local repository observation".to_owned()],
        },
    }
}

fn snapshot(
    repository: &RepositoryIdentity,
    generation: &str,
    mut artifacts: Vec<SnapshotArtifact>,
) -> RepositoryScanOutcome {
    artifacts.sort_by(|left, right| left.artifact.artifact_key.cmp(&right.artifact.artifact_key));
    for artifact in &mut artifacts {
        generation.clone_into(&mut artifact.snapshot_generation);
    }
    let planned_paths = artifacts
        .iter()
        .map(|artifact| artifact.artifact.artifact_key.locator().path().clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    RepositoryScanOutcome::Available(RepositorySnapshot {
        repository_id: repository.repository_id,
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        policy_version: "planned-paths-plus-safe-tracked-modifications-v2",
        head_tree_oid: "repo-tree".to_owned(),
        generation: generation.to_owned(),
        planned_paths,
        artifacts,
        scanned_files: 1,
        scanned_bytes: 100,
        skipped_files: Vec::new(),
    })
}

#[allow(clippy::too_many_lines)]
fn graph_fixture() -> GraphFixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("installation");
    let store = GitStore::initialize(&root).unwrap();
    let source_space = add_space(
        &store,
        "FrontendImplementation",
        "frontend rendering behavior",
    );
    let contract_space = add_space(&store, "ServerContract", "server response contract");
    let validation_space = add_space(&store, "IosValidation", "ios compatibility validation");
    let generic_space = add_space(&store, "GenericWorkflow", "generic workflow");

    let (source_context, source_v1, source_publication) = add_context(
        &store,
        source_space,
        draft(
            ContextKind::Decision,
            "frontend source behavior is stable",
            "fe",
            Vec::new(),
        ),
    );
    let (validation_context, _validation_revision, _) = add_context(
        &store,
        validation_space,
        draft(
            ContextKind::Validation,
            "iOS compatibility validates the response fallback",
            "ios",
            vec![relation(source_context, ContextRelationKind::ValidatedBy)],
        ),
    );
    let (contract_context, _contract_revision, _) = add_context(
        &store,
        contract_space,
        draft(
            ContextKind::Contract,
            "server contract defines SearchResponse semantics",
            "server",
            vec![relation(
                validation_context,
                ContextRelationKind::ValidatedBy,
            )],
        ),
    );
    let (source_revision, source_publication) = revise_context(
        &store,
        source_space,
        source_context,
        source_v1,
        source_publication,
        draft(
            ContextKind::Decision,
            "frontend source behavior is stable",
            "fe",
            vec![relation(contract_context, ContextRelationKind::Implements)],
        ),
    );
    add_context(
        &store,
        generic_space,
        draft(
            ContextKind::Decision,
            "generic workflow remains deterministic",
            "shared",
            Vec::new(),
        ),
    );

    let index = ProjectionIndex::for_store(&store);
    let metadata = index.synchronize().unwrap().metadata;
    let store = store.with_candidate_submission_index(Arc::new(index.clone()));
    let graph_store = EngineeringProjectionStore::initialize(&root).unwrap();
    let repository = repository();
    let artifact = symbol_artifact(&repository, "SearchSymbol");
    let reference = reference(&repository, source_context, source_revision, "SearchSymbol");
    let context_snapshots = build_graph_context_snapshots(
        &index.domain_snapshot().unwrap().projection,
        std::slice::from_ref(&reference),
    )
    .unwrap();
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&reference),
            &[snapshot(
                &repository,
                "repo-current",
                vec![artifact.clone()],
            )],
            &context_snapshots,
        )
        .unwrap();
    graph_store
        .rebuild_for_context_tree(&projection, Some(&metadata.indexed_tree_oid))
        .unwrap();

    GraphFixture {
        _temporary: temporary,
        store,
        index,
        graph_store,
        source_space,
        contract_space,
        validation_space,
        generic_space,
        source_context,
        contract_context,
        validation_context,
        source_revision,
        source_publication,
        repository,
        reference,
        context_snapshots,
    }
}

fn resolved_focus(repository_id: RepositoryId, locator: ArtifactLocator) -> ResolvedFocus {
    ResolvedFocus {
        repository_id,
        locator,
    }
}

fn task_request(
    repository_id: RepositoryId,
    mode: ContextPackMode,
    token_budget: usize,
) -> TaskContextRequest {
    let task_id = TaskId::new();
    TaskContextRequest {
        task_id,
        working_intent: WorkingIntentSnapshot {
            goal: "implement generic workflow".to_owned(),
            current_direction: Some("preserve deterministic workflow behavior".to_owned()),
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            domains: Vec::new(),
            platforms: vec!["fe".to_owned()],
            constraints: Vec::new(),
            acceptance_conditions: Vec::new(),
            artifact_hints: Vec::new(),
            interface_hints: Vec::new(),
            open_questions: Vec::new(),
        },
        task_signals: Vec::new(),
        resolved_focus: Some(resolved_focus(
            repository_id,
            symbol_locator("SearchSymbol"),
        )),
        token_budget,
        max_spaces: 8,
        candidate_limit: 100,
        mode,
    }
}

fn fusion(association: &sctx_domain::TaskSpaceAssociation) -> TaskAssociationFusionExplanation {
    association
        .reasons
        .iter()
        .find_map(|reason| serde_json::from_str(reason).ok())
        .expect("association exposes typed RRF features")
}

#[test]
#[allow(clippy::too_many_lines)]
fn exact_graph_priority_reaches_cross_end_contexts_in_two_cycle_safe_hops() {
    let fixture = graph_fixture();
    let engine =
        SearchEngine::with_engineering_graph(fixture.index.clone(), fixture.graph_store.clone());
    let request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::AutomaticInjection,
        12_000,
    );
    let pack = engine.task_context_pack(&request).unwrap();

    assert!(
        pack.artifact_generation
            .as_deref()
            .is_some_and(|value| value.starts_with("eng_"))
    );
    assert_eq!(pack.associations[0].space_id, fixture.source_space);
    assert!(
        pack.associations
            .iter()
            .any(|item| item.space_id == fixture.contract_space)
    );
    assert!(
        pack.associations
            .iter()
            .any(|item| item.space_id == fixture.validation_space)
    );
    assert!(
        pack.associations
            .iter()
            .any(|item| item.space_id == fixture.generic_space)
    );
    assert!(
        fusion(&pack.associations[0])
            .channels
            .iter()
            .any(|feature| { feature.channel == TaskAssociationChannel::ResolvedArtifactExact })
    );

    let source = pack
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.source_context)
        .unwrap();
    let contract = pack
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.contract_context)
        .unwrap();
    let validation = pack
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.validation_context)
        .unwrap();
    assert!(source.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::EngineeringGraph { relation_hops, .. } if relation_hops.is_empty()
    )));
    assert!(contract.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::EngineeringGraph { relation_hops, .. } if relation_hops.len() == 1
    )));
    assert!(validation.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::EngineeringGraph { relation_hops, .. } if relation_hops.len() == 2
    )));
    assert!(
        pack.items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .all(|path| {
                match path {
                    TaskRetrievalPath::EngineeringGraph {
                        path,
                        relation_hops,
                    } => {
                        relation_hops.len() <= 2
                            && Some(path.artifact_generation.as_str())
                                == pack.artifact_generation.as_deref()
                    }
                    TaskRetrievalPath::ContextRelation { hops } => hops.len() <= 2,
                    _ => true,
                }
            })
    );
    assert_eq!(
        pack.estimated_tokens,
        estimate_task_context_payload_tokens(&pack)
    );
    assert!(pack.estimated_tokens <= pack.token_budget);

    let mut different_focus = request;
    different_focus
        .resolved_focus
        .as_mut()
        .unwrap()
        .repository_id = RepositoryId::new();
    let different_focus_pack = engine.task_context_pack(&different_focus).unwrap();
    assert_eq!(different_focus_pack.task_fingerprint, pack.task_fingerprint);

    let mut wrong_repository = different_focus;
    wrong_repository.working_intent.goal = "zxunreachablefocus".to_owned();
    wrong_repository.working_intent.current_direction = Some("zxunreachablegraph".to_owned());
    wrong_repository.working_intent.platforms.clear();
    let filtered = engine.task_context_pack(&wrong_repository).unwrap();
    assert!(filtered.associations.is_empty());
    assert!(filtered.items.is_empty());
    assert_eq!(filtered.graph_diagnostics.len(), 1);
    assert_eq!(
        filtered.graph_diagnostics[0].kind,
        TaskGraphDiagnosticKind::ArtifactNotReachableInGraph
    );
    assert!(
        !serde_json::to_string(&filtered.graph_diagnostics)
            .unwrap()
            .contains("missing")
    );
    assert_eq!(
        filtered.estimated_tokens,
        estimate_task_context_payload_tokens(&filtered)
    );
    assert!(filtered.estimated_tokens <= filtered.token_budget);

    let mut bounded_unreachable = wrong_repository;
    bounded_unreachable.token_budget = 256;
    let bounded = engine.task_context_pack(&bounded_unreachable).unwrap();
    assert_eq!(
        bounded.estimated_tokens,
        estimate_task_context_payload_tokens(&bounded)
    );
    assert!(bounded.estimated_tokens <= bounded.token_budget);
    if bounded.graph_diagnostics.is_empty() {
        assert!(bounded.omitted.iter().any(|omitted| {
            matches!(
                omitted.reason.as_str(),
                "diagnostic_token_budget" | "omitted"
            )
        }));
    }
    assert!(filtered.associations.iter().all(|association| {
        fusion(association)
            .channels
            .iter()
            .all(|feature| feature.channel != TaskAssociationChannel::ResolvedArtifactExact)
    }));
}

#[test]
fn identical_locator_in_two_repositories_retrieves_only_focused_repository_context() {
    let fixture = graph_fixture();
    let second_repository = repository();
    let second_space = add_space(
        &fixture.store,
        "Second Repository Requirement",
        "second repository isolated graph",
    );
    let (second_context, second_revision, _) = add_context(
        &fixture.store,
        second_space,
        draft(
            ContextKind::Decision,
            "second repository owns the same qualified Symbol locator",
            "server",
            Vec::new(),
        ),
    );
    let second_reference = reference(
        &second_repository,
        second_context,
        second_revision,
        "SearchSymbol",
    );
    fixture.index.synchronize().unwrap();
    let domain = fixture.index.domain_snapshot().unwrap();
    let roots = vec![fixture.reference.clone(), second_reference];
    let contexts = build_graph_context_snapshots(&domain.projection, &roots).unwrap();
    let projection = EngineeringReferenceResolver
        .resolve(
            &roots,
            &[
                snapshot(
                    &fixture.repository,
                    "repo-one",
                    vec![symbol_artifact(&fixture.repository, "SearchSymbol")],
                ),
                snapshot(
                    &second_repository,
                    "repo-two",
                    vec![symbol_artifact(&second_repository, "SearchSymbol")],
                ),
            ],
            &contexts,
        )
        .unwrap();
    let tree = domain.metadata.indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&projection, Some(&tree))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(
        second_repository.repository_id,
        ContextPackMode::AutomaticInjection,
        12_000,
    );
    request.working_intent.goal = "opaque repository-scoped graph focus".to_owned();
    request.working_intent.current_direction =
        Some("retrieve only exact focused repository".to_owned());
    request.working_intent.platforms.clear();
    let pack = engine.task_context_pack(&request).unwrap();
    let direct = pack
        .items
        .iter()
        .filter(|item| {
            item.retrieval_paths.iter().any(|path| {
                matches!(
                    path,
                    TaskRetrievalPath::EngineeringGraph { relation_hops, .. }
                        if relation_hops.is_empty()
                )
            })
        })
        .map(|item| item.context.context_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(direct, BTreeSet::from([second_context]));
    assert!(!direct.contains(&fixture.source_context));
    assert!(pack.graph_diagnostics.is_empty());
}

#[test]
fn generic_test_outcome_never_matches_qualified_test_artifact() {
    let fixture = graph_fixture();
    let locator = ArtifactLocator::Test {
        path: RepoRelativePath::new("tests/search.rs").unwrap(),
        qualified_test_name: "search::returns_results".to_owned(),
    };
    let projected = ProjectedEngineeringReference {
        context_id: fixture.source_context,
        revision_id: fixture.source_revision,
        reference: EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: fixture.repository.repository_id,
            artifact_kind: ArtifactKind::Test,
            relation: ReferenceRelation::Validates,
            locator: locator.clone(),
            supports: "the qualified Test validates this Context".to_owned(),
            limitations: vec!["frozen test fixture".to_owned()],
        },
    };
    let artifact = SnapshotArtifact {
        artifact: EngineeringArtifact {
            repository: fixture.repository.clone(),
            artifact_key: ArtifactKey::derive(fixture.repository.repository_id, locator.clone())
                .unwrap(),
            display_name: "search::returns_results".to_owned(),
        },
        snapshot_generation: "test-outcome".to_owned(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        observations: vec![ArtifactObservation {
            path: "tests/search.rs".to_owned(),
            line: Some(10),
            language: SourceLanguage::Rust,
            source_state: ArtifactSourceState::TrackedHead,
        }],
    };
    let projection = EngineeringReferenceResolver
        .resolve(
            &[projected],
            &[snapshot(
                &fixture.repository,
                "test-outcome",
                vec![artifact],
            )],
            &fixture.context_snapshots,
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&projection, Some(&tree))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::AutomaticInjection,
        8_000,
    );
    request.resolved_focus = None;
    request.task_signals = vec![sctx_domain::TaskSignal {
        kind: sctx_domain::TaskSignalKind::TestOutcome,
        content: locator.canonical_key(),
    }];
    request.working_intent.goal = "zxtestoutcomeonly".to_owned();
    request.working_intent.current_direction = Some("zxnonlocatingoutcome".to_owned());
    request.working_intent.platforms.clear();
    let pack = engine.task_context_pack(&request).unwrap();
    assert!(pack.associations.is_empty());
    assert!(pack.items.is_empty());
    assert!(pack.graph_diagnostics.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn context_tree_mismatch_preserves_historical_graph_while_unavailable_artifacts_fall_back() {
    let fixture = graph_fixture();
    let current = fixture.graph_store.read_projection().unwrap().unwrap();
    fixture
        .graph_store
        .rebuild_for_context_tree(&current, Some("different-context-tree"))
        .unwrap();
    let engine =
        SearchEngine::with_engineering_graph(fixture.index.clone(), fixture.graph_store.clone());
    let mismatched = engine
        .task_context_pack(&task_request(
            fixture.repository.repository_id,
            ContextPackMode::AutomaticInjection,
            8_000,
        ))
        .unwrap();
    assert_eq!(
        mismatched.artifact_generation.as_deref(),
        Some(current.artifact_generation.as_str())
    );
    assert_eq!(
        mismatched.graph_context_tree_oid.as_deref(),
        Some("different-context-tree")
    );
    assert_ne!(
        mismatched.graph_context_tree_oid.as_deref(),
        Some(mismatched.indexed_tree_oid.as_str())
    );
    assert!(mismatched.associations.iter().any(|item| {
        fusion(item)
            .channels
            .iter()
            .any(|feature| feature.channel == TaskAssociationChannel::ResolvedArtifactExact)
    }));
    assert!(
        mismatched
            .items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .any(|path| { matches!(path, TaskRetrievalPath::EngineeringGraph { .. }) })
    );
    assert!(
        mismatched
            .associations
            .iter()
            .any(|item| item.space_id == fixture.generic_space)
    );

    let unavailable = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&fixture.reference),
            &[RepositoryScanOutcome::Unavailable {
                repository_id: fixture.repository.repository_id,
                reason: "checkout is offline".to_owned(),
            }],
            &fixture.context_snapshots,
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&unavailable, Some(&tree))
        .unwrap();
    let fallback = engine
        .task_context_pack(&task_request(
            fixture.repository.repository_id,
            ContextPackMode::AutomaticInjection,
            8_000,
        ))
        .unwrap();
    assert_eq!(
        fallback.artifact_generation.as_deref(),
        Some(unavailable.artifact_generation.as_str())
    );
    assert!(fallback.associations.iter().all(|item| {
        fusion(item)
            .channels
            .iter()
            .all(|feature| feature.channel != TaskAssociationChannel::ResolvedArtifactExact)
    }));
    assert!(
        fallback
            .associations
            .iter()
            .any(|item| item.space_id == fixture.generic_space)
    );
    assert_eq!(fallback.graph_diagnostics.len(), 1);
    assert_eq!(
        fallback.graph_diagnostics[0].kind,
        TaskGraphDiagnosticKind::ArtifactNotReachableInGraph
    );

    let mut diagnostic_request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::Explicit,
        8_000,
    );
    diagnostic_request.working_intent.goal = "frontend source behavior".to_owned();
    diagnostic_request.working_intent.current_direction =
        Some("inspect offline repository evidence".to_owned());
    let diagnostic = engine.task_context_pack(&diagnostic_request).unwrap();
    assert!(diagnostic.graph_diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == TaskGraphDiagnosticKind::ArtifactNotReachableInGraph
            && diagnostic.resolved_focus.repository_id == fixture.repository.repository_id
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn historical_graph_revision_survives_append_new_revision_withdraw_and_index_rebuild() {
    let fixture = graph_fixture();
    let built = fixture.graph_store.read_snapshot().unwrap().unwrap();
    let built_bytes = fixture.graph_store.canonical_bytes().unwrap().unwrap();
    let built_tree = built.context_tree_oid.clone().unwrap();
    let built_generation = built.projection.artifact_generation.clone();

    fixture
        .store
        .submit_candidate(CandidateSubmissionRequest {
            submission_id: SubmissionId::new(),
            source_episode: WorkEpisodeRef {
                episode_id: WorkEpisodeId::new(),
                task_session_id: TaskSessionId::new(),
                task_id: TaskId::new(),
            },
            content: draft(
                ContextKind::Discovery,
                "unrelated Candidate does not mutate an existing Graph",
                "shared",
                Vec::new(),
            ),
        })
        .unwrap();
    append(
        &fixture.store,
        Event::engineering_reference_recorded(
            fixture.source_context,
            fixture.source_revision,
            EngineeringReferenceDraft {
                repository_id: fixture.repository.repository_id,
                artifact_kind: ArtifactKind::Symbol,
                relation: ReferenceRelation::Implements,
                locator: symbol_locator("SearchSymbol"),
                supports: "an appended Reference is not an implicit Graph rebuild".to_owned(),
                limitations: vec!["the Graph remains an explicit snapshot".to_owned()],
            },
            None,
        )
        .unwrap(),
    );
    add_context(
        &fixture.store,
        fixture.generic_space,
        draft(
            ContextKind::Discovery,
            "unrelatedcurrentneedle Context and Publication are append-only",
            "shared",
            Vec::new(),
        ),
    );
    let (new_revision, new_publication) = revise_context(
        &fixture.store,
        fixture.source_space,
        fixture.source_context,
        fixture.source_revision,
        fixture.source_publication,
        draft(
            ContextKind::Decision,
            "newcurrentneedle frontend behavior is the current revision",
            "fe",
            Vec::new(),
        ),
    );
    fixture.index.synchronize().unwrap();

    let engine =
        SearchEngine::with_engineering_graph(fixture.index.clone(), fixture.graph_store.clone());
    let mut collision_request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::AutomaticInjection,
        20_000,
    );
    collision_request.working_intent.goal = "newcurrentneedle".to_owned();
    collision_request.working_intent.current_direction =
        Some("compare current text with frozen implementation".to_owned());
    let collision = engine.task_context_pack(&collision_request).unwrap();
    assert_eq!(
        collision.graph_context_tree_oid.as_deref(),
        Some(built_tree.as_str())
    );
    assert_ne!(collision.indexed_tree_oid, built_tree);
    assert_eq!(
        collision.artifact_generation.as_deref(),
        Some(built_generation.as_str())
    );
    let same_context = collision
        .items
        .iter()
        .filter(|item| item.context.context_id == fixture.source_context)
        .collect::<Vec<_>>();
    assert_eq!(
        same_context.len(),
        2,
        "old Graph and current FTS revisions must coexist"
    );
    let historical = same_context
        .iter()
        .find(|item| item.context.revision_id == fixture.source_revision)
        .unwrap();
    assert!(
        historical
            .retrieval_paths
            .iter()
            .any(|path| matches!(path, TaskRetrievalPath::EngineeringGraph { .. }))
    );
    assert!(matches!(
        historical.context.safety_source,
        ContextSafetySource::EngineeringGraphSnapshot { revision_id, .. }
            if revision_id == fixture.source_revision
    ));
    let current = same_context
        .iter()
        .find(|item| item.context.revision_id == new_revision)
        .unwrap();
    assert!(
        current
            .retrieval_paths
            .iter()
            .any(|path| matches!(path, TaskRetrievalPath::ContextFts { .. }))
    );
    assert!(
        current
            .retrieval_paths
            .iter()
            .all(|path| !matches!(path, TaskRetrievalPath::EngineeringGraph { .. }))
    );
    assert_eq!(
        current.context.safety_source,
        ContextSafetySource::CurrentProjection
    );

    withdraw(
        &fixture.store,
        fixture.source_space,
        fixture.source_context,
        new_revision,
        new_publication,
    );
    fixture.index.synchronize().unwrap();
    let after_withdraw = engine.task_context_pack(&collision_request).unwrap();
    let source_items = after_withdraw
        .items
        .iter()
        .filter(|item| item.context.context_id == fixture.source_context)
        .collect::<Vec<_>>();
    assert_eq!(source_items.len(), 1);
    assert_eq!(source_items[0].context.revision_id, fixture.source_revision);
    assert_eq!(
        source_items[0].context.status,
        sctx_search::ContextStatus::Accepted
    );
    assert!(source_items[0].context.auto_injection_eligible);
    assert!(matches!(
        source_items[0].context.safety_source,
        ContextSafetySource::EngineeringGraphSnapshot { .. }
    ));
    let frozen_relation_target = after_withdraw
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.contract_context)
        .unwrap();
    assert!(
        frozen_relation_target
            .retrieval_paths
            .iter()
            .any(|path| matches!(
                path,
                TaskRetrievalPath::EngineeringGraph { relation_hops, .. }
                    if relation_hops.first().is_some_and(|hop| {
                        hop.source_context_id == fixture.source_context
                            && hop.source_revision_id == fixture.source_revision
                            && hop.target_context_id == fixture.contract_context
                    })
            ))
    );

    let database = fixture.index.database_path().to_path_buf();
    remove_file_if_present(&database);
    remove_file_if_present(&PathBuf::from(format!("{}-wal", database.display())));
    remove_file_if_present(&PathBuf::from(format!("{}-shm", database.display())));
    fixture.index.synchronize().unwrap();
    let rebuilt = engine.task_context_pack(&collision_request).unwrap();
    assert!(rebuilt.items.iter().any(|item| {
        item.context.context_id == fixture.source_context
            && item.context.revision_id == fixture.source_revision
            && item
                .retrieval_paths
                .iter()
                .any(|path| matches!(path, TaskRetrievalPath::EngineeringGraph { .. }))
    }));
    assert_eq!(
        fixture.graph_store.canonical_bytes().unwrap().unwrap(),
        built_bytes
    );

    let graph_store = Arc::new(fixture.graph_store.clone());
    let projection = Arc::new(built.projection);
    let provenance = Arc::new(built_tree);
    let writer_store = Arc::clone(&graph_store);
    let writer_projection = Arc::clone(&projection);
    let writer_provenance = Arc::clone(&provenance);
    let writer = thread::spawn(move || {
        for _ in 0..20 {
            writer_store
                .rebuild_for_context_tree(&writer_projection, Some(&writer_provenance))
                .unwrap();
        }
    });
    let readers = (0..8)
        .map(|_| {
            let engine = engine.clone();
            let request = collision_request.clone();
            let expected_generation = built_generation.clone();
            let expected_revision = fixture.source_revision;
            thread::spawn(move || {
                for _ in 0..10 {
                    let pack = engine.task_context_pack(&request).unwrap();
                    assert_eq!(
                        pack.artifact_generation.as_deref(),
                        Some(expected_generation.as_str())
                    );
                    assert!(pack.items.iter().any(|item| {
                        item.context.revision_id == expected_revision
                            && item.retrieval_paths.iter().any(|path| {
                                matches!(path, TaskRetrievalPath::EngineeringGraph { .. })
                            })
                    }));
                }
            })
        })
        .collect::<Vec<_>>();
    writer.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
}

fn remove_file_if_present(path: &std::path::Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

#[test]
fn sparse_graph_builder_excludes_large_unrelated_context_corpus_from_projection_and_generation() {
    let fixture = graph_fixture();
    let before = fixture.graph_store.read_snapshot().unwrap().unwrap();
    assert_eq!(before.projection.contexts.len(), 3);
    for index in 0..64 {
        let space = add_space(
            &fixture.store,
            &format!("Unrelated Space {index}"),
            &format!("unrelated-space-{index}"),
        );
        add_context(
            &fixture.store,
            space,
            draft(
                ContextKind::Discovery,
                &format!("unrelated-corpus-{index} must not enter Engineering Graph"),
                "shared",
                Vec::new(),
            ),
        );
    }
    let domain = fixture.index.domain_snapshot().unwrap();
    let contexts =
        build_graph_context_snapshots(&domain.projection, std::slice::from_ref(&fixture.reference))
            .unwrap();
    assert_eq!(contexts, before.projection.contexts);
    assert!(
        contexts
            .iter()
            .all(|context| { !context.revision.statement.contains("unrelated-corpus-") })
    );
    let rebuilt = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&fixture.reference),
            &[snapshot(
                &fixture.repository,
                "repo-current",
                vec![symbol_artifact(&fixture.repository, "SearchSymbol")],
            )],
            &contexts,
        )
        .unwrap();
    assert_eq!(
        rebuilt.artifact_generation, before.projection.artifact_generation,
        "unreachable Context append must not alter Graph generation"
    );
    let current_tree = domain.metadata.indexed_tree_oid;
    assert_ne!(
        before.context_tree_oid.as_deref(),
        Some(current_tree.as_str())
    );
    fixture
        .graph_store
        .rebuild_for_context_tree(&rebuilt, Some(&current_tree))
        .unwrap();
    let stored = fixture.graph_store.read_snapshot().unwrap().unwrap();
    assert_eq!(stored.projection.contexts.len(), 3);
    assert_eq!(stored.projection, rebuilt);
}

#[test]
#[allow(clippy::too_many_lines)]
fn build_time_candidate_incomplete_and_conflicted_contexts_never_cross_automatic_graph_boundary() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("unsafe-graph");
    let store = GitStore::initialize(&root).unwrap();
    let space = add_space(&store, "Unsafe Graph", "unsafe graph diagnostics");
    let (candidate_context, candidate_revision) = add_unpublished_context(
        &store,
        space,
        draft(
            ContextKind::Decision,
            "candidate Graph Context must remain explicit only",
            "fe",
            Vec::new(),
        ),
    );
    let mut incomplete = draft(
        ContextKind::Decision,
        "incomplete Graph Context must remain explicit only",
        "fe",
        Vec::new(),
    );
    incomplete.evidence[0].limitations.clear();
    let (incomplete_context, incomplete_revision, _) = add_context(&store, space, incomplete);
    let (conflict_context, conflict_revision, conflict_publication) = add_context(
        &store,
        space,
        draft(
            ContextKind::Decision,
            "conflicted Graph Context must remain explicit only",
            "fe",
            Vec::new(),
        ),
    );
    let (other_context, other_revision, other_publication) = add_context(
        &store,
        space,
        draft(
            ContextKind::Decision,
            "opposing conflicted Graph Context",
            "fe",
            Vec::new(),
        ),
    );
    append(
        &store,
        Event::semantic_conflict_opened(
            space,
            SemanticConflictDraft {
                participants: vec![
                    ConflictParticipant {
                        context_id: conflict_context,
                        revision_id: conflict_revision,
                        publication_id: conflict_publication,
                    },
                    ConflictParticipant {
                        context_id: other_context,
                        revision_id: other_revision,
                        publication_id: other_publication,
                    },
                ],
                reason: "the two accepted decisions intentionally conflict".to_owned(),
                applicability: Applicability {
                    domains: vec!["search".to_owned()],
                    platforms: vec!["fe".to_owned()],
                    conditions: vec!["active".to_owned()],
                },
            },
            None,
        )
        .unwrap(),
    );

    let repository = repository();
    let roots = [
        (candidate_context, candidate_revision, "CandidateSymbol"),
        (incomplete_context, incomplete_revision, "IncompleteSymbol"),
        (conflict_context, conflict_revision, "ConflictSymbol"),
    ]
    .into_iter()
    .map(|(context_id, revision_id, symbol)| {
        reference(&repository, context_id, revision_id, symbol)
    })
    .collect::<Vec<_>>();
    let index = ProjectionIndex::for_store(&store);
    let domain = index.domain_snapshot().unwrap();
    let contexts = build_graph_context_snapshots(&domain.projection, &roots).unwrap();
    assert_eq!(contexts.len(), 3);
    assert!(contexts.iter().all(|context| {
        !context.safety.automatic_injection_eligible && !context.safety.blockers.is_empty()
    }));
    let projection = EngineeringReferenceResolver
        .resolve(
            &roots,
            &[snapshot(
                &repository,
                "unsafe-repository",
                ["CandidateSymbol", "IncompleteSymbol", "ConflictSymbol"]
                    .into_iter()
                    .map(|symbol| symbol_artifact(&repository, symbol))
                    .collect(),
            )],
            &contexts,
        )
        .unwrap();
    let graph_store = EngineeringProjectionStore::initialize(&root).unwrap();
    graph_store
        .rebuild_for_context_tree(&projection, Some(&domain.metadata.indexed_tree_oid))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(index, graph_store);
    for (context_id, symbol) in [
        (candidate_context, "CandidateSymbol"),
        (incomplete_context, "IncompleteSymbol"),
        (conflict_context, "ConflictSymbol"),
    ] {
        let mut automatic = task_request(
            repository.repository_id,
            ContextPackMode::AutomaticInjection,
            8_000,
        );
        automatic.working_intent.goal = "opaque unsafe graph lookup".to_owned();
        automatic.working_intent.current_direction =
            Some("inspect without text fallback".to_owned());
        automatic.resolved_focus.as_mut().unwrap().locator = symbol_locator(symbol);
        let automatic_pack = engine.task_context_pack(&automatic).unwrap();
        assert!(
            automatic_pack
                .items
                .iter()
                .all(|item| item.context.context_id != context_id)
        );
        assert_eq!(automatic_pack.graph_diagnostics.len(), 1);
        assert_eq!(
            automatic_pack.graph_diagnostics[0].resolved_focus,
            *automatic.resolved_focus.as_ref().unwrap()
        );

        automatic.mode = ContextPackMode::Explicit;
        let explicit = engine.task_context_pack(&automatic).unwrap();
        assert!(explicit.graph_diagnostics.is_empty());
        let item = explicit
            .items
            .iter()
            .find(|item| item.context.context_id == context_id)
            .unwrap();
        assert!(!item.context.auto_injection_eligible);
        assert!(matches!(
            &item.context.safety_source,
            ContextSafetySource::EngineeringGraphSnapshot { safety, .. }
                if !safety.automatic_injection_eligible && !safety.blockers.is_empty()
        ));
    }
}

#[test]
fn ambiguous_edges_are_explicit_diagnostics_only_and_never_raise_automatic_eligibility() {
    let fixture = graph_fixture();
    let mut ambiguous_artifact = symbol_artifact(&fixture.repository, "SearchSymbol");
    ambiguous_artifact.observations.push(ArtifactObservation {
        path: "src/search.ts".to_owned(),
        line: Some(20),
        language: SourceLanguage::TypeScriptJavaScript,
        source_state: ArtifactSourceState::TrackedHead,
    });
    let ambiguous_reference = reference(
        &fixture.repository,
        fixture.source_context,
        fixture.source_revision,
        "SearchSymbol",
    );
    let context_snapshots = fixture.context_snapshots.clone();
    let ambiguous = EngineeringReferenceResolver
        .resolve(
            &[ambiguous_reference],
            &[snapshot(
                &fixture.repository,
                "repo-ambiguous",
                vec![ambiguous_artifact],
            )],
            &context_snapshots,
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&ambiguous, Some(&tree))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::Explicit,
        12_000,
    );
    request.working_intent.goal = "frontend source behavior".to_owned();
    request.working_intent.current_direction = Some("inspect frontend source decision".to_owned());
    let explicit = engine.task_context_pack(&request).unwrap();
    assert!(explicit.graph_diagnostics.is_empty());
    let source = explicit
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.source_context)
        .unwrap();
    assert!(source.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::GraphDiagnostic { diagnostic }
            if diagnostic.resolution_status == sctx_domain::ResolutionStatus::Ambiguous
    )));
    assert!(
        fusion(
            explicit
                .associations
                .iter()
                .find(|item| item.space_id == fixture.source_space)
                .unwrap()
        )
        .channels
        .iter()
        .all(|feature| feature.channel != TaskAssociationChannel::ResolvedArtifactExact)
    );

    request.mode = ContextPackMode::AutomaticInjection;
    let automatic = engine.task_context_pack(&request).unwrap();
    assert_eq!(automatic.graph_diagnostics.len(), 1);
    assert_eq!(
        automatic.graph_diagnostics[0].resolved_focus,
        *request.resolved_focus.as_ref().unwrap()
    );
    assert!(
        automatic
            .items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .all(|path| {
                !matches!(
                    path,
                    TaskRetrievalPath::EngineeringGraph { .. }
                        | TaskRetrievalPath::GraphDiagnostic { .. }
                )
            })
    );
}

#[test]
fn graph_paths_remain_budgeted_and_top_k_deterministic() {
    let fixture = graph_fixture();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::AutomaticInjection,
        900,
    );
    request.max_spaces = 2;
    let first = engine.task_context_pack(&request).unwrap();
    let second = engine.task_context_pack(&request).unwrap();
    assert_eq!(first, second);
    assert!(first.associations.len() <= 2);
    assert_eq!(
        first.estimated_tokens,
        estimate_task_context_payload_tokens(&first)
    );
    assert!(first.estimated_tokens <= first.token_budget);
    assert!(first.omitted.iter().any(|item| {
        matches!(
            item.reason.as_str(),
            "space_top_k" | "space_token_budget" | "item_token_budget"
        )
    }));
}

#[test]
fn exact_file_signal_uses_repository_relative_path_locator() {
    let fixture = graph_fixture();
    let locator = ArtifactLocator::File {
        path: RepoRelativePath::new("src/search.ts").unwrap(),
    };
    let artifact_key =
        ArtifactKey::derive(fixture.repository.repository_id, locator.clone()).unwrap();
    let file = SnapshotArtifact {
        artifact: EngineeringArtifact {
            repository: fixture.repository.clone(),
            artifact_key,
            display_name: "src/search.ts".to_owned(),
        },
        snapshot_generation: "repo-file".to_owned(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        observations: vec![ArtifactObservation {
            path: "src/search.ts".to_owned(),
            line: None,
            language: SourceLanguage::TypeScriptJavaScript,
            source_state: ArtifactSourceState::TrackedHead,
        }],
    };
    let projected = ProjectedEngineeringReference {
        context_id: fixture.source_context,
        revision_id: fixture.source_revision,
        reference: EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: fixture.repository.repository_id,
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator: locator.clone(),
            supports: "the FE file implements the source decision".to_owned(),
            limitations: vec!["path is a rebuildable locator".to_owned()],
        },
    };
    let context_snapshots = fixture.context_snapshots.clone();
    let projection = EngineeringReferenceResolver
        .resolve(
            &[projected],
            &[snapshot(&fixture.repository, "repo-file", vec![file])],
            &context_snapshots,
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&projection, Some(&tree))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(
        fixture.repository.repository_id,
        ContextPackMode::AutomaticInjection,
        8_000,
    );
    request.resolved_focus = Some(resolved_focus(
        fixture.repository.repository_id,
        locator.clone(),
    ));
    let pack = engine.task_context_pack(&request).unwrap();
    let source = pack
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.source_context)
        .unwrap();
    assert!(source.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::EngineeringGraph { path, .. }
            if path.resolved_focus.locator == locator
                && path.resolved_focus.repository_id == fixture.repository.repository_id
                && path.artifact_key.locator() == &path.resolved_focus.locator
    )));
}
