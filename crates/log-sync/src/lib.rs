//! Independent, bounded Git synchronization for sealed log batches.
//!
//! This crate intentionally does not depend on the knowledge Git store, runtime database,
//! installer, or their locks. The sealed spool and upload journal are the recovery source;
//! `repository/` is only a disposable cache.

pub mod runner;

mod sync;

pub use sync::{
    PruneReport, SealStatus, SyncError, SyncErrorCode, SyncErrorStage, SyncOptions, SyncOutcome,
    SyncReport, bounded_tool_diagnostic, prune_cache, sync, sync_scheduled,
};
