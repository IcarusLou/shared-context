use std::{
    fs,
    io::{BufReader, Cursor},
    path::Path,
    process::Command,
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use sctx_domain::{
    Applicability, ArtifactKind, ArtifactLocator, ContextId, ContextKind, ContextRevisionDraft,
    DecisionSource, EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, IntentSnapshot,
    PublicationAction, PublicationDraft, ReferenceRelation, RepoRelativePath, RepositoryId,
    ReviewDraft, ReviewVerdict, RevisionId, TaskId, TaskSignal, TaskSignalKind,
    WorkingIntentSnapshot,
};
use sctx_engineering_graph::EngineeringProjectionStore;
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_local_state::{AuthorizedSessionScopeStore, UserConfigStore};
use sctx_mcp::{
    ArtifactFocusQuery, ArtifactFocusQueryCoordinates, AssociationExplainInput,
    AssociationRebuildInput, ClientKind, DisconnectReason, EngineeringReferenceRecordInput,
    ExpectedRevisionId, McpServer, RepositoryScanInput, TaskBoundary, TaskContextReadInput,
    TaskIntentUpdateInput, association_explain_at_root, association_rebuild_at_root,
    engineering_reference_record_at_root, repository_scan_at_root, task_artifact_focus_at_root,
    task_context_readonly_at_root, task_intent_update_at_root,
};
use sctx_search::TaskRetrievalPath;
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};
use tempfile::TempDir;

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

#[allow(clippy::needless_pass_by_value)]
fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[allow(clippy::needless_pass_by_value)]
fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    rpc_request(
        id,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
}

fn run_mcp(server: &mut McpServer, requests: &[Value]) -> Vec<Value> {
    let mut input = requests
        .iter()
        .flat_map(|request| {
            let mut bytes = serde_json::to_vec(request).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let outcome = server
        .serve(&mut BufReader::new(Cursor::new(&mut input)), &mut output)
        .unwrap();
    assert_eq!(outcome.disconnect, DisconnectReason::CleanEof);
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

fn task_update_arguments(session: &str) -> Value {
    json!({
        "agent_kind": "codex",
        "external_session_id": session,
        "task_boundary": "new",
        "expected_revision_id": null,
        "intent": {
            "goal": "zxqvortex",
            "current_direction": "zyqnebula"
        }
    })
}

/// Authorizes one Session started at the directory every configured checkout lives under,
/// which is how a multi-Repository Session is derived now that Groups are gone.
fn authorize_group_session(root: &Path, session: &str) {
    let catalog = UserConfigStore::open_existing(root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    let mut checkouts = catalog
        .repositories
        .iter()
        .flat_map(|repository| repository.checkout_paths.iter());
    let mut parent = checkouts.next().expect("one configured checkout").clone();
    for checkout in checkouts {
        while !checkout.starts_with(&parent) {
            parent = parent.parent().expect("a shared directory").to_path_buf();
        }
    }
    let parent = parent
        .parent()
        .expect("not the filesystem root")
        .to_path_buf();
    AuthorizedSessionScopeStore::initialize(root)
        .unwrap()
        .try_authorize_missing(
            &ExternalSessionLocator::new("codex", session).unwrap(),
            &catalog,
            &parent,
        )
        .unwrap();
}

fn focus_coordinates(locator: &ArtifactLocator) -> Value {
    match locator {
        ArtifactLocator::File { .. } => json!({"locator_kind": "file"}),
        ArtifactLocator::Module { .. } => json!({"locator_kind": "module"}),
        ArtifactLocator::Api {
            protocol,
            operation,
            normalized_route,
            ..
        } => json!({
            "locator_kind": "api",
            "protocol": protocol,
            "operation": operation,
            "normalized_route": normalized_route
        }),
        ArtifactLocator::Schema {
            namespace,
            version,
            qualified_name,
            ..
        } => json!({
            "locator_kind": "schema",
            "namespace": namespace,
            "version": version,
            "qualified_name": qualified_name
        }),
        ArtifactLocator::Symbol {
            language,
            module,
            enclosing_type,
            symbol_name,
            signature,
            ..
        } => json!({
            "locator_kind": "symbol",
            "language": language,
            "module": module,
            "enclosing_type": enclosing_type,
            "symbol_name": symbol_name,
            "signature": signature
        }),
        ArtifactLocator::Test {
            qualified_test_name,
            ..
        } => json!({
            "locator_kind": "test",
            "qualified_test_name": qualified_test_name
        }),
    }
}

fn init_repo(path: &Path, files: &[(&str, &str)]) {
    fs::create_dir_all(path).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["config", "user.name", "Graph Workflow"]);
    git(path, &["config", "user.email", "graph@example.invalid"]);
    for (relative, content) in files {
        let target = path.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, content).unwrap();
    }
    git(path, &["add", "--", "."]);
    git(path, &["commit", "-q", "-m", "fixture"]);
}

fn seed_six_kinds(path: &Path) {
    init_repo(
        path,
        &[
            (
                "rust/src/lib.rs",
                r"
pub struct RustResult { value: String }
#[test]
fn rust_contract_test() { assert!(true); }
",
            ),
            (
                "web/src/search.ts",
                r#"
export interface SearchResponse { value: string }
export function webSearch() { return fetch("/api/search"); }
"#,
            ),
            (
                "schema/openapi.json",
                r#"{"openapi":"3.0.0","paths":{"/api/search":{}},"components":{"schemas":{"SearchResponse":{}}}}"#,
            ),
        ],
    );
}

fn context_draft(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
        problem_view: None,
        hints: Vec::new(),
        kind: ContextKind::Decision,
        topic_key: Some(format!("graph/{statement}")),
        statement: statement.to_owned(),
        rationale: "The verified implementation artifact carries this behavior".to_owned(),
        applicability: Applicability::default(),
        assumptions: Vec::new(),
        recheck_when: vec!["The implementation artifact changes".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "The graph workflow fixture verified the Context".to_owned(),
            content: serde_json::json!({"test": "engineering_workflow", "result": "passed"}),
            interpretation: "The Context is safe for automatic retrieval".to_owned(),
            limitations: vec!["Synthetic repository fixture".to_owned()],
        }],
    }
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append fixture Event");
}

fn accepted_context(root: &Path, statement: &str) -> (ContextId, RevisionId) {
    let store = GitStore::bootstrap_local(root).unwrap();
    let space = Event::space_created(
        IntentSnapshot {
            title: statement.to_owned(),
            problem: "Agents need verified engineering context".to_owned(),
            desired_outcome: "Retrieve through a current Artifact association".to_owned(),
            in_scope: vec!["Engineering Graph".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["The Context is graph-retrievable".to_owned()],
            domain_terms: vec![statement.to_owned()],
        },
        None,
    )
    .unwrap();
    let space_id = match space.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(&store, space);
    let revision = Event::context_revision_added(space_id, context_draft(statement), None).unwrap();
    let (context_id, revision_id) = match revision.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    };
    append(&store, revision);
    let review = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "Verified fixture evidence".to_owned(),
        },
        None,
    )
    .unwrap();
    let review_event_id = review.event_id();
    append(&store, review);
    append(
        &store,
        Event::publication_changed(
            space_id,
            context_id,
            PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: PublicationAction::Publish,
                revision_id,
                review_event_ids: vec![review_event_id],
            },
            None,
        )
        .unwrap(),
    );
    (context_id, revision_id)
}

