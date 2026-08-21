use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{
    ErrorKind, ExternalSessionLocator, TaskId, TaskIntent, TaskIntentDraft, TaskSignal,
    TaskSignalKind, TaskSignalLifecycle,
};
use sctx_task_runtime::TaskRuntime;
use tempfile::TempDir;

fn intent(task_id: TaskId, goal: &str) -> TaskIntent {
    TaskIntent {
        task_id,
        goal: goal.to_owned(),
        desired_change: format!("Deliver {goal}"),
        in_scope: vec![goal.to_owned()],
        out_of_scope: vec![],
        domains: vec!["task-runtime".to_owned()],
        platforms: vec![],
        constraints: vec!["No Space route".to_owned()],
        acceptance_conditions: vec![format!("{goal} is isolated")],
        artifacts: vec![],
        interfaces: vec![],
        unknowns: vec![],
    }
}

fn locator(id: &str) -> ExternalSessionLocator {
    ExternalSessionLocator::new("codex", id).expect("valid locator")
}

fn signal(kind: TaskSignalKind, content: &str) -> TaskSignal {
    TaskSignal {
        kind,
        content: content.to_owned(),
    }
}

fn intent_draft(goal: &str) -> TaskIntentDraft {
    TaskIntentDraft::from(&intent(TaskId::new(), goal))
}

#[test]
fn initializes_only_the_home_local_runtime_database() {
    let home = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize_for_home(home.path()).unwrap();

    assert_eq!(runtime.root(), home.path().join(".shared-context"));
    assert_eq!(
        runtime.database_path(),
        home.path().join(".shared-context/state/runtime.sqlite")
    );
    assert!(runtime.database_path().is_file());
    assert!(!runtime.root().join("repository").exists());
    assert!(!runtime.state().join("index.sqlite").exists());
}

#[test]
fn open_or_create_is_atomic_and_keeps_one_initial_revision() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let task_id = TaskId::new();
    let initial_intent = intent(task_id, "initial task");
    let initial_signals = vec![signal(TaskSignalKind::Prompt, "Implement runtime")];

    let created = runtime
        .open_or_create(
            locator("session-a"),
            initial_intent.clone(),
            initial_signals.clone(),
        )
        .unwrap();
    let reopened = runtime
        .open_or_create(locator("session-a"), initial_intent, initial_signals)
        .unwrap();

    assert!(created.created);
    assert!(!reopened.created);
    assert_eq!(created.snapshot, reopened.snapshot);
    assert_eq!(created.snapshot.intent_revisions.len(), 1);
    assert_eq!(
        created.snapshot.intent_revisions[0].parent_revision_id,
        None
    );
}

#[test]
fn concurrent_open_or_create_returns_one_shared_session() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let worker_count = 8;
    let barrier = Arc::new(Barrier::new(worker_count));
    let task_id = TaskId::new();
    let mut workers = Vec::new();

    for _ in 0..worker_count {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            runtime
                .open_or_create(
                    locator("shared-session"),
                    intent(task_id, "shared task"),
                    vec![],
                )
                .unwrap()
        }));
    }

    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let first_session_id = outcomes[0].snapshot.task_session_id;
    assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.snapshot.task_session_id == first_session_id)
    );
}

#[test]
fn intent_append_advances_one_linear_head_and_rejects_cross_task_parents() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let first = runtime
        .open_or_create(
            locator("session-a"),
            intent(TaskId::new(), "first task"),
            vec![],
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .open_or_create(
            locator("session-b"),
            intent(TaskId::new(), "second task"),
            vec![],
        )
        .unwrap()
        .snapshot;
    let first_parent = first.intent_revisions[0].revision_id;
    let second_parent = second.intent_revisions[0].revision_id;

    let revision = runtime
        .append_intent_revision(
            first.task_session_id,
            first_parent,
            intent(first.task_id, "refined first task"),
        )
        .unwrap();
    let snapshot = runtime
        .read_snapshot(first.task_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.intent_revisions.len(), 2);
    assert_eq!(revision.parent_revision_id, Some(first_parent));
    assert_eq!(
        snapshot.current_intent_revision().unwrap().revision_id,
        revision.revision_id
    );

    let stale = runtime
        .append_intent_revision(
            first.task_session_id,
            first_parent,
            intent(first.task_id, "stale update"),
        )
        .unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::InvalidInput);
    assert!(stale.message().contains("current Head"));

    let cross_parent = runtime
        .append_intent_revision(
            first.task_session_id,
            second_parent,
            intent(first.task_id, "cross-task parent"),
        )
        .unwrap_err();
    assert_eq!(cross_parent.kind(), ErrorKind::InvalidInput);
    assert!(cross_parent.message().contains("another Task Session"));

    let mixed_task = runtime
        .append_intent_revision(
            first.task_session_id,
            revision.revision_id,
            intent(second.task_id, "mixed task"),
        )
        .unwrap_err();
    assert_eq!(mixed_task.kind(), ErrorKind::InvalidInput);
    assert!(mixed_task.message().contains("Session task"));
}

