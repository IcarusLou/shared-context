//! Local, disposable runtime state for isolated Agent task sessions.
//!
//! This crate owns only `state/runtime.sqlite`. It has no dependency on the
//! Context Git Store or the rebuildable knowledge index, so deleting its
//! database can lose Task state but cannot mutate durable knowledge facts.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sctx_domain::{
    Error, ErrorKind, ExternalSessionLocator, Result, TaskId, TaskIntent, TaskIntentRevision,
    TaskIntentRevisionId, TaskSessionId, TaskSessionSnapshot, TaskSignal, TaskSignalKind,
};

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of atomically locating or creating one Task Session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSessionOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub created: bool,
}

/// Result of atomically merging normalized Task Signals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeSignalsOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub inserted: usize,
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
    /// Returns [`ErrorKind::Io`] when the state directory or `SQLite` schema
    /// cannot be created, and [`ErrorKind::InvariantViolation`] for an
    /// unsupported existing schema version.
    pub fn initialize_for_home(home: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(home.as_ref().join(".shared-context"))
    }

    /// Initializes runtime state at an explicit Shared Context installation root.
    ///
    /// This method creates only the `state` directory and `runtime.sqlite`.
    /// It never opens or modifies the Context repository or `index.sqlite`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Io`] when the state directory or `SQLite` schema
    /// cannot be created, and [`ErrorKind::InvariantViolation`] for an
    /// unsupported existing schema version.
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

    /// Shared Context installation root containing this disposable runtime.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// State directory containing `runtime.sqlite`.
    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// Exact database path exclusively owned by this runtime.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Atomically opens the Session identified by a locator or creates it with
    /// one initial Intent revision and normalized Task Signals.
    ///
    /// Existing Sessions are returned unchanged. Call
    /// [`Self::append_intent_revision`] and [`Self::merge_signals`] for updates.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid locator, Intent, or
    /// signal, and an I/O or invariant error when runtime persistence fails.
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
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error("begin open-or-create Task Session transaction"))?;

        if let Some(task_session_id) = find_session_by_locator(&transaction, &locator)? {
            let snapshot = read_snapshot_in_transaction(&transaction, task_session_id)?
                .ok_or_else(|| invariant("located Task Session disappeared inside transaction"))?;
            transaction
                .commit()
                .map_err(sql_error("commit existing Task Session transaction"))?;
            return Ok(OpenSessionOutcome {
                snapshot,
                created: false,
            });
        }

        let snapshot = TaskSessionSnapshot::from_initial(locator, initial_intent, signals)?;
        insert_initial_session(&transaction, &snapshot)?;
        let persisted = read_snapshot_in_transaction(&transaction, snapshot.task_session_id)?
            .ok_or_else(|| invariant("new Task Session was not readable before commit"))?;
        transaction
            .commit()
            .map_err(sql_error("commit new Task Session transaction"))?;
        Ok(OpenSessionOutcome {
            snapshot: persisted,
            created: true,
        })
    }

    /// Atomically appends one Intent revision when `parent_revision_id` is the
    /// Session's current Head.
    ///
    /// Competing callers using the same parent cannot create branches: exactly
    /// one advances the Head and the rest receive a stale-parent error.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the Session is unknown, the
    /// parent is stale or belongs to another Task, or the Intent is invalid or
    /// belongs to another Task.
    pub fn append_intent_revision(
        &self,
        task_session_id: TaskSessionId,
        parent_revision_id: TaskIntentRevisionId,
        intent: TaskIntent,
    ) -> Result<TaskIntentRevision> {
        intent.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error("begin append Task Intent revision transaction"))?;
        let (task_id, current_revision_id) = read_session_head(&transaction, task_session_id)?
            .ok_or_else(|| invalid("task_session_id does not identify a runtime Session"))?;

        if intent.task_id != task_id {
            return Err(invalid(
                "task_intent_revision intent must belong to the Session task",
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
                    task_id.to_string(),
                    parent_revision_id.to_string(),
                    ordinal,
                    intent_json,
                ],
            )
            .map_err(sql_error("insert Task Intent revision"))?;
        let changed = transaction
            .execute(
                "UPDATE task_session
                 SET current_intent_revision_id = ?1
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
                "Task Intent Head changed despite an exclusive write transaction",
            ));
        }
        transaction
            .commit()
            .map_err(sql_error("commit Task Intent revision transaction"))?;
        Ok(revision)
    }

    /// Atomically merges normalized Task Signals into one Session.
    ///
    /// Signal content is trimmed before identity comparison. Existing signals
    /// and duplicates within the request are retained once, and concurrent
    /// merges cannot overwrite distinct signals.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an unknown Session or invalid
    /// signal and an I/O or invariant error when runtime persistence fails.
    pub fn merge_signals(
        &self,
        task_session_id: TaskSessionId,
        signals: Vec<TaskSignal>,
    ) -> Result<MergeSignalsOutcome> {
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error("begin merge Task Signals transaction"))?;
        if read_session_head(&transaction, task_session_id)?.is_none() {
            return Err(invalid(
                "task_session_id does not identify a runtime Session",
            ));
        }

        let mut inserted = 0;
        for signal in &signals {
            inserted += transaction
                .execute(
                    "INSERT OR IGNORE INTO task_signal (task_session_id, kind, content)
                     VALUES (?1, ?2, ?3)",
                    params![
                        task_session_id.to_string(),
                        signal_kind_name(signal.kind),
                        signal.content,
                    ],
                )
                .map_err(sql_error("merge Task Signal"))?;
        }
        let snapshot = read_snapshot_in_transaction(&transaction, task_session_id)?
            .ok_or_else(|| invariant("Task Session disappeared while merging signals"))?;
        transaction
            .commit()
            .map_err(sql_error("commit Task Signal transaction"))?;
        Ok(MergeSignalsOutcome { snapshot, inserted })
    }

    /// Reads one consistent Task Session snapshot.
    ///
    /// # Errors
    ///
    /// Returns an I/O or invariant error when runtime state cannot be read or
    /// violates the Task Session domain contract.
    pub fn read_snapshot(
        &self,
        task_session_id: TaskSessionId,
    ) -> Result<Option<TaskSessionSnapshot>> {
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin Task Session snapshot transaction"))?;
        let snapshot = read_snapshot_in_transaction(&transaction, task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Task Session snapshot transaction"))?;
        Ok(snapshot)
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
            "CREATE TABLE IF NOT EXISTS task_session (
                task_session_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL UNIQUE,
                agent_kind TEXT NOT NULL,
                external_session_id TEXT NOT NULL,
                current_intent_revision_id TEXT NOT NULL,
                UNIQUE (agent_kind, external_session_id),
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
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id)
                    ON DELETE CASCADE,
                FOREIGN KEY (task_session_id, parent_revision_id)
                    REFERENCES task_intent_revision (task_session_id, revision_id)
            ) STRICT;

            CREATE UNIQUE INDEX IF NOT EXISTS task_intent_one_initial
                ON task_intent_revision (task_session_id)
                WHERE parent_revision_id IS NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS task_intent_one_child_per_parent
                ON task_intent_revision (task_session_id, parent_revision_id)
                WHERE parent_revision_id IS NOT NULL;

            CREATE TABLE IF NOT EXISTS task_signal (
                task_session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                PRIMARY KEY (task_session_id, kind, content),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id)
                    ON DELETE CASCADE
            ) STRICT;

            PRAGMA user_version = 1;",
        )
        .map_err(sql_error("initialize task runtime schema"))
}

