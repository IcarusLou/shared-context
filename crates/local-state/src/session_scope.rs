use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use sctx_domain::{
    Error, ErrorKind, ExternalSessionLocator, RepositoryGroupId, RepositoryId, Result,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ActivationScope, ActivationScopeDecision, RepositoryCatalogRevision, RepositoryCatalogSnapshot,
};

const MAX_LOCATOR_BYTES: usize = 4 * 1024;
const MAX_SCOPE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Typed on-disk schema version for one private activation lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizedSessionScopeRecordVersion {
    V1,
}

/// Minimal persisted authorization decision without checkout or Group-root paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorizedSessionScopeDecision {
    Disabled,
    Direct {
        repository_id: RepositoryId,
    },
    Group {
        repository_group_id: RepositoryGroupId,
    },
}

/// One `ExternalSessionLocator`-owned, TTL-bounded local activation lease.
///
/// The record is disposable authorization state. It is not a `TaskSession`, a
/// Workspace route, a Context fact, or durable engineering knowledge.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedSessionScope {
    pub version: AuthorizedSessionScopeRecordVersion,
    pub external_session_locator: ExternalSessionLocator,
    pub decision: AuthorizedSessionScopeDecision,
    pub allowed_repository_ids: Vec<RepositoryId>,
    pub catalog_revision: RepositoryCatalogRevision,
    pub issued_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
}

impl AuthorizedSessionScope {
    fn validate(&self) -> Result<()> {
        self.external_session_locator.validate()?;
        validate_locator_size(&self.external_session_locator)?;
        self.catalog_revision.validate()?;
        if self.expires_at_unix_seconds <= self.issued_at_unix_seconds {
            return Err(invalid(
                "AuthorizedSessionScope expiry must follow its issue time",
            ));
        }
        if self
            .expires_at_unix_seconds
            .saturating_sub(self.issued_at_unix_seconds)
            > MAX_SCOPE_TTL.as_secs()
        {
            return Err(invalid("AuthorizedSessionScope TTL exceeds the maximum"));
        }
        if self.allowed_repository_ids.len() > 256 {
            return Err(invalid(
                "AuthorizedSessionScope contains too many Repository identities",
            ));
        }
        let unique = self
            .allowed_repository_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if unique.len() != self.allowed_repository_ids.len()
            || !self
                .allowed_repository_ids
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        {
            return Err(invalid(
                "AuthorizedSessionScope Repository identities must be unique and sorted",
            ));
        }
        match self.decision {
            AuthorizedSessionScopeDecision::Disabled if self.allowed_repository_ids.is_empty() => {
                Ok(())
            }
            AuthorizedSessionScopeDecision::Direct { repository_id }
                if self.allowed_repository_ids == [repository_id] =>
            {
                Ok(())
            }
            AuthorizedSessionScopeDecision::Group { .. }
                if !self.allowed_repository_ids.is_empty() =>
            {
                Ok(())
            }
            _ => Err(invalid(
                "AuthorizedSessionScope decision and allowed Repositories disagree",
            )),
        }
    }

    fn semantically_matches(&self, other: &Self) -> bool {
        self.external_session_locator == other.external_session_locator
            && self.decision == other.decision
            && self.allowed_repository_ids == other.allowed_repository_ids
            && self.catalog_revision == other.catalog_revision
    }
}

/// TTL and aggregate ceilings for private activation leases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionScopePolicy {
    pub ttl: Duration,
    pub max_entry_bytes: usize,
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for AuthorizedSessionScopePolicy {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(2 * 60 * 60),
            max_entry_bytes: 16 * 1024,
            max_entries: 4_096,
            max_total_bytes: 8 * 1024 * 1024,
        }
    }
}

impl AuthorizedSessionScopePolicy {
    fn validate(self) -> Result<()> {
        if self.ttl.is_zero() || self.ttl > MAX_SCOPE_TTL {
            return Err(invalid(
                "AuthorizedSessionScope TTL must be between one second and 24 hours",
            ));
        }
        if self.max_entry_bytes == 0 || self.max_entries == 0 || self.max_total_bytes == 0 {
            return Err(invalid(
                "AuthorizedSessionScope entry and aggregate limits must be greater than zero",
            ));
        }
        if self.max_entry_bytes as u64 > self.max_total_bytes {
            return Err(invalid(
                "AuthorizedSessionScope entry limit must not exceed its aggregate byte limit",
            ));
        }
        Ok(())
    }
}

/// Result of one serialized authorization operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionScopeAuthorizeOutcome {
    pub scope: AuthorizedSessionScope,
    /// False when this operation only refreshed an identical authorization.
    pub semantic_changed: bool,
}

/// Fail-closed interpretation of one locator's private lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorizedSessionScopeRead {
    Missing,
    Expired,
    StaleCatalog,
    Current(AuthorizedSessionScope),
}

/// Safe diagnosis category for an entry cleanup refused to interpret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizedSessionScopeCleanupDiagnosticKind {
    InvalidRecord,
    UnsafeEntry,
    Oversized,
}

