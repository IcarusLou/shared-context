//! `TaskRuntime` in-place schema upgrades. Version 13 -> 14 is additive (`hook_event`) and must
//! touch no pre-existing row; version 14 -> 15 discards `context_usage` and must touch nothing
//! else; version 15 -> 16 discards the recorded omissions only and keeps every proof; version
//! 16 -> 17 is additive again (disposition provenance) and must leave every decided Review
//! readable as the human decision it was; version 17 -> 18 is additive (two `external_session`
//! counters for the `TurnStop` checkpoint reminder gate, WP-V6 fix 3). The five chain, so a
//! version 13 database reopened today lands on the current version.
//!
//! There is no standalone "build an old database" helper, so these construct one honestly: they
//! open a fresh (current-schema) `TaskRuntime`, write representative business rows through the
//! public API and a hand-crafted `candidate_review` row, then downgrade the file to look exactly
//! like a real older database and rewind `PRAGMA user_version`. Reopening must migrate forward.

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
fn schema_version_13_chains_forward_in_place_and_keeps_existing_rows() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");

    // 1. A real Task Session, written through the public API against the current schema.
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
                 DROP INDEX IF EXISTS auto_confirm_rejection_recorded_at;
                 DROP TABLE IF EXISTS auto_confirm_rejection;
                 ALTER TABLE candidate_review DROP COLUMN decision_source;
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

    // 4. Reopening must migrate in place: the version chains all the way to the current one,
    //    hook_event now exists, and every pre-existing row -- Task Runtime tables written through
    //    the public API, and the hand-crafted candidate_review row -- is untouched.
    let runtime = TaskRuntime::initialize(&root).unwrap();

    let connection = Connection::open(runtime.database_path()).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        version, 18,
        "migration must chain through to the current version"
    );
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

/// Version 14 -> 15 clears every recorded injection outcome and nothing else.
///
/// The rows a version 14 installation holds were all decided by statement token similarity, which
/// credited restatement and recorded real reuse as an omission. They are advisory local ranking
/// state with no Event behind them and no way to re-derive them, so the migration deletes them.
/// What was injected into which Task is a fact and survives.
#[test]
fn schema_version_14_discards_the_recorded_injection_outcomes_only() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");
    let locator = ExternalSessionLocator::new("codex", "usage-migration-session").unwrap();
    let task_id = TaskId::new();
    let context_id = sctx_domain::ContextId::new();
    let revision_id = sctx_domain::RevisionId::new();
    let intent_revision_id = {
        let runtime = TaskRuntime::initialize(&root).unwrap();
        let outcome = runtime
            .open_or_create(
                locator.clone(),
                task_id,
                intent("survive a usage reset"),
                Vec::new(),
            )
            .unwrap();
        let intent_revision_id = outcome
            .snapshot
            .current_intent_revision()
            .unwrap()
            .revision_id;
        runtime
            .record_task_injections(
                task_id,
                intent_revision_id,
                sctx_task_runtime::ContextInjectionSource::IntentUpdate,
                &[sctx_task_runtime::InjectedContext {
                    context_id,
                    revision_id,
                }],
            )
            .unwrap();
        runtime
            .record_context_usage(&[sctx_task_runtime::ContextUsageRecord {
                context_id,
                task_id,
                outcome: sctx_task_runtime::ContextUsageOutcome::Ignored,
            }])
            .unwrap();
        assert_eq!(
            runtime.context_usage_totals(&[context_id]).unwrap()[&context_id],
            sctx_task_runtime::ContextUsageTotals {
                reused: 0,
                ignored: 1,
                refuted: 0,
            }
        );
        intent_revision_id
    };

    // Neither version 15 nor 16 changed a table shape, so rewinding the stamp alone is a
    // faithful version 14, and reopening chains 14 -> 15 -> 16 in one call.
    let database_path = root.join("state").join("runtime.sqlite");
    Connection::open(&database_path)
        .unwrap()
        .execute_batch("PRAGMA user_version = 14;")
        .unwrap();

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let connection = Connection::open(runtime.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row::<i64, _, _>("PRAGMA user_version", [], |row| row.get(0))
            .unwrap(),
        18
    );
    assert!(
        runtime
            .context_usage_totals(&[context_id])
            .unwrap()
            .is_empty(),
        "the migration must discard every outcome the old comparison decided"
    );
    let injections = runtime.read_task_injections(task_id).unwrap();
    assert_eq!(
        injections.len(),
        1,
        "what was injected is a fact and survives"
    );
    assert_eq!(injections[0].context_id, context_id);
    assert_eq!(injections[0].intent_revision_id, intent_revision_id);
    assert_eq!(
        runtime
            .read_snapshot_by_locator(&locator)
            .unwrap()
            .unwrap()
            .task_id,
        task_id
    );
}

