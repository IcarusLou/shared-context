//! Deterministic, rebuildable `SQLite` projection of the current Git `HEAD` tree.
//!
//! The index never reads managed working-tree files and never walks Git history. It resolves
//! `HEAD^{tree}`, reads event/object blobs by object ID, delegates compatibility parsing to
//! `sctx-event-schema`, and delegates all domain ordering and quarantine decisions to the pure
//! `sctx-domain` reducer.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

mod git_tree;
mod project;
mod schema;

pub use sctx_domain::{Error, ErrorKind, Result};

/// Current physical `SQLite` schema version.
pub const DB_SCHEMA_VERSION: &str = "1";
/// Event parser implementation version recorded in every projection.
pub const EVENT_PARSER_VERSION: &str = "1";
/// Pure reducer implementation version recorded in every projection.
pub const REDUCER_VERSION: &str = "1";
/// Conflict detector implementation version recorded in every projection.
pub const CONFLICT_DETECTOR_VERSION: &str = "1";
/// Token normalization is intentionally deferred; `0` makes that boundary explicit.
pub const NORMALIZER_TOKENIZER_VERSION: &str = "0";
/// Search ranking is intentionally deferred; `0` makes that boundary explicit.
pub const SEARCH_RANKING_VERSION: &str = "0";

pub(crate) const IMPLEMENTATION_VERSIONS: [(&str, &str); 6] = [
    ("db_schema_version", DB_SCHEMA_VERSION),
    ("event_parser_version", EVENT_PARSER_VERSION),
    ("reducer_version", REDUCER_VERSION),
    ("conflict_detector_version", CONFLICT_DETECTOR_VERSION),
    ("normalizer_tokenizer_version", NORMALIZER_TOKENIZER_VERSION),
    ("search_ranking_version", SEARCH_RANKING_VERSION),
];

/// Why one synchronization did or did not rebuild the projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebuildReason {
    /// The healthy database already represented this Tree and implementation set.
    Current,
    /// No database file existed.
    MissingDatabase,
    /// The prior file could not pass `quick_check` and was isolated.
    CorruptDatabase,
    /// At least one implementation version was absent or different.
    ImplementationVersionChanged,
    /// Git `HEAD^{tree}` was different from the indexed Tree.
    TreeChanged,
    /// The caller explicitly requested a scratch rebuild.
    Forced,
}

/// Metadata that identifies one atomic projection generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexMetadata {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub db_schema_version: String,
    pub event_parser_version: String,
    pub reducer_version: String,
    pub conflict_detector_version: String,
    pub normalizer_tokenizer_version: String,
    pub search_ranking_version: String,
}

/// Result of a projection synchronization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebuildOutcome {
    pub rebuilt: bool,
    pub reason: RebuildReason,
    pub metadata: IndexMetadata,
    pub source_file_count: u64,
    pub diagnostic_count: u64,
    /// Isolated main database path. WAL/SHM companions use the same suffix when present.
    pub quarantined_database: Option<PathBuf>,
}

/// Result of checking a database file without repairing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickCheck {
    pub healthy: bool,
    pub detail: String,
}

/// Runtime settings required for every index connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabasePragmas {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: bool,
    pub busy_timeout_ms: i64,
}

/// Projection manager for one repository and its external state directory.
#[derive(Clone, Debug)]
pub struct ProjectionIndex {
    repository: PathBuf,
    state: PathBuf,
    database: PathBuf,
}

