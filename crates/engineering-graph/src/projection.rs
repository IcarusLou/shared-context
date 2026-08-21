use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sctx_domain::{Error, ErrorKind, Result};

use crate::EngineeringProjection;

const SCHEMA_VERSION: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Disposable local projection store for resolved Engineering Graph state.
#[derive(Clone, Debug)]
pub struct EngineeringProjectionStore {
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
}

/// One atomically-read Engineering projection and the Context Tree from which
/// its persistent References were projected.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineeringProjectionSnapshot {
    pub context_tree_oid: Option<String>,
    pub projection: EngineeringProjection,
}

impl EngineeringProjectionStore {
    /// Initializes `state/engineering.sqlite` below one installation root.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem, `SQLite`, or schema errors.
    pub fn initialize(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let state = root.join("state");
        fs::create_dir_all(&state).map_err(io_error("create Engineering projection state"))?;
        let store = Self {
            database: state.join("engineering.sqlite"),
            root,
            state,
        };
        let _connection = store.open_connection()?;
        Ok(store)
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

    /// Atomically replaces every derived row with one complete generation.
    ///
    /// # Errors
    ///
    /// Returns an invariant error for invalid/stale projection content and
    /// typed storage errors for transaction failures.
    pub fn rebuild(&self, projection: &EngineeringProjection) -> Result<()> {
        self.rebuild_for_context_tree(projection, None)
    }

    /// Atomically replaces every derived row and binds it to the exact Context
    /// Tree used to load its persistent Engineering References.
    ///
    /// A missing Tree is retained for compatibility with graph-only callers,
    /// but Task Retrieval deliberately treats such a projection as unpinned.
    ///
    /// # Errors
    ///
    /// Returns an invariant error for an empty Tree or invalid/stale projection
    /// content and typed storage errors for transaction failures.
    pub fn rebuild_for_context_tree(
        &self,
        projection: &EngineeringProjection,
        context_tree_oid: Option<&str>,
    ) -> Result<()> {
        projection.validate()?;
        if context_tree_oid.is_some_and(|tree| tree.trim().is_empty()) {
            return Err(invariant("Engineering projection Context Tree is empty"));
        }
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error("begin Engineering projection rebuild"))?;
        transaction
            .execute("DELETE FROM resolved_reference", [])
            .map_err(sql_error("clear Engineering Reference projection"))?;
        transaction
            .execute("DELETE FROM projection_meta", [])
            .map_err(sql_error("clear Engineering projection metadata"))?;
        transaction
            .execute(
                "INSERT INTO projection_meta (
                    singleton, policy_version, artifact_generation, context_tree_oid
                 ) VALUES (1, ?1, ?2, ?3)",
                params![
                    projection.policy_version,
                    projection.artifact_generation,
                    context_tree_oid
                ],
            )
            .map_err(sql_error("write Engineering projection metadata"))?;
        for reference in &projection.references {
            let payload = serde_json::to_string(reference)
                .map_err(json_error("serialize resolved Engineering Reference"))?;
            transaction
                .execute(
                    "INSERT INTO resolved_reference (
                        reference_id, artifact_generation, payload_json
                     ) VALUES (?1, ?2, ?3)",
                    params![
                        reference.reference_id.to_string(),
                        projection.artifact_generation,
                        payload,
                    ],
                )
                .map_err(sql_error("write resolved Engineering Reference"))?;
        }
        transaction
            .commit()
            .map_err(sql_error("commit Engineering projection rebuild"))
    }

    /// Scratch-equivalent incremental replacement.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::rebuild`].
    pub fn rebuild_incremental(&self, projection: &EngineeringProjection) -> Result<()> {
        self.rebuild(projection)
    }

    /// Reads one generation-consistent derived projection.
    ///
    /// # Errors
    ///
    /// Returns an invariant error if any row carries an old generation.
    pub fn read_projection(&self) -> Result<Option<EngineeringProjection>> {
        Ok(self.read_snapshot()?.map(|snapshot| snapshot.projection))
    }

    /// Reads one generation-consistent projection and its optional Context Tree
    /// binding from a single `SQLite` transaction.
    ///
    /// # Errors
    ///
    /// Returns an invariant error if any row carries an old generation.
    pub fn read_snapshot(&self) -> Result<Option<EngineeringProjectionSnapshot>> {
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin Engineering projection read"))?;
        let meta = transaction
            .query_row(
                "SELECT policy_version, artifact_generation, context_tree_oid
                 FROM projection_meta WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error("read Engineering projection metadata"))?;
        let Some((policy_version, artifact_generation, context_tree_oid)) = meta else {
            transaction
                .commit()
                .map_err(sql_error("commit empty Engineering projection read"))?;
            return Ok(None);
        };
        let mut statement = transaction
            .prepare(
                "SELECT artifact_generation, payload_json
                 FROM resolved_reference ORDER BY reference_id ASC",
            )
            .map_err(sql_error("prepare resolved Engineering Reference read"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_error("query resolved Engineering References"))?;
        let mut references = Vec::new();
        for row in rows {
            let (row_generation, payload) =
                row.map_err(sql_error("read resolved Engineering Reference row"))?;
            if row_generation != artifact_generation {
                return Err(invariant(
                    "resolved Engineering Reference leaked an old generation",
                ));
            }
            references.push(
                serde_json::from_str(&payload)
                    .map_err(json_error("parse resolved Engineering Reference"))?,
            );
        }
        drop(statement);
        let projection = EngineeringProjection {
            policy_version,
            artifact_generation,
            references,
        };
        projection.validate()?;
        transaction
            .commit()
            .map_err(sql_error("commit Engineering projection read"))?;
        Ok(Some(EngineeringProjectionSnapshot {
            context_tree_oid,
            projection,
        }))
    }

    /// Returns deterministic bytes for rebuild equivalence checks and diagnostics.
    ///
    /// # Errors
    ///
    /// Returns typed projection read or serialization errors.
    pub fn canonical_bytes(&self) -> Result<Option<Vec<u8>>> {
        self.read_snapshot()?
            .map(|snapshot| {
                serde_json::to_vec(&(snapshot.context_tree_oid, snapshot.projection))
                    .map_err(json_error("serialize canonical Engineering projection"))
            })
            .transpose()
    }

    fn open_connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.database)
            .map_err(sql_error("open Engineering projection database"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(sql_error("configure Engineering projection busy timeout"))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error("configure Engineering projection journal mode"))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_error(
                "configure Engineering projection synchronous mode",
            ))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_error("enable Engineering projection foreign keys"))?;
        ensure_schema(&connection)?;
        Ok(connection)
    }
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(sql_error("read Engineering projection schema version"))?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(invariant(format!(
            "unsupported Engineering projection schema version {version}"
        )));
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS projection_meta (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                policy_version TEXT NOT NULL,
                artifact_generation TEXT NOT NULL,
                context_tree_oid TEXT
            ) STRICT;
            CREATE TABLE IF NOT EXISTS resolved_reference (
                reference_id TEXT PRIMARY KEY,
                artifact_generation TEXT NOT NULL,
                payload_json TEXT NOT NULL CHECK (json_valid(payload_json))
            ) STRICT;
            PRAGMA user_version = 2;",
        )
        .map_err(sql_error("initialize Engineering projection schema"))
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