/// A Direct-scoped Checkpoint acknowledges without deriving, and Candidate Build places the
/// spellings once (WP-P). The ACK is receipt plus outbox; nothing in it reads a checkout.
#[test]
#[allow(clippy::too_many_lines)]
fn checkpoint_ack_derives_nothing_and_candidate_build_places_the_spellings_once() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("build derivation root");
    let checkout = temporary.path().join("derivation checkout");
    init_repo(
        &checkout,
        &[("app/src/anchor/ProductAnchorAssem.kt", "// fixture\n")],
    );
    let checkout = fs::canonicalize(&checkout).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    let repository = UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            "Derivation".parse().unwrap(),
            std::slice::from_ref(&checkout),
        )
        .unwrap()
        .repository;
    let session = "build-derivation";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(&locator, &catalog, &checkout)
        .unwrap();
    task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("place the spellings at build time").unwrap(),
        },
    )
    .unwrap();
    let accepted = sctx_mcp::task_checkpoint_at_root(
        &root,
        &sctx_mcp::TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![sctx_mcp::TaskCheckpointClaimInput {
                context_kind: ContextKind::Issue,
                statement: "ProductAnchorAssem.kt:202 returns early and skips navigation"
                    .to_owned(),
                rationale: "The early return observably diverges from the baseline".to_owned(),
                conditions: vec!["live entry service is absent".to_owned()],
                evidence: vec![sctx_mcp::TaskCheckpointEvidenceInput {
                    evidence_type: EvidenceType::SourceSnapshot,
                    summary: "ProductAnchorAssem.kt:202 returns before dispatch".to_owned(),
                    limitations: Vec::new(),
                }],
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("a nonempty Checkpoint is accepted");
    let runtime = TaskRuntime::initialize(&root).unwrap();
    let episode = runtime
        .read_work_episode(accepted.episode_id)
        .unwrap()
        .unwrap();
    assert!(
        episode.checkpoints[0].claims[0]
            .engineering_references
            .is_empty(),
        "the durable ACK never consults a checkout"
    );

    // The Build drains the outbox without the authoring session's scope in hand and still resolves
    // the checkout, because the scope is looked up by the Episode's own locator.
    sctx_mcp::build_closed_episode_at_root(&root, accepted.episode_id).unwrap();
    let derived = runtime
        .read_work_episode(accepted.episode_id)
        .unwrap()
        .unwrap();
    let references = &derived.checkpoints[0].claims[0].engineering_references;
    assert_eq!(references.len(), 1, "{references:#?}");
    assert_eq!(
        references[0].locator.path().as_str(),
        "app/src/anchor/ProductAnchorAssem.kt"
    );
    assert_eq!(references[0].repository_id, repository.repository_id);
    assert_eq!(
        derived.checkpoints[0].claims[0].topic_key_hint.as_deref(),
        Some("issue:Derivation:app/src/anchor/ProductAnchorAssem.kt")
    );
    assert!(
        runtime
            .pending_claim_reference_candidates(accepted.episode_id)
            .unwrap()
            .is_none(),
        "a placed Episode never asks Git again"
    );
    sctx_mcp::build_closed_episode_at_root(&root, accepted.episode_id).unwrap();
    assert_eq!(
        &runtime
            .read_work_episode(accepted.episode_id)
            .unwrap()
            .unwrap()
            .checkpoints[0]
            .claims[0]
            .engineering_references,
        references,
        "a Build rerun reports the first derivation"
    );
}

/// A Session started at the common parent of two checkouts derives references for both, and
/// picks the checkout by where the spelling actually is (WP-N2).
#[test]
#[allow(clippy::too_many_lines)]
fn a_common_parent_session_places_each_spelling_in_the_checkout_that_tracks_it() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("parent derivation root");
    let parent = temporary.path().join("parent workspace");
    let android = parent.join("android");
    let ios = parent.join("ios");
    init_repo(
        &android,
        &[
            ("app/src/anchor/ProductAnchorAssem.kt", "// fixture\n"),
            ("app/src/shared/Ambiguous.kt", "// fixture\n"),
        ],
    );
    init_repo(
        &ios,
        &[
            ("Sources/Anchor/ProductAnchorView.swift", "// fixture\n"),
            ("Sources/Shared/Ambiguous.kt", "// fixture\n"),
        ],
    );
    let parent = fs::canonicalize(&parent).unwrap();
    let android = fs::canonicalize(&android).unwrap();
    let ios = fs::canonicalize(&ios).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    let config = UserConfigStore::initialize(&root).unwrap();
    let android_id = config
        .add_repository("Android".parse().unwrap(), std::slice::from_ref(&android))
        .unwrap()
        .repository
        .repository_id;
    let ios_id = config
        .add_repository("iOS".parse().unwrap(), std::slice::from_ref(&ios))
        .unwrap()
        .repository
        .repository_id;

    let session = "parent-derivation";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = config.repository_catalog_wait().unwrap();
    let scope = AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(&locator, &catalog, &parent)
        .unwrap()
        .scope;
    assert_eq!(
        scope.decision.repository_ids().len(),
        2,
        "starting at the common parent records for both Repositories"
    );

    task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("compare the anchor across both platforms").unwrap(),
        },
    )
    .unwrap();
    let accepted = sctx_mcp::task_checkpoint_at_root(
        &root,
        &sctx_mcp::TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![sctx_mcp::TaskCheckpointClaimInput {
                context_kind: ContextKind::Issue,
                statement: "ProductAnchorAssem.kt:202 and ProductAnchorView.swift:88 both return \
                            early, and Ambiguous.kt hides it"
                    .to_owned(),
                rationale: "Both platforms skip navigation on the same condition".to_owned(),
                conditions: vec!["live entry service is absent".to_owned()],
                evidence: vec![sctx_mcp::TaskCheckpointEvidenceInput {
                    evidence_type: EvidenceType::SourceSnapshot,
                    summary: "ProductAnchorAssem.kt:202 returns before dispatch".to_owned(),
                    limitations: Vec::new(),
                }],
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("a nonempty Checkpoint is accepted");

    sctx_mcp::build_closed_episode_at_root(&root, accepted.episode_id).unwrap();
    let runtime = TaskRuntime::initialize(&root).unwrap();
    let derived = runtime
        .read_work_episode(accepted.episode_id)
        .unwrap()
        .unwrap();
    let references = &derived.checkpoints[0].claims[0].engineering_references;
    let placed = references
        .iter()
        .map(|reference| {
            (
                reference.repository_id.clone(),
                reference.locator.path().as_str().to_owned(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        placed,
        std::collections::BTreeSet::from([
            (
                android_id.clone(),
                "app/src/anchor/ProductAnchorAssem.kt".to_owned()
            ),
            (
                ios_id.clone(),
                "Sources/Anchor/ProductAnchorView.swift".to_owned()
            ),
        ]),
        "each spelling is placed in the one checkout that tracks it: {references:#?}"
    );
    assert!(
        !placed
            .iter()
            .any(|(_, path)| path.ends_with("Ambiguous.kt")),
        "a basename both checkouts track stays an unresolved hint"
    );
}

#[test]
fn public_artifact_focus_uses_strict_text_only_while_graph_is_unavailable() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("public focus fallback root");
    GitStore::bootstrap_local(&root).unwrap();
    let checkout = temporary.path().join("public focus fallback checkout");
    init_repo(
        &checkout,
        &[(
            "src/contracts/fallback.rs",
            "pub fn fallback_contract() {}\n",
        )],
    );
    let checkout = fs::canonicalize(checkout).unwrap();
    let config = UserConfigStore::open_existing(&root).unwrap();
    let repository_id = config
        .add_repository(RepositoryId::new(), std::slice::from_ref(&checkout))
        .unwrap()
        .repository
        .repository_id;
    let (expected_context, expected_revision) = accepted_context(
        &root,
        &format!("{repository_id} src/contracts/fallback.rs owns the strict fallback contract"),
    );
    // The Reference is what makes the Focus reach anything at all now. Before ADR-0007 this test
    // needed none: with no Graph, a Focus fell back to matching its *path spelling* as text against
    // the Context corpus, which is how a Context came back for containing the string
    // `src/contracts/fallback.rs` in its statement. That fallback is gone with every other text
    // route into automatic injection, and what replaces it is the honest version of the same idea
    // -- the Context says which file it is about, in a Reference, and the Focus names that file.
    engineering_reference_record_at_root(
        &root,
        &reference_input(
            expected_context,
            expected_revision,
            &repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            ArtifactLocator::File {
                path: RepoRelativePath::new("src/contracts/fallback.rs").unwrap(),
            },
        ),
    )
    .unwrap();
    let session = "public-focus-text-fallback";
    let catalog = config.repository_catalog().unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(
            &ExternalSessionLocator::new("codex", session).unwrap(),
            &catalog,
            &checkout,
        )
        .unwrap();
    let task = task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("zzzzabsentpublicfallbackgoal").unwrap(),
        },
    )
    .unwrap();
    let focused = task_artifact_focus_at_root(
        &root,
        &ArtifactFocusQuery {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_revision_id: task.context.intent_revision_id.to_string(),
            absolute_file_path: checkout
                .join("src/contracts/fallback.rs")
                .to_string_lossy()
                .into_owned(),
            locator: ArtifactFocusQueryCoordinates::File,
            token_budget: 8_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    assert_eq!(focused.resolved_focus.repository_id, repository_id);
    assert!(focused.context.artifact_generation.is_none());
    // A Focus works with no Engineering Graph at all, and this is a stronger statement than the
    // text fallback it replaces: the route does not degrade when the Graph is missing, because the
    // route never used the Graph. The Working Intent here is deliberate nonsense
    // (`zzzzabsentpublicfallbackgoal`), so nothing but the anchor can be responsible.
    assert!(
        focused.context.items.iter().any(|item| {
            item.context.context_id == expected_context
                && matches!(
                    item.retrieval_paths.as_slice(),
                    [TaskRetrievalPath::FileAnchor { location, .. }]
                        if location == &format!("{repository_id}:src/contracts/fallback.rs")
                )
        }),
        "{:#?}",
        focused.context.items
    );
    assert!(focused.context.candidate_spaces.iter().all(|association| {
        association.matched_artifacts.is_empty() && association.relation_paths.is_empty()
    }));
}

fn scan_input(path: &Path, paths: &[&str]) -> RepositoryScanInput {
    RepositoryScanInput {
        checkout_path: path.to_string_lossy().into_owned(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        max_artifacts: 500,
    }
}

fn event_count(root: &Path) -> usize {
    git(
        &root.join("repository"),
        &["ls-tree", "-r", "--name-only", "HEAD"],
    )
    .lines()
    .filter(|path| path.starts_with("events/"))
    .count()
}

fn reference_input(
    context_id: ContextId,
    revision_id: RevisionId,
    repository_id: &sctx_domain::RepositoryId,
    kind: ArtifactKind,
    relation: ReferenceRelation,
    locator: ArtifactLocator,
) -> EngineeringReferenceRecordInput {
    EngineeringReferenceRecordInput {
        context_id: context_id.to_string(),
        revision_id: revision_id.to_string(),
        repository_id: repository_id.to_string(),
        artifact_kind: kind,
        relation,
        locator,
        supports: "Direct source inspection verified this relationship".to_owned(),
        limitations: vec!["Verified only against the current tracked snapshot".to_owned()],
    }
}

fn reference_relation(kind: ArtifactKind) -> ReferenceRelation {
    match kind {
        ArtifactKind::File | ArtifactKind::Module | ArtifactKind::Symbol => {
            ReferenceRelation::Implements
        }
        ArtifactKind::Api | ArtifactKind::Schema => ReferenceRelation::Defines,
        ArtifactKind::Test => ReferenceRelation::Validates,
    }
}

fn task_intent(_task_id: TaskId, goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: Some(format!("Use the verified {goal} implementation")),
        in_scope: vec![goal.to_owned()],
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec!["Graph Context is retrieved".to_owned()],
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn scan_record_rebuild_explain_and_task_pack_cross_two_repositories_and_a_worktree() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("shared context root");
    let first = temporary.path().join("前端 repo");
    let worktree = temporary.path().join("前端 worktree");
    let second = temporary.path().join("服务端 repo");
    init_repo(
        &first,
        &[(
            "src/alpha.rs",
            "pub fn alpha_graph_entry() -> bool { true }\n",
        )],
    );
    git(
        &first,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    init_repo(
        &second,
        &[(
            "src/beta.rs",
            "pub fn beta_graph_entry() -> bool { true }\n",
        )],
    );
    let first = fs::canonicalize(first).unwrap();
    let worktree = fs::canonicalize(worktree).unwrap();
    let second = fs::canonicalize(second).unwrap();
    let (context_id, revision_id) = accepted_context(&root, "alpha graph decision");
    let config = UserConfigStore::initialize(&root).unwrap();
    config
        .add_repository(
            sctx_domain::RepositoryId::new(),
            &[first.clone(), worktree.clone()],
        )
        .unwrap();
    config
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&second),
        )
        .unwrap();

    let first_scan = repository_scan_at_root(
        &root,
        &scan_input(&first, &["src/alpha.rs", "src/alpha.rs"]),
    )
    .unwrap();
    let worktree_scan =
        repository_scan_at_root(&root, &scan_input(&worktree, &["src/alpha.rs"])).unwrap();
    let second_scan =
        repository_scan_at_root(&root, &scan_input(&second, &["src/beta.rs"])).unwrap();
    assert_eq!(first_scan.repository_id, worktree_scan.repository_id);
    assert_ne!(first_scan.repository_id, second_scan.repository_id);
    assert!(first_scan.repository_generation.is_some());
    assert_eq!(first_scan.planned_path_count, 1);
    assert_eq!(
        first_scan.repository_generation,
        first_scan.artifact_generation
    );
    assert!(first_scan.artifact_count > 0);
    assert!(first_scan.artifacts.iter().all(|artifact| {
        artifact.display_name != "pub fn alpha_graph_entry() -> bool { true }"
    }));
    assert!(
        !serde_json::to_string(&first_scan)
            .unwrap()
            .contains("pub fn alpha_graph_entry() -> bool { true }")
    );

    let recorded = engineering_reference_record_at_root(
        &root,
        &reference_input(
            context_id,
            revision_id,
            &first_scan.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            ArtifactLocator::File {
                path: RepoRelativePath::new("src/alpha.rs").unwrap(),
            },
        ),
    )
    .unwrap();
    let rebuilt = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert!(rebuilt.stored);
    assert_eq!(rebuilt.repositories.len(), 1);
    assert_eq!(rebuilt.repositories[0].planned_path_count, 1);
    assert_eq!(rebuilt.reference_count, 1);
    assert_eq!(rebuilt.status_counts.resolved, 1);

    let explained = association_explain_at_root(
        &root,
        &AssociationExplainInput {
            reference_id: recorded.reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(explained.status, sctx_domain::ResolutionStatus::Resolved);
    assert!(explained.resolved_artifact.is_some());
    assert!(explained.ambiguity_candidates.len() <= 1);
    assert!(!explained.evidence.is_empty());
    assert!(explained.graph_paths.iter().all(|path| {
        path.iter()
            .any(|node| node == &format!("reference:{}", recorded.reference_id))
    }));

    fs::remove_dir_all(&worktree).unwrap();
    fs::remove_dir_all(&first).unwrap();
    fs::remove_dir_all(&second).unwrap();

    let authoritative = task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "graph-session".to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot {
                goal: "Use alpha graph context".to_owned(),
                current_direction: Some("Retrieve the verified alpha implementation".to_owned()),
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
        },
    )
    .unwrap();
    // An automatic Pack no longer carries the Graph's generation, because it no longer reads the
    // Graph. What this test is really about -- scan, record, rebuild, explain, and a Focus that
    // works across two Repositories and a worktree -- is unchanged, and every one of those steps is
    // still asserted below.
    assert!(authoritative.context.artifact_generation.is_none());
    let focused = task_artifact_focus_at_root(
        &root,
        &ArtifactFocusQuery {
            agent_kind: "codex".to_owned(),
            external_session_id: "graph-session".to_owned(),
            expected_revision_id: authoritative.context.intent_revision_id.to_string(),
            absolute_file_path: first.join("src/alpha.rs").to_string_lossy().into_owned(),
            locator: ArtifactFocusQueryCoordinates::File,
            token_budget: 4_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    assert_eq!(
        focused.resolved_focus.repository_id,
        first_scan.repository_id
    );
    let pack = focused.context;
    assert!(pack.artifact_generation.is_none());
    assert!(
        pack.items.iter().any(|item| {
            item.context.context_id == context_id
                && item
                    .retrieval_paths
                    .iter()
                    .any(|path| matches!(path, TaskRetrievalPath::FileAnchor { .. }))
        }),
        "the Focus reaches its Context across the worktree, by the file it names: {:#?}",
        pack.items
    );
    assert!(
        rebuilt
            .repositories
            .iter()
            .all(|repository| repository.status == "available"),
        "Task retrieval consumes the already-built Graph without rescanning now-missing Repositories"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_mcp_artifact_focus_is_query_scoped_across_six_kinds_and_hot_path() {
    #[derive(Clone)]
    struct FocusCase {
        repository_path: std::path::PathBuf,
        repository_id: sctx_domain::RepositoryId,
        context_id: ContextId,
        locator: ArtifactLocator,
    }

    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("focus MCP root");
    GitStore::bootstrap_local(&root).unwrap();
    let cross = temporary.path().join("workspace cross");
    let main_repository = cross.join("main/multilanguage");
    seed_six_kinds(&main_repository);
    let cross_repositories =
        ["fe/search", "android/search", "ios/search"].map(|relative| cross.join(relative));
    for repository in &cross_repositories {
        init_repo(
            repository,
            &[(
                "src/shared.ts",
                "export function sharedSearch() { return true; }\n",
            )],
        );
    }
    let main_repository = fs::canonicalize(main_repository).unwrap();
    let cross_repositories =
        cross_repositories.map(|repository| fs::canonicalize(repository).unwrap());
    let config = UserConfigStore::initialize(&root).unwrap();
    let main_id = config
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&main_repository),
        )
        .unwrap()
        .repository
        .repository_id;
    let cross_ids = cross_repositories.each_ref().map(|repository| {
        config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(repository),
            )
            .unwrap()
            .repository
            .repository_id
    });
    let main_scan = repository_scan_at_root(
        &root,
        &scan_input(
            &main_repository,
            &[
                "rust/src/lib.rs",
                "web/src/search.ts",
                "schema/openapi.json",
            ],
        ),
    )
    .unwrap();
    let mut cases = Vec::new();
    for kind in [
        ArtifactKind::File,
        ArtifactKind::Module,
        ArtifactKind::Symbol,
        ArtifactKind::Api,
        ArtifactKind::Schema,
        ArtifactKind::Test,
    ] {
        let artifact = main_scan
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == kind)
            .unwrap_or_else(|| panic!("missing six-kind Artifact {kind:?}"));
        let (context_id, revision_id) =
            accepted_context(&root, &format!("public Focus {kind:?} Context"));
        engineering_reference_record_at_root(
            &root,
            &reference_input(
                context_id,
                revision_id,
                &main_id,
                kind,
                reference_relation(kind),
                artifact.locator.clone(),
            ),
        )
        .unwrap();
        cases.push(FocusCase {
            repository_path: main_repository.clone(),
            repository_id: main_id.clone(),
            context_id,
            locator: artifact.locator.clone(),
        });
    }
    for (index, (repository, repository_id)) in cross_repositories.iter().zip(cross_ids).enumerate()
    {
        let scan =
            repository_scan_at_root(&root, &scan_input(repository, &["src/shared.ts"])).unwrap();
        let artifact = scan
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.kind == ArtifactKind::Symbol && artifact.display_name == "sharedSearch"
            })
            .unwrap();
        let (context_id, revision_id) =
            accepted_context(&root, &format!("cross Repository Focus {index}"));
        engineering_reference_record_at_root(
            &root,
            &reference_input(
                context_id,
                revision_id,
                &repository_id,
                ArtifactKind::Symbol,
                ReferenceRelation::Implements,
                artifact.locator.clone(),
            ),
        )
        .unwrap();
        cases.push(FocusCase {
            repository_path: repository.clone(),
            repository_id,
            context_id,
            locator: artifact.locator.clone(),
        });
    }
    let rebuilt = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(rebuilt.status_counts.resolved, cases.len());
    let graph_store =
        sctx_engineering_graph::EngineeringProjectionStore::initialize(&root).unwrap();
    let graph_before = graph_store.canonical_bytes().unwrap().unwrap();
    let store = GitStore::bootstrap_local(&root).unwrap();
    let knowledge_head_before = git(store.repository(), &["rev-parse", "HEAD"]);

    fs::remove_dir_all(&main_repository).unwrap();
    for repository in &cross_repositories {
        fs::remove_dir_all(repository).unwrap();
    }
    let mut server = McpServer::new(&root, ClientKind::Codex).unwrap();
    let initialized = run_mcp(
        &mut server,
        &[rpc_request(
            1,
            "initialize",
            json!({"protocolVersion": "2024-11-05"}),
        )],
    );
    assert_eq!(initialized[0]["result"]["protocolVersion"], "2024-11-05");

    let mut first_session = None;
    for (index, case) in cases.iter().enumerate() {
        let session = TaskId::new().to_string();
        authorize_group_session(&root, &session);
        let created_task = run_mcp(
            &mut server,
            &[tool_call(
                10 + u64::try_from(index).unwrap() * 10,
                "task_intent_update",
                task_update_arguments(&session),
            )],
        );
        assert_eq!(
            created_task[0]["result"]["isError"], false,
            "task_intent_update failed: {}",
            created_task[0]
        );
        let task = &created_task[0]["result"]["structuredContent"];
        let expected_revision_id = task["intent_revision_id"].as_str().unwrap();
        let task_id = task["task_id"].as_str().unwrap();
        let task_session_id = task["task_session_id"].as_str().unwrap().parse().unwrap();
        let runtime = TaskRuntime::initialize(&root).unwrap();
        let locator = ExternalSessionLocator::new("codex", &session).unwrap();
        let before_snapshot = runtime.read_snapshot(task_session_id).unwrap().unwrap();
        let before_history = runtime.read_signal_history(task_session_id).unwrap();
        let runtime_before = serde_json::to_vec(&(
            runtime
                .read_external_session_by_locator(&locator)
                .unwrap()
                .unwrap(),
            &before_history,
        ))
        .unwrap();
        let absolute = case.repository_path.join(case.locator.path().as_str());
        assert!(
            !absolute.exists(),
            "declared-path resolver must allow missing tails"
        );
        let arguments = json!({
            "agent_kind": "codex",
            "external_session_id": session,
            "expected_revision_id": expected_revision_id,
            "absolute_file_path": absolute,
            "locator": focus_coordinates(&case.locator),
            "token_budget": 8000,
            "max_spaces": 8
        });
        let focused = run_mcp(
            &mut server,
            &[tool_call(
                11 + u64::try_from(index).unwrap() * 10,
                "task_artifact_focus",
                arguments.clone(),
            )],
        );
        assert_eq!(
            focused[0]["result"]["isError"], false,
            "task_artifact_focus failed: {}",
            focused[0]
        );
        let data = &focused[0]["result"]["structuredContent"];
        assert_eq!(
            data["resolved_focus"]["repository_id"],
            case.repository_id.to_string()
        );
        assert_eq!(
            data["resolved_focus"]["locator"],
            serde_json::to_value(&case.locator).unwrap()
        );
        assert!(data.get("created").is_none());
        assert!(data.get("focus").is_none());
        // A Focus anchors the *file* its coordinate lives in, so every Context recorded against
        // that file comes back and not only the one whose Artifact kind the query named. The Graph
        // channel resolved a Focus to one exact Artifact and returned the Context attached to it;
        // ADR-0007's first lane joins on `(repository, path)` instead, and a Session that opened a
        // file to read one Symbol has opened the file the Api and the File Contexts describe too.
        // What the Focus still guarantees is that its own Context is among them.
        let items = data["context"]["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|item| item["context_id"] == case.context_id.to_string()),
            "the focused Context is missing from its own file's anchors: {items:#?}"
        );
        assert!(
            items
                .iter()
                .all(|item| item["retrieval_channels"] == json!(["file_anchor"])),
            "a Focus reaches knowledge by anchoring, and by nothing else: {items:#?}"
        );
        assert_eq!(data["context"]["graph_diagnostics"], json!([]));

        let retried = run_mcp(
            &mut server,
            &[tool_call(
                12 + u64::try_from(index).unwrap() * 10,
                "task_artifact_focus",
                arguments,
            )],
        );
        let retry = &retried[0]["result"]["structuredContent"];
        assert_eq!(retry["resolved_focus"], data["resolved_focus"]);
        let after_snapshot = runtime.read_snapshot(task_session_id).unwrap().unwrap();
        let after_history = runtime.read_signal_history(task_session_id).unwrap();
        let runtime_after = serde_json::to_vec(&(
            runtime
                .read_external_session_by_locator(&locator)
                .unwrap()
                .unwrap(),
            &after_history,
        ))
        .unwrap();
        assert_eq!(after_snapshot, before_snapshot);
        assert_eq!(after_history, before_history);
        assert_eq!(runtime_after, runtime_before);
        assert_eq!(after_snapshot.intent_revisions.len(), 1);
        if index == 0 {
            first_session = Some((session, task_id.to_owned(), expected_revision_id.to_owned()));
        }
    }

    let (session, _task_id, revision_id) = first_session.unwrap();
    let first_case = &cases[0];
    let second_case = &cases[1];
    let focus_arguments = |case: &FocusCase| {
        json!({
            "agent_kind": "codex",
            "external_session_id": session.clone(),
            "expected_revision_id": revision_id.clone(),
            "absolute_file_path": case.repository_path.join(case.locator.path().as_str()),
            "locator": focus_coordinates(&case.locator),
            "token_budget": 8000,
            "max_spaces": 8
        })
    };
    let only_second = run_mcp(
        &mut server,
        &[tool_call(
            500,
            "task_artifact_focus",
            focus_arguments(second_case),
        )],
    );
    let only_second = &only_second[0]["result"]["structuredContent"];
    assert_eq!(only_second["context"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        only_second["context"]["items"][0]["context_id"],
        second_case.context_id.to_string()
    );
    let stable_task_fingerprint = only_second["context"]["task_fingerprint"].clone();
    let ordinary = run_mcp(
        &mut server,
        &[tool_call(
            501,
            "task_context",
            json!({
                "agent_kind": "codex", "external_session_id": session.clone(),
                "token_budget": 2000, "max_spaces": 8
            }),
        )],
    );
    assert_eq!(
        ordinary[0]["result"]["structuredContent"]["items"],
        json!([])
    );
    assert_eq!(
        ordinary[0]["result"]["structuredContent"]["task_fingerprint"],
        stable_task_fingerprint
    );
    assert_eq!(
        ordinary[0]["result"]["structuredContent"]["graph_diagnostics"],
        json!([])
    );
    drop(server);
    let mut server = McpServer::new(&root, ClientKind::Codex).unwrap();
    let restarted = run_mcp(
        &mut server,
        &[rpc_request(
            502,
            "initialize",
            json!({"protocolVersion": "2024-11-05"}),
        )],
    );
    assert_eq!(restarted[0]["result"]["protocolVersion"], "2024-11-05");
    let after_restart = run_mcp(
        &mut server,
        &[tool_call(
            503,
            "task_artifact_focus",
            focus_arguments(first_case),
        )],
    );
    assert_eq!(
        after_restart[0]["result"]["isError"], false,
        "artifact Focus after MCP restart failed: {}",
        after_restart[0]
    );
    let after_restart = &after_restart[0]["result"]["structuredContent"];
    assert!(
        after_restart["context"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["context_id"] == first_case.context_id.to_string()),
        "a restart changes nothing about what a file anchors: {}",
        after_restart["context"]["items"]
    );
    assert_eq!(
        after_restart["context"]["task_fingerprint"],
        stable_task_fingerprint
    );

    let mut new_task_arguments = task_update_arguments(&session);
    new_task_arguments["expected_revision_id"] = json!(revision_id);
    let switched = run_mcp(
        &mut server,
        &[tool_call(504, "task_intent_update", new_task_arguments)],
    );
    assert_eq!(
        switched[0]["result"]["isError"], false,
        "Task switch failed: {}",
        switched[0]
    );
    assert_eq!(
        switched[0]["result"]["structuredContent"]["items"],
        json!([]),
        "a new Task must not restore a prior request-local Focus"
    );
    let after_switch = run_mcp(
        &mut server,
        &[tool_call(
            505,
            "task_context",
            json!({
                "agent_kind": "codex", "external_session_id": session.clone(),
                "token_budget": 2000, "max_spaces": 8
            }),
        )],
    );
    assert_eq!(
        after_switch[0]["result"]["structuredContent"]["items"],
        json!([])
    );

    let missing_session_id = TaskId::new().to_string();
    authorize_group_session(&root, &missing_session_id);
    let missing_task = run_mcp(
        &mut server,
        &[tool_call(
            510,
            "task_intent_update",
            task_update_arguments(&missing_session_id),
        )],
    );
    let missing_revision = missing_task[0]["result"]["structuredContent"]["intent_revision_id"]
        .as_str()
        .unwrap();
    let missing_path = main_repository.join("future/not-created.ts");
    let missing_arguments = json!({
        "agent_kind": "codex",
        "external_session_id": missing_session_id,
        "expected_revision_id": missing_revision,
        "absolute_file_path": missing_path,
        "locator": {
            "locator_kind": "symbol",
            "language": "typescript-javascript",
            "module": "future",
            "enclosing_type": null,
            "symbol_name": "notCreated",
            "signature": "export function notCreated()"
        },
        "token_budget": 2000,
        "max_spaces": 8
    });
    let missing = run_mcp(
        &mut server,
        &[tool_call(
            511,
            "task_artifact_focus",
            missing_arguments.clone(),
        )],
    );
    let missing = &missing[0]["result"]["structuredContent"];
    assert_eq!(missing["context"]["items"], json!([]));
    // A Focus on a file nobody has written a Context about returns nothing, and says so in the one
    // line an empty Pack owes its reader. It no longer says it as a Graph diagnostic: the Pack does
    // not consult the Graph, so "not reachable in the Graph" is not a fact it is in a position to
    // report. The Focus still resolves -- the coordinate below is the proof -- it simply anchors no
    // knowledge.
    assert_eq!(missing["context"]["graph_diagnostics"], json!([]));
    assert_eq!(
        missing["context"]["omitted"][0]["reason"],
        "no_lane_evidence"
    );
    assert_eq!(
        missing["resolved_focus"]["locator"]["path"],
        "future/not-created.ts"
    );
    let mut bounded_arguments = missing_arguments.clone();
    bounded_arguments["token_budget"] = json!(256);
    let bounded = run_mcp(
        &mut server,
        &[tool_call(5111, "task_artifact_focus", bounded_arguments)],
    );
    let bounded = &bounded[0]["result"]["structuredContent"]["context"];
    assert!(bounded["estimated_tokens"].as_u64().unwrap() <= 256);
    // The smallest budget the server accepts still buys the sentence that says why the Pack is
    // empty. It used to be a Graph diagnostic or, when even that did not fit, a notice that the
    // diagnostic had been dropped; it is now the one-line reason itself, which costs so little that
    // there is nothing left to drop it for.
    assert!(
        bounded["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |omitted| omitted["reason"] == "no_lane_evidence" || omitted["reason"] == "omitted"
            ),
        "{}",
        bounded["omitted"]
    );

    let mut stale_arguments = missing_arguments.clone();
    stale_arguments["expected_revision_id"] = json!(sctx_domain::TaskIntentRevisionId::new());
    let stale = run_mcp(
        &mut server,
        &[tool_call(512, "task_artifact_focus", stale_arguments)],
    );
    assert_eq!(stale[0]["result"]["isError"], true);

    for (offset, forbidden) in [
        ("repository_id", json!(main_id)),
        ("relative_path", json!("future/not-created.ts")),
        ("artifact_key", json!("forged")),
        ("generation", json!("forged")),
        ("hook", json!(true)),
        ("corroboration", json!({})),
        ("workspace", json!(cross)),
    ] {
        let mut forged = missing_arguments.clone();
        forged
            .as_object_mut()
            .unwrap()
            .insert(offset.to_owned(), forbidden);
        let rejected = run_mcp(
            &mut server,
            &[tool_call(520, "task_artifact_focus", forged)],
        );
        assert_eq!(rejected[0]["result"]["isError"], true, "accepted {offset}");
    }
    let mut nested_path = missing_arguments.clone();
    nested_path["locator"]["path"] = json!("future/not-created.ts");
    let rejected = run_mcp(
        &mut server,
        &[tool_call(521, "task_artifact_focus", nested_path)],
    );
    assert_eq!(rejected[0]["result"]["isError"], true);

    let mut durations = Vec::with_capacity(100);
    for index in 0..100 {
        let started = Instant::now();
        let response = run_mcp(
            &mut server,
            &[tool_call(
                600 + index,
                "task_artifact_focus",
                missing_arguments.clone(),
            )],
        );
        assert_eq!(response[0]["result"]["isError"], false);
        durations.push(started.elapsed().as_micros());
    }
    durations.sort_unstable();
    let p95 = durations[durations.len() * 95 / 100];
    eprintln!("task_artifact_focus established-session p95={p95}us");
    assert!(p95 < 250_000, "task_artifact_focus p95 {p95}us >= 250ms");

    assert_eq!(
        git(store.repository(), &["rev-parse", "HEAD"]),
        knowledge_head_before
    );
    assert_eq!(
        graph_store.canonical_bytes().unwrap().unwrap(),
        graph_before
    );
    let implementation = include_str!("../src/lib.rs");
    let focus_hot_path = implementation
        .split_once("    fn task_artifact_focus(\n")
        .unwrap()
        .1
        .split_once("    fn task_intent_update(\n")
        .unwrap()
        .0;
    for forbidden in [
        "Command::",
        "RepositoryScanner",
        "association_rebuild",
        "engineering_reference_record",
        "hook",
    ] {
        assert!(
            !focus_hot_path.contains(forbidden),
            "hot path contains {forbidden}"
        );
    }
}

#[test]
fn reference_recording_is_concurrent_private_and_rejects_unsafe_or_incomplete_input() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("root");
    let repo = temporary.path().join("repo");
    init_repo(&repo, &[("src/lib.rs", "pub fn graph_target() {}\n")]);
    let (context_id, revision_id) = accepted_context(&root, "concurrent graph reference");
    UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&repo),
        )
        .unwrap();
    let scan = repository_scan_at_root(&root, &scan_input(&repo, &["src/lib.rs"])).unwrap();
    assert!(
        repository_scan_at_root(&root, &scan_input(&repo, &[])).is_err(),
        "an empty public scan plan must not enumerate the Repository"
    );
    let missing = repository_scan_at_root(&root, &scan_input(&repo, &["src/missing.rs"])).unwrap();
    assert_eq!(missing.artifact_count, 0);
    assert_eq!(missing.scanned_files, 0);
    assert_eq!(missing.planned_path_count, 1);
    assert_eq!(missing.skipped_paths.len(), 1);
    assert_eq!(missing.skipped_paths[0].path, "src/missing.rs");
    assert_eq!(missing.skipped_paths[0].reason, "missing");
    let before_rejected = event_count(&root);

    assert!(
        repository_scan_at_root(
            &root,
            &RepositoryScanInput {
                checkout_path: "../unsafe".to_owned(),
                paths: vec!["src/lib.rs".to_owned()],
                max_artifacts: 10,
            },
        )
        .is_err()
    );
    let mut incomplete = reference_input(
        context_id,
        revision_id,
        &scan.repository_id,
        ArtifactKind::File,
        ReferenceRelation::Implements,
        ArtifactLocator::File {
            path: RepoRelativePath::new("src/lib.rs").unwrap(),
        },
    );
    incomplete.limitations.clear();
    assert!(engineering_reference_record_at_root(&root, &incomplete).is_err());
    let mut secret = incomplete.clone();
    secret.limitations = vec!["secret scanning fixture".to_owned()];
    secret.supports = "api_key=sk-secret-value-that-must-not-persist".to_owned();
    assert!(engineering_reference_record_at_root(&root, &secret).is_err());
    assert_eq!(event_count(&root), before_rejected);

    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut threads = Vec::new();
    for index in 0..workers {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        let mut input = reference_input(
            context_id,
            revision_id,
            &scan.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            ArtifactLocator::File {
                path: RepoRelativePath::new("src/lib.rs").unwrap(),
            },
        );
        input.supports = format!("Concurrent verified observation {index}");
        threads.push(thread::spawn(move || {
            barrier.wait();
            engineering_reference_record_at_root(root, &input).unwrap()
        }));
    }
    let responses = threads
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    let ids = responses
        .iter()
        .map(|response| response.reference_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), workers);
    fs::remove_file(repo.join("src/lib.rs")).unwrap();
    let rebuilt = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(rebuilt.reference_count, workers);
    assert_eq!(rebuilt.repositories.len(), 1);
    assert_eq!(rebuilt.repositories[0].planned_path_count, 1);
    assert_eq!(rebuilt.repositories[0].status, "available");
    assert_eq!(rebuilt.repositories[0].artifact_count, 0);
    assert_eq!(rebuilt.status_counts.missing, workers);
}

