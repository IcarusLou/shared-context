//! Local, disposable runtime state for external Agent sessions and explicit Tasks.
//!
//! This crate owns only `state/runtime.sqlite`. It has no dependency on the
//! Context Git Store or rebuildable knowledge index. Task boundaries are
//! explicit: the runtime never guesses a new Task from Prompt or Workspace text.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sctx_domain::{
    ArtifactLocator, Error, ErrorKind, ExternalSessionId, ExternalSessionLocator,
    ExternalSessionSnapshot, RepositoryId, Result, SignalId, TaskArtifactFocus,
    TaskArtifactFocusRecord, TaskId, TaskIntent, TaskIntentDraft, TaskIntentRevision,
    TaskIntentRevisionId, TaskSessionId, TaskSessionSnapshot, TaskSignal, TaskSignalKind,
    TaskSignalLifecycle, TaskSignalRecord,
};

const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of atomically locating or creating one `ExternalSession`'s first Task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSessionOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub created: bool,
}

/// Result of atomically merging normalized Signals into the `ActiveTask`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeSignalsOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub inserted: usize,
    pub inserted_signal_ids: Vec<SignalId>,
}

/// Result of a CAS-guarded merge of structured Artifact Focuses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeArtifactFocusesOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub inserted: usize,
    pub inserted_signal_ids: Vec<SignalId>,
    pub focus_signal_ids: Vec<SignalId>,
}

/// Result of explicitly creating and activating a new Task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartNewTaskOutcome {
    pub external_session_id: ExternalSessionId,
    pub previous_task_id: TaskId,
    pub snapshot: TaskSessionSnapshot,
}

/// Result of explicitly selecting a retained Task as active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchActiveTaskOutcome {
    pub external_session_id: ExternalSessionId,
    pub previous_task_id: TaskId,
    pub snapshot: TaskSessionSnapshot,
    pub switched: bool,
}

/// Result of superseding identified Signals without deleting history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupersedeSignalsOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub superseded_signal_ids: Vec<SignalId>,
}

/// Owner of the installation-local `state/runtime.sqlite` database.
#[derive(Clone, Debug)]
pub struct TaskRuntime {
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
}

impl TaskRuntime {
    /// Initializes runtime state below `home/.shared-context`.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem, `SQLite`, or schema-version errors.
    pub fn initialize_for_home(home: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(home.as_ref().join(".shared-context"))
    }

