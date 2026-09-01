//! Private local Repository Registry and rebuildable Engineering Graph state.
//!
//! Stable Repository identity comes only from the explicit local Repository
//! Catalog. The `SQLite` Registry is a disposable projection of that Catalog;
//! paths, basenames, remotes, and Git topology never create or merge identity.

mod artifact_focus;
mod projection;
mod relocation;
mod resolver;
mod scanner;

pub use artifact_focus::{
    ARTIFACT_FOCUS_QUERY_BUDGET, ArtifactFocusHit, ArtifactFocusLookup, ArtifactFocusOutcome,
    ArtifactFocusReader, MAX_ARTIFACT_FOCUS_HITS,
};
pub use projection::{EngineeringProjectionSnapshot, EngineeringProjectionStore};
pub use relocation::{RelocationCandidate, find_relocation_candidate, relocatable_path};
pub use resolver::{
    CandidateMatchEvidence, EngineeringProjection, EngineeringReferenceResolver,
    GraphContextRelation, GraphContextSafety, GraphContextSafetyBlocker, GraphContextSnapshot,
    GraphContextStatus, MatchBasis, ProjectedEngineeringReference, ResolvedReferenceProjection,
    build_graph_context_snapshots,
};
pub use scanner::{
    ArtifactObservation, ArtifactSourceState, MAX_REPOSITORY_SCAN_PLAN_PATHS,
    RepositoryScanOutcome, RepositoryScanPlan, RepositoryScanner, RepositoryScannerLimits,
    RepositorySnapshot, SkippedFile, SkippedFileReason, SnapshotArtifact, SnapshotSourcePolicy,
    SourceLanguage,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sctx_domain::{Error, ErrorKind, RepositoryId, RepositoryIdentity, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i64 = 4;
/// Registry schema versions that reach [`SCHEMA_VERSION`] by additive
/// `CREATE TABLE IF NOT EXISTS` alone. The projection rows are unchanged, so an
/// older disposable Registry is upgraded in place instead of rejected.
const ADDITIVE_UPGRADE_VERSIONS: [i64; 1] = [3];
/// Version tag of the Catalog fingerprint recipe. Bumping it invalidates every
/// persisted fingerprint, which only forces one extra full synchronization.
const CATALOG_FINGERPRINT_VERSION: &str = "catalog-fingerprint-v1";
/// `projection_meta` key holding the fingerprint of the Catalog this Registry
/// was last projected from, together with the report that projection returned.
const CATALOG_FINGERPRINT_KEY: &str = "catalog_fingerprint";
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CATALOG_REPOSITORIES: usize = 256;
const MAX_CHECKOUTS_PER_REPOSITORY: usize = 32;
/// Bounded retry policy for `PRAGMA journal_mode=WAL`, which requires an
/// exclusive lock and is not protected by `busy_timeout`. Only
/// `SQLITE_BUSY`/`SQLITE_LOCKED` is retried; every other error is returned
/// immediately.
const WAL_RETRY_MAX_ATTEMPTS: u32 = 20;
const WAL_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const WAL_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);
const WAL_RETRY_BUDGET: Duration = Duration::from_secs(3);

/// Current local accessibility of one configured checkout or Repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryAvailability {
    Available,
    Unavailable,
}

/// Trusted Catalog input for rebuilding the disposable Registry projection.
///
/// This type is intentionally not part of any MCP request schema. Its
/// `RepositoryId` must already have been parsed from the local Catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRepositorySpec {
    pub repository_id: RepositoryId,
    pub checkout_paths: Vec<PathBuf>,
}

/// One private locator projected from the explicit Catalog.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LocalRepositoryLocator {
    pub checkout_path: PathBuf,
    pub availability: RepositoryAvailability,
}

/// Complete Registry projection for one stable Catalog identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RegisteredRepository {
    pub identity: RepositoryIdentity,
    pub availability: RepositoryAvailability,
    pub locators: Vec<LocalRepositoryLocator>,
}

