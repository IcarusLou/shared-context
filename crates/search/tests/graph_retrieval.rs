//! What the Engineering Graph still decides, now that the Pack no longer asks it anything.
//!
//! This file used to carry sixteen tests and carries two. The fourteen that went were all about one
//! thing: the Graph as a *retrieval channel* into an automatic Context Pack -- an exact Artifact
//! match reached from a resolved Focus, the relation hops taken off it, the `artifact_generation`
//! and `context_tree_oid` provenance the Pack reported, the `graph_diagnostics` that explained a
//! Focus which resolved to nothing, the text fallback taken when no Graph was available, and the
//! generation-race retry the Pack performed around all of it.
//!
//! ADR-0007 removed that channel. Automatic injection reaches knowledge through a file the Session
//! opened, joined against the Engineering Reference rows in the index -- not through the Graph
//! projection, which is a separate build with its own generation and its own rename tolerance. The
//! Graph's remaining job is relocation bookkeeping, and a Pack that explained its own Graph
//! provenance was measured as costing more than it was worth. Neither is a property this file can
//! assert any more, because neither exists.
//!
//! What survives here is what the Graph still is: a builder and a resolver. A test outcome that
//! names no Artifact must not match a qualified one, and a sparse projection must not drag an
//! unrelated corpus into its generation. Those are facts about resolution, they are unchanged by
//! ADR-0007, and they are the two tests below.
//!
//! The claims worth keeping from the fourteen did not vanish with them. That no unsafe Context
//! state crosses into automatic injection is now asserted where it is stronger -- in
//! `task_association`, where the lane *does* retrieve all four unsafe states and the safety rules
//! refuse them anyway, rather than here, where the Graph channel never offered them in the first
//! place. That a Focus pointing at a file nobody wrote about yields nothing is the empty Pack, and
//! it has its own test. The one thing genuinely narrowed, and filed rather than asserted: a Focus
//! on a file that has since been renamed no longer reaches its Context, because the index-side
//! reverse lookup matches an exact path where the Graph matched through a `MatchBasis`.

use std::sync::Arc;

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ContextId, ContextKind,
    ContextRelation, ContextRelationKind, ContextRevisionDraft, EngineeringArtifact,
    EngineeringReference, EvidenceSnapshotDraft, EvidenceType, PublicationAction, PublicationDraft,
    ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId, RepositoryIdentity,
    ResolvedFocus, RevisionId, SpaceId, TaskId, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, GraphContextSnapshot, ProjectedEngineeringReference,
    RepositoryScanOutcome, RepositorySnapshot, ScanCoverage, SnapshotArtifact,
    SnapshotSourcePolicy, SourceLanguage, build_graph_context_snapshots,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{ContextPackMode, SearchEngine, TaskContextRequest};
use tempfile::TempDir;

struct GraphFixture {
    _temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    graph_store: EngineeringProjectionStore,
    source_context: ContextId,
    source_revision: RevisionId,
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
        problem_view: None,
        hints: Vec::new(),
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
    let artifact_key =
        ArtifactKey::derive(repository.repository_id.clone(), symbol_locator(name)).unwrap();
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
            repository_id: repository.repository_id.clone(),
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
        repository_id: repository.repository_id.clone(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        policy_version: "planned-paths-plus-safe-tracked-modifications-v2",
        head_tree_oid: "repo-tree".to_owned(),
        generation: generation.to_owned(),
        planned_paths,
        coverage: ScanCoverage::Complete,
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
    let store = GitStore::bootstrap_local(&root).unwrap();
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
    let (source_revision, _source_publication) = revise_context(
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
        source_context,
        source_revision,
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
        signal_history: Vec::new(),
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
            repository_id: fixture.repository.repository_id.clone(),
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
            artifact_key: ArtifactKey::derive(
                fixture.repository.repository_id.clone(),
                locator.clone(),
            )
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
        fixture.repository.repository_id.clone(),
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
