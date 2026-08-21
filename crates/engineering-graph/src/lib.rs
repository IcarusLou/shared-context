//! Private local Repository Registry for rebuildable Engineering Graph state.
//!
//! The Registry owns only `state/repository-registry.sqlite`. Checkout paths,
//! Git common-dir observations, declared names, and remotes are matching hints;
//! the generated [`RepositoryId`] remains the sole Repository identity.

mod projection;
mod resolver;
mod scanner;

pub use projection::{EngineeringProjectionSnapshot, EngineeringProjectionStore};
pub use resolver::{
    CandidateMatchEvidence, EngineeringProjection, EngineeringReferenceResolver, MatchBasis,
    ProjectedEngineeringReference, ResolvedReferenceProjection,
};

pub use scanner::{
    ArtifactObservation, ArtifactSourceState, RepositoryScanOutcome, RepositoryScanner,
    RepositoryScannerLimits, RepositorySnapshot, SkippedFile, SkippedFileReason, SnapshotArtifact,
    SnapshotSourcePolicy, SourceLanguage,
};

use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sctx_domain::{
    Error, ErrorKind, RepositoryId, RepositoryIdentity, Result, SemanticFingerprint,
};

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Current local accessibility of a registered Repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryAvailability {
    Available,
    Unavailable,
}

/// Caller input for observing one local Git checkout or worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterRepositoryRequest {
    pub checkout_path: PathBuf,
    pub declared_identity: Option<String>,
    pub remote_hint: Option<String>,
}

/// One private local locator record. None of these fields are Repository identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalRepositoryLocator {
    pub checkout_path: PathBuf,
    pub git_common_dir: PathBuf,
    pub git_common_dir_identity: String,
    pub declared_identity: Option<String>,
    pub remote_hint: Option<String>,
    pub availability: RepositoryAvailability,
}

/// Complete local Registry view for one stable Repository identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredRepository {
    pub identity: RepositoryIdentity,
    pub availability: RepositoryAvailability,
    pub locators: Vec<LocalRepositoryLocator>,
}

/// Outcome of registering one checkout observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterRepositoryOutcome {
    pub repository: RegisteredRepository,
    pub created_identity: bool,
    pub created_locator: bool,
}

/// Supported local hint lookups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryLocatorQuery {
    CheckoutPath(PathBuf),
    DeclaredIdentity(String),
    RemoteHint(String),
}

