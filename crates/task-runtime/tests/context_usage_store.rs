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