impl ProjectionIndex {
    /// Creates a manager. No database or Git operation occurs until synchronization.
    #[must_use]
    pub fn new(repository: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self {
        let state = state.into();
        Self {
            repository: repository.into(),
            database: state.join("index.sqlite"),
            state,
        }
    }

    /// Creates a manager for an initialized append-only store.
    #[must_use]
    pub fn for_store(store: &sctx_git_store::GitStore) -> Self {
        Self::new(store.repository(), store.state())
    }

    /// Database file managed by this index.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Synchronizes to the current `HEAD` Tree, rebuilding only when required.
    ///
    /// # Errors
    ///
    /// Returns an error for Git access, locking, `SQLite`, or projection materialization failures.
    pub fn synchronize(&self) -> Result<RebuildOutcome> {
        self.synchronize_inner(false)
    }

    /// Forces a deterministic scratch rebuild from the current `HEAD` Tree.
    ///
    /// # Errors
    ///
    /// Returns an error for Git access, locking, `SQLite`, or projection materialization failures.
    pub fn rebuild(&self) -> Result<RebuildOutcome> {
        self.synchronize_inner(true)
    }

    /// Reads the complete version/generation identity from an existing healthy database.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is absent, corrupt, or lacks complete metadata.
    pub fn metadata(&self) -> Result<IndexMetadata> {
        let connection = self.open_read_only()?;
        require_quick_check(&connection)?;
        read_metadata(&connection)?.ok_or_else(|| {
            invariant("projection database does not contain complete version metadata")
        })
    }

    /// Runs `SQLite` `quick_check` without mutating or replacing the database.
    ///
    /// # Errors
    ///
    /// Only filesystem-level inspection failures are returned as errors; `SQLite` corruption is
    /// represented by `healthy = false`.
    pub fn quick_check(&self) -> Result<QuickCheck> {
        if !self.database.exists() {
            return Ok(QuickCheck {
                healthy: false,
                detail: "database file does not exist".to_owned(),
            });
        }
        match Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(connection) => Ok(quick_check_connection(&connection)),
            Err(error) => Ok(QuickCheck {
                healthy: false,
                detail: error.to_string(),
            }),
        }
    }

    /// Reads the effective `SQLite` runtime settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or a PRAGMA cannot be read.
    pub fn pragmas(&self) -> Result<DatabasePragmas> {
        let connection = Connection::open(&self.database).map_err(sql_error("open index"))?;
        configure(&connection)?;
        read_pragmas(&connection)
    }

    fn synchronize_inner(&self, forced: bool) -> Result<RebuildOutcome> {
        fs::create_dir_all(&self.state).map_err(io_error("create index state directory"))?;
        let lock = open_lock(&self.state.join("index.lock"))?;
        lock.lock_exclusive().map_err(io_error("lock index.lock"))?;

        let mut isolated_database = None;
        let mut force_next = forced;
        let result = loop {
            let head = git_tree::read_head(&self.repository)?;
            let database_existed = self.database.exists();
            let (mut connection, isolated) = self.open_healthy_or_replace()?;
            if isolated_database.is_none() {
                isolated_database.clone_from(&isolated);
            }

            let current_metadata = read_metadata_if_present(&connection)?;
            let versions_match = schema::is_complete(&connection)?
                && current_metadata.as_ref().is_some_and(versions_are_current);
            let tree_matches = current_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.indexed_tree_oid == head.oid);
            if !force_next && isolated.is_none() && versions_match && tree_matches {
                break Ok(outcome_from_database(
                    &connection,
                    false,
                    RebuildReason::Current,
                    isolated_database,
                )?);
            }

            let reason = if isolated.is_some() {
                RebuildReason::CorruptDatabase
            } else if !database_existed {
                RebuildReason::MissingDatabase
            } else if force_next {
                RebuildReason::Forced
            } else if !versions_match {
                RebuildReason::ImplementationVersionChanged
            } else {
                RebuildReason::TreeChanged
            };
            let previous_generation = current_metadata
                .as_ref()
                .map_or(0, |metadata| metadata.projection_generation);
            let generation = previous_generation.checked_add(1).ok_or_else(|| {
                invariant("projection generation overflowed its u64 representation")
            })?;
            let input = project::build(&head.blobs);
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error("begin shadow rebuild transaction"))?;
            schema::replace_projection(&transaction, &input, &head.oid, generation)?;
            transaction
                .commit()
                .map_err(sql_error("commit shadow rebuild transaction"))?;
            require_quick_check(&connection)?;

