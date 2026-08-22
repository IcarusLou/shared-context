use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{
    CaptureId, ExternalSessionLocator, NormalizedBreadcrumbKind, NormalizedWorkObservation, TaskId,
    TaskIntent, TaskIntentDraft, TaskSignal, TaskSignalKind, TestOutcomeStatus, WorkEpisodeStatus,
    WorkSourceRef,
};
use sctx_task_runtime::{CaptureIngestion, TaskRuntime, WorkEpisodeDiagnosticKind};
use tempfile::TempDir;

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

fn open_task(
    runtime: &TaskRuntime,
    session: &str,
    goal: &str,
) -> (ExternalSessionLocator, sctx_domain::TaskSessionSnapshot) {
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let task_id = TaskId::new();
    let snapshot = runtime
        .open_or_create(
            locator.clone(),
            intent(task_id, goal),
            vec![TaskSignal {
                kind: TaskSignalKind::Workspace,
                content: "/workspace/shared".to_owned(),
            }],
        )
        .unwrap()
        .snapshot;
    (locator, snapshot)
}

fn breadcrumb(summary: &str) -> NormalizedWorkObservation {
    NormalizedWorkObservation::Breadcrumb {
        category: NormalizedBreadcrumbKind::Exploration,
        summary: summary.to_owned(),
    }
}