/// One deterministic cleanup diagnosis. The entry key is a SHA-256 filename,
/// never raw external Session text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionScopeCleanupDiagnostic {
    pub entry_key: String,
    pub kind: AuthorizedSessionScopeCleanupDiagnosticKind,
}

/// Result of narrow TTL cleanup under `state/authorized-session-scopes` only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizedSessionScopeCleanup {
    pub removed_entry_keys: Vec<String>,
    pub reclaimed_bytes: u64,
    pub diagnostics: Vec<AuthorizedSessionScopeCleanupDiagnostic>,
}

/// Private, bounded activation lease storage outside the Git repository.
#[derive(Clone, Debug)]
pub struct AuthorizedSessionScopeStore {
    directory: PathBuf,
    lock_path: PathBuf,
    policy: AuthorizedSessionScopePolicy,
}

impl AuthorizedSessionScopeStore {
    /// Opens `<root>/state/authorized-session-scopes` with default limits.
    ///
    /// # Errors
    ///
    /// Refuses unsafe private-state paths or permission setup failures.
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self> {
        Self::with_policy(root, AuthorizedSessionScopePolicy::default())
    }

    /// Opens the private store with an explicit bounded policy.
    ///
    /// # Errors
    ///
    /// Rejects zero/oversized limits, symlinks, non-directories, or I/O failure.
    pub fn with_policy(
        root: impl AsRef<Path>,
        policy: AuthorizedSessionScopePolicy,
    ) -> Result<Self> {
        policy.validate()?;
        let root = std::path::absolute(root.as_ref()).map_err(io_error("make root absolute"))?;
        let state = root.join("state");
        ensure_private_directory(&root)?;
        ensure_private_directory(&state)?;
        let directory = state.join("authorized-session-scopes");
        ensure_private_directory(&directory)?;
        Ok(Self {
            lock_path: state.join("authorized-session-scopes.lock"),
            directory,
            policy,
        })
    }

    /// Exact product-private record directory. This is never a business Repository path.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Atomically creates or replaces one locator's activation lease.
    ///
    /// This is the future `SessionStart` authorization seam. Later lifecycle
    /// events must call [`Self::read`] and must not re-resolve a changed CWD.
    ///
    /// Concurrent calls for the same locator linearize under the Store lock.
    /// A semantically identical retry atomically refreshes the one record's TTL
    /// without expanding its authorization; a different explicit call replaces
    /// that locator's decision and reports a semantic change.
    ///
    /// # Errors
    ///
    /// Fails before writing when the decision was not produced by the supplied
    /// Catalog, limits would be exceeded, or existing state is unsafe.
    pub fn authorize(
        &self,
        external_session_locator: &ExternalSessionLocator,
        activation_scope: &ActivationScope,
        catalog: &RepositoryCatalogSnapshot,
    ) -> Result<AuthorizedSessionScopeAuthorizeOutcome> {
        self.authorize_at(
            external_session_locator,
            activation_scope,
            catalog,
            SystemTime::now(),
        )
    }

    /// Reads one lease and validates it against the current Catalog snapshot.
    ///
    /// Missing, expired, or stale records are explicit non-authorizing results.
    /// Expired records are narrowly removed. Corrupt, oversized, mismatched, or
    /// unsafe records return an error and never become authorization.
    ///
    /// # Errors
    ///
    /// Returns typed validation, Catalog, locking, or filesystem failures.
    pub fn read(
        &self,
        external_session_locator: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
    ) -> Result<AuthorizedSessionScopeRead> {
        self.read_at(external_session_locator, catalog, SystemTime::now())
    }

    /// Removes this locator only when its valid record has expired.
    ///
    /// # Errors
    ///
    /// Refuses an unsafe record or cross-locator digest mismatch.
    pub fn expire(&self, external_session_locator: &ExternalSessionLocator) -> Result<bool> {
        self.expire_at(external_session_locator, SystemTime::now())
    }

    /// Unconditionally removes one exact locator's valid record for `SessionEnd`.
    ///
    /// # Errors
    ///
    /// Refuses an unsafe record or cross-locator digest mismatch.
    pub fn remove(&self, external_session_locator: &ExternalSessionLocator) -> Result<bool> {
        validate_locator(external_session_locator)?;
        self.with_lock(|| {
            let path = self.record_path(external_session_locator);
            let Some(record) = self.read_optional_record(&path)? else {
                return Ok(false);
            };
            verify_record_locator(&record, external_session_locator)?;
            fs::remove_file(&path).map_err(io_error("remove AuthorizedSessionScope"))?;
            sync_directory(&self.directory)?;
            Ok(true)
        })
    }

    /// Removes all expired valid records in deterministic filename order.
    ///
    /// Corrupt, oversized, symlink, or non-regular entries are diagnosed and
    /// preserved. Cleanup never traverses outside the dedicated lease directory.
    ///
    /// # Errors
    ///
    /// Returns a locking or filesystem error if bounded cleanup cannot finish.
    pub fn cleanup_expired(&self) -> Result<AuthorizedSessionScopeCleanup> {
        self.with_lock(|| self.cleanup_expired_at(SystemTime::now()))
    }