#[test]
fn signals_are_normalized_and_merged_without_duplicates() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let session = runtime
        .open_or_create(
            locator("session-a"),
            intent(TaskId::new(), "normalize signals"),
            vec![signal(TaskSignalKind::File, "  src/search.tsx  ")],
        )
        .unwrap()
        .snapshot;

    let outcome = runtime
        .merge_signals(
            session.task_session_id,
            vec![
                signal(TaskSignalKind::File, "src/search.tsx"),
                signal(TaskSignalKind::File, " src/search.tsx "),
                signal(TaskSignalKind::Test, " compatibility passes "),
            ],
        )
        .unwrap();

    assert_eq!(outcome.inserted, 1);
    assert_eq!(outcome.snapshot.task_signals.len(), 2);
    assert!(
        outcome
            .snapshot
            .task_signals
            .contains(&signal(TaskSignalKind::File, "src/search.tsx"))
    );
    assert!(
        outcome
            .snapshot
            .task_signals
            .contains(&signal(TaskSignalKind::Test, "compatibility passes"))
    );
}

#[test]
fn locator_merge_updates_only_an_existing_session_and_never_creates_one() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    assert!(
        runtime
            .merge_signals_by_locator(
                &locator("missing-session"),
                vec![signal(TaskSignalKind::File, "src/missing.rs")],
            )
            .unwrap()
            .is_none()
    );

    let session = runtime
        .open_or_create(
            locator("session-a"),
            intent(TaskId::new(), "locator merge"),
            vec![signal(TaskSignalKind::Prompt, "locator merge")],
        )
        .unwrap()
        .snapshot;
    let outcome = runtime
        .merge_signals_by_locator(
            &locator("session-a"),
            vec![
                signal(TaskSignalKind::File, " src/search.rs "),
                signal(TaskSignalKind::Test, "SearchContractTest succeeded"),
            ],
        )
        .unwrap()
        .unwrap();

    assert_eq!(outcome.snapshot.task_session_id, session.task_session_id);
    assert_eq!(outcome.inserted, 2);
    assert!(
        outcome
            .snapshot
            .task_signals
            .contains(&signal(TaskSignalKind::File, "src/search.rs"))
    );
    assert!(outcome.snapshot.task_signals.contains(&signal(
        TaskSignalKind::Test,
        "SearchContractTest succeeded"
    )));
    assert_eq!(
        runtime
            .read_snapshot_by_locator(&locator("session-a"))
            .unwrap()
            .unwrap(),
        outcome.snapshot
    );
}

#[test]
fn same_workspace_signal_does_not_join_external_sessions() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let workspace = signal(TaskSignalKind::Workspace, "/work/shared");
    let first = runtime
        .open_or_create(
            locator("session-a"),
            intent(TaskId::new(), "frontend task"),
            vec![workspace.clone()],
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .open_or_create(
            locator("session-b"),
            intent(TaskId::new(), "backend task"),
            vec![workspace],
        )
        .unwrap()
        .snapshot;

    runtime
        .merge_signals(
            first.task_session_id,
            vec![signal(TaskSignalKind::File, "web/Search.tsx")],
        )
        .unwrap();
    runtime
        .merge_signals(
            second.task_session_id,
            vec![signal(TaskSignalKind::Api, "search-v2")],
        )
        .unwrap();
    let first = runtime
        .read_snapshot(first.task_session_id)
        .unwrap()
        .unwrap();
    let second = runtime
        .read_snapshot(second.task_session_id)
        .unwrap()
        .unwrap();

    assert_ne!(first.task_session_id, second.task_session_id);
    assert_ne!(first.task_id, second.task_id);
    assert!(
        first
            .task_signals
            .iter()
            .any(|value| value.kind == TaskSignalKind::File)
    );
    assert!(
        !first
            .task_signals
            .iter()
            .any(|value| value.kind == TaskSignalKind::Api)
    );
    assert!(
        second
            .task_signals
            .iter()
            .any(|value| value.kind == TaskSignalKind::Api)
    );
    assert!(
        !second
            .task_signals
            .iter()
            .any(|value| value.kind == TaskSignalKind::File)
    );
}

