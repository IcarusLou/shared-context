//! Independent, bounded Git synchronization for sealed log batches.
//!
//! This crate intentionally does not depend on the knowledge Git store, runtime database,
//! installer, or their locks. The sealed spool and upload journal are the recovery source;
//! `repository/` is only a disposable cache.

pub mod runner;

mod sync;

pub use sync::{
    PruneReport, SealStatus, SyncError, SyncErrorCode, SyncOptions, SyncOutcome, SyncReport,
    prune_cache, sync,
};
