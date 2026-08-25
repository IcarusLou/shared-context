//! Private local Repository Registry and rebuildable Engineering Graph state.
//!
//! Stable Repository identity comes only from the explicit local Repository
//! Catalog. The `SQLite` Registry is a disposable projection of that Catalog;
//! paths, basenames, remotes, and Git topology never create or merge identity.

mod projection;
mod resolver;
mod scanner;

pub use projection::{EngineeringProjectionSnapshot, EngineeringProjectionStore};
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
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sctx_domain::{Error, ErrorKind, RepositoryId, RepositoryIdentity, Result};
use serde::Serialize;

const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CATALOG_REPOSITORIES: usize = 256;
const MAX_CHECKOUTS_PER_REPOSITORY: usize = 32;

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

        let mut connection = self.open_connection()?;
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
        transaction
            .commit()
            .map_err(sql_error("commit Repository Catalog sync"))?;
        Ok(RepositoryCatalogSyncReport {
            repository_count: observations.len(),
            locator_count,
            unavailable_locator_count,
        })
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
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error("configure Repository Registry journal mode"))?;
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

fn immediate<'a>(connection: &'a mut Connection, context: &'static str) -> Result<Transaction<'a>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error(context))
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(sql_error("read Repository Registry schema version"))?;
    if version != 0 && version != SCHEMA_VERSION {
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
            PRAGMA user_version = 3;",
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
