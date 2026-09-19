//! What the M3 Engineering Graph still guarantees, now that the Pack no longer consults it.
//!
//! Three of this file's four tests were about the Graph as a retrieval channel into an automatic
//! Context Pack -- the exact Artifact reached from a Focus, the relation hops off it, the
//! `artifact_generation` the Pack reported, the ambiguous edge that had to stay a diagnostic, and
//! the frozen safety a Graph snapshot lent a revision whose current head had been withdrawn.
//! ADR-0007 removed that channel, and the ruling that followed made it deliberate: the Graph's job
//! is relocation bookkeeping.
//!
//! Two of those claims are worth naming because they did not merely move, they stopped being
//! expressible. An *ambiguous* Engineering Reference -- one whose locator several Artifacts answer
//! -- was refused by the Graph resolver and therefore never injected; Lane A joins on the
//! `(repository, path)` the Reference itself records, where there is nothing to be ambiguous about,
//! so the Context it names is reachable again. And `ContextSafetySource::EngineeringGraphSnapshot`,
//! the frozen revision a Pack could still inject after the current one was withdrawn, has no
//! producer left: every lane item is `CurrentProjection`, and a withdrawn Context is simply absent.
//!
//! What survives is the multilanguage scan oracle: moves and renames are marked missing, and a
//! rebuild reports them. That is resolution, and ADR-0007 does not touch it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

use sctx_domain::{
    ArtifactKind, ReferenceId, RepositoryId, RepositoryIdentity, ResolutionStatus, ResolvedFocus,
    TaskId, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    EngineeringProjection, EngineeringProjectionStore, EngineeringReferenceResolver,
    ProjectedEngineeringReference, RepositoryScanOutcome, RepositoryScanPlan, RepositoryScanner,
    RepositoryScannerLimits, RepositorySnapshot, SourceLanguage, build_graph_context_snapshots,
};
use sctx_event_schema::{ParsedEvent, parse_event};
use sctx_git_store::GitStore;
use sctx_index::{IndexMetadata, ProjectionIndex};
use sctx_search::{ContextPackMode, SearchEngine, TaskContextRequest, TaskRetrievalPath};
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
// The oracle file is the schema; this mirrors it whole so a field that stops being read here is
// still a field the fixture is required to carry.
#[allow(dead_code)]
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
#[allow(dead_code)]
struct ExpectedRelation {
    source: String,
    target: String,
    kind: String,
    depth: u8,
}

struct MilestoneThreeFixture {
    _temporary: TempDir,
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
        signal_history: Vec::new(),
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
        SourceLanguage::Java => "java",
        SourceLanguage::Json => "json",
        SourceLanguage::Yaml => "yaml",
        SourceLanguage::Proto => "proto",
        SourceLanguage::Xml => "xml",
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
