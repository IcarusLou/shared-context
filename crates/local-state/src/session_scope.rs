use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use sctx_domain::{Error, ErrorKind, ExternalSessionLocator, RepositoryId, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ActivationScope, RepositoryCatalogSnapshot};

const MAX_LOCATOR_BYTES: usize = 4 * 1024;

/// Ceiling on the Repository identities one derived activation may name. It matches the
/// Catalog's own identity ceiling: a common-parent Session can legitimately name every
/// registered Repository, and nothing beyond the Catalog can enter this record.
const MAX_SCOPE_REPOSITORIES: usize = 256;

/// Age after which an activation lease is treated as an orphan of a Session that
/// never delivered `SessionEnd` (Codex desktop notably does not) and is reclaimed
/// by `sctx doctor --fix` and by `upgrade`.
pub const ORPHAN_LEASE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Typed on-disk schema version for one private activation lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizedSessionScopeRecordVersion {
    V1,
}

/// Minimal persisted authorization decision: Repository identity only, never a path.
///
/// `Enabled` carries the same unique, sorted identities the Catalog derived for this
/// Session's startup directory — one when it started inside a checkout, several when it
/// started at a common parent of several checkouts. A record written by a superseded
/// schema (an explicit Group decision, or a separate allowed-Repository list) does not
/// deserialize and is therefore reported as `Missing`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorizedSessionScopeDecision {
    Disabled,
    Enabled { repository_ids: Vec<RepositoryId> },
}

impl AuthorizedSessionScopeDecision {
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

/// One `ExternalSessionLocator`-owned local activation lease, permanently bound
/// to the Agent Session that created it.
///
/// The record never expires: a long-running Session must not lose its
/// authorization halfway through. It is still disposable authorization state —
/// not a `TaskSession`, a Workspace route, a Context fact, or durable
/// engineering knowledge — and it is reclaimed by age, by `SessionEnd`, or by
/// capacity pressure.
///
/// `startup_cwd` is the canonical directory the Session started in. It is the
/// only durable input of the decision, so the decision can be re-derived against
/// a later Catalog without asking the Agent again. It is backend-only local
/// metadata and must never reach a prompt, an MCP response, or durable Context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedSessionScope {
    pub version: AuthorizedSessionScopeRecordVersion,
    pub external_session_locator: ExternalSessionLocator,
    pub decision: AuthorizedSessionScopeDecision,
    pub startup_cwd: PathBuf,
    pub issued_at_unix_seconds: u64,
    /// Delivery-only marker for the one-shot Intent bootstrap reminder. It does not participate
    /// in authorization, Catalog matching, re-resolution, or reclamation semantics.
    #[serde(default, skip_serializing_if = "is_false")]
    pub intent_bootstrap_notified: bool,
    /// Delivery-only marker for an activation marker this lease re-stated after a `SessionStart`
    /// failed open without one. Like `intent_bootstrap_notified` it is bookkeeping, not
    /// authorization: it never participates in the decision, Catalog matching, re-resolution, or
    /// reclamation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub activation_marker_delivered: bool,
}

impl AuthorizedSessionScope {
    fn validate(&self) -> Result<()> {
        self.external_session_locator.validate()?;
        validate_locator_size(&self.external_session_locator)?;
        validate_startup_cwd(&self.startup_cwd)?;
        let AuthorizedSessionScopeDecision::Enabled { repository_ids } = &self.decision else {
            return Ok(());
        };
        if repository_ids.is_empty() {
            return Err(invalid(
                "an Enabled AuthorizedSessionScope names no Repository",
            ));
        }
        if repository_ids.len() > MAX_SCOPE_REPOSITORIES {
            return Err(invalid(
                "AuthorizedSessionScope contains too many Repository identities",
            ));
        }
        let unique = repository_ids.iter().cloned().collect::<BTreeSet<_>>();
        if unique.len() != repository_ids.len()
            || !repository_ids.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(invalid(
                "AuthorizedSessionScope Repository identities must be unique and sorted",
            ));
        }
        Ok(())
    }
}

/// Aggregate ceilings for private activation leases. Leases have no TTL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionScopePolicy {
    pub max_entry_bytes: usize,
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for AuthorizedSessionScopePolicy {
    fn default() -> Self {
        Self {
            max_entry_bytes: 16 * 1024,
            max_entries: 4_096,
            max_total_bytes: 8 * 1024 * 1024,
        }
    }
}

impl AuthorizedSessionScopePolicy {
    fn validate(self) -> Result<()> {
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

/// Result of one serialized first-authorization operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionScopeAuthorizeOutcome {
    pub scope: AuthorizedSessionScope,
    /// False when a concurrent caller had already persisted this locator's decision.
    pub created: bool,
    /// Entry keys of the least recently issued leases evicted to make room.
    pub evicted_entry_keys: Vec<String>,
}

/// Fail-closed interpretation of one locator's private lease.
///
/// A lease is permanent, so there is no expiry or stale-Catalog state. A record
/// that cannot be interpreted — corrupt, oversized, written by an older schema,
/// or keyed for another locator — is reported as `Missing` and the next
/// `SessionStart` overwrites it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorizedSessionScopeRead {
    Missing,
    Current(AuthorizedSessionScope),
}

/// Safe diagnosis category for an entry reclamation refused to interpret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizedSessionScopeCleanupDiagnosticKind {
    InvalidRecord,
    UnsafeEntry,
    Oversized,
}

/// One deterministic reclamation diagnosis. The entry key is a SHA-256 filename,
/// never raw external Session text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionScopeCleanupDiagnostic {
    pub entry_key: String,
    pub kind: AuthorizedSessionScopeCleanupDiagnosticKind,
}

/// Result of bounded orphan reclamation under `state/authorized-session-scopes` only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizedSessionScopeReclaim {
    /// Interpretable leases whose `issued_at` predates the reclamation threshold.
    pub removed_entry_keys: Vec<String>,
    /// Regular private entries that can never authorize again (corrupt, oversized,
    /// or written by a superseded lease schema).
    pub removed_unreadable_entry_keys: Vec<String>,
    pub reclaimed_bytes: u64,
    pub retained_entries: usize,
    pub diagnostics: Vec<AuthorizedSessionScopeCleanupDiagnostic>,
}

