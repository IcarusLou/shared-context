use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use rusqlite::Connection;
use sctx_domain::{
    Applicability, AutomaticCandidateStatus, AutomaticContextCandidate, CandidateAnalysis,
    CandidateAnalysisStatus, CandidateAssessmentPath, CandidateAssessmentRelation,
    CandidateBuilderProvenance, CandidateConfidence, CandidateId, CandidateRelationAssessment,
    CandidateReviewStatus, CheckpointEvidenceRef, CheckpointUnknown, ConfirmationId,
    ContextCandidate, ContextId, ContextKind, ContextRevisionDraft, DecisionSource, ErrorKind,
    EventId, EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator,
    NormalizedWorkObservation, TaskId, TaskSignal, TaskSignalKind, TestOutcomeStatus,
    WorkEpisodeStatus, WorkSourceRef, WorkingIntentSnapshot,
};
use sctx_task_runtime::{
    AgentCheckpointSubmission, AgentCheckpointWrite, AutomatedEpisodeBoundary,
    CandidateBuildItemPreparation, CandidateBuildItemStatus, CandidateBuildStatus,
    CandidateReviewDiscard, CandidateReviewDiscardStatus, CandidateReviewSurvey,
    CheckpointBoundary, CheckpointClaimDraft, DEFAULT_CANDIDATE_REVIEW_TTL,
    DirectCheckpointClaimDraft, DirectEvidenceDraft, IntentRevisionWriteStatus,
    MAX_CANDIDATE_REVIEW_TTL, TaskRuntime,
};
use tempfile::TempDir;

fn intent(goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: Some(format!("implement {goal}")),
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
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
            task_id,
            intent(goal),
            vec![TaskSignal {
                kind: TaskSignalKind::Workspace,
                content: "/workspace/shared".to_owned(),
            }],
        )
        .unwrap()
        .snapshot;
    (locator, snapshot)
}

fn validation_observation(summary: &str) -> NormalizedWorkObservation {
    NormalizedWorkObservation::InlineValidation {
        evidence: EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: summary.to_owned(),
            content: serde_json::json!({"summary": summary}),
            interpretation: "The Runtime test produced a normalized Observation".to_owned(),
            limitations: Vec::new(),
        },
    }
}

fn checkpoint_claim(statement: &str) -> CheckpointClaimDraft {
    CheckpointClaimDraft {
        context_kind_hint: None,
        topic_key_hint: None,
        statement: statement.to_owned(),
        rationale: "Direct validation supports this engineering conclusion".to_owned(),
        applicability: Applicability {
            domains: vec!["runtime".to_owned()],
            platforms: Vec::new(),
            conditions: vec!["Agent Checkpoint".to_owned()],
        },
        evidence_refs: Vec::new(),
        inline_validations: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: statement.to_owned(),
            content: serde_json::json!({"test": "checkpoint", "actual": "passed"}),
            interpretation: "The focused runtime behavior was directly validated".to_owned(),
            limitations: Vec::new(),
        }],
        engineering_references: Vec::new(),
    }
}

fn candidate_content(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
        problem_view: None,
        hints: Vec::new(),
        kind: ContextKind::Discovery,
        topic_key: Some("candidate/runtime-analysis".to_owned()),
        statement: statement.to_owned(),
        rationale: "Runtime-derived analysis is replaceable".to_owned(),
        applicability: Applicability::default(),
        assumptions: Vec::new(),
        recheck_when: vec!["the analysis projection changes".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the analysis replacement completed".to_owned(),
            content: serde_json::json!({"actual": "stored"}),
            interpretation: "derived review state is recoverable".to_owned(),
            limitations: Vec::new(),
        }],
    }
}

fn checkpoint_write(
    locator: &ExternalSessionLocator,
    task: &sctx_domain::TaskSessionSnapshot,
    expected_episode_version: u64,
    boundary: CheckpointBoundary,
    claims: Vec<CheckpointClaimDraft>,
    unknowns: Vec<CheckpointUnknown>,
) -> AgentCheckpointWrite {
    AgentCheckpointWrite {
        locator: locator.clone(),
        expected_task_id: task.task_id,
        expected_intent_revision_id: task.current_intent_revision().unwrap().revision_id,
        expected_episode_version,
        boundary,
        claims,
        unknowns,
    }
}

fn direct_submission(
    locator: &ExternalSessionLocator,
    statement: &str,
) -> AgentCheckpointSubmission {
    AgentCheckpointSubmission {
        locator: locator.clone(),
        claims: vec![DirectCheckpointClaimDraft {
            context_kind: ContextKind::Validation,
            statement: statement.to_owned(),
            rationale: "The direct Checkpoint operation is durable".to_owned(),
            conditions: vec!["content addressed".to_owned()],
            evidence: vec![DirectEvidenceDraft {
                evidence_type: EvidenceType::ExperimentRecord,
                summary: format!("{statement} passed"),
                limitations: Vec::new(),
            }],
        }],
        unknowns: Vec::new(),
    }
}

fn finalize_review(
    runtime: &TaskRuntime,
    locator: &ExternalSessionLocator,
    task: &sctx_domain::TaskSessionSnapshot,
    statement: &str,
) -> sctx_task_runtime::CandidateReviewRecord {
    runtime
        .open_work_episode(
            locator,
            task.task_id,
            task.current_intent_revision().unwrap().revision_id,
        )
        .unwrap();
    let closed = runtime
        .write_agent_checkpoint(&checkpoint_write(
            locator,
            task,
            0,
            CheckpointBoundary::Close,
            vec![checkpoint_claim(statement)],
            Vec::new(),
        ))
        .unwrap();
    let build = runtime
        .prepare_candidate_build(
            closed.episode.episode.episode_id,
            &[CandidateBuildItemPreparation {
                checkpoint_id: closed.checkpoint.checkpoint_id,
                claim_id: closed.checkpoint.claims[0].claim_id,
                content_hash: Some(format!("sha256:{statement}")),
                status: CandidateBuildItemStatus::Prepared,
                error_code: None,
            }],
        )
        .unwrap();
    let item = &build.items[0];
    let candidate_id = CandidateId::new();
    runtime
        .record_candidate_build_item_result(
            build.build_id,
            item.submission_id,
            CandidateBuildItemStatus::Created,
            Some(candidate_id),
            Some(EventId::new()),
            None,
        )
        .unwrap();
    runtime
        .read_candidate_review(locator, candidate_id)
        .unwrap()
        .unwrap()
}