#[test]
#[allow(clippy::too_many_lines)]
fn ambiguous_and_unavailable_explanations_never_choose_and_graph_failure_degrades_task_reads() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("root");
    let repo = temporary.path().join("repo");
    init_repo(
        &repo,
        &[(
            "src/a/one.rs",
            "pub struct SharedGraphSymbol;\npub struct SharedGraphSymbol;\n",
        )],
    );
    let (context_id, revision_id) = accepted_context(&root, "ambiguous graph reference");
    UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&repo),
        )
        .unwrap();
    let scan = repository_scan_at_root(&root, &scan_input(&repo, &["src/a/one.rs"])).unwrap();
    let locator = scan
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == ArtifactKind::Symbol)
        .map(|artifact| artifact.locator.clone())
        .unwrap();
    let ambiguous_input = reference_input(
        context_id,
        revision_id,
        &scan.repository_id,
        ArtifactKind::Symbol,
        ReferenceRelation::Defines,
        locator,
    );
    let recorded = engineering_reference_record_at_root(&root, &ambiguous_input).unwrap();
    association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    let ambiguous = association_explain_at_root(
        &root,
        &AssociationExplainInput {
            reference_id: recorded.reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(ambiguous.status, sctx_domain::ResolutionStatus::Ambiguous);
    assert!(ambiguous.resolved_artifact.is_none());
    assert_eq!(ambiguous.ambiguity_candidates.len(), 1);
    assert_eq!(
        ambiguous.graph_paths.len(),
        ambiguous.ambiguity_candidates.len()
    );

    fs::remove_dir_all(&repo).unwrap();
    let unavailable = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: true,
        },
    )
    .unwrap();
    assert!(!unavailable.stored);
    assert_eq!(unavailable.repositories[0].status, "unavailable");
    assert!(unavailable.repositories[0].unavailable_reason.is_some());
    let stored_unavailable = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(stored_unavailable.status_counts.unavailable, 1);
    let explained_unavailable = association_explain_at_root(
        &root,
        &AssociationExplainInput {
            reference_id: recorded.reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(
        explained_unavailable.status,
        sctx_domain::ResolutionStatus::Unavailable
    );
    assert!(explained_unavailable.ambiguity_candidates.is_empty());

    let task_id = TaskId::new();
    TaskRuntime::initialize(&root)
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("codex", "degraded-graph").unwrap(),
            task_id,
            task_intent(task_id, "ambiguous graph task"),
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "src/a/one.rs".to_owned(),
            }],
        )
        .unwrap();
    let graph_database = root.join("state/engineering.sqlite");
    fs::write(&graph_database, b"corrupt graph database").unwrap();
    let degraded = task_context_readonly_at_root(
        &root,
        &TaskContextReadInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "degraded-graph".to_owned(),
            token_budget: 4_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    assert!(degraded.artifact_generation.is_none());
}