/// Version 15 -> 16 discards the recorded omissions and keeps every proof.
///
/// Version 15 decided reuse from two signals; two more decide it now, so a stored `ignored` is a
/// verdict this version would not necessarily reach on the same input and cannot be re-derived.
/// `reused` and `refuted` are proofs and no signal was removed, so both still hold.
#[test]
fn schema_version_15_discards_the_recorded_omissions_and_keeps_the_proofs() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");
    let locator = ExternalSessionLocator::new("codex", "omission-migration-session").unwrap();
    let task_id = TaskId::new();
    let ignored = sctx_domain::ContextId::new();
    let reused = sctx_domain::ContextId::new();
    let refuted = sctx_domain::ContextId::new();
    {
        let runtime = TaskRuntime::initialize(&root).unwrap();
        runtime
            .open_or_create(
                locator.clone(),
                task_id,
                intent("survive an omission reset"),
                Vec::new(),
            )
            .unwrap();
        runtime
            .record_context_usage(&[
                sctx_task_runtime::ContextUsageRecord {
                    context_id: ignored,
                    task_id,
                    outcome: sctx_task_runtime::ContextUsageOutcome::Ignored,
                },
                sctx_task_runtime::ContextUsageRecord {
                    context_id: reused,
                    task_id,
                    outcome: sctx_task_runtime::ContextUsageOutcome::Reused,
                },
                sctx_task_runtime::ContextUsageRecord {
                    context_id: refuted,
                    task_id,
                    outcome: sctx_task_runtime::ContextUsageOutcome::Refuted,
                },
            ])
            .unwrap();
    }

    Connection::open(root.join("state").join("runtime.sqlite"))
        .unwrap()
        .execute_batch("PRAGMA user_version = 15;")
        .unwrap();

    let runtime = TaskRuntime::initialize(&root).unwrap();
    assert_eq!(
        Connection::open(runtime.database_path())
            .unwrap()
            .query_row::<i64, _, _>("PRAGMA user_version", [], |row| row.get(0))
            .unwrap(),
        18
    );
    let totals = runtime
        .context_usage_totals(&[ignored, reused, refuted])
        .unwrap();
    assert!(
        !totals.contains_key(&ignored),
        "an omission the old rule decided is not a verdict this version stands behind"
    );
    assert_eq!(
        totals[&reused],
        sctx_task_runtime::ContextUsageTotals {
            reused: 1,
            ignored: 0,
            refuted: 0,
        },
        "proven reuse survives: no signal was removed"
    );
    assert_eq!(
        totals[&refuted],
        sctx_task_runtime::ContextUsageTotals {
            reused: 0,
            ignored: 0,
            refuted: 1,
        },
        "a refutation is an Agent-stated contradiction and survives"
    );
}