#[test]
fn concurrent_same_session_updates_preserve_signals_and_one_intent_head() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let session = runtime
        .open_or_create(
            locator("session-a"),
            intent(TaskId::new(), "concurrent task"),
            vec![],
        )
        .unwrap()
        .snapshot;
    let parent = session.intent_revisions[0].revision_id;
    let worker_count = 8;
    let barrier = Arc::new(Barrier::new(worker_count));
    let mut workers = Vec::new();

    for index in 0..worker_count {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            runtime
                .merge_signals(
                    session.task_session_id,
                    vec![signal(
                        TaskSignalKind::File,
                        &format!("src/file-{index}.rs"),
                    )],
                )
                .unwrap();
            runtime.append_intent_revision(
                session.task_session_id,
                parent,
                intent(session.task_id, &format!("intent-{index}")),
            )
        }));
    }

    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let mut intent_successes = 0;
    for outcome in outcomes {
        match outcome {
            Ok(_) => intent_successes += 1,
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::InvalidInput);
                assert!(error.message().contains("current Head"));
            }
        }
    }
    let snapshot = runtime
        .read_snapshot(session.task_session_id)
        .unwrap()
        .unwrap();

    assert_eq!(intent_successes, 1);
    assert_eq!(snapshot.intent_revisions.len(), 2);
    assert_eq!(snapshot.task_signals.len(), worker_count);
    assert!(snapshot.validate().is_ok());
}

#[test]
fn explicit_new_task_boundary_keeps_history_and_excludes_old_signals_from_active_snapshot() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let external_locator = locator("multi-task-session");
    let first = runtime
        .open_or_create(
            external_locator.clone(),
            intent(TaskId::new(), "alpha requirement"),
            vec![
                signal(TaskSignalKind::Prompt, "alpha requirement"),
                signal(TaskSignalKind::File, "src/alpha.rs"),
            ],
        )
        .unwrap()
        .snapshot;

    let second = runtime
        .start_new_task(
            &external_locator,
            first.task_id,
            &intent_draft("banana requirement"),
            vec![signal(TaskSignalKind::Prompt, "banana requirement")],
        )
        .unwrap()
        .snapshot;

    assert_ne!(first.task_id, second.task_id);
    assert_ne!(first.task_session_id, second.task_session_id);
    assert_eq!(
        second.task_signals,
        vec![signal(TaskSignalKind::Prompt, "banana requirement")]
    );
    assert!(
        second
            .task_signals
            .iter()
            .all(|value| !value.content.contains("alpha"))
    );
    assert_eq!(
        runtime
            .read_snapshot_by_locator(&external_locator)
            .unwrap()
            .unwrap(),
        second
    );
    assert_eq!(
        runtime
            .read_snapshot(first.task_session_id)
            .unwrap()
            .unwrap(),
        first
    );
    let external = runtime
        .read_external_session_by_locator(&external_locator)
        .unwrap()
        .unwrap();
    assert_eq!(external.tasks.len(), 2);
    assert_eq!(external.active_task_id, second.task_id);
    assert_eq!(external.active_task(), Some(&second));

    let error = runtime
        .merge_signals(
            first.task_session_id,
            vec![signal(TaskSignalKind::Test, "old task test")],
        )
        .unwrap_err();
    assert!(error.message().contains("ActiveTask"));
}

#[test]
fn explicit_switch_restores_only_the_target_tasks_own_active_signals() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let external_locator = locator("switch-session");
    let first = runtime
        .open_or_create(
            external_locator.clone(),
            intent(TaskId::new(), "first task"),
            vec![signal(TaskSignalKind::File, "src/first.rs")],
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .start_new_task(
            &external_locator,
            first.task_id,
            &intent_draft("second task"),
            vec![signal(TaskSignalKind::Api, "second-v2")],
        )
        .unwrap()
        .snapshot;

    let switched = runtime
        .switch_active_task(&external_locator, second.task_id, first.task_id)
        .unwrap();

    assert!(switched.switched);
    assert_eq!(switched.snapshot.task_id, first.task_id);
    assert_eq!(switched.snapshot.task_signals, first.task_signals);
    assert!(
        switched
            .snapshot
            .task_signals
            .iter()
            .all(|value| value.kind != TaskSignalKind::Api)
    );
    assert_eq!(
        runtime
            .read_snapshot(second.task_session_id)
            .unwrap()
            .unwrap()
            .task_signals,
        second.task_signals
    );
}

