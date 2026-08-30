//! Local user configuration, Session authorization, maintenance coordination,
//! and the shared Secret/PII scanner. The Repository Catalog is authoritative
//! for local Repository identity, but none of this crate is durable knowledge.
//!
//! This crate deliberately has no event-schema, reducer, Git, `SQLite`, or Agent
//! adapter dependency. `config.toml` must be preserved to retain its
//! authoritative local Repository IDs.

mod artifact_reminder;
mod config;
mod maintenance;
mod privacy;
mod session_scope;

pub use artifact_reminder::{
    ARTIFACT_REMINDER_MAX_ENTRIES, ArtifactReminderKey, ArtifactReminderMark, ArtifactReminderStore,
};
pub use config::{
    ActivationScope, ActivationScopeDecision, CatalogCheckoutStatus, CatalogRepositoryGroupStatus,
    ContextTtlPolicy, HookSettings, LEGACY_REPOSITORY_ID_PREFIX, RepositoryCatalogAddOutcome,
    RepositoryCatalogCheckoutCheck, RepositoryCatalogDiagnostic, RepositoryCatalogDoctorReport,
    RepositoryCatalogEntry, RepositoryCatalogGroupCheck, RepositoryCatalogInspection,
    RepositoryCatalogRenameOutcome, RepositoryCatalogRevision, RepositoryCatalogSnapshot,
    RepositoryGroupCatalogAddOutcome, RepositoryGroupCatalogEntry,
    RepositoryGroupCatalogRemoveOutcome, RepositoryGroupCatalogUpdateOutcome,
    ResolvedRepositoryPath, UserConfigStore,
};
pub use maintenance::{MaintenanceGuard, MaintenanceLock};
pub use privacy::{PrivacyFinding, PrivacyFindingKind, PrivacyScan, PrivacyScanner, RedactedText};
pub use sctx_domain::{Error, ErrorKind, Result};
pub use session_scope::{
    AuthorizedSessionScope, AuthorizedSessionScopeAuthorizeOutcome, AuthorizedSessionScopeCleanup,
    AuthorizedSessionScopeCleanupDiagnostic, AuthorizedSessionScopeCleanupDiagnosticKind,
    AuthorizedSessionScopeDecision, AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeRecordVersion, AuthorizedSessionScopeStore,
};
