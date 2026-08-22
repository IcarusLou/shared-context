//! Local user configuration, short-lived capture breadcrumbs, and the shared
//! Secret/PII scanner. The Repository Catalog is authoritative for local
//! Repository identity, but none of this crate is durable knowledge.
//!
//! This crate deliberately has no event-schema, reducer, Git, `SQLite`, or Agent
//! adapter dependency. Capture state remains disposable; `config.toml` must be
//! preserved to retain its authoritative local Repository IDs.

mod capture;
mod config;
mod privacy;

pub use capture::{
    Breadcrumb, BreadcrumbKind, CaptureArtifactMapping, CaptureClaim, CaptureClaimOutcome,
    CaptureDiagnosticKind, CaptureListReport, CapturePolicy, CaptureRead, CaptureReceipt,
    CaptureRecord, CaptureRecordVersion, CaptureStore, CaptureStoreDiagnostic,
    CaptureStoreDiagnosticKind, CaptureTaskOwner, CleanupReport, map_capture_artifacts,
};
pub use config::{
    CatalogCheckoutStatus, RepositoryCatalogAddOutcome, RepositoryCatalogCheckoutCheck,
    RepositoryCatalogDoctorReport, RepositoryCatalogEntry, RepositoryCatalogSnapshot,
    ResolvedRepositoryPath, UserConfigStore,
};
pub use privacy::{PrivacyFinding, PrivacyFindingKind, PrivacyScan, PrivacyScanner, RedactedText};
pub use sctx_domain::{Error, ErrorKind, Result};