/// Summary of one atomic Catalog-to-Registry synchronization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogSyncReport {
    pub repository_count: usize,
    pub locator_count: usize,
    pub unavailable_locator_count: usize,
}

/// Owner of the disposable private Repository Registry database.
#[derive(Clone, Debug)]
pub struct RepositoryRegistry {
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
}

impl RepositoryRegistry {
    /// Initializes the Registry below `home/.shared-context`.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem or `SQLite` errors.
    pub fn initialize_for_home(home: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(home.as_ref().join(".shared-context"))
    }

    /// Initializes the Registry at one Shared Context installation root.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem or `SQLite` errors.
    pub fn initialize(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let state = root.join("state");
        fs::create_dir_all(&state).map_err(io_error("create Repository Registry state"))?;
        let registry = Self {
            database: state.join("repository-registry.sqlite"),
            root,
            state,
        };
        let _connection = registry.open_connection()?;
        Ok(registry)
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

    /// Atomically replaces the Registry projection with one trusted Catalog snapshot.
    ///
    /// Paths are verified as exact Git worktree roots when present. Missing
    /// configured paths remain projected as unavailable locators. No Git fact
    /// participates in Repository identity or merging.
    ///
    /// # Errors
    ///
    /// Rejects duplicate IDs/paths, unsafe or symlinked paths, non-root Git
    /// directories, oversized Catalogs, and Registry persistence failures.
    pub fn sync_catalog(
        &self,
        repositories: &[CatalogRepositorySpec],
    ) -> Result<RepositoryCatalogSyncReport> {
        let repositories = normalize_catalog(repositories)?;
        // Request-serving code synchronizes on every call, and an unchanged Catalog against
        // unchanged checkout states can only reproduce the projection already stored. The
        // fingerprint turns that case into one read: no `git` process per checkout, and no
        // write transaction competing with concurrent readers of the same Registry.
        let fingerprint = catalog_fingerprint(&repositories)?;
        let mut connection = self.open_connection()?;
        if let Some(report) = read_catalog_projection_report(&connection, &fingerprint)? {
            return Ok(report);
        }
        let observations = repositories
            .iter()
            .map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .map(|path| inspect_catalog_checkout(path))
                    .collect::<Result<Vec<_>>>()
                    .map(|locators| (repository.repository_id.clone(), locators))
            })
            .collect::<Result<Vec<_>>>()?;

