use rusqlite::Connection;
use sctx_domain::{ContextId, ExternalSessionLocator, RevisionId, TaskId, WorkingIntentSnapshot};
use sctx_task_runtime::{
    ContextInjectionSource, ContextUsageOutcome, ContextUsageRecord, InjectedContext, TaskRuntime,
    recall_stats::read_recall_stats,
};
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

fn intent() -> WorkingIntentSnapshot {
    serde_json::from_value(serde_json::json!({"goal": "measure actual injection usage"})).unwrap()
}
fn state_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root.join("state"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}
fn context() -> InjectedContext {
    InjectedContext {
        context_id: ContextId::new(),
        revision_id: RevisionId::new(),
    }
}

#[test]
fn missing_runtime_stats_do_not_create_the_installation() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("absent");
    let stats = read_recall_stats(&root).unwrap();
    assert!(!stats.runtime_available);
    assert_eq!(stats.schema_version, None);
    assert_eq!(stats.totals.injections, 0);
    assert_eq!(stats.totals.coverage_percent, None);
    assert_eq!(stats.totals.strong_reuse_rate_percent, None);
    assert!(!root.exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn recall_snapshot_reads_live_wal_and_separates_evidence_without_source_writes() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let runtime = TaskRuntime::initialize(root).unwrap();
    // Keep a live WAL writer so the newest committed rows cannot be read from main DB alone.
    let writer = Connection::open(runtime.database_path()).unwrap();
    writer
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .unwrap();
    let locator = ExternalSessionLocator::new("codex", "recall-counts").unwrap();
    let first = runtime
        .open_or_create(locator.clone(), TaskId::new(), intent(), Vec::new())
        .unwrap()
        .snapshot;
    let contexts = [context(), context(), context()];
    runtime
        .record_task_injections_at(
            first.task_id,
            first.current_intent_revision().unwrap().revision_id,
            ContextInjectionSource::TaskContext,
            &contexts,
            10,
        )
        .unwrap();
    for (context, outcome) in contexts.iter().zip([
        ContextUsageOutcome::Reused,
        ContextUsageOutcome::Ignored,
        ContextUsageOutcome::Refuted,
    ]) {
        runtime
            .record_context_usage_at(
                &[ContextUsageRecord {
                    task_id: first.task_id,
                    context_id: context.context_id,
                    outcome,
                }],
                20,
            )
            .unwrap();
    }
    // Verdicts for a non-injected Context cannot inflate injection coverage or strong reuse.
    runtime
        .record_context_usage_at(
            &[ContextUsageRecord {
                task_id: first.task_id,
                context_id: ContextId::new(),
                outcome: ContextUsageOutcome::Reused,
            }],
            20,
        )
        .unwrap();
    let second = runtime
        .start_new_task(&locator, first.task_id, &intent(), Vec::new())
        .unwrap()
        .snapshot;
    runtime
        .record_task_injections_at(
            second.task_id,
            second.current_intent_revision().unwrap().revision_id,
            ContextInjectionSource::TaskContext,
            &[contexts[0]],
            30,
        )
        .unwrap();
    runtime
        .record_task_injections_at(
            second.task_id,
            second.current_intent_revision().unwrap().revision_id,
            ContextInjectionSource::TaskContext,
            &[contexts[1]],
            50,
        )
        .unwrap();
    runtime
        .open_or_create(
            ExternalSessionLocator::new("cursor", "recall-counts").unwrap(),
            TaskId::new(),
            intent(),
            Vec::new(),
        )
        .unwrap();
    writer.execute("INSERT INTO context_usage (context_id, task_id, outcome, recorded_at_unix_seconds, basis)
        VALUES (?1, ?2, 'ignored', 40, 'session_close')", [contexts[0].context_id.to_string(), second.task_id.to_string()]).unwrap();
    let before = state_files(root);
    assert!(
        before
            .get("runtime.sqlite-wal")
            .is_some_and(|bytes| !bytes.is_empty())
    );
    let stats = read_recall_stats(root).unwrap();
    assert_eq!(state_files(root), before);
    assert!(stats.runtime_available);
    assert_eq!(stats.schema_version, Some(21));
    assert_eq!(
        (
            stats.totals.injections,
            stats.totals.judged,
            stats.totals.unjudged
        ),
        (5, 4, 1)
    );
    assert_eq!(stats.totals.coverage_percent, Some(80.0));
    assert_eq!(stats.totals.strong_samples, 3);
    assert!((stats.totals.strong_reuse_rate_percent.unwrap() - 100.0 / 3.0).abs() < 0.0001);
    let strong = &stats.totals.outcomes.checkpoint_derived;
    assert_eq!((strong.reused, strong.ignored, strong.refuted), (1, 1, 1));
    let weak = &stats.totals.outcomes.session_close;
    assert_eq!((weak.reused, weak.ignored, weak.refuted), (0, 1, 0));
    assert_eq!(stats.by_task.len(), 3);
    assert_eq!(stats.by_session.len(), 2);
    assert_eq!(
        stats
            .by_task
            .iter()
            .map(|task| task.totals.injections)
            .sum::<u64>(),
        stats.totals.injections
    );
    let codex = stats
        .by_session
        .iter()
        .find(|session| session.agent_kind == "codex")
        .unwrap();
    assert_eq!(codex.tasks, 2);
    assert_eq!(codex.totals, stats.totals);
    let cursor = stats
        .by_session
        .iter()
        .find(|session| session.agent_kind == "cursor")
        .unwrap();
    assert_eq!(cursor.tasks, 1);
    assert_eq!(cursor.totals.coverage_percent, None);
    drop(writer);
    let before = state_files(root);
    assert!(!before.contains_key("runtime.sqlite-wal"));
    assert_eq!(read_recall_stats(root).unwrap(), stats);
    assert_eq!(
        state_files(root),
        before,
        "a closed WAL source gets no new sidecars"
    );
}

#[test]
fn readonly_stats_reject_old_schema_and_rollback_journal_without_changing_them() {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path()).unwrap();
    let connection = Connection::open(runtime.database_path()).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE context_usage DROP COLUMN basis;
        ALTER TABLE candidate_review DROP COLUMN top_relation; PRAGMA user_version = 18;",
        )
        .unwrap();
    drop(connection);
    let before = state_files(temporary.path());
    assert!(
        read_recall_stats(temporary.path())
            .unwrap_err()
            .message()
            .contains("upgrade explicitly")
    );
    assert_eq!(state_files(temporary.path()), before);
    fs::write(
        temporary.path().join("state/runtime.sqlite-journal"),
        b"active journal",
    )
    .unwrap();
    let before = state_files(temporary.path());
    assert!(
        read_recall_stats(temporary.path())
            .unwrap_err()
            .message()
            .contains("rollback journal")
    );
    assert_eq!(state_files(temporary.path()), before);
}
