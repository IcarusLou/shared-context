//! Injection records and their derived Context usage outcomes.

use sctx_domain::{ContextId, ExternalSessionLocator, RevisionId, TaskId, WorkingIntentSnapshot};
use sctx_task_runtime::{
    ContextInjectionSource, ContextUsageOutcome, ContextUsageRecord, ContextUsageTotals,
    InjectedContext, TaskRuntime,
};
use tempfile::TempDir;

fn intent(goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: Some(format!("Deliver {goal}")),
        in_scope: vec![goal.to_owned()],
        out_of_scope: vec![],
        domains: vec!["task-runtime".to_owned()],
        platforms: vec![],
        constraints: vec![],
        acceptance_conditions: vec![format!("{goal} is recorded")],
        artifact_hints: vec![],
        interface_hints: vec![],
        open_questions: vec![],
    }
}

/// Opens one Task and returns its identity plus its current Intent revision.
fn open_task(runtime: &TaskRuntime, session: &str) -> (TaskId, sctx_domain::TaskIntentRevisionId) {
    let task_id = TaskId::new();
    let outcome = runtime
        .open_or_create(
            ExternalSessionLocator::new("codex", session).unwrap(),
            task_id,
            intent(session),
            Vec::new(),
        )
        .unwrap();
    let revision_id = outcome
        .snapshot
        .current_intent_revision()
        .unwrap()
        .revision_id;
    (task_id, revision_id)
}

fn injected() -> (InjectedContext, InjectedContext) {
    (
        InjectedContext {
            context_id: ContextId::new(),
            revision_id: RevisionId::new(),
        },
        InjectedContext {
            context_id: ContextId::new(),
            revision_id: RevisionId::new(),
        },
    )
}

#[test]
fn re_injecting_one_context_refreshes_only_its_timestamp() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let (task_id, revision) = open_task(&runtime, "session-injection");
    let (first, second) = injected();

    runtime
        .record_task_injections_at(
            task_id,
            revision,
            ContextInjectionSource::IntentUpdate,
            &[first, second, first],
            1_000,
        )
        .unwrap();
    runtime
        .record_task_injections_at(
            task_id,
            revision,
            ContextInjectionSource::TaskContext,
            &[first],
            2_000,
        )
        .unwrap();

    let records = runtime.read_task_injections(task_id).unwrap();
    assert_eq!(records.len(), 2, "one row per (task, context) pair");
    let refreshed = records
        .iter()
        .find(|record| record.context_id == first.context_id)
        .unwrap();
    assert_eq!(refreshed.injected_at_unix_seconds, 2_000);
    assert_eq!(
        refreshed.source,
        ContextInjectionSource::IntentUpdate,
        "the entry point that first injected the Context stays the recorded provenance"
    );
    assert_eq!(refreshed.revision_id, first.revision_id);
    assert_eq!(refreshed.intent_revision_id, revision);
    assert!(
        runtime
            .read_task_injections(TaskId::new())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn usage_outcomes_are_per_task_and_never_downgrade_a_refutation() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let (first_task, _) = open_task(&runtime, "session-usage-one");
    let (second_task, _) = open_task(&runtime, "session-usage-two");
    let context_id = ContextId::new();

    runtime
        .record_context_usage_at(
            &[
                ContextUsageRecord {
                    context_id,
                    task_id: first_task,
                    outcome: ContextUsageOutcome::Ignored,
                },
                ContextUsageRecord {
                    context_id,
                    task_id: second_task,
                    outcome: ContextUsageOutcome::Ignored,
                },
            ],
            1_000,
        )
        .unwrap();
    // A later Checkpoint in the same Task may upgrade its own verdict.
    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                context_id,
                task_id: first_task,
                outcome: ContextUsageOutcome::Reused,
            }],
            2_000,
        )
        .unwrap();
    assert_eq!(
        runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
        ContextUsageTotals {
            reused: 1,
            ignored: 1,
            refuted: 0,
        }
    );

    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                context_id,
                task_id: second_task,
                outcome: ContextUsageOutcome::Refuted,
            }],
            3_000,
        )
        .unwrap();
    // Replaying the earlier ignore must not undo the refutation.
    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                context_id,
                task_id: second_task,
                outcome: ContextUsageOutcome::Ignored,
            }],
            4_000,
        )
        .unwrap();

    assert_eq!(
        runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
        ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 1,
        }
    );
    assert!(
        runtime
            .context_usage_totals(&[ContextId::new()])
            .unwrap()
            .is_empty()
    );
    assert!(runtime.context_usage_totals(&[]).unwrap().is_empty());
}