    /// Initializes runtime state at an explicit installation root.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem, `SQLite`, or schema-version errors.
    pub fn initialize(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let state = root.join("state");
        fs::create_dir_all(&state).map_err(io_error("create task runtime state directory"))?;
        let runtime = Self {
            database: state.join("runtime.sqlite"),
            root,
            state,
        };
        let _connection = runtime.open_connection()?;
        Ok(runtime)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Opens the `ActiveTask` or creates an `ExternalSession` with its first Task.
    /// Existing Sessions are never implicitly switched to a new Task.
    ///
    /// # Errors
    ///
    /// Returns typed validation or storage errors.
    pub fn open_or_create(
        &self,
        locator: ExternalSessionLocator,
        initial_intent: TaskIntent,
        signals: Vec<TaskSignal>,
    ) -> Result<OpenSessionOutcome> {
        locator.validate()?;
        initial_intent.validate()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin open-or-create transaction")?;
        if let Some(task_session_id) = find_active_task_by_locator(&transaction, &locator)? {
            let snapshot = require_snapshot(&transaction, task_session_id)?;
            transaction
                .commit()
                .map_err(sql_error("commit existing ExternalSession transaction"))?;
            return Ok(OpenSessionOutcome {
                snapshot,
                created: false,
            });
        }

        let external_session_id = ExternalSessionId::new();
        let snapshot = TaskSessionSnapshot::from_initial(locator, initial_intent, signals)?;
        insert_external_session(
            &transaction,
            external_session_id,
            &snapshot.external_session_locator,
            &snapshot,
        )?;
        insert_task(&transaction, external_session_id, 0, &snapshot)?;
        let persisted = require_snapshot(&transaction, snapshot.task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit new ExternalSession transaction"))?;
        Ok(OpenSessionOutcome {
            snapshot: persisted,
            created: true,
        })
    }

    /// Explicitly creates and activates a new runtime-owned Task.
    ///
    /// # Errors
    ///
    /// Returns an input error for a missing Session, stale `ActiveTask` CAS guard,
    /// or invalid Task content and Signals.
    pub fn start_new_task(
        &self,
        locator: &ExternalSessionLocator,
        expected_active_task_id: TaskId,
        initial_intent: &TaskIntentDraft,
        signals: Vec<TaskSignal>,
    ) -> Result<StartNewTaskOutcome> {
        locator.validate()?;
        initial_intent.validate()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin new Task transaction")?;
        let external = read_external_identity(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSessionLocator does not identify a runtime Session"))?;
        require_expected_active(external.active_task_id, expected_active_task_id)?;

        let task_id = TaskId::new();
        let snapshot = TaskSessionSnapshot::from_initial(
            locator.clone(),
            initial_intent.bind(task_id),
            signals,
        )?;
        let ordinal = next_task_ordinal(&transaction, external.external_session_id)?;
        insert_task(
            &transaction,
            external.external_session_id,
            ordinal,
            &snapshot,
        )?;
        compare_and_switch(
            &transaction,
            external.external_session_id,
            expected_active_task_id,
            snapshot.task_session_id,
            snapshot.task_id,
        )?;
        let persisted = require_snapshot(&transaction, snapshot.task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit new Task transaction"))?;
        Ok(StartNewTaskOutcome {
            external_session_id: external.external_session_id,
            previous_task_id: expected_active_task_id,
            snapshot: persisted,
        })
    }

    /// Explicitly switches to a retained historical Task using an `ActiveTask` CAS guard.
    ///
    /// # Errors
    ///
    /// Returns an input error for missing/stale/cross-Session identities.
    pub fn switch_active_task(
        &self,
        locator: &ExternalSessionLocator,
        expected_active_task_id: TaskId,
        target_task_id: TaskId,
    ) -> Result<SwitchActiveTaskOutcome> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin ActiveTask switch transaction")?;
        let external = read_external_identity(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSessionLocator does not identify a runtime Session"))?;
        require_expected_active(external.active_task_id, expected_active_task_id)?;
        let target_task_session_id = find_task_in_external_session(
            &transaction,
            external.external_session_id,
            target_task_id,
        )?
        .ok_or_else(|| invalid("target_task_id is not retained by this ExternalSession"))?;
        let switched = target_task_id != expected_active_task_id;
        if switched {
            compare_and_switch(
                &transaction,
                external.external_session_id,
                expected_active_task_id,
                target_task_session_id,
                target_task_id,
            )?;
        }
        let snapshot = require_snapshot(&transaction, target_task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit ActiveTask switch transaction"))?;
        Ok(SwitchActiveTaskOutcome {
            external_session_id: external.external_session_id,
            previous_task_id: expected_active_task_id,
            snapshot,
            switched,
        })
    }

    /// Appends one Intent revision only to the current `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Returns an input error for inactive/cross-Task/stale-parent data.
    pub fn append_intent_revision(
        &self,
        task_session_id: TaskSessionId,
        parent_revision_id: TaskIntentRevisionId,
        intent: TaskIntent,
    ) -> Result<TaskIntentRevision> {
        intent.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Intent append transaction")?;
        let (task_id, current_revision_id) = read_active_task_head(&transaction, task_session_id)?
            .ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        if intent.task_id != task_id {
            return Err(invalid(
                "Task Intent must belong to the Session task selected as ActiveTask",
            ));
        }
        if parent_revision_id != current_revision_id {
            reject_invalid_parent(&transaction, task_session_id, task_id, parent_revision_id)?;
        }
        let revision = TaskIntentRevision {
            revision_id: TaskIntentRevisionId::new(),
            parent_revision_id: Some(parent_revision_id),
            intent,
        };
        revision.validate()?;
        let ordinal = read_revision_ordinal(&transaction, current_revision_id)?
            .checked_add(1)
            .ok_or_else(|| invariant("Task Intent revision ordinal overflow"))?;
        insert_intent_revision(&transaction, task_session_id, ordinal, &revision)?;
        let changed = transaction
            .execute(
                "UPDATE task_session SET current_intent_revision_id = ?1
                 WHERE task_session_id = ?2 AND current_intent_revision_id = ?3",
                params![
                    revision.revision_id.to_string(),
                    task_session_id.to_string(),
                    parent_revision_id.to_string(),
                ],
            )
            .map_err(sql_error("advance Task Intent Head"))?;
        if changed != 1 {
            return Err(invariant(
                "Task Intent Head changed inside write transaction",
            ));
        }
        transaction
            .commit()
            .map_err(sql_error("commit Intent append transaction"))?;
        Ok(revision)
    }

    /// Merges normalized Signals only into the current `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Returns an input error for an inactive Task or invalid Signal.
    pub fn merge_signals(
        &self,
        task_session_id: TaskSessionId,
        signals: Vec<TaskSignal>,
    ) -> Result<MergeSignalsOutcome> {
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Signal merge transaction")?;
        let (task_id, _) =
            read_active_task_head(&transaction, task_session_id)?.ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        let outcome =
            merge_signals_in_transaction(&transaction, task_session_id, task_id, &signals)?;
        transaction
            .commit()
            .map_err(sql_error("commit Signal merge transaction"))?;
        Ok(outcome)
    }

    /// Finds one `ExternalSession` and merges Signals into its `ActiveTask`.
    /// A missing locator returns `None` and never creates a Task.
    ///
    /// # Errors
    ///
    /// Returns typed input or storage errors.
    pub fn merge_signals_by_locator(
        &self,
        locator: &ExternalSessionLocator,
        signals: Vec<TaskSignal>,
    ) -> Result<Option<MergeSignalsOutcome>> {
        locator.validate()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin locator Signal merge transaction")?;
        let Some(task_session_id) = find_active_task_by_locator(&transaction, locator)? else {
            transaction
                .commit()
                .map_err(sql_error("commit missing locator transaction"))?;
            return Ok(None);
        };
        let (task_id, _) = read_active_task_head(&transaction, task_session_id)?
            .ok_or_else(|| invariant("located ActiveTask is not active"))?;
        let outcome =
            merge_signals_in_transaction(&transaction, task_session_id, task_id, &signals)?;
        transaction
            .commit()
            .map_err(sql_error("commit locator Signal merge transaction"))?;
        Ok(Some(outcome))
    }

    /// CAS-merges structured Artifact Focuses into the current `ActiveTask`
    /// without creating an Intent revision.
    ///
    /// # Errors
    ///
    /// Returns an input error for an inactive/stale Task or Intent guard, or
    /// invalid Focus coordinates.
    pub fn merge_artifact_focuses(
        &self,
        task_session_id: TaskSessionId,
        expected_active_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
        focuses: Vec<TaskArtifactFocus>,
    ) -> Result<MergeArtifactFocusesOutcome> {
        let focuses = normalize_artifact_focuses(focuses)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Artifact Focus merge transaction")?;
        let (task_id, current_revision_id) = read_active_task_head(&transaction, task_session_id)?
            .ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        require_expected_active(task_id, expected_active_task_id)?;
        if current_revision_id != expected_intent_revision_id {
            return Err(invalid(format!(
                "expected_intent_revision_id is stale; current Intent Head is {current_revision_id}"
            )));
        }
        let outcome = merge_artifact_focuses_in_transaction(
            &transaction,
            task_session_id,
            task_id,
            &focuses,
        )?;
        transaction
            .commit()
            .map_err(sql_error("commit Artifact Focus merge transaction"))?;
        Ok(outcome)
    }

    /// Supersedes stable Signal IDs without deleting history.
    ///
    /// # Errors
    ///
    /// Returns an input error for stale Task identity or invalid Signal IDs.
    pub fn supersede_signals(
        &self,
        task_session_id: TaskSessionId,
        expected_active_task_id: TaskId,
        signal_ids: Vec<SignalId>,
    ) -> Result<SupersedeSignalsOutcome> {
        require_unique_signal_ids(&signal_ids)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Signal supersede transaction")?;
        let (task_id, _) =
            read_active_task_head(&transaction, task_session_id)?.ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        require_expected_active(task_id, expected_active_task_id)?;
        for signal_id in &signal_ids {
            if let Some(record) = read_signal_record(&transaction, *signal_id)? {
                record.validate_for_task(task_session_id, task_id)?;
                if record.lifecycle != TaskSignalLifecycle::Active {
                    return Err(invalid(format!(
                        "Signal is already superseded: {signal_id}"
                    )));
                }
            } else if let Some(record) = read_artifact_focus_record(&transaction, *signal_id)? {
                record.validate_for_task(task_session_id, task_id)?;
                if record.lifecycle != TaskSignalLifecycle::Active {
                    return Err(invalid(format!(
                        "Signal is already superseded: {signal_id}"
                    )));
                }
            } else {
                return Err(invalid(format!("Signal does not exist: {signal_id}")));
            }
        }
        for signal_id in &signal_ids {
            let mut changed = transaction
                .execute(
                    "UPDATE task_signal SET lifecycle = 'superseded'
                     WHERE signal_id = ?1 AND lifecycle = 'active'",
                    [signal_id.to_string()],
                )
                .map_err(sql_error("supersede Task Signal"))?;
            if changed == 0 {
                changed = transaction
                    .execute(
                        "UPDATE task_artifact_focus SET lifecycle = 'superseded'
                         WHERE signal_id = ?1 AND lifecycle = 'active'",
                        [signal_id.to_string()],
                    )
                    .map_err(sql_error("supersede Task Artifact Focus"))?;
            }
            if changed != 1 {
                return Err(invariant(
                    "Signal lifecycle changed inside write transaction",
                ));
            }
        }
        let snapshot = require_snapshot(&transaction, task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Signal supersede transaction"))?;
        Ok(SupersedeSignalsOutcome {
            snapshot,
            superseded_signal_ids: signal_ids,
        })
    }

    /// Reads any retained Task by `TaskSessionId`.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_snapshot(
        &self,
        task_session_id: TaskSessionId,
    ) -> Result<Option<TaskSessionSnapshot>> {
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin Task snapshot transaction"))?;
        let snapshot = read_snapshot_in_transaction(&transaction, task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Task snapshot transaction"))?;
        Ok(snapshot)
    }

    /// Reads only the `ActiveTask` selected by an `ExternalSessionLocator`.
    ///
    /// # Errors
    ///
    /// Returns typed input, storage, or invariant errors.
    pub fn read_snapshot_by_locator(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<Option<TaskSessionSnapshot>> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin ActiveTask snapshot transaction"))?;
        let snapshot = find_active_task_by_locator(&transaction, locator)?
            .map(|id| read_snapshot_in_transaction(&transaction, id))
            .transpose()?
            .flatten();
        transaction
            .commit()
            .map_err(sql_error("commit ActiveTask snapshot transaction"))?;
        Ok(snapshot)
    }

    /// Reads one `ExternalSession` with its `ActiveTask` and all historical Tasks.
    ///
    /// # Errors
    ///
    /// Returns typed input, storage, or invariant errors.
    pub fn read_external_session_by_locator(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<Option<ExternalSessionSnapshot>> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin ExternalSession snapshot transaction"))?;
        let snapshot = read_external_session_in_transaction(&transaction, locator)?;
        transaction
            .commit()
            .map_err(sql_error("commit ExternalSession snapshot transaction"))?;
        Ok(snapshot)
    }

    /// Reads active and superseded Signal records for any retained Task.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_signal_history(
        &self,
        task_session_id: TaskSessionId,
    ) -> Result<Vec<TaskSignalRecord>> {
        read_signal_records(&self.open_connection()?, task_session_id)
    }

    /// Reads active and superseded Artifact Focus records for any retained Task.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_artifact_focus_history(
        &self,
        task_session_id: TaskSessionId,
    ) -> Result<Vec<TaskArtifactFocusRecord>> {
        read_artifact_focus_records(&self.open_connection()?, task_session_id)
    }

    fn open_connection(&self) -> Result<Connection> {
        let connection =
            Connection::open(&self.database).map_err(sql_error("open task runtime database"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(sql_error("configure task runtime busy timeout"))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error("configure task runtime journal mode"))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_error("configure task runtime synchronous mode"))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_error("enable task runtime foreign keys"))?;
        ensure_schema(&connection)?;
        Ok(connection)
    }
}

#[derive(Clone, Copy)]
#[allow(clippy::struct_field_names)]
struct ExternalIdentity {
    external_session_id: ExternalSessionId,
    active_task_session_id: TaskSessionId,
    active_task_id: TaskId,
}

fn immediate<'a>(connection: &'a mut Connection, context: &'static str) -> Result<Transaction<'a>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error(context))
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(sql_error("read task runtime schema version"))?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(invariant(format!(
            "unsupported task runtime schema version {version}; expected {SCHEMA_VERSION}"
        )));
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS external_session (
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
            CREATE TABLE IF NOT EXISTS task_session (
                task_session_id TEXT PRIMARY KEY,
                external_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL UNIQUE,
                current_intent_revision_id TEXT NOT NULL,
                task_ordinal INTEGER NOT NULL CHECK (task_ordinal >= 0),
                UNIQUE (external_session_id, task_session_id, task_id),
                UNIQUE (external_session_id, task_ordinal),
                FOREIGN KEY (external_session_id) REFERENCES external_session (external_session_id),
                FOREIGN KEY (task_session_id, current_intent_revision_id)
                    REFERENCES task_intent_revision (task_session_id, revision_id)
                    DEFERRABLE INITIALLY DEFERRED
            ) STRICT;
            CREATE TABLE IF NOT EXISTS task_intent_revision (
                task_session_id TEXT NOT NULL,
                revision_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                parent_revision_id TEXT,
                revision_ordinal INTEGER NOT NULL CHECK (revision_ordinal >= 0),
                intent_json TEXT NOT NULL CHECK (json_valid(intent_json)),
                UNIQUE (task_session_id, revision_id),
                UNIQUE (task_session_id, revision_ordinal),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id),
                FOREIGN KEY (task_session_id, parent_revision_id)
                    REFERENCES task_intent_revision (task_session_id, revision_id)
            ) STRICT;
            CREATE UNIQUE INDEX IF NOT EXISTS task_intent_one_initial
                ON task_intent_revision (task_session_id) WHERE parent_revision_id IS NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS task_intent_one_child_per_parent
                ON task_intent_revision (task_session_id, parent_revision_id)
                WHERE parent_revision_id IS NOT NULL;
            CREATE TABLE IF NOT EXISTS task_signal (
                signal_id TEXT PRIMARY KEY,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                lifecycle TEXT NOT NULL CHECK (lifecycle IN ('active', 'superseded')),
                signal_ordinal INTEGER NOT NULL CHECK (signal_ordinal >= 0),
                UNIQUE (task_session_id, signal_ordinal),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id)
            ) STRICT;
            CREATE UNIQUE INDEX IF NOT EXISTS task_signal_one_active_semantic
                ON task_signal (task_session_id, kind, content)
                WHERE lifecycle = 'active';
            CREATE TABLE IF NOT EXISTS task_artifact_focus (
                signal_id TEXT PRIMARY KEY,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                repository_id TEXT NOT NULL,
                locator_json TEXT NOT NULL CHECK (json_valid(locator_json)),
                lifecycle TEXT NOT NULL CHECK (lifecycle IN ('active', 'superseded')),
                focus_ordinal INTEGER NOT NULL CHECK (focus_ordinal >= 0),
                UNIQUE (task_session_id, focus_ordinal),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id)
            ) STRICT;
            CREATE UNIQUE INDEX IF NOT EXISTS task_artifact_focus_one_active_identity
                ON task_artifact_focus (task_session_id, repository_id, locator_json)
                WHERE lifecycle = 'active';
            PRAGMA user_version = 3;",
        )
        .map_err(sql_error("initialize task runtime schema"))
}

