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
    pub last_upload_attempt_stage: Option<String>,
    pub last_upload_attempt_detail: Option<String>,
    pub last_upload_attempt_retryable: Option<bool>,
    pub last_upload_attempt_uploaded_batches: Option<u64>,
    pub last_upload_attempt_uploaded_bytes: Option<u64>,
    pub next_upload_retry_unix_ms: Option<i64>,
    pub consecutive_upload_failures: Option<u64>,
    pub automatic_upload_retry_blocked: Option<bool>,
    pub blocked_upload_stream_id: Option<String>,
    pub collector_error: Option<ErrorCode>,
    pub upload_error: Option<String>,
    pub upload_status_observed: bool,
    pub upload_status_schema_version: Option<u64>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

impl Default for CollectorStatus {
    fn default() -> Self {
        Self {
            schema_version: 1,
            accepted: 0,
            persisted: 0,
            observable_dropped: 0,
            invalid_frames: 0,
            last_heartbeat_unix_ms: None,
            storage_pressure: false,
            last_error: None,
        }
    }
}

/// Reads the local collector, spool, and upload status without mutating them.
///
/// # Errors
/// Returns an error only when top-level status inspection itself cannot proceed.
#[allow(clippy::too_many_lines)]
pub fn status(root: &Path) -> Result<StatusReport> {
    let paths = layout(root);
    let (configured, config_error, enabled) = match load_config(root) {
        Ok(config) => (true, None, Some(config.enabled)),
        Err(error) => (false, Some(error.code()), None),
    };
    let (collector, collector_status_error): (CollectorStatus, Option<ErrorCode>) =
        match read_bounded(&paths.collector_status, 64 * 1024, ErrorCode::InvalidInput) {
            Ok(bytes) => match serde_json::from_slice::<CollectorStatus>(&bytes) {
                Ok(mut value) if value.schema_version <= 1 => {
                    value.schema_version = 1;
                    (value, None)
                }
                Ok(_) | Err(_) => (CollectorStatus::default(), Some(ErrorCode::InvalidInput)),
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
        last_upload_attempt_stage: upload
            .as_ref()
            .and_then(|value| value.get("last_error_stage"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        last_upload_attempt_detail: upload
            .as_ref()
            .and_then(|value| value.get("last_error_detail"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        last_upload_attempt_retryable: upload
            .as_ref()
            .and_then(|value| value.get("last_error_retryable"))
            .and_then(serde_json::Value::as_bool),
        last_upload_attempt_uploaded_batches: upload
            .as_ref()
            .and_then(|value| value.get("last_attempt_uploaded_batches"))
            .and_then(serde_json::Value::as_u64),
        last_upload_attempt_uploaded_bytes: upload
            .as_ref()
            .and_then(|value| value.get("last_attempt_uploaded_bytes"))
            .and_then(serde_json::Value::as_u64),
        next_upload_retry_unix_ms: upload
            .as_ref()
            .and_then(|value| value.get("next_retry_unix_ms"))
            .and_then(serde_json::Value::as_i64),
        consecutive_upload_failures: upload
            .as_ref()
            .and_then(|value| value.get("consecutive_retryable_failures"))
            .and_then(serde_json::Value::as_u64),
        automatic_upload_retry_blocked: upload
            .as_ref()
            .and_then(|value| value.get("automatic_retry_blocked"))
            .and_then(serde_json::Value::as_bool),
        blocked_upload_stream_id: upload
            .as_ref()
            .and_then(|value| value.get("blocked_stream_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        collector_error: collector.last_error,
        upload_error: upload_error.clone(),
        upload_status_observed: upload.is_some(),
        upload_status_schema_version: upload
            .as_ref()
            .and_then(|value| value.get("schema_version"))
            .and_then(serde_json::Value::as_u64),
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
    .and_then(|bytes| serde_json::from_slice::<CollectorStatus>(&bytes).ok())
    .filter(|status| status.schema_version <= 1)
    .map(|mut status| {
        status.schema_version = 1;
        status
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn collector_schema_zero_is_read_and_normalized_for_the_next_write() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        fs::write(
            root.path().join("state/collector-status.json"),
            br#"{"schema_version":0,"accepted":3,"persisted":2,"observable_dropped":1,"invalid_frames":0,"last_heartbeat_unix_ms":17,"storage_pressure":false,"last_error":null}"#,
        )
        .unwrap();

        let loaded = load_collector_status(root.path());
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.accepted, 3);
        let report = status(root.path()).unwrap();
        assert_eq!(report.collector_status_error, None);
        assert_eq!(report.collector_persisted, 2);
    }

    #[test]
    fn upload_status_preserves_unknown_and_partial_attempt_details() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        fs::write(
            root.path().join("state/upload-status.json"),
            br#"{"schema_version":1,"last_attempt_unix_ms":200,"last_success_unix_ms":100,"last_error":"timeout","last_error_stage":"push","last_error_code":"timeout","last_error_detail":"push exceeded deadline","last_error_retryable":true,"last_attempt_uploaded_batches":10,"last_attempt_uploaded_bytes":300000,"next_retry_unix_ms":260,"consecutive_retryable_failures":1,"automatic_retry_blocked":false}"#,
        )
        .unwrap();

        let report = status(root.path()).unwrap();
        assert_eq!(report.last_upload_unix_ms, Some(100));
        assert!(report.upload_status_observed);
        assert_eq!(report.upload_status_schema_version, Some(1));
        assert_eq!(report.last_upload_attempt_unix_ms, Some(200));
        assert_eq!(report.last_upload_attempt_stage.as_deref(), Some("push"));
        assert_eq!(
            report.last_upload_attempt_detail.as_deref(),
            Some("push exceeded deadline")
        );
        assert_eq!(report.last_upload_attempt_uploaded_batches, Some(10));
        assert_eq!(report.last_upload_attempt_uploaded_bytes, Some(300_000));
        assert_eq!(report.next_upload_retry_unix_ms, Some(260));
        assert_eq!(report.consecutive_upload_failures, Some(1));
        assert_eq!(report.automatic_upload_retry_blocked, Some(false));
        assert_eq!(report.blocked_upload_stream_id, None);

        fs::write(
            root.path().join("state/upload-status.json"),
            br#"{"schema_version":1,"last_attempt_unix_ms":null,"last_success_unix_ms":null,"last_error":null}"#,
        )
        .unwrap();
        let unknown = status(root.path()).unwrap();
        assert!(unknown.upload_status_observed);
        assert_eq!(unknown.last_upload_unix_ms, None);
        assert_eq!(unknown.last_upload_attempt_uploaded_batches, None);
        assert_eq!(unknown.automatic_upload_retry_blocked, None);
        assert_eq!(unknown.blocked_upload_stream_id, None);

        fs::write(
            root.path().join("state/upload-status.json"),
            br#"{"schema_version":1,"last_attempt_unix_ms":300,"last_success_unix_ms":100,"last_error":"conflict","automatic_retry_blocked":true,"blocked_stream_id":"stream-a"}"#,
        )
        .unwrap();
        let blocked = status(root.path()).unwrap();
        assert_eq!(blocked.automatic_upload_retry_blocked, Some(true));
        assert_eq!(
            blocked.blocked_upload_stream_id.as_deref(),
            Some("stream-a")
        );

        fs::remove_file(root.path().join("state/upload-status.json")).unwrap();
        let missing = status(root.path()).unwrap();
        assert!(!missing.upload_status_observed);
        assert_eq!(missing.last_upload_unix_ms, None);
    }
}
