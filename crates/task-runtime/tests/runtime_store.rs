use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use rusqlite::Connection;
use sctx_domain::{
    ErrorKind, ExternalSessionLocator, TaskId, TaskSignal, TaskSignalKind, TaskSignalLifecycle,
    WorkingIntentSnapshot,
};
use sctx_task_runtime::{IntentRevisionWriteStatus, TaskRuntime};
use tempfile::TempDir;

fn intent(_task_id: TaskId, goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: Some(format!("Deliver {goal}")),
        in_scope: vec![goal.to_owned()],
        out_of_scope: vec![],
        domains: vec!["task-runtime".to_owned()],
        platforms: vec![],
        constraints: vec!["No Space route".to_owned()],
        acceptance_conditions: vec![format!("{goal} is isolated")],
        artifact_hints: vec![],
        interface_hints: vec![],
        open_questions: vec![],
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

fn intent_draft(goal: &str) -> WorkingIntentSnapshot {
    intent(TaskId::new(), goal)
}

fn working(goal: &str) -> WorkingIntentSnapshot {
    intent_draft(goal)
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
            task_id,
            initial_intent.clone(),
            initial_signals.clone(),
        )
        .unwrap();
    let reopened = runtime
        .open_or_create(
            locator("session-a"),
            task_id,
            initial_intent,
            initial_signals,
        )
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
fn twenty_concurrent_identical_initial_requests_return_one_task_and_revision() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let worker_count = 20;
    let barrier = Arc::new(Barrier::new(worker_count));
    let mut workers = Vec::new();

    for _ in 0..worker_count {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            runtime
                .open_or_create(
                    locator("shared-session"),
                    TaskId::new(),
                    intent(TaskId::new(), "shared task"),
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
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.snapshot.intent_revisions.len() == 1)
    );
}

#[test]
fn concurrent_divergent_initial_requests_choose_one_and_reject_every_other_without_overwrite() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let barrier = Arc::new(Barrier::new(20));
    let outcomes = (0..20)
        .map(|index| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let goal = format!("divergent authoritative goal {index}");
                let result = runtime.open_or_create(
                    locator("divergent-session"),
                    TaskId::new(),
                    working(&goal),
                    Vec::new(),
                );
                (goal, result)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert!(
        outcomes
            .iter()
            .filter_map(|(_, result)| result.as_ref().err())
            .all(|error| error.kind() == ErrorKind::Conflict
                && error.message().contains("different Working Intent"))
    );
    let (winning_goal, winning) = outcomes
        .iter()
        .find_map(|(goal, result)| result.as_ref().ok().map(|value| (goal, value)))
        .unwrap();
    assert!(winning.created);
    let persisted = runtime
        .read_snapshot_by_locator(&locator("divergent-session"))
        .unwrap()
        .unwrap();
    assert_eq!(persisted.task_id, winning.snapshot.task_id);
    assert_eq!(persisted.intent_revisions.len(), 1);
    assert_eq!(
        &persisted.intent_revisions[0].working_intent.goal, winning_goal,
        "losing divergent requests must not overwrite authoritative text"
    );
}

#[test]
fn sequential_existing_locator_with_different_initial_intent_is_rejected_without_write() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let created = runtime
        .open_or_create(
            locator("sequential-divergent"),
            TaskId::new(),
            working("first authoritative text"),
            Vec::new(),
        )
        .unwrap();
    let error = runtime
        .open_or_create(
            locator("sequential-divergent"),
            TaskId::new(),
            working("different authoritative text"),
            Vec::new(),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    let persisted = runtime
        .read_snapshot(created.snapshot.task_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.intent_revisions.len(), 1);
    assert_eq!(
        persisted.intent_revisions[0].working_intent.goal,
        "first authoritative text"
    );
}

#[test]
fn intent_append_advances_one_linear_head_and_rejects_cross_session_parents() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let first = runtime
        .open_or_create(
            locator("session-a"),
            TaskId::new(),
            intent(TaskId::new(), "first task"),
            vec![],
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .open_or_create(
            locator("session-b"),
            TaskId::new(),
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
    assert_eq!(revision.revision.parent_revision_id, Some(first_parent));
    assert_eq!(
        snapshot.current_intent_revision().unwrap().revision_id,
        revision.revision.revision_id
    );

    let stale = runtime
        .append_intent_revision(
            first.task_session_id,
            first_parent,
            intent(first.task_id, "stale update"),
        )
        .unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::StaleState);
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
}