#[test]
fn old_empty_fat_semantic_bytes_preserve_open_and_closed_retries() {
    for boundary in [CheckpointBoundary::Continue, CheckpointBoundary::Close] {
        let temporary = TempDir::new().unwrap();
        let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
        let (locator, task) = open_task(&runtime, "old-fat-retry", "preserve old retry bytes");
        let input = checkpoint_write(
            &locator,
            &task,
            0,
            boundary,
            vec![checkpoint_claim("Old low-level Claim")],
            Vec::new(),
        );
        let first = runtime.write_agent_checkpoint(&input).unwrap();
        // Frozen old helper output; only the server-owned IDs and boundary vary.
        let old = r#"{"boundary":"BOUNDARY","claims":[{"applicability":{"conditions":["Agent Checkpoint"],"domains":["runtime"],"platforms":[]},"artifact_refs":[],"assumptions":[],"context_kind_hint":null,"engineering_references":[],"evidence_refs":[],"inline_validations":[{"content":{"actual":"passed","test":"checkpoint"},"interpretation":"The focused runtime behavior was directly validated","kind":"experiment_record","limitations":[],"supports":"Old low-level Claim"}],"rationale":"Direct validation supports this engineering conclusion","recheck_when":[],"related_contexts":[],"relations":[],"statement":"Old low-level Claim","topic_key_hint":null}],"expected_intent_revision_id":"INTENT_ID","expected_task_id":"TASK_ID","unknowns":[]}"#
            .replace("TASK_ID", &task.task_id.to_string())
            .replace("INTENT_ID", &input.expected_intent_revision_id.to_string())
            .replace("BOUNDARY", if boundary == CheckpointBoundary::Close { "close" } else { "continue" });
        let connection = Connection::open(runtime.database_path()).unwrap();
        let stored: String = connection
            .query_row(
                "SELECT semantic_json FROM agent_checkpoint WHERE checkpoint_id = ?1",
                [first.checkpoint.checkpoint_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, old,
            "new writes must match the old binary comparator"
        );
        assert_eq!(
            connection
                .execute(
                    "UPDATE agent_checkpoint SET semantic_json = ?1 WHERE checkpoint_id = ?2",
                    rusqlite::params![old, first.checkpoint.checkpoint_id.to_string()]
                )
                .unwrap(),
            1
        );
        let retry = runtime.write_agent_checkpoint(&input).unwrap();
        assert!(!retry.created);
        assert_eq!(retry.checkpoint, first.checkpoint);
        assert_eq!(
            retry.episode.episode.episode_id,
            first.episode.episode.episode_id
        );
        assert_eq!(
            runtime
                .list_work_episodes(task.task_session_id, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM agent_checkpoint", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn content_addressed_checkpoint_operations_converge_across_concurrency_and_delayed_retry() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (locator, task) = open_task(&runtime, "content-operation", "persist direct Checkpoints");

    let a = direct_submission(&locator, "Operation A");
    let first_a = runtime.submit_agent_checkpoint(&a).unwrap();
    assert!(!first_a.replayed);
    assert_eq!(first_a.build.status, CandidateBuildStatus::Pending);
    assert_eq!(first_a.build.items.len(), 1);
    assert_eq!(
        first_a.build.items[0].status,
        CandidateBuildItemStatus::Queued
    );

    let b = direct_submission(&locator, "Operation B");
    let first_b = runtime.submit_agent_checkpoint(&b).unwrap();
    assert!(!first_b.replayed);
    assert_ne!(first_b.operation_id, first_a.operation_id);
    assert_ne!(
        first_b.checkpoint.checkpoint_id,
        first_a.checkpoint.checkpoint_id
    );
    assert_ne!(
        first_b.episode.episode.episode_id,
        first_a.episode.episode.episode_id
    );

    let delayed_a = runtime.submit_agent_checkpoint(&a).unwrap();
    assert!(delayed_a.replayed);
    assert_eq!(delayed_a.operation_id, first_a.operation_id);
    assert_eq!(
        delayed_a.checkpoint.checkpoint_id,
        first_a.checkpoint.checkpoint_id
    );
    assert_eq!(
        delayed_a.episode.episode.episode_id,
        first_a.episode.episode.episode_id
    );
    assert_eq!(delayed_a.build.build_id, first_a.build.build_id);
    assert_eq!(
        runtime
            .list_work_episodes(task.task_session_id, 10)
            .unwrap()
            .len(),
        2,
        "A/B/A must not create a third Episode"
    );

    let parallel = direct_submission(&locator, "Operation C concurrent");
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let submission = parallel.clone();
            thread::spawn(move || {
                barrier.wait();
                runtime.submit_agent_checkpoint(&submission).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|outcome| !outcome.replayed).count(),
        1
    );
    assert!(outcomes.iter().all(|outcome| {
        outcome.operation_id == outcomes[0].operation_id
            && outcome.checkpoint.checkpoint_id == outcomes[0].checkpoint.checkpoint_id
            && outcome.episode.episode.episode_id == outcomes[0].episode.episode.episode_id
            && outcome.build.build_id == outcomes[0].build.build_id
            && outcome.build.items[0].submission_id == outcomes[0].build.items[0].submission_id
    }));
    assert_eq!(
        runtime
            .list_work_episodes(task.task_session_id, 10)
            .unwrap()
            .len(),
        3
    );
    let revised = runtime
        .append_intent_revision(
            task.task_session_id,
            task.current_intent_revision().unwrap().revision_id,
            intent("persist direct Checkpoints under a revised Intent"),
        )
        .unwrap();
    assert_eq!(revised.status, IntentRevisionWriteStatus::Created);
    let revised_a = runtime.submit_agent_checkpoint(&a).unwrap();
    assert!(!revised_a.replayed);
    assert_ne!(revised_a.operation_id, first_a.operation_id);
    let (other_locator, other_task) = open_task(
        &runtime,
        "content-operation-other",
        "persist direct Checkpoints",
    );
    let other_a = runtime
        .submit_agent_checkpoint(&direct_submission(&other_locator, "Operation A"))
        .unwrap();
    assert!(!other_a.replayed);
    assert_ne!(other_a.operation_id, first_a.operation_id);
    assert_ne!(other_task.task_id, task.task_id);
    assert_eq!(
        runtime
            .list_recoverable_candidate_build_episodes(&locator, 10)
            .unwrap()
            .len(),
        4
    );
    let connection = Connection::open(runtime.database_path()).unwrap();
    let persisted_semantics = connection
        .query_row(
            "SELECT semantic_json FROM checkpoint_operation WHERE operation_id = ?1",
            [&first_a.operation_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    // This literal is the pre-R2-1 direct operation shape, independent of Claim serde.
    assert_eq!(
        persisted_semantics,
        r#"{"claims":[{"conditions":["content addressed"],"context_kind":"validation","evidence":[{"evidence_type":"experiment_record","limitations":[],"summary":"Operation A passed"}],"rationale":"The direct Checkpoint operation is durable","statement":"Operation A"}],"unknowns":[]}"#
    );
    assert!(persisted_semantics.contains("Operation A"));
    assert!(!persisted_semantics.contains("content-operation"));
    assert!(!persisted_semantics.contains(&task.task_id.to_string()));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM checkpoint_operation", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        5
    );
}

#[test]
fn checkpoint_operation_rolls_back_episode_checkpoint_and_outbox_together() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let (locator, _task) = open_task(&runtime, "operation-rollback", "prove atomic outbox");
    let connection = Connection::open(runtime.database_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_candidate_build
             BEFORE INSERT ON candidate_build
             BEGIN SELECT RAISE(ABORT, 'injected build reservation failure'); END;",
        )
        .unwrap();
    let error = runtime
        .submit_agent_checkpoint(&direct_submission(&locator, "Atomic rollback"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    for table in [
        "work_episode",
        "work_observation",
        "agent_checkpoint",
        "candidate_build",
        "candidate_build_item",
        "checkpoint_operation",
    ] {
        let count = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} retained partial operation residue");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn checkpoint_is_atomic_semantically_idempotent_and_closes_without_hook_observations() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (locator, task) = open_task(&runtime, "checkpoint", "persist conclusions");
    let first = checkpoint_write(
        &locator,
        &task,
        0,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("Inline validation is self-contained")],
        Vec::new(),
    );
    let barrier = Arc::new(Barrier::new(8));
    let outcomes = (0..8)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let input = first.clone();
            thread::spawn(move || {
                barrier.wait();
                runtime.write_agent_checkpoint(&input).unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.checkpoint.checkpoint_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    let continued = &outcomes[0];
    assert_eq!(continued.episode.episode.version, 1);
    assert_eq!(continued.episode.checkpoints.len(), 1);
    assert_eq!(continued.inline_observation_ids.len(), 1);
    assert_eq!(continued.episode.episode.observations.len(), 1);

    let mut conflicting = first.clone();
    conflicting.claims[0].statement = "different semantic retry".to_owned();
    assert_eq!(
        runtime
            .write_agent_checkpoint(&conflicting)
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );

    let close = checkpoint_write(
        &locator,
        &task,
        1,
        CheckpointBoundary::Close,
        Vec::new(),
        vec![CheckpointUnknown {
            statement: "Compatibility remains to be checked".to_owned(),
            blocking: true,
            recheck_when: vec!["the client matrix is available".to_owned()],
        }],
    );
    let closed = runtime.write_agent_checkpoint(&close).unwrap();
    assert!(closed.created);
    assert_eq!(closed.episode.episode.version, 2);
    assert_eq!(closed.episode.checkpoints.len(), 2);
    assert!(matches!(
        closed.episode.episode.status,
        WorkEpisodeStatus::Closed { final_checkpoint_id }
            if final_checkpoint_id == closed.checkpoint.checkpoint_id
    ));
    let retry = runtime.write_agent_checkpoint(&close).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.checkpoint, closed.checkpoint);

    let stale = checkpoint_write(
        &locator,
        &task,
        2,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("closed Episodes reject later writes")],
        Vec::new(),
    );
    assert_eq!(
        runtime.write_agent_checkpoint(&stale).unwrap_err().kind(),
        ErrorKind::StaleState
    );

    let (_, other_task) = open_task(&runtime, "checkpoint-other", "isolated owner");
    let other_locator = ExternalSessionLocator::new("codex", "checkpoint-other").unwrap();
    let mut cross_task_claim = checkpoint_claim("cross Task evidence is rejected");
    cross_task_claim.inline_validations.clear();
    cross_task_claim.evidence_refs = vec![CheckpointEvidenceRef::Observation {
        observation_id: continued.inline_observation_ids[0],
    }];
    let cross_task = checkpoint_write(
        &other_locator,
        &other_task,
        0,
        CheckpointBoundary::Continue,
        vec![cross_task_claim],
        Vec::new(),
    );
    assert_eq!(
        runtime
            .write_agent_checkpoint(&cross_task)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert!(
        runtime
            .list_work_episodes(other_task.task_session_id, 10)
            .unwrap()
            .is_empty(),
        "failed cross-Task writes must roll back Episode creation"
    );

    runtime
        .start_new_task(&locator, task.task_id, &intent("switched task"), Vec::new())
        .unwrap();
    assert_eq!(
        runtime.write_agent_checkpoint(&close).unwrap_err().kind(),
        ErrorKind::InvalidInput,
        "Task switch must make the previous Checkpoint owner inactive"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn checkpoint_disambiguates_closed_retries_new_episodes_and_open_episode_versions() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let (locator, task) = open_task(&runtime, "checkpoint-episodes", "resume the same task");
    let first = checkpoint_write(
        &locator,
        &task,
        0,
        CheckpointBoundary::Close,
        vec![checkpoint_claim("Episode one is complete")],
        Vec::new(),
    );
    let closed_first = runtime.write_agent_checkpoint(&first).unwrap();
    assert!(closed_first.created);
    assert!(matches!(
        closed_first.episode.episode.status,
        WorkEpisodeStatus::Closed { .. }
    ));

    let retry = runtime.write_agent_checkpoint(&first).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.checkpoint, closed_first.checkpoint);
    assert_eq!(retry.episode, closed_first.episode);

    let second = checkpoint_write(
        &locator,
        &task,
        0,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("Episode two resumed new work")],
        Vec::new(),
    );
    let opened_second = runtime.write_agent_checkpoint(&second).unwrap();
    assert!(opened_second.created);
    assert_ne!(
        opened_second.episode.episode.episode_id,
        closed_first.episode.episode.episode_id
    );
    assert_eq!(opened_second.episode.episode.version, 1);

    let appended = checkpoint_write(
        &locator,
        &task,
        1,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("Episode two accepts its current version")],
        Vec::new(),
    );
    let appended = runtime.write_agent_checkpoint(&appended).unwrap();
    assert!(appended.created);
    assert_eq!(
        appended.episode.episode.episode_id,
        opened_second.episode.episode.episode_id
    );
    assert_eq!(appended.episode.episode.version, 2);

    let mut old_version_conflict = second;
    old_version_conflict.claims[0].statement = "An old Episode two parent cannot fork".to_owned();
    assert_eq!(
        runtime
            .write_agent_checkpoint(&old_version_conflict)
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );

    let close_second = checkpoint_write(
        &locator,
        &task,
        2,
        CheckpointBoundary::Close,
        Vec::new(),
        vec![CheckpointUnknown {
            statement: "Episode two follow-up is recorded".to_owned(),
            blocking: false,
            recheck_when: vec!["the follow-up is resolved".to_owned()],
        }],
    );
    runtime.write_agent_checkpoint(&close_second).unwrap();
    let closed_history = runtime
        .list_work_episodes(task.task_session_id, 10)
        .unwrap();
    assert_eq!(closed_history.len(), 2);

    let nonzero_after_close = checkpoint_write(
        &locator,
        &task,
        2,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim(
            "A closed Episode rejects nonzero new work",
        )],
        Vec::new(),
    );
    assert_eq!(
        runtime
            .write_agent_checkpoint(&nonzero_after_close)
            .unwrap_err()
            .kind(),
        ErrorKind::StaleState
    );
    assert_eq!(
        runtime
            .list_work_episodes(task.task_session_id, 10)
            .unwrap(),
        closed_history,
        "a stale closed-Episode write must leave Episode state unchanged"
    );

    let mut wrong_task = checkpoint_write(
        &locator,
        &task,
        0,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("Task CAS must be exact")],
        Vec::new(),
    );
    wrong_task.expected_task_id = TaskId::new();
    assert_eq!(
        runtime
            .write_agent_checkpoint(&wrong_task)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        runtime
            .list_work_episodes(task.task_session_id, 10)
            .unwrap(),
        closed_history,
        "a Task CAS failure must not create an Episode"
    );

    let old_revision_id = task.current_intent_revision().unwrap().revision_id;
    runtime
        .append_intent_revision(
            task.task_session_id,
            old_revision_id,
            intent("resume the same task with a revised intent"),
        )
        .unwrap();
    let after_intent_update = runtime
        .list_work_episodes(task.task_session_id, 10)
        .unwrap();
    let stale_intent = checkpoint_write(
        &locator,
        &task,
        0,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("Intent CAS must be exact")],
        Vec::new(),
    );
    assert_eq!(
        runtime
            .write_agent_checkpoint(&stale_intent)
            .unwrap_err()
            .kind(),
        ErrorKind::StaleState
    );
    assert_eq!(
        runtime
            .list_work_episodes(task.task_session_id, 10)
            .unwrap(),
        after_intent_update,
        "an Intent CAS failure must not create an Episode"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn concurrent_new_episode_checkpoints_converge_and_divergent_content_cannot_fork() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (locator, task) = open_task(
        &runtime,
        "checkpoint-new-episode-race",
        "serialize resumed checkpoints",
    );
    runtime
        .write_agent_checkpoint(&checkpoint_write(
            &locator,
            &task,
            0,
            CheckpointBoundary::Close,
            vec![checkpoint_claim("The first Episode establishes history")],
            Vec::new(),
        ))
        .unwrap();

    let same_new_episode = checkpoint_write(
        &locator,
        &task,
        0,
        CheckpointBoundary::Continue,
        vec![checkpoint_claim("Concurrent resumed work is identical")],
        Vec::new(),
    );
    let barrier = Arc::new(Barrier::new(20));
    let outcomes = (0..20)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let input = same_new_episode.clone();
            thread::spawn(move || {
                barrier.wait();
                runtime.write_agent_checkpoint(&input).unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.episode.episode.episode_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.checkpoint.checkpoint_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        runtime
            .list_work_episodes(task.task_session_id, 10)
            .unwrap()
            .len(),
        2
    );

    let closed_second = runtime.close_checkpointed_work_episode(&locator).unwrap();
    assert!(matches!(
        closed_second,
        AutomatedEpisodeBoundary::Closed {
            newly_closed: true,
            ..
        }
    ));
    let barrier = Arc::new(Barrier::new(20));
    let divergent = (0..20)
        .map(|index| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let mut input = checkpoint_write(
                &locator,
                &task,
                0,
                CheckpointBoundary::Continue,
                vec![checkpoint_claim("Divergent resumed work")],
                Vec::new(),
            );
            input.claims[0].statement = format!("Divergent resumed work {index}");
            thread::spawn(move || {
                barrier.wait();
                runtime.write_agent_checkpoint(&input)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        divergent.iter().filter(|outcome| outcome.is_ok()).count(),
        1
    );
    assert!(
        divergent
            .iter()
            .filter_map(|outcome| outcome.as_ref().err())
            .all(|error| error.kind() == ErrorKind::Conflict)
    );
    let history = runtime
        .list_work_episodes(task.task_session_id, 10)
        .unwrap();
    assert_eq!(history.len(), 3);
    let open = history
        .iter()
        .filter(|episode| episode.episode.status == WorkEpisodeStatus::Open)
        .collect::<Vec<_>>();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].checkpoints.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn lifecycle_boundary_requires_current_checkpoint_and_concurrent_retries_close_once() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let missing = ExternalSessionLocator::new("codex", "lifecycle-missing").unwrap();
    assert_eq!(
        runtime.close_checkpointed_work_episode(&missing).unwrap(),
        AutomatedEpisodeBoundary::NoActiveTask
    );

    let (locator, task) = open_task(&runtime, "lifecycle", "solidify checkpointed work");
    assert!(matches!(
        runtime.close_checkpointed_work_episode(&locator).unwrap(),
        AutomatedEpisodeBoundary::NoEpisode {
            task_session_id,
            task_id,
            ..
        } if task_session_id == task.task_session_id && task_id == task.task_id
    ));
    let opened = runtime
        .open_work_episode(
            &locator,
            task.task_id,
            task.current_intent_revision().unwrap().revision_id,
        )
        .unwrap()
        .episode;
    assert!(matches!(
        runtime.close_checkpointed_work_episode(&locator).unwrap(),
        AutomatedEpisodeBoundary::CheckpointRequired { episode, .. }
            if episode.episode.episode_id == opened.episode.episode_id
    ));
    let checkpoint = runtime
        .write_agent_checkpoint(&checkpoint_write(
            &locator,
            &task,
            0,
            CheckpointBoundary::Continue,
            vec![checkpoint_claim(
                "Lifecycle automation reuses explicit cognition",
            )],
            Vec::new(),
        ))
        .unwrap();
    runtime
        .merge_signals(
            task.task_session_id,
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "the lifecycle implementation changed".to_owned(),
            }],
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(16));
    let outcomes = (0..16)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let locator = locator.clone();
            thread::spawn(move || {
                barrier.wait();
                runtime.close_checkpointed_work_episode(&locator).unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                AutomatedEpisodeBoundary::Closed {
                    newly_closed: true,
                    ..
                }
            ))
            .count(),
        1
    );
    for outcome in outcomes {
        let AutomatedEpisodeBoundary::Closed {
            episode,
            newly_closed: _,
        } = outcome
        else {
            panic!("every concurrent lifecycle retry must observe the closed Episode");
        };
        assert_eq!(episode.episode.episode_id, opened.episode.episode_id);
        assert_eq!(episode.episode.version, 2);
        assert_eq!(episode.episode.signal_refs.len(), 2);
        assert!(matches!(
            episode.episode.status,
            WorkEpisodeStatus::Closed { final_checkpoint_id }
                if final_checkpoint_id == checkpoint.checkpoint.checkpoint_id
        ));
    }
    assert!(matches!(
        runtime
            .close_checkpointed_work_episode_cas(
                &locator,
                task.task_id,
                task.current_intent_revision().unwrap().revision_id,
                1,
            )
            .unwrap(),
        AutomatedEpisodeBoundary::Closed {
            newly_closed: false,
            ..
        }
    ));
    assert_eq!(
        runtime
            .close_checkpointed_work_episode_cas(
                &locator,
                task.task_id,
                task.current_intent_revision().unwrap().revision_id,
                0,
            )
            .unwrap_err()
            .kind(),
        ErrorKind::StaleState
    );

    let (stale_locator, stale_task) = open_task(
        &runtime,
        "lifecycle-stale",
        "require current Intent checkpoint",
    );
    runtime
        .write_agent_checkpoint(&checkpoint_write(
            &stale_locator,
            &stale_task,
            0,
            CheckpointBoundary::Continue,
            vec![checkpoint_claim("The old Intent was checkpointed")],
            Vec::new(),
        ))
        .unwrap();
    runtime
        .append_intent_revision(
            stale_task.task_session_id,
            stale_task.current_intent_revision().unwrap().revision_id,
            intent("the revised Intent requires a new Checkpoint"),
        )
        .unwrap();
    let required = runtime
        .close_checkpointed_work_episode(&stale_locator)
        .unwrap();
    assert!(matches!(
        required,
        AutomatedEpisodeBoundary::CheckpointRequired { episode, .. }
            if episode.episode.status == WorkEpisodeStatus::Open
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_build_reservation_is_concurrent_stable_promotable_and_finalized_once() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (locator, task) = open_task(&runtime, "candidate-build", "reserve submissions");
    let closed = runtime
        .write_agent_checkpoint(&checkpoint_write(
            &locator,
            &task,
            0,
            CheckpointBoundary::Close,
            vec![
                checkpoint_claim("first Candidate Claim"),
                checkpoint_claim("second Candidate Claim"),
            ],
            Vec::new(),
        ))
        .unwrap();
    let claims = &closed.checkpoint.claims;
    let preparations = vec![
        CandidateBuildItemPreparation {
            checkpoint_id: closed.checkpoint.checkpoint_id,
            claim_id: claims[0].claim_id,
            content_hash: Some("sha256:first".to_owned()),
            status: CandidateBuildItemStatus::Prepared,
            error_code: None,
        },
        CandidateBuildItemPreparation {
            checkpoint_id: closed.checkpoint.checkpoint_id,
            claim_id: claims[1].claim_id,
            content_hash: None,
            status: CandidateBuildItemStatus::NeedsEvidence,
            error_code: Some("insufficient_evidence".to_owned()),
        },
    ];
    let barrier = Arc::new(Barrier::new(20));
    let reservations = (0..20)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            let preparations = preparations.clone();
            let episode_id = closed.episode.episode.episode_id;
            thread::spawn(move || {
                barrier.wait();
                runtime
                    .prepare_candidate_build(episode_id, &preparations)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        reservations
            .iter()
            .map(|view| view.build_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        reservations
            .iter()
            .flat_map(|view| view.items.iter().map(|item| item.submission_id))
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2,
        "each Claim keeps one distinct stable SubmissionId"
    );
    assert_eq!(reservations[0].status, CandidateBuildStatus::Pending);

    let mut promoted = preparations;
    promoted[1].content_hash = Some("sha256:second".to_owned());
    promoted[1].status = CandidateBuildItemStatus::Prepared;
    promoted[1].error_code = None;
    let promoted = runtime
        .prepare_candidate_build(closed.episode.episode.episode_id, &promoted)
        .unwrap();
    assert!(
        promoted
            .items
            .iter()
            .all(|item| item.status == CandidateBuildItemStatus::Prepared)
    );
    let first = &promoted.items[0];
    let candidate_id = CandidateId::new();
    let event_id = EventId::new();
    let finalized = runtime
        .record_candidate_build_item_result(
            promoted.build_id,
            first.submission_id,
            CandidateBuildItemStatus::Created,
            Some(candidate_id),
            Some(event_id),
            None,
        )
        .unwrap();
    let retried = runtime
        .record_candidate_build_item_result(
            promoted.build_id,
            first.submission_id,
            CandidateBuildItemStatus::AlreadyExists,
            Some(candidate_id),
            Some(event_id),
            None,
        )
        .unwrap();
    assert_eq!(finalized, retried);
    assert_eq!(retried.items[0].status, CandidateBuildItemStatus::Created);
    assert!(
        runtime
            .record_candidate_build_item_result(
                promoted.build_id,
                first.submission_id,
                CandidateBuildItemStatus::AlreadyExists,
                Some(CandidateId::new()),
                Some(event_id),
                None,
            )
            .is_err()
    );
    let second_candidate_id = CandidateId::new();
    runtime
        .record_candidate_build_item_result(
            promoted.build_id,
            promoted.items[1].submission_id,
            CandidateBuildItemStatus::Created,
            Some(second_candidate_id),
            Some(EventId::new()),
            None,
        )
        .unwrap();
    let first_page = runtime
        .list_candidate_reviews(&locator, CandidateReviewStatus::Pending, 1, None)
        .unwrap();
    assert_eq!(first_page.records.len(), 1);
    assert!(first_page.next_cursor.is_some());
    let second_page = runtime
        .list_candidate_reviews(
            &locator,
            CandidateReviewStatus::Pending,
            1,
            first_page.next_cursor.as_deref(),
        )
        .unwrap();
    assert_eq!(second_page.records.len(), 1);
    assert_ne!(
        first_page.records[0].candidate_id,
        second_page.records[0].candidate_id
    );
    assert!(second_page.next_cursor.is_none());
    assert!(
        runtime
            .list_candidate_reviews(
                &locator,
                CandidateReviewStatus::Pending,
                1,
                Some("not-a-review-cursor"),
            )
            .is_err()
    );
    let review = runtime
        .read_candidate_review(&locator, candidate_id)
        .unwrap()
        .unwrap();
    assert_eq!(review.build_id, promoted.build_id);
    assert_eq!(review.checkpoint_id, first.checkpoint_id);
    assert_eq!(review.claim_id, first.claim_id);
    assert_eq!(review.review_version, 1);
    assert_eq!(review.status, CandidateReviewStatus::Pending);
    assert_eq!(
        review.expires_at_unix_seconds - review.created_at_unix_seconds,
        DEFAULT_CANDIDATE_REVIEW_TTL.as_secs()
    );
    assert!(DEFAULT_CANDIDATE_REVIEW_TTL <= MAX_CANDIDATE_REVIEW_TTL);

    // The maintenance survey sees both Pending Reviews without a locator, filters by the expiry
    // horizon it was asked about, and -- unlike every scoped list reader above -- retires nothing:
    // surveying at a moment well past expiry leaves the Review exactly as Pending as it was.
    assert_eq!(
        runtime
            .survey_candidate_reviews_at(review.created_at_unix_seconds, 0)
            .unwrap(),
        CandidateReviewSurvey {
            pending_count: 2,
            expiring_soon_count: 0,
        }
    );
    assert_eq!(
        runtime
            .survey_candidate_reviews_at(review.expires_at_unix_seconds + 60, 0)
            .unwrap(),
        CandidateReviewSurvey {
            pending_count: 2,
            expiring_soon_count: 2,
        }
    );
    assert_eq!(
        runtime
            .read_candidate_review(&locator, candidate_id)
            .unwrap()
            .unwrap()
            .status,
        CandidateReviewStatus::Pending
    );

    let persisted = ContextCandidate {
        candidate_id,
        submission_id: first.submission_id,
        source_episode: closed.episode.episode.ownership(),
        content: candidate_content("Runtime Candidate analysis"),
    };
    let observation_ids = closed.checkpoint.claims[0]
        .evidence_refs
        .iter()
        .filter_map(|evidence| match evidence {
            CheckpointEvidenceRef::Observation { observation_id } => Some(*observation_id),
            _ => None,
        })
        .collect();
    let automatic = AutomaticContextCandidate::from_persisted_candidate(
        &persisted,
        &closed.episode.episode,
        &closed.episode.checkpoints,
        CandidateBuilderProvenance {
            build_id: promoted.build_id,
            source_episode: closed.episode.episode.ownership(),
            checkpoint_ids: vec![closed.checkpoint.checkpoint_id],
            observation_ids,
            engineering_references: Vec::new(),
        },
        CandidateAnalysis {
            status: CandidateAnalysisStatus::Complete,
            assessments: vec![CandidateRelationAssessment {
                relation: CandidateAssessmentRelation::Novel,
                target: None,
                confidence: CandidateConfidence {
                    basis_points: 6_000,
                    rationale: "No target Context was retrieved".to_owned(),
                },
                paths: vec![CandidateAssessmentPath::NoSufficientCandidate],
                reasons: vec!["No target Context was retrieved".to_owned()],
            }],
            context_tree_oid: Some("tree".to_owned()),
            context_generation: Some(1),
            graph_context_tree_oid: None,
            artifact_generation: None,
            token_budget: 1_024,
            estimated_tokens: 100,
            omitted_target_count: 0,
            error_code: None,
        },
        Vec::new(),
        CandidateConfidence {
            basis_points: 6_000,
            rationale: "Novel review result".to_owned(),
        },
        Vec::new(),
        AutomaticCandidateStatus::NeedsSpaceReview,
    )
    .unwrap();
    let mut failed = automatic.clone();
    failed.analysis = CandidateAnalysis {
        status: CandidateAnalysisStatus::Failed,
        error_code: Some("analysis_dependency_unavailable".to_owned()),
        ..CandidateAnalysis::default()
    };
    failed.confidence = CandidateConfidence {
        basis_points: 0,
        rationale: "Analysis failed and remains retryable".to_owned(),
    };
    failed.status = AutomaticCandidateStatus::Draft;
    let audit_relation = || {
        Connection::open(runtime.database_path())
            .unwrap()
            .query_row(
                "SELECT top_relation FROM candidate_review WHERE candidate_id = ?1",
                [candidate_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
    };
    let first_analysis = runtime.replace_candidate_analysis(&failed).unwrap();
    assert_eq!(first_analysis.analysis_generation, 1);
    assert_eq!(audit_relation(), None);
    assert_eq!(
        first_analysis.candidate.analysis.status,
        CandidateAnalysisStatus::Failed
    );
    let completed = runtime.replace_candidate_analysis(&automatic).unwrap();
    assert_eq!(completed.analysis_generation, 2);
    assert_eq!(audit_relation().as_deref(), Some("novel"));
    let replaced = runtime.replace_candidate_analysis(&automatic).unwrap();
    assert_eq!(replaced.analysis_generation, 3);
    assert_eq!(
        replaced.candidate.analysis.status,
        CandidateAnalysisStatus::Complete
    );
    assert_eq!(
        runtime
            .read_candidate_analysis(candidate_id)
            .unwrap()
            .unwrap(),
        replaced
    );
    Connection::open(runtime.database_path())
        .unwrap()
        .execute(
            "DELETE FROM candidate_analysis WHERE candidate_id = ?1",
            [candidate_id.to_string()],
        )
        .unwrap();
    assert!(
        runtime
            .read_candidate_analysis(candidate_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(audit_relation().as_deref(), Some("novel"));
    let recomputed = runtime.replace_candidate_analysis(&automatic).unwrap();
    assert_eq!(recomputed.analysis_generation, 1);

    let discarded = runtime
        .discard_candidate_review(&CandidateReviewDiscard {
            decision_source: DecisionSource::Human,
            locator: locator.clone(),
            expected_task_id: task.task_id,
            expected_intent_revision_id: task.current_intent_revision().unwrap().revision_id,
            candidate_id,
            expected_review_version: 1,
            reason: "not useful for durable knowledge".to_owned(),
        })
        .unwrap();
    assert_eq!(discarded.status, CandidateReviewDiscardStatus::Discarded);
    assert_eq!(discarded.record.review_version, 2);
    let mut later_analysis = automatic.clone();
    later_analysis.analysis.assessments[0].relation = CandidateAssessmentRelation::Supports;
    later_analysis.analysis.assessments[0].target = Some(sctx_domain::ContextRevisionRef {
        context_id: ContextId::new(),
        revision_id: sctx_domain::RevisionId::new(),
    });
    later_analysis.analysis.assessments[0].paths = vec![CandidateAssessmentPath::ContextFullText {
        matched_terms: vec!["audit".to_owned()],
    }];
    runtime.replace_candidate_analysis(&later_analysis).unwrap();
    assert_eq!(audit_relation().as_deref(), Some("novel"));
    runtime.replace_candidate_analysis(&failed).unwrap();
    assert_eq!(audit_relation().as_deref(), Some("novel"));
    let stats = runtime.candidate_disposition_stats().unwrap();
    assert_eq!(
        stats.relation_decisions[0].top_relation.as_deref(),
        Some("novel")
    );
    assert_eq!(stats.relation_decisions[0].counts.discarded, 1);

    let idempotent = runtime
        .discard_candidate_review(&CandidateReviewDiscard {
            decision_source: DecisionSource::Human,
            locator: locator.clone(),
            expected_task_id: task.task_id,
            expected_intent_revision_id: task.current_intent_revision().unwrap().revision_id,
            candidate_id,
            expected_review_version: 1,
            reason: "not useful for durable knowledge".to_owned(),
        })
        .unwrap();
    assert_eq!(
        idempotent.status,
        CandidateReviewDiscardStatus::AlreadyDiscarded
    );
    assert_eq!(
        runtime
            .discard_candidate_review(&CandidateReviewDiscard {
                decision_source: DecisionSource::Human,
                locator: locator.clone(),
                expected_task_id: task.task_id,
                expected_intent_revision_id: task.current_intent_revision().unwrap().revision_id,
                candidate_id,
                expected_review_version: 1,
                reason: "different retry meaning".to_owned(),
            })
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );
    assert!(
        runtime
            .list_candidate_reviews(&locator, CandidateReviewStatus::Pending, 10, None)
            .unwrap()
            .records
            .iter()
            .all(|record| record.candidate_id != candidate_id)
    );
    assert_eq!(
        runtime
            .list_candidate_reviews(&locator, CandidateReviewStatus::Discarded, 10, None)
            .unwrap()
            .records[0]
            .candidate_id,
        candidate_id
    );
    let cleanup = runtime
        .cleanup_expired_candidate_reviews_at(review.expires_at_unix_seconds + 1)
        .unwrap();
    assert!(cleanup.expired_candidate_ids.contains(&candidate_id));
    assert_eq!(audit_relation().as_deref(), Some("novel"));
    let stats = runtime.candidate_disposition_stats().unwrap();
    assert_eq!(stats.relation_decisions[0].counts.discarded, 1);
    assert_eq!(
        stats.human.discarded, 0,
        "legacy live-status total is unchanged"
    );

    assert!(
        runtime
            .read_candidate_analysis(candidate_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime
            .read_candidate_review(&locator, candidate_id)
            .unwrap()
            .unwrap()
            .status,
        CandidateReviewStatus::Expired
    );
    assert!(
        runtime
            .list_candidate_reviews(&locator, CandidateReviewStatus::Pending, 10, None)
            .unwrap()
            .records
            .iter()
            .all(|record| record.candidate_id != candidate_id)
    );
    assert!(
        runtime
            .list_candidate_reviews(&locator, CandidateReviewStatus::Expired, 10, None)
            .unwrap()
            .records
            .iter()
            .any(|record| record.candidate_id == candidate_id)
    );
    runtime
        .record_candidate_build_item_result(
            promoted.build_id,
            first.submission_id,
            CandidateBuildItemStatus::AlreadyExists,
            Some(candidate_id),
            Some(event_id),
            None,
        )
        .unwrap();
    assert_eq!(
        runtime
            .read_candidate_review(&locator, candidate_id)
            .unwrap()
            .unwrap()
            .status,
        CandidateReviewStatus::Expired,
        "Builder retry must not resurrect an Expired Review tombstone"
    );
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
fn candidate_reviews_isolate_sessions_episodes_stale_and_reserved_confirmed_state() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let (first_locator, first) = open_task(&runtime, "review-a", "first review task");
    let (second_locator, second) = open_task(&runtime, "review-b", "second review task");
    let first_review = finalize_review(&runtime, &first_locator, &first, "first Episode Candidate");
    let later_review = finalize_review(&runtime, &first_locator, &first, "later Episode Candidate");
    let other_review = finalize_review(
        &runtime,
        &second_locator,
        &second,
        "other Session Candidate",
    );

    let first_records = runtime
        .list_candidate_reviews(&first_locator, CandidateReviewStatus::Pending, 10, None)
        .unwrap()
        .records;
    assert_eq!(first_records.len(), 2);
    assert!(
        first_records
            .iter()
            .any(|record| record.candidate_id == first_review.candidate_id)
    );
    assert!(
        first_records
            .iter()
            .any(|record| record.candidate_id == later_review.candidate_id)
    );
    assert_ne!(
        first_review.source_episode.episode_id,
        later_review.source_episode.episode_id
    );
    let second_records = runtime
        .list_candidate_reviews(&second_locator, CandidateReviewStatus::Pending, 10, None)
        .unwrap()
        .records;
    assert_eq!(second_records.len(), 1);
    assert_eq!(second_records[0].candidate_id, other_review.candidate_id);
    assert!(
        runtime
            .read_candidate_review(&second_locator, first_review.candidate_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime
            .discard_candidate_review(&CandidateReviewDiscard {
                decision_source: DecisionSource::Human,
                locator: first_locator.clone(),
                expected_task_id: first.task_id,
                expected_intent_revision_id: first.current_intent_revision().unwrap().revision_id,
                candidate_id: first_review.candidate_id,
                expected_review_version: 99,
                reason: "stale attempt".to_owned(),
            })
            .unwrap_err()
            .kind(),
        ErrorKind::StaleState
    );
    assert_eq!(
        runtime
            .discard_candidate_review(&CandidateReviewDiscard {
                decision_source: DecisionSource::Human,
                locator: second_locator.clone(),
                expected_task_id: second.task_id,
                expected_intent_revision_id: second.current_intent_revision().unwrap().revision_id,
                candidate_id: first_review.candidate_id,
                expected_review_version: 1,
                reason: "cross task attempt".to_owned(),
            })
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );

    Connection::open(runtime.database_path())
        .unwrap()
        .execute(
            "UPDATE candidate_review
             SET status = 'confirmed', confirmation_id = ?1, result_context_id = ?2
             WHERE candidate_id = ?3",
            [
                ConfirmationId::new().to_string(),
                ContextId::new().to_string(),
                later_review.candidate_id.to_string(),
            ],
        )
        .unwrap();
    assert_eq!(
        runtime
            .discard_candidate_review(&CandidateReviewDiscard {
                decision_source: DecisionSource::Human,
                locator: first_locator.clone(),
                expected_task_id: first.task_id,
                expected_intent_revision_id: first.current_intent_revision().unwrap().revision_id,
                candidate_id: later_review.candidate_id,
                expected_review_version: 1,
                reason: "must not discard confirmed".to_owned(),
            })
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );
    assert_eq!(
        runtime
            .list_candidate_reviews(&first_locator, CandidateReviewStatus::Confirmed, 10, None)
            .unwrap()
            .records[0]
            .candidate_id,
        later_review.candidate_id
    );

    fs::remove_file(runtime.database_path()).unwrap();
    let reset = TaskRuntime::initialize(temporary.path()).unwrap();
    assert!(
        reset
            .list_candidate_reviews(&first_locator, CandidateReviewStatus::Pending, 10, None)
            .is_err(),
        "runtime deletion must not rediscover Git-only Candidate identities"
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
    let same = runtime
        .append_intent_revision(
            initial.task_session_id,
            current,
            initial
                .current_intent_revision()
                .unwrap()
                .working_intent
                .clone(),
        )
        .unwrap();
    assert_eq!(same.status, IntentRevisionWriteStatus::AlreadyCurrent);
    assert_eq!(same.revision.revision_id, current);
    assert_eq!(
        runtime
            .read_work_episode(opened.episode.episode_id)
            .unwrap()
            .unwrap()
            .episode
            .intent_revisions
            .revision_ids,
        vec![current]
    );
    let revision = runtime
        .append_intent_revision(initial.task_session_id, current, intent("revised"))
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
        revision.revision.revision_id
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
            revision.revision.revision_id,
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
            &intent("replacement task"),
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
fn concurrent_work_observation_append_is_version_guarded() {
    let temporary = TempDir::new().unwrap();
    let runtime = Arc::new(TaskRuntime::initialize(temporary.path()).unwrap());
    let (locator, task) = open_task(&runtime, "episode-ingest", "observe work");
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
                validation_observation(&format!("concurrent observation {index}")),
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

    assert_eq!(
        runtime
            .prepare_work_episode_close(episode.episode.episode_id, 1)
            .unwrap()
            .observation_ids
            .len(),
        1
    );
    let verification = runtime
        .verify_source_episode(episode.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(verification.status, WorkEpisodeStatus::Open);
    assert_eq!(verification.observation_count, 1);
}

#[test]
fn deleting_runtime_loses_episode_only_and_preserves_other_state() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("root");
    let runtime = TaskRuntime::initialize(&root).unwrap();
    let (locator, task) = open_task(&runtime, "episode-delete", "delete runtime");
    let checkpoint = runtime
        .write_agent_checkpoint(&checkpoint_write(
            &locator,
            &task,
            0,
            CheckpointBoundary::Close,
            Vec::new(),
            vec![CheckpointUnknown {
                statement: "Runtime deletion removes local Checkpoint state".to_owned(),
                blocking: false,
                recheck_when: Vec::new(),
            }],
        ))
        .unwrap();
    let episode_id = checkpoint.episode.episode.episode_id;
    fs::create_dir_all(root.join("repository")).unwrap();
    fs::write(root.join("repository/fact"), b"git fact").unwrap();
    fs::write(root.join("state/index.sqlite"), b"index").unwrap();
    fs::create_dir_all(root.join("state/unrelated")).unwrap();
    fs::write(root.join("state/unrelated/safe.json"), b"unrelated").unwrap();
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
        fs::read(root.join("state/unrelated/safe.json")).unwrap(),
        b"unrelated"
    );
}

/// The `TurnStop` checkpoint reminder gate (WP-V6 fix 3, `docs/deferred-issues.md` #6): capped at
/// three reminders per external Session, and an idle turn -- no
/// [`TaskRuntime::record_checkpoint_reminder_activity`] since the last reminder -- neither shows
/// the reminder again nor spends part of the budget.
#[test]
fn turn_stop_checkpoint_reminder_gate_caps_at_three_and_skips_idle_turns() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let (locator, _task) = open_task(&runtime, "reminder-gate", "throttle the TurnStop nag");

    // 1st reminder: always fires, there is nothing yet to compare it against.
    assert!(
        runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "the first reminder fires unconditionally"
    );

    // Immediately again, with no recorded activity in between: an idle turn is suppressed and
    // must not consume part of the three-reminder budget.
    assert!(
        !runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "a turn with no activity since the last reminder must not repeat it"
    );
    assert!(
        !runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "repeating the idle check must still not consume budget"
    );

    // Real activity unlocks the 2nd reminder.
    runtime
        .record_checkpoint_reminder_activity(&locator)
        .unwrap();
    assert!(
        runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "activity since the last reminder unlocks the next one"
    );
    assert!(
        !runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "the activity was spent by the reminder that just fired"
    );

    // Activity unlocks the 3rd and final reminder.
    runtime
        .record_checkpoint_reminder_activity(&locator)
        .unwrap();
    assert!(
        runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "the third reminder still fires"
    );

    // The budget is now spent: even with fresh activity, a 4th reminder never fires again this
    // Session.
    runtime
        .record_checkpoint_reminder_activity(&locator)
        .unwrap();
    assert!(
        !runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "a 4th reminder must not fire even with new activity: the Session budget is spent"
    );
    runtime
        .record_checkpoint_reminder_activity(&locator)
        .unwrap();
    assert!(
        !runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "the budget stays spent for the rest of the Session"
    );
}

/// A Session with no `ExternalSession` row yet (no `ActiveTask`) has nothing to throttle: the gate
/// fails open, and recording activity against it is a harmless no-op rather than an error.
#[test]
fn turn_stop_checkpoint_reminder_gate_fails_open_with_no_active_task() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let missing = ExternalSessionLocator::new("codex", "reminder-gate-missing").unwrap();

    assert!(
        runtime.gate_turn_stop_checkpoint_reminder(&missing),
        "no ExternalSession row means nothing to throttle: the gate fails open"
    );
    assert!(
        runtime.gate_turn_stop_checkpoint_reminder(&missing),
        "failing open is not itself state: it does not start consuming a budget that has no row \
         to live on"
    );
    runtime
        .record_checkpoint_reminder_activity(&missing)
        .expect("recording activity with no row is a harmless no-op, not an error");
}
