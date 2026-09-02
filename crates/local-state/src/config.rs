use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use fs2::FileExt;
use sctx_domain::{
    ArtifactLocator, Error, ErrorKind, RepoRelativePath, RepositoryId, ResolvedFocus, Result,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CONFIG_VERSION: u32 = 1;
const MAX_CATALOG_REPOSITORIES: usize = 256;
const MAX_CHECKOUTS_PER_REPOSITORY: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u32,
    store: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    repositories: Vec<RepositoryConfigDocument>,
    /// Optional activation switches. Absent means the derived defaults below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activation: Option<ActivationConfigDocument>,
    /// Optional experimental Hook switches. Absent means every switch is off and
    /// the serialized document keeps its previous bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks: Option<HookConfigDocument>,
    /// Optional Context time-to-live policy. Absent means no Context ever expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_ttl: Option<ContextTtlConfigDocument>,
    /// Optional Engineering Graph maintenance switches. Absent means the defaults below, which
    /// keep the Graph attached to the knowledge that names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engineering: Option<EngineeringConfigDocument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retrieval: Option<RetrievalConfigDocument>,
}

/// Optional `[activation]` table: overrides for derived Session activation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationConfigDocument {
    #[serde(default)]
    allow_home: bool,
}

/// Explicit local activation switches, read from the same `config.toml` as the Catalog.
///
/// Activation is derived, never registered: a Session is Enabled when it started
/// inside a registered checkout, or when it started at a directory that contains
/// at least one registered checkout. `allow_home` is the single escape hatch for
/// the home-directory guard in [`parent_activation_is_guarded`]; it is off by default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ActivationSettings {
    pub allow_home: bool,
}

impl ActivationSettings {
    const fn from_document(document: Option<ActivationConfigDocument>) -> Self {
        match document {
            Some(activation) => Self {
                allow_home: activation.allow_home,
            },
            None => Self { allow_home: false },
        }
    }
}

/// Optional `[context_ttl]` table: how long an accepted Context of one kind stays current.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextTtlConfigDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    validation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<String>,
}

/// Configured lifetime per Context kind, in whole seconds.
///
/// An absent entry disables expiry for that kind, and the default policy expires nothing. Expiry
/// is measured from the publication time of the accepted revision. V1 Events carry no timestamp
/// of their own, so that time is the commit time of the Event that published the revision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ContextTtlPolicy {
    pub validation_seconds: Option<i64>,
    pub progress_seconds: Option<i64>,
}

impl ContextTtlPolicy {
    /// Whether any Context kind has a configured lifetime.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.validation_seconds.is_some() || self.progress_seconds.is_some()
    }

    fn from_document(document: Option<&ContextTtlConfigDocument>) -> Result<Self> {
        let Some(document) = document else {
            return Ok(Self::default());
        };
        Ok(Self {
            validation_seconds: document
                .validation
                .as_deref()
                .map(|value| parse_ttl_duration(value, "validation"))
                .transpose()?,
            progress_seconds: document
                .progress
                .as_deref()
                .map(|value| parse_ttl_duration(value, "progress"))
                .transpose()?,
        })
    }
}

/// Parses `<positive integer><s|m|h|d|w>` into whole seconds.
fn parse_ttl_duration(value: &str, field: &str) -> Result<i64> {
    let trimmed = value.trim();
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| invalid(format!("[context_ttl] {field} requires a unit suffix")))?;
    let (digits, unit) = trimmed.split_at(split);
    let amount = digits
        .parse::<i64>()
        .map_err(|_| invalid(format!("[context_ttl] {field} is not a positive duration")))?;
    if amount <= 0 {
        return Err(invalid(format!(
            "[context_ttl] {field} must be a positive duration"
        )));
    }
    let multiplier = match unit {
        "s" => 1_i64,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(invalid(format!(
                "[context_ttl] {field} unit must be one of s, m, h, d, w"
            )));
        }
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| invalid(format!("[context_ttl] {field} duration overflows")))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookConfigDocument {
    #[serde(default)]
    artifact_focus_reminder: bool,
}

/// Explicit local Hook switches read on the bounded Hook hot path.
///
/// Every switch is an off-by-default experiment: a missing `[hooks]` table, a
/// missing key, and an explicit `false` are the same decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HookSettings {
    /// P4.1 experiment: the `PostTool` Hook may run one read-only Engineering Graph lookup
    /// for a located file and emit one bounded Artifact focus reminder.
    pub artifact_focus_reminder: bool,
}

impl HookSettings {
    const fn from_document(document: Option<HookConfigDocument>) -> Self {
        match document {
            Some(hooks) => Self {
                artifact_focus_reminder: hooks.artifact_focus_reminder,
            },
            None => Self {
                artifact_focus_reminder: false,
            },
        }
    }
}

/// Optional `[engineering]` table: Engineering Graph maintenance switches.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineeringConfigDocument {
    #[serde(default = "enabled")]
    auto_scan: bool,
}

const fn enabled() -> bool {
    true
}

impl Default for EngineeringConfigDocument {
    fn default() -> Self {
        Self { auto_scan: true }
    }
}

/// Explicit local Engineering Graph maintenance switches.
///
/// Unlike `[hooks]`, these are on by default: an Engineering Reference that nothing ever scans is
/// an association the installation silently does not have, so the bounded rescan that follows a
/// Confirmation is the normal behaviour and `auto_scan = false` is the explicit opt-out for an
/// installation whose checkouts are too expensive to touch on the interactive path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct EngineeringSettings {
    /// Whether an interactive MCP write that records or names Engineering References may spend
    /// its bounded time budget rescanning the Repositories those References point into.
    pub auto_scan: bool,
}

impl Default for EngineeringSettings {
    fn default() -> Self {
        Self { auto_scan: true }
    }
}

impl EngineeringSettings {
    const fn from_document(document: Option<EngineeringConfigDocument>) -> Self {
        match document {
            Some(engineering) => Self {
                auto_scan: engineering.auto_scan,
            },
            None => Self { auto_scan: true },
        }
    }
}