/// Version 16 -> 17 adds disposition provenance without touching one decided Review.
///
/// Both changes are additive: `candidate_review.decision_source` starts `NULL` on every row that
/// already existed, and `auto_confirm_rejection` starts empty. A `NULL` reads back as `human`,
/// which is exactly what those dispositions were — `agent_policy` did not exist when they were
/// written, so there is nothing to guess and nothing to rewrite.
#[test]
fn schema_version_16_adds_disposition_provenance_without_rewriting_a_decision() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");
    let locator = ExternalSessionLocator::new("codex", "provenance-migration-session").unwrap();
    let task_id = TaskId::new();
    let task_session_id = {
        let runtime = TaskRuntime::initialize(&root).unwrap();
        runtime
            .open_or_create(
                locator.clone(),
                task_id,
                intent("survive a provenance upgrade"),
                Vec::new(),
            )
            .unwrap()
            .snapshot
            .task_session_id
    };
    let database_path = root.join("state").join("runtime.sqlite");

    // A version 16 installation exactly: one already-decided Review, and neither version 17
    // addition present.
    {
        let connection = Connection::open(&database_path).unwrap();
        connection.execute("PRAGMA foreign_keys = OFF", []).unwrap();
        connection
            .execute(
                "INSERT INTO candidate_review (
                    candidate_id, submission_id, episode_id, task_session_id, task_id,
                    build_id, final_checkpoint_id, checkpoint_id, claim_id, review_version,
                    status, discard_reason, created_at_unix_seconds, expires_at_unix_seconds,
                    discarded_at_unix_seconds
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 2, 'discarded', 'process detail',
                           1000, 2000, 1500)",
                params![
                    "candidate-provenance-fixture",
                    "submission-provenance-fixture",
                    "episode-provenance-fixture",
                    task_session_id.to_string(),
                    task_id.to_string(),
                    "build-provenance-fixture",
                    "checkpoint-provenance-fixture",
                    "checkpoint-provenance-fixture",
                    "claim-provenance-fixture",
                ],
            )
            .unwrap();
        connection
            .execute_batch(
                "DROP INDEX IF EXISTS auto_confirm_rejection_recorded_at;
                 DROP TABLE IF EXISTS auto_confirm_rejection;
                 ALTER TABLE candidate_review DROP COLUMN decision_source;
                 PRAGMA user_version = 16;",
            )
            .unwrap();
    }

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let connection = Connection::open(runtime.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row::<i64, _, _>("PRAGMA user_version", [], |row| row.get(0))
            .unwrap(),
        18
    );
    let rejections_exist: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'auto_confirm_rejection')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        rejections_exist,
        "migration must create the refusal counter table"
    );

    let (status, reason, decision_source): (String, String, Option<String>) = connection
        .query_row(
            "SELECT status, discard_reason, decision_source FROM candidate_review
             WHERE candidate_id = ?1",
            params!["candidate-provenance-fixture"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "discarded", "the decision itself is untouched");
    assert_eq!(reason, "process detail");
    assert_eq!(
        decision_source, None,
        "a Review decided before the column existed carries no invented provenance"
    );

    let totals = runtime.candidate_disposition_stats().unwrap();
    assert_eq!(
        totals.human.discarded, 1,
        "a NULL provenance reads back as the human decision it was"
    );
    assert_eq!(totals.agent_policy.discarded, 0);
    assert_eq!(totals.auto_confirm_not_permitted, 0);
}

