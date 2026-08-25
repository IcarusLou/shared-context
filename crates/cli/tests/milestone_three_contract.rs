use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

use sctx_domain::{
    Applicability, ArtifactKind, ContextGovernanceStatus, ContextId, ContextKind,
    ContextRelationKind, ContextRevisionDraft, EvidenceSnapshotDraft, EvidenceType,
    PublicationAction, PublicationDraft, ReferenceId, RepositoryId, RepositoryIdentity,
    ResolutionStatus, ResolvedFocus, SpaceId, TaskId, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    EngineeringProjection, EngineeringProjectionStore, EngineeringReferenceResolver,
    ProjectedEngineeringReference, RepositoryScanOutcome, RepositoryScanPlan, RepositoryScanner,
    RepositoryScannerLimits, RepositorySnapshot, SourceLanguage, build_graph_context_snapshots,
};
use sctx_event_schema::{Event, EventPayload, ParsedEvent, parse_event};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::{IndexMetadata, ProjectionIndex};
use sctx_search::{
    ContextPackMode, ContextSafetySource, SearchEngine, TaskContextPack, TaskContextRequest,
    TaskRetrievalPath, estimate_task_context_payload_tokens,
};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;

const ORACLE_JSON: &str = include_str!("../../../tests/oracles/milestone-three-v1.json");
const FIXTURE_REPOSITORY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/milestone-three/repository"
);