#[test]
#[allow(clippy::too_many_lines)]
fn working_intent_continue_is_semantically_idempotent_and_stale_changes_write_nothing() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let task_id = TaskId::new();
    let initial_working = WorkingIntentSnapshot {
        goal: "Implement Search Result".to_owned(),
        current_direction: Some("Use the existing API".to_owned()),
        in_scope: vec!["Frontend".to_owned(), "Contract".to_owned()],
        out_of_scope: Vec::new(),
        domains: vec!["Search".to_owned()],
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    };
    let session = runtime
        .open_or_create(
            locator("working-idempotent"),
            task_id,
            initial_working.clone(),
            Vec::new(),
        )
        .unwrap()
        .snapshot;
    let parent = session.current_intent_revision().unwrap().revision_id;
    let connection = Connection::open(runtime.database_path()).unwrap();
    let columns = connection
        .prepare("PRAGMA table_info(task_intent_revision)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(columns.contains(&"authority_json".to_owned()));
    assert!(columns.contains(&"semantic_hash".to_owned()));
    for forbidden in [
        "task_id",
        "intent_json",
        "maturity",
        "evidence",
        "evidence_refs",
    ] {
        assert!(!columns.iter().any(|column| column == forbidden));
    }
    let authority: String = connection
        .query_row(
            "SELECT authority_json FROM task_intent_revision WHERE revision_id = ?1",
            [parent.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<WorkingIntentSnapshot>(&authority).unwrap(),
        initial_working
    );
    drop(connection);
    let mut equivalent = initial_working.clone();
    equivalent.goal = "  implement   SEARCH result  ".to_owned();
    equivalent.current_direction = Some("use THE existing api".to_owned());
    equivalent.in_scope.reverse();
    let already = runtime
        .append_intent_revision(session.task_session_id, parent, equivalent)
        .unwrap();
    assert_eq!(already.status, IntentRevisionWriteStatus::AlreadyCurrent);
    assert_eq!(already.revision.revision_id, parent);
    assert_eq!(
        runtime
            .read_snapshot(session.task_session_id)
            .unwrap()
            .unwrap()
            .intent_revisions
            .len(),
        1
    );

    let mut changed = initial_working.clone();
    changed.current_direction = Some("Adopt the v2 API".to_owned());
    let created = runtime
        .append_intent_revision(session.task_session_id, parent, changed.clone())
        .unwrap();
    assert_eq!(created.status, IntentRevisionWriteStatus::Created);
    assert_ne!(created.revision.revision_id, parent);
    let retry = runtime
        .append_intent_revision(session.task_session_id, parent, changed)
        .unwrap();
    assert_eq!(retry.status, IntentRevisionWriteStatus::AlreadyCurrent);
    assert_eq!(retry.revision.revision_id, created.revision.revision_id);

    let mut stale_change = initial_working.clone();
    stale_change.current_direction = Some("Choose a third implementation".to_owned());
    let stale = runtime
        .append_intent_revision(session.task_session_id, parent, stale_change)
        .unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::StaleState);
    let persisted = runtime
        .read_snapshot(session.task_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.intent_revisions.len(), 2);
    assert_eq!(
        persisted.current_intent_revision().unwrap().revision_id,
        created.revision.revision_id
    );
    assert_eq!(
        persisted.intent_revisions[0].working_intent, initial_working,
        "authoritative text must not be rewritten by canonical comparison"
    );
}

#[test]
fn twenty_concurrent_identical_working_intent_continues_converge_to_one_revision() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let task_id = TaskId::new();
    let session = runtime
        .open_or_create(
            locator("working-concurrent"),
            task_id,
            working("initial Working Intent"),
            Vec::new(),
        )
        .unwrap()
        .snapshot;
    let parent = session.current_intent_revision().unwrap().revision_id;
    let next = working("one shared successor");
    let barrier = Arc::new(Barrier::new(20));
    let outcomes = (0..20)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let next = next.clone();
            thread::spawn(move || {
                barrier.wait();
                runtime
                    .append_intent_revision(session.task_session_id, parent, next)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.status == IntentRevisionWriteStatus::Created)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.revision.revision_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        runtime
            .read_snapshot(session.task_session_id)
            .unwrap()
            .unwrap()
            .intent_revisions
            .len(),
        2
    );
}

