//! `hook_event` diagnostic rows: write, read-back, and bounded retention.

use sctx_task_runtime::{HookEventDecision, HookEventRecord, TaskRuntime};
use tempfile::TempDir;

fn runtime() -> (TempDir, TaskRuntime) {
    let temporary = TempDir::new().unwrap();
    let runtime = TaskRuntime::initialize(temporary.path().join(".shared-context")).unwrap();
    (temporary, runtime)
}

fn record(reason: &str, decision: HookEventDecision, at_unix_ms: u64) -> HookEventRecord {
    HookEventRecord {
        recorded_at_unix_ms: at_unix_ms,
        agent_kind: "cursor".to_owned(),
        external_session_id: Some("session-1".to_owned()),
        event_kind: "post_tool_use".to_owned(),
        decision,
        reason: reason.to_owned(),
        duration_ms: 3,
        detail: None,
    }
}

#[test]
fn record_hook_event_round_trips_through_recent_hook_events() {
    let (_temporary, runtime) = runtime();
    runtime
        .record_hook_event(&record("ok", HookEventDecision::Enabled, 1_000))
        .unwrap();
    runtime
        .record_hook_event(&record(
            "payload_decode_failed",
            HookEventDecision::FailOpen,
            2_000,
        ))
        .unwrap();

    let recent = runtime.recent_hook_events(10).unwrap();
    assert_eq!(recent.len(), 2);
    // Newest first.
    assert_eq!(recent[0].reason, "payload_decode_failed");
    assert_eq!(recent[0].decision, "fail_open");
    assert_eq!(recent[0].recorded_at_unix_ms, 2_000);
    assert_eq!(recent[1].reason, "ok");
    assert_eq!(recent[1].decision, "enabled");
    assert_eq!(recent[1].agent_kind, "cursor");
    assert_eq!(recent[1].external_session_id.as_deref(), Some("session-1"));
    assert_eq!(recent[1].event_kind, "post_tool_use");
    assert_eq!(recent[1].duration_ms, 3);
}

#[test]
fn recent_hook_events_respects_the_requested_limit() {
    let (_temporary, runtime) = runtime();
    for index in 0..5u64 {
        runtime
            .record_hook_event(&record("ok", HookEventDecision::Enabled, index))
            .unwrap();
    }
    assert_eq!(runtime.recent_hook_events(2).unwrap().len(), 2);
    assert_eq!(runtime.recent_hook_events(100).unwrap().len(), 5);
}

#[test]
fn hook_event_counts_since_groups_by_decision_and_reason_within_the_window() {
    let (_temporary, runtime) = runtime();
    runtime
        .record_hook_event(&record("ok", HookEventDecision::Enabled, 10_000))
        .unwrap();
    runtime
        .record_hook_event(&record("ok", HookEventDecision::Enabled, 10_001))
        .unwrap();
    runtime
        .record_hook_event(&record(
            "maintenance_lock_busy",
            HookEventDecision::FailOpen,
            10_002,
        ))
        .unwrap();
    // Outside the window: must not be counted.
    runtime
        .record_hook_event(&record("ok", HookEventDecision::Enabled, 1))
        .unwrap();

    let counts = runtime.hook_event_counts_since(10_000).unwrap();
    assert_eq!(counts.len(), 2);
    let ok = counts
        .iter()
        .find(|count| count.reason == "ok")
        .expect("ok count present");
    assert_eq!(ok.decision, "enabled");
    assert_eq!(ok.count, 2);
    let busy = counts
        .iter()
        .find(|count| count.reason == "maintenance_lock_busy")
        .expect("maintenance_lock_busy count present");
    assert_eq!(busy.decision, "fail_open");
    assert_eq!(busy.count, 1);
}

#[test]
fn record_hook_event_rejects_an_oversized_detail() {
    let (_temporary, runtime) = runtime();
    let mut oversized = record("ok", HookEventDecision::Enabled, 1);
    oversized.detail = Some("x".repeat(257));
    let error = runtime.record_hook_event(&oversized).unwrap_err();
    assert_eq!(error.kind(), sctx_domain::ErrorKind::InvalidInput);
}

#[test]
fn many_sequential_writes_all_succeed_and_stay_queryable() {
    // This exercises the `id % HOOK_EVENT_PRUNE_INTERVAL == 0` prune branch on every 64th
    // insert without asserting an exact row count tied to the retention constant — under the
    // default 5_000-row retention floor, none of these 130 rows are old enough to be swept.
    // The pruning DELETE itself is covered by a whitebox unit test in `src/lib.rs`, which seeds
    // `sqlite_sequence` so the retention threshold is reachable without 5_000+ real inserts.
    let (_temporary, runtime) = runtime();
    for index in 0..130u64 {
        runtime
            .record_hook_event(&record("ok", HookEventDecision::Enabled, index))
            .unwrap();
    }
    let recent = runtime.recent_hook_events(1_000).unwrap();
    assert_eq!(recent.len(), 130);
}