            let observed_tree = git_tree::tree_oid(&self.repository)?;
            if observed_tree != head.oid {
                force_next = false;
                continue;
            }
            break Ok(outcome_from_database(
                &connection,
                true,
                reason,
                isolated_database,
            )?);
        };

        let unlock = FileExt::unlock(&lock).map_err(io_error("unlock index.lock"));
        match (result, unlock) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    fn open_healthy_or_replace(&self) -> Result<(Connection, Option<PathBuf>)> {
        if !self.database.exists() {
            let connection = Connection::open(&self.database)
                .map_err(sql_error("create projection database"))?;
            configure(&connection)?;
            return Ok((connection, None));
        }

        let connection = Connection::open(&self.database)
            .map_err(sql_error("open existing projection database"))?;
        if quick_check_connection(&connection).healthy {
            configure(&connection)?;
            return Ok((connection, None));
        }
        drop(connection);
        let quarantined = quarantine_database(&self.database)?;
        let connection = Connection::open(&self.database)
            .map_err(sql_error("create replacement projection database"))?;
        configure(&connection)?;
        Ok((connection, Some(quarantined)))
    }

    fn open_read_only(&self) -> Result<Connection> {
        if !self.database.exists() {
            return Err(invariant("projection database does not exist"));
        }
        Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_error("open projection database read-only"))
    }
}

impl sctx_git_store::CommitObserver for ProjectionIndex {
    fn committed(&self, repository: &Path, _commit_oid: &str) -> Result<()> {
        let expected = fs::canonicalize(&self.repository)
            .map_err(io_error("canonicalize index repository"))?;
        let actual =
            fs::canonicalize(repository).map_err(io_error("canonicalize committed repository"))?;
        if actual != expected {
            return Err(invariant(format!(
                "index observer was called for another repository: {}",
                actual.display()
            )));
        }
        self.synchronize().map(|_| ())
    }
}

fn configure(connection: &Connection) -> Result<()> {
    configure_sqlite(connection).map_err(sql_error("configure SQLite connection"))?;
    let pragmas = read_pragmas(connection)?;
    if pragmas.journal_mode != "wal"
        || pragmas.synchronous != 1
        || !pragmas.foreign_keys
        || pragmas.busy_timeout_ms != 3_000
    {
        return Err(invariant(format!(
            "SQLite PRAGMAs were not applied: {pragmas:?}"
        )));
    }
    Ok(())
}