    fn authorize_at(
        &self,
        external_session_locator: &ExternalSessionLocator,
        activation_scope: &ActivationScope,
        catalog: &RepositoryCatalogSnapshot,
        now: SystemTime,
    ) -> Result<AuthorizedSessionScopeAuthorizeOutcome> {
        validate_locator(external_session_locator)?;
        let catalog_revision = catalog.revision()?;
        let (decision, allowed_repository_ids) =
            validated_persisted_decision(activation_scope, catalog)?;
        let issued_at_unix_seconds = unix_seconds(now)?;
        let expires_at = now
            .checked_add(self.policy.ttl)
            .ok_or_else(|| invalid("AuthorizedSessionScope TTL overflows system time"))?;
        let scope = AuthorizedSessionScope {
            version: AuthorizedSessionScopeRecordVersion::V1,
            external_session_locator: external_session_locator.clone(),
            decision,
            allowed_repository_ids,
            catalog_revision,
            issued_at_unix_seconds,
            expires_at_unix_seconds: unix_seconds(expires_at)?,
        };
        scope.validate()?;
        let bytes = self.serialize_scope(&scope)?;

        self.with_lock(|| {
            self.cleanup_expired_at(now)?;
            let path = self.record_path(external_session_locator);
            let existing = self.read_optional_record(&path)?;
            let mut semantic_changed = true;
            if let Some(existing) = &existing {
                verify_record_locator(existing, external_session_locator)?;
                semantic_changed = !existing.semantically_matches(&scope);
            }
            let usage = self.usage(existing.as_ref().map(|_| path.as_path()))?;
            if existing.is_none() && usage.entries >= self.policy.max_entries {
                return Err(invalid(format!(
                    "AuthorizedSessionScope state would exceed {} entries",
                    self.policy.max_entries
                )));
            }
            if usage.bytes.saturating_add(bytes.len() as u64) > self.policy.max_total_bytes {
                return Err(invalid(format!(
                    "AuthorizedSessionScope state would exceed {} bytes",
                    self.policy.max_total_bytes
                )));
            }
            self.replace_record(&path, &bytes, existing.is_some())?;
            Ok(AuthorizedSessionScopeAuthorizeOutcome {
                scope,
                semantic_changed,
            })
        })
    }

    fn read_at(
        &self,
        external_session_locator: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
        now: SystemTime,
    ) -> Result<AuthorizedSessionScopeRead> {
        validate_locator(external_session_locator)?;
        self.with_lock(|| {
            let path = self.record_path(external_session_locator);
            let Some(record) = self.read_optional_record(&path)? else {
                return Ok(AuthorizedSessionScopeRead::Missing);
            };
            verify_record_locator(&record, external_session_locator)?;
            if record.expires_at_unix_seconds <= unix_seconds(now)? {
                fs::remove_file(&path)
                    .map_err(io_error("remove expired AuthorizedSessionScope"))?;
                sync_directory(&self.directory)?;
                return Ok(AuthorizedSessionScopeRead::Expired);
            }
            if !scope_matches_catalog(&record, catalog)? {
                return Ok(AuthorizedSessionScopeRead::StaleCatalog);
            }
            Ok(AuthorizedSessionScopeRead::Current(record))
        })
    }

    fn expire_at(
        &self,
        external_session_locator: &ExternalSessionLocator,
        now: SystemTime,
    ) -> Result<bool> {
        validate_locator(external_session_locator)?;
        self.with_lock(|| {
            let path = self.record_path(external_session_locator);
            let Some(record) = self.read_optional_record(&path)? else {
                return Ok(false);
            };
            verify_record_locator(&record, external_session_locator)?;
            if record.expires_at_unix_seconds > unix_seconds(now)? {
                return Ok(false);
            }
            fs::remove_file(&path).map_err(io_error("expire AuthorizedSessionScope"))?;
            sync_directory(&self.directory)?;
            Ok(true)
        })
    }

    fn cleanup_expired_at(&self, now: SystemTime) -> Result<AuthorizedSessionScopeCleanup> {
        let now = unix_seconds(now)?;
        let mut report = AuthorizedSessionScopeCleanup::default();
        for entry in sorted_entries(&self.directory)? {
            let path = entry.path();
            let entry_key = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(&path)
                .map_err(io_error("inspect AuthorizedSessionScope entry"))?;
            match self.read_record_path(&path) {
                Ok(record) if record.expires_at_unix_seconds <= now => {
                    fs::remove_file(&path)
                        .map_err(io_error("remove expired AuthorizedSessionScope"))?;
                    report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(metadata.len());
                    report.removed_entry_keys.push(entry_key);
                }
                Ok(_) => {}
                Err(_) => report
                    .diagnostics
                    .push(AuthorizedSessionScopeCleanupDiagnostic {
                        entry_key,
                        kind: diagnostic_kind(&metadata, self.policy.max_entry_bytes),
                    }),
            }
        }
        if !report.removed_entry_keys.is_empty() {
            sync_directory(&self.directory)?;
        }
        Ok(report)
    }

