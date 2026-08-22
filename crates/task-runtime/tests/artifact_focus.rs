use std::{sync::Arc, thread};

use sctx_domain::{
    ArtifactLocator, ExternalSessionLocator, RepoRelativePath, RepositoryId, TaskArtifactFocus,
    TaskId, TaskIntent, TaskIntentDraft, TaskSignal, TaskSignalKind, TaskSignalLifecycle,
};
use sctx_task_runtime::TaskRuntime;
use tempfile::TempDir;

fn external(name: &str) -> ExternalSessionLocator {
    ExternalSessionLocator::new("codex", name).unwrap()
}

fn intent(task_id: TaskId, goal: &str) -> TaskIntent {
    TaskIntent {
        task_id,
        goal: goal.to_owned(),
        desired_change: format!("implement {goal}"),
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

fn path(value: &str) -> RepoRelativePath {
    RepoRelativePath::new(value).unwrap()
}

fn focuses(repository_id: RepositoryId) -> Vec<TaskArtifactFocus> {
    [
        ArtifactLocator::File {
            path: path("src/search.ts"),
        },
        ArtifactLocator::Module {
            path: path("src/search"),
        },
        ArtifactLocator::Symbol {
            path: path("src/search.ts"),
            language: "typescript".to_owned(),
            module: "search".to_owned(),
            enclosing_type: None,
            symbol_name: "run".to_owned(),
            signature: "run(query: string)".to_owned(),
        },
        ArtifactLocator::Api {
            path: path("api/search.yaml"),
            protocol: "http".to_owned(),
            operation: "GET".to_owned(),
            normalized_route: "/v2/search".to_owned(),
        },
        ArtifactLocator::Schema {
            path: path("api/search.yaml"),
            namespace: "search".to_owned(),
            version: "v2".to_owned(),
            qualified_name: "search::Response".to_owned(),
        },
        ArtifactLocator::Test {
            path: path("tests/search.rs"),
            qualified_test_name: "search::returns_results".to_owned(),
        },
    ]
    .into_iter()
    .map(|locator| TaskArtifactFocus {
        repository_id,
        locator,
    })
    .collect()
}

fn open(
    runtime: &TaskRuntime,
    locator: ExternalSessionLocator,
    goal: &str,
) -> sctx_domain::TaskSessionSnapshot {
    runtime
        .open_or_create(
            locator,
            intent(TaskId::new(), goal),
            vec![TaskSignal {
                kind: TaskSignalKind::Workspace,
                content: "/work/shared".to_owned(),
            }],
        )
        .unwrap()
        .snapshot
}

#[test]
fn six_kinds_are_cas_merged_idempotent_superseded_and_retained_without_intent_revision() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let initial = open(&runtime, external("focus-lifecycle"), "focus lifecycle");
    let head = initial.current_intent_revision().unwrap().revision_id;
    let all = focuses(RepositoryId::new());

    let merged = runtime
        .merge_artifact_focuses(initial.task_session_id, initial.task_id, head, all.clone())
        .unwrap();
    assert_eq!(merged.inserted, 6);
    assert_eq!(merged.snapshot.artifact_focuses.len(), 6);
    assert_eq!(merged.snapshot.intent_revisions, initial.intent_revisions);

    let repeated = runtime
        .merge_artifact_focuses(
            initial.task_session_id,
            initial.task_id,
            head,
            vec![all[0].clone(), all[0].clone()],
        )
        .unwrap();
    assert_eq!(repeated.inserted, 0);
    assert_eq!(
        repeated.snapshot.artifact_focuses,
        merged.snapshot.artifact_focuses
    );
    assert!(
        runtime
            .merge_artifact_focuses(initial.task_session_id, TaskId::new(), head, Vec::new())
            .is_err()
    );
    assert!(
        runtime
            .merge_artifact_focuses(
                initial.task_session_id,
                initial.task_id,
                sctx_domain::TaskIntentRevisionId::new(),
                Vec::new(),
            )
            .is_err()
    );

    let ids = merged
        .snapshot
        .artifact_focuses
        .iter()
        .take(2)
        .map(|record| record.signal_id)
        .collect::<Vec<_>>();
    let superseded = runtime
        .supersede_signals(initial.task_session_id, initial.task_id, ids.clone())
        .unwrap();
    assert_eq!(superseded.snapshot.artifact_focuses.len(), 4);
    let history = runtime
        .read_artifact_focus_history(initial.task_session_id)
        .unwrap();
    assert_eq!(history.len(), 6);
    assert_eq!(
        history
            .iter()
            .filter(|record| { record.lifecycle == TaskSignalLifecycle::Superseded })
            .count(),
        2
    );

    let reactivated = runtime
        .merge_artifact_focuses(
            initial.task_session_id,
            initial.task_id,
            head,
            vec![all[0].clone()],
        )
        .unwrap();
    assert_eq!(reactivated.inserted, 1);
    assert!(!ids.contains(&reactivated.inserted_signal_ids[0]));
    assert_eq!(
        runtime
            .read_artifact_focus_history(initial.task_session_id)
            .unwrap()
            .len(),
        7
    );
}

#[test]
fn new_task_does_not_inherit_focus_and_switch_restores_only_own_active_history() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let locator = external("task-boundary");
    let first = open(&runtime, locator.clone(), "first task");
    let head = first.current_intent_revision().unwrap().revision_id;
    runtime
        .merge_artifact_focuses(
            first.task_session_id,
            first.task_id,
            head,
            vec![focuses(RepositoryId::new())[0].clone()],
        )
        .unwrap();
    let second = runtime
        .start_new_task(
            &locator,
            first.task_id,
            &TaskIntentDraft {
                goal: "second task".to_owned(),
                desired_change: "implement second task".to_owned(),
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
            Vec::new(),
        )
        .unwrap()
        .snapshot;
    assert!(second.artifact_focuses.is_empty());
    assert_eq!(
        runtime
            .read_artifact_focus_history(first.task_session_id)
            .unwrap()
            .len(),
        1
    );
    let restored = runtime
        .switch_active_task(&locator, second.task_id, first.task_id)
        .unwrap()
        .snapshot;
    assert_eq!(restored.artifact_focuses.len(), 1);
}

#[test]
fn same_workspace_double_sessions_and_concurrent_duplicate_focus_remain_isolated() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let first = open(&runtime, external("session-a"), "alpha");
    let second = open(&runtime, external("session-b"), "beta");
    let focus = focuses(RepositoryId::new())[0].clone();
    let first_head = first.current_intent_revision().unwrap().revision_id;
    let mut workers = Vec::new();
    for _ in 0..8 {
        let runtime = Arc::clone(&runtime);
        let focus = focus.clone();
        workers.push(thread::spawn(move || {
            runtime
                .merge_artifact_focuses(
                    first.task_session_id,
                    first.task_id,
                    first_head,
                    vec![focus],
                )
                .unwrap()
                .inserted
        }));
    }
    assert_eq!(
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum::<usize>(),
        1
    );
    let first_snapshot = runtime
        .read_snapshot(first.task_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(first_snapshot.artifact_focuses.len(), 1);
    assert!(
        runtime
            .read_snapshot(second.task_session_id)
            .unwrap()
            .unwrap()
            .artifact_focuses
            .is_empty()
    );

    let second_head = second.current_intent_revision().unwrap().revision_id;
    runtime
        .merge_artifact_focuses(
            second.task_session_id,
            second.task_id,
            second_head,
            vec![focus],
        )
        .unwrap();
    let second_snapshot = runtime
        .read_snapshot(second.task_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(second_snapshot.artifact_focuses.len(), 1);
    assert_ne!(
        first_snapshot.artifact_focuses[0].signal_id,
        second_snapshot.artifact_focuses[0].signal_id
    );
}

#[test]
fn malformed_cross_kind_and_path_traversal_focus_is_rejected_before_runtime_write() {
    let malformed = serde_json::from_value::<TaskArtifactFocus>(serde_json::json!({
        "repository_id": RepositoryId::new(),
        "locator": {
            "locator_kind": "file",
            "path": "src/search.ts",
            "symbol_name": "cross-kind"
        }
    }));
    assert!(malformed.is_err());
    assert!(RepoRelativePath::new("../outside.rs").is_err());

    let invalid_path: RepoRelativePath =
        serde_json::from_value(serde_json::Value::String("../outside.rs".to_owned())).unwrap();
    let invalid = TaskArtifactFocus {
        repository_id: RepositoryId::new(),
        locator: ArtifactLocator::File { path: invalid_path },
    };
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let snapshot = open(&runtime, external("invalid"), "invalid focus");
    assert!(
        runtime
            .merge_artifact_focuses(
                snapshot.task_session_id,
                snapshot.task_id,
                snapshot.current_intent_revision().unwrap().revision_id,
                vec![invalid],
            )
            .is_err()
    );
    assert!(
        runtime
            .read_artifact_focus_history(snapshot.task_session_id)
            .unwrap()
            .is_empty()
    );
}