fn insert_external_session(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    locator: &ExternalSessionLocator,
    task: &TaskSessionSnapshot,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO external_session (
                external_session_id, agent_kind, external_session_key,
                active_task_session_id, active_task_id
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                external_session_id.to_string(),
                locator.agent_kind,
                locator.external_session_id,
                task.task_session_id.to_string(),
                task.task_id.to_string(),
            ],
        )
        .map_err(sql_error("insert ExternalSession"))?;
    Ok(())
}

fn insert_task(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    task_ordinal: i64,
    snapshot: &TaskSessionSnapshot,
) -> Result<()> {
    snapshot.validate()?;
    let revision = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("initial Task has no Intent revision"))?;
    transaction
        .execute(
            "INSERT INTO task_session (
                task_session_id, external_session_id, task_id,
                current_intent_revision_id, task_ordinal
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                snapshot.task_session_id.to_string(),
                external_session_id.to_string(),
                snapshot.task_id.to_string(),
                revision.revision_id.to_string(),
                task_ordinal,
            ],
        )
        .map_err(sql_error("insert Task Session"))?;
    insert_intent_revision(transaction, snapshot.task_session_id, 0, revision)?;
    let _outcome = merge_signals_in_transaction(
        transaction,
        snapshot.task_session_id,
        snapshot.task_id,
        &snapshot.task_signals,
    )?;
    Ok(())
}