/// Owner of the private Repository Registry database.
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

    /// Registers or refreshes one verified local Git checkout.
    ///
    /// Common-dir identity and explicit declared identity may converge records;
    /// path basename and remote hints never merge identities.
    ///
    /// # Errors
    ///
    /// Returns typed errors for unsafe paths, non-Git directories, conflicting
    /// hints, or Registry persistence failures.
    pub fn register(
        &self,
        request: &RegisterRepositoryRequest,
    ) -> Result<RegisterRepositoryOutcome> {
        let observation = inspect_checkout(request)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Repository registration")?;
        let by_common =
            repository_by_common_identity(&transaction, &observation.git_common_dir_identity)?;
        let by_declared = observation
            .declared_identity
            .as_deref()
            .map(|hint| repository_by_declared(&transaction, hint))
            .transpose()?
            .flatten();
        if by_common.is_some() && by_declared.is_some() && by_common != by_declared {
            return Err(invalid(
                "verified Git common-dir and declared identity resolve to different Repositories",
            ));
        }
        let existing = by_common.or(by_declared);
        let (repository_id, created_identity) = if let Some(repository_id) = existing {
            (repository_id, false)
        } else {
            let repository_id = RepositoryId::new();
            insert_repository(&transaction, repository_id, &observation)?;
            (repository_id, true)
        };
        if let Some(declared) = &observation.declared_identity {
            attach_declared_identity(&transaction, repository_id, declared)?;
        }
        let created_locator = upsert_locator(&transaction, repository_id, &observation)?;
        mark_missing_common_locators(
            &transaction,
            repository_id,
            &observation.git_common_dir_identity,
        )?;
        refresh_repository_availability(&transaction, repository_id)?;
        let repository = read_repository(&transaction, repository_id)?
            .ok_or_else(|| invariant("registered Repository disappeared inside transaction"))?;
        transaction
            .commit()
            .map_err(sql_error("commit Repository registration"))?;
        Ok(RegisterRepositoryOutcome {
            repository,
            created_identity,
            created_locator,
        })
    }

    /// Resolves one stable Repository identity.
    ///
    /// # Errors
    ///
    /// Returns typed Registry read or invariant errors.
    pub fn resolve_by_id(
        &self,
        repository_id: RepositoryId,
    ) -> Result<Option<RegisteredRepository>> {
        read_repository(&self.open_connection()?, repository_id)
    }

    /// Resolves a Repository through one local hint.
    ///
    /// Remote hints resolve only when unique and never create or merge identities.
    ///
    /// # Errors
    ///
    /// Returns typed errors for invalid/ambiguous hints or Registry read failures.
    pub fn resolve_by_locator(
        &self,
        query: &RepositoryLocatorQuery,
    ) -> Result<Option<RegisteredRepository>> {
        let connection = self.open_connection()?;
        let repository_id = match query {
            RepositoryLocatorQuery::CheckoutPath(path) => {
                let path = existing_path_key(path)?;
                connection
                    .query_row(
                        "SELECT repository_id FROM repository_locator WHERE checkout_path = ?1",
                        [path],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(sql_error("resolve Repository by checkout path"))?
                    .map(|value| parse_repository_id(&value))
                    .transpose()?
            }
            RepositoryLocatorQuery::DeclaredIdentity(hint) => {
                let hint = normalize_declared(hint)?;
                connection
                    .query_row(
                        "SELECT repository_id FROM repository_declared_identity WHERE declared_identity = ?1",
                        [hint],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(sql_error("resolve Repository by declared identity"))?
                    .map(|value| parse_repository_id(&value))
                    .transpose()?
            }
            RepositoryLocatorQuery::RemoteHint(hint) => {
                let hint = normalize_remote(hint)?;
                unique_repository_hint(
                    &connection,
                    "SELECT DISTINCT repository_id FROM repository_locator WHERE remote_hint = ?1",
                    &hint,
                    "remote hint is ambiguous across local Repositories",
                )?
            }
        };
        repository_id
            .map(|repository_id| read_repository(&connection, repository_id))
            .transpose()
            .map(Option::flatten)
    }

    /// Marks one known locator unavailable without deleting history.
    ///
    /// # Errors
    ///
    /// Returns an input error for an unknown/cross-Repository locator.
    pub fn mark_unavailable(
        &self,
        repository_id: RepositoryId,
        checkout_path: &Path,
    ) -> Result<RegisteredRepository> {
        self.set_availability(repository_id, checkout_path, false, None)
    }

    /// Re-observes one known checkout as available after verifying Git identity.
    ///
    /// # Errors
    ///
    /// Returns an input error when the path no longer resolves to the registered
    /// common-dir identity.
    pub fn observe_available(
        &self,
        repository_id: RepositoryId,
        checkout_path: &Path,
    ) -> Result<RegisteredRepository> {
        let observation = inspect_checkout(&RegisterRepositoryRequest {
            checkout_path: checkout_path.to_path_buf(),
            declared_identity: None,
            remote_hint: None,
        })?;
        self.set_availability(repository_id, checkout_path, true, Some(&observation))
    }

    /// Forgets one private local locator without deleting Repository identity.
    ///
    /// # Errors
    ///
    /// Returns an input error for an unknown/cross-Repository locator.
    pub fn forget_local_locator(
        &self,
        repository_id: RepositoryId,
        checkout_path: &Path,
    ) -> Result<RegisteredRepository> {
        let path = existing_path_key(checkout_path)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin locator forget transaction")?;
        require_locator_owner(&transaction, repository_id, &path)?;
        let changed = transaction
            .execute(
                "DELETE FROM repository_locator WHERE repository_id = ?1 AND checkout_path = ?2",
                params![repository_id.to_string(), path],
            )
            .map_err(sql_error("forget local Repository locator"))?;
        if changed != 1 {
            return Err(invariant("Repository locator changed inside transaction"));
        }
        refresh_repository_availability(&transaction, repository_id)?;
        let repository = read_repository(&transaction, repository_id)?
            .ok_or_else(|| invariant("Repository identity was deleted while forgetting locator"))?;
        transaction
            .commit()
            .map_err(sql_error("commit locator forget transaction"))?;
        Ok(repository)
    }

    fn set_availability(
        &self,
        repository_id: RepositoryId,
        checkout_path: &Path,
        available: bool,
        observation: Option<&CheckoutObservation>,
    ) -> Result<RegisteredRepository> {
        let path = existing_path_key(checkout_path)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin locator availability transaction")?;
        let stored_common = require_locator_owner(&transaction, repository_id, &path)?;
        if let Some(observation) = observation {
            if observation.git_common_dir_identity != stored_common {
                return Err(invalid(
                    "observed Git common-dir identity differs from registered locator",
                ));
            }
        }
        transaction
            .execute(
                "UPDATE repository_locator SET available = ?1 WHERE repository_id = ?2 AND checkout_path = ?3",
                params![available, repository_id.to_string(), path],
            )
            .map_err(sql_error("update Repository locator availability"))?;
        refresh_repository_availability(&transaction, repository_id)?;
        let repository = read_repository(&transaction, repository_id)?
            .ok_or_else(|| invariant("Repository identity disappeared"))?;
        transaction
            .commit()
            .map_err(sql_error("commit locator availability transaction"))?;
        Ok(repository)
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

#[derive(Clone, Debug)]
struct CheckoutObservation {
    checkout_path: PathBuf,
    git_common_dir: PathBuf,
    git_common_dir_identity: String,
    declared_identity: Option<String>,
    remote_hint: Option<String>,
}

fn inspect_checkout(request: &RegisterRepositoryRequest) -> Result<CheckoutObservation> {
    reject_unsafe_path(&request.checkout_path)?;
    let metadata = fs::symlink_metadata(&request.checkout_path)
        .map_err(io_error("inspect Repository checkout path"))?;
    if metadata.file_type().is_symlink() {
        return Err(invalid("Repository checkout path must not be a symlink"));
    }
    if !metadata.is_dir() {
        return Err(invalid("Repository checkout path must be a directory"));
    }
    let requested = fs::canonicalize(&request.checkout_path)
        .map_err(io_error("canonicalize Repository checkout path"))?;
    let top_level = git_output(&requested, &["rev-parse", "--show-toplevel"])?;
    let checkout_path =
        fs::canonicalize(top_level.trim()).map_err(io_error("canonicalize Git worktree root"))?;
    if checkout_path != requested {
        return Err(invalid(
            "Repository checkout path must identify the Git worktree root",
        ));
    }
    let common = git_output(
        &checkout_path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let git_common_dir =
        fs::canonicalize(common.trim()).map_err(io_error("canonicalize Git common-dir"))?;
    let git_common_dir_identity = common_dir_identity(&git_common_dir)?;
    let declared_identity = request
        .declared_identity
        .as_deref()
        .map(normalize_declared)
        .transpose()?;
    let remote_hint = match request.remote_hint.as_deref() {
        Some(remote) => Some(normalize_remote(remote)?),
        None => discover_remote(&checkout_path)?
            .map(|remote| normalize_remote(&remote))
            .transpose()?,
    };
    Ok(CheckoutObservation {
        checkout_path,
        git_common_dir,
        git_common_dir_identity,
        declared_identity,
        remote_hint,
    })
}

fn reject_unsafe_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(invalid("Repository checkout path must be absolute"));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(
            "Repository checkout path must not contain relative traversal",
        ));
    }
    Ok(())
}

fn existing_path_key(path: &Path) -> Result<String> {
    reject_unsafe_path(path)?;
    let value = if path.exists() {
        fs::canonicalize(path).map_err(io_error("canonicalize Repository locator"))?
    } else {
        path.to_path_buf()
    };
    Ok(value.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn common_dir_identity(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(io_error("inspect Git common-dir identity"))?;
    Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn common_dir_identity(path: &Path) -> Result<String> {
    Ok(format!("canonical:{}", path.to_string_lossy()))
}

fn discover_remote(checkout: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .map_err(io_error("read local Git remote hint"))?;
    if output.status.success() {
        let value = String::from_utf8(output.stdout)
            .map_err(|error| invalid(format!("Git remote hint is not UTF-8: {error}")))?;
        return Ok(Some(value.trim().to_owned()));
    }
    Ok(None)
}

fn git_output(checkout: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(args)
        .output()
        .map_err(io_error("run local Git Repository inspection"))?;
    if !output.status.success() {
        return Err(invalid(format!(
            "path is not a valid Git worktree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| invalid(format!("Git Repository output is not UTF-8: {error}")))
}

fn normalize_declared(value: &str) -> Result<String> {
    normalize_hint(value, "declared Repository identity")
}

fn normalize_remote(value: &str) -> Result<String> {
    let mut normalized = normalize_hint(value, "Repository remote hint")?;
    while normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.as_bytes().ends_with(b".git") {
        normalized.truncate(normalized.len() - 4);
    }
    if normalized.is_empty() {
        return Err(invalid("Repository remote hint must not be empty"));
    }
    Ok(normalized)
}

fn normalize_hint(value: &str, field: &str) -> Result<String> {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if normalized.is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    Ok(normalized)
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
                semantic_fingerprint TEXT NOT NULL,
                available INTEGER NOT NULL CHECK (available IN (0, 1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS repository_declared_identity (
                declared_identity TEXT PRIMARY KEY,
                repository_id TEXT NOT NULL,
                FOREIGN KEY (repository_id) REFERENCES repository_identity (repository_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS repository_locator (
                locator_id INTEGER PRIMARY KEY,
                repository_id TEXT NOT NULL,
                checkout_path TEXT NOT NULL UNIQUE,
                git_common_dir TEXT NOT NULL,
                git_common_dir_identity TEXT NOT NULL,
                declared_identity TEXT,
                remote_hint TEXT,
                available INTEGER NOT NULL CHECK (available IN (0, 1)),
                FOREIGN KEY (repository_id) REFERENCES repository_identity (repository_id)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS repository_locator_common
                ON repository_locator (git_common_dir_identity);
            CREATE INDEX IF NOT EXISTS repository_locator_remote
                ON repository_locator (remote_hint);
            PRAGMA user_version = 1;",
        )
        .map_err(sql_error("initialize Repository Registry schema"))
}

fn repository_by_common_identity(
    connection: &Connection,
    common_identity: &str,
) -> Result<Option<RepositoryId>> {
    unique_repository_hint(
        connection,
        "SELECT DISTINCT repository_id FROM repository_locator WHERE git_common_dir_identity = ?1",
        common_identity,
        "Git common-dir identity maps to multiple Repositories",
    )
}

fn repository_by_declared(connection: &Connection, declared: &str) -> Result<Option<RepositoryId>> {
    connection
        .query_row(
            "SELECT repository_id FROM repository_declared_identity WHERE declared_identity = ?1",
            [declared],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("resolve declared Repository identity"))?
        .map(|value| parse_repository_id(&value))
        .transpose()
}

fn unique_repository_hint(
    connection: &Connection,
    query: &str,
    hint: &str,
    ambiguous_message: &str,
) -> Result<Option<RepositoryId>> {
    let mut statement = connection
        .prepare(query)
        .map_err(sql_error("prepare Repository hint query"))?;
    let rows = statement
        .query_map([hint], |row| row.get::<_, String>(0))
        .map_err(sql_error("query Repository hint"))?;
    let mut ids = HashSet::new();
    for row in rows {
        ids.insert(parse_repository_id(
            &row.map_err(sql_error("read Repository hint row"))?,
        )?);
    }
    match ids.len() {
        0 => Ok(None),
        1 => Ok(ids.into_iter().next()),
        _ => Err(invalid(ambiguous_message)),
    }
}

fn insert_repository(
    transaction: &Transaction<'_>,
    repository_id: RepositoryId,
    observation: &CheckoutObservation,
) -> Result<()> {
    let canonical_name = observation
        .declared_identity
        .clone()
        .or_else(|| {
            observation
                .checkout_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or_else(|| invalid("Repository checkout has no canonical name"))?;
    let semantic = observation.declared_identity.as_ref().map_or_else(
        || format!("git-common:{}", observation.git_common_dir_identity),
        |declared| format!("declared:{declared}"),
    );
    let identity = RepositoryIdentity {
        repository_id,
        canonical_name,
        semantic_fingerprint: SemanticFingerprint::new(semantic)?,
    };
    identity.validate()?;
    transaction
        .execute(
            "INSERT INTO repository_identity (
                repository_id, canonical_name, semantic_fingerprint, available
             ) VALUES (?1, ?2, ?3, 1)",
            params![
                identity.repository_id.to_string(),
                identity.canonical_name,
                identity.semantic_fingerprint.as_str(),
            ],
        )
        .map_err(sql_error("insert Repository identity"))?;
    Ok(())
}

fn attach_declared_identity(
    transaction: &Transaction<'_>,
    repository_id: RepositoryId,
    declared: &str,
) -> Result<()> {
    if let Some(owner) = repository_by_declared(transaction, declared)? {
        if owner != repository_id {
            return Err(invalid(
                "declared Repository identity already belongs to another RepositoryId",
            ));
        }
        return Ok(());
    }
    transaction
        .execute(
            "INSERT INTO repository_declared_identity (declared_identity, repository_id)
             VALUES (?1, ?2)",
            params![declared, repository_id.to_string()],
        )
        .map_err(sql_error("attach declared Repository identity"))?;
    Ok(())
}

fn upsert_locator(
    transaction: &Transaction<'_>,
    repository_id: RepositoryId,
    observation: &CheckoutObservation,
) -> Result<bool> {
    let path = observation.checkout_path.to_string_lossy();
    let existing = transaction
        .query_row(
            "SELECT repository_id FROM repository_locator WHERE checkout_path = ?1",
            [path.as_ref()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("inspect existing Repository locator"))?;
    if let Some(existing) = existing {
        if parse_repository_id(&existing)? != repository_id {
            return Err(invalid(
                "checkout path is already registered to another RepositoryId",
            ));
        }
        transaction
            .execute(
                "UPDATE repository_locator SET
                    git_common_dir = ?1, git_common_dir_identity = ?2,
                    declared_identity = ?3, remote_hint = ?4, available = 1
                 WHERE checkout_path = ?5",
                params![
                    observation.git_common_dir.to_string_lossy(),
                    observation.git_common_dir_identity,
                    observation.declared_identity,
                    observation.remote_hint,
                    path,
                ],
            )
            .map_err(sql_error("refresh Repository locator"))?;
        return Ok(false);
    }
    transaction
        .execute(
            "INSERT INTO repository_locator (
                repository_id, checkout_path, git_common_dir,
                git_common_dir_identity, declared_identity, remote_hint, available
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            params![
                repository_id.to_string(),
                path,
                observation.git_common_dir.to_string_lossy(),
                observation.git_common_dir_identity,
                observation.declared_identity,
                observation.remote_hint,
            ],
        )
        .map_err(sql_error("insert Repository locator"))?;
    Ok(true)
}

fn require_locator_owner(
    connection: &Connection,
    repository_id: RepositoryId,
    path: &str,
) -> Result<String> {
    let row = connection
        .query_row(
            "SELECT repository_id, git_common_dir_identity
             FROM repository_locator WHERE checkout_path = ?1",
            [path],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("read Repository locator owner"))?
        .ok_or_else(|| invalid("Repository locator does not exist"))?;
    if parse_repository_id(&row.0)? != repository_id {
        return Err(invalid(
            "Repository locator belongs to another RepositoryId",
        ));
    }
    Ok(row.1)
}

fn mark_missing_common_locators(
    connection: &Connection,
    repository_id: RepositoryId,
    common_identity: &str,
) -> Result<()> {
    let mut statement = connection
        .prepare(
            "SELECT checkout_path FROM repository_locator
             WHERE repository_id = ?1 AND git_common_dir_identity = ?2 AND available = 1",
        )
        .map_err(sql_error("prepare moved Repository locator check"))?;
    let paths = statement
        .query_map(params![repository_id.to_string(), common_identity], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error("query moved Repository locators"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("read moved Repository locator row"))?;
    drop(statement);
    for path in paths {
        if !Path::new(&path).exists() {
            connection
                .execute(
                    "UPDATE repository_locator SET available = 0
                     WHERE repository_id = ?1 AND checkout_path = ?2",
                    params![repository_id.to_string(), path],
                )
                .map_err(sql_error("mark moved Repository locator unavailable"))?;
        }
    }
    Ok(())
}

fn refresh_repository_availability(
    connection: &Connection,
    repository_id: RepositoryId,
) -> Result<()> {
    connection
        .execute(
            "UPDATE repository_identity
             SET available = EXISTS(
                 SELECT 1 FROM repository_locator
                 WHERE repository_id = ?1 AND available = 1
             )
             WHERE repository_id = ?1",
            [repository_id.to_string()],
        )
        .map_err(sql_error("refresh Repository availability"))?;
    Ok(())
}

fn read_repository(
    connection: &Connection,
    repository_id: RepositoryId,
) -> Result<Option<RegisteredRepository>> {
    let row = connection
        .query_row(
            "SELECT canonical_name, semantic_fingerprint, available
             FROM repository_identity WHERE repository_id = ?1",
            [repository_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Repository identity"))?;
    let Some((canonical_name, fingerprint, available)) = row else {
        return Ok(None);
    };
    let identity = RepositoryIdentity {
        repository_id,
        canonical_name,
        semantic_fingerprint: SemanticFingerprint::new(fingerprint)?,
    };
    identity.validate()?;
    let mut statement = connection
        .prepare(
            "SELECT checkout_path, git_common_dir, git_common_dir_identity,
                    declared_identity, remote_hint, available
             FROM repository_locator WHERE repository_id = ?1
             ORDER BY checkout_path ASC",
        )
        .map_err(sql_error("prepare Repository locators"))?;
    let rows = statement
        .query_map([repository_id.to_string()], |row| {
            Ok(LocalRepositoryLocator {
                checkout_path: PathBuf::from(row.get::<_, String>(0)?),
                git_common_dir: PathBuf::from(row.get::<_, String>(1)?),
                git_common_dir_identity: row.get(2)?,
                declared_identity: row.get(3)?,
                remote_hint: row.get(4)?,
                availability: if row.get::<_, bool>(5)? {
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
