//! Local user configuration, Session authorization, maintenance coordination,
//! and the shared Secret/PII scanner. The Repository Catalog is authoritative
//! for local Repository identity, but none of this crate is durable knowledge.
//!
//! This crate deliberately has no event-schema, reducer, Git, `SQLite`, or Agent
//! adapter dependency. `config.toml` must be preserved to retain its
//! authoritative local Repository IDs.

mod config;
mod maintenance;
mod policy;
mod privacy;
mod session_scope;

pub use config::{
    ActivationScope, ActivationSettings, CatalogCheckoutStatus, ContextTtlPolicy,
    EngineeringSettings, HookSettings, LEGACY_REPOSITORY_ID_PREFIX, MaintenanceSettings,
    PolicySettings, RepositoryCatalogAddOutcome, RepositoryCatalogCheckoutCheck,
    RepositoryCatalogDiagnostic, RepositoryCatalogDoctorReport, RepositoryCatalogEntry,
    RepositoryCatalogRenameOutcome, RepositoryCatalogRevision, RepositoryCatalogSnapshot,
    ResolvedRepositoryPath, RetrievalSettings, UserConfigStore, migrate_legacy_repository_groups,
};
pub use maintenance::{MaintenanceGuard, MaintenanceLock};
pub use policy::{
    CHECKPOINT_SECTION_MAX_BYTES, DEFAULT_POLICY_MARKDOWN, POLICY_FILE_NAME, POLICY_SECTION_NAMES,
    Policy, PolicyStatus, ResolvedPolicy, SESSION_SECTION_MAX_BYTES, STOP_SECTION_MAX_BYTES,
    TRIAGE_SECTION_MAX_BYTES, installation_policy, resolve_policy,
};
pub use privacy::{PrivacyFinding, PrivacyFindingKind, PrivacyScan, PrivacyScanner, RedactedText};
pub use sctx_domain::{Error, ErrorKind, Result};
pub use session_scope::{
    AuthorizedSessionScope, AuthorizedSessionScopeAuthorizeOutcome,
    AuthorizedSessionScopeCleanupDiagnostic, AuthorizedSessionScopeCleanupDiagnosticKind,
    AuthorizedSessionScopeDecision, AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeReclaim, AuthorizedSessionScopeRecordVersion,
    AuthorizedSessionScopeStore, AuthorizedSessionScopeSurvey, ORPHAN_LEASE_MAX_AGE,
};