    fn read_optional_record(&self, path: &Path) -> Result<Option<AuthorizedSessionScope>> {
        match fs::symlink_metadata(path) {
            Ok(_) => self.read_record_path(path).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("inspect AuthorizedSessionScope record")(error)),
        }
    }

    fn read_record_path(&self, path: &Path) -> Result<AuthorizedSessionScope> {
        let metadata = fs::symlink_metadata(path)
            .map_err(io_error("inspect AuthorizedSessionScope record"))?;
        if !metadata.file_type().is_file()
            || metadata.nlink() != 1
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            return Err(invalid("unsafe AuthorizedSessionScope entry"));
        }
        if metadata.len() > self.policy.max_entry_bytes as u64 {
            return Err(invalid("oversized AuthorizedSessionScope entry"));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid(
                "AuthorizedSessionScope entry permissions are not private",
            ));
        }
        let mut file = File::open(path).map_err(io_error("open AuthorizedSessionScope record"))?;
        let capacity = usize::try_from(metadata.len())
            .map_err(|_| invalid("AuthorizedSessionScope length exceeds platform bounds"))?;
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)
            .map_err(io_error("read AuthorizedSessionScope record"))?;
        let record: AuthorizedSessionScope = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("AuthorizedSessionScope record is invalid"))?;
        record.validate()?;
        Ok(record)
    }

    fn serialize_scope(&self, scope: &AuthorizedSessionScope) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(scope)
            .map_err(|_| invalid("serialize AuthorizedSessionScope record"))?;
        bytes.push(b'\n');
        if bytes.len() > self.policy.max_entry_bytes {
            return Err(invalid(format!(
                "AuthorizedSessionScope record is {} bytes; entry limit is {} bytes",
                bytes.len(),
                self.policy.max_entry_bytes
            )));
        }
        Ok(bytes)
    }

    fn replace_record(&self, path: &Path, bytes: &[u8], replacing: bool) -> Result<()> {
        if replacing {
            validate_private_regular_file(path, "existing AuthorizedSessionScope")?;
        } else {
            reject_symlink(path)?;
        }
        let temporary = self
            .directory
            .join(format!(".scope-{}.tmp", uuid::Uuid::new_v4().hyphenated()));
        let outcome = (|| {
            write_private_new(&temporary, bytes)?;
            fs::rename(&temporary, path)
                .map_err(io_error("atomically replace AuthorizedSessionScope"))?;
            sync_directory(&self.directory)
        })();
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        outcome
    }

    fn record_path(&self, locator: &ExternalSessionLocator) -> PathBuf {
        self.directory
            .join(format!("scope-{}.json", locator_digest(locator)))
    }

    fn usage(&self, excluded: Option<&Path>) -> Result<ScopeUsage> {
        let mut usage = ScopeUsage::default();
        for entry in sorted_entries(&self.directory)? {
            let path = entry.path();
            if excluded == Some(path.as_path()) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(io_error("inspect AuthorizedSessionScope usage"))?;
            usage.entries = usage.entries.saturating_add(1);
            usage.bytes = usage.bytes.saturating_add(metadata.len());
        }
        Ok(usage)
    }

    fn lock(&self) -> Result<File> {
        reject_symlink(&self.lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock_path)
            .map_err(io_error("open authorized-session-scopes.lock"))?;
        let metadata = fs::symlink_metadata(&self.lock_path)
            .map_err(io_error("inspect authorized-session-scopes.lock"))?;
        if !metadata.file_type().is_file() || metadata.nlink() != 1 {
            return Err(invariant(
                "authorized-session-scopes.lock must be a private single-link regular file",
            ));
        }
        fs::set_permissions(&self.lock_path, fs::Permissions::from_mode(0o600))
            .map_err(io_error("set authorized-session-scopes.lock permissions"))?;
        lock.lock_exclusive()
            .map_err(io_error("lock authorized-session-scopes.lock"))?;
        Ok(lock)
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.lock()?;
        let outcome = operation();
        let unlock =
            FileExt::unlock(&lock).map_err(io_error("unlock authorized-session-scopes.lock"));
        outcome.and_then(|value| unlock.map(|()| value))
    }
}

#[derive(Default)]
struct ScopeUsage {
    entries: usize,
    bytes: u64,
}