/// Read-only count of what orphan reclamation would remove.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizedSessionScopeSurvey {
    pub total_entries: usize,
    pub stale_entries: usize,
    pub unreadable_entries: usize,
    pub reclaimable_bytes: u64,
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

    /// Non-blockingly persists the first authorization for one locator.
    ///
    /// The decision is derived here from `canonical_startup_cwd` and the supplied
    /// Catalog, so a caller can never persist a decision the Catalog does not
    /// support. If a concurrent caller already persisted an interpretable
    /// decision, that first decision is returned unchanged: the first successful
    /// `SessionStart` owns the Session's `startup_cwd` and no later
    /// `SessionStart` cwd rewrites it. A busy lock is an immediate typed error;
    /// no delayed operation remains after return.
    ///
    /// When the store is at its entry or byte ceiling, the least recently issued
    /// leases are evicted to make room and reported in the outcome, so a machine
    /// that accumulated orphans never locks out a live Session.
    ///
    /// # Errors
    ///
    /// Returns immediately for lock contention, and rejects an unresolvable
    /// startup directory or unsafe existing state without changing it.
    pub fn try_authorize_missing(
        &self,
        external_session_locator: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
        canonical_startup_cwd: &Path,
    ) -> Result<AuthorizedSessionScopeAuthorizeOutcome> {
        self.try_authorize_missing_at(
            external_session_locator,
            catalog,
            canonical_startup_cwd,
            SystemTime::now(),
        )
    }

    /// Reads one lease exactly as it is stored, waiting for the Store lock.
    ///
    /// This performs no Catalog re-resolution; use [`Self::try_read_reconciled`]
    /// on an authorization path.
    ///
    /// # Errors
    ///
    /// Returns typed locking or filesystem failures.
    pub fn read(
        &self,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<AuthorizedSessionScopeRead> {
        validate_locator(external_session_locator)?;
        self.with_lock(|| self.classify_locked(external_session_locator))
    }

    /// Classifies one lease without waiting for another scope operation.
    ///
    /// This shared-lock path is strictly non-mutating and performs no Catalog
    /// re-resolution. Concurrent Hook readers never exclude one another.
    ///
    /// # Errors
    ///
    /// A busy lock is an immediate typed failure and never becomes authorization.
    pub fn try_read(
        &self,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<AuthorizedSessionScopeRead> {
        validate_locator(external_session_locator)?;
        self.with_try_shared_lock(|| self.classify_locked(external_session_locator))
    }

    /// Reads one lease and re-derives its decision against the current Catalog.
    ///
    /// The lease stores the Session's canonical `startup_cwd`, so `repository add`
    /// and `repository remove` take effect on the very next call of an already
    /// running Session without asking the Agent again. Re-resolution is pure: no
    /// filesystem stat, no Git, no Repository scan.
    ///
    /// The freshly resolved decision is authoritative for this call. The record is
    /// only rewritten when the decision actually changed, and only under a
    /// non-blocking exclusive lock — a busy lock leaves the stored record alone and
    /// still returns the current decision.
    ///
    /// # Errors
    ///
    /// Returns immediately for a busy shared lock, and propagates a Catalog that
    /// cannot resolve the recorded startup directory so the caller fails closed.
    pub fn try_read_reconciled(
        &self,
        external_session_locator: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
    ) -> Result<AuthorizedSessionScopeRead> {
        let AuthorizedSessionScopeRead::Current(record) =
            self.try_read(external_session_locator)?
        else {
            return Ok(AuthorizedSessionScopeRead::Missing);
        };
        let resolved = catalog.resolve_recorded_activation_scope(&record.startup_cwd)?;
        let decision = validated_persisted_decision(&resolved, catalog)?;
        if decision == record.decision {
            return Ok(AuthorizedSessionScopeRead::Current(record));
        }
        let mut reconciled = record;
        reconciled.decision = decision;
        reconciled.validate()?;
        let _persisted = self.try_persist_reconciled(external_session_locator, &reconciled);
        Ok(AuthorizedSessionScopeRead::Current(reconciled))
    }

    /// Unconditionally removes one exact locator's record for `SessionEnd`.
    ///
    /// # Errors
    ///
    /// Refuses an unsafe record or cross-locator digest mismatch.
    pub fn remove(&self, external_session_locator: &ExternalSessionLocator) -> Result<bool> {
        validate_locator(external_session_locator)?;
        self.with_lock(|| self.remove_exact_unlocked(external_session_locator))
    }

    /// Non-blockingly removes one exact locator's record for a Hook `SessionEnd`.
    ///
    /// A busy lock or an unsafe record is an immediate error. Callers on the Hook path
    /// deliberately ignore that error so Agent shutdown remains fail-open without touching another
    /// locator or waiting for concurrent scope work; the leftover lease is then an orphan that
    /// [`Self::reclaim_stale_leases`] removes.
    ///
    /// # Errors
    ///
    /// Returns immediately for lock contention and refuses unsafe records or locator mismatches.
    pub fn try_remove(&self, external_session_locator: &ExternalSessionLocator) -> Result<bool> {
        validate_locator(external_session_locator)?;
        self.with_try_lock(|| self.remove_exact_unlocked(external_session_locator))
    }

    /// Non-blockingly records delivery of the one-shot Intent bootstrap reminder.
    ///
    /// The exact current authorization must already exist. This changes only
    /// `intent_bootstrap_notified`; it preserves the decision, Repository identities,
    /// startup directory, and issue time. `true` means this caller recorded the first
    /// delivery; `false` means it was already recorded or the current decision is Disabled.
    ///
    /// # Errors
    ///
    /// Returns immediately for lock contention and rejects missing, unsafe, or
    /// invalid scope state without changing it.
    pub fn try_mark_intent_bootstrap_notified(
        &self,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<bool> {
        self.try_mark_delivery(external_session_locator, |record| {
            &mut record.intent_bootstrap_notified
        })
    }

    /// Non-blockingly records delivery of the one-shot re-stated activation marker.
    ///
    /// This is the marker a lease self-heal owes a Session whose `SessionStart` failed open
    /// without one. Semantics match [`Self::try_mark_intent_bootstrap_notified`] exactly:
    /// `true` means this caller recorded the first delivery; `false` means it was already
    /// recorded or the current decision is Disabled.
    ///
    /// # Errors
    ///
    /// Returns immediately for lock contention and rejects missing, unsafe, or invalid scope
    /// state without changing it.
    pub fn try_mark_activation_marker_delivered(
        &self,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<bool> {
        self.try_mark_delivery(external_session_locator, |record| {
            &mut record.activation_marker_delivered
        })
    }

    /// Flips one delivery-only flag of an existing Enabled lease, preserving everything else.
    fn try_mark_delivery(
        &self,
        external_session_locator: &ExternalSessionLocator,
        select: fn(&mut AuthorizedSessionScope) -> &mut bool,
    ) -> Result<bool> {
        validate_locator(external_session_locator)?;
        self.with_try_lock(|| {
            let path = self.record_path(external_session_locator);
            let mut record = self
                .read_optional_record(&path)?
                .ok_or_else(|| invalid("AuthorizedSessionScope is missing"))?;
            verify_record_locator(&record, external_session_locator)?;
            if !record.decision.is_enabled() || *select(&mut record) {
                return Ok(false);
            }
            *select(&mut record) = true;
            record.validate()?;
            let bytes = self.serialize_scope(&record)?;
            let usage = self.usage(Some(path.as_path()))?;
            self.validate_new_usage(&usage, false, bytes.len())?;
            self.replace_record(&path, &bytes, true)?;
            Ok(true)
        })
    }

    /// Counts, without changing anything, what orphan reclamation would remove.
    ///
    /// # Errors
    ///
    /// Returns a locking or filesystem error.
    pub fn survey_stale_leases(&self, max_age: Duration) -> Result<AuthorizedSessionScopeSurvey> {
        self.with_lock(|| self.survey_stale_leases_at(max_age, SystemTime::now()))
    }

    /// Removes leases issued longer than `max_age` ago plus entries that can never
    /// authorize again, in deterministic filename order.
    ///
    /// Agents that never deliver `SessionEnd` — Codex desktop among them — leave
    /// their lease behind forever, so this is the only path that bounds the record
    /// directory over a machine's lifetime. Symlink and non-regular entries are
    /// diagnosed and preserved; reclamation never traverses outside the dedicated
    /// lease directory.
    ///
    /// # Errors
    ///
    /// Returns a locking or filesystem error if bounded reclamation cannot finish.
    pub fn reclaim_stale_leases(&self, max_age: Duration) -> Result<AuthorizedSessionScopeReclaim> {
        self.with_lock(|| self.reclaim_stale_leases_at(max_age, SystemTime::now()))
    }

    fn remove_exact_unlocked(
        &self,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<bool> {
        let path = self.record_path(external_session_locator);
        let Some(record) = self.read_optional_record(&path)? else {
            return Ok(false);
        };
        verify_record_locator(&record, external_session_locator)?;
        fs::remove_file(&path).map_err(io_error("remove AuthorizedSessionScope"))?;
        sync_directory(&self.directory)?;
        Ok(true)
    }

    fn try_authorize_missing_at(
        &self,
        external_session_locator: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
        canonical_startup_cwd: &Path,
        now: SystemTime,
    ) -> Result<AuthorizedSessionScopeAuthorizeOutcome> {
        let scope = Self::scope_for_authorization(
            external_session_locator,
            catalog,
            canonical_startup_cwd,
            now,
        )?;
        let bytes = self.serialize_scope(&scope)?;

        self.with_try_lock(|| {
            let path = self.record_path(external_session_locator);
            if let Some(existing) = self.classified_record(&path, external_session_locator)? {
                return Ok(AuthorizedSessionScopeAuthorizeOutcome {
                    scope: existing,
                    created: false,
                    evicted_entry_keys: Vec::new(),
                });
            }
            let evicted_entry_keys = self.evict_for_capacity(&path, bytes.len(), now)?;
            self.replace_record(&path, &bytes, false)?;
            Ok(AuthorizedSessionScopeAuthorizeOutcome {
                scope,
                created: true,
                evicted_entry_keys,
            })
        })
    }

    fn try_persist_reconciled(
        &self,
        external_session_locator: &ExternalSessionLocator,
        reconciled: &AuthorizedSessionScope,
    ) -> Result<()> {
        let bytes = self.serialize_scope(reconciled)?;
        self.with_try_lock(|| {
            let path = self.record_path(external_session_locator);
            let Some(existing) = self.classified_record(&path, external_session_locator)? else {
                return Ok(());
            };
            if existing.startup_cwd != reconciled.startup_cwd
                || existing.issued_at_unix_seconds != reconciled.issued_at_unix_seconds
            {
                return Ok(());
            }
            let usage = self.usage(Some(path.as_path()))?;
            self.validate_new_usage(&usage, false, bytes.len())?;
            self.replace_record(&path, &bytes, true)
        })
    }

    fn scope_for_authorization(
        external_session_locator: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
        canonical_startup_cwd: &Path,
        now: SystemTime,
    ) -> Result<AuthorizedSessionScope> {
        validate_locator(external_session_locator)?;
        let activation_scope = catalog.resolve_recorded_activation_scope(canonical_startup_cwd)?;
        let decision = validated_persisted_decision(&activation_scope, catalog)?;
        let scope = AuthorizedSessionScope {
            version: AuthorizedSessionScopeRecordVersion::V1,
            external_session_locator: external_session_locator.clone(),
            decision,
            startup_cwd: canonical_startup_cwd.to_path_buf(),
            issued_at_unix_seconds: unix_seconds(now)?,
            intent_bootstrap_notified: false,
            activation_marker_delivered: false,
        };
        scope.validate()?;
        Ok(scope)
    }

    fn classify_locked(
        &self,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<AuthorizedSessionScopeRead> {
        let path = self.record_path(external_session_locator);
        Ok(
            match self.classified_record(&path, external_session_locator)? {
                Some(record) => AuthorizedSessionScopeRead::Current(record),
                None => AuthorizedSessionScopeRead::Missing,
            },
        )
    }

    /// Interprets one record path, treating every uninterpretable entry as absent.
    ///
    /// A corrupt, oversized, world-readable, superseded-schema, or cross-locator
    /// record can never authorize, so it is reported as absent and the next
    /// `SessionStart` replaces it. A symlink or other non-regular entry is also
    /// absent here, and `replace_record` still refuses to write through it.
    fn classified_record(
        &self,
        path: &Path,
        external_session_locator: &ExternalSessionLocator,
    ) -> Result<Option<AuthorizedSessionScope>> {
        match self.read_optional_record(path) {
            Ok(Some(record)) if record.external_session_locator == *external_session_locator => {
                Ok(Some(record))
            }
            Err(error) if error.kind() == ErrorKind::Io => Err(error),
            Ok(_) | Err(_) => Ok(None),
        }
    }

    fn survey_stale_leases_at(
        &self,
        max_age: Duration,
        now: SystemTime,
    ) -> Result<AuthorizedSessionScopeSurvey> {
        let mut survey = AuthorizedSessionScopeSurvey::default();
        for entry in self.classify_entries(max_age, now)? {
            survey.total_entries = survey.total_entries.saturating_add(1);
            match entry.disposition {
                EntryDisposition::Stale => {
                    survey.stale_entries = survey.stale_entries.saturating_add(1);
                    survey.reclaimable_bytes = survey.reclaimable_bytes.saturating_add(entry.bytes);
                }
                EntryDisposition::Unreadable(kind) => {
                    survey.unreadable_entries = survey.unreadable_entries.saturating_add(1);
                    survey.reclaimable_bytes = survey.reclaimable_bytes.saturating_add(entry.bytes);
                    survey
                        .diagnostics
                        .push(AuthorizedSessionScopeCleanupDiagnostic {
                            entry_key: entry.entry_key,
                            kind,
                        });
                }
                EntryDisposition::Unsafe => {
                    survey
                        .diagnostics
                        .push(AuthorizedSessionScopeCleanupDiagnostic {
                            entry_key: entry.entry_key,
                            kind: AuthorizedSessionScopeCleanupDiagnosticKind::UnsafeEntry,
                        });
                }
                EntryDisposition::Live => {}
            }
        }
        Ok(survey)
    }

    fn reclaim_stale_leases_at(
        &self,
        max_age: Duration,
        now: SystemTime,
    ) -> Result<AuthorizedSessionScopeReclaim> {
        let mut report = AuthorizedSessionScopeReclaim::default();
        for entry in self.classify_entries(max_age, now)? {
            match entry.disposition {
                EntryDisposition::Stale => {
                    fs::remove_file(&entry.path)
                        .map_err(io_error("remove stale AuthorizedSessionScope"))?;
                    report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(entry.bytes);
                    report.removed_entry_keys.push(entry.entry_key);
                }
                EntryDisposition::Unreadable(_) => {
                    fs::remove_file(&entry.path)
                        .map_err(io_error("remove unreadable AuthorizedSessionScope"))?;
                    report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(entry.bytes);
                    report.removed_unreadable_entry_keys.push(entry.entry_key);
                }
                EntryDisposition::Unsafe => {
                    report
                        .diagnostics
                        .push(AuthorizedSessionScopeCleanupDiagnostic {
                            entry_key: entry.entry_key,
                            kind: AuthorizedSessionScopeCleanupDiagnosticKind::UnsafeEntry,
                        });
                    report.retained_entries = report.retained_entries.saturating_add(1);
                }
                EntryDisposition::Live => {
                    report.retained_entries = report.retained_entries.saturating_add(1);
                }
            }
        }
        if !report.removed_entry_keys.is_empty() || !report.removed_unreadable_entry_keys.is_empty()
        {
            sync_directory(&self.directory)?;
        }
        Ok(report)
    }

    fn classify_entries(&self, max_age: Duration, now: SystemTime) -> Result<Vec<ClassifiedEntry>> {
        let now = unix_seconds(now)?;
        let threshold = now.saturating_sub(max_age.as_secs());
        let mut classified = Vec::new();
        for entry in sorted_entries(&self.directory)? {
            let path = entry.path();
            let entry_key = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(&path)
                .map_err(io_error("inspect AuthorizedSessionScope entry"))?;
            let disposition = if !metadata.file_type().is_file() || metadata.nlink() != 1 {
                EntryDisposition::Unsafe
            } else {
                match self.read_record_path(&path) {
                    Ok(record) if record.issued_at_unix_seconds <= threshold => {
                        EntryDisposition::Stale
                    }
                    Ok(_) => EntryDisposition::Live,
                    Err(error) if error.kind() == ErrorKind::Io => return Err(error),
                    Err(_) => EntryDisposition::Unreadable(diagnostic_kind(
                        &metadata,
                        self.policy.max_entry_bytes,
                    )),
                }
            };
            classified.push(ClassifiedEntry {
                entry_key,
                path,
                bytes: metadata.len(),
                disposition,
            });
        }
        Ok(classified)
    }

    /// Frees entry and byte headroom for one new lease by evicting the least
    /// recently issued records, oldest first.
    ///
    /// Uninterpretable entries are evicted before any live lease: they can never
    /// authorize a Session again. This replaces the previous fail-closed ceiling,
    /// which would have refused a live Session's very first authorization on a
    /// machine that had accumulated orphaned leases.
    fn evict_for_capacity(
        &self,
        target: &Path,
        record_bytes: usize,
        now: SystemTime,
    ) -> Result<Vec<String>> {
        let mut evicted = Vec::new();
        let mut order: Option<Vec<EvictionCandidate>> = None;
        loop {
            let usage = self.usage(Some(target))?;
            if self.validate_new_usage(&usage, true, record_bytes).is_ok() {
                break;
            }
            if order.is_none() {
                order = Some(self.eviction_candidates(target, now)?);
            }
            let Some(candidate) = order.as_mut().and_then(Vec::pop) else {
                self.validate_new_usage(&usage, true, record_bytes)?;
                break;
            };
            match fs::remove_file(&candidate.path) {
                Ok(()) => evicted.push(candidate.entry_key),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error("evict AuthorizedSessionScope")(error));
                }
            }
        }
        if !evicted.is_empty() {
            sync_directory(&self.directory)?;
        }
        Ok(evicted)
    }

    /// Builds the eviction order with the *most* evictable record last, so
    /// `Vec::pop` yields uninterpretable entries first and then the oldest leases.
    fn eviction_candidates(
        &self,
        target: &Path,
        now: SystemTime,
    ) -> Result<Vec<EvictionCandidate>> {
        let now = unix_seconds(now)?;
        let mut candidates = Vec::new();
        for entry in sorted_entries(&self.directory)? {
            let path = entry.path();
            if path == target {
                continue;
            }
            let entry_key = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(&path)
                .map_err(io_error("inspect AuthorizedSessionScope entry"))?;
            if !metadata.file_type().is_file() || metadata.nlink() != 1 {
                continue;
            }
            let issued_at = match self.read_record_path(&path) {
                Ok(record) => Some(record.issued_at_unix_seconds.min(now)),
                Err(error) if error.kind() == ErrorKind::Io => return Err(error),
                Err(_) => None,
            };
            candidates.push(EvictionCandidate {
                entry_key,
                path,
                issued_at,
            });
        }
        // Newest first, then uninterpretable entries last: `pop` evicts them first.
        candidates.sort_by(|left, right| {
            right
                .issued_at
                .cmp(&left.issued_at)
                .then_with(|| left.entry_key.cmp(&right.entry_key))
        });
        Ok(candidates)
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

    fn validate_new_usage(
        &self,
        usage: &ScopeUsage,
        adding_entry: bool,
        record_bytes: usize,
    ) -> Result<()> {
        if adding_entry && usage.entries >= self.policy.max_entries {
            return Err(invalid(format!(
                "AuthorizedSessionScope state would exceed {} entries",
                self.policy.max_entries
            )));
        }
        if usage.bytes.saturating_add(record_bytes as u64) > self.policy.max_total_bytes {
            return Err(invalid(format!(
                "AuthorizedSessionScope state would exceed {} bytes",
                self.policy.max_total_bytes
            )));
        }
        Ok(())
    }

    fn lock(&self) -> Result<File> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()
            .map_err(io_error("lock authorized-session-scopes.lock"))?;
        Ok(lock)
    }

    fn try_lock(&self) -> Result<File> {
        let lock = self.open_lock()?;
        FileExt::try_lock_exclusive(&lock)
            .map_err(io_error("try lock authorized-session-scopes.lock"))?;
        Ok(lock)
    }

    fn try_shared_lock(&self) -> Result<File> {
        let lock = self.open_lock()?;
        FileExt::try_lock_shared(&lock)
            .map_err(io_error("try lock authorized-session-scopes.lock shared"))?;
        Ok(lock)
    }

    fn open_lock(&self) -> Result<File> {
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
        Ok(lock)
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.lock()?;
        let outcome = operation();
        let unlock =
            FileExt::unlock(&lock).map_err(io_error("unlock authorized-session-scopes.lock"));
        outcome.and_then(|value| unlock.map(|()| value))
    }

    fn with_try_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.try_lock()?;
        let outcome = operation();
        let unlock =
            FileExt::unlock(&lock).map_err(io_error("unlock authorized-session-scopes.lock"));
        outcome.and_then(|value| unlock.map(|()| value))
    }

    fn with_try_shared_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.try_shared_lock()?;
        let outcome = operation();
        let unlock = FileExt::unlock(&lock)
            .map_err(io_error("unlock authorized-session-scopes.lock shared"));
        outcome.and_then(|value| unlock.map(|()| value))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryDisposition {
    Live,
    Stale,
    Unreadable(AuthorizedSessionScopeCleanupDiagnosticKind),
    Unsafe,
}

#[derive(Clone, Debug)]
struct ClassifiedEntry {
    entry_key: String,
    path: PathBuf,
    bytes: u64,
    disposition: EntryDisposition,
}

#[derive(Clone, Debug)]
struct EvictionCandidate {
    entry_key: String,
    path: PathBuf,
    /// `None` for an entry that can never authorize again; it sorts last so it is
    /// evicted before any interpretable lease.
    issued_at: Option<u64>,
}

#[derive(Default)]
struct ScopeUsage {
    entries: usize,
    bytes: u64,
}

/// Converts one freshly derived `ActivationScope` into the persisted decision.
///
/// Derivation already reads the same Catalog, so this is a closing invariant check
/// rather than a second policy: every named identity must still be configured, and
/// the record refuses to hold an identity the Catalog cannot account for.
fn validated_persisted_decision(
    activation_scope: &ActivationScope,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<AuthorizedSessionScopeDecision> {
    let ActivationScope::Enabled { repository_ids } = activation_scope else {
        return Ok(AuthorizedSessionScopeDecision::Disabled);
    };
    if !catalog_contains_repositories(catalog, repository_ids) {
        return Err(invalid(
            "ActivationScope names a Repository the Catalog does not configure",
        ));
    }
    Ok(AuthorizedSessionScopeDecision::Enabled {
        repository_ids: repository_ids.clone(),
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
            .any(|repository| &repository.repository_id == repository_id)
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

fn validate_startup_cwd(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
        || path.to_str().is_none()
    {
        return Err(invalid(
            "AuthorizedSessionScope startup directory must be an absolute UTF-8 path without dot segments",
        ));
    }
    if path.as_os_str().len() > MAX_LOCATOR_BYTES {
        return Err(invalid(
            "AuthorizedSessionScope startup directory exceeds the local lease limit",
        ));
    }
    Ok(())
}

pub(crate) fn locator_digest(locator: &ExternalSessionLocator) -> String {
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

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip predicate receives `&T`.
const fn is_false(value: &bool) -> bool {
    !*value
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
        fs::{self, OpenOptions},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant, UNIX_EPOCH},
    };

    use fs2::FileExt;
    use sctx_domain::{ExternalSessionLocator, RepositoryId};
    use tempfile::tempdir;

    use super::{
        AuthorizedSessionScopeCleanupDiagnosticKind, AuthorizedSessionScopeDecision,
        AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead, AuthorizedSessionScopeStore,
    };
    use crate::{RepositoryCatalogEntry, RepositoryCatalogSnapshot};

    /// The common parent two registered checkouts share; starting here derives both.
    const PARENT_ROOT: &str = "/private/checkouts";
    const MEMBER_CHECKOUT: &str = "/private/checkouts/member";
    const SIBLING_CHECKOUT: &str = "/private/checkouts/sibling";
    const OUTSIDE: &str = "/private/unregistered";

    fn locator(value: &str) -> ExternalSessionLocator {
        ExternalSessionLocator::new("codex", value).unwrap()
    }

    fn enabled(repository_ids: &[&RepositoryId]) -> AuthorizedSessionScopeDecision {
        let mut repository_ids = repository_ids
            .iter()
            .map(|id| (*id).clone())
            .collect::<Vec<_>>();
        repository_ids.sort();
        AuthorizedSessionScopeDecision::Enabled { repository_ids }
    }

    fn fixture_catalog() -> (RepositoryCatalogSnapshot, RepositoryId, RepositoryId) {
        let member = RepositoryId::new();
        let sibling = RepositoryId::new();
        (
            RepositoryCatalogSnapshot {
                repositories: vec![
                    RepositoryCatalogEntry {
                        repository_id: member.clone(),
                        checkout_paths: vec![MEMBER_CHECKOUT.into()],
                    },
                    RepositoryCatalogEntry {
                        repository_id: sibling.clone(),
                        checkout_paths: vec![SIBLING_CHECKOUT.into()],
                    },
                ],
                activation: crate::ActivationSettings::default(),
            },
            member,
            sibling,
        )
    }

    fn policy() -> AuthorizedSessionScopePolicy {
        AuthorizedSessionScopePolicy {
            max_entry_bytes: 4_096,
            max_entries: 16,
            max_total_bytes: 32_768,
        }
    }

    fn authorize(
        store: &AuthorizedSessionScopeStore,
        external: &ExternalSessionLocator,
        catalog: &RepositoryCatalogSnapshot,
        cwd: &str,
    ) -> super::AuthorizedSessionScope {
        store
            .try_authorize_missing(external, catalog, Path::new(cwd))
            .unwrap()
            .scope
    }

    #[test]
    fn records_are_private_minimal_hashed_and_bound_to_the_startup_directory() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let external = locator("../会话/../../not-a-filename");
        let outcome = store
            .try_authorize_missing(&external, &catalog, Path::new(OUTSIDE))
            .unwrap();
        assert!(outcome.created);
        assert!(outcome.evicted_entry_keys.is_empty());
        assert!(!outcome.scope.intent_bootstrap_notified);
        assert_eq!(
            outcome.scope.decision,
            AuthorizedSessionScopeDecision::Disabled
        );
        assert_eq!(outcome.scope.startup_cwd, PathBuf::from(OUTSIDE));

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
        assert!(!stored.contains("intent_bootstrap_notified"));
        assert!(!stored.contains("expires_at"));
        assert!(!stored.contains("catalog_revision"));
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
            store.read(&external).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == AuthorizedSessionScopeDecision::Disabled
        ));
    }

    #[test]
    fn leases_never_expire_and_a_superseded_schema_reads_as_missing() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, member, _) = fixture_catalog();
        let external = locator("long-running");
        let ancient = UNIX_EPOCH + Duration::from_secs(1_000);
        let authorized = store
            .try_authorize_missing_at(&external, &catalog, Path::new(MEMBER_CHECKOUT), ancient)
            .unwrap()
            .scope;
        assert_eq!(authorized.issued_at_unix_seconds, 1_000);
        assert!(matches!(
            store.try_read(&external).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == enabled(&[&member])
        ));
        assert!(matches!(
            store.try_read_reconciled(&external, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));

        let legacy = locator("legacy-ttl-record");
        let path = store.record_path(&legacy);
        fs::write(
            &path,
            serde_json::json!({
                "version": "v1",
                "external_session_locator": {
                    "agent_kind": "codex",
                    "external_session_id": "legacy-ttl-record",
                },
                "decision": {"kind": "direct", "repository_id": member.to_string()},
                "allowed_repository_ids": [member.to_string()],
                "catalog_revision": catalog.revision().unwrap().as_str(),
                "issued_at_unix_seconds": 1_000,
                "expires_at_unix_seconds": 8_200,
            })
            .to_string(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.try_read(&legacy).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        assert_eq!(
            store.try_read_reconciled(&legacy, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        let replaced = authorize(&store, &legacy, &catalog, MEMBER_CHECKOUT);
        assert!(replaced.issued_at_unix_seconds > 1_000);
        assert!(!fs::read_to_string(&path).unwrap().contains("expires_at"));
    }

    #[test]
    fn reconciliation_follows_the_catalog_and_only_rewrites_on_change() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, member, _) = fixture_catalog();
        let external = locator("follows-catalog");
        let disabled_catalog = RepositoryCatalogSnapshot::default();
        let authorized = authorize(&store, &external, &disabled_catalog, MEMBER_CHECKOUT);
        assert_eq!(
            authorized.decision,
            AuthorizedSessionScopeDecision::Disabled
        );
        let record_path = store.record_path(&external);
        let before = fs::read_to_string(&record_path).unwrap();

        assert!(matches!(
            store.try_read_reconciled(&external, &disabled_catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == AuthorizedSessionScopeDecision::Disabled
        ));
        assert_eq!(fs::read_to_string(&record_path).unwrap(), before);

        assert!(matches!(
            store.try_read_reconciled(&external, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == enabled(&[&member])
                    && scope.issued_at_unix_seconds == authorized.issued_at_unix_seconds
        ));
        let after = fs::read_to_string(&record_path).unwrap();
        assert_ne!(after, before);
        assert!(matches!(
            store.try_read(&external).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == enabled(&[&member])
        ));

        assert!(matches!(
            store.try_read_reconciled(&external, &disabled_catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == AuthorizedSessionScopeDecision::Disabled
        ));
        assert!(matches!(
            store.try_read(&external).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision == AuthorizedSessionScopeDecision::Disabled
        ));
    }

    #[test]
    fn intent_bootstrap_notification_is_one_shot_and_outside_authorization_semantics() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let external = locator("intent-bootstrap");
        let authorized = authorize(&store, &external, &catalog, MEMBER_CHECKOUT);
        assert!(!authorized.intent_bootstrap_notified);

        assert!(store.try_mark_intent_bootstrap_notified(&external).unwrap());
        let notified = match store.try_read(&external).unwrap() {
            AuthorizedSessionScopeRead::Current(scope) => scope,
            other @ AuthorizedSessionScopeRead::Missing => {
                panic!("expected current scope, got {other:?}")
            }
        };
        assert!(notified.intent_bootstrap_notified);
        assert_eq!(notified.decision, authorized.decision);
        assert_eq!(notified.startup_cwd, authorized.startup_cwd);
        assert_eq!(
            notified.issued_at_unix_seconds,
            authorized.issued_at_unix_seconds
        );
        assert!(!store.try_mark_intent_bootstrap_notified(&external).unwrap());

        let retry = store
            .try_authorize_missing(&external, &catalog, Path::new(MEMBER_CHECKOUT))
            .unwrap();
        assert!(!retry.created);
        assert!(retry.scope.intent_bootstrap_notified);

        let disabled = locator("intent-bootstrap-disabled");
        authorize(&store, &disabled, &catalog, OUTSIDE);
        assert!(!store.try_mark_intent_bootstrap_notified(&disabled).unwrap());
        assert!(matches!(
            store.try_read(&disabled).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if !scope.intent_bootstrap_notified
                    && scope.decision == AuthorizedSessionScopeDecision::Disabled
        ));
    }

    /// The re-stated activation marker a lease self-heal owes is delivery bookkeeping, exactly
    /// like the Intent bootstrap notice: one shot, never on a Disabled lease, and independent of
    /// the other flag so neither can consume the other.
    #[test]
    fn activation_marker_delivery_is_one_shot_and_independent_of_the_bootstrap_notice() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let external = locator("marker-delivery");
        let authorized = authorize(&store, &external, &catalog, MEMBER_CHECKOUT);
        assert!(!authorized.activation_marker_delivered);

        assert!(
            store
                .try_mark_activation_marker_delivered(&external)
                .unwrap()
        );
        assert!(
            !store
                .try_mark_activation_marker_delivered(&external)
                .unwrap()
        );
        // Delivering the marker leaves the bootstrap notice still owed, and vice versa.
        assert!(store.try_mark_intent_bootstrap_notified(&external).unwrap());
        let scope = match store.try_read(&external).unwrap() {
            AuthorizedSessionScopeRead::Current(scope) => scope,
            other @ AuthorizedSessionScopeRead::Missing => {
                panic!("expected current scope, got {other:?}")
            }
        };
        assert!(scope.activation_marker_delivered && scope.intent_bootstrap_notified);
        assert_eq!(scope.decision, authorized.decision);
        assert_eq!(scope.startup_cwd, authorized.startup_cwd);
        assert_eq!(
            scope.issued_at_unix_seconds,
            authorized.issued_at_unix_seconds
        );

        let disabled = locator("marker-delivery-disabled");
        authorize(&store, &disabled, &catalog, OUTSIDE);
        assert!(
            !store
                .try_mark_activation_marker_delivered(&disabled)
                .unwrap()
        );
    }

    #[test]
    fn concurrent_first_authorizations_linearize_and_cross_locator_keys_stay_isolated() {
        let temporary = tempdir().unwrap();
        let store = Arc::new(AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap());
        let (catalog, _, _) = fixture_catalog();
        let external = locator("same/session");
        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let store = Arc::clone(&store);
            let catalog = catalog.clone();
            let external = external.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                loop {
                    if let Ok(outcome) =
                        store.try_authorize_missing(&external, &catalog, Path::new(MEMBER_CHECKOUT))
                    {
                        return outcome;
                    }
                }
            }));
        }
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);
        assert!(
            outcomes
                .windows(2)
                .all(|pair| pair[0].scope == pair[1].scope)
        );

        for hostile in ["same\\session", "same?session", "同一/会话", "same∕session"] {
            let hostile = locator(hostile);
            authorize(&store, &hostile, &catalog, MEMBER_CHECKOUT);
            assert!(matches!(
                store.read(&hostile).unwrap(),
                AuthorizedSessionScopeRead::Current(_)
            ));
        }
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 5);
    }

    #[test]
    fn try_lock_operations_return_immediately_without_a_late_record() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let external = locator("held-lock");
        let notify_external = locator("held-notification-lock");
        authorize(&store, &notify_external, &catalog, MEMBER_CHECKOUT);
        assert_eq!(
            store.try_read(&external).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&store.lock_path)
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();

        let started = Instant::now();
        assert!(store.try_read(&external).is_err());
        assert!(
            store
                .try_authorize_missing(&external, &catalog, Path::new(MEMBER_CHECKOUT))
                .is_err()
        );
        assert!(
            store
                .try_mark_intent_bootstrap_notified(&notify_external)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        FileExt::unlock(&lock).unwrap();
        thread::sleep(Duration::from_millis(50));

        assert_eq!(
            store.try_read(&external).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        assert!(matches!(
            store.try_read(&notify_external).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if !scope.intent_bootstrap_notified
        ));
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 1);
    }

    #[test]
    fn try_remove_is_exact_nonblocking_and_never_completes_after_return() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let remove = locator("remove-exact");
        let preserve = locator("preserve-exact");
        for external in [&remove, &preserve] {
            authorize(&store, external, &catalog, MEMBER_CHECKOUT);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&store.lock_path)
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let started = Instant::now();
        assert!(store.try_remove(&remove).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        FileExt::unlock(&lock).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            store.read(&remove).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
        assert!(store.try_remove(&remove).unwrap());
        assert!(!store.try_remove(&remove).unwrap());
        assert!(matches!(
            store.read(&preserve).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
    }

    #[test]
    fn the_first_persisted_startup_directory_owns_the_session() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, member, _) = fixture_catalog();
        let external = locator("sticky-first");
        let direct = authorize(&store, &external, &catalog, MEMBER_CHECKOUT);
        assert_eq!(direct.decision, enabled(&[&member]));

        let retained = store
            .try_authorize_missing(&external, &catalog, Path::new(PARENT_ROOT))
            .unwrap();
        assert!(!retained.created);
        assert_eq!(retained.scope, direct);
        assert!(matches!(
            store.try_read(&external).unwrap(),
            AuthorizedSessionScopeRead::Current(scope) if scope == direct
        ));
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 1);
    }

    #[test]
    fn shared_try_reads_coexist_for_same_and_different_locators() {
        let temporary = tempdir().unwrap();
        let store = Arc::new(AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap());
        let (catalog, _, _) = fixture_catalog();
        let same = locator("shared-same");
        let different = locator("shared-different");
        for external in [&same, &different] {
            authorize(&store, external, &catalog, MEMBER_CHECKOUT);
        }

        let first_shared = store.try_shared_lock().unwrap();
        let second_shared = store.try_shared_lock().unwrap();
        FileExt::unlock(&second_shared).unwrap();
        FileExt::unlock(&first_shared).unwrap();

        let workers = 12;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::new();
        for index in 0..workers {
            let store = Arc::clone(&store);
            let external = if index % 2 == 0 {
                same.clone()
            } else {
                different.clone()
            };
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.try_read(&external)
            }));
        }
        for handle in handles {
            assert!(matches!(
                handle.join().unwrap().unwrap(),
                AuthorizedSessionScopeRead::Current(_)
            ));
        }
    }

    #[test]
    fn reclamation_is_bounded_and_never_leaves_the_lease_directory() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(temporary.path(), policy()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let now = UNIX_EPOCH + Duration::from_secs(60 * 24 * 60 * 60);
        let max_age = Duration::from_secs(30 * 24 * 60 * 60);
        let orphan = locator("orphan");
        let live = locator("live");
        store
            .try_authorize_missing_at(
                &orphan,
                &catalog,
                Path::new(MEMBER_CHECKOUT),
                now - Duration::from_secs(31 * 24 * 60 * 60),
            )
            .unwrap();
        store
            .try_authorize_missing_at(
                &live,
                &catalog,
                Path::new(MEMBER_CHECKOUT),
                now - Duration::from_secs(29 * 24 * 60 * 60),
            )
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
            temporary.path().join("state/diagnostics/sentinel.json"),
            temporary.path().join("state/runtime/sentinel.sqlite"),
            temporary.path().join("repository/context-sentinel.json"),
        ];
        for sentinel in &sibling_sentinels {
            fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            fs::write(sentinel, "preserved").unwrap();
        }
        let unsafe_link = store.directory().join("scope-symlink.json");
        std::os::unix::fs::symlink(&outside, &unsafe_link).unwrap();

        let survey = store
            .with_lock(|| store.survey_stale_leases_at(max_age, now))
            .unwrap();
        assert_eq!(survey.total_entries, 5);
        assert_eq!(survey.stale_entries, 1);
        assert_eq!(survey.unreadable_entries, 2);

        let report = store
            .with_lock(|| store.reclaim_stale_leases_at(max_age, now))
            .unwrap();
        assert_eq!(report.removed_entry_keys.len(), 1);
        assert_eq!(report.removed_unreadable_entry_keys.len(), 2);
        assert_eq!(report.retained_entries, 2);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].kind,
            AuthorizedSessionScopeCleanupDiagnosticKind::UnsafeEntry
        );
        assert_eq!(
            store.try_read(&orphan).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        assert!(matches!(
            store.try_read(&live).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
        assert_eq!(fs::read_to_string(outside).unwrap(), "preserve");
        assert!(
            sibling_sentinels
                .iter()
                .all(|sentinel| fs::read_to_string(sentinel).unwrap() == "preserved")
        );
        assert!(unsafe_link.exists());
    }

    #[test]
    fn uninterpretable_exact_records_read_as_missing_and_symlinks_are_never_written_through() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(temporary.path(), policy()).unwrap();
        let (catalog, _, _) = fixture_catalog();

        let corrupt = locator("corrupt/exact");
        authorize(&store, &corrupt, &catalog, MEMBER_CHECKOUT);
        let corrupt_path = store.record_path(&corrupt);
        fs::write(&corrupt_path, b"not json").unwrap();
        fs::set_permissions(&corrupt_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.read(&corrupt).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        assert!(matches!(
            authorize(&store, &corrupt, &catalog, MEMBER_CHECKOUT).decision,
            AuthorizedSessionScopeDecision::Enabled { .. }
        ));

        let oversized = locator("oversized/exact");
        authorize(&store, &oversized, &catalog, MEMBER_CHECKOUT);
        let oversized_path = store.record_path(&oversized);
        fs::write(&oversized_path, vec![b'x'; 5_000]).unwrap();
        fs::set_permissions(&oversized_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.read(&oversized).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );

        let unsafe_locator = locator("symlink/exact");
        let unsafe_path = store.record_path(&unsafe_locator);
        let outside = temporary.path().join("outside-exact");
        fs::write(&outside, "preserve").unwrap();
        std::os::unix::fs::symlink(&outside, &unsafe_path).unwrap();
        assert_eq!(
            store.read(&unsafe_locator).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        assert!(
            store
                .try_authorize_missing(&unsafe_locator, &catalog, Path::new(MEMBER_CHECKOUT))
                .is_err()
        );
        assert!(store.remove(&unsafe_locator).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "preserve");
    }

    #[test]
    fn capacity_pressure_evicts_the_least_recently_issued_lease_instead_of_failing_closed() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::with_policy(
            temporary.path(),
            AuthorizedSessionScopePolicy {
                max_entry_bytes: 1_024,
                max_entries: 2,
                max_total_bytes: 8 * 1_024,
            },
        )
        .unwrap();
        let (catalog, _, _) = fixture_catalog();
        let start = UNIX_EPOCH + Duration::from_secs(1_000);
        let oldest = locator("oldest");
        let middle = locator("middle");
        let newest = locator("newest");
        store
            .try_authorize_missing_at(&oldest, &catalog, Path::new(MEMBER_CHECKOUT), start)
            .unwrap();
        store
            .try_authorize_missing_at(
                &middle,
                &catalog,
                Path::new(MEMBER_CHECKOUT),
                start + Duration::from_secs(10),
            )
            .unwrap();
        let evicting = store
            .try_authorize_missing_at(
                &newest,
                &catalog,
                Path::new(MEMBER_CHECKOUT),
                start + Duration::from_secs(20),
            )
            .unwrap();
        assert!(evicting.created);
        assert_eq!(evicting.evicted_entry_keys.len(), 1);
        assert_eq!(
            store.try_read(&oldest).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );
        assert!(matches!(
            store.try_read(&middle).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
        assert!(matches!(
            store.try_read(&newest).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));

        let record_bytes = fs::metadata(store.record_path(&newest)).unwrap().len();
        let byte_store = AuthorizedSessionScopeStore::with_policy(
            temporary.path().join("byte-limit"),
            AuthorizedSessionScopePolicy {
                max_entry_bytes: usize::try_from(record_bytes).unwrap() + 1,
                max_entries: 64,
                max_total_bytes: record_bytes.saturating_mul(2).saturating_sub(1),
            },
        )
        .unwrap();
        byte_store
            .try_authorize_missing_at(&oldest, &catalog, Path::new(MEMBER_CHECKOUT), start)
            .unwrap();
        let byte_evicting = byte_store
            .try_authorize_missing_at(
                &newest,
                &catalog,
                Path::new(MEMBER_CHECKOUT),
                start + Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(byte_evicting.evicted_entry_keys.len(), 1);
        assert_eq!(
            byte_store.try_read(&oldest).unwrap(),
            AuthorizedSessionScopeRead::Missing
        );

        assert!(
            AuthorizedSessionScopeStore::with_policy(
                temporary.path().join("bad"),
                AuthorizedSessionScopePolicy {
                    max_entry_bytes: 0,
                    ..AuthorizedSessionScopePolicy::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn activation_is_derived_from_registered_checkouts_and_their_parents() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, member, sibling) = fixture_catalog();
        let outside = authorize(&store, &locator("outside"), &catalog, OUTSIDE);
        assert_eq!(outside.decision, AuthorizedSessionScopeDecision::Disabled);
        assert!(outside.decision.repository_ids().is_empty());

        let nested = authorize(
            &store,
            &locator("nested"),
            &catalog,
            "/private/checkouts/member/app/src",
        );
        assert_eq!(nested.decision, enabled(&[&member]));

        // The common parent of two registered checkouts enables exactly those two.
        let parent = authorize(&store, &locator("parent"), &catalog, PARENT_ROOT);
        assert_eq!(parent.decision, enabled(&[&member, &sibling]));

        // So does a higher ancestor, as long as it is not one of the guarded ones.
        let ancestor = authorize(&store, &locator("ancestor"), &catalog, "/private");
        assert_eq!(ancestor.decision, enabled(&[&member, &sibling]));

        // The filesystem root never derives activation, however many checkouts it holds.
        let root = authorize(&store, &locator("root"), &catalog, "/");
        assert_eq!(root.decision, AuthorizedSessionScopeDecision::Disabled);

        assert!(
            store
                .try_authorize_missing(
                    &locator("relative"),
                    &catalog,
                    Path::new("relative/startup")
                )
                .is_err()
        );
        assert!(
            store
                .try_authorize_missing(
                    &locator("dotted"),
                    &catalog,
                    Path::new("/private/checkouts/../checkouts/member")
                )
                .is_err()
        );
    }

    #[test]
    fn reconciliation_leaves_the_record_alone_when_the_exclusive_lock_is_busy() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let (catalog, _, _) = fixture_catalog();
        let disabled_catalog = RepositoryCatalogSnapshot::default();
        let external = locator("busy-writeback");
        authorize(&store, &external, &disabled_catalog, MEMBER_CHECKOUT);
        let record_path = store.record_path(&external);
        let before = fs::read_to_string(&record_path).unwrap();

        let holder = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let lock = holder.try_lock().unwrap();
        let started = Instant::now();
        assert!(matches!(
            store.try_read_reconciled(&external, &catalog),
            Err(_) | Ok(AuthorizedSessionScopeRead::Current(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        FileExt::unlock(&lock).unwrap();
        assert_eq!(fs::read_to_string(&record_path).unwrap(), before);

        assert!(matches!(
            store.try_read_reconciled(&external, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision.is_enabled()
        ));
        assert_ne!(fs::read_to_string(&record_path).unwrap(), before);
    }

    #[test]
    fn survey_and_reclamation_agree_on_an_empty_store() {
        let temporary = tempdir().unwrap();
        let store = AuthorizedSessionScopeStore::initialize(temporary.path()).unwrap();
        let survey = store.survey_stale_leases(Duration::from_secs(1)).unwrap();
        assert_eq!(survey, super::AuthorizedSessionScopeSurvey::default());
        let reclaim = store.reclaim_stale_leases(Duration::from_secs(1)).unwrap();
        assert_eq!(reclaim, super::AuthorizedSessionScopeReclaim::default());
    }
}