#[test]
fn signals_are_normalized_and_merged_without_duplicates() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let session = runtime
        .open_or_create(
            locator("session-a"),
            TaskId::new(),
            intent(TaskId::new(), "normalize signals"),
            vec![signal(TaskSignalKind::Diff, "  src/search.tsx  ")],
        )
        .unwrap()
        .snapshot;

    let outcome = runtime
        .merge_signals(
            session.task_session_id,
            vec![
                signal(TaskSignalKind::Diff, "src/search.tsx"),
                signal(TaskSignalKind::Diff, " src/search.tsx "),
                signal(TaskSignalKind::TestOutcome, " compatibility passes "),
            ],
        )
        .unwrap();

    assert_eq!(outcome.inserted, 1);
    assert_eq!(outcome.snapshot.task_signals.len(), 2);
    assert!(
        outcome
            .snapshot
            .task_signals
            .contains(&signal(TaskSignalKind::Diff, "src/search.tsx"))
    );
    assert!(
        outcome
            .snapshot
            .task_signals
            .contains(&signal(TaskSignalKind::TestOutcome, "compatibility passes"))
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
                vec![signal(TaskSignalKind::Diff, "src/missing.rs")],
            )
            .unwrap()
            .is_none()
    );

    let session = runtime
        .open_or_create(
            locator("session-a"),
            TaskId::new(),
            intent(TaskId::new(), "locator merge"),
            vec![signal(TaskSignalKind::Prompt, "locator merge")],
        )
        .unwrap()
        .snapshot;
    let outcome = runtime
        .merge_signals_by_locator(
            &locator("session-a"),
            vec![
                signal(TaskSignalKind::Diff, " src/search.rs "),
                signal(TaskSignalKind::TestOutcome, "SearchContractTest succeeded"),
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
            .contains(&signal(TaskSignalKind::Diff, "src/search.rs"))
    );
    assert!(outcome.snapshot.task_signals.contains(&signal(
        TaskSignalKind::TestOutcome,
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
            TaskId::new(),
            intent(TaskId::new(), "frontend task"),
            vec![workspace.clone()],
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .open_or_create(
            locator("session-b"),
            TaskId::new(),
            intent(TaskId::new(), "backend task"),
            vec![workspace],
        )
        .unwrap()
        .snapshot;

    runtime
        .merge_signals(
            first.task_session_id,
            vec![signal(TaskSignalKind::Diff, "web/Search.tsx")],
        )
        .unwrap();
    runtime
        .merge_signals(
            second.task_session_id,
            vec![signal(TaskSignalKind::Diff, "search-v2")],
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
            .any(|value| value.content == "web/Search.tsx")
    );
    assert!(
        !first
            .task_signals
            .iter()
            .any(|value| value.content == "search-v2")
    );
    assert!(
        second
            .task_signals
            .iter()
            .any(|value| value.content == "search-v2")
    );
    assert!(
        !second
            .task_signals
            .iter()
            .any(|value| value.content == "web/Search.tsx")
    );
}

#[test]
fn concurrent_same_session_updates_preserve_signals_and_one_intent_head() {
    let root = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(root.path()).unwrap());
    let session = runtime
        .open_or_create(
            locator("session-a"),
            TaskId::new(),
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
                        TaskSignalKind::Diff,
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
                assert_eq!(error.kind(), ErrorKind::StaleState);
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
            TaskId::new(),
            intent(TaskId::new(), "alpha requirement"),
            vec![
                signal(TaskSignalKind::Prompt, "alpha requirement"),
                signal(TaskSignalKind::Diff, "src/alpha.rs"),
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
            vec![signal(TaskSignalKind::TestOutcome, "old task test")],
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
            TaskId::new(),
            intent(TaskId::new(), "first task"),
            vec![signal(TaskSignalKind::Diff, "src/first.rs")],
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .start_new_task(
            &external_locator,
            first.task_id,
            &intent_draft("second task"),
            vec![signal(TaskSignalKind::Diff, "second-v2")],
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
            .all(|value| value.content != "second-v2")
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
            TaskId::new(),
            intent(TaskId::new(), "signal lifecycle"),
            vec![
                signal(TaskSignalKind::Prompt, "signal lifecycle"),
                signal(TaskSignalKind::Diff, "src/obsolete.rs"),
            ],
        )
        .unwrap()
        .snapshot;
    let history = runtime
        .read_signal_history(session.task_session_id)
        .unwrap();
    let obsolete = history
        .iter()
        .find(|record| record.signal.kind == TaskSignalKind::Diff)
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
            .all(|value| value.kind != TaskSignalKind::Diff)
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
            vec![signal(TaskSignalKind::Diff, "src/obsolete.rs")],
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
            TaskId::new(),
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
fn explicit_new_with_identical_working_intent_always_creates_a_distinct_task() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let external_locator = locator("identical-new-tasks");
    let same = working("identical explicit new content");
    let first = runtime
        .open_or_create(
            external_locator.clone(),
            TaskId::new(),
            same.clone(),
            Vec::new(),
        )
        .unwrap()
        .snapshot;
    let second = runtime
        .start_new_task(&external_locator, first.task_id, &same, Vec::new())
        .unwrap()
        .snapshot;
    assert_ne!(first.task_id, second.task_id);
    assert_ne!(first.task_session_id, second.task_session_id);
    let history = runtime
        .read_external_session_by_locator(&external_locator)
        .unwrap()
        .unwrap();
    assert_eq!(history.tasks.len(), 2);
    assert_eq!(history.active_task_id, second.task_id);
    assert_eq!(
        history.tasks[0].intent_revisions[0].semantic_hash,
        history.tasks[1].intent_revisions[0].semantic_hash
    );
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
            TaskId::new(),
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
            TaskId::new(),
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