/// Registers one Repository, records one Engineering Reference against an accepted Context, and
/// returns the installation root. Nothing here ever calls `association_rebuild`.
fn auto_scan_root(
    temporary: &TempDir,
    auto_scan: Option<bool>,
) -> (std::path::PathBuf, sctx_domain::ReferenceId) {
    let root = temporary.path().join("auto scan root");
    let checkout = temporary.path().join("auto scan checkout");
    init_repo(
        &checkout,
        &[(
            "src/anchor.rs",
            "pub fn auto_scan_anchor() -> bool { true }\n",
        )],
    );
    let checkout = fs::canonicalize(&checkout).unwrap();
    let (context_id, revision_id) = accepted_context(&root, "auto scan decision");
    let config = UserConfigStore::initialize(&root).unwrap();
    let repository = config
        .add_repository("AutoScan".parse().unwrap(), std::slice::from_ref(&checkout))
        .unwrap()
        .repository;
    if let Some(auto_scan) = auto_scan {
        let config_path = root.join("config.toml");
        let mut text = fs::read_to_string(&config_path).unwrap();
        text.push_str(if auto_scan {
            "\n[engineering]\nauto_scan = true\n"
        } else {
            "\n[engineering]\nauto_scan = false\n"
        });
        fs::write(&config_path, text).unwrap();
    }
    let recorded = engineering_reference_record_at_root(
        &root,
        &reference_input(
            context_id,
            revision_id,
            &repository.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            ArtifactLocator::File {
                path: RepoRelativePath::new("src/anchor.rs").unwrap(),
            },
        ),
    )
    .unwrap();
    (root, recorded.reference_id)
}