        let transaction = immediate(&mut connection, "begin Repository Catalog sync")?;
        transaction
            .execute("DELETE FROM repository_locator", [])
            .map_err(sql_error("clear Repository locators"))?;
        transaction
            .execute("DELETE FROM repository_identity", [])
            .map_err(sql_error("clear Repository identities"))?;
        let mut locator_count = 0_usize;
        let mut unavailable_locator_count = 0_usize;
        for (repository_id, locators) in &observations {
            let available = locators
                .iter()
                .any(|locator| locator.availability == RepositoryAvailability::Available);
            let identity = RepositoryIdentity {
                repository_id: repository_id.clone(),
                canonical_name: repository_id.to_string(),
            };
            identity.validate()?;
            transaction
                .execute(
                    "INSERT INTO repository_identity (
                        repository_id, canonical_name, available
                     ) VALUES (?1, ?2, ?3)",
                    params![
                        identity.repository_id.to_string(),
                        identity.canonical_name,
                        available
                    ],
                )
                .map_err(sql_error("project Catalog Repository identity"))?;
            for locator in locators {
                locator_count = locator_count.saturating_add(1);
                if locator.availability == RepositoryAvailability::Unavailable {
                    unavailable_locator_count = unavailable_locator_count.saturating_add(1);
                }
                transaction
                    .execute(
                        "INSERT INTO repository_locator (
                            repository_id, checkout_path, available
                         ) VALUES (?1, ?2, ?3)",
                        params![
                            repository_id.to_string(),
                            path_text(&locator.checkout_path)?,
                            locator.availability == RepositoryAvailability::Available,
                        ],
                    )
                    .map_err(sql_error("project Catalog Repository locator"))?;
            }
        }
        let report = RepositoryCatalogSyncReport {
            repository_count: observations.len(),
            locator_count,
            unavailable_locator_count,
        };
        write_catalog_projection_report(&transaction, &fingerprint, &report)?;
        transaction
            .commit()
            .map_err(sql_error("commit Repository Catalog sync"))?;
        Ok(report)
    }

    /// Resolves one stable Repository identity.
    ///
    /// # Errors
    ///
    /// Returns typed Registry read or invariant errors.
    pub fn resolve_by_id(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<Option<RegisteredRepository>> {
        read_repository(&self.open_connection()?, repository_id)
    }

    /// Resolves one exact configured checkout path.
    ///
    /// # Errors
    ///
    /// Returns typed path, Registry read, or invariant errors.
    pub fn resolve_by_checkout_path(
        &self,
        checkout_path: &Path,
    ) -> Result<Option<RegisteredRepository>> {
        validate_absolute_path(checkout_path)?;
        let key = if checkout_path.exists() {
            let metadata = fs::symlink_metadata(checkout_path)
                .map_err(io_error("inspect Repository checkout locator"))?;
            if metadata.file_type().is_symlink() {
                return Err(invalid("Repository checkout locator must not be a symlink"));
            }
            fs::canonicalize(checkout_path)
                .map_err(io_error("canonicalize Repository checkout locator"))?
        } else {
            checkout_path.to_path_buf()
        };
        let connection = self.open_connection()?;
        let repository_id = connection
            .query_row(
                "SELECT repository_id FROM repository_locator WHERE checkout_path = ?1",
                [path_text(&key)?],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error("resolve configured Repository checkout"))?
            .map(|value| parse_repository_id(&value))
            .transpose()?;
        repository_id
            .map(|repository_id| read_repository(&connection, &repository_id))
            .transpose()
            .map(Option::flatten)
    }

    /// Lists every Catalog-projected Repository identity in stable ID order.
    ///
    /// # Errors
    ///
    /// Returns typed Registry read or invariant errors.
    pub fn list(&self) -> Result<Vec<RegisteredRepository>> {
        let connection = self.open_connection()?;
        let mut statement = connection
            .prepare("SELECT repository_id FROM repository_identity ORDER BY repository_id ASC")
            .map_err(sql_error("prepare Repository list"))?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error("query Repository list"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error("read Repository list row"))?;
        drop(statement);
        ids.into_iter()
            .map(|value| {
                let repository_id = parse_repository_id(&value)?;
                read_repository(&connection, &repository_id)?
                    .ok_or_else(|| invariant("listed Repository disappeared during read"))
            })
            .collect()
    }

    fn open_connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.database)
            .map_err(sql_error("open Repository Registry database"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(sql_error("configure Repository Registry busy timeout"))?;
        enable_wal_journal_mode(&connection)?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_error("configure Repository Registry synchronous mode"))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_error("enable Repository Registry foreign keys"))?;
        ensure_schema(&connection)?;
        Ok(connection)
    }
}