fn validated_persisted_decision(
    activation_scope: &ActivationScope,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<(AuthorizedSessionScopeDecision, Vec<RepositoryId>)> {
    match &activation_scope.decision {
        ActivationScopeDecision::Disabled if activation_scope.allowed_repository_ids.is_empty() => {
            Ok((AuthorizedSessionScopeDecision::Disabled, Vec::new()))
        }
        ActivationScopeDecision::Direct {
            repository_id,
            checkout_path,
        } if activation_scope.allowed_repository_ids == [*repository_id]
            && catalog.repositories.iter().any(|repository| {
                repository.repository_id == *repository_id
                    && repository.checkout_paths.contains(checkout_path)
            }) =>
        {
            Ok((
                AuthorizedSessionScopeDecision::Direct {
                    repository_id: *repository_id,
                },
                vec![*repository_id],
            ))
        }
        ActivationScopeDecision::Group {
            repository_group_id,
            root_path,
        } => {
            let group = catalog
                .repository_groups
                .iter()
                .find(|group| {
                    group.repository_group_id == *repository_group_id
                        && group.root_path == *root_path
                })
                .ok_or_else(|| invalid("ActivationScope Group is absent from the Catalog"))?;
            if activation_scope.allowed_repository_ids != group.member_repository_ids {
                return Err(invalid(
                    "ActivationScope Group membership disagrees with the Catalog",
                ));
            }
            if !catalog_contains_repositories(catalog, &group.member_repository_ids) {
                return Err(invalid(
                    "ActivationScope Group contains an unconfigured Repository",
                ));
            }
            Ok((
                AuthorizedSessionScopeDecision::Group {
                    repository_group_id: *repository_group_id,
                },
                group.member_repository_ids.clone(),
            ))
        }
        _ => Err(invalid(
            "ActivationScope decision was not produced by the supplied Catalog",
        )),
    }
}

fn scope_matches_catalog(
    scope: &AuthorizedSessionScope,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<bool> {
    if scope.catalog_revision != catalog.revision()? {
        return Ok(false);
    }
    Ok(match scope.decision {
        AuthorizedSessionScopeDecision::Disabled => scope.allowed_repository_ids.is_empty(),
        AuthorizedSessionScopeDecision::Direct { repository_id } => {
            scope.allowed_repository_ids == [repository_id]
                && catalog
                    .repositories
                    .iter()
                    .any(|repository| repository.repository_id == repository_id)
        }
        AuthorizedSessionScopeDecision::Group {
            repository_group_id,
        } => catalog.repository_groups.iter().any(|group| {
            group.repository_group_id == repository_group_id
                && group.member_repository_ids == scope.allowed_repository_ids
                && catalog_contains_repositories(catalog, &group.member_repository_ids)
        }),
    })
}

fn catalog_contains_repositories(
    catalog: &RepositoryCatalogSnapshot,
    repository_ids: &[RepositoryId],
) -> bool {
    repository_ids.iter().all(|repository_id| {
        catalog
            .repositories
            .iter()
            .any(|repository| repository.repository_id == *repository_id)
    })
}

fn verify_record_locator(
    record: &AuthorizedSessionScope,
    expected: &ExternalSessionLocator,
) -> Result<()> {
    if record.external_session_locator != *expected {
        return Err(invariant(
            "AuthorizedSessionScope key and ExternalSessionLocator disagree",
        ));
    }
    Ok(())
}

fn validate_locator(locator: &ExternalSessionLocator) -> Result<()> {
    locator.validate()?;
    validate_locator_size(locator)
}

fn validate_locator_size(locator: &ExternalSessionLocator) -> Result<()> {
    if locator
        .agent_kind
        .len()
        .saturating_add(locator.external_session_id.len())
        > MAX_LOCATOR_BYTES
    {
        return Err(invalid(
            "ExternalSessionLocator exceeds the local lease limit",
        ));
    }
    Ok(())
}

fn locator_digest(locator: &ExternalSessionLocator) -> String {
    let mut hasher = Sha256::new();
    hasher.update((locator.agent_kind.len() as u64).to_be_bytes());
    hasher.update(locator.agent_kind.as_bytes());
    hasher.update((locator.external_session_id.len() as u64).to_be_bytes());
    hasher.update(locator.external_session_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sorted_entries(directory: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(directory)
        .map_err(io_error("read AuthorizedSessionScope directory"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io_error("read AuthorizedSessionScope entry"))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn diagnostic_kind(
    metadata: &fs::Metadata,
    max_entry_bytes: usize,
) -> AuthorizedSessionScopeCleanupDiagnosticKind {
    if metadata.len() > max_entry_bytes as u64 {
        AuthorizedSessionScopeCleanupDiagnosticKind::Oversized
    } else if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        AuthorizedSessionScopeCleanupDiagnosticKind::UnsafeEntry
    } else {
        AuthorizedSessionScopeCleanupDiagnosticKind::InvalidRecord
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(invariant(format!(
                "private state path must be a non-symlink directory: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(io_error("create private directory"))?;
        }
        Err(error) => return Err(io_error("inspect private directory")(error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set private directory permissions"))
}

fn validate_private_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect private state file"))?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(invariant(format!("{label} must be a private regular file")));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invariant(format!(
            "refusing symlinked private state path: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect private state path")(error)),
    }
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error("create AuthorizedSessionScope temporary record"))?;
    file.write_all(bytes)
        .map_err(io_error("write AuthorizedSessionScope temporary record"))?;
    file.sync_all()
        .map_err(io_error("sync AuthorizedSessionScope temporary record"))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("sync AuthorizedSessionScope directory"))
}

fn unix_seconds(time: SystemTime) -> Result<u64> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| invalid("AuthorizedSessionScope timestamp is before the Unix epoch"))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier},
        thread,
        time::{Duration, UNIX_EPOCH},
    };

    use sctx_domain::{ExternalSessionLocator, RepositoryGroupId, RepositoryId};
    use tempfile::tempdir;

    use super::{
        AuthorizedSessionScopeCleanupDiagnosticKind, AuthorizedSessionScopeDecision,
        AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead, AuthorizedSessionScopeStore,
    };
    use crate::{
        ActivationScope, ActivationScopeDecision, RepositoryCatalogEntry,
        RepositoryCatalogSnapshot, RepositoryGroupCatalogEntry,
    };

    fn locator(value: &str) -> ExternalSessionLocator {
        ExternalSessionLocator::new("codex", value).unwrap()
    }

    fn fixture_catalog() -> (RepositoryCatalogSnapshot, RepositoryId, RepositoryGroupId) {
        let repository_id = RepositoryId::new();
        let repository_group_id = RepositoryGroupId::new();
        (
            RepositoryCatalogSnapshot {
                repositories: vec![RepositoryCatalogEntry {
                    repository_id,
                    checkout_paths: vec!["/private/checkouts/member".into()],
                }],
                repository_groups: vec![RepositoryGroupCatalogEntry {
                    repository_group_id,
                    root_path: "/private/checkouts".into(),
                    member_repository_ids: vec![repository_id],
                }],
            },
            repository_id,
            repository_group_id,
        )
    }

    fn direct_scope(repository_id: RepositoryId) -> ActivationScope {
        ActivationScope {
            decision: ActivationScopeDecision::Direct {
                repository_id,
                checkout_path: "/private/checkouts/member".into(),
            },
            allowed_repository_ids: vec![repository_id],
        }
    }

    fn policy(ttl: Duration) -> AuthorizedSessionScopePolicy {
        AuthorizedSessionScopePolicy {
            ttl,
            max_entry_bytes: 4_096,
            max_entries: 16,
            max_total_bytes: 32_768,
        }
    }

    #[test]
    fn records_are_private_minimal_hashed_and_disabled_is_sticky() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let external = locator("../会话/../../not-a-filename");
        let disabled = ActivationScope {
            decision: ActivationScopeDecision::Disabled,
            allowed_repository_ids: Vec::new(),
        };
        let outcome = store.authorize(&external, &disabled, &catalog).unwrap();
        assert!(outcome.semantic_changed);
        assert_eq!(
            outcome.scope.decision,
            AuthorizedSessionScopeDecision::Disabled
        );

        let entries = fs::read_dir(store.directory())
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        let path = entries[0].path();
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(filename.starts_with("scope-"));
        assert_eq!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("json")
        );
        assert_eq!(filename.len(), "scope-".len() + 64 + ".json".len());
        assert!(!filename.contains("会话"));
        assert_eq!(
            fs::metadata(store.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let lock = temporary
            .path()
            .join("state/authorized-session-scopes.lock");
        assert_eq!(
            fs::metadata(lock).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let stored = fs::read_to_string(path).unwrap();
        for forbidden in [
            "prompt",
            "transcript",
            "tool_input",
            "tool_output",
            "report_content",
            "checkout_path",
            "root_path",
            "business_file",
        ] {
            assert!(!stored.contains(forbidden));
        }
        assert!(matches!(
            store.read(&external, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == AuthorizedSessionScopeDecision::Disabled
        ));
    }

    #[test]
    fn semantic_retries_linearize_and_cross_locator_keys_stay_isolated() {
        let temporary = tempdir().unwrap();
        let store = Arc::new(AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap());
        let (catalog, repository_id, _) = fixture_catalog();
        let scope = direct_scope(repository_id);
        let external = locator("same/session");
        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let store = Arc::clone(&store);
            let catalog = catalog.clone();
            let scope = scope.clone();
            let external = external.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.authorize(&external, &scope, &catalog).unwrap()
            }));
        }
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.semantic_changed)
                .count(),
            1
        );
        assert!(outcomes.windows(2).all(|pair| {
            pair[0].scope.decision == pair[1].scope.decision
                && pair[0].scope.allowed_repository_ids == pair[1].scope.allowed_repository_ids
                && pair[0].scope.catalog_revision == pair[1].scope.catalog_revision
        }));

        for hostile in ["same\\session", "same?session", "同一/会话", "same∕session"] {
            let hostile = locator(hostile);
            store.authorize(&hostile, &scope, &catalog).unwrap();
            assert!(matches!(
                store.read(&hostile, &catalog).unwrap(),
                AuthorizedSessionScopeRead::Current(_)
            ));
        }
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 5);
        assert!(matches!(
            store.read(&external, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
    }

    #[test]
    fn expiry_catalog_revision_and_group_membership_fail_closed_without_scanning() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(
            temporary.path(),
            policy(Duration::from_secs(2)),
        )
        .unwrap();
        let (catalog, repository_id, repository_group_id) = fixture_catalog();
        let external = locator("ttl");
        let start = UNIX_EPOCH + Duration::from_secs(100);
        store
            .authorize_at(&external, &direct_scope(repository_id), &catalog, start)
            .unwrap();
        assert!(matches!(
            store
                .read_at(&external, &catalog, start + Duration::from_secs(1))
                .unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
        let retry = store
            .authorize_at(
                &external,
                &direct_scope(repository_id),
                &catalog,
                start + Duration::from_secs(1),
            )
            .unwrap();
        assert!(!retry.semantic_changed);
        assert_eq!(retry.scope.issued_at_unix_seconds, 101);
        assert_eq!(retry.scope.expires_at_unix_seconds, 103);
        assert!(matches!(
            store
                .read_at(&external, &catalog, start + Duration::from_secs(2))
                .unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
        assert!(matches!(
            store
                .read_at(&external, &catalog, start + Duration::from_secs(3))
                .unwrap(),
            AuthorizedSessionScopeRead::Expired
        ));
        assert!(matches!(
            store.read_at(&external, &catalog, start).unwrap(),
            AuthorizedSessionScopeRead::Missing
        ));

        let group_scope = ActivationScope {
            decision: ActivationScopeDecision::Group {
                repository_group_id,
                root_path: "/private/checkouts".into(),
            },
            allowed_repository_ids: vec![repository_id],
        };
        store
            .authorize_at(&external, &group_scope, &catalog, start)
            .unwrap();
        let mut stale = catalog.clone();
        let replacement_repository_id = RepositoryId::new();
        stale.repositories.push(RepositoryCatalogEntry {
            repository_id: replacement_repository_id,
            checkout_paths: vec!["/private/checkouts/replacement".into()],
        });
        stale.repository_groups[0].member_repository_ids = vec![replacement_repository_id];
        assert!(matches!(
            store
                .read_at(&external, &stale, start + Duration::from_secs(1))
                .unwrap(),
            AuthorizedSessionScopeRead::StaleCatalog
        ));

        let path = store.record_path(&external);
        let mut record = store.read_record_path(&path).unwrap();
        record.catalog_revision = stale.revision().unwrap();
        let bytes = store.serialize_scope(&record).unwrap();
        store
            .with_lock(|| store.replace_record(&path, &bytes, true))
            .unwrap();
        assert!(matches!(
            store
                .read_at(&external, &stale, start + Duration::from_secs(1))
                .unwrap(),
            AuthorizedSessionScopeRead::StaleCatalog
        ));
    }

    #[test]
    fn cleanup_is_bounded_and_corrupt_oversized_and_symlink_entries_never_authorize() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(
            temporary.path(),
            policy(Duration::from_secs(2)),
        )
        .unwrap();
        let (catalog, repository_id, _) = fixture_catalog();
        let start = UNIX_EPOCH + Duration::from_secs(100);
        let expired = locator("expired");
        store
            .authorize_at(&expired, &direct_scope(repository_id), &catalog, start)
            .unwrap();

        let corrupt = store.directory().join("scope-corrupt.json");
        fs::write(&corrupt, b"not json").unwrap();
        fs::set_permissions(&corrupt, fs::Permissions::from_mode(0o600)).unwrap();
        let oversized = store.directory().join("scope-oversized.json");
        fs::write(&oversized, vec![b'x'; 5_000]).unwrap();
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, "preserve").unwrap();
        let sibling_sentinels = [
            temporary.path().join("state/capture/sentinel.json"),
            temporary.path().join("state/runtime/sentinel.sqlite"),
            temporary.path().join("repository/context-sentinel.json"),
        ];
        for sentinel in &sibling_sentinels {
            fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            fs::write(sentinel, "preserved").unwrap();
        }
        let unsafe_link = store.directory().join("scope-symlink.json");
        std::os::unix::fs::symlink(&outside, &unsafe_link).unwrap();

        let report = store
            .with_lock(|| store.cleanup_expired_at(start + Duration::from_secs(2)))
            .unwrap();
        assert_eq!(report.removed_entry_keys.len(), 1);
        assert_eq!(report.diagnostics.len(), 3);
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == AuthorizedSessionScopeCleanupDiagnosticKind::InvalidRecord
        }));
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == AuthorizedSessionScopeCleanupDiagnosticKind::Oversized
        }));
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == AuthorizedSessionScopeCleanupDiagnosticKind::UnsafeEntry
        }));
        assert_eq!(fs::read_to_string(outside).unwrap(), "preserve");
        assert!(
            sibling_sentinels
                .iter()
                .all(|sentinel| fs::read_to_string(sentinel).unwrap() == "preserved")
        );
        assert!(unsafe_link.exists());
    }

    #[test]
    fn exact_corrupt_oversized_and_symlink_records_fail_closed_and_release_the_lock() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(
            temporary.path(),
            policy(Duration::from_secs(60)),
        )
        .unwrap();
        let (catalog, repository_id, _) = fixture_catalog();
        let scope = direct_scope(repository_id);

        let corrupt = locator("corrupt/exact");
        store.authorize(&corrupt, &scope, &catalog).unwrap();
        let corrupt_path = store.record_path(&corrupt);
        fs::write(&corrupt_path, b"not json").unwrap();
        fs::set_permissions(&corrupt_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.read(&corrupt, &catalog).is_err());
        assert!(matches!(
            store
                .read(&locator("missing-after-error"), &catalog)
                .unwrap(),
            AuthorizedSessionScopeRead::Missing
        ));

        let oversized = locator("oversized/exact");
        store.authorize(&oversized, &scope, &catalog).unwrap();
        let oversized_path = store.record_path(&oversized);
        fs::write(&oversized_path, vec![b'x'; 5_000]).unwrap();
        fs::set_permissions(&oversized_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.read(&oversized, &catalog).is_err());

        let unsafe_locator = locator("symlink/exact");
        store.authorize(&unsafe_locator, &scope, &catalog).unwrap();
        let unsafe_path = store.record_path(&unsafe_locator);
        fs::remove_file(&unsafe_path).unwrap();
        let outside = temporary.path().join("outside-exact");
        fs::write(&outside, "preserve").unwrap();
        std::os::unix::fs::symlink(&outside, &unsafe_path).unwrap();
        assert!(store.read(&unsafe_locator, &catalog).is_err());
        assert!(store.remove(&unsafe_locator).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "preserve");
    }

    #[test]
    fn catalog_revision_is_order_independent_and_covers_every_authority_field() {
        let (mut catalog, _, _) = fixture_catalog();
        let second_repository_id = RepositoryId::new();
        catalog.repositories.push(RepositoryCatalogEntry {
            repository_id: second_repository_id,
            checkout_paths: vec!["/private/checkouts/second-b".into()],
        });
        catalog.repositories[0]
            .checkout_paths
            .push("/private/checkouts/member-b".into());
        catalog.repository_groups[0]
            .member_repository_ids
            .push(second_repository_id);
        catalog.repository_groups[0].member_repository_ids.sort();
        let expected = catalog.revision().unwrap();
        assert!(expected.as_str().starts_with("sha256:"));

        let mut reordered = catalog.clone();
        reordered.repositories.reverse();
        for repository in &mut reordered.repositories {
            repository.checkout_paths.reverse();
        }
        reordered.repository_groups[0]
            .member_repository_ids
            .reverse();
        assert_eq!(reordered.revision().unwrap(), expected);

        let mut changed_checkout = catalog.clone();
        changed_checkout.repositories[0]
            .checkout_paths
            .push("/private/checkouts/other".into());
        assert_ne!(changed_checkout.revision().unwrap(), expected);

        let mut changed_root = catalog.clone();
        changed_root.repository_groups[0].root_path = "/private/another-group".into();
        assert_ne!(changed_root.revision().unwrap(), expected);
    }

    #[test]
    fn entry_count_bytes_remove_and_invalid_scope_boundaries_are_enforced() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(
            temporary.path(),
            AuthorizedSessionScopePolicy {
                ttl: Duration::from_secs(60),
                max_entry_bytes: 1_024,
                max_entries: 1,
                max_total_bytes: 1_024,
            },
        )
        .unwrap();
        let (catalog, repository_id, _) = fixture_catalog();
        let scope = direct_scope(repository_id);
        let first = locator("first");
        store.authorize(&first, &scope, &catalog).unwrap();
        assert!(
            store
                .authorize(&locator("second"), &scope, &catalog)
                .is_err()
        );
        assert!(!store.expire(&first).unwrap());
        assert!(store.remove(&first).unwrap());
        assert!(!store.remove(&first).unwrap());

        let fabricated = ActivationScope {
            decision: ActivationScopeDecision::Direct {
                repository_id,
                checkout_path: "/not/the/catalog/path".into(),
            },
            allowed_repository_ids: vec![repository_id],
        };
        assert!(store.authorize(&first, &fabricated, &catalog).is_err());
        assert!(
            AuthorizedSessionScopeStore::with_policy(
                temporary.path().join("bad"),
                AuthorizedSessionScopePolicy {
                    ttl: Duration::from_secs(24 * 60 * 60 + 1),
                    ..AuthorizedSessionScopePolicy::default()
                }
            )
            .is_err()
        );

        let sizing_root = temporary.path().join("sizing");
        let sizing_store = AuthorizedSessionScopeStore::with_policy(
            &sizing_root,
            AuthorizedSessionScopePolicy {
                ttl: Duration::from_secs(60),
                max_entry_bytes: 4_096,
                max_entries: 16,
                max_total_bytes: 32_768,
            },
        )
        .unwrap();
        let sample = sizing_store
            .authorize(&locator("byte-a"), &scope, &catalog)
            .unwrap();
        let record_bytes = sizing_store.serialize_scope(&sample.scope).unwrap().len();
        let byte_store = AuthorizedSessionScopeStore::with_policy(
            temporary.path().join("byte-limit"),
            AuthorizedSessionScopePolicy {
                ttl: Duration::from_secs(60),
                max_entry_bytes: record_bytes + 1,
                max_entries: 16,
                max_total_bytes: (record_bytes as u64).saturating_mul(2).saturating_sub(1),
            },
        )
        .unwrap();
        byte_store
            .authorize(&locator("byte-a"), &scope, &catalog)
            .unwrap();
        assert!(
            byte_store
                .authorize(&locator("byte-b"), &scope, &catalog)
                .is_err()
        );
    }
}