fn insert_intent_revision(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    ordinal: i64,
    revision: &TaskIntentRevision,
) -> Result<()> {
    let intent_json = serde_json::to_string(&revision.intent)
        .map_err(json_error("serialize Task Intent revision"))?;
    transaction
        .execute(
            "INSERT INTO task_intent_revision (
                task_session_id, revision_id, task_id, parent_revision_id,
                revision_ordinal, intent_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task_session_id.to_string(),
                revision.revision_id.to_string(),
                revision.task_id().to_string(),
                revision.parent_revision_id.map(|value| value.to_string()),
                ordinal,
                intent_json,
            ],
        )
        .map_err(sql_error("insert Task Intent revision"))?;
    Ok(())
}

fn merge_signals_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    signals: &[TaskSignal],
) -> Result<MergeSignalsOutcome> {
    let mut inserted_signal_ids = Vec::new();
    let mut next_ordinal = next_signal_ordinal(transaction, task_session_id)?;
    for signal in signals {
        if find_active_signal(transaction, task_session_id, signal)?.is_some() {
            continue;
        }
        let signal_id = SignalId::new();
        transaction
            .execute(
                "INSERT INTO task_signal (
                    signal_id, task_session_id, task_id, kind, content,
                    lifecycle, signal_ordinal
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
                params![
                    signal_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                    signal_kind_name(signal.kind),
                    signal.content,
                    next_ordinal,
                ],
            )
            .map_err(sql_error("insert active Task Signal"))?;
        inserted_signal_ids.push(signal_id);
        next_ordinal = next_ordinal
            .checked_add(1)
            .ok_or_else(|| invariant("Task Signal ordinal overflow"))?;
    }
    let snapshot = require_snapshot(transaction, task_session_id)?;
    Ok(MergeSignalsOutcome {
        snapshot,
        inserted: inserted_signal_ids.len(),
        inserted_signal_ids,
    })
}

