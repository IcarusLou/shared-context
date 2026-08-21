use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKeyBasis, ArtifactKind, ContentFingerprint, ContextId,
    ContextKind, ContextRelation, ContextRelationKind, ContextRevisionDraft, EngineeringArtifact,
    EngineeringReference, EvidenceSnapshotDraft, EvidenceType, LocatorHints, PublicationAction,
    PublicationDraft, ReferenceId, ReferenceRelation, RepositoryId, RepositoryIdentity, RevisionId,
    SemanticFingerprint, SpaceId, TaskId, TaskIntent, TaskSignal, TaskSignalKind,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, ProjectedEngineeringReference, RepositoryScanOutcome,
    RepositorySnapshot, SnapshotArtifact, SnapshotSourcePolicy, SourceLanguage,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    ContextPackMode, SearchEngine, TaskAssociationChannel, TaskAssociationFusionExplanation,
    TaskContextRequest, TaskRetrievalPath, estimate_task_context_payload_tokens,
};
use tempfile::TempDir;

struct GraphFixture {
    _temporary: TempDir,
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
    repository: RepositoryIdentity,
    reference: ProjectedEngineeringReference,
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
        semantic_fingerprint: SemanticFingerprint::new("repository:web-search").unwrap(),
    }
}

fn symbol_artifact(
    repository: &RepositoryIdentity,
    name: &str,
    semantic: &str,
) -> SnapshotArtifact {
    let artifact_key = ArtifactKey::derive(
        repository.repository_id,
        ArtifactKind::Symbol,
        ArtifactKeyBasis::Logical {
            namespace: Some("fe::search".to_owned()),
            logical_name: name.to_owned(),
        },
    )
    .unwrap();
    SnapshotArtifact {
        artifact: EngineeringArtifact {
            repository: repository.clone(),
            artifact_key,
            display_name: name.to_owned(),
            locator_hints: LocatorHints {
                module: Some("src/search.ts".to_owned()),
                path: Some("src/search.ts".to_owned()),
                symbol: Some(name.to_owned()),
                language: Some("typescript".to_owned()),
                ..LocatorHints::default()
            },
            content_fingerprint: None,
            semantic_fingerprint: Some(SemanticFingerprint::new(semantic).unwrap()),
        },
        snapshot_generation: "repo-current".to_owned(),
        source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
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
    symbol: Option<&str>,
    semantic: Option<&str>,
) -> ProjectedEngineeringReference {
    ProjectedEngineeringReference {
        context_id,
        revision_id,
        reference: EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: repository.repository_id,
            artifact_kind: ArtifactKind::Symbol,
            relation: ReferenceRelation::Implements,
            locator_hints: symbol.map(|symbol| LocatorHints {
                module: Some("src/search.ts".to_owned()),
                path: None,
                symbol: Some(symbol.to_owned()),
                language: Some("typescript".to_owned()),
                ..LocatorHints::default()
            }),
            content_fingerprint: None,
            semantic_fingerprint: semantic.map(|value| SemanticFingerprint::new(value).unwrap()),
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
    RepositoryScanOutcome::Available(RepositorySnapshot {
        repository_id: repository.repository_id,
        source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
        policy_version: "tracked-head-plus-safe-tracked-modifications-v1",
        head_tree_oid: "repo-tree".to_owned(),
        generation: generation.to_owned(),
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
    let (source_revision, _) = revise_context(
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
    let graph_store = EngineeringProjectionStore::initialize(&root).unwrap();
    let repository = repository();
    let artifact = symbol_artifact(&repository, "SearchSymbol", "semantic:search-symbol");
    let reference = reference(
        &repository,
        source_context,
        source_revision,
        Some("SearchSymbol"),
        None,
    );
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&reference),
            &[snapshot(
                &repository,
                "repo-current",
                vec![artifact.clone()],
            )],
            None,
        )
        .unwrap();
    graph_store
        .rebuild_for_context_tree(&projection, Some(&metadata.indexed_tree_oid))
        .unwrap();

    GraphFixture {
        _temporary: temporary,
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
        repository,
        reference,
    }
}

fn task_request(mode: ContextPackMode, token_budget: usize) -> TaskContextRequest {
    TaskContextRequest {
        task_intent: TaskIntent {
            task_id: TaskId::new(),
            goal: "implement generic workflow".to_owned(),
            desired_change: "preserve deterministic workflow behavior".to_owned(),
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            domains: Vec::new(),
            platforms: vec!["fe".to_owned()],
            constraints: Vec::new(),
            acceptance_conditions: Vec::new(),
            artifacts: Vec::new(),
            interfaces: Vec::new(),
            unknowns: Vec::new(),
        },
        task_signals: vec![TaskSignal {
            kind: TaskSignalKind::Symbol,
            content: "SearchSymbol".to_owned(),
        }],
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
fn exact_graph_priority_reaches_cross_end_contexts_in_two_cycle_safe_hops() {
    let fixture = graph_fixture();
    let engine =
        SearchEngine::with_engineering_graph(fixture.index.clone(), fixture.graph_store.clone());
    let request = task_request(ContextPackMode::AutomaticInjection, 12_000);
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

    let mut wrong_repository = request;
    wrong_repository.task_signals.push(TaskSignal {
        kind: TaskSignalKind::Repository,
        content: RepositoryId::new().to_string(),
    });
    let filtered = engine.task_context_pack(&wrong_repository).unwrap();
    assert_ne!(filtered.task_fingerprint, pack.task_fingerprint);
    assert!(filtered.associations.iter().all(|association| {
        fusion(association)
            .channels
            .iter()
            .all(|feature| feature.channel != TaskAssociationChannel::ResolvedArtifactExact)
    }));
}

#[test]
fn mismatched_or_unavailable_graph_degrades_without_cross_snapshot_edges() {
    let fixture = graph_fixture();
    let current = fixture.graph_store.read_projection().unwrap().unwrap();
    fixture
        .graph_store
        .rebuild_for_context_tree(&current, Some("different-context-tree"))
        .unwrap();
    let engine =
        SearchEngine::with_engineering_graph(fixture.index.clone(), fixture.graph_store.clone());
    let mismatched = engine
        .task_context_pack(&task_request(ContextPackMode::AutomaticInjection, 8_000))
        .unwrap();
    assert_eq!(mismatched.artifact_generation, None);
    assert!(mismatched.associations.iter().all(|item| {
        fusion(item)
            .channels
            .iter()
            .all(|feature| feature.channel != TaskAssociationChannel::ResolvedArtifactExact)
    }));
    assert!(
        mismatched
            .items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .all(|path| { !matches!(path, TaskRetrievalPath::EngineeringGraph { .. }) })
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
            Some(&current),
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&unavailable, Some(&tree))
        .unwrap();
    let fallback = engine
        .task_context_pack(&task_request(ContextPackMode::AutomaticInjection, 8_000))
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

    let mut diagnostic_request = task_request(ContextPackMode::Explicit, 8_000);
    diagnostic_request.task_intent.goal = "frontend source behavior".to_owned();
    diagnostic_request.task_intent.desired_change =
        "inspect offline repository evidence".to_owned();
    diagnostic_request.task_signals = vec![TaskSignal {
        kind: TaskSignalKind::Repository,
        content: fixture.repository.repository_id.to_string(),
    }];
    let diagnostic = engine.task_context_pack(&diagnostic_request).unwrap();
    assert!(diagnostic.items.iter().flat_map(|item| &item.retrieval_paths).any(|path| {
        matches!(
            path,
            TaskRetrievalPath::GraphDiagnostic { diagnostic }
                if diagnostic.resolution_status == sctx_domain::ResolutionStatus::Unavailable
        )
    }));
}

#[test]
fn ambiguous_edges_are_explicit_diagnostics_only_and_never_raise_automatic_eligibility() {
    let fixture = graph_fixture();
    let semantic = "semantic:ambiguous-symbol";
    let left = symbol_artifact(&fixture.repository, "SearchSymbol", semantic);
    let right = symbol_artifact(&fixture.repository, "SearchSymbolAlternative", semantic);
    let ambiguous_reference = reference(
        &fixture.repository,
        fixture.source_context,
        fixture.source_revision,
        None,
        Some(semantic),
    );
    let ambiguous = EngineeringReferenceResolver
        .resolve(
            &[ambiguous_reference],
            &[snapshot(
                &fixture.repository,
                "repo-ambiguous",
                vec![left, right],
            )],
            None,
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&ambiguous, Some(&tree))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(ContextPackMode::Explicit, 12_000);
    request.task_intent.goal = "frontend source behavior".to_owned();
    request.task_intent.desired_change = "inspect frontend source decision".to_owned();
    let explicit = engine.task_context_pack(&request).unwrap();
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
    let mut request = task_request(ContextPackMode::AutomaticInjection, 900);
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
fn exact_file_signal_uses_current_projection_locator_not_a_path_identity_key() {
    let fixture = graph_fixture();
    let fingerprint = ContentFingerprint::new("content:file-search").unwrap();
    let artifact_key = ArtifactKey::derive(
        fixture.repository.repository_id,
        ArtifactKind::File,
        ArtifactKeyBasis::ContentFingerprint {
            fingerprint: fingerprint.clone(),
        },
    )
    .unwrap();
    let file = SnapshotArtifact {
        artifact: EngineeringArtifact {
            repository: fixture.repository.clone(),
            artifact_key,
            display_name: "src/search.ts".to_owned(),
            locator_hints: LocatorHints {
                path: Some("src/search.ts".to_owned()),
                language: Some("typescript".to_owned()),
                ..LocatorHints::default()
            },
            content_fingerprint: Some(fingerprint),
            semantic_fingerprint: Some(SemanticFingerprint::new("semantic:file-search").unwrap()),
        },
        snapshot_generation: "repo-file".to_owned(),
        source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
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
            locator_hints: Some(LocatorHints {
                path: Some("src/search.ts".to_owned()),
                ..LocatorHints::default()
            }),
            content_fingerprint: None,
            semantic_fingerprint: None,
            supports: "the FE file implements the source decision".to_owned(),
            limitations: vec!["path is a rebuildable locator".to_owned()],
        },
    };
    let projection = EngineeringReferenceResolver
        .resolve(
            &[projected],
            &[snapshot(&fixture.repository, "repo-file", vec![file])],
            None,
        )
        .unwrap();
    let tree = fixture.index.metadata().unwrap().indexed_tree_oid;
    fixture
        .graph_store
        .rebuild_for_context_tree(&projection, Some(&tree))
        .unwrap();
    let engine = SearchEngine::with_engineering_graph(fixture.index, fixture.graph_store);
    let mut request = task_request(ContextPackMode::AutomaticInjection, 8_000);
    request.task_signals = vec![TaskSignal {
        kind: TaskSignalKind::File,
        content: "src/search.ts".to_owned(),
    }];
    let pack = engine.task_context_pack(&request).unwrap();
    let source = pack
        .items
        .iter()
        .find(|item| item.context.context_id == fixture.source_context)
        .unwrap();
    assert!(source.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::EngineeringGraph { path, .. }
            if path.task_signal_kind == TaskSignalKind::File
                && path.task_signal_content == "src/search.ts"
                && path.artifact_key.basis()
                    == &ArtifactKeyBasis::ContentFingerprint {
                        fingerprint: ContentFingerprint::new("content:file-search").unwrap()
                    }
    )));
}