/// Optional `[retrieval]` table: the local embedding recall channel.
///
/// Both keys name absolute paths the operator downloaded on purpose. Neither has a default and
/// neither is ever guessed: a model this installation did not ask for is 2 GB of disk and a
/// gigabyte of resident memory, so an absent table means the channel does not exist.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetrievalConfigDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding_model_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding_runtime_path: Option<String>,
}

/// Explicit local embedding recall settings.
///
/// The channel is off unless *both* paths are configured. A model without an ONNX Runtime cannot
/// be executed and a runtime without a model has nothing to execute, so half a configuration is
/// the same fact as none -- reported by `sctx doctor`, never half-enabled.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RetrievalSettings {
    /// Directory holding `model.onnx` (plus `model.onnx_data` for an external-data model) and
    /// `tokenizer.json`.
    pub embedding_model_path: Option<PathBuf>,
    /// The ONNX Runtime dynamic library this process loads at run time.
    pub embedding_runtime_path: Option<PathBuf>,
}

impl RetrievalSettings {
    /// True when both halves are present, which is the only state that enables the channel.
    #[must_use]
    pub const fn embedding_enabled(&self) -> bool {
        self.embedding_model_path.is_some() && self.embedding_runtime_path.is_some()
    }

    /// True when exactly one half is configured, which is always an operator mistake.
    #[must_use]
    pub const fn embedding_half_configured(&self) -> bool {
        self.embedding_model_path.is_some() != self.embedding_runtime_path.is_some()
    }

    fn from_document(document: Option<&RetrievalConfigDocument>) -> Result<Self> {
        let Some(document) = document else {
            return Ok(Self::default());
        };
        Ok(Self {
            embedding_model_path: retrieval_path(
                document.embedding_model_path.as_deref(),
                "retrieval.embedding_model_path",
            )?,
            embedding_runtime_path: retrieval_path(
                document.embedding_runtime_path.as_deref(),
                "retrieval.embedding_runtime_path",
            )?,
        })
    }
}

/// Validates one configured retrieval path. A relative path is rejected rather than resolved
/// against an ambiguous working directory: the MCP server, the CLI, and the Hooks all run from
/// different ones.
fn retrieval_path(value: Option<&str>, field: &str) -> Result<Option<PathBuf>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{field} must not be empty"),
        ));
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{field} must be an absolute path"),
        ));
    }
    Ok(Some(path))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryConfigDocument {
    id: RepositoryId,
    paths: Vec<String>,
}

/// One explicitly configured local Repository identity and its checkout roots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogEntry {
    pub repository_id: RepositoryId,
    pub checkout_paths: Vec<PathBuf>,
}

/// Immutable in-memory view used for bounded Repository path mapping and for
/// deriving one Session's activation from its startup directory.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogSnapshot {
    pub repositories: Vec<RepositoryCatalogEntry>,
    pub activation: ActivationSettings,
}

/// Deterministic local revision of the complete explicit `RepositoryCatalog`.
///
/// This is a SHA-256 digest of a canonically ordered serialization of configured
/// Repository IDs, checkout paths, and the explicit activation switches. Computing
/// it is pure: it performs no filesystem scan and never invokes Git.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryCatalogRevision(String);

impl RepositoryCatalogRevision {
    /// Stable textual representation used only by private local state.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Result of an atomic Catalog add operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogAddOutcome {
    pub repository: RepositoryCatalogEntry,
    pub created_identity: bool,
    pub added_paths: usize,
}

/// Result of one atomic local-only Catalog `RepositoryId` rename (ADR-0001, Mew #235).
///
/// Only the local Catalog identity changes. Append-only Git history is never
/// rewritten, so any `EngineeringReference` recorded under `previous_repository_id`
/// keeps that spelling; `sctx repository doctor` reports how many such References
/// remain.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogRenameOutcome {
    pub previous_repository_id: RepositoryId,
    pub repository: RepositoryCatalogEntry,
}

/// Local `SessionStart` authorization decision for one canonical startup directory.
///
/// The decision is two-state on purpose. `Enabled` names exactly the registered
/// Repository identities this Session may record for: one identity when the Session
/// started inside a registered checkout, and every identity with a checkout under the
/// startup directory when it started at a common parent of several checkouts.
/// `Disabled` is everything else, including the guarded ancestors in
/// [`parent_activation_is_guarded`].
///
/// No absolute path crosses this type: activation carries identity, never location,
/// so a decision can be persisted, re-derived, and compared without leaking a
/// checkout path into a lease, an MCP response, a report, or durable Context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivationScope {
    Enabled { repository_ids: Vec<RepositoryId> },
    Disabled,
}

impl ActivationScope {
    /// Registered Repository identities this Session may record for; empty when Disabled.
    #[must_use]
    pub fn repository_ids(&self) -> &[RepositoryId] {
        match self {
            Self::Enabled { repository_ids } => repository_ids,
            Self::Disabled => &[],
        }
    }

    /// Whether Shared Context records anything at all for this Session.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }
}

/// Stable Catalog interpretation of one absolute file path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedRepositoryPath {
    pub repository_id: RepositoryId,
    pub checkout_path: PathBuf,
    pub relative_path: RepoRelativePath,
}

impl ResolvedRepositoryPath {
    /// Converts the resolved Repository identity and exact relative path into
    /// one lossless File Focus without exposing a public submission surface.
    #[must_use]
    pub fn into_resolved_file_focus(self) -> ResolvedFocus {
        ResolvedFocus {
            repository_id: self.repository_id,
            locator: ArtifactLocator::File {
                path: self.relative_path,
            },
        }
    }
}

/// Current validation state of one configured checkout path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCheckoutStatus {
    Available,
    Missing,
    Symlink,
    NotDirectory,
    NotGitRoot,
}

/// Prefix of the pre-ADR-0001 opaque `RepositoryId` spelling. Values with this
/// prefix remain valid and readable (ADR-0001 does not rewrite Git history) but
/// are flagged by `doctor_repository_catalog` for team-name migration via
/// [`UserConfigStore::rename_repository`].
pub const LEGACY_REPOSITORY_ID_PREFIX: &str = "rpo_";

