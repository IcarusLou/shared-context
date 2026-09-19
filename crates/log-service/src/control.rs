use std::{
    fs,
    os::unix::fs::FileTypeExt,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ErrorCode, Result, atomic_write_json, fs::read_bounded, layout};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SealRequestOutcome {
    Sealed,
    NoCollector,
    TimedOut,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SealRequest {
    pub nonce: String,
    pub requested_at_unix_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SealAck {
    pub nonce: String,
    pub sealed: bool,
    pub error: Option<String>,
}

/// Requests an active collector seal and waits at most two seconds for its matching acknowledgement.
///
/// # Errors
/// Returns an error when the request marker cannot be durably published or inspected.
pub fn request_seal(root: &Path, deadline: &Instant) -> Result<SealRequestOutcome> {
    let paths = layout(root);
    let endpoint_exists =
        fs::symlink_metadata(&paths.endpoint).is_ok_and(|metadata| metadata.file_type().is_fifo());
    if !endpoint_exists {
        return Ok(SealRequestOutcome::NoCollector);
    }
    let request = SealRequest {
        nonce: Uuid::new_v4().to_string(),
        requested_at_unix_ms: now_unix_ms(),
    };
    let request_path = paths.state.join("seal-request.json");
    let ack_path = paths.state.join("seal-ack.json");
    atomic_write_json(&request_path, &request)?;
    let local_limit = Instant::now() + Duration::from_secs(2);
    let effective_deadline = (*deadline).min(local_limit);
    while Instant::now() < effective_deadline {
        match read_bounded(&ack_path, 4096, ErrorCode::InvalidInput) {
            Ok(bytes) => {
                if let Ok(ack) = serde_json::from_slice::<SealAck>(&bytes)
                    && ack.nonce == request.nonce
                {
                    return Ok(if ack.sealed {
                        SealRequestOutcome::Sealed
                    } else {
                        SealRequestOutcome::Rejected
                    });
                }
            }
            Err(error) if ack_path.exists() => return Err(error),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(SealRequestOutcome::TimedOut)
}

pub(crate) fn read_pending(root: &Path) -> Option<SealRequest> {
    let paths = layout(root);
    let request = read_bounded(
        &paths.state.join("seal-request.json"),
        4096,
        ErrorCode::InvalidInput,
    )
    .ok()?;
    let request: SealRequest = serde_json::from_slice(&request).ok()?;
    let already_acked = read_bounded(
        &paths.state.join("seal-ack.json"),
        4096,
        ErrorCode::InvalidInput,
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice::<SealAck>(&bytes).ok())
    .is_some_and(|ack| ack.nonce == request.nonce);
    (!already_acked).then_some(request)
}

pub(crate) fn acknowledge(
    root: &Path,
    request: &SealRequest,
    result: &Result<Option<crate::ReadyBatch>>,
) -> Result<()> {
    let ack = SealAck {
        nonce: request.nonce.clone(),
        sealed: result.is_ok(),
        error: result
            .as_ref()
            .err()
            .map(|error| format!("{:?}", error.code())),
    };
    atomic_write_json(&layout(root).state.join("seal-ack.json"), &ack)
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
