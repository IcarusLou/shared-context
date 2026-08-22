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
    CandidateReviewStatus, CaptureEvidenceRef, CaptureId, CaptureUnknown, ConfirmationId,
    ContextCandidate, ContextId, ContextKind, ContextRevisionDraft, ErrorKind, EventId,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, NormalizedBreadcrumbKind,
    NormalizedWorkObservation, TaskId, TaskIntent, TaskIntentDraft, TaskSignal, TaskSignalKind,
    TestOutcomeStatus, WorkEpisodeStatus, WorkSourceRef,
};
use sctx_task_runtime::{
    AgentCheckpointWrite, AutomatedEpisodeBoundary, CandidateBuildItemPreparation,
    CandidateBuildItemStatus, CandidateBuildStatus, CandidateReviewDiscard,
    CandidateReviewDiscardStatus, CaptureIngestion, CheckpointBoundary, CheckpointClaimDraft,
    DEFAULT_CANDIDATE_REVIEW_TTL, MAX_CANDIDATE_REVIEW_TTL, TaskRuntime, WorkEpisodeDiagnosticKind,
};
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
        assumptions: Vec::new(),
        recheck_when: vec!["the validated behavior changes".to_owned()],
        evidence_refs: Vec::new(),
        inline_validations: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: statement.to_owned(),
            content: serde_json::json!({"test": "checkpoint", "actual": "passed"}),
            interpretation: "The focused runtime behavior was directly validated".to_owned(),
            limitations: Vec::new(),
        }],
        artifact_refs: Vec::new(),
        related_contexts: Vec::new(),
    }
}

fn candidate_content(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
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
    unknowns: Vec<CaptureUnknown>,
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
        vec![CaptureUnknown {
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
    cross_task_claim.evidence_refs = vec![CaptureEvidenceRef::Observation {
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
        .start_new_task(&locator, task.task_id, &draft("switched task"), Vec::new())
        .unwrap();
    assert_eq!(
        runtime.write_agent_checkpoint(&close).unwrap_err().kind(),
        ErrorKind::InvalidInput,
        "Task switch must make the previous Checkpoint owner inactive"
    );
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
            intent(
                stale_task.task_id,
                "the revised Intent requires a new Checkpoint",
            ),
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
            CaptureEvidenceRef::Observation { observation_id } => Some(*observation_id),
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
    let first_analysis = runtime.replace_candidate_analysis(&failed).unwrap();
    assert_eq!(first_analysis.analysis_generation, 1);
    assert_eq!(
        first_analysis.candidate.analysis.status,
        CandidateAnalysisStatus::Failed
    );
    let completed = runtime.replace_candidate_analysis(&automatic).unwrap();
    assert_eq!(completed.analysis_generation, 2);
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
    let recomputed = runtime.replace_candidate_analysis(&automatic).unwrap();
    assert_eq!(recomputed.analysis_generation, 1);

    let discarded = runtime
        .discard_candidate_review(&CandidateReviewDiscard {
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
    let idempotent = runtime
        .discard_candidate_review(&CandidateReviewDiscard {
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
    let checkpoint = runtime
        .write_agent_checkpoint(&checkpoint_write(
            &locator,
            &task,
            0,
            CheckpointBoundary::Close,
            Vec::new(),
            vec![CaptureUnknown {
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