fn find_active_signal(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    signal: &TaskSignal,
) -> Result<Option<SignalId>> {
    transaction
        .query_row(
            "SELECT signal_id FROM task_signal
             WHERE task_session_id = ?1 AND kind = ?2 AND content = ?3
               AND lifecycle = 'active'",
            params![
                task_session_id.to_string(),
                signal_kind_name(signal.kind),
                signal.content,
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find active Task Signal"))?
        .map(|value| parse_id(&value, "task_signal.signal_id"))
        .transpose()
}

fn merge_artifact_focuses_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    focuses: &[TaskArtifactFocus],
) -> Result<MergeArtifactFocusesOutcome> {
    let mut inserted_signal_ids = Vec::new();
    let mut focus_signal_ids = Vec::with_capacity(focuses.len());
    let mut next_ordinal = next_artifact_focus_ordinal(transaction, task_session_id)?;
    for focus in focuses {
        let locator_json = serde_json::to_string(&focus.locator)
            .map_err(json_error("serialize Task Artifact Focus locator"))?;
        if let Some(signal_id) = find_active_artifact_focus(
            transaction,
            task_session_id,
            focus.repository_id,
            &locator_json,
        )? {
            focus_signal_ids.push(signal_id);
            continue;
        }
        let signal_id = SignalId::new();
        transaction
            .execute(
                "INSERT INTO task_artifact_focus (
                    signal_id, task_session_id, task_id, repository_id,
                    locator_json, lifecycle, focus_ordinal
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
                params![
                    signal_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                    focus.repository_id.to_string(),
                    locator_json,
                    next_ordinal,
                ],
            )
            .map_err(sql_error("insert active Task Artifact Focus"))?;
        inserted_signal_ids.push(signal_id);
        focus_signal_ids.push(signal_id);
        next_ordinal = next_ordinal
            .checked_add(1)
            .ok_or_else(|| invariant("Task Artifact Focus ordinal overflow"))?;
    }
    let snapshot = require_snapshot(transaction, task_session_id)?;
    Ok(MergeArtifactFocusesOutcome {
        snapshot,
        inserted: inserted_signal_ids.len(),
        inserted_signal_ids,
        focus_signal_ids,
    })
}

fn find_active_artifact_focus(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    repository_id: RepositoryId,
    locator_json: &str,
) -> Result<Option<SignalId>> {
    transaction
        .query_row(
            "SELECT signal_id FROM task_artifact_focus
             WHERE task_session_id = ?1 AND repository_id = ?2 AND locator_json = ?3
               AND lifecycle = 'active'",
            params![
                task_session_id.to_string(),
                repository_id.to_string(),
                locator_json
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find active Task Artifact Focus"))?
        .map(|value| parse_id(&value, "task_artifact_focus.signal_id"))
        .transpose()
}

fn next_artifact_focus_ordinal(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<i64> {
    transaction
        .query_row(
            "SELECT COALESCE(MAX(focus_ordinal), -1) + 1
             FROM task_artifact_focus WHERE task_session_id = ?1",
            [task_session_id.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error("read next Artifact Focus ordinal"))
}

fn read_external_identity(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<ExternalIdentity>> {
    transaction
        .query_row(
            "SELECT external_session_id, active_task_session_id, active_task_id
             FROM external_session
             WHERE agent_kind = ?1 AND external_session_key = ?2",
            params![locator.agent_kind, locator.external_session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read ExternalSession identity"))?
        .map(|(external_session_id, task_session_id, task_id)| {
            Ok(ExternalIdentity {
                external_session_id: parse_id(
                    &external_session_id,
                    "external_session.external_session_id",
                )?,
                active_task_session_id: parse_id(
                    &task_session_id,
                    "external_session.active_task_session_id",
                )?,
                active_task_id: parse_id(&task_id, "external_session.active_task_id")?,
            })
        })
        .transpose()
}

fn find_active_task_by_locator(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<TaskSessionId>> {
    Ok(read_external_identity(transaction, locator)?.map(|value| value.active_task_session_id))
}

fn find_task_in_external_session(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    task_id: TaskId,
) -> Result<Option<TaskSessionId>> {
    transaction
        .query_row(
            "SELECT task_session_id FROM task_session
             WHERE external_session_id = ?1 AND task_id = ?2",
            params![external_session_id.to_string(), task_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find retained Task"))?
        .map(|value| parse_id(&value, "task_session.task_session_id"))
        .transpose()
}

fn require_expected_active(actual: TaskId, expected: TaskId) -> Result<()> {
    if actual != expected {
        return Err(invalid(
            "expected_active_task_id does not match the ExternalSession ActiveTask",
        ));
    }
    Ok(())
}

fn compare_and_switch(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    expected_active_task_id: TaskId,
    target_task_session_id: TaskSessionId,
    target_task_id: TaskId,
) -> Result<()> {
    let changed = transaction
        .execute(
            "UPDATE external_session
             SET active_task_session_id = ?1, active_task_id = ?2
             WHERE external_session_id = ?3 AND active_task_id = ?4",
            params![
                target_task_session_id.to_string(),
                target_task_id.to_string(),
                external_session_id.to_string(),
                expected_active_task_id.to_string(),
            ],
        )
        .map_err(sql_error("compare and switch ActiveTask"))?;
    if changed != 1 {
        return Err(invalid("expected_active_task_id became stale"));
    }
    Ok(())
}

fn read_active_task_head(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Option<(TaskId, TaskIntentRevisionId)>> {
    let row = transaction
        .query_row(
            "SELECT task.task_id, task.current_intent_revision_id
             FROM task_session task
             JOIN external_session external
               ON external.external_session_id = task.external_session_id
              AND external.active_task_session_id = task.task_session_id
              AND external.active_task_id = task.task_id
             WHERE task.task_session_id = ?1",
            [task_session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("read ActiveTask Head"))?;
    row.map(|(task_id, revision_id)| {
        Ok((
            parse_id(&task_id, "task_session.task_id")?,
            parse_id(&revision_id, "task_session.current_intent_revision_id")?,
        ))
    })
    .transpose()
}

fn reject_invalid_parent(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    parent_revision_id: TaskIntentRevisionId,
) -> Result<()> {
    let owner = transaction
        .query_row(
            "SELECT task_session_id, task_id FROM task_intent_revision WHERE revision_id = ?1",
            [parent_revision_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("inspect rejected Intent parent"))?;
    if let Some((owner_session, owner_task)) = owner {
        let owner_session: TaskSessionId = parse_id(&owner_session, "revision.task_session_id")?;
        let owner_task: TaskId = parse_id(&owner_task, "revision.task_id")?;
        if owner_session != task_session_id || owner_task != task_id {
            return Err(invalid("Intent parent belongs to another Task Session"));
        }
    }
    Err(invalid("Intent parent must be the ActiveTask current Head"))
}

fn next_task_ordinal(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(task_ordinal), -1) + 1 FROM task_session WHERE external_session_id = ?1",
        external_session_id.to_string(),
        "read next Task ordinal",
    )
}

fn next_signal_ordinal(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(signal_ordinal), -1) + 1 FROM task_signal WHERE task_session_id = ?1",
        task_session_id.to_string(),
        "read next Signal ordinal",
    )
}

fn next_ordinal(
    transaction: &Transaction<'_>,
    query: &str,
    identity: String,
    context: &'static str,
) -> Result<i64> {
    transaction
        .query_row(query, [identity], |row| row.get(0))
        .map_err(sql_error(context))
}

fn read_revision_ordinal(
    transaction: &Transaction<'_>,
    revision_id: TaskIntentRevisionId,
) -> Result<i64> {
    transaction
        .query_row(
            "SELECT revision_ordinal FROM task_intent_revision WHERE revision_id = ?1",
            [revision_id.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error("read Intent revision ordinal"))
}

fn require_snapshot(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<TaskSessionSnapshot> {
    read_snapshot_in_transaction(transaction, task_session_id)?
        .ok_or_else(|| invariant("Task disappeared inside runtime transaction"))
}

fn read_snapshot_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Option<TaskSessionSnapshot>> {
    let row = transaction
        .query_row(
            "SELECT task.task_id, external.agent_kind, external.external_session_key,
                    task.current_intent_revision_id
             FROM task_session task
             JOIN external_session external
               ON external.external_session_id = task.external_session_id
             WHERE task.task_session_id = ?1",
            [task_session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Task Session"))?;
    let Some((task_id, agent_kind, external_session_key, current_revision_id)) = row else {
        return Ok(None);
    };
    let task_id = parse_id(&task_id, "task_session.task_id")?;
    let current_revision_id = parse_id(&current_revision_id, "task_session.current_revision_id")?;
    let snapshot = TaskSessionSnapshot {
        task_session_id,
        task_id,
        external_session_locator: ExternalSessionLocator {
            agent_kind,
            external_session_id: external_session_key,
        },
        intent_revisions: read_intent_revisions(transaction, task_session_id)?,
        task_signals: read_active_task_signals(transaction, task_session_id)?,
        artifact_focuses: read_artifact_focus_records(transaction, task_session_id)?
            .into_iter()
            .filter(|record| record.lifecycle == TaskSignalLifecycle::Active)
            .collect(),
    };
    snapshot.validate().map_err(|error| {
        invariant(format!(
            "persisted Task violates contract: {}",
            error.message()
        ))
    })?;
    if snapshot
        .current_intent_revision()
        .is_none_or(|revision| revision.revision_id != current_revision_id)
    {
        return Err(invariant(
            "persisted Intent Head differs from revision chain",
        ));
    }
    Ok(Some(snapshot))
}

fn read_external_session_in_transaction(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<ExternalSessionSnapshot>> {
    let Some(identity) = read_external_identity(transaction, locator)? else {
        return Ok(None);
    };
    let mut statement = transaction
        .prepare(
            "SELECT task_session_id FROM task_session
             WHERE external_session_id = ?1 ORDER BY task_ordinal ASC",
        )
        .map_err(sql_error("prepare Task history"))?;
    let rows = statement
        .query_map([identity.external_session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error("query Task history"))?;
    let mut tasks = Vec::new();
    for row in rows {
        let task_session_id = parse_id(
            &row.map_err(sql_error("read Task history row"))?,
            "task_session.task_session_id",
        )?;
        tasks.push(require_snapshot(transaction, task_session_id)?);
    }
    let snapshot = ExternalSessionSnapshot {
        external_session_id: identity.external_session_id,
        locator: locator.clone(),
        active_task_session_id: identity.active_task_session_id,
        active_task_id: identity.active_task_id,
        tasks,
    };
    snapshot.validate().map_err(|error| {
        invariant(format!(
            "persisted ExternalSession violates contract: {}",
            error.message()
        ))
    })?;
    Ok(Some(snapshot))
}

fn read_intent_revisions(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskIntentRevision>> {
    let mut statement = transaction
        .prepare(
            "SELECT revision_id, parent_revision_id, task_id, intent_json
             FROM task_intent_revision
             WHERE task_session_id = ?1 ORDER BY revision_ordinal ASC",
        )
        .map_err(sql_error("prepare Intent revisions"))?;
    let rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error("query Intent revisions"))?;
    let mut revisions = Vec::new();
    for row in rows {
        let (revision_id, parent_revision_id, stored_task_id, intent_json) =
            row.map_err(sql_error("read Intent revision row"))?;
        let stored_task_id: TaskId = parse_id(&stored_task_id, "revision.task_id")?;
        let intent: TaskIntent =
            serde_json::from_str(&intent_json).map_err(json_error("parse Task Intent"))?;
        if stored_task_id != intent.task_id {
            return Err(invariant("Intent JSON identity differs from runtime row"));
        }
        revisions.push(TaskIntentRevision {
            revision_id: parse_id(&revision_id, "revision.revision_id")?,
            parent_revision_id: parent_revision_id
                .map(|value| parse_id(&value, "revision.parent_revision_id"))
                .transpose()?,
            intent,
        });
    }
    Ok(revisions)
}

fn read_active_task_signals(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignal>> {
    Ok(read_signal_records(connection, task_session_id)?
        .into_iter()
        .filter(|record| record.lifecycle == TaskSignalLifecycle::Active)
        .map(|record| record.signal)
        .collect())
}

fn read_signal_records(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignalRecord>> {
    let mut statement = connection
        .prepare(
            "SELECT signal_id, task_id, kind, content, lifecycle FROM task_signal
             WHERE task_session_id = ?1 ORDER BY signal_ordinal ASC",
        )
        .map_err(sql_error("prepare Signal history"))?;
    let rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(sql_error("query Signal history"))?;
    let mut records = Vec::new();
    for row in rows {
        let (signal_id, task_id, kind, content, lifecycle) =
            row.map_err(sql_error("read Signal history row"))?;
        let record = TaskSignalRecord {
            signal_id: parse_id(&signal_id, "signal.signal_id")?,
            task_session_id,
            task_id: parse_id(&task_id, "signal.task_id")?,
            signal: TaskSignal {
                kind: parse_signal_kind(&kind)?,
                content,
            },
            lifecycle: parse_signal_lifecycle(&lifecycle)?,
        };
        record.validate_for_task(task_session_id, record.task_id)?;
        records.push(record);
    }
    Ok(records)
}

fn read_artifact_focus_records(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskArtifactFocusRecord>> {
    let mut statement = connection
        .prepare(
            "SELECT signal_id, task_id, repository_id, locator_json, lifecycle
             FROM task_artifact_focus
             WHERE task_session_id = ?1 ORDER BY focus_ordinal ASC",
        )
        .map_err(sql_error("prepare Artifact Focus history"))?;
    let rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(sql_error("query Artifact Focus history"))?;
    let mut records = Vec::new();
    for row in rows {
        let (signal_id, task_id, repository_id, locator_json, lifecycle) =
            row.map_err(sql_error("read Artifact Focus history row"))?;
        let locator: ArtifactLocator = serde_json::from_str(&locator_json)
            .map_err(json_error("parse Artifact Focus locator"))?;
        let record = TaskArtifactFocusRecord {
            signal_id: parse_id(&signal_id, "task_artifact_focus.signal_id")?,
            task_session_id,
            task_id: parse_id(&task_id, "task_artifact_focus.task_id")?,
            focus: TaskArtifactFocus {
                repository_id: parse_id(&repository_id, "task_artifact_focus.repository_id")?,
                locator,
            },
            lifecycle: parse_signal_lifecycle(&lifecycle)?,
        };
        record.validate_for_task(task_session_id, record.task_id)?;
        records.push(record);
    }
    Ok(records)
}

fn read_artifact_focus_record(
    transaction: &Transaction<'_>,
    signal_id: SignalId,
) -> Result<Option<TaskArtifactFocusRecord>> {
    transaction
        .query_row(
            "SELECT task_session_id, task_id, repository_id, locator_json, lifecycle
             FROM task_artifact_focus WHERE signal_id = ?1",
            [signal_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Artifact Focus record"))?
        .map(
            |(task_session_id, task_id, repository_id, locator_json, lifecycle)| {
                Ok(TaskArtifactFocusRecord {
                    signal_id,
                    task_session_id: parse_id(
                        &task_session_id,
                        "task_artifact_focus.task_session_id",
                    )?,
                    task_id: parse_id(&task_id, "task_artifact_focus.task_id")?,
                    focus: TaskArtifactFocus {
                        repository_id: parse_id(
                            &repository_id,
                            "task_artifact_focus.repository_id",
                        )?,
                        locator: serde_json::from_str(&locator_json)
                            .map_err(json_error("parse Artifact Focus locator"))?,
                    },
                    lifecycle: parse_signal_lifecycle(&lifecycle)?,
                })
            },
        )
        .transpose()
}

fn read_signal_record(
    transaction: &Transaction<'_>,
    signal_id: SignalId,
) -> Result<Option<TaskSignalRecord>> {
    transaction
        .query_row(
            "SELECT task_session_id, task_id, kind, content, lifecycle
             FROM task_signal WHERE signal_id = ?1",
            [signal_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Signal record"))?
        .map(|(task_session_id, task_id, kind, content, lifecycle)| {
            Ok(TaskSignalRecord {
                signal_id,
                task_session_id: parse_id(&task_session_id, "signal.task_session_id")?,
                task_id: parse_id(&task_id, "signal.task_id")?,
                signal: TaskSignal {
                    kind: parse_signal_kind(&kind)?,
                    content,
                },
                lifecycle: parse_signal_lifecycle(&lifecycle)?,
            })
        })
        .transpose()
}

fn require_unique_signal_ids(signal_ids: &[SignalId]) -> Result<()> {
    if signal_ids.is_empty() {
        return Err(invalid("signal_ids must contain at least one SignalId"));
    }
    let mut unique = HashSet::with_capacity(signal_ids.len());
    if signal_ids
        .iter()
        .any(|signal_id| !unique.insert(*signal_id))
    {
        return Err(invalid("signal_ids must not contain duplicates"));
    }
    Ok(())
}

fn normalize_signals(signals: Vec<TaskSignal>) -> Result<Vec<TaskSignal>> {
    let mut normalized = Vec::with_capacity(signals.len());
    let mut seen = HashSet::with_capacity(signals.len());
    for signal in signals {
        let signal = TaskSignal {
            kind: signal.kind,
            content: signal.content.trim().to_owned(),
        };
        signal.validate()?;
        if seen.insert(signal.clone()) {
            normalized.push(signal);
        }
    }
    Ok(normalized)
}

fn normalize_artifact_focuses(focuses: Vec<TaskArtifactFocus>) -> Result<Vec<TaskArtifactFocus>> {
    let mut normalized = Vec::with_capacity(focuses.len());
    let mut seen = HashSet::with_capacity(focuses.len());
    for focus in focuses {
        focus.validate()?;
        if seen.insert(focus.clone()) {
            normalized.push(focus);
        }
    }
    Ok(normalized)
}

const fn signal_kind_name(kind: TaskSignalKind) -> &'static str {
    match kind {
        TaskSignalKind::Prompt => "prompt",
        TaskSignalKind::Workspace => "workspace",
        TaskSignalKind::Diff => "diff",
        TaskSignalKind::TestOutcome => "test_outcome",
    }
}

fn parse_signal_kind(value: &str) -> Result<TaskSignalKind> {
    match value {
        "prompt" => Ok(TaskSignalKind::Prompt),
        "workspace" => Ok(TaskSignalKind::Workspace),
        "diff" => Ok(TaskSignalKind::Diff),
        "test_outcome" => Ok(TaskSignalKind::TestOutcome),
        _ => Err(invariant(format!("unknown persisted Signal kind: {value}"))),
    }
}

fn parse_signal_lifecycle(value: &str) -> Result<TaskSignalLifecycle> {
    match value {
        "active" => Ok(TaskSignalLifecycle::Active),
        "superseded" => Ok(TaskSignalLifecycle::Superseded),
        _ => Err(invariant(format!(
            "unknown persisted Signal lifecycle: {value}"
        ))),
    }
}

fn parse_id<T>(value: &str, field: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| invariant(format!("persisted {field} is invalid: {error}")))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn json_error(context: &'static str) -> impl FnOnce(serde_json::Error) -> Error {
    move |error| Error::new(ErrorKind::InvariantViolation, format!("{context}: {error}"))
}