/// One typed, JSON-stable Catalog diagnosis finding. Each variant serializes
/// with a fixed `kind` tag so CLI/MCP callers can match on it without parsing
/// free text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryCatalogDiagnostic {
    /// A Catalog entry still uses the legacy `rpo_<uuid>` identity spelling.
    LegacyRepositoryId {
        repository_id: RepositoryId,
        message: String,
        migration_command: String,
    },
}

/// Bounded explicit Catalog diagnosis.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogDoctorReport {
    pub healthy: bool,
    pub repository_count: usize,
    pub checkout_count: usize,
    pub checkouts: Vec<RepositoryCatalogCheckoutCheck>,
    pub diagnostics: Vec<RepositoryCatalogDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogCheckoutCheck {
    pub repository_id: RepositoryId,
    pub checkout_path: PathBuf,
    pub status: CatalogCheckoutStatus,
}

/// Locked, atomic manager for `config.toml` under one installation root.
#[derive(Clone, Debug)]
pub struct UserConfigStore {
    root: PathBuf,
    config_path: PathBuf,
    repository: PathBuf,
    lock_path: PathBuf,
}

impl UserConfigStore {
    /// Serializes one empty Catalog for the final fixed Store under `root`.
    ///
    /// This is a pure staging helper for transactional reset; it does not create
    /// or modify the installation.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe root or serialization failure.
    pub fn empty_document(root: impl AsRef<Path>) -> Result<String> {
        let root = absolute(root.as_ref())?;
        let document = ConfigDocument {
            version: CONFIG_VERSION,
            store: path_text(&root.join("repository"))?,
            repositories: Vec::new(),
            activation: None,
            hooks: None,
            context_ttl: None,
            engineering: None,
            retrieval: None,
        };
        validate_document_structure(&document, &root.join("repository"))?;
        toml::to_string_pretty(&document).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("serialize empty config.toml: {error}"),
            )
        })
    }

    /// Creates or validates the one user configuration and fixed repository.
    ///
    /// # Errors
    ///
    /// Refuses symlinked/private-state paths, malformed configuration, or a
    /// configured Store different from `<root>/repository`.
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        ensure_private_directory(&root)?;
        let state = root.join("state");
        ensure_private_directory(&state)?;
        let manager = Self {
            config_path: root.join("config.toml"),
            repository: root.join("repository"),
            lock_path: state.join("config.lock"),
            root,
        };
        let lock = manager.lock()?;
        let outcome = (|| {
            if manager.config_path.exists() {
                manager.read_document()?;
                set_file_mode(&manager.config_path, 0o600)?;
            } else {
                manager.write_document(&ConfigDocument {
                    version: CONFIG_VERSION,
                    store: path_text(&manager.repository)?,
                    repositories: Vec::new(),
                    activation: None,
                    hooks: None,
                    context_ttl: None,
                    engineering: None,
                    retrieval: None,
                })?;
            }
            Ok(())
        })();
        finish_locked(&lock, outcome)?;
        Ok(manager)
    }

    /// Opens an existing configuration without creating paths or taking an
    /// exclusive lock. This is the bounded Hook hot-path entry.
    ///
    /// # Errors
    ///
    /// Rejects missing, non-directory, or symlinked installation state.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        let metadata = fs::symlink_metadata(&root).map_err(io_error("inspect config root"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invariant(
                "configuration root must be a non-symlink directory",
            ));
        }
        let state = root.join("state");
        let manager = Self {
            config_path: root.join("config.toml"),
            repository: root.join("repository"),
            lock_path: state.join("config.lock"),
            root,
        };
        reject_symlink(&manager.config_path)?;
        reject_symlink(&manager.lock_path)?;
        for path in [&manager.config_path, &manager.lock_path] {
            let metadata = fs::metadata(path).map_err(io_error("inspect private config file"))?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(invariant(
                    "existing configuration files must be private regular files",
                ));
            }
        }
        Ok(manager)
    }

    /// Installation root containing the sole configured Store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The only configured Git Store path.
    #[must_use]
    pub fn repository(&self) -> &Path {
        &self.repository
    }

    /// Reads the complete explicit Repository Catalog under a shared lock.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn repository_catalog(&self) -> Result<RepositoryCatalogSnapshot> {
        let lock = self.lock_shared()?;
        let outcome = self
            .read_document()
            .map(|document| catalog_snapshot(&document));
        finish_locked(&lock, outcome)
    }

    /// Reads the Catalog and the explicit Hook switches from the same bounded,
    /// non-blocking `config.toml` read.
    ///
    /// The Hook hot path uses this instead of a second file open: the disabled
    /// decision costs exactly the read it already performed.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn repository_catalog_with_hooks(
        &self,
    ) -> Result<(RepositoryCatalogSnapshot, HookSettings)> {
        let lock = self.lock_shared()?;
        let outcome = self.read_document().map(|document| {
            let hooks = HookSettings::from_document(document.hooks);
            (catalog_snapshot(&document), hooks)
        });
        finish_locked(&lock, outcome)
    }

    /// Reads the Catalog and the explicit `[context_ttl]` policy from the same
    /// bounded, non-blocking `config.toml` read.
    ///
    /// Public MCP dispatch authorizes every call against a frozen Catalog and
    /// then serves it under that same Catalog's Context lifetime policy. Both
    /// come from one file, so they come from one read: a second open would add
    /// a lock acquisition per call and could observe a newer `config.toml` than
    /// the one that authorized the call.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn repository_catalog_with_context_ttl(
        &self,
    ) -> Result<(RepositoryCatalogSnapshot, ContextTtlPolicy)> {
        let lock = self.lock_shared()?;
        let outcome = self.read_document().and_then(|document| {
            let context_ttl = ContextTtlPolicy::from_document(document.context_ttl.as_ref())?;
            Ok((catalog_snapshot(&document), context_ttl))
        });
        finish_locked(&lock, outcome)
    }

    /// Reads the explicit `[context_ttl]` policy from `config.toml`.
    ///
    /// A missing table is the default policy: nothing expires. Invalid durations are a typed
    /// configuration error rather than a silently ignored setting.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn context_ttl_policy(&self) -> Result<ContextTtlPolicy> {
        // Request-serving code reads this on every call, so it waits for a concurrent explicit
        // configuration writer instead of failing the call the way the Hook hot path does.
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let outcome = self
            .read_document()
            .and_then(|document| ContextTtlPolicy::from_document(document.context_ttl.as_ref()));
        finish_locked(&lock, outcome)
    }

    /// Reads the explicit `[engineering]` switches from `config.toml`.
    ///
    /// A missing table is the default: automatic bounded rescanning is on. Read the same way as
    /// [`Self::context_ttl_policy`] -- waiting for a concurrent explicit writer rather than
    /// failing the call -- because only request-serving code asks, never the Hook hot path.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn engineering_settings(&self) -> Result<EngineeringSettings> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let outcome = self
            .read_document()
            .map(|document| EngineeringSettings::from_document(document.engineering));
        finish_locked(&lock, outcome)
    }

    /// Reads the explicit `[retrieval]` table.
    ///
    /// A missing table is the default: the embedding channel does not exist. Read with the
    /// blocking shared lock for the same reason as [`Self::engineering_settings`] -- only
    /// request-serving code and `sctx doctor` ask, never the Hook hot path.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn retrieval_settings(&self) -> Result<RetrievalSettings> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let outcome = self
            .read_document()
            .and_then(|document| RetrievalSettings::from_document(document.retrieval.as_ref()));
        finish_locked(&lock, outcome)
    }

    /// Reads the complete Catalog while waiting for a concurrent explicit
    /// configuration writer. Request-serving code uses this after installation
    /// initialization; Hook hot paths keep using the non-blocking reader.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn repository_catalog_wait(&self) -> Result<RepositoryCatalogSnapshot> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let outcome = self
            .read_document()
            .map(|document| catalog_snapshot(&document));
        finish_locked(&lock, outcome)
    }

    /// Reads the Catalog for local CLI list, doctor, and repair flows.
    ///
    /// Unlike [`Self::repository_catalog`], this waits for a concurrent explicit
    /// configuration writer instead of failing the read the way the Hook hot path
    /// does. It is structurally the same Snapshot.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or unexpected filesystem errors.
    pub fn inspect_repository_catalog(&self) -> Result<RepositoryCatalogSnapshot> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let outcome = self
            .read_document()
            .map(|document| catalog_snapshot(&document));
        finish_locked(&lock, outcome)
    }

    /// Resolves the local `SessionStart` `ActivationScope` under the Catalog read lock.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or path validation failures. A
    /// failure never becomes an inferred authorization decision.
    pub fn resolve_activation_scope(
        &self,
        canonical_startup_cwd: &Path,
    ) -> Result<ActivationScope> {
        self.repository_catalog()?
            .resolve_activation_scope(canonical_startup_cwd)
    }

    /// Atomically creates an explicitly named `RepositoryId` or attaches checkout
    /// paths to the existing exact identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, unsafe, non-Git-root, cross-identity, or oversized input.
    #[allow(clippy::too_many_lines)]
    pub fn add_repository(
        &self,
        repository_id: RepositoryId,
        checkout_paths: &[PathBuf],
    ) -> Result<RepositoryCatalogAddOutcome> {
        if checkout_paths.is_empty() {
            return Err(invalid(
                "repository add requires at least one checkout path",
            ));
        }
        if checkout_paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
            return Err(invalid(format!(
                "repository add accepts at most {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths"
            )));
        }
        let paths = checkout_paths
            .iter()
            .map(|path| validate_git_checkout_root(path))
            .collect::<Result<BTreeSet<_>>>()?;
        let lock = self.lock()?;
        let outcome = (|| {
            let mut document = self.read_document()?;
            let configured_owner = document
                .repositories
                .iter()
                .flat_map(|repository| {
                    let repository_id = repository.id.clone();
                    repository
                        .paths
                        .iter()
                        .map(move |path| (path.as_str(), repository_id.clone()))
                })
                .collect::<BTreeMap<_, _>>();
            for path in &paths {
                let text = path_text(path)?;
                if let Some(owner) = configured_owner.get(text.as_str())
                    && &repository_id != owner
                {
                    return Err(invalid(format!(
                        "checkout path is already configured for Repository {owner}"
                    )));
                }
            }
            let created_identity = !document
                .repositories
                .iter()
                .any(|repository| repository.id == repository_id);
            if created_identity {
                if document.repositories.len() >= MAX_CATALOG_REPOSITORIES {
                    return Err(invariant(format!(
                        "Repository Catalog exceeds {MAX_CATALOG_REPOSITORIES} identities"
                    )));
                }
                if let Some(conflict) = document.repositories.iter().find(|repository| {
                    repository
                        .id
                        .as_str()
                        .eq_ignore_ascii_case(repository_id.as_str())
                }) {
                    return Err(invalid(format!(
                        "Repository ID differs only by ASCII case from configured identity {}",
                        conflict.id
                    )));
                }
            }
            let lookup_id = repository_id.clone();
            let repository = if let Some(repository) = document
                .repositories
                .iter_mut()
                .find(|repository| repository.id == repository_id)
            {
                repository
            } else {
                document.repositories.push(RepositoryConfigDocument {
                    id: repository_id,
                    paths: Vec::new(),
                });
                let index = document.repositories.len().saturating_sub(1);
                document
                    .repositories
                    .get_mut(index)
                    .ok_or_else(|| invariant("failed to insert Repository Catalog identity"))?
            };
            let before = repository.paths.len();
            repository.paths.extend(
                paths
                    .iter()
                    .map(|path| path_text(path))
                    .collect::<Result<Vec<_>>>()?,
            );
            repository.paths.sort();
            repository.paths.dedup();
            if repository.paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
                return Err(invariant(format!(
                    "Repository {} exceeds {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths",
                    repository.id
                )));
            }
            let added_paths = repository.paths.len().saturating_sub(before);
            document
                .repositories
                .sort_by_key(|repository| repository.id.to_string());
            self.validate_document(&document)?;
            self.write_document(&document)?;
            let snapshot = catalog_snapshot(&document);
            let repository = snapshot
                .repositories
                .into_iter()
                .find(|repository| repository.repository_id == lookup_id)
                .ok_or_else(|| invariant("configured Repository disappeared before commit"))?;
            Ok(RepositoryCatalogAddOutcome {
                repository,
                created_identity,
                added_paths,
            })
        })();
        finish_locked(&lock, outcome)
    }

    /// Validates every configured checkout without changing Catalog identity.
    ///
    /// # Errors
    ///
    /// Returns configuration or local process errors. Per-checkout drift is
    /// represented in the typed report.
    pub fn doctor_repository_catalog(&self) -> Result<RepositoryCatalogDoctorReport> {
        let catalog = self.inspect_repository_catalog()?;
        let mut checkouts = Vec::new();
        for repository in &catalog.repositories {
            for path in &repository.checkout_paths {
                checkouts.push(RepositoryCatalogCheckoutCheck {
                    repository_id: repository.repository_id.clone(),
                    checkout_path: path.clone(),
                    status: checkout_status(path)?,
                });
            }
        }
        let healthy = checkouts
            .iter()
            .all(|checkout| checkout.status == CatalogCheckoutStatus::Available);
        let diagnostics = catalog
            .repositories
            .iter()
            .filter(|repository| {
                repository
                    .repository_id
                    .as_str()
                    .starts_with(LEGACY_REPOSITORY_ID_PREFIX)
            })
            .map(
                |repository| RepositoryCatalogDiagnostic::LegacyRepositoryId {
                    repository_id: repository.repository_id.clone(),
                    message: format!(
                        "Repository {} still uses the pre-ADR-0001 legacy identity spelling; \
                     rename it to a team-chosen readable name. Existing EngineeringReference \
                     events keep the legacy spelling (ADR-0001 does not rewrite Git history).",
                        repository.repository_id
                    ),
                    migration_command: format!(
                        "sctx repository rename --from {} --to <ReadableRepositoryId>",
                        repository.repository_id
                    ),
                },
            )
            .collect();
        Ok(RepositoryCatalogDoctorReport {
            healthy,
            repository_count: catalog.repositories.len(),
            checkout_count: checkouts.len(),
            checkouts,
            diagnostics,
        })
    }

    /// Atomically renames one local Catalog `RepositoryId` identity.
    ///
    /// Only the local Catalog changes: no Git Event is appended or rewritten,
    /// so any `EngineeringReference` recorded under `from` durably keeps that
    /// spelling (ADR-0001).
    ///
    /// # Errors
    ///
    /// Returns `ErrorKind::InvalidInput` when `from` and `to` are identical,
    /// `ErrorKind::RepositoryNotConfigured` when `from` is not configured, and
    /// `ErrorKind::Conflict` when `to` already names a configured identity
    /// (exact or case-insensitive match).
    pub fn rename_repository(
        &self,
        from: &RepositoryId,
        to: &RepositoryId,
    ) -> Result<RepositoryCatalogRenameOutcome> {
        if from.as_str() == to.as_str() {
            return Err(invalid(
                "repository rename requires a --to identity different from --from",
            ));
        }
        let lock = self.lock()?;
        let outcome = (|| {
            let mut document = self.read_document()?;
            let index = document
                .repositories
                .iter()
                .position(|repository| &repository.id == from)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::RepositoryNotConfigured,
                        format!("Repository identity is not configured: {from}"),
                    )
                })?;
            if let Some(conflict) = document
                .repositories
                .iter()
                .find(|repository| repository.id.as_str().eq_ignore_ascii_case(to.as_str()))
            {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "Repository ID {to} already names configured identity {}",
                        conflict.id
                    ),
                ));
            }
            document.repositories[index].id = to.clone();
            document
                .repositories
                .sort_by_key(|repository| repository.id.to_string());
            self.validate_document(&document)?;
            self.write_document(&document)?;
            let repository = catalog_snapshot(&document)
                .repositories
                .into_iter()
                .find(|repository| &repository.repository_id == to)
                .ok_or_else(|| invariant("renamed Repository disappeared before commit"))?;
            Ok(RepositoryCatalogRenameOutcome {
                previous_repository_id: from.clone(),
                repository,
            })
        })();
        finish_locked(&lock, outcome)
    }

    fn lock(&self) -> Result<File> {
        let lock = open_private_file(&self.lock_path)?;
        lock.lock_exclusive()
            .map_err(io_error("lock config.lock"))?;
        Ok(lock)
    }

    fn lock_shared(&self) -> Result<File> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::try_lock_shared(&lock).map_err(io_error("try lock config.lock shared"))?;
        Ok(lock)
    }

    fn read_document(&self) -> Result<ConfigDocument> {
        reject_symlink(&self.config_path)?;
        let text = fs::read_to_string(&self.config_path).map_err(io_error("read config.toml"))?;
        let document: ConfigDocument = toml::from_str(&text).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("parse {}: {error}", self.config_path.display()),
            )
        })?;
        validate_document_structure(&document, &self.repository)?;
        Ok(document)
    }

    fn validate_document(&self, document: &ConfigDocument) -> Result<()> {
        validate_document_structure(document, &self.repository)
    }

    fn write_document(&self, document: &ConfigDocument) -> Result<()> {
        let text = toml::to_string_pretty(document).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("serialize config.toml: {error}"),
            )
        })?;
        let temporary = self
            .root
            .join(format!(".config.{}.tmp", uuid::Uuid::new_v4().hyphenated()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(io_error("create temporary config"))?;
        file.write_all(text.as_bytes())
            .map_err(io_error("write temporary config"))?;
        file.sync_all().map_err(io_error("sync temporary config"))?;
        fs::rename(&temporary, &self.config_path).map_err(io_error("replace config.toml"))?;
        sync_directory(&self.root)?;
        set_file_mode(&self.config_path, 0o600)
    }
}

