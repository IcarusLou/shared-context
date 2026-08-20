//! Non-authoritative user configuration, short-lived capture breadcrumbs, and
//! the shared Secret/PII scanner.
//!
//! This crate deliberately has no event-schema, reducer, Git, `SQLite`, or Agent
//! adapter dependency. Local state can therefore be deleted without changing
//! domain identity or projection results.

mod capture;
mod config;
mod privacy;

pub use capture::{
    Breadcrumb, BreadcrumbKind, CapturePolicy, CaptureReceipt, CaptureStore, CleanupReport,
};
pub use config::UserConfigStore;
pub use privacy::{PrivacyFinding, PrivacyFindingKind, PrivacyScan, PrivacyScanner, RedactedText};
pub use sctx_domain::{Error, ErrorKind, Result};
