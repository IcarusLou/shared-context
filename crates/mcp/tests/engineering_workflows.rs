use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{
    Applicability, ArtifactKind, ContextId, ContextKind, ContextRevisionDraft,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, IntentSnapshot, LocatorHints,
    PublicationAction, PublicationDraft, ReferenceRelation, ReviewDraft, ReviewVerdict, RevisionId,
    TaskId, TaskIntent, TaskIntentDraft, TaskSignal, TaskSignalKind,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_mcp::{
    AssociationExplainInput, AssociationRebuildInput, EngineeringReferenceRecordInput,
    ExpectedRevisionId, IntentMaturity, RepositoryScanInput, TaskBoundary, TaskContextReadInput,
    TaskIntentUpdateInput, association_explain_at_root, association_rebuild_at_root,
    engineering_reference_record_at_root, repository_scan_at_root, task_context_readonly_at_root,
    task_intent_update_at_root,
};
use sctx_search::TaskRetrievalPath;
use sctx_task_runtime::TaskRuntime;
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

fn context_draft(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
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
    let store = GitStore::initialize(root).unwrap();
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

fn scan_input(path: &Path) -> RepositoryScanInput {
    RepositoryScanInput {
        checkout_path: path.to_string_lossy().into_owned(),
        declared_identity: None,
        remote_hint: None,
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
    repository_id: sctx_domain::RepositoryId,
    kind: ArtifactKind,
    relation: ReferenceRelation,
    locator_hints: LocatorHints,
) -> EngineeringReferenceRecordInput {
    EngineeringReferenceRecordInput {
        context_id: context_id.to_string(),
        revision_id: revision_id.to_string(),
        repository_id: repository_id.to_string(),
        artifact_kind: kind,
        relation,
        locator_hints: Some(locator_hints),
        content_fingerprint: None,
        semantic_fingerprint: None,
        supports: "Direct source inspection verified this relationship".to_owned(),
        limitations: vec!["Verified only against the current tracked snapshot".to_owned()],
    }
}

fn task_intent(task_id: TaskId, goal: &str) -> TaskIntent {
    TaskIntent {
        task_id,
        goal: goal.to_owned(),
        desired_change: format!("Use the verified {goal} implementation"),
        in_scope: vec![goal.to_owned()],
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec!["Graph Context is retrieved".to_owned()],
        artifacts: Vec::new(),
        interfaces: Vec::new(),
        unknowns: Vec::new(),
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
    let (context_id, revision_id) = accepted_context(&root, "alpha graph decision");

    let first_scan = repository_scan_at_root(&root, &scan_input(&first)).unwrap();
    let worktree_scan = repository_scan_at_root(&root, &scan_input(&worktree)).unwrap();
    let second_scan = repository_scan_at_root(&root, &scan_input(&second)).unwrap();
    assert_eq!(first_scan.repository_id, worktree_scan.repository_id);
    assert_ne!(first_scan.repository_id, second_scan.repository_id);
    assert!(first_scan.repository_generation.is_some());
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
            first_scan.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            LocatorHints {
                path: Some("src/alpha.rs".to_owned()),
                language: Some("rust".to_owned()),
                ..LocatorHints::default()
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
    assert_eq!(rebuilt.repositories.len(), 2);
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

    let authoritative = task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "graph-session".to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            maturity: IntentMaturity::Provisional,
            intent: TaskIntentDraft {
                goal: "Use alpha graph context".to_owned(),
                desired_change: "Retrieve the verified alpha implementation".to_owned(),
                in_scope: Vec::new(),
                out_of_scope: Vec::new(),
                domains: Vec::new(),
                platforms: Vec::new(),
                constraints: Vec::new(),
                acceptance_conditions: Vec::new(),
                artifacts: Vec::new(),
                interfaces: Vec::new(),
                unknowns: Vec::new(),
            },
            evidence_refs: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(
        authoritative.context.artifact_generation,
        Some(rebuilt.artifact_generation.clone())
    );
    TaskRuntime::initialize(&root)
        .unwrap()
        .merge_signals_by_locator(
            &ExternalSessionLocator::new("codex", "graph-session").unwrap(),
            vec![TaskSignal {
                kind: TaskSignalKind::File,
                content: "src/alpha.rs".to_owned(),
            }],
        )
        .unwrap()
        .unwrap();
    let pack = task_context_readonly_at_root(
        &root,
        &TaskContextReadInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "graph-session".to_owned(),
            token_budget: 4_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    assert_eq!(pack.artifact_generation, Some(rebuilt.artifact_generation));
    assert!(pack.items.iter().any(|item| {
        item.context.context_id == context_id
            && item
                .retrieval_paths
                .iter()
                .any(|path| matches!(path, TaskRetrievalPath::EngineeringGraph { .. }))
    }));
}

#[test]
fn reference_recording_is_concurrent_private_and_rejects_unsafe_or_incomplete_input() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("root");
    let repo = temporary.path().join("repo");
    init_repo(&repo, &[("src/lib.rs", "pub fn graph_target() {}\n")]);
    let (context_id, revision_id) = accepted_context(&root, "concurrent graph reference");
    let scan = repository_scan_at_root(&root, &scan_input(&repo)).unwrap();
    let before_rejected = event_count(&root);

    assert!(
        repository_scan_at_root(
            &root,
            &RepositoryScanInput {
                checkout_path: "../unsafe".to_owned(),
                declared_identity: None,
                remote_hint: None,
                max_artifacts: 10,
            },
        )
        .is_err()
    );
    let mut incomplete = reference_input(
        context_id,
        revision_id,
        scan.repository_id,
        ArtifactKind::File,
        ReferenceRelation::Implements,
        LocatorHints {
            path: Some("src/lib.rs".to_owned()),
            ..LocatorHints::default()
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
            scan.repository_id,
            ArtifactKind::File,
            ReferenceRelation::Implements,
            LocatorHints {
                path: Some("src/lib.rs".to_owned()),
                ..LocatorHints::default()
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
    let rebuilt = association_rebuild_at_root(
        &root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(rebuilt.reference_count, workers);
}

#[test]
#[allow(clippy::too_many_lines)]
fn ambiguous_and_unavailable_explanations_never_choose_and_graph_failure_degrades_task_reads() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("root");
    let repo = temporary.path().join("repo");
    init_repo(
        &repo,
        &[
            ("src/a/one.rs", "pub struct SharedGraphSymbol;\n"),
            ("src/b/two.rs", "pub struct SharedGraphSymbol;\n"),
        ],
    );
    let (context_id, revision_id) = accepted_context(&root, "ambiguous graph reference");
    let scan = repository_scan_at_root(&root, &scan_input(&repo)).unwrap();
    let semantic = scan
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == ArtifactKind::Symbol)
        .and_then(|artifact| artifact.semantic_fingerprint.as_ref())
        .unwrap()
        .as_str()
        .to_owned();
    let mut ambiguous_input = reference_input(
        context_id,
        revision_id,
        scan.repository_id,
        ArtifactKind::Symbol,
        ReferenceRelation::Defines,
        LocatorHints {
            path: Some("src/a/one.rs".to_owned()),
            symbol: Some("old-unqualified-symbol".to_owned()),
            language: Some("rust".to_owned()),
            ..LocatorHints::default()
        },
    );
    ambiguous_input.semantic_fingerprint = Some(semantic);
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
    assert!(ambiguous.ambiguity_candidates.len() >= 2);
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
            task_intent(task_id, "ambiguous graph task"),
            vec![TaskSignal {
                kind: TaskSignalKind::File,
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