#[test]
fn signal_supersede_hides_active_input_but_retains_queryable_history_and_identity() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let session = runtime
        .open_or_create(
            locator("signal-lifecycle"),
            intent(TaskId::new(), "signal lifecycle"),
            vec![
                signal(TaskSignalKind::Prompt, "signal lifecycle"),
                signal(TaskSignalKind::File, "src/obsolete.rs"),
            ],
        )
        .unwrap()
        .snapshot;
    let history = runtime
        .read_signal_history(session.task_session_id)
        .unwrap();
    let obsolete = history
        .iter()
        .find(|record| record.signal.kind == TaskSignalKind::File)
        .unwrap();

    let superseded = runtime
        .supersede_signals(
            session.task_session_id,
            session.task_id,
            vec![obsolete.signal_id],
        )
        .unwrap();
    assert!(
        superseded
            .snapshot
            .task_signals
            .iter()
            .all(|value| value.kind != TaskSignalKind::File)
    );
    let retained = runtime
        .read_signal_history(session.task_session_id)
        .unwrap();
    assert_eq!(retained.len(), 2);
    let retained_obsolete = retained
        .iter()
        .find(|record| record.signal_id == obsolete.signal_id)
        .unwrap();
    assert_eq!(retained_obsolete.lifecycle, TaskSignalLifecycle::Superseded);

    let readded = runtime
        .merge_signals(
            session.task_session_id,
            vec![signal(TaskSignalKind::File, "src/obsolete.rs")],
        )
        .unwrap();
    assert_eq!(readded.inserted, 1);
    assert_ne!(readded.inserted_signal_ids[0], obsolete.signal_id);
    let final_history = runtime
        .read_signal_history(session.task_session_id)
        .unwrap();
    assert_eq!(final_history.len(), 3);
    assert_eq!(
        final_history
            .iter()
            .filter(|record| record.lifecycle == TaskSignalLifecycle::Active)
            .count(),
        2
    );
}

#[test]
fn concurrent_new_task_cas_creates_exactly_one_history_entry() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let external_locator = locator("task-cas");
    let initial = runtime
        .open_or_create(
            external_locator.clone(),
            intent(TaskId::new(), "initial task"),
            vec![],
        )
        .unwrap()
        .snapshot;
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for goal in ["competing task a", "competing task b"] {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        let external_locator = external_locator.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            runtime.start_new_task(
                &external_locator,
                initial.task_id,
                &intent_draft(goal),
                vec![signal(TaskSignalKind::Prompt, goal)],
            )
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|value| value.is_ok()).count(), 1);
    assert!(
        outcomes
            .iter()
            .filter_map(|value| value.as_ref().err())
            .all(|error| error.kind() == ErrorKind::InvalidInput
                && error.message().contains("expected_active_task_id"))
    );
    let external = runtime
        .read_external_session_by_locator(&external_locator)
        .unwrap()
        .unwrap();
    assert_eq!(external.tasks.len(), 2);
    assert_ne!(external.active_task_id, initial.task_id);
    assert!(external.validate().is_ok());
}

#[test]
fn invalid_locator_is_rejected_before_any_session_is_created() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let error = runtime
        .open_or_create(
            ExternalSessionLocator {
                agent_kind: " ".to_owned(),
                external_session_id: "session-a".to_owned(),
            },
            intent(TaskId::new(), "invalid locator"),
            vec![],
        )
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.message().contains("agent_kind"));
}

#[test]
fn deleting_runtime_database_loses_sessions_without_touching_knowledge_files() {
    let root = TempDir::new().unwrap();
    let repository = root.path().join("repository");
    let state = root.path().join("state");
    fs::create_dir_all(&repository).unwrap();
    fs::create_dir_all(&state).unwrap();
    let knowledge = repository.join("knowledge.fact");
    let index = state.join("index.sqlite");
    fs::write(&knowledge, b"durable knowledge").unwrap();
    fs::write(&index, b"knowledge projection").unwrap();

    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let session_id = runtime
        .open_or_create(
            locator("session-a"),
            intent(TaskId::new(), "disposable task"),
            vec![],
        )
        .unwrap()
        .snapshot
        .task_session_id;
    fs::remove_file(runtime.database_path()).unwrap();

    let recreated = TaskRuntime::initialize(root.path()).unwrap();
    assert!(recreated.read_snapshot(session_id).unwrap().is_none());
    assert_eq!(fs::read(knowledge).unwrap(), b"durable knowledge");
    assert_eq!(fs::read(index).unwrap(), b"knowledge projection");
}