impl RepositoryCatalogSnapshot {
    /// Computes the semantic revision used to invalidate local Session leases.
    ///
    /// Ordering differences in caller-built Snapshots do not change the result.
    /// The revision changes when any configured Repository identity, checkout,
    /// or activation switch changes. No path is inspected and Git is never
    /// executed.
    ///
    /// # Errors
    ///
    /// Returns an input error when a configured path cannot be serialized.
    pub fn revision(&self) -> Result<RepositoryCatalogRevision> {
        let mut canonical = self.clone();
        canonical
            .repositories
            .sort_by_key(|repository| repository.repository_id.clone());
        for repository in &mut canonical.repositories {
            repository.checkout_paths.sort();
        }
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|_| invalid("serialize Repository Catalog revision source"))?;
        let digest = Sha256::digest(bytes);
        Ok(RepositoryCatalogRevision(format!("sha256:{digest:x}")))
    }

    /// Resolves one canonical Agent startup directory into local authorization.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical startup directory or ambiguous Catalog ownership.
    /// Repository health belongs to Catalog configuration and doctor operations;
    /// this `SessionStart` resolver does not invoke Git.
    pub fn resolve_activation_scope(
        &self,
        canonical_startup_cwd: &Path,
    ) -> Result<ActivationScope> {
        validate_canonical_directory(canonical_startup_cwd, "Agent startup directory")?;
        self.resolve_recorded_activation_scope(canonical_startup_cwd)
    }

    /// Derives the activation of one already-canonical startup directory without
    /// touching the filesystem.
    ///
    /// Two rules, in order, and nothing else is registered by hand:
    ///
    /// 1. The directory is inside a registered checkout — the deepest one wins —
    ///    so the Session records for exactly that Repository.
    /// 2. The directory *contains* registered checkouts, which is what starting an
    ///    Agent at the common parent of several checkouts looks like, so the Session
    ///    records for every Repository whose checkout lives below it.
    ///
    /// Anything else is Disabled, including the guarded ancestors described in
    /// [`parent_activation_is_guarded`]: without that guard, starting an Agent in the
    /// home directory would enable every Repository on the machine, which is the
    /// opposite of recording only inside the Repositories the user registered.
    ///
    /// This is the pure re-resolution seam an activation lease uses to follow later
    /// Catalog edits: it never stats a path, never runs Git, and never scans a
    /// Repository, so it is safe on the Hook and MCP hot paths.
    ///
    /// # Errors
    ///
    /// Rejects a relative or dot-segmented directory and ambiguous Catalog ownership.
    pub fn resolve_recorded_activation_scope(
        &self,
        canonical_startup_cwd: &Path,
    ) -> Result<ActivationScope> {
        validate_absolute_path(canonical_startup_cwd, "Agent startup directory")?;

        if let Some((repository_id, _)) = self.deepest_checkout_for(canonical_startup_cwd)? {
            return Ok(ActivationScope::Enabled {
                repository_ids: vec![repository_id],
            });
        }
        if parent_activation_is_guarded(canonical_startup_cwd, self.activation) {
            return Ok(ActivationScope::Disabled);
        }
        let repository_ids = self
            .repositories
            .iter()
            .filter(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .any(|checkout| checkout.starts_with(canonical_startup_cwd))
            })
            .map(|repository| repository.repository_id.clone())
            .collect::<BTreeSet<_>>();
        if repository_ids.is_empty() {
            return Ok(ActivationScope::Disabled);
        }
        Ok(ActivationScope::Enabled {
            repository_ids: repository_ids.into_iter().collect(),
        })
    }

    /// Deepest configured checkout that contains `path`, or `None` when the path is
    /// outside every configured checkout.
    ///
    /// This is the one longest-prefix ownership rule shared by activation derivation
    /// and Hook path attribution. It is pure: no stat, no Git, no scan.
    ///
    /// # Errors
    ///
    /// Returns an invariant violation when the same deepest checkout is claimed by two
    /// Repository identities, which configuration validation already forbids.
    pub fn deepest_checkout_for(&self, path: &Path) -> Result<Option<(RepositoryId, PathBuf)>> {
        let mut matches = self
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .filter_map(move |checkout| {
                        path.starts_with(checkout)
                            .then_some((repository.repository_id.clone(), checkout))
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .1
                .components()
                .count()
                .cmp(&left.1.components().count())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some((repository_id, checkout_path)) = matches.first().cloned() else {
            return Ok(None);
        };
        if matches.iter().skip(1).any(|(other_id, other_path)| {
            *other_path == checkout_path && other_id != &repository_id
        }) {
            return Err(invariant(
                "ActivationScope has ambiguous checkout ownership",
            ));
        }
        Ok(Some((repository_id, checkout_path.clone())))
    }

    /// Canonicalizes and resolves one existing absolute file inside both an
    /// allowed Workspace and one configured checkout.
    ///
    /// # Errors
    ///
    /// Returns a typed not-configured error or rejects unsafe/outside paths.
    pub fn resolve_file_path(
        &self,
        file_path: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<ResolvedRepositoryPath> {
        validate_absolute_path(file_path, "file path")?;
        let metadata = fs::symlink_metadata(file_path).map_err(io_error("inspect file path"))?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("file path must not be a symlink"));
        }
        if !metadata.is_file() {
            return Err(invalid("file path must identify a regular file"));
        }
        let canonical_file = fs::canonicalize(file_path).map_err(io_error("canonicalize file"))?;
        let canonical_workspaces = canonical_workspace_roots(workspace_roots)?;
        let resolved = self.resolve_canonical_file(&canonical_file, &canonical_workspaces)?;
        if !file_path.starts_with(&resolved.checkout_path) {
            return Err(invalid(
                "file path must use the configured canonical checkout path",
            ));
        }
        reject_symlink_below_checkout(file_path, &resolved.checkout_path)?;
        Ok(resolved)
    }

    /// Resolves one Agent-declared absolute Artifact path through the configured Catalog without
    /// requiring the leaf (or a trailing path suffix) to exist.
    ///
    /// Existing components are checked for symlink escape and non-directory parents. No Git,
    /// scanner, Workspace, or source-existence claim participates in this mapping.
    ///
    /// # Errors
    ///
    /// Returns `RepositoryNotConfigured` when no configured checkout is a prefix, or rejects dot
    /// segments, symlink traversal, and an existing non-directory before the final component.
    pub fn resolve_declared_path(&self, declared_path: &Path) -> Result<ResolvedRepositoryPath> {
        validate_absolute_path(declared_path, "declared Artifact path")?;
        let mut matches = self
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .filter_map(move |checkout| {
                        declared_path
                            .starts_with(checkout)
                            .then_some((repository.repository_id.clone(), checkout))
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .1
                .components()
                .count()
                .cmp(&left.1.components().count())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some((repository_id, checkout_path)) = matches.first().cloned() else {
            return Err(Error::new(
                ErrorKind::RepositoryNotConfigured,
                "declared Artifact path is not inside a configured Repository checkout",
            ));
        };
        validate_declared_components(declared_path, checkout_path)?;
        let relative = declared_path
            .strip_prefix(checkout_path)
            .map_err(|_| invariant("configured declared-path prefix disappeared"))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| invalid("Repository-relative Artifact path must be valid UTF-8"))?;
        Ok(ResolvedRepositoryPath {
            repository_id,
            checkout_path: checkout_path.clone(),
            relative_path: RepoRelativePath::new(relative)?,
        })
    }

    /// Pure longest-prefix mapping for already-canonical paths.
    ///
    /// # Errors
    ///
    /// Returns `RepositoryNotConfigured` when no configured checkout matches,
    /// or `InvalidInput` when the file is outside every allowed Workspace.
    pub fn resolve_canonical_file(
        &self,
        canonical_file: &Path,
        canonical_workspace_roots: &[PathBuf],
    ) -> Result<ResolvedRepositoryPath> {
        validate_absolute_path(canonical_file, "canonical file path")?;
        if canonical_workspace_roots.is_empty()
            || !canonical_workspace_roots
                .iter()
                .any(|workspace| canonical_file.starts_with(workspace))
        {
            return Err(invalid("file path is outside the allowed Workspace"));
        }
        let mut matches = self
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .filter_map(move |checkout| {
                        canonical_file
                            .starts_with(checkout)
                            .then_some((repository.repository_id.clone(), checkout))
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .1
                .components()
                .count()
                .cmp(&left.1.components().count())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some((repository_id, checkout_path)) = matches.first().cloned() else {
            return Err(Error::new(
                ErrorKind::RepositoryNotConfigured,
                "file path is not inside a configured Repository checkout",
            ));
        };
        let relative = canonical_file
            .strip_prefix(checkout_path)
            .map_err(|_| invariant("longest-prefix Repository match disappeared"))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| invalid("Repository-relative file path must be valid UTF-8"))?;
        Ok(ResolvedRepositoryPath {
            repository_id,
            checkout_path: checkout_path.clone(),
            relative_path: RepoRelativePath::new(relative)?,
        })
    }
}

fn catalog_snapshot(document: &ConfigDocument) -> RepositoryCatalogSnapshot {
    RepositoryCatalogSnapshot {
        repositories: document
            .repositories
            .iter()
            .map(|repository| RepositoryCatalogEntry {
                repository_id: repository.id.clone(),
                checkout_paths: repository.paths.iter().map(PathBuf::from).collect(),
            })
            .collect(),
        activation: ActivationSettings::from_document(document.activation),
    }
}

/// Startup directories that never derive parent-directory activation.
///
/// The filesystem root, the user's home directory, and the directory that contains
/// home are the ancestors that essentially every checkout on the machine shares, so
/// deriving activation from them would enable every registered Repository at once.
/// The guard is structural rather than a blocklist: it applies only to rule 2 of
/// [`RepositoryCatalogSnapshot::resolve_recorded_activation_scope`], so a checkout the
/// user explicitly registered at such a path still activates directly.
///
/// `[activation] allow_home = true` lifts the two home guards for operators who really
/// do keep every Repository directly under home. The filesystem root is never derivable.
fn parent_activation_is_guarded(directory: &Path, settings: ActivationSettings) -> bool {
    if directory.parent().is_none() {
        return true;
    }
    if settings.allow_home {
        return false;
    }
    let Some(home) = home_directory() else {
        return false;
    };
    directory == home || Some(directory) == home.parent()
}

/// The invoking user's home directory, when the environment names an absolute one
/// that is not itself the filesystem root.
fn home_directory() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    (home.is_absolute() && home.parent().is_some()).then_some(home)
}

/// Rewrites one `config.toml` body that still carries the removed `[[repository_groups]]`
/// section, returning `None` when there is nothing to migrate.
///
/// Explicit Groups were the hand-registered form of "this parent directory activates these
/// Repositories"; activation now derives that from the registered checkouts themselves, so
/// the section carries no decision any more and a document that still contains it no longer
/// parses. `upgrade` and `setup` drop it inside their transaction, which keeps a backup.
///
/// # Errors
///
/// Returns an input error when the document is not parseable TOML or cannot be re-serialized.
pub fn migrate_legacy_repository_groups(document: &str) -> Result<Option<String>> {
    let mut table: toml::Table = document
        .parse()
        .map_err(|error| invalid(format!("parse config.toml for migration: {error}")))?;
    if table.remove("repository_groups").is_none() {
        return Ok(None);
    }
    toml::to_string_pretty(&table)
        .map(Some)
        .map_err(|error| invalid(format!("serialize migrated config.toml: {error}")))
}

fn validate_document_structure(document: &ConfigDocument, repository: &Path) -> Result<()> {
    if document.version != CONFIG_VERSION {
        return Err(invariant(format!(
            "unsupported config version {}",
            document.version
        )));
    }
    let expected = path_text(repository)?;
    if document.store != expected {
        return Err(invariant(format!(
            "config must contain exactly the fixed Store {}; found {}",
            repository.display(),
            document.store
        )));
    }
    validate_repository_documents(&document.repositories)
}

fn validate_repository_documents(repositories: &[RepositoryConfigDocument]) -> Result<()> {
    if repositories.len() > MAX_CATALOG_REPOSITORIES {
        return Err(invariant(format!(
            "Repository Catalog exceeds {MAX_CATALOG_REPOSITORIES} identities"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeMap::<String, RepositoryId>::new();
    for repository in repositories {
        if !ids.insert(repository.id.clone()) {
            return Err(invalid(format!(
                "duplicate Repository Catalog identity: {}",
                repository.id
            )));
        }
        if repository.paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
            return Err(invariant(format!(
                "Repository {} exceeds {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths",
                repository.id
            )));
        }
        let mut own_paths = BTreeSet::new();
        for path in &repository.paths {
            let path_value = Path::new(path);
            validate_absolute_path(path_value, "Repository checkout path")?;
            if !own_paths.insert(path) {
                return Err(invalid(format!(
                    "duplicate checkout path for Repository {}: {path}",
                    repository.id
                )));
            }
            if let Some(owner) = paths.insert(path.clone(), repository.id.clone()) {
                return Err(invalid(format!(
                    "checkout path belongs to multiple Repository identities: {owner} and {}",
                    repository.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_git_checkout_root(path: &Path) -> Result<PathBuf> {
    validate_absolute_path(path, "Repository checkout path")?;
    match checkout_status(path)? {
        CatalogCheckoutStatus::Available => {
            fs::canonicalize(path).map_err(io_error("canonicalize Repository checkout"))
        }
        CatalogCheckoutStatus::Missing => Err(invalid("Repository checkout path does not exist")),
        CatalogCheckoutStatus::Symlink => {
            Err(invalid("Repository checkout path must not be a symlink"))
        }
        CatalogCheckoutStatus::NotDirectory => {
            Err(invalid("Repository checkout path must be a directory"))
        }
        CatalogCheckoutStatus::NotGitRoot => Err(invalid(
            "Repository checkout path must be a Git worktree root",
        )),
    }
}

fn validate_canonical_directory(path: &Path, field: &str) -> Result<()> {
    validate_absolute_path(path, field)?;
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect canonical directory"))?;
    if metadata.file_type().is_symlink() {
        return Err(invalid(format!("{field} must not be a symlink")));
    }
    if !metadata.is_dir() {
        return Err(invalid(format!("{field} must be a directory")));
    }
    let canonical = fs::canonicalize(path).map_err(io_error("canonicalize directory"))?;
    if canonical != path {
        return Err(invalid(format!("{field} must use its canonical path")));
    }
    Ok(())
}

fn checkout_status(path: &Path) -> Result<CatalogCheckoutStatus> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CatalogCheckoutStatus::Missing);
        }
        Err(error) => return Err(io_error("inspect Repository checkout")(error)),
    };
    if metadata.file_type().is_symlink() {
        return Ok(CatalogCheckoutStatus::Symlink);
    }
    if !metadata.is_dir() {
        return Ok(CatalogCheckoutStatus::NotDirectory);
    }
    let canonical = fs::canonicalize(path).map_err(io_error("canonicalize Repository checkout"))?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(io_error("inspect configured Git Repository"))?;
    if !output.status.success() {
        return Ok(CatalogCheckoutStatus::NotGitRoot);
    }
    let top_level = String::from_utf8(output.stdout)
        .map_err(|error| invalid(format!("Git top-level is not UTF-8: {error}")))?;
    let top_level = fs::canonicalize(top_level.trim())
        .map_err(io_error("canonicalize configured Git top-level"))?;
    Ok(if top_level == canonical {
        CatalogCheckoutStatus::Available
    } else {
        CatalogCheckoutStatus::NotGitRoot
    })
}

fn canonical_workspace_roots(workspace_roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    for root in workspace_roots {
        validate_absolute_path(root, "Workspace root")?;
        let metadata = fs::symlink_metadata(root).map_err(io_error("inspect Workspace root"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid("Workspace root must be a non-symlink directory"));
        }
        roots.insert(fs::canonicalize(root).map_err(io_error("canonicalize Workspace root"))?);
    }
    Ok(roots.into_iter().collect())
}

fn reject_symlink_below_checkout(path: &Path, checkout: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(checkout)
        .map_err(|_| invalid("file path is outside the configured checkout"))?;
    let mut current = checkout.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(io_error("inspect file component"))?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("file path must not traverse a symlink"));
        }
    }
    Ok(())
}

fn validate_declared_components(path: &Path, checkout: &Path) -> Result<()> {
    let checkout_metadata = match fs::symlink_metadata(checkout) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("inspect configured checkout")(error)),
    };
    if checkout_metadata.file_type().is_symlink() || !checkout_metadata.is_dir() {
        return Err(invalid(
            "configured checkout must remain a non-symlink directory",
        ));
    }
    if fs::canonicalize(checkout).map_err(io_error("canonicalize configured checkout"))? != checkout
    {
        return Err(invalid(
            "configured checkout canonical path changed or traverses a symlink",
        ));
    }
    let relative = path
        .strip_prefix(checkout)
        .map_err(|_| invalid("declared Artifact path is outside the configured checkout"))?;
    let components = relative.components().collect::<Vec<_>>();
    let mut current = checkout.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error("inspect declared Artifact component")(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(invalid(
                "declared Artifact path must not traverse a symlink",
            ));
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Err(invalid(
                "declared Artifact path traverses an existing non-directory",
            ));
        }
    }
    Ok(())
}

fn validate_absolute_path(path: &Path, field: &str) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid(format!(
            "{field} must be an absolute path without dot segments"
        )));
    }
    if path.to_str().is_none() {
        return Err(invalid(format!("{field} must be valid UTF-8")));
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(io_error("make path absolute"))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {}", path.display()),
        )
    })
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(io_error("inspect private directory"))?;
        if !metadata.file_type().is_dir() {
            return Err(invariant(format!(
                "private state path is not a directory: {}",
                path.display()
            )));
        }
    } else {
        fs::create_dir_all(path).map_err(io_error("create private directory"))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set private directory permissions"))
}

fn open_private_file(path: &Path) -> Result<File> {
    reject_symlink(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(io_error("open private file"))?;
    set_file_mode(path, 0o600)?;
    Ok(file)
}

fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(io_error("set private file permissions"))
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invariant(format!(
            "refusing symlinked private state path: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("inspect {}: {error}", path.display()),
        )),
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("sync directory"))
}

fn finish_locked<T>(lock: &File, outcome: Result<T>) -> Result<T> {
    FileExt::unlock(lock).map_err(io_error("unlock config.lock"))?;
    outcome
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{operation}: {error}"))
}