fn draft(goal: &str) -> TaskIntentDraft {
    TaskIntentDraft {
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

#[test]
fn same_workspace_sessions_have_isolated_episodes_and_concurrent_open_converges() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (first_locator, first) = open_task(&runtime, "episode-a", "first task");
    let (second_locator, second) = open_task(&runtime, "episode-b", "second task");
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut threads = Vec::new();
    for _ in 0..workers {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        let locator = first_locator.clone();
        let task_id = first.task_id;
        let revision_id = first.current_intent_revision().unwrap().revision_id;
        threads.push(thread::spawn(move || {
            barrier.wait();
            runtime
                .open_work_episode(&locator, task_id, revision_id)
                .unwrap()
        }));
    }
    let outcomes = threads
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    let episode_ids = outcomes
        .iter()
        .map(|outcome| outcome.episode.episode.episode_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(episode_ids.len(), 1);
    assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);

    let second_episode = runtime
        .open_work_episode(
            &second_locator,
            second.task_id,
            second.current_intent_revision().unwrap().revision_id,
        )
        .unwrap();
    assert_ne!(
        outcomes[0].episode.episode.episode_id,
        second_episode.episode.episode.episode_id
    );
    assert_ne!(first.task_session_id, second.task_session_id);
    assert_eq!(
        runtime
            .list_work_episodes(first.task_session_id, 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn intent_and_signal_refs_advance_only_through_explicit_episode_api() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let (locator, initial) = open_task(&runtime, "episode-refs", "initial");
    let opened = runtime
        .open_work_episode(
            &locator,
            initial.task_id,
            initial.current_intent_revision().unwrap().revision_id,
        )
        .unwrap()
        .episode;
    assert_eq!(opened.episode.version, 0);
    assert_eq!(opened.episode.intent_revisions.revision_ids.len(), 1);
    assert_eq!(opened.episode.signal_refs.len(), 1);

    let current = initial.current_intent_revision().unwrap().revision_id;
    let revision = runtime
        .append_intent_revision(
            initial.task_session_id,
            current,
            intent(initial.task_id, "revised"),
        )
        .unwrap();
    let merge = runtime
        .merge_signals(
            initial.task_session_id,
            vec![TaskSignal {
                kind: TaskSignalKind::TestOutcome,
                content: "ContractTest passed".to_owned(),
            }],
        )
        .unwrap();
    assert_eq!(merge.inserted, 1);
    let before = runtime
        .read_work_episode(opened.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(before.episode.intent_revisions.revision_ids.len(), 1);
    assert_eq!(before.episode.signal_refs.len(), 1);

    let advanced = runtime
        .advance_work_episode_refs(opened.episode.episode_id, 0)
        .unwrap();
    assert_eq!(advanced.added_intent_revisions, 1);
    assert_eq!(advanced.added_signal_refs, 1);
    assert_eq!(advanced.episode.episode.version, 1);
    assert_eq!(
        advanced.episode.episode.intent_revisions.last(),
        revision.revision_id
    );
    assert!(
        advanced
            .episode
            .episode
            .signal_refs
            .iter()
            .any(|signal| signal.signal_id == merge.inserted_signal_ids[0])
    );
    assert!(
        runtime
            .advance_work_episode_refs(opened.episode.episode_id, 0)
            .is_err()
    );
    let idempotent = runtime
        .advance_work_episode_refs(opened.episode.episode_id, 1)
        .unwrap();
    assert_eq!(idempotent.added_intent_revisions, 0);
    assert_eq!(idempotent.added_signal_refs, 0);
    assert_eq!(idempotent.episode.episode.version, 1);
    let test_signal = idempotent
        .episode
        .episode
        .signal_refs
        .iter()
        .find(|signal| signal.signal_id == merge.inserted_signal_ids[0])
        .copied()
        .unwrap();
    let appended = runtime
        .append_work_observation(
            opened.episode.episode_id,
            1,
            revision.revision_id,
            vec![WorkSourceRef::TaskSignal(test_signal)],
            NormalizedWorkObservation::TestOutcome {
                test_name: "ContractTest".to_owned(),
                status: TestOutcomeStatus::Passed,
                summary: "contract validation passed".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(appended.episode.episode.version, 2);
    assert!(matches!(
        appended.episode.episode.observations[0].observation,
        NormalizedWorkObservation::TestOutcome { .. }
    ));
    runtime
        .start_new_task(
            &locator,
            initial.task_id,
            &draft("replacement task"),
            Vec::new(),
        )
        .unwrap();
    assert!(
        runtime
            .advance_work_episode_refs(opened.episode.episode_id, 2)
            .is_err(),
        "inactive historical Task Episode must not accept writes"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn concurrent_append_is_version_guarded_and_capture_ingestion_is_idempotent() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (locator, task) = open_task(&runtime, "episode-ingest", "capture work");
    let opened = runtime
        .open_work_episode(
            &locator,
            task.task_id,
            task.current_intent_revision().unwrap().revision_id,
        )
        .unwrap()
        .episode;
    let signal_source = WorkSourceRef::TaskSignal(opened.episode.signal_refs[0]);
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut threads = Vec::new();
    for index in 0..workers {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        let source = signal_source.clone();
        let episode_id = opened.episode.episode_id;
        let intent_revision_id = opened.episode.intent_revisions.last();
        threads.push(thread::spawn(move || {
            barrier.wait();
            runtime.append_work_observation(
                episode_id,
                0,
                intent_revision_id,
                vec![source],
                breadcrumb(&format!("concurrent observation {index}")),
            )
        }));
    }
    let outcomes = threads
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "concurrent append errors: {:?}",
        outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().err().map(sctx_domain::Error::message))
            .collect::<Vec<_>>()
    );
    let episode = runtime
        .read_work_episode(opened.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(episode.episode.version, 1);
    assert_eq!(episode.episode.observations.len(), 1);

    let capture_id = CaptureId::new();
    let input = CaptureIngestion {
        capture_id,
        episode_id: episode.episode.episode_id,
        expected_episode_version: 1,
        task_session_id: task.task_session_id,
        task_id: task.task_id,
        intent_revision_id: episode.episode.intent_revisions.last(),
        additional_sources: Vec::new(),
        observation: breadcrumb("claimed Capture meaning"),
        diagnostics: vec![WorkEpisodeDiagnosticKind::CaptureRepositoryNotConfigured],
    };
    let barrier = Arc::new(Barrier::new(workers));
    let mut ingesters = Vec::new();
    for _ in 0..workers {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        let input = input.clone();
        ingesters.push(thread::spawn(move || {
            barrier.wait();
            runtime.ingest_capture(&input).unwrap()
        }));
    }
    let ingested = ingesters
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ingested.iter().filter(|outcome| outcome.inserted).count(),
        1
    );
    assert_eq!(
        ingested
            .iter()
            .map(|outcome| outcome.observation_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    let retried = runtime.ingest_capture(&input).unwrap();
    assert!(!retried.inserted);
    assert_eq!(retried.episode.episode.version, 2);
    assert_eq!(retried.episode.episode.observations.len(), 2);
    assert_eq!(retried.episode.diagnostics.len(), 1);
    let mut conflicting_retry = input.clone();
    conflicting_retry.observation = breadcrumb("different Capture meaning");
    assert!(runtime.ingest_capture(&conflicting_retry).is_err());

    let (_, other_task) = open_task(&runtime, "episode-other", "other owner");
    let mut cross_task = input.clone();
    cross_task.task_session_id = other_task.task_session_id;
    cross_task.task_id = other_task.task_id;
    assert!(runtime.ingest_capture(&cross_task).is_err());
    assert_eq!(
        runtime
            .prepare_work_episode_close(episode.episode.episode_id, 2)
            .unwrap()
            .observation_ids
            .len(),
        2
    );
    let verification = runtime
        .verify_source_episode(episode.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(verification.status, WorkEpisodeStatus::Open);
    assert_eq!(verification.observation_count, 2);
}

#[test]
fn deleting_runtime_loses_episode_only_and_preserves_other_state() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("root");
    let runtime = TaskRuntime::initialize(&root).unwrap();
    let (locator, task) = open_task(&runtime, "episode-delete", "delete runtime");
    let episode_id = runtime
        .open_work_episode(
            &locator,
            task.task_id,
            task.current_intent_revision().unwrap().revision_id,
        )
        .unwrap()
        .episode
        .episode
        .episode_id;
    fs::create_dir_all(root.join("repository")).unwrap();
    fs::write(root.join("repository/fact"), b"git fact").unwrap();
    fs::write(root.join("state/index.sqlite"), b"index").unwrap();
    fs::create_dir_all(root.join("state/capture")).unwrap();
    fs::write(root.join("state/capture/cap-safe.json"), b"capture").unwrap();
    let database = runtime.database_path().to_path_buf();
    drop(runtime);
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", database.display()));
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove runtime database: {error}"),
        }
    }
    let restored = TaskRuntime::initialize(&root).unwrap();
    assert!(
        restored
            .verify_source_episode(episode_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(root.join("repository/fact")).unwrap(), b"git fact");
    assert_eq!(fs::read(root.join("state/index.sqlite")).unwrap(), b"index");
    assert_eq!(
        fs::read(root.join("state/capture/cap-safe.json")).unwrap(),
        b"capture"
    );
}
