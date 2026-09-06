#![forbid(unsafe_code)]

//! Independent telemetry configuration, collection, durable spool, diagnostics,
//! and deterministic offline analysis.

mod analysis;
mod collector;
mod config;
mod control;
mod diagnostics;
mod error;
mod fs;
mod spool;
mod status;

pub use analysis::{ErrorCount, Report, ReportRow, Trace, report, trace};
pub use collector::{Collector, CollectorOptions};
pub use config::{
    Config, InitOptions, InitReport, StorageConfig, StreamTarget, SyncConfig, disable, enable,
    encode_email_path, init, load_config,
};
pub use control::{SealRequestOutcome, request_seal};
pub use diagnostics::{
    HookDiagnostics, HookReasonCount, load_hook_diagnostics, record_hook_decision,
};
pub use error::{Error, ErrorCode, Result};
pub use fs::{atomic_write_json, layout, read_bounded_file};
pub use spool::{BatchManifest, ReadyBatch, StreamBinding, list_ready_batches, load_ready_batch};
pub use status::{DoctorReport, ProbeStatus, StatusReport, doctor_probe, status};