/// Reuse is monotonic within one `(context, task)` pair.
///
/// Four independent signals decide reuse at different moments of one Task, and the ones that
/// arrive last -- the Candidate analysis assessment above all -- run after the Candidate Build
/// pass that writes the omissions. A Build rerun re-derives the early signals only, so if a
/// re-recorded `ignored` could overwrite a stored `reused`, every recovery drain would erase the
/// proof the analysis had just established.
#[test]
fn a_recorded_reuse_is_never_downgraded_by_a_later_omission() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let (task_id, _) = open_task(&runtime, "session-usage-monotonic");
    let context_id = ContextId::new();

    let reused = ContextUsageTotals {
        reused: 1,
        ignored: 0,
        refuted: 0,
    };
    let record = |outcome, at| {
        runtime
            .record_context_usage_at(
                &[ContextUsageRecord {
                    context_id,
                    task_id,
                    outcome,
                }],
                at,
            )
            .unwrap()
    };

    record(ContextUsageOutcome::Ignored, 1_000);
    record(ContextUsageOutcome::Reused, 2_000);
    assert_eq!(
        runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
        reused
    );

    // The Candidate Build reruns and re-derives only the early signals.
    record(ContextUsageOutcome::Ignored, 3_000);
    assert_eq!(
        runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
        reused,
        "an omission is the absence of evidence and cannot retract a proof"
    );

    // Re-running the analysis writes the same verdict without doubling the count.
    record(ContextUsageOutcome::Reused, 4_000);
    assert_eq!(
        runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
        reused,
        "one pair contributes one row however often it is re-derived"
    );

    // A refutation still outranks reuse, and reuse cannot walk it back.
    record(ContextUsageOutcome::Refuted, 5_000);
    record(ContextUsageOutcome::Reused, 6_000);
    assert_eq!(
        runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
        ContextUsageTotals {
            reused: 0,
            ignored: 0,
            refuted: 1,
        }
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn session_close_covers_all_owned_tasks_without_overwriting_strong_verdicts() {
    let root = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let (first, revision) = open_task(&runtime, "close-coverage");
    let locator = ExternalSessionLocator::new("codex", "close-coverage").unwrap();
    let (one, two) = injected();
    runtime
        .record_task_injections_at(
            first,
            revision,
            ContextInjectionSource::TaskContext,
            &[one, two],
            10,
        )
        .unwrap();
    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                context_id: one.context_id,
                task_id: first,
                outcome: ContextUsageOutcome::Refuted,
            }],
            20,
        )
        .unwrap();
    let second = runtime
        .start_new_task(&locator, first, &intent("second task"), Vec::new())
        .unwrap()
        .snapshot;
    runtime
        .record_task_injections_at(
            second.task_id,
            second.current_intent_revision().unwrap().revision_id,
            ContextInjectionSource::TaskContext,
            &[one],
            30,
        )
        .unwrap();
    // Identical external key in another host and another key in this host are both excluded.
    for other in [
        ExternalSessionLocator::new("cursor", "close-coverage").unwrap(),
        ExternalSessionLocator::new("codex", "other-coverage").unwrap(),
    ] {
        let task = runtime
            .open_or_create(other, TaskId::new(), intent("unrelated"), Vec::new())
            .unwrap()
            .snapshot;
        runtime
            .record_task_injections_at(
                task.task_id,
                task.current_intent_revision().unwrap().revision_id,
                ContextInjectionSource::TaskContext,
                &[one],
                30,
            )
            .unwrap();
    }
    let written: usize = std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| scope.spawn(|| runtime.record_session_close_usage_at(&locator, 40).unwrap()))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    });
    assert_eq!(written, 2);
    assert_eq!(
        runtime.record_session_close_usage_at(&locator, 50).unwrap(),
        0
    );
    let connection = rusqlite::Connection::open(runtime.database_path()).unwrap();
    let read = |task: TaskId, context: ContextId| {
        connection.query_row("SELECT outcome, basis, recorded_at_unix_seconds FROM context_usage WHERE task_id = ?1 AND context_id = ?2",
            [task.to_string(), context.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))).unwrap()
    };
    assert_eq!(
        read(first, one.context_id),
        ("refuted".into(), "checkpoint_derived".into(), 20)
    );
    assert_eq!(
        read(first, two.context_id),
        ("ignored".into(), "session_close".into(), 40)
    );
    assert_eq!(
        read(second.task_id, one.context_id),
        ("ignored".into(), "session_close".into(), 40)
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM context_usage", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                context_id: two.context_id,
                task_id: first,
                outcome: ContextUsageOutcome::Ignored,
            }],
            60,
        )
        .unwrap();
    assert_eq!(
        read(first, two.context_id),
        ("ignored".into(), "checkpoint_derived".into(), 60)
    );
    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                context_id: one.context_id,
                task_id: second.task_id,
                outcome: ContextUsageOutcome::Reused,
            }],
            60,
        )
        .unwrap();
    assert_eq!(
        read(second.task_id, one.context_id),
        ("reused".into(), "checkpoint_derived".into(), 60)
    );
}