fn insert_initial_session(
    transaction: &Transaction<'_>,
    snapshot: &TaskSessionSnapshot,
) -> Result<()> {
    snapshot.validate()?;
    let revision = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("initial Task Session has no Intent revision"))?;
    let intent_json = serde_json::to_string(&revision.intent)
        .map_err(json_error("serialize initial Task Intent"))?;
    transaction
        .execute(
            "INSERT INTO task_session (
                task_session_id, task_id, agent_kind, external_session_id,
                current_intent_revision_id
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                snapshot.task_session_id.to_string(),
                snapshot.task_id.to_string(),
                snapshot.external_session_locator.agent_kind,
                snapshot.external_session_locator.external_session_id,
                revision.revision_id.to_string(),
            ],
        )
        .map_err(sql_error("insert Task Session"))?;
    transaction
        .execute(
            "INSERT INTO task_intent_revision (
                task_session_id, revision_id, task_id, parent_revision_id,
                revision_ordinal, intent_json
             ) VALUES (?1, ?2, ?3, NULL, 0, ?4)",
            params![
                snapshot.task_session_id.to_string(),
                revision.revision_id.to_string(),
                snapshot.task_id.to_string(),
                intent_json,
            ],
        )
        .map_err(sql_error("insert initial Task Intent revision"))?;
    for signal in &snapshot.task_signals {
        transaction
            .execute(
                "INSERT INTO task_signal (task_session_id, kind, content)
                 VALUES (?1, ?2, ?3)",
                params![
                    snapshot.task_session_id.to_string(),
                    signal_kind_name(signal.kind),
                    signal.content,
                ],
            )
            .map_err(sql_error("insert initial Task Signal"))?;
    }
    Ok(())
}