fn configure_sqlite(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn read_pragmas(connection: &Connection) -> Result<DatabasePragmas> {
    Ok(DatabasePragmas {
        journal_mode: connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .map_err(sql_error("read journal_mode"))?
            .to_ascii_lowercase(),
        synchronous: connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .map_err(sql_error("read synchronous"))?,
        foreign_keys: connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
            .map_err(sql_error("read foreign_keys"))?
            != 0,
        busy_timeout_ms: connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .map_err(sql_error("read busy_timeout"))?,
    })
}

fn quick_check_connection(connection: &Connection) -> QuickCheck {
    let result = (|| -> rusqlite::Result<Vec<String>> {
        let mut statement = connection.prepare("PRAGMA quick_check")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect()
    })();
    match result {
        Ok(rows) if rows.as_slice() == ["ok"] => QuickCheck {
            healthy: true,
            detail: "ok".to_owned(),
        },
        Ok(rows) => QuickCheck {
            healthy: false,
            detail: rows.join("; "),
        },
        Err(error) => QuickCheck {
            healthy: false,
            detail: error.to_string(),
        },
    }
}

fn require_quick_check(connection: &Connection) -> Result<()> {
    let check = quick_check_connection(connection);
    if check.healthy {
        Ok(())
    } else {
        Err(invariant(format!(
            "SQLite quick_check failed: {}",
            check.detail
        )))
    }
}

fn read_metadata_if_present(connection: &Connection) -> Result<Option<IndexMetadata>> {
    let has_meta = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error("inspect projection schema"))?;
    if has_meta {
        let compatible_columns = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('meta') WHERE name IN ('key', 'value')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error("inspect projection metadata layout"))?;
        if compatible_columns == 2 {
            read_metadata(connection)
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

fn read_metadata(connection: &Connection) -> Result<Option<IndexMetadata>> {
    let Some(indexed_tree_oid) = schema::read_meta_value(connection, "indexed_tree_oid")? else {
        return Ok(None);
    };
    let Some(projection_generation) = schema::read_generation(connection)? else {
        return Ok(None);
    };
    let version = |key| schema::read_meta_value(connection, key);
    let Some(db_schema_version) = version("db_schema_version")? else {
        return Ok(None);
    };
    let Some(event_parser_version) = version("event_parser_version")? else {
        return Ok(None);
    };
    let Some(reducer_version) = version("reducer_version")? else {
        return Ok(None);
    };
    let Some(conflict_detector_version) = version("conflict_detector_version")? else {
        return Ok(None);
    };
    let Some(normalizer_tokenizer_version) = version("normalizer_tokenizer_version")? else {
        return Ok(None);
    };
    let Some(search_ranking_version) = version("search_ranking_version")? else {
        return Ok(None);
    };
    Ok(Some(IndexMetadata {
        indexed_tree_oid,
        projection_generation,
        db_schema_version,
        event_parser_version,
        reducer_version,
        conflict_detector_version,
        normalizer_tokenizer_version,
        search_ranking_version,
    }))
}

fn versions_are_current(metadata: &IndexMetadata) -> bool {
    metadata.db_schema_version == DB_SCHEMA_VERSION
        && metadata.event_parser_version == EVENT_PARSER_VERSION
        && metadata.reducer_version == REDUCER_VERSION
        && metadata.conflict_detector_version == CONFLICT_DETECTOR_VERSION
        && metadata.normalizer_tokenizer_version == NORMALIZER_TOKENIZER_VERSION
        && metadata.search_ranking_version == SEARCH_RANKING_VERSION
}

fn outcome_from_database(
    connection: &Connection,
    rebuilt: bool,
    reason: RebuildReason,
    quarantined_database: Option<PathBuf>,
) -> Result<RebuildOutcome> {
    let metadata = read_metadata(connection)?
        .ok_or_else(|| invariant("rebuilt projection is missing metadata"))?;
    let source_file_count = count(connection, "source_file")?;
    let diagnostic_count = count(connection, "diagnostic")?;
    Ok(RebuildOutcome {
        rebuilt,
        reason,
        metadata,
        source_file_count,
        diagnostic_count,
        quarantined_database,
    })
}

fn count(connection: &Connection, table: &str) -> Result<u64> {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(sql_error("count projection rows"))
}

fn quarantine_database(database: &Path) -> Result<PathBuf> {
    let suffix = format!("corrupt-{}", uuid::Uuid::new_v4().hyphenated());
    let mut target_name = database
        .file_name()
        .ok_or_else(|| invariant("database path has no file name"))?
        .to_os_string();
    target_name.push(".");
    target_name.push(suffix);
    let target = database.with_file_name(target_name);
    fs::rename(database, &target).map_err(io_error("isolate corrupt projection database"))?;
    for suffix in ["-wal", "-shm"] {
        let source = append_to_path(database, suffix)?;
        if source.exists() {
            let destination = append_to_path(&target, suffix)?;
            fs::rename(&source, &destination)
                .map_err(io_error("isolate corrupt SQLite sidecar"))?;
        }
    }
    Ok(target)
}

fn append_to_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| invariant("SQLite path has no file name"))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_error("open index lock"))
}

pub(crate) fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}
