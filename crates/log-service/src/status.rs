use std::{fs, os::unix::fs::FileTypeExt, path::Path};

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, Result, fs::read_bounded, layout, list_ready_batches, load_config};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Ok,
    Missing,
    Invalid,
    Permission,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct StatusReport {
    pub configured: bool,
    pub config_error: Option<ErrorCode>,
    pub enabled: Option<bool>,
    pub endpoint: ProbeStatus,
    pub collector_accepted: u64,
    pub collector_persisted: u64,
    pub observable_dropped: u64,
    pub drop_observation_complete: bool,
    pub invalid_frames: u64,
    pub collector_status_error: Option<ErrorCode>,
    pub last_heartbeat_unix_ms: Option<i64>,
    pub ready_batches: u64,
    pub ready_bytes: u64,
    pub oldest_batch_unix_ms: Option<i64>,
    pub spool_observed: bool,
    pub spool_error: Option<ErrorCode>,
    pub storage_pressure: bool,
    pub last_upload_unix_ms: Option<i64>,
    pub last_upload_attempt_unix_ms: Option<i64>,
    pub collector_error: Option<ErrorCode>,
    pub upload_error: Option<String>,
    pub upload_status_error: Option<ErrorCode>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub logs_root: ProbeStatus,
    pub config: ProbeStatus,
    pub runtime_directory: ProbeStatus,
    pub fifo: ProbeStatus,
    pub spool: ProbeStatus,
    pub diagnostics: ProbeStatus,
    pub endpoint_path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectorStatus {
    pub schema_version: u32,
    pub accepted: u64,
    pub persisted: u64,
    pub observable_dropped: u64,
    pub invalid_frames: u64,
    pub last_heartbeat_unix_ms: Option<i64>,
    pub storage_pressure: bool,
    pub last_error: Option<ErrorCode>,
}

/// Reads the local collector, spool, and upload status without mutating them.
///
/// # Errors
/// Returns an error only when top-level status inspection itself cannot proceed.
pub fn status(root: &Path) -> Result<StatusReport> {
    let paths = layout(root);
    let (configured, config_error, enabled) = match load_config(root) {
        Ok(config) => (true, None, Some(config.enabled)),
        Err(error) => (false, Some(error.code()), None),
    };
    let (collector, collector_status_error): (CollectorStatus, Option<ErrorCode>) =
        match read_bounded(&paths.collector_status, 64 * 1024, ErrorCode::InvalidInput) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => (value, None),
                Err(_) => (CollectorStatus::default(), Some(ErrorCode::InvalidInput)),
            },
            Err(_)
                if fs::symlink_metadata(&paths.collector_status)
                    .is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound) =>
            {
                (CollectorStatus::default(), None)
            }
            Err(error) => (CollectorStatus::default(), Some(error.code())),
        };
    let (batches, spool_error) = match list_ready_batches(root) {
        Ok(batches) => (batches, None),
        Err(error) => (Vec::new(), Some(error.code())),
    };
    let ready_bytes = batches
        .iter()
        .map(|batch| batch.manifest.content_bytes)
        .sum();
    let oldest_batch_unix_ms = batches
        .iter()
        .map(|batch| batch.manifest.first_event_unix_ms)
        .min();
    let upload_path = paths.state.join("upload-status.json");
    let (upload, upload_status_error) = match read_upload_status(&upload_path) {
        Ok(upload) => (upload, None),
        Err(_)
            if fs::symlink_metadata(&upload_path)
                .is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound) =>
        {
            (None, None)
        }
        Err(error) => (None, Some(error.code())),
    };
    let upload_error = upload
        .as_ref()
        .and_then(|value| value.get("last_error"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(StatusReport {
        configured,
        config_error,
        enabled,
        endpoint: probe_fifo(&paths.endpoint),
        collector_accepted: collector.accepted,
        collector_persisted: collector.persisted,
        observable_dropped: collector.observable_dropped,
        drop_observation_complete: false,
        invalid_frames: collector.invalid_frames,
        collector_status_error,
        last_heartbeat_unix_ms: collector.last_heartbeat_unix_ms,
        ready_batches: u64::try_from(batches.len()).unwrap_or(u64::MAX),
        ready_bytes,
        oldest_batch_unix_ms,
        spool_observed: spool_error.is_none(),
        spool_error,
        storage_pressure: collector.storage_pressure,
        last_upload_unix_ms: upload
            .as_ref()
            .and_then(|value| {
                value
                    .get("last_success_unix_ms")
                    .or_else(|| value.get("last_upload_unix_ms"))
            })
            .and_then(serde_json::Value::as_i64),
        last_upload_attempt_unix_ms: upload
            .as_ref()
            .and_then(|value| value.get("last_attempt_unix_ms"))
            .and_then(serde_json::Value::as_i64),
        collector_error: collector.last_error,
        upload_error: upload_error.clone(),
        upload_status_error,
        last_error: upload_error
            .or_else(|| collector.last_error.map(|code| code.as_str().to_owned()))
            .or_else(|| spool_error.map(|code| code.as_str().to_owned()))
            .or_else(|| collector_status_error.map(|code| code.as_str().to_owned()))
            .or_else(|| upload_status_error.map(|code| code.as_str().to_owned())),
    })
}

/// Probes configuration, runtime endpoint, spool, and diagnostics without creating files.
///
/// # Errors
/// Returns an error only when the probe cannot construct a coherent report.
pub fn doctor_probe(root: &Path) -> Result<DoctorReport> {
    let paths = layout(root);
    Ok(DoctorReport {
        logs_root: probe_directory(root),
        config: match load_config(root) {
            Ok(_) => ProbeStatus::Ok,
            Err(error) if error.code() == ErrorCode::NotConfigured => ProbeStatus::Missing,
            Err(error) if error.code() == ErrorCode::Permission => ProbeStatus::Permission,
            Err(_) => ProbeStatus::Invalid,
        },
        runtime_directory: paths
            .endpoint
            .parent()
            .map_or(ProbeStatus::Invalid, probe_directory),
        fifo: probe_fifo(&paths.endpoint),
        spool: probe_directory(&paths.spool),
        diagnostics: probe_regular_or_missing(&paths.hook_diagnostics, 16 * 1024 * 1024),
        endpoint_path: paths.endpoint.display().to_string(),
    })
}

fn probe_directory(path: &Path) -> ProbeStatus {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => ProbeStatus::Ok,
        Ok(_) => ProbeStatus::Invalid,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProbeStatus::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            ProbeStatus::Permission
        }
        Err(_) => ProbeStatus::Unavailable,
    }
}

fn probe_fifo(path: &Path) -> ProbeStatus {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_fifo() => ProbeStatus::Ok,
        Ok(_) => ProbeStatus::Invalid,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProbeStatus::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            ProbeStatus::Permission
        }
        Err(_) => ProbeStatus::Unavailable,
    }
}

fn probe_regular_or_missing(path: &Path, limit: u64) -> ProbeStatus {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= limit => ProbeStatus::Ok,
        Ok(_) => ProbeStatus::Invalid,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProbeStatus::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            ProbeStatus::Permission
        }
        Err(_) => ProbeStatus::Unavailable,
    }
}

fn read_upload_status(path: &Path) -> Result<Option<serde_json::Value>> {
    let bytes = read_bounded(path, 64 * 1024, ErrorCode::InvalidInput)?;
    let value = serde_json::from_slice(&bytes).map_err(|error| {
        crate::Error::new(
            ErrorCode::InvalidInput,
            format!("parse upload status: {error}"),
        )
    })?;
    Ok(Some(value))
}

pub(crate) fn load_collector_status(root: &Path) -> CollectorStatus {
    read_bounded(
        &layout(root).collector_status,
        64 * 1024,
        ErrorCode::InvalidInput,
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    .unwrap_or_default()
}