fn normalize_catalog(repositories: &[CatalogRepositorySpec]) -> Result<Vec<CatalogRepositorySpec>> {
    if repositories.len() > MAX_CATALOG_REPOSITORIES {
        return Err(invalid(format!(
            "Repository Catalog exceeds {MAX_CATALOG_REPOSITORIES} identities"
        )));
    }
    let mut by_id = BTreeMap::<RepositoryId, BTreeSet<PathBuf>>::new();
    let mut path_owners = BTreeMap::<PathBuf, RepositoryId>::new();
    for repository in repositories {
        if by_id.contains_key(&repository.repository_id) {
            return Err(invalid(format!(
                "duplicate Catalog RepositoryId: {}",
                repository.repository_id
            )));
        }
        if repository.checkout_paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
            return Err(invalid(format!(
                "Repository {} exceeds {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths",
                repository.repository_id
            )));
        }
        let mut paths = BTreeSet::new();
        for path in &repository.checkout_paths {
            validate_absolute_path(path)?;
            if !paths.insert(path.clone()) {
                return Err(invalid(format!(
                    "duplicate Catalog checkout path: {}",
                    path.display()
                )));
            }
            if let Some(owner) = path_owners.insert(path.clone(), repository.repository_id.clone())
            {
                return Err(invalid(format!(
                    "Catalog checkout path belongs to both {owner} and {}",
                    repository.repository_id
                )));
            }
        }
        by_id.insert(repository.repository_id.clone(), paths);
    }
    Ok(by_id
        .into_iter()
        .map(|(repository_id, checkout_paths)| CatalogRepositorySpec {
            repository_id,
            checkout_paths: checkout_paths.into_iter().collect(),
        })
        .collect())
}

/// Deterministic fingerprint of one already-normalized Catalog together with the
/// local state of every configured checkout.
///
/// It covers exactly the inputs [`RepositoryRegistry::sync_catalog`] projects:
/// the stable identities, their configured paths, and the cheap filesystem facts
/// that decide whether [`inspect_catalog_checkout`] resolves a path to an
/// available locator, an unavailable one, or a typed rejection. Every transition
/// between those outcomes changes a signature, so an unchanged fingerprint means
/// the stored projection is exactly what a full synchronization would rewrite.
fn catalog_fingerprint(repositories: &[CatalogRepositorySpec]) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_component(&mut hasher, CATALOG_FINGERPRINT_VERSION);
    for repository in repositories {
        hash_component(&mut hasher, &repository.repository_id.to_string());
        for path in &repository.checkout_paths {
            hash_component(&mut hasher, &path_text(path)?);
            hash_component(&mut hasher, &checkout_state_signature(path)?);
        }
    }
    Ok(format!("cat_{:x}", hasher.finalize()))
}

/// Cheap local signature of one configured checkout: only `lstat` calls, never a
/// `git` process. Each distinct signature maps to one [`inspect_catalog_checkout`]
/// outcome, so a signature change is the trigger for the full inspection.
fn checkout_state_signature(path: &Path) -> Result<String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("missing".to_owned());
        }
        Err(error) => return Err(io_error("inspect configured Repository checkout")(error)),
    };
    if metadata.file_type().is_symlink() {
        return Ok("symlink".to_owned());
    }
    if !metadata.is_dir() {
        return Ok("not-a-directory".to_owned());
    }
    if fs::symlink_metadata(path.join(".git")).is_ok() {
        Ok("worktree-root".to_owned())
    } else {
        Ok("directory".to_owned())
    }
}

/// Reads the report of the projection stored for `fingerprint`, or `None` when
/// this Registry was last projected from a different Catalog state.
fn read_catalog_projection_report(
    connection: &Connection,
    fingerprint: &str,
) -> Result<Option<RepositoryCatalogSyncReport>> {
    connection
        .query_row(
            "SELECT repository_count, locator_count, unavailable_locator_count
             FROM projection_meta WHERE key = ?1 AND fingerprint = ?2",
            params![CATALOG_FINGERPRINT_KEY, fingerprint],
            |row| {
                Ok(RepositoryCatalogSyncReport {
                    repository_count: projected_count(row.get::<_, i64>(0)?),
                    locator_count: projected_count(row.get::<_, i64>(1)?),
                    unavailable_locator_count: projected_count(row.get::<_, i64>(2)?),
                })
            },
        )
        .optional()
        .map_err(sql_error("read Repository Catalog projection fingerprint"))
}