#[derive(Clone, Debug, Deserialize)]
struct Oracle {
    expected: Expected,
    events: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct Expected {
    repository_id: String,
    spaces: BTreeMap<String, String>,
    contexts: BTreeMap<String, String>,
    revisions: BTreeMap<String, String>,
    references: BTreeMap<String, String>,
    artifacts: BTreeMap<String, ExpectedArtifact>,
    relations_from_symbol: Vec<ExpectedRelation>,
    planned_paths: Vec<String>,
    languages: Vec<String>,
    token_budget: usize,
    bounded_token_budget: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedArtifact {
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    old_name: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    old_path: Option<String>,
    #[serde(default)]
    new_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedRelation {
    source: String,
    target: String,
    kind: String,
    depth: u8,
}

struct MilestoneThreeFixture {
    _temporary: TempDir,
    store: GitStore,
    root: PathBuf,
    repository_path: PathBuf,
    oracle: Oracle,
    repository: RepositoryIdentity,
    scan_plan: RepositoryScanPlan,
    index: ProjectionIndex,
    metadata: IndexMetadata,
    snapshot: RepositorySnapshot,
    references: Vec<ProjectedEngineeringReference>,
    graph_store: EngineeringProjectionStore,
    projection: EngineeringProjection,
}

impl MilestoneThreeFixture {
    #[allow(clippy::too_many_lines)]
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("shared context root");
        let repository_path = temporary.path().join("多语言 repository");
        copy_tree(Path::new(FIXTURE_REPOSITORY), &repository_path);
        init_git_repository(&repository_path);

        let oracle: Oracle = serde_json::from_str(ORACLE_JSON).unwrap();
        let store = GitStore::bootstrap_local(&root).unwrap();
        install_event_oracle(&store, &oracle.events);
        let index = ProjectionIndex::for_store(&store);
        let metadata = index.synchronize().unwrap().metadata;
        let domain = index.domain_snapshot().unwrap();
        assert!(
            domain.diagnostics.is_empty(),
            "fixed M3 event corpus must be internally valid: {:?}",
            domain.diagnostics
        );
        assert_eq!(domain.metadata, metadata);
        assert_eq!(
            domain
                .projection
                .engineering_references
                .keys()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>(),
            oracle.expected.references.values().cloned().collect(),
            "Reference IDs are hand-authored in the oracle, never copied from production output"
        );
        assert_eq!(
            domain
                .projection
                .spaces
                .values()
                .flat_map(|space| space.contexts.values())
                .flat_map(|context| context.revisions.keys())
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>(),
            oracle.expected.revisions.values().cloned().collect(),
            "Context Revision IDs are fixed oracle input rather than captured output"
        );

        let repository_id: RepositoryId = parse_id(&oracle.expected.repository_id);
        let repository = RepositoryIdentity {
            repository_id: repository_id.clone(),
            canonical_name: "milestone-three-multilingual-fixture".to_owned(),
        };
        let projected = domain
            .projection
            .engineering_references
            .values()
            .map(|reference| ProjectedEngineeringReference {
                context_id: reference.context_id,
                revision_id: reference.revision_id,
                reference: reference.reference.clone(),
            })
            .collect::<Vec<_>>();
        let scan_plan = RepositoryScanPlan::new(
            repository_id,
            projected
                .iter()
                .map(|reference| reference.reference.locator.path().clone())
                .collect(),
        )
        .unwrap();
        assert_eq!(
            scan_plan
                .paths()
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
            oracle.expected.planned_paths,
            "the fixed M3 scan plan is derived only from Reference locator paths"
        );
        let scan = RepositoryScanner::new(RepositoryScannerLimits::default())
            .scan(&repository, &repository_path, &scan_plan)
            .unwrap();
        let RepositoryScanOutcome::Available(snapshot) = scan else {
            panic!("fixture Repository must be available")
        };
        let context_snapshots =
            build_graph_context_snapshots(&domain.projection, &projected).unwrap();
        let projection = EngineeringReferenceResolver
            .resolve(
                &projected,
                &[RepositoryScanOutcome::Available(snapshot.clone())],
                &context_snapshots,
            )
            .unwrap();
        let graph_store = EngineeringProjectionStore::initialize(&root).unwrap();
        graph_store
            .rebuild_for_context_tree(&projection, Some(&metadata.indexed_tree_oid))
            .unwrap();
        Self {
            _temporary: temporary,
            store,
            root,
            repository_path,
            oracle,
            repository,
            scan_plan,
            index,
            metadata,
            snapshot,
            references: projected,
            graph_store,
            projection,
        }
    }

    fn reference(&self, name: &str) -> &sctx_engineering_graph::ResolvedReferenceProjection {
        let expected = parse_id::<ReferenceId>(&self.oracle.expected.references[name]);
        self.projection
            .references
            .iter()
            .find(|reference| reference.reference_id == expected)
            .unwrap_or_else(|| panic!("missing fixed Reference {name}"))
    }

    fn engine(&self) -> SearchEngine {
        SearchEngine::with_engineering_graph(self.index.clone(), self.graph_store.clone())
    }
}

fn task_request(
    focus: ResolvedFocus,
    mode: ContextPackMode,
    token_budget: usize,
    goal: &str,
) -> TaskContextRequest {
    let task_id: TaskId = parse_id("tsk_00000000-0000-4000-8000-000000003801");
    TaskContextRequest {
        task_id,
        working_intent: WorkingIntentSnapshot {
            goal: goal.to_owned(),
            current_direction: Some(format!("apply a verified change for {goal}")),
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            domains: Vec::new(),
            platforms: Vec::new(),
            constraints: Vec::new(),
            acceptance_conditions: Vec::new(),
            artifact_hints: Vec::new(),
            interface_hints: Vec::new(),
            open_questions: Vec::new(),
        },
        task_signals: Vec::new(),
        resolved_focus: Some(focus),
        token_budget,
        max_spaces: 8,
        candidate_limit: 100,
        mode,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_multilanguage_oracle_marks_moves_and_renames_missing_and_rebuilds() {
    let mut fixture = MilestoneThreeFixture::new();
    assert_multilanguage_snapshot(&fixture);

    let initial_symbol_key = fixture
        .reference("symbol")
        .resolution
        .resolved_artifact
        .clone()
        .expect("fixed Symbol Reference resolves");
    assert_eq!(
        fixture.reference("api").resolution.status,
        ResolutionStatus::Resolved
    );
    assert_eq!(
        fixture.reference("schema").resolution.status,
        ResolutionStatus::Resolved
    );
    assert_eq!(
        fixture.reference("ambiguous").resolution.status,
        ResolutionStatus::Ambiguous
    );

    let moved = &fixture.oracle.expected.artifacts["file_move"];
    git(
        &fixture.repository_path,
        &[
            "mv",
            moved.old_path.as_deref().unwrap(),
            moved.new_path.as_deref().unwrap(),
        ],
    );
    let symbol = &fixture.oracle.expected.artifacts["symbol"];
    let symbol_path = fixture
        .repository_path
        .join(symbol.path.as_deref().unwrap());
    let source = fs::read_to_string(&symbol_path).unwrap();
    fs::write(
        &symbol_path,
        source.replace(
            symbol.old_name.as_deref().unwrap(),
            symbol.new_name.as_deref().unwrap(),
        ),
    )
    .unwrap();
    git(&fixture.repository_path, &["add", "--", "."]);
    git(
        &fixture.repository_path,
        &["commit", "-q", "-m", "move file and rename symbol"],
    );

    let rescanned = RepositoryScanner::new(RepositoryScannerLimits::default())
        .scan_incremental(
            &fixture.snapshot,
            &fixture.repository,
            &fixture.repository_path,
            &fixture.scan_plan,
        )
        .unwrap();
    let RepositoryScanOutcome::Available(current_snapshot) = rescanned else {
        panic!("moved fixture Repository must remain available")
    };
    let current = EngineeringReferenceResolver
        .resolve_incremental(
            &fixture.projection,
            &fixture.references,
            &[RepositoryScanOutcome::Available(current_snapshot.clone())],
            &fixture.projection.contexts,
        )
        .unwrap();
    let current_file = resolved(&current, &fixture.oracle.expected.references["file"]);
    assert_eq!(current_file.resolution.status, ResolutionStatus::Missing);
    assert!(current_file.resolution.resolved_artifact.is_none());
    assert!(current_file.association.is_none());
    assert!(current_file.artifacts.is_empty());
    assert!(!current_snapshot.artifacts.iter().any(|artifact| {
        artifact.artifact.artifact_key.kind() == ArtifactKind::File
            && artifact.artifact.artifact_key.locator().path().as_str()
                == moved.new_path.as_deref().unwrap()
    }));

    let current_symbol = resolved(&current, &fixture.oracle.expected.references["symbol"]);
    assert_eq!(current_symbol.resolution.status, ResolutionStatus::Missing);
    assert!(current_symbol.resolution.resolved_artifact.is_none());
    assert!(current_symbol.association.is_none());
    assert!(
        fixture
            .references
            .iter()
            .find(|reference| reference.reference.reference_id == current_symbol.reference_id)
            .unwrap()
            .reference
            .locator
            .canonical_key()
            .contains(symbol.old_name.as_deref().unwrap()),
        "the persistent Reference remains the original observation"
    );
    assert!(current_symbol.evidence.is_empty());
    assert!(current_snapshot.artifacts.iter().any(|artifact| {
        artifact.artifact.artifact_key.kind() == ArtifactKind::Symbol
            && artifact.artifact.display_name == symbol.new_name.as_deref().unwrap()
            && artifact.artifact.artifact_key != initial_symbol_key
    }));

    fixture
        .graph_store
        .rebuild_for_context_tree(&current, Some(&fixture.metadata.indexed_tree_oid))
        .unwrap();
    let canonical_before = fixture.graph_store.canonical_bytes().unwrap().unwrap();
    let database = fixture.graph_store.database_path().to_path_buf();
    remove_if_present(&database);
    remove_if_present(&PathBuf::from(format!("{}-wal", database.display())));
    remove_if_present(&PathBuf::from(format!("{}-shm", database.display())));
    let rebuilt_store = EngineeringProjectionStore::initialize(&fixture.root).unwrap();
    rebuilt_store
        .rebuild_for_context_tree(&current, Some(&fixture.metadata.indexed_tree_oid))
        .unwrap();
    assert_eq!(
        rebuilt_store.canonical_bytes().unwrap().unwrap(),
        canonical_before,
        "deleting the disposable SQLite Graph and rebuilding from the same facts is byte-equivalent"
    );

    fixture.graph_store = rebuilt_store;
    fixture.projection = current;
    fixture.snapshot = current_snapshot;
    let moved_file_pack = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator: sctx_domain::ArtifactLocator::File {
                    path: sctx_domain::RepoRelativePath::new(moved.new_path.as_deref().unwrap())
                        .unwrap(),
                },
            },
            ContextPackMode::AutomaticInjection,
            fixture.oracle.expected.token_budget,
            "zxq moved implementation",
        ))
        .unwrap();
    assert!(
        moved_file_pack
            .items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .all(|path| { !matches!(path, TaskRetrievalPath::EngineeringGraph { .. }) })
    );
    let renamed_locator = fixture
        .snapshot
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact.display_name == symbol.new_name.as_deref().unwrap())
        .unwrap()
        .artifact
        .artifact_key
        .locator()
        .clone();
    let renamed_symbol_pack = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator: renamed_locator,
            },
            ContextPackMode::AutomaticInjection,
            fixture.oracle.expected.token_budget,
            "zxr renamed implementation",
        ))
        .unwrap();
    assert!(
        renamed_symbol_pack
            .items
            .iter()
            .flat_map(|item| &item.retrieval_paths)
            .all(|path| { !matches!(path, TaskRetrievalPath::EngineeringGraph { .. }) })
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_graph_oracle_opens_requirement_decision_contract_and_cross_platform_validation() {
    let fixture = MilestoneThreeFixture::new();
    let symbol_signal = fixture
        .reference("symbol")
        .resolution
        .resolved_artifact
        .as_ref()
        .unwrap()
        .locator()
        .clone();
    let pack = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator: symbol_signal.clone(),
            },
            ContextPackMode::AutomaticInjection,
            fixture.oracle.expected.token_budget,
            "zxq inspect current implementation",
        ))
        .unwrap();

    assert_pack_generations_and_budget(&fixture, &pack);
    let expected_contexts = [
        "decision",
        "contract",
        "ios_validation",
        "android_validation",
    ]
    .into_iter()
    .map(|name| fixture.oracle.expected.contexts[name].clone())
    .collect::<BTreeSet<_>>();
    assert_eq!(context_ids(&pack), expected_contexts);
    assert!(pack.associations.iter().any(|association| {
        association.space_id == parse_id::<SpaceId>(&fixture.oracle.expected.spaces["requirement"])
    }));
    let domain = fixture.index.domain_snapshot().unwrap();
    assert_eq!(
        {
            let intent = &domain.projection.spaces
                [&parse_id::<SpaceId>(&fixture.oracle.expected.spaces["requirement"])]
                .intent;
            let head = intent.heads.iter().next().unwrap();
            &intent.revisions[head].intent.title
        },
        "Search Results Requirement",
        "the direct Symbol association lands in the hand-authored Requirement Intent"
    );
    assert_direct_graph_reference(
        &pack,
        &fixture.oracle.expected.contexts["decision"],
        &fixture.oracle.expected.references["symbol"],
    );
    for relation in &fixture.oracle.expected.relations_from_symbol {
        assert_relation_path(&pack, relation);
    }
    assert_cycle_safe_and_bounded(&pack);

    for reference_name in ["api", "schema"] {
        let exact_signal = fixture
            .reference(reference_name)
            .resolution
            .resolved_artifact
            .as_ref()
            .unwrap()
            .locator()
            .clone();
        let cross_end = fixture
            .engine()
            .task_context_pack(&task_request(
                ResolvedFocus {
                    repository_id: fixture.repository.repository_id.clone(),
                    locator: exact_signal,
                },
                ContextPackMode::AutomaticInjection,
                fixture.oracle.expected.token_budget,
                "zxs opaque route",
            ))
            .unwrap();
        assert_pack_generations_and_budget(&fixture, &cross_end);
        assert_eq!(context_ids(&cross_end), expected_contexts);
        assert_direct_graph_reference(
            &cross_end,
            &fixture.oracle.expected.contexts["contract"],
            &fixture.oracle.expected.references[reference_name],
        );
        for target in ["decision", "ios_validation", "android_validation"] {
            let item = context_item(&cross_end, &fixture.oracle.expected.contexts[target]);
            assert!(item.retrieval_paths.iter().any(|path| matches!(
                path,
                TaskRetrievalPath::EngineeringGraph { relation_hops, .. }
                    if relation_hops.len() == 1
            )));
        }
        assert_cycle_safe_and_bounded(&cross_end);
    }

    let mut bounded_request = task_request(
        ResolvedFocus {
            repository_id: fixture.repository.repository_id.clone(),
            locator: symbol_signal,
        },
        ContextPackMode::AutomaticInjection,
        fixture.oracle.expected.bounded_token_budget,
        "zxt budget graph output",
    );
    bounded_request.max_spaces = 2;
    let bounded = fixture
        .engine()
        .task_context_pack(&bounded_request)
        .unwrap();
    assert_eq!(
        bounded.estimated_tokens,
        estimate_task_context_payload_tokens(&bounded)
    );
    assert!(bounded.estimated_tokens <= bounded.token_budget);
    assert!(bounded.associations.len() <= 2);
    assert!(!bounded.omitted.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn ambiguous_and_unavailable_edges_diagnose_or_fall_back_without_automatic_graph_injection() {
    let fixture = MilestoneThreeFixture::new();
    let ambiguous = fixture.reference("ambiguous");
    assert_eq!(ambiguous.resolution.status, ResolutionStatus::Ambiguous);
    assert_eq!(ambiguous.resolution.candidates.len(), 1);
    assert!(ambiguous.association.is_none());
    let ambiguous_locator = ambiguous.resolution.candidates[0].locator().clone();

    let explicit = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator: ambiguous_locator.clone(),
            },
            ContextPackMode::Explicit,
            fixture.oracle.expected.token_budget,
            "ambiguous Symbol association",
        ))
        .unwrap();
    let ambiguous_item = context_item(&explicit, &fixture.oracle.expected.contexts["ambiguous"]);
    assert!(ambiguous_item.retrieval_paths.iter().any(|path| matches!(
        path,
        TaskRetrievalPath::GraphDiagnostic { diagnostic }
            if diagnostic.reference_id
                == parse_id::<ReferenceId>(&fixture.oracle.expected.references["ambiguous"])
                && diagnostic.resolution_status == ResolutionStatus::Ambiguous
                && diagnostic.candidate_artifact_keys.len() == 1
    )));

    let automatic = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator: ambiguous_locator,
            },
            ContextPackMode::AutomaticInjection,
            fixture.oracle.expected.token_budget,
            "zxu no textual fallback",
        ))
        .unwrap();
    assert!(!context_ids(&automatic).contains(&fixture.oracle.expected.contexts["ambiguous"]));
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

    let domain = fixture.index.domain_snapshot().unwrap();
    let references = domain
        .projection
        .engineering_references
        .values()
        .map(|reference| ProjectedEngineeringReference {
            context_id: reference.context_id,
            revision_id: reference.revision_id,
            reference: reference.reference.clone(),
        })
        .collect::<Vec<_>>();
    let unavailable = EngineeringReferenceResolver
        .resolve(
            &references,
            &[RepositoryScanOutcome::Unavailable {
                repository_id: fixture.repository.repository_id.clone(),
                reason: "fixed oracle checkout unavailable".to_owned(),
            }],
            &fixture.projection.contexts,
        )
        .unwrap();
    assert!(unavailable.references.iter().all(|reference| {
        reference.resolution.status == ResolutionStatus::Unavailable
            && reference.association.is_none()
    }));
    fixture
        .graph_store
        .rebuild_for_context_tree(&unavailable, Some(&fixture.metadata.indexed_tree_oid))
        .unwrap();
    let fallback = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator: fixture
                    .projection
                    .references
                    .iter()
                    .find(|reference| {
                        reference.reference_id
                            == parse_id::<ReferenceId>(
                                &fixture.oracle.expected.references["symbol"],
                            )
                    })
                    .unwrap()
                    .resolution
                    .resolved_artifact
                    .as_ref()
                    .unwrap()
                    .locator()
                    .clone(),
            },
            ContextPackMode::AutomaticInjection,
            fixture.oracle.expected.token_budget,
            "frontend renders SearchEnvelopeClient",
        ))
        .unwrap();
    assert_eq!(
        fallback.artifact_generation.as_deref(),
        Some(unavailable.artifact_generation.as_str())
    );
    assert!(
        context_ids(&fallback).contains(&fixture.oracle.expected.contexts["decision"]),
        "Intent/Context BM25 fallback remains usable when the Repository is unavailable"
    );
    assert!(
        fallback
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
    assert_eq!(
        fallback.estimated_tokens,
        estimate_task_context_payload_tokens(&fallback)
    );
    assert!(fallback.estimated_tokens <= fallback.token_budget);
}