/// Version 17 -> 18 adds the two `external_session` counters the `TurnStop` checkpoint reminder
/// gate uses (WP-V6 fix 3) and touches nothing else.
///
/// Unlike the other fixtures in this file, `external_session`'s *shape* changed, so writing rows
/// through the current schema and only rewinding `user_version` would not reproduce a genuine
/// pre-migration database -- the columns would already be there before the migration ever ran.
/// This rebuilds `external_session` in its true pre-migration shape (no reminder columns) to
/// prove the `ADD COLUMN` path itself, not the migration's defensive skip of a column that
/// already exists.
#[test]
fn schema_version_17_adds_the_checkpoint_reminder_counters_at_zero() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");
    let locator = ExternalSessionLocator::new("codex", "reminder-migration-session").unwrap();
    let task_id = TaskId::new();
    let task_session_id = {
        let runtime = TaskRuntime::initialize(&root).unwrap();
        runtime
            .open_or_create(
                locator.clone(),
                task_id,
                intent("survive a reminder-counter upgrade"),
                Vec::new(),
            )
            .unwrap()
            .snapshot
            .task_session_id
    };

    let database_path = root.join("state").join("runtime.sqlite");
    {
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 CREATE TABLE external_session_v17 (
                     external_session_id TEXT PRIMARY KEY,
                     agent_kind TEXT NOT NULL,
                     external_session_key TEXT NOT NULL,
                     active_task_session_id TEXT NOT NULL,
                     active_task_id TEXT NOT NULL,
                     UNIQUE (agent_kind, external_session_key),
                     FOREIGN KEY (external_session_id, active_task_session_id, active_task_id)
                         REFERENCES task_session (external_session_id, task_session_id, task_id)
                         DEFERRABLE INITIALLY DEFERRED
                 ) STRICT;
                 INSERT INTO external_session_v17
                     SELECT external_session_id, agent_kind, external_session_key,
                            active_task_session_id, active_task_id
                     FROM external_session;
                 DROP TABLE external_session;
                 ALTER TABLE external_session_v17 RENAME TO external_session;
                 PRAGMA user_version = 17;",
            )
            .unwrap();
        let has_reminder_columns: bool = connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM pragma_table_info('external_session')
                     WHERE name = 'checkpoint_reminder_count'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !has_reminder_columns,
            "fixture must reproduce a genuine version 17 external_session, without the reminder \
             columns"
        );
    }

    let runtime = TaskRuntime::initialize(&root).unwrap();
    let connection = Connection::open(runtime.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row::<i64, _, _>("PRAGMA user_version", [], |row| row.get(0))
            .unwrap(),
        18,
        "migration must chain through to the current version"
    );
    let (reminder_count, activity): (i64, i64) = connection
        .query_row(
            "SELECT checkpoint_reminder_count, activity_since_checkpoint_reminder
             FROM external_session WHERE active_task_session_id = ?1",
            params![task_session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (reminder_count, activity),
        (0, 0),
        "a migrated pre-existing Session starts with a fresh reminder budget"
    );

    // The migrated row works through the public gate/activity API exactly like a fresh row would.
    assert!(
        runtime.gate_turn_stop_checkpoint_reminder(&locator),
        "the first reminder on a migrated row still fires unconditionally"
    );
    runtime
        .record_checkpoint_reminder_activity(&locator)
        .unwrap();
    assert_eq!(
        runtime
            .read_snapshot_by_locator(&locator)
            .unwrap()
            .unwrap()
            .task_id,
        task_id,
        "the migrated row's Task identity is untouched"
    );
}

/// Re-running the additive migrations over a database that already has every column must not fail.
///
/// `ADD COLUMN` has no `IF NOT EXISTS`, so both additive migrations guard on the column itself. A
/// stamp rewound by hand — the same shape an interrupted upgrade leaves behind — must still
/// migrate.
#[test]
fn schema_migrations_are_reentrant_over_existing_columns() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");
    let locator = ExternalSessionLocator::new("codex", "reentrant-migration-session").unwrap();
    {
        let runtime = TaskRuntime::initialize(&root).unwrap();
        runtime
            .open_or_create(
                locator.clone(),
                TaskId::new(),
                intent("survive a repeated upgrade"),
                Vec::new(),
            )
            .unwrap();
    }
    Connection::open(root.join("state").join("runtime.sqlite"))
        .unwrap()
        .execute_batch("PRAGMA user_version = 16;")
        .unwrap();

    let runtime = TaskRuntime::initialize(&root).unwrap();
    assert_eq!(
        Connection::open(runtime.database_path())
            .unwrap()
            .query_row::<i64, _, _>("PRAGMA user_version", [], |row| row.get(0))
            .unwrap(),
        18
    );
    assert!(
        runtime
            .read_snapshot_by_locator(&locator)
            .unwrap()
            .is_some()
    );
}