fn find_session_by_locator(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<TaskSessionId>> {
    let value = transaction
        .query_row(
            "SELECT task_session_id FROM task_session
             WHERE agent_kind = ?1 AND external_session_id = ?2",
            params![locator.agent_kind, locator.external_session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("locate Task Session"))?;
    value
        .map(|value| parse_id(&value, "task_session.task_session_id"))
        .transpose()
}

fn read_session_head(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Option<(TaskId, TaskIntentRevisionId)>> {
    let row = transaction
        .query_row(
            "SELECT task_id, current_intent_revision_id
             FROM task_session WHERE task_session_id = ?1",
            [task_session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("read Task Session Head"))?;
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
        .map_err(sql_error("inspect rejected Task Intent parent"))?;
    if let Some((owner_session, owner_task)) = owner {
        let owner_session: TaskSessionId =
            parse_id(&owner_session, "task_intent_revision.task_session_id")?;
        let owner_task: TaskId = parse_id(&owner_task, "task_intent_revision.task_id")?;
        if owner_session != task_session_id || owner_task != task_id {
            return Err(invalid(
                "task_intent_revision parent belongs to another Task Session",
            ));
        }
    }
    Err(invalid(
        "task_intent_revision parent must be the Session's current Head",
    ))
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
        .map_err(sql_error("read Task Intent revision ordinal"))
}

fn read_snapshot_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Option<TaskSessionSnapshot>> {
    let row = transaction
        .query_row(
            "SELECT task_id, agent_kind, external_session_id, current_intent_revision_id
             FROM task_session WHERE task_session_id = ?1",
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
    let Some((task_id, agent_kind, external_session_id, current_revision_id)) = row else {
        return Ok(None);
    };
    let task_id = parse_id(&task_id, "task_session.task_id")?;
    let current_revision_id = parse_id(
        &current_revision_id,
        "task_session.current_intent_revision_id",
    )?;
    let locator = ExternalSessionLocator {
        agent_kind,
        external_session_id,
    };

    let intent_revisions = read_intent_revisions(transaction, task_session_id)?;
    let task_signals = read_task_signals(transaction, task_session_id)?;

    let snapshot = TaskSessionSnapshot {
        task_session_id,
        task_id,
        external_session_locator: locator,
        intent_revisions,
        task_signals,
    };
    snapshot.validate().map_err(|error| {
        invariant(format!(
            "persisted Task Session violates its domain contract: {}",
            error.message()
        ))
    })?;
    if snapshot
        .current_intent_revision()
        .is_none_or(|revision| revision.revision_id != current_revision_id)
    {
        return Err(invariant(
            "persisted Task Session Head differs from its revision chain",
        ));
    }
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
             WHERE task_session_id = ?1
             ORDER BY revision_ordinal ASC",
        )
        .map_err(sql_error("prepare Task Intent revision read"))?;
    let revision_rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error("query Task Intent revisions"))?;
    let mut intent_revisions = Vec::new();
    for row in revision_rows {
        let (revision_id, parent_revision_id, stored_task_id, intent_json) =
            row.map_err(sql_error("read Task Intent revision row"))?;
        let stored_task_id: TaskId = parse_id(&stored_task_id, "task_intent_revision.task_id")?;
        let intent: TaskIntent =
            serde_json::from_str(&intent_json).map_err(json_error("parse Task Intent revision"))?;
        if stored_task_id != intent.task_id {
            return Err(invariant(
                "Task Intent JSON identity differs from its runtime row",
            ));
        }
        intent_revisions.push(TaskIntentRevision {
            revision_id: parse_id(&revision_id, "task_intent_revision.revision_id")?,
            parent_revision_id: parent_revision_id
                .map(|value| parse_id(&value, "task_intent_revision.parent_revision_id"))
                .transpose()?,
            intent,
        });
    }
    Ok(intent_revisions)
}

fn read_task_signals(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignal>> {
    let mut statement = transaction
        .prepare(
            "SELECT kind, content FROM task_signal
             WHERE task_session_id = ?1 ORDER BY kind ASC, content ASC",
        )
        .map_err(sql_error("prepare Task Signal read"))?;
    let signal_rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error("query Task Signals"))?;
    let mut task_signals = Vec::new();
    for row in signal_rows {
        let (kind, content) = row.map_err(sql_error("read Task Signal row"))?;
        task_signals.push(TaskSignal {
            kind: parse_signal_kind(&kind)?,
            content,
        });
    }
    Ok(task_signals)
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

const fn signal_kind_name(kind: TaskSignalKind) -> &'static str {
    match kind {
        TaskSignalKind::Prompt => "prompt",
        TaskSignalKind::Workspace => "workspace",
        TaskSignalKind::Repository => "repository",
        TaskSignalKind::File => "file",
        TaskSignalKind::Symbol => "symbol",
        TaskSignalKind::Diff => "diff",
        TaskSignalKind::Api => "api",
        TaskSignalKind::Schema => "schema",
        TaskSignalKind::Test => "test",
    }
}

fn parse_signal_kind(value: &str) -> Result<TaskSignalKind> {
    match value {
        "prompt" => Ok(TaskSignalKind::Prompt),
        "workspace" => Ok(TaskSignalKind::Workspace),
        "repository" => Ok(TaskSignalKind::Repository),
        "file" => Ok(TaskSignalKind::File),
        "symbol" => Ok(TaskSignalKind::Symbol),
        "diff" => Ok(TaskSignalKind::Diff),
        "api" => Ok(TaskSignalKind::Api),
        "schema" => Ok(TaskSignalKind::Schema),
        "test" => Ok(TaskSignalKind::Test),
        _ => Err(invariant(format!(
            "persisted Task Signal kind is unknown: {value}"
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