#[test]
#[allow(clippy::too_many_lines)]
fn adapter_injects_frozen_safe_revision_after_current_revision_is_withdrawn() {
    let fixture = MilestoneThreeFixture::new();
    let decision_space = parse_id::<SpaceId>(&fixture.oracle.expected.spaces["requirement"]);
    let decision_context = parse_id::<ContextId>(&fixture.oracle.expected.contexts["decision"]);
    let decision_revision = parse_id(&fixture.oracle.expected.revisions["decision"]);
    let before = fixture.index.domain_snapshot().unwrap();
    let context = &before.projection.spaces[&decision_space].contexts[&decision_context];
    let ContextGovernanceStatus::Accepted {
        publication_id: previous_publication,
        ..
    } = context.governance
    else {
        panic!("fixed decision must be accepted before Graph build")
    };
    let revision_event = Event::context_revised(
        decision_space,
        decision_context,
        vec![decision_revision],
        ContextRevisionDraft {
            kind: ContextKind::Decision,
            topic_key: Some("search/frontend-rendering".to_owned()),
            statement: "withdrawncurrentneedle replaces the historical Graph decision".to_owned(),
            rationale: "The current Store moves independently from an explicit Graph build"
                .to_owned(),
            applicability: Applicability {
                domains: vec!["search".to_owned()],
                platforms: vec!["fe".to_owned()],
                conditions: vec!["v2 response".to_owned()],
            },
            assumptions: vec!["the Graph is not rebuilt".to_owned()],
            recheck_when: vec!["an explicit association rebuild occurs".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "The replacement Revision was appended".to_owned(),
                content: serde_json::json!({"result": "appended"}),
                interpretation: "Current governance can advance independently".to_owned(),
                limitations: vec!["synthetic #147 fixture".to_owned()],
            }],
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextRevisionAdded { revision, .. } = revision_event.payload() else {
        unreachable!()
    };
    let current_revision = revision.revision_id;
    fixture
        .store
        .append_event(AppendRequest::event(revision_event))
        .unwrap();
    let publish_event = Event::publication_changed(
        decision_space,
        decision_context,
        PublicationDraft {
            previous_publication_ids: vec![previous_publication],
            action: PublicationAction::Publish,
            revision_id: current_revision,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextPublicationChanged { publication, .. } = publish_event.payload()
    else {
        unreachable!()
    };
    let current_publication = publication.publication_id;
    fixture
        .store
        .append_event(AppendRequest::event(publish_event))
        .unwrap();
    fixture
        .store
        .append_event(AppendRequest::event(
            Event::publication_changed(
                decision_space,
                decision_context,
                PublicationDraft {
                    previous_publication_ids: vec![current_publication],
                    action: PublicationAction::Withdraw,
                    revision_id: current_revision,
                    review_event_ids: Vec::new(),
                },
                None,
            )
            .unwrap(),
        ))
        .unwrap();
    fixture.index.synchronize().unwrap();
    let after = fixture.index.domain_snapshot().unwrap();
    assert!(matches!(
        after.projection.spaces[&decision_space].contexts[&decision_context].governance,
        ContextGovernanceStatus::Deprecated { revision_id, .. }
            if revision_id == current_revision
    ));

    let locator = fixture
        .reference("symbol")
        .resolution
        .resolved_artifact
        .as_ref()
        .unwrap()
        .locator()
        .clone();
    let pack = fixture
        .engine()
        .task_context_pack(&task_request(
            ResolvedFocus {
                repository_id: fixture.repository.repository_id.clone(),
                locator,
            },
            ContextPackMode::AutomaticInjection,
            fixture.oracle.expected.token_budget,
            "opaque historical graph injection",
        ))
        .unwrap();
    let item = pack
        .items
        .iter()
        .find(|item| item.context.context_id == decision_context)
        .unwrap();
    assert_eq!(item.context.revision_id, decision_revision);
    assert_eq!(item.context.status, sctx_search::ContextStatus::Accepted);
    assert!(item.context.auto_injection_eligible);
    assert!(matches!(
        item.context.safety_source,
        ContextSafetySource::EngineeringGraphSnapshot { revision_id, .. }
            if revision_id == decision_revision
    ));
    let rendered = sctx_agent_adapter::render_untrusted_task_context_pack(&pack).unwrap();
    assert!(rendered.contains("frontend renders SearchEnvelopeClient"));
    assert!(!rendered.contains("withdrawncurrentneedle"));
    let mut mismatched_provenance = pack.clone();
    mismatched_provenance.graph_context_tree_oid = Some("wrong-build-tree".to_owned());
    assert!(
        sctx_agent_adapter::render_untrusted_task_context_pack(&mismatched_provenance).is_err()
    );
}

fn assert_multilanguage_snapshot(fixture: &MilestoneThreeFixture) {
    let actual_languages = fixture
        .snapshot
        .artifacts
        .iter()
        .flat_map(|artifact| artifact.observations.iter())
        .map(|observation| language_name(observation.language).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_languages,
        fixture.oracle.expected.languages.iter().cloned().collect(),
        "only languages reached by the fixed Reference-derived plan are parsed"
    );
    for name in ["symbol", "api", "schema"] {
        let expected = &fixture.oracle.expected.artifacts[name];
        let kind = artifact_kind(&expected.kind);
        assert!(fixture.snapshot.artifacts.iter().any(|artifact| {
            artifact.artifact.artifact_key.kind() == kind
                && artifact.artifact.display_name
                    == expected
                        .name
                        .as_deref()
                        .or(expected.old_name.as_deref())
                        .unwrap()
                && Some(artifact.artifact.artifact_key.locator().path().as_str())
                    == expected.path.as_deref()
        }));
    }
    let planned_paths = fixture
        .oracle
        .expected
        .planned_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert!(fixture.snapshot.artifacts.iter().all(|artifact| {
        artifact
            .observations
            .iter()
            .all(|observation| planned_paths.contains(observation.path.as_str()))
    }));
}

fn assert_pack_generations_and_budget(fixture: &MilestoneThreeFixture, pack: &TaskContextPack) {
    assert_eq!(pack.indexed_tree_oid, fixture.metadata.indexed_tree_oid);
    assert_eq!(
        pack.projection_generation,
        fixture.metadata.projection_generation
    );
    assert_eq!(
        pack.artifact_generation.as_deref(),
        Some(fixture.projection.artifact_generation.as_str())
    );
    let snapshot = fixture.graph_store.read_snapshot().unwrap().unwrap();
    assert_eq!(
        snapshot.context_tree_oid.as_deref(),
        Some(pack.indexed_tree_oid.as_str())
    );
    assert_eq!(pack.graph_context_tree_oid, snapshot.context_tree_oid);
    assert_eq!(
        snapshot.projection.artifact_generation,
        pack.artifact_generation.as_deref().unwrap()
    );
    for path in pack.items.iter().flat_map(|item| &item.retrieval_paths) {
        if let TaskRetrievalPath::EngineeringGraph { path, .. } = path {
            assert_eq!(
                Some(path.artifact_generation.as_str()),
                pack.artifact_generation.as_deref()
            );
        }
    }
    assert_eq!(
        pack.estimated_tokens,
        estimate_task_context_payload_tokens(pack)
    );
    assert!(pack.estimated_tokens <= pack.token_budget);
}

fn assert_direct_graph_reference(pack: &TaskContextPack, context: &str, reference: &str) {
    let item = context_item(pack, context);
    assert!(
        item.retrieval_paths.iter().any(|path| matches!(
            path,
            TaskRetrievalPath::EngineeringGraph { path, relation_hops }
                if path.reference_id == parse_id::<ReferenceId>(reference)
                    && path.resolution_status == ResolutionStatus::Resolved
                    && relation_hops.is_empty()
        )),
        "missing direct Graph path for Context {context} and Reference {reference}"
    );
}

fn assert_relation_path(pack: &TaskContextPack, expected: &ExpectedRelation) {
    let target = context_item(pack, &expected.target);
    assert!(
        target.retrieval_paths.iter().any(|path| match path {
            TaskRetrievalPath::EngineeringGraph { relation_hops, .. }
            | TaskRetrievalPath::ContextRelation {
                hops: relation_hops,
            } => relation_hops.iter().any(|hop| {
                hop.source_context_id == parse_id::<ContextId>(&expected.source)
                    && hop.target_context_id == parse_id::<ContextId>(&expected.target)
                    && relation_kind_name(hop.kind) == expected.kind
                    && hop.depth == expected.depth
            }),
            _ => false,
        }),
        "missing fixed relation {} -> {} ({}, depth {})",
        expected.source,
        expected.target,
        expected.kind,
        expected.depth
    );
}

fn assert_cycle_safe_and_bounded(pack: &TaskContextPack) {
    let ids = context_ids(pack);
    assert_eq!(
        ids.len(),
        pack.items.len(),
        "cycles must not duplicate Context items"
    );
    for path in pack.items.iter().flat_map(|item| &item.retrieval_paths) {
        let hops = match path {
            TaskRetrievalPath::EngineeringGraph { relation_hops, .. } => relation_hops,
            TaskRetrievalPath::ContextRelation { hops } => hops,
            _ => continue,
        };
        assert!(hops.len() <= 2);
        assert!(hops.iter().all(|hop| hop.depth <= 2));
        let mut visited = BTreeSet::new();
        for hop in hops {
            assert!(visited.insert(hop.source_context_id));
        }
        if let Some(last) = hops.last() {
            assert!(!visited.contains(&last.target_context_id));
        }
    }
}

fn context_ids(pack: &TaskContextPack) -> BTreeSet<String> {
    pack.items
        .iter()
        .map(|item| item.context.context_id.to_string())
        .collect()
}

fn context_item<'a>(pack: &'a TaskContextPack, context: &str) -> &'a sctx_search::TaskContextItem {
    let context = parse_id::<ContextId>(context);
    pack.items
        .iter()
        .find(|item| item.context.context_id == context)
        .unwrap_or_else(|| panic!("missing fixed Context {context}"))
}

fn resolved<'a>(
    projection: &'a EngineeringProjection,
    reference: &str,
) -> &'a sctx_engineering_graph::ResolvedReferenceProjection {
    let reference = parse_id::<ReferenceId>(reference);
    projection
        .references
        .iter()
        .find(|resolved| resolved.reference_id == reference)
        .unwrap_or_else(|| panic!("missing fixed Reference {reference}"))
}