/// Reads back one persisted projection count. The Registry only ever stores counts
/// it produced itself, so an out-of-range value is disposable state, not an error.
fn projected_count(value: i64) -> usize {
    usize::try_from(value).unwrap_or(0)
}

/// Records the fingerprint and report of the projection this transaction wrote.
fn write_catalog_projection_report(
    transaction: &Transaction<'_>,
    fingerprint: &str,
    report: &RepositoryCatalogSyncReport,
) -> Result<()> {
    transaction
        .execute(
            "INSERT OR REPLACE INTO projection_meta (
                key, fingerprint, repository_count, locator_count, unavailable_locator_count
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                CATALOG_FINGERPRINT_KEY,
                fingerprint,
                i64::try_from(report.repository_count).map_err(|_| invariant(
                    "Repository count exceeds the Registry integer range"
                ))?,
                i64::try_from(report.locator_count)
                    .map_err(|_| invariant("locator count exceeds the Registry integer range"))?,
                i64::try_from(report.unavailable_locator_count).map_err(|_| invariant(
                    "unavailable locator count exceeds the Registry integer range"
                ))?,
            ],
        )
        .map_err(sql_error(
            "record Repository Catalog projection fingerprint",
        ))?;
    Ok(())
}

fn hash_component(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn inspect_catalog_checkout(path: &Path) -> Result<LocalRepositoryLocator> {
    validate_absolute_path(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalRepositoryLocator {
                checkout_path: path.to_path_buf(),
                availability: RepositoryAvailability::Unavailable,
            });
        }
        Err(error) => return Err(io_error("inspect configured Repository checkout")(error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(invalid(
            "configured Repository checkout must not be a symlink",
        ));
    }
    if !metadata.is_dir() {
        return Err(invalid(
            "configured Repository checkout must be a directory",
        ));
    }
    let canonical =
        fs::canonicalize(path).map_err(io_error("canonicalize configured Repository checkout"))?;
    if canonical != path {
        return Err(invalid(
            "configured Repository checkout path must already be canonical",
        ));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(io_error("inspect configured Git Repository"))?;
    if !output.status.success() {
        return Err(invalid("configured checkout is not a Git Repository"));
    }
    let top_level = String::from_utf8(output.stdout)
        .map_err(|error| invalid(format!("Git top-level is not UTF-8: {error}")))?;
    let top_level = fs::canonicalize(top_level.trim())
        .map_err(io_error("canonicalize configured Git top-level"))?;
    if top_level != canonical {
        return Err(invalid(
            "configured Repository checkout must identify the Git worktree root",
        ));
    }
    Ok(LocalRepositoryLocator {
        checkout_path: canonical,
        availability: RepositoryAvailability::Available,
    })
}

fn validate_absolute_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(invalid(
            "Repository checkout path must be absolute without dot segments",
        ));
    }
    if path.to_str().is_none() {
        return Err(invalid("Repository checkout path must be valid UTF-8"));
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("Repository checkout path must be valid UTF-8"))
}

/// Enables `WAL` journal mode with a bounded, backed-off retry against
/// `SQLITE_BUSY`/`SQLITE_LOCKED`.
///
/// `PRAGMA journal_mode=WAL` needs a brief exclusive lock to rewrite the
/// database header the first time a database switches into `WAL` mode, and
/// that exclusive-lock acquisition is not covered by `busy_timeout`. When
/// several connections race to `initialize` the same new database, one may
/// observe the file as locked. Retries here are deterministic (fixed
/// attempt count and backoff schedule, bounded by both an attempt ceiling
/// and a wall-clock budget) and only ever apply to busy/locked failures;
/// every other error is returned immediately.
fn enable_wal_journal_mode(connection: &Connection) -> Result<()> {
    let current: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(sql_error("read Repository Registry journal mode"))?;
    if current.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = Instant::now() + WAL_RETRY_BUDGET;
    let mut backoff = WAL_RETRY_INITIAL_BACKOFF;
    let mut last_error = None;
    for attempt in 0..WAL_RETRY_MAX_ATTEMPTS {
        match connection.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = is_database_busy(&error);
                let attempts_remain = attempt + 1 < WAL_RETRY_MAX_ATTEMPTS;
                let now = Instant::now();
                last_error = Some(error);
                if !retryable || !attempts_remain || now >= deadline {
                    break;
                }
                thread::sleep(backoff.min(deadline.saturating_duration_since(now)));
                backoff = (backoff * 2).min(WAL_RETRY_MAX_BACKOFF);
            }
        }
    }
    Err(sql_error("configure Repository Registry journal mode")(
        last_error.expect("the retry loop always records the failing pragma_update error"),
    ))
}