/// The Graph channel has to be live in a real installation, not only in one that remembers to run
/// `sctx association rebuild` by hand. Recording an Engineering Reference is an interactive MCP
/// write, so it spends its bounded budget scanning the Repository the Reference points into.
#[test]
fn recording_an_engineering_reference_scans_the_repository_it_names() {
    let temporary = TempDir::new().unwrap();
    let (root, reference_id) = auto_scan_root(&temporary, None);

    let snapshot = EngineeringProjectionStore::initialize(&root)
        .unwrap()
        .read_snapshot()
        .unwrap()
        .expect("the default [engineering] table scans without being asked");
    assert!(
        !snapshot.projection.contexts.is_empty(),
        "an installation whose graph_context_snapshot is empty has no Graph channel at all"
    );
    assert_eq!(snapshot.projection.references.len(), 1);
    assert_eq!(
        snapshot.projection.references[0].resolution.status,
        sctx_domain::ResolutionStatus::Resolved,
        "the Reference resolves against the snapshot the same write produced"
    );

    let explained = association_explain_at_root(
        &root,
        &AssociationExplainInput {
            reference_id: reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(explained.status, sctx_domain::ResolutionStatus::Resolved);
}

/// `auto_scan = false` is the explicit opt-out, and it is silent: the write succeeds and the
/// Graph is simply left where the last explicit rebuild put it.
#[test]
fn auto_scan_false_leaves_the_graph_exactly_where_it_was() {
    let temporary = TempDir::new().unwrap();
    let (root, _) = auto_scan_root(&temporary, Some(false));

    assert!(
        EngineeringProjectionStore::initialize(&root)
            .unwrap()
            .read_snapshot()
            .unwrap()
            .is_none(),
        "the opt-out writes no projection at all"
    );
    // The same opt-out reaches the shape a newly registered Repository was left in before
    // registration scanned anything: References in the Store, and nothing that ever read the
    // checkout. The scan `sctx repository add` now runs obeys the switch like everything else.
    let opted_out = sctx_mcp::repository_first_scan_at_root(&root).unwrap();
    assert!(
        !opted_out.attempted,
        "`auto_scan = false` turns off the registration scan too"
    );
    assert!(
        EngineeringProjectionStore::initialize(&root)
            .unwrap()
            .read_snapshot()
            .unwrap()
            .is_none()
    );

    let config_path = root.join("config.toml");
    let text = fs::read_to_string(&config_path)
        .unwrap()
        .replace("auto_scan = false", "auto_scan = true");
    fs::write(&config_path, text).unwrap();
    let scanned = sctx_mcp::repository_first_scan_at_root(&root).unwrap();
    assert!(scanned.attempted);
    assert!(
        scanned.completed,
        "a registration scan gets a budget wide enough to finish a first read"
    );
    assert_eq!(
        scanned
            .rebuild
            .expect("a completed scan reports what it built")
            .status_counts
            .resolved,
        1,
        "registration resolves the References that were waiting on the checkout"
    );

    // The explicit command still works, and is what the doctor warning points at.
    let rebuilt = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(rebuilt.status_counts.resolved, 1);
}

/// The same rescan on the path that actually produces most References: a Confirmation whose
/// Claim named a file. `candidate_confirm` is an interactive MCP call, so it pays the bounded
/// budget once and reports `graph_rebuild_pending` when it could not.
#[test]
#[allow(clippy::too_many_lines)]
fn a_confirmation_that_names_engineering_references_rescans_within_its_budget() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("confirm scan root");
    let checkout = temporary.path().join("confirm scan checkout");
    init_repo(
        &checkout,
        &[("app/src/anchor/ProductAnchorAssem.kt", "// fixture\n")],
    );
    let checkout = fs::canonicalize(&checkout).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            "ConfirmScan".parse().unwrap(),
            std::slice::from_ref(&checkout),
        )
        .unwrap();
    let session = "confirm-scan";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(&locator, &catalog, &checkout)
        .unwrap();
    let space = sctx_mcp::space_create_at_root(
        &root,
        &sctx_mcp::SpaceCreateInput {
            intent: IntentSnapshot {
                title: "Anchor assembly navigation".to_owned(),
                problem: "The anchor assembly skips navigation".to_owned(),
                desired_outcome: "Every confirmed Claim reaches the Graph".to_owned(),
                in_scope: vec!["Engineering Graph".to_owned()],
                out_of_scope: Vec::new(),
                acceptance_conditions: vec!["The Reference resolves".to_owned()],
                domain_terms: vec!["anchor".to_owned()],
            },
        },
    )
    .unwrap();
    let task = task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("confirm the anchor assembly finding").unwrap(),
        },
    )
    .unwrap();
    let accepted = sctx_mcp::task_checkpoint_at_root(
        &root,
        &sctx_mcp::TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![sctx_mcp::TaskCheckpointClaimInput {
                context_kind: ContextKind::Issue,
                statement: "ProductAnchorAssem.kt:202 returns early and skips navigation"
                    .to_owned(),
                rationale: "The early return observably diverges from the baseline".to_owned(),
                conditions: vec!["live entry service is absent".to_owned()],
                evidence: vec![sctx_mcp::TaskCheckpointEvidenceInput {
                    evidence_type: EvidenceType::SourceSnapshot,
                    summary: "ProductAnchorAssem.kt:202 returns before dispatch".to_owned(),
                    limitations: Vec::new(),
                }],
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("a nonempty Checkpoint is accepted");
    sctx_mcp::build_closed_episode_at_root(&root, accepted.episode_id).unwrap();

    assert!(
        EngineeringProjectionStore::initialize(&root)
            .unwrap()
            .read_snapshot()
            .unwrap()
            .is_none(),
        "nothing before the Confirmation has written a Graph"
    );

    let listed = sctx_mcp::candidate_list_at_root(
        &root,
        &sctx_mcp::CandidateListInput {
            scope: sctx_domain::CandidateReviewScope::Task,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: sctx_domain::CandidateReviewStatus::Pending,
            limit: 100,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert_eq!(listed.reviews.len(), 1);
    let candidate_id = listed.reviews[0].0.candidate_id;

    let confirmed = sctx_mcp::candidate_confirm_at_root(
        &root,
        &sctx_mcp::CandidateConfirmInput {
            decision_source: DecisionSource::Human,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            candidate_id: candidate_id.to_string(),
            expected_review_version: 1,
            primary: sctx_mcp::CandidateConfirmPrimaryInput::Existing(
                sctx_mcp::ExistingCandidatePrimaryInput {
                    existing_space_id: space.space_id.to_string(),
                },
            ),
            related_space_ids: Vec::new(),
            edits: sctx_domain::OptionalCandidateEdits::default(),
        },
    )
    .unwrap();
    assert!(
        !confirmed.graph_rebuild_pending,
        "the bounded rescan finished inside the Confirmation"
    );
    assert!(
        confirmed.advice.is_none(),
        "a Confirmation with nothing pending has nothing to advise: {:?}",
        confirmed.advice
    );

    let snapshot = EngineeringProjectionStore::initialize(&root)
        .unwrap()
        .read_snapshot()
        .unwrap()
        .expect("the Confirmation built the Graph");
    assert!(
        !snapshot.projection.contexts.is_empty(),
        "graph_context_snapshot is what a Task Context Pack reads; an empty one is no channel"
    );
    assert!(!snapshot.projection.references.is_empty());
    assert!(
        snapshot
            .projection
            .references
            .iter()
            .all(|reference| reference.resolution.status
                == sctx_domain::ResolutionStatus::Resolved),
        "{:#?}",
        snapshot.projection.references
    );

    // The pending flag was reported honestly and then read by nobody: a caller that does not know
    // what a pending rebuild costs has no reason to mention it. The sentence travels with the
    // flag, so the fix sits in the same place as the fact. The explicit opt-out is the
    // deterministic way to reach the pending branch without racing a clock.
    let config_path = root.join("config.toml");
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[engineering]\nauto_scan = false\n");
    fs::write(&config_path, text).unwrap();
    let pending = confirm_next_candidate(&root, session, &task, space.space_id);
    assert!(
        pending.graph_rebuild_pending,
        "the opt-out leaves the Graph behind the Store"
    );
    let advice = pending
        .advice
        .expect("a pending rebuild must say what to do about it");
    assert!(advice.contains("sctx association rebuild"), "{advice}");
    assert!(advice.contains("Tell the user"), "{advice}");
}

/// Checkpoints one more Claim naming the same file and confirms the Candidate it produces.
fn confirm_next_candidate(
    root: &std::path::Path,
    session: &str,
    task: &sctx_mcp::TaskIntentUpdateResponse,
    space_id: sctx_domain::SpaceId,
) -> sctx_mcp::CandidateConfirmResponse {
    let accepted = sctx_mcp::task_checkpoint_at_root(
        root,
        &sctx_mcp::TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![sctx_mcp::TaskCheckpointClaimInput {
                context_kind: ContextKind::Issue,
                statement: "ProductAnchorAssem.kt:311 drops the navigation callback".to_owned(),
                rationale: "The dropped callback observably diverges from the baseline".to_owned(),
                conditions: vec!["live entry service is absent".to_owned()],
                evidence: vec![sctx_mcp::TaskCheckpointEvidenceInput {
                    evidence_type: EvidenceType::SourceSnapshot,
                    summary: "ProductAnchorAssem.kt:311 never dispatches".to_owned(),
                    limitations: Vec::new(),
                }],
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("a nonempty Checkpoint is accepted");
    sctx_mcp::build_closed_episode_at_root(root, accepted.episode_id).unwrap();
    let listed = sctx_mcp::candidate_list_at_root(
        root,
        &sctx_mcp::CandidateListInput {
            scope: sctx_domain::CandidateReviewScope::Task,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: sctx_domain::CandidateReviewStatus::Pending,
            limit: 100,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    let review = listed
        .reviews
        .first()
        .expect("the second Checkpoint produced a Candidate");
    sctx_mcp::candidate_confirm_at_root(
        root,
        &sctx_mcp::CandidateConfirmInput {
            decision_source: DecisionSource::Human,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            candidate_id: review.0.candidate_id.to_string(),
            expected_review_version: review.0.review_version,
            primary: sctx_mcp::CandidateConfirmPrimaryInput::Existing(
                sctx_mcp::ExistingCandidatePrimaryInput {
                    existing_space_id: space_id.to_string(),
                },
            ),
            related_space_ids: Vec::new(),
            edits: sctx_domain::OptionalCandidateEdits::default(),
        },
    )
    .unwrap()
}

/// Resolution never guesses after a move, and that is exactly why the move has to be reported.
/// A `Missing` Reference and a renamed file are the same status and completely different
/// situations, and only the Repository's own history can tell them apart.
#[test]
fn a_renamed_artifact_is_reported_as_a_relocation_candidate_and_never_resolved() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("relocation root");
    let checkout = temporary.path().join("relocation checkout");
    init_repo(
        &checkout,
        &[(
            "src/alpha.rs",
            "pub fn relocation_anchor() -> bool { true }\n",
        )],
    );
    let checkout = fs::canonicalize(&checkout).unwrap();
    let (context_id, revision_id) = accepted_context(&root, "relocation decision");
    let repository = UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            "Relocation".parse().unwrap(),
            std::slice::from_ref(&checkout),
        )
        .unwrap()
        .repository;
    let recorded = engineering_reference_record_at_root(
        &root,
        &reference_input(
            context_id,
            revision_id,
            &repository.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            ArtifactLocator::File {
                path: RepoRelativePath::new("src/alpha.rs").unwrap(),
            },
        ),
    )
    .unwrap();

    git(&checkout, &["mv", "src/alpha.rs", "src/beta.rs"]);
    git(&checkout, &["commit", "-q", "-m", "move the anchor"]);

    let rebuilt = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(rebuilt.status_counts.missing, 1);
    assert_eq!(rebuilt.status_counts.resolved, 0);
    assert_eq!(rebuilt.relocation_candidates.len(), 1);
    let candidate = &rebuilt.relocation_candidates[0];
    assert_eq!(candidate.reference_id, recorded.reference_id);
    assert_eq!(candidate.from, "src/alpha.rs");
    assert_eq!(candidate.to, "src/beta.rs");
    assert_eq!(candidate.commit.len(), 40);
    assert!(candidate.advice.contains("engineering_reference_record"));

    let explained = association_explain_at_root(
        &root,
        &AssociationExplainInput {
            reference_id: recorded.reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(explained.status, sctx_domain::ResolutionStatus::Missing);
    assert!(
        explained.resolved_artifact.is_none() && explained.ambiguity_candidates.is_empty(),
        "naming the rename never resolves the Reference to it"
    );
    assert_eq!(
        explained
            .relocation_candidate
            .as_ref()
            .map(|found| found.to.as_str()),
        Some("src/beta.rs")
    );

    // A plain delete is not a rename, and the diagnosis says nothing rather than something close.
    git(&checkout, &["rm", "-q", "src/beta.rs"]);
    git(&checkout, &["commit", "-q", "-m", "drop the anchor"]);
    let deleted = engineering_reference_record_at_root(
        &root,
        &reference_input(
            context_id,
            revision_id,
            &repository.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            ArtifactLocator::File {
                path: RepoRelativePath::new("src/beta.rs").unwrap(),
            },
        ),
    )
    .unwrap();
    let after_delete = association_explain_at_root(
        &root,
        &AssociationExplainInput {
            reference_id: deleted.reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_delete.status, sctx_domain::ResolutionStatus::Missing);
    assert!(after_delete.relocation_candidate.is_none());
}