fn install_event_oracle(store: &GitStore, events: &[Value]) {
    for raw in events {
        let bytes = serde_json::to_vec_pretty(raw).unwrap();
        let ParsedEvent::Known(event) = parse_event(&bytes).unwrap() else {
            panic!("M3 oracle contains an unknown Event version")
        };
        let id = event.event_id().to_string();
        let path = store
            .repository()
            .join("events/00")
            .join(format!("{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    git(store.repository(), &["add", "--", "events"]);
    git(
        store.repository(),
        &["commit", "-q", "-m", "install fixed M3 event oracle"],
    );
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    let mut entries = fs::read_dir(source)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let source = entry.path();
        let target = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&source, &target);
        } else {
            fs::copy(source, target).unwrap();
        }
    }
}

fn init_git_repository(path: &Path) {
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "M3 Fixed Oracle"]);
    git(path, &["config", "user.email", "m3-oracle@example.invalid"]);
    git(path, &["add", "--", "."]);
    git(path, &["commit", "-q", "-m", "fixed multilingual fixture"]);
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn remove_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

fn parse_id<T>(value: &str) -> T
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .unwrap_or_else(|error| panic!("invalid fixed ID {value}: {error}"))
}

const fn language_name(language: SourceLanguage) -> &'static str {
    match language {
        SourceLanguage::Rust => "rust",
        SourceLanguage::TypeScriptJavaScript => "typescript-javascript",
        SourceLanguage::Swift => "swift",
        SourceLanguage::Kotlin => "kotlin",
        SourceLanguage::Json => "json",
        SourceLanguage::Yaml => "yaml",
        SourceLanguage::Proto => "proto",
    }
}

fn artifact_kind(kind: &str) -> ArtifactKind {
    match kind {
        "symbol" => ArtifactKind::Symbol,
        "api" => ArtifactKind::Api,
        "schema" => ArtifactKind::Schema,
        "file" => ArtifactKind::File,
        other => panic!("unsupported fixed Artifact kind {other}"),
    }
}

const fn relation_kind_name(kind: ContextRelationKind) -> &'static str {
    match kind {
        ContextRelationKind::DependsOn => "depends_on",
        ContextRelationKind::Constrains => "constrains",
        ContextRelationKind::Implements => "implements",
        ContextRelationKind::ValidatedBy => "validated_by",
        ContextRelationKind::Contradicts => "contradicts",
        ContextRelationKind::RelatedTo => "related_to",
    }
}