/// Returns `true` for `SQLITE_BUSY`/`SQLITE_LOCKED` failures, the only
/// `SQLite` errors safe to retry for `enable_wal_journal_mode`.
fn is_database_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn immediate<'a>(connection: &'a mut Connection, context: &'static str) -> Result<Transaction<'a>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error(context))
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(sql_error("read Repository Registry schema version"))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version != 0 && !ADDITIVE_UPGRADE_VERSIONS.contains(&version) {
        return Err(invariant(format!(
            "unsupported Repository Registry schema version {version}"
        )));
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS repository_identity (
                repository_id TEXT PRIMARY KEY,
                canonical_name TEXT NOT NULL,
                available INTEGER NOT NULL CHECK (available IN (0, 1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS repository_locator (
                locator_id INTEGER PRIMARY KEY,
                repository_id TEXT NOT NULL,
                checkout_path TEXT NOT NULL UNIQUE,
                available INTEGER NOT NULL CHECK (available IN (0, 1)),
                FOREIGN KEY (repository_id) REFERENCES repository_identity (repository_id)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS repository_locator_repository
                ON repository_locator (repository_id);
            CREATE TABLE IF NOT EXISTS projection_meta (
                key TEXT PRIMARY KEY,
                fingerprint TEXT NOT NULL,
                repository_count INTEGER NOT NULL,
                locator_count INTEGER NOT NULL,
                unavailable_locator_count INTEGER NOT NULL
            ) STRICT;
            PRAGMA user_version = 4;",
        )
        .map_err(sql_error("initialize Repository Registry schema"))
}

fn read_repository(
    connection: &Connection,
    repository_id: &RepositoryId,
) -> Result<Option<RegisteredRepository>> {
    let row = connection
        .query_row(
            "SELECT canonical_name, available
             FROM repository_identity WHERE repository_id = ?1",
            [repository_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()
        .map_err(sql_error("read Repository identity"))?;
    let Some((canonical_name, available)) = row else {
        return Ok(None);
    };
    let identity = RepositoryIdentity {
        repository_id: repository_id.clone(),
        canonical_name,
    };
    identity.validate()?;
    let mut statement = connection
        .prepare(
            "SELECT checkout_path, available
             FROM repository_locator WHERE repository_id = ?1
             ORDER BY checkout_path ASC",
        )
        .map_err(sql_error("prepare Repository locators"))?;
    let rows = statement
        .query_map([repository_id.to_string()], |row| {
            Ok(LocalRepositoryLocator {
                checkout_path: PathBuf::from(row.get::<_, String>(0)?),
                availability: if row.get::<_, bool>(1)? {
                    RepositoryAvailability::Available
                } else {
                    RepositoryAvailability::Unavailable
                },
            })
        })
        .map_err(sql_error("query Repository locators"))?;
    let locators = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("read Repository locator row"))?;
    Ok(Some(RegisteredRepository {
        identity,
        availability: if available {
            RepositoryAvailability::Available
        } else {
            RepositoryAvailability::Unavailable
        },
        locators,
    }))
}

fn parse_repository_id(value: &str) -> Result<RepositoryId> {
    value
        .parse()
        .map_err(|error| invariant(format!("persisted RepositoryId is invalid: {error}")))
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
