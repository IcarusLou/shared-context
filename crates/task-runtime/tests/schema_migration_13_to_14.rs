//! `TaskRuntime` schema version 13 -> 14 is an in-place, additive migration: it must add
//! `hook_event` and advance `PRAGMA user_version` without touching any pre-existing row.
//!
//! There is no standalone "build a v13 database" helper, so this constructs one honestly: it
//! opens a fresh (current-schema) `TaskRuntime`, writes representative business rows through
//! the public API and a hand-crafted `candidate_review` row, then downgrades the file to look
//! exactly like a real version 13 database by dropping `hook_event` (the only thing v14 added)
//! and rewinding `PRAGMA user_version`. Reopening it must then migrate forward in place.

use rusqlite::{Connection, params};
use sctx_domain::{ExternalSessionLocator, TaskId, WorkingIntentSnapshot};
use sctx_task_runtime::TaskRuntime;
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

#[test]
#[allow(clippy::too_many_lines)]
fn schema_version_13_migrates_in_place_to_14_and_keeps_existing_rows() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");

    // 1. A real Task Session, written through the public API against the current (v14) schema.
    let locator = ExternalSessionLocator::new("codex", "schema-migration-session").unwrap();
    let task_id = TaskId::new();
    let (task_session_id, revision_id) = {
        let runtime = TaskRuntime::initialize(&root).unwrap();
        let outcome = runtime
            .open_or_create(
                locator.clone(),
                task_id,
                intent("survive a schema upgrade"),
                Vec::new(),
            )
            .unwrap();
        (
            outcome.snapshot.task_session_id,
            outcome
                .snapshot
                .current_intent_revision()
                .unwrap()
                .revision_id,
        )
    };
    let database_path = root.join("state").join("runtime.sqlite");
    assert!(database_path.is_file());

    // 2. A hand-crafted `candidate_review` row (its own foreign-key chain is irrelevant to this
    //    migration test, so it is inserted with foreign key enforcement off, exactly as a
    //    version 13 installation's own already-written row would already exist on disk).
    let candidate_id = "candidate-schema-migration-fixture";
    {
        let connection = Connection::open(&database_path).unwrap();
        connection.execute("PRAGMA foreign_keys = OFF", []).unwrap();
        connection
            .execute(
                "INSERT INTO candidate_review (
                    candidate_id, submission_id, episode_id, task_session_id, task_id,
                    build_id, final_checkpoint_id, checkpoint_id, claim_id, review_version,
                    status, created_at_unix_seconds, expires_at_unix_seconds
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, 'pending', 1000, 2000)",
                params![
                    candidate_id,
                    "submission-schema-migration-fixture",
                    "episode-schema-migration-fixture",
                    task_session_id.to_string(),
                    task_id.to_string(),
                    "build-schema-migration-fixture",
                    "checkpoint-schema-migration-fixture",
                    "checkpoint-schema-migration-fixture",
                    "claim-schema-migration-fixture",
                ],
            )
            .unwrap();

        // 3. Downgrade the file to a genuine version 13 shape: `hook_event` is the only schema
        //    difference version 14 introduces, so dropping it and rewinding `user_version`
        //    reproduces a real pre-upgrade installation exactly.
        connection
            .execute_batch(
                "DROP INDEX IF EXISTS hook_event_recorded_at;
                 DROP TABLE IF EXISTS hook_event;
                 PRAGMA user_version = 13;",
            )
            .unwrap();
    }
    {
        let connection = Connection::open(&database_path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 13, "fixture must start at schema version 13");
        let hook_event_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hook_event')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!hook_event_exists, "fixture must not have hook_event yet");
    }

    // 4. Reopening must migrate in place: version advances to 14, hook_event now exists, and
    //    every pre-existing row -- Task Runtime tables written through the public API, and the
    //    hand-crafted candidate_review row -- is untouched.
    let runtime = TaskRuntime::initialize(&root).unwrap();

    let connection = Connection::open(runtime.database_path()).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 14, "migration must advance user_version to 14");
    let hook_event_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hook_event')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(hook_event_exists, "migration must create hook_event");
    let hook_event_row_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM hook_event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        hook_event_row_count, 0,
        "migration must not fabricate hook_event rows"
    );

    let snapshot = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    assert_eq!(snapshot.task_session_id, task_session_id);
    assert_eq!(snapshot.task_id, task_id);
    assert_eq!(
        snapshot.current_intent_revision().unwrap().revision_id,
        revision_id
    );
    assert_eq!(
        snapshot
            .current_intent_revision()
            .unwrap()
            .working_intent
            .goal,
        "survive a schema upgrade"
    );

    let (status, review_version, submission_id): (String, i64, String) = connection
        .query_row(
            "SELECT status, review_version, submission_id FROM candidate_review WHERE candidate_id = ?1",
            params![candidate_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(review_version, 1);
    assert_eq!(submission_id, "submission-schema-migration-fixture");

    // Recording a Hook diagnostic now works against the migrated schema.
    let record = sctx_task_runtime::HookEventRecord {
        recorded_at_unix_ms: 1,
        agent_kind: "codex".to_owned(),
        external_session_id: Some("schema-migration-session".to_owned()),
        event_kind: "session_start".to_owned(),
        decision: sctx_task_runtime::HookEventDecision::Enabled,
        reason: "ok".to_owned(),
        duration_ms: 0,
        detail: None,
    };
    runtime.record_hook_event(&record).unwrap();
    assert_eq!(runtime.recent_hook_events(10).unwrap().len(), 1);
}
