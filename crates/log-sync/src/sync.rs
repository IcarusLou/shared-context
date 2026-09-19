use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use sctx_log_service::{
    BatchManifest, Config, ReadyBatch, SealRequestOutcome, atomic_write_json, list_ready_batches,
    load_config, request_seal,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runner::{CommandSpec, DiskGuard, RunnerError, git_environment, run};

const STATE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);
const DEFAULT_MAX_NEW_PAYLOAD: u64 = 20 * 1024 * 1024;
const DEFAULT_SHARD_MAX_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_SHARD_MAX_FILES: u64 = 4096;
const DEFAULT_LOCAL_BUDGET: u64 = 1024 * 1024 * 1024;
const DEFAULT_MIN_FREE: u64 = 1024 * 1024 * 1024;
const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
const MAX_GROUP_BATCHES: usize = 256;

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub deadline: Duration,
    pub max_new_payload_bytes: u64,
    pub shard_max_bytes: u64,
    pub shard_max_files: u64,
    pub local_budget_bytes: u64,
    pub min_free_disk_bytes: u64,
    pub cache_max_age: Duration,
    pub max_retries: u8,
    pub output_limit: usize,
    pub git_binary: PathBuf,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_DEADLINE,
            max_new_payload_bytes: DEFAULT_MAX_NEW_PAYLOAD,
            shard_max_bytes: DEFAULT_SHARD_MAX_BYTES,
            shard_max_files: DEFAULT_SHARD_MAX_FILES,
            local_budget_bytes: DEFAULT_LOCAL_BUDGET,
            min_free_disk_bytes: DEFAULT_MIN_FREE,
            cache_max_age: DEFAULT_CACHE_MAX_AGE,
            max_retries: 1,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            git_binary: PathBuf::from("git"),
        }
    }
}

impl SyncOptions {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            deadline: Duration::from_secs(config.sync.timeout_seconds.min(60)),
            max_new_payload_bytes: config.sync.max_new_payload_mib.saturating_mul(MIB),
            shard_max_bytes: config.storage.shard_max_mib.saturating_mul(MIB),
            shard_max_files: config.storage.shard_max_files,
            local_budget_bytes: config.storage.local_budget_mib.saturating_mul(MIB),
            min_free_disk_bytes: config.storage.min_free_disk_mib.saturating_mul(MIB),
            cache_max_age: Duration::from_secs(
                config
                    .storage
                    .cache_max_age_days
                    .saturating_mul(24 * 60 * 60),
            ),
            max_retries: u8::try_from(config.sync.max_retry_count).unwrap_or(u8::MAX),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    Uploaded,
    Partial,
    NoReady,
    SkippedBusy,
    SkippedNotDue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SealStatus {
    Sealed,
    NoCollector,
    TimedOut,
    Rejected,
    RequestFailed,
    NotRequestedBusy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncReport {
    pub outcome: SyncOutcome,
    pub uploaded_batches: u64,
    pub uploaded_bytes: u64,
    pub remaining_batches: u64,
    pub branch: Option<String>,
    pub cache_pruned: bool,
    pub seal_status: SealStatus,
    pub active_included: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PruneReport {
    pub removed: bool,
    pub bytes_reclaimed: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncErrorCode {
    InvalidRoot,
    InvalidBatch,
    InvalidRemote,
    Io,
    Git,
    Timeout,
    Conflict,
    RemoteHistoryMissing,
    PendingJournal,
    StorageBudget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncErrorStage {
    Validate,
    Discover,
    Recover,
    Prepare,
    Fetch,
    Apply,
    Commit,
    Push,
    Verify,
    Receipt,
    Cleanup,
    Storage,
}

#[derive(Debug)]
pub struct SyncError {
    code: SyncErrorCode,
    stage: SyncErrorStage,
    safe_detail: String,
    local_diagnostic: Option<String>,
    uploaded_batches: u64,
    uploaded_bytes: u64,
}

impl SyncError {
    #[must_use]
    pub const fn code(&self) -> SyncErrorCode {
        self.code
    }

    #[must_use]
    pub const fn stage(&self) -> SyncErrorStage {
        self.stage
    }

    #[must_use]
    pub fn safe_detail(&self) -> &str {
        &self.safe_detail
    }

    /// What the failing tool actually said, bounded and stripped of control characters.
    ///
    /// [`safe_detail`](Self::safe_detail) is a closed vocabulary on purpose, and it stays one:
    /// it is the string that travels, and a classifier that cannot recognise a failure reports
    /// `unknown` rather than leaking an unbounded message. `unknown` is also where diagnosis
    /// stops. A real installation spent nearly twenty hours on
    /// `git ls-remote exited with code 128 (unknown)`, twenty-two times, with Git's own
    /// explanation discarded at the moment it was produced and nothing on disk to recover it
    /// from.
    ///
    /// This is that explanation, and it is **local only**: it is written to
    /// `<logs_root>/state/upload-status.json`, which is a private file on the machine that
    /// produced the failure, and it must never be copied into a telemetry Event, a report that
    /// is uploaded, or anything else that leaves the host.
    #[must_use]
    pub fn local_diagnostic(&self) -> Option<&str> {
        self.local_diagnostic.as_deref()
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self.code,
            SyncErrorCode::Io
                | SyncErrorCode::Git
                | SyncErrorCode::Timeout
                | SyncErrorCode::StorageBudget
        )
    }

    const fn retryable_in_attempt(&self) -> bool {
        matches!(
            self.code,
            SyncErrorCode::Io | SyncErrorCode::Git | SyncErrorCode::Timeout
        )
    }

    #[must_use]
    pub const fn uploaded_batches(&self) -> u64 {
        self.uploaded_batches
    }

    #[must_use]
    pub const fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }

    fn new(code: SyncErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            stage: default_error_stage(code),
            safe_detail: bounded_safe_detail(message.into()),
            local_diagnostic: None,
            uploaded_batches: 0,
            uploaded_bytes: 0,
        }
    }

    fn at(stage: SyncErrorStage, code: SyncErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            stage,
            safe_detail: bounded_safe_detail(detail.into()),
            local_diagnostic: None,
            uploaded_batches: 0,
            uploaded_bytes: 0,
        }
    }

    fn with_local_diagnostic(mut self, diagnostic: Option<String>) -> Self {
        self.local_diagnostic = diagnostic;
        self
    }

    fn with_stage(mut self, stage: SyncErrorStage) -> Self {
        self.stage = stage;
        self
    }

    fn with_progress(mut self, uploaded_batches: u64, uploaded_bytes: u64) -> Self {
        self.uploaded_batches = self.uploaded_batches.saturating_add(uploaded_batches);
        self.uploaded_bytes = self.uploaded_bytes.saturating_add(uploaded_bytes);
        self
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.safe_detail)
    }
}

impl std::error::Error for SyncError {}

impl From<io::Error> for SyncError {
    fn from(error: io::Error) -> Self {
        Self::new(SyncErrorCode::Io, safe_io_detail(error.kind()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Prepared,
    Committed,
    RemoteConfirmed,
    ReceiptDurable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct UploadJournal {
    schema_version: u32,
    batch_id: String,
    stream_id: String,
    remote: String,
    remote_digest: String,
    branch: String,
    events_relative_path: String,
    manifest_relative_path: String,
    content_bytes: u64,
    content_sha256: String,
    phase: JournalPhase,
    commit_oid: Option<String>,
    #[serde(default)]
    batches: Vec<JournalBatch>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct JournalBatch {
    batch_id: String,
    events_relative_path: String,
    manifest_relative_path: String,
    content_bytes: u64,
    content_sha256: String,
    #[serde(default)]
    receipt_durable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Receipt {
    schema_version: u32,
    batch_id: String,
    stream_id: String,
    remote_digest: String,
    branch: String,
    content_sha256: String,
    remote_commit_oid: String,
    confirmed_at_unix_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ShardState {
    schema_version: u32,
    stream_id: String,
    remote_digest: String,
    month: String,
    index: u32,
    branch: String,
    #[serde(default)]
    remote_confirmed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CacheOwnership {
    schema_version: u32,
    canonical_logs_root: String,
    remote_digest: String,
    branch: String,
    created_at_unix_ms: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct UploadStatus {
    schema_version: u32,
    last_attempt_unix_ms: Option<i64>,
    last_success_unix_ms: Option<i64>,
    last_error: Option<String>,
    #[serde(default)]
    last_error_stage: Option<SyncErrorStage>,
    #[serde(default)]
    last_error_code: Option<SyncErrorCode>,
    #[serde(default)]
    last_error_detail: Option<String>,
    #[serde(default)]
    last_error_retryable: Option<bool>,
    /// What the failing tool said, for the operator reading this file. Local only --- see
    /// [`SyncError::local_diagnostic`].
    #[serde(default)]
    last_error_diagnostic: Option<String>,
    /// When the current unbroken run of failures began.
    ///
    /// Without it a streak has no start: `consecutive_retryable_failures` counts attempts and
    /// `last_attempt_unix_ms` names the newest one, so "22 failures" could have begun an hour ago
    /// or two days ago, and only a lucky `last_success_unix_ms` narrowed it down. A real
    /// installation had to infer a 19h50m outage from the last success, because nothing recorded
    /// when the outage started.
    #[serde(default)]
    first_failure_unix_ms: Option<i64>,
    #[serde(default)]
    last_attempt_uploaded_batches: u64,
    #[serde(default)]
    last_attempt_uploaded_bytes: u64,
    #[serde(default)]
    next_retry_unix_ms: Option<i64>,
    #[serde(default)]
    consecutive_retryable_failures: u32,
    #[serde(default)]
    automatic_retry_blocked: bool,
    #[serde(default)]
    target_digest: Option<String>,
    #[serde(default)]
    configured_target_digest: Option<String>,
    #[serde(default)]
    blocked_stream_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkScope {
    digest: String,
    stream_id: Option<String>,
}

struct SyncLock {
    file: File,
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Uploads sealed batches under `root` while holding only the logging sync lock.
///
/// # Errors
///
/// Returns a typed error for invalid state, unsafe paths, storage pressure, Git failure, a remote
/// conflict, or deadline expiry. Unconfirmed source batches are preserved on every error.
#[allow(clippy::too_many_lines)]
pub fn sync(root: &Path, options: &SyncOptions) -> Result<SyncReport, SyncError> {
    sync_mode(root, options, false)
}

/// Runs a scheduled synchronization only when its durable retry time is due.
///
/// The due check and attempt status update happen under the same synchronization lock. A
/// permanent error blocks scheduled attempts until a manual [`sync`] succeeds or records a new
/// retryable result.
///
/// # Errors
///
/// Returns the same typed synchronization errors as [`sync`].
pub fn sync_scheduled(root: &Path, options: &SyncOptions) -> Result<SyncReport, SyncError> {
    sync_mode(root, options, true)
}

fn sync_mode(root: &Path, options: &SyncOptions, scheduled: bool) -> Result<SyncReport, SyncError> {
    validate_options(options)?;
    let canonical_root = validate_root(root)?;
    let Some(_lock) = acquire_lock(&canonical_root)? else {
        return Ok(SyncReport {
            outcome: SyncOutcome::SkippedBusy,
            uploaded_batches: 0,
            uploaded_bytes: 0,
            remaining_batches: 0,
            branch: None,
            cache_pruned: false,
            seal_status: SealStatus::NotRequestedBusy,
            active_included: false,
        });
    };
    let configured = load_config(&canonical_root).ok();
    let configured_target_digest = configured.as_ref().and_then(config_target_digest);
    if scheduled {
        if configured.as_ref().is_none_or(|config| {
            !config.enabled
                || !config.sync.scheduled
                || config.email.is_none()
                || config.remote.is_none()
        }) {
            return Ok(SyncReport {
                outcome: SyncOutcome::SkippedNotDue,
                uploaded_batches: 0,
                uploaded_bytes: 0,
                remaining_batches: 0,
                branch: None,
                cache_pruned: false,
                seal_status: SealStatus::NotRequestedBusy,
                active_included: false,
            });
        }
        let status = read_upload_status(&canonical_root);
        let work_scope = next_work_scope(&canonical_root, configured.as_ref());
        let not_due = scheduled_gate_is_closed(
            &status,
            work_scope.as_ref(),
            configured_target_digest.as_deref(),
            now_unix_millis(),
        );
        if not_due {
            return Ok(SyncReport {
                outcome: SyncOutcome::SkippedNotDue,
                uploaded_batches: 0,
                uploaded_bytes: 0,
                remaining_batches: list_ready_batches(&canonical_root)
                    .map_or(0, |ready| ready.len() as u64),
                branch: None,
                cache_pruned: false,
                seal_status: SealStatus::NotRequestedBusy,
                active_included: false,
            });
        }
        atomic_write_json(&canonical_root.join("state/upload-status.json"), &status).map_err(
            |_| {
                SyncError::at(
                    SyncErrorStage::Prepare,
                    SyncErrorCode::Io,
                    "scheduled upload status preflight failed",
                )
            },
        )?;
    }
    let result = sync_locked(root, &canonical_root, options);
    let next_scope = next_work_scope(&canonical_root, configured.as_ref());
    record_upload_status_locked(
        &canonical_root,
        &result,
        next_scope.as_ref(),
        configured_target_digest,
    );
    result
}

#[allow(clippy::too_many_lines)]
fn sync_locked(
    requested_root: &Path,
    canonical_root: &Path,
    options: &SyncOptions,
) -> Result<SyncReport, SyncError> {
    let deadline = Instant::now() + options.deadline;
    let seal_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_secs(2));
    // The FIFO endpoint identity is derived from the caller's path spelling. Keep state and Git
    // operations on the validated canonical root, but address the collector exactly as producers
    // and `logs collect --logs-root` do (for example `/tmp` versus `/private/tmp` on macOS).
    let seal_status = match request_seal(requested_root, &seal_deadline) {
        Ok(SealRequestOutcome::Sealed) => SealStatus::Sealed,
        Ok(SealRequestOutcome::NoCollector) => SealStatus::NoCollector,
        Ok(SealRequestOutcome::TimedOut) => SealStatus::TimedOut,
        Ok(SealRequestOutcome::Rejected) => SealStatus::Rejected,
        Err(_) => SealStatus::RequestFailed,
    };
    let mut ready = list_ready_batches(canonical_root)
        .map_err(|_| SyncError::new(SyncErrorCode::InvalidBatch, "ready batch discovery failed"))?;
    ready.sort_by_key(|batch| batch.manifest.sealed_at_unix_ms);
    let journal_pending = load_journal(canonical_root)?.is_some();
    ensure_storage_headroom(canonical_root, options, journal_pending)?;
    let mut uploaded_batches = 0_u64;
    let mut uploaded_bytes = 0_u64;
    let mut last_branch = None;

    if let Some(journal) = load_journal(canonical_root)? {
        let recovered =
            recover_pending_journal(canonical_root, &ready, journal, options, deadline)?;
        uploaded_batches = uploaded_batches.saturating_add(recovered.0);
        uploaded_bytes = uploaded_bytes.saturating_add(recovered.1);
        last_branch = recovered.2;
        ready = list_ready_batches(canonical_root).map_err(|_| {
            SyncError::new(SyncErrorCode::InvalidBatch, "ready batch discovery failed")
                .with_progress(uploaded_batches, uploaded_bytes)
        })?;
        ready.sort_by_key(|batch| batch.manifest.sealed_at_unix_ms);
    }
    if ready.is_empty() {
        let cache_pruned = prune_expired_cache_locked(canonical_root, options.cache_max_age)
            .map_err(|error| error.with_progress(uploaded_batches, uploaded_bytes))?;
        compact_receipts(canonical_root)
            .map_err(|error| error.with_progress(uploaded_batches, uploaded_bytes))?;
        return Ok(SyncReport {
            outcome: if uploaded_batches == 0 {
                SyncOutcome::NoReady
            } else {
                SyncOutcome::Uploaded
            },
            uploaded_batches,
            uploaded_bytes,
            remaining_batches: 0,
            branch: last_branch,
            cache_pruned,
            seal_status,
            active_included: seal_status == SealStatus::Sealed,
        });
    }

    let mut cursor = 0_usize;
    while cursor < ready.len() {
        let group_end = select_group_end(&ready, cursor, uploaded_batches, uploaded_bytes, options)
            .map_err(|error| error.with_progress(uploaded_batches, uploaded_bytes))?;
        if group_end == cursor {
            break;
        }
        let group = &ready[cursor..group_end];
        let group_bytes = group_payload_bytes(group)
            .map_err(|error| error.with_progress(uploaded_batches, uploaded_bytes))?;
        if uploaded_batches > 0
            && uploaded_bytes.saturating_add(group_bytes) > options.max_new_payload_bytes
        {
            break;
        }
        let branch = match sync_group(canonical_root, group, options, deadline) {
            Ok(branch) => branch,
            Err(error) => {
                return Err(error.with_progress(uploaded_batches, uploaded_bytes));
            }
        };
        uploaded_batches = uploaded_batches.saturating_add(group.len() as u64);
        uploaded_bytes = uploaded_bytes.saturating_add(group_bytes);
        last_branch = Some(branch);
        cursor = group_end;
        if Instant::now() >= deadline {
            break;
        }
    }
    let remaining = list_ready_batches(canonical_root)
        .map_err(|_| {
            SyncError::new(SyncErrorCode::InvalidBatch, "ready batch discovery failed")
                .with_progress(uploaded_batches, uploaded_bytes)
        })?
        .len() as u64;
    let cache_pruned = if remaining == 0 {
        prune_expired_cache_locked(canonical_root, options.cache_max_age)
            .map_err(|error| error.with_progress(uploaded_batches, uploaded_bytes))?
    } else {
        false
    };
    if remaining == 0 {
        compact_receipts(canonical_root)
            .map_err(|error| error.with_progress(uploaded_batches, uploaded_bytes))?;
    }
    Ok(SyncReport {
        outcome: if remaining == 0 {
            SyncOutcome::Uploaded
        } else {
            SyncOutcome::Partial
        },
        uploaded_batches,
        uploaded_bytes,
        remaining_batches: remaining,
        branch: last_branch,
        cache_pruned,
        seal_status,
        active_included: seal_status == SealStatus::Sealed,
    })
}

fn read_upload_status(root: &Path) -> UploadStatus {
    let path = root.join("state/upload-status.json");
    let mut status = read_json::<UploadStatus>(&path).unwrap_or_default();
    if status.last_success_unix_ms.is_none() {
        status.last_success_unix_ms = latest_receipt_success(root);
    }
    status
}

fn latest_receipt_success(root: &Path) -> Option<i64> {
    fs::read_dir(root.join("state/receipts"))
        .ok()?
        .filter_map(Result::ok)
        .take(1024)
        .filter_map(|entry| read_json::<Receipt>(&entry.path()).ok())
        .filter(|receipt| receipt.schema_version == STATE_SCHEMA_VERSION)
        .map(|receipt| receipt.confirmed_at_unix_ms)
        .max()
}

fn record_upload_status_locked(
    root: &Path,
    result: &Result<SyncReport, SyncError>,
    next_scope: Option<&WorkScope>,
    configured_target_digest: Option<String>,
) {
    let path = root.join("state/upload-status.json");
    let mut status = read_upload_status(root);
    status.schema_version = STATE_SCHEMA_VERSION;
    status.target_digest = next_scope.map(|scope| scope.digest.clone());
    status.configured_target_digest = configured_target_digest;
    let now = now_unix_millis();
    status.last_attempt_unix_ms = Some(now);
    match result {
        Ok(report) => {
            if report.uploaded_batches > 0 {
                status.last_success_unix_ms = Some(now);
            }
            status.last_error = None;
            status.last_error_stage = None;
            status.last_error_code = None;
            status.last_error_detail = None;
            status.last_error_retryable = None;
            status.last_error_diagnostic = None;
            status.first_failure_unix_ms = None;
            status.last_attempt_uploaded_batches = report.uploaded_batches;
            status.last_attempt_uploaded_bytes = report.uploaded_bytes;
            status.consecutive_retryable_failures = 0;
            status.automatic_retry_blocked = false;
            status.blocked_stream_id = None;
            status.next_retry_unix_ms = Some(now.saturating_add(5 * 60 * 1000));
        }
        Err(error) => {
            if error.uploaded_batches() > 0 {
                status.last_success_unix_ms = Some(now);
            }
            status.last_error = Some(sync_error_code_name(error.code()).to_owned());
            status.last_error_stage = Some(error.stage());
            status.last_error_code = Some(error.code());
            status.last_error_detail = Some(error.safe_detail().to_owned());
            status.last_error_retryable = Some(error.retryable());
            status.last_error_diagnostic = error.local_diagnostic().map(str::to_owned);
            // The streak's start is kept, not overwritten: this is the one field that says how
            // long an outage has been running, and re-stamping it every attempt would make every
            // outage look like it began moments ago.
            status.first_failure_unix_ms = status.first_failure_unix_ms.or(Some(now));
            status.last_attempt_uploaded_batches = error.uploaded_batches();
            status.last_attempt_uploaded_bytes = error.uploaded_bytes();
            if error.retryable() {
                status.consecutive_retryable_failures =
                    status.consecutive_retryable_failures.saturating_add(1);
                status.automatic_retry_blocked = false;
                status.blocked_stream_id = None;
                let delays = [1_i64, 5, 15, 60];
                let index =
                    usize::try_from(status.consecutive_retryable_failures.saturating_sub(1))
                        .unwrap_or(usize::MAX)
                        .min(delays.len() - 1);
                status.next_retry_unix_ms = Some(now.saturating_add(delays[index] * 60 * 1000));
            } else {
                status.consecutive_retryable_failures = 0;
                status.automatic_retry_blocked = true;
                status.blocked_stream_id = next_scope.and_then(|scope| scope.stream_id.clone());
                status.next_retry_unix_ms = None;
            }
        }
    }
    // Upload status is an observational summary. Once a receipt is durable, failure to update
    // this file must not turn a completed upload into an error or reclassify the batch.
    let _ = atomic_write_json(&path, &status);
}

fn config_target_digest(config: &Config) -> Option<String> {
    let remote = config.remote.as_deref()?;
    Some(target_digest(&config.stream_id, remote))
}

fn target_digest(stream_id: &str, remote: &str) -> String {
    sha256_bytes(format!("{stream_id}\0{remote}").as_bytes())
}

fn next_work_scope(root: &Path, configured: Option<&Config>) -> Option<WorkScope> {
    if journal_path(root).exists() {
        return Some(match load_journal(root) {
            Ok(Some(journal)) => WorkScope {
                digest: target_digest(&journal.stream_id, &journal.remote),
                stream_id: Some(journal.stream_id),
            },
            Ok(None) => unreachable!("journal existence was checked"),
            Err(_) => root_work_scope("corrupt_journal"),
        });
    }
    let Ok(mut ready) = list_ready_batches(root) else {
        return Some(root_work_scope("corrupt_spool"));
    };
    ready.sort_by_key(|batch| batch.manifest.sealed_at_unix_ms);
    if let Some(batch) = ready.first() {
        return Some(match assigned_remote(&batch.manifest) {
            Ok(remote) => WorkScope {
                digest: target_digest(&batch.manifest.stream.stream_id, remote),
                stream_id: Some(batch.manifest.stream.stream_id.clone()),
            },
            Err(_) => root_work_scope("corrupt_spool"),
        });
    }
    configured.and_then(|config| {
        config.remote.as_deref().map(|remote| WorkScope {
            digest: target_digest(&config.stream_id, remote),
            stream_id: Some(config.stream_id.clone()),
        })
    })
}

fn root_work_scope(kind: &str) -> WorkScope {
    WorkScope {
        digest: sha256_bytes(format!("root\0{kind}").as_bytes()),
        stream_id: None,
    }
}

fn scheduled_gate_is_closed(
    status: &UploadStatus,
    work_scope: Option<&WorkScope>,
    configured_target_digest: Option<&str>,
    now_unix_ms: i64,
) -> bool {
    let same_work =
        status.target_digest.as_deref() == work_scope.map(|scope| scope.digest.as_str());
    let same_config = status.configured_target_digest.as_deref() == configured_target_digest;
    same_work
        && same_config
        && (status.automatic_retry_blocked
            || status
                .next_retry_unix_ms
                .is_some_and(|next| next > now_unix_ms))
}

const fn sync_error_code_name(code: SyncErrorCode) -> &'static str {
    match code {
        SyncErrorCode::InvalidRoot => "invalid_root",
        SyncErrorCode::InvalidBatch => "invalid_batch",
        SyncErrorCode::InvalidRemote => "invalid_remote",
        SyncErrorCode::Io => "io",
        SyncErrorCode::Git => "git",
        SyncErrorCode::Timeout => "timeout",
        SyncErrorCode::Conflict => "conflict",
        SyncErrorCode::RemoteHistoryMissing => "remote_history_missing",
        SyncErrorCode::PendingJournal => "pending_journal",
        SyncErrorCode::StorageBudget => "storage_budget",
    }
}

/// Deletes the complete managed Git cache after validating its ownership and in-tree gitdir.
///
/// # Errors
///
/// Returns a typed error when synchronization is active, a journal is pending, or the cache is
/// not demonstrably owned by this logging root.
pub fn prune_cache(root: &Path) -> Result<PruneReport, SyncError> {
    let canonical_root = validate_root(root)?;
    let Some(_lock) = acquire_lock(&canonical_root)? else {
        return Err(SyncError::new(
            SyncErrorCode::PendingJournal,
            "log synchronization is active",
        ));
    };
    if journal_path(&canonical_root).exists() {
        return Err(SyncError::new(
            SyncErrorCode::PendingJournal,
            "upload journal must be recovered before pruning the cache",
        ));
    }
    remove_managed_cache(&canonical_root)
}

fn select_group_end(
    ready: &[ReadyBatch],
    start: usize,
    _already_uploaded_batches: u64,
    already_uploaded_bytes: u64,
    options: &SyncOptions,
) -> Result<usize, SyncError> {
    let first = &ready[start];
    validate_batch_identity(&first.manifest)?;
    let first_remote = assigned_remote(&first.manifest)?;
    let first_stream = &first.manifest.stream.stream_id;
    let mut end = start;
    let mut group_bytes = 0_u64;
    while let Some(batch) = ready.get(end) {
        if end - start >= MAX_GROUP_BATCHES {
            break;
        }
        validate_batch_identity(&batch.manifest)?;
        if &batch.manifest.stream.stream_id != first_stream
            || assigned_remote(&batch.manifest)? != first_remote
        {
            break;
        }
        let batch_bytes = batch_payload_bytes(batch)?;
        let next_bytes = group_bytes.saturating_add(batch_bytes);
        let next_count = u64::try_from(end - start + 1).unwrap_or(u64::MAX);
        let exceeds_run =
            already_uploaded_bytes.saturating_add(next_bytes) > options.max_new_payload_bytes;
        let exceeds_shard = next_bytes > options.shard_max_bytes
            || next_count.saturating_mul(2) > options.shard_max_files;
        if end > start && (exceeds_run || exceeds_shard) {
            break;
        }
        group_bytes = next_bytes;
        end += 1;
    }
    Ok(end)
}

fn batch_payload_bytes(batch: &ReadyBatch) -> Result<u64, SyncError> {
    Ok(batch
        .manifest
        .content_bytes
        .saturating_add(fs::metadata(&batch.manifest_path)?.len()))
}

fn group_payload_bytes(batches: &[ReadyBatch]) -> Result<u64, SyncError> {
    batches.iter().try_fold(0_u64, |total, batch| {
        Ok(total.saturating_add(batch_payload_bytes(batch)?))
    })
}

fn recover_pending_journal(
    root: &Path,
    ready: &[ReadyBatch],
    journal: UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(u64, u64, Option<String>), SyncError> {
    let entries = journal_entries(&journal);
    if matches!(journal.phase, JournalPhase::ReceiptDurable) {
        let receipts = entries
            .iter()
            .map(|entry| load_valid_receipt_entry(root, &journal, entry))
            .collect::<Result<Vec<_>, _>>();
        if let Ok(receipts) = receipts {
            retry_transient(options.max_retries, || {
                protect_durable_receipt_tip(root, &journal, &receipts, options, deadline)
            })?;
            mark_shard_remote_confirmed(root, &journal)?;
            for entry in &entries {
                if let Some(batch) = ready
                    .iter()
                    .find(|batch| batch.manifest.batch_id == entry.batch_id)
                {
                    delete_confirmed_batch(batch)?;
                }
            }
            remove_journal(root)?;
            return Ok((
                entries.len() as u64,
                entries.iter().fold(0_u64, |total, entry| {
                    total.saturating_add(entry.content_bytes)
                }),
                Some(journal.branch),
            ));
        }
    }
    let mut pending = Vec::with_capacity(entries.len());
    for entry in &entries {
        let batch = ready
            .iter()
            .find(|batch| batch.manifest.batch_id == entry.batch_id)
            .ok_or_else(|| {
                SyncError::at(
                    SyncErrorStage::Recover,
                    SyncErrorCode::InvalidBatch,
                    "upload journal references a missing batch without a durable receipt",
                )
            })?;
        validate_journal_batch(&journal, entry, batch)?;
        pending.push(batch.clone());
    }
    let bytes = group_payload_bytes(&pending)?;
    let branch = sync_group(root, &pending, options, deadline)?;
    Ok((pending.len() as u64, bytes, Some(branch)))
}

#[allow(clippy::too_many_lines)]
fn sync_group(
    root: &Path,
    batches: &[ReadyBatch],
    options: &SyncOptions,
    deadline: Instant,
) -> Result<String, SyncError> {
    let confirmed_bytes = group_payload_bytes(batches)?;
    let first = batches
        .first()
        .ok_or_else(|| SyncError::new(SyncErrorCode::InvalidBatch, "upload group is empty"))?;
    validate_batch_identity(&first.manifest)?;
    let remote = assigned_remote(&first.manifest)?;
    validate_remote(remote)?;
    let remote_digest = sha256_bytes(remote.as_bytes());
    let mut journal = if let Some(existing) = load_journal(root)? {
        let entries = journal_entries(&existing);
        if existing.stream_id != first.manifest.stream.stream_id
            || existing.remote_digest != remote_digest
            || existing.remote != first.manifest.stream.remote.as_deref().unwrap_or_default()
            || entries.len() != batches.len()
        {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "upload journal does not match the sealed batch group",
            ));
        }
        for (entry, batch) in entries.iter().zip(batches) {
            validate_journal_batch(&existing, entry, batch)?;
        }
        existing
    } else {
        let shard = retry_transient(options.max_retries, || {
            select_shard(root, first, options, deadline, &remote_digest)
        })?;
        let entries = batches
            .iter()
            .map(|batch| {
                let (events_relative_path, manifest_relative_path) = batch_paths(&batch.manifest)?;
                Ok(JournalBatch {
                    batch_id: batch.manifest.batch_id.clone(),
                    events_relative_path,
                    manifest_relative_path,
                    content_bytes: batch.manifest.content_bytes,
                    content_sha256: batch.manifest.content_sha256.clone(),
                    receipt_durable: false,
                })
            })
            .collect::<Result<Vec<_>, SyncError>>()?;
        let first_entry = entries.first().expect("nonempty upload group");
        let journal = UploadJournal {
            schema_version: STATE_SCHEMA_VERSION,
            batch_id: first_entry.batch_id.clone(),
            stream_id: first.manifest.stream.stream_id.clone(),
            remote: assigned_remote(&first.manifest)?.to_owned(),
            remote_digest,
            branch: shard.branch,
            events_relative_path: first_entry.events_relative_path.clone(),
            manifest_relative_path: first_entry.manifest_relative_path.clone(),
            content_bytes: first_entry.content_bytes,
            content_sha256: first_entry.content_sha256.clone(),
            phase: JournalPhase::Prepared,
            commit_oid: None,
            batches: entries,
        };
        write_journal(root, &journal)?;
        journal
    };
    retry_transient(options.max_retries, || {
        protect_confirmed_history(root, &journal, options, deadline)
    })?;

    if matches!(journal.phase, JournalPhase::ReceiptDurable) {
        let receipts = journal_entries(&journal)
            .iter()
            .map(|entry| load_valid_receipt_entry(root, &journal, entry))
            .collect::<Result<Vec<_>, _>>();
        if let Ok(receipts) = receipts {
            retry_transient(options.max_retries, || {
                protect_durable_receipt_tip(root, &journal, &receipts, options, deadline)
            })?;
            mark_shard_remote_confirmed(root, &journal)?;
            for batch in batches {
                delete_confirmed_batch(batch)?;
            }
            remove_journal(root)?;
            return Ok(journal.branch);
        }
        // A phase marker alone never authorizes deletion. Reconfirm the exact remote ref and
        // actual blobs, then replace the absent/corrupt receipt before touching the source.
        journal.phase = JournalPhase::Committed;
        write_journal(root, &journal)?;
    }

    let mut retries = 0_u8;
    let mut retry_stage = SyncErrorStage::Prepare;
    loop {
        ensure_before_deadline(deadline).map_err(|error| error.with_stage(retry_stage))?;
        let attempt = sync_group_attempt(root, batches, &mut journal, options, deadline);
        match attempt {
            Ok(Some(remote_oid)) => {
                finish_confirmed_group(root, batches, &mut journal, &remote_oid)
                    .map_err(|error| error.with_progress(batches.len() as u64, confirmed_bytes))?;
                return Ok(journal.branch);
            }
            Ok(None) => {
                return Err(SyncError::at(
                    SyncErrorStage::Verify,
                    SyncErrorCode::Git,
                    "git remote verification did not find the committed batch group",
                ));
            }
            Err(error) if error.retryable_in_attempt() && retries < options.max_retries => {
                retry_stage = error.stage();
                retries += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn retry_transient<T>(
    max_retries: u8,
    mut operation: impl FnMut() -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    let mut retries = 0_u8;
    loop {
        match operation() {
            Err(error) if error.retryable_in_attempt() && retries < max_retries => {
                retries += 1;
            }
            result => return result,
        }
    }
}

fn sync_group_attempt(
    root: &Path,
    batches: &[ReadyBatch],
    journal: &mut UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<Option<String>, SyncError> {
    let committed_cache = matches!(journal.phase, JournalPhase::Committed)
        && local_commit_available(root, journal, options, deadline)?;
    if !committed_cache {
        prepare_cache(root, journal, options, deadline)?;
    }
    if let Some(remote_oid) = verify_remote_group(root, batches, journal, options, deadline)? {
        return Ok(Some(remote_oid));
    }
    if matches!(journal.phase, JournalPhase::Committed) && !committed_cache {
        journal.phase = JournalPhase::Prepared;
        journal.commit_oid = None;
        write_journal(root, journal)?;
    }
    if !matches!(journal.phase, JournalPhase::Committed) {
        if ensure_group_shard_capacity(root, batches, journal, options, deadline)? {
            let _ = remove_managed_cache(root)?;
            return sync_group_attempt(root, batches, journal, options, deadline);
        }
        apply_group(root, batches, journal)
            .map_err(|error| error.with_stage(SyncErrorStage::Apply))?;
        let oid = commit_group(root, batches, journal, options, deadline)
            .map_err(|error| error.with_stage(SyncErrorStage::Commit))?;
        journal.phase = JournalPhase::Committed;
        journal.commit_oid = Some(oid);
        write_journal(root, journal)?;
    }
    let push = git_for(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [
            OsString::from("push"),
            OsString::from("origin"),
            OsString::from(format!("HEAD:refs/heads/{}", journal.branch)),
        ],
        options.output_limit,
        SyncErrorStage::Push,
        "push",
    );
    match verify_remote_group(root, batches, journal, options, deadline) {
        Ok(Some(remote_oid)) => Ok(Some(remote_oid)),
        Ok(None) => {
            if remote_ref_exists(root, &journal.remote, &journal.branch, options, deadline)? {
                journal.phase = JournalPhase::Prepared;
                journal.commit_oid = None;
                write_journal(root, journal)?;
                let _ = remove_managed_cache(root)?;
            }
            match push {
                Ok(_) => Err(SyncError::at(
                    SyncErrorStage::Verify,
                    SyncErrorCode::Git,
                    "git remote verification did not find the committed batch group",
                )),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn local_commit_available(
    root: &Path,
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<bool, SyncError> {
    if !repository_path(root).is_dir() {
        return Ok(false);
    }
    let ownership = match read_cache_ownership(root) {
        Ok(ownership)
            if ownership.remote_digest == journal.remote_digest
                && ownership.branch == journal.branch =>
        {
            ownership
        }
        Ok(_) => return Ok(false),
        Err(error) => return Err(error),
    };
    let _ = ownership;
    let Some(expected) = journal.commit_oid.as_deref() else {
        return Ok(false);
    };
    let output = git_allow_failure(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [OsString::from("rev-parse"), OsString::from("HEAD")],
        options.output_limit,
    )?;
    Ok(output.status.success() && parse_oid(&output.stdout).is_ok_and(|oid| oid == expected))
}

fn protect_confirmed_history(
    root: &Path,
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(), SyncError> {
    let state: ShardState = read_json(&shard_state_path(root, &journal.stream_id))?;
    if state.schema_version != STATE_SCHEMA_VERSION
        || state.stream_id != journal.stream_id
        || state.remote_digest != journal.remote_digest
        || state.branch != journal.branch
    {
        return Err(SyncError::new(
            SyncErrorCode::Conflict,
            "shard state does not match the pending upload journal",
        ));
    }
    let journal_confirms_remote = matches!(
        journal.phase,
        JournalPhase::RemoteConfirmed | JournalPhase::ReceiptDurable
    );
    if (state.remote_confirmed || journal_confirms_remote)
        && !remote_ref_exists(root, &journal.remote, &journal.branch, options, deadline)?
    {
        return Err(SyncError::new(
            SyncErrorCode::RemoteHistoryMissing,
            format!(
                "previously confirmed remote shard {} is missing",
                journal.branch
            ),
        ));
    }
    Ok(())
}

fn protect_durable_receipt_tip(
    root: &Path,
    journal: &UploadJournal,
    receipts: &[Receipt],
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(), SyncError> {
    let remote_oid = remote_ref_oid(root, &journal.remote, &journal.branch, options, deadline)?;
    let exact_tip = remote_oid.as_deref().is_some_and(|oid| {
        receipts
            .iter()
            .all(|receipt| receipt.remote_commit_oid == oid)
    });
    if !exact_tip {
        return Err(SyncError::at(
            SyncErrorStage::Recover,
            SyncErrorCode::RemoteHistoryMissing,
            "durably confirmed remote shard tip changed before source cleanup",
        ));
    }
    Ok(())
}

fn select_shard(
    root: &Path,
    batch: &ReadyBatch,
    options: &SyncOptions,
    deadline: Instant,
    remote_digest: &str,
) -> Result<ShardState, SyncError> {
    let month = utc_month(now_unix_seconds())?;
    let state_path = shard_state_path(root, &batch.manifest.stream.stream_id);
    if state_path.is_file() {
        let state: ShardState = read_json(&state_path)?;
        if state.schema_version == STATE_SCHEMA_VERSION
            && state.stream_id == batch.manifest.stream.stream_id
            && state.remote_digest == remote_digest
            && state.month == month
        {
            if state.remote_confirmed
                && !remote_ref_exists(
                    root,
                    assigned_remote(&batch.manifest)?,
                    &state.branch,
                    options,
                    deadline,
                )?
            {
                return Err(SyncError::new(
                    SyncErrorCode::RemoteHistoryMissing,
                    format!(
                        "previously recorded remote shard {} is missing",
                        state.branch
                    ),
                ));
            }
            return Ok(state);
        }
    }
    let prefix = format!(
        "logs/{}/{}/{month}",
        batch.manifest.stream.installation_id, batch.manifest.stream.stream_id
    );
    let index = discover_latest_shard(
        root,
        assigned_remote(&batch.manifest)?,
        &prefix,
        options,
        deadline,
    )?
    .unwrap_or(1);
    let state = ShardState {
        schema_version: STATE_SCHEMA_VERSION,
        stream_id: batch.manifest.stream.stream_id.clone(),
        remote_digest: remote_digest.to_owned(),
        month,
        index,
        branch: format!("{prefix}/{index:04}"),
        remote_confirmed: index > 1
            || remote_ref_exists(
                root,
                assigned_remote(&batch.manifest)?,
                &format!("{prefix}/{index:04}"),
                options,
                deadline,
            )?,
    };
    fs::create_dir_all(state_path.parent().expect("shard state has parent"))?;
    atomic_write_json(&state_path, &state)
        .map_err(|_| SyncError::new(SyncErrorCode::Io, "shard state write failed"))?;
    Ok(state)
}

fn prepare_cache(
    root: &Path,
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(), SyncError> {
    let repository = repository_path(root);
    if repository.exists() {
        match read_cache_ownership(root) {
            Ok(ownership)
                if ownership.remote_digest == journal.remote_digest
                    && ownership.branch == journal.branch =>
            {
                if remote_ref_exists(root, &journal.remote, &journal.branch, options, deadline)? {
                    fetch_exact(root, &journal.branch, options, deadline)?;
                    checkout_fetch_head(root, options, deadline)?;
                    return Ok(());
                }
                let _ = remove_managed_cache(root)?;
            }
            Ok(_) => {
                let _ = remove_managed_cache(root)?;
            }
            Err(error) => return Err(error),
        }
    }
    cleanup_cache_staging(root, journal)?;
    let staging = cache_staging_path(root);
    fs::create_dir(&staging)?;
    write_cache_ownership_at(&staging.join("ownership.json"), root, journal)?;
    let worktree = staging.join("worktree");
    let branch_exists =
        remote_ref_exists(root, &journal.remote, &journal.branch, options, deadline)?;
    if branch_exists {
        git(
            root,
            options,
            deadline,
            Some(&staging),
            [
                OsString::from("clone"),
                OsString::from("--depth=1"),
                OsString::from("--single-branch"),
                OsString::from("--no-tags"),
                OsString::from("--branch"),
                OsString::from(&journal.branch),
                OsString::from("--"),
                OsString::from(&journal.remote),
                worktree.as_os_str().to_owned(),
            ],
            options.output_limit,
        )?;
    } else {
        fs::create_dir(&worktree)?;
        git(
            root,
            options,
            deadline,
            Some(&worktree),
            [OsString::from("init")],
            options.output_limit,
        )?;
        git(
            root,
            options,
            deadline,
            Some(&worktree),
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                OsString::from(&journal.remote),
            ],
            options.output_limit,
        )?;
        git(
            root,
            options,
            deadline,
            Some(&worktree),
            [
                OsString::from("checkout"),
                OsString::from("--orphan"),
                OsString::from("upload"),
            ],
            options.output_limit,
        )?;
    }
    write_cache_ownership_at(&worktree.join(".sctx-log-cache.json"), root, journal)?;
    fs::rename(&worktree, &repository)?;
    fs::remove_dir_all(&staging)?;
    File::open(root)?.sync_all()?;
    ensure_storage_headroom_after_git(root, options)?;
    Ok(())
}

fn fetch_exact(
    root: &Path,
    branch: &str,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(), SyncError> {
    git_for(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [
            OsString::from("fetch"),
            OsString::from("--depth=1"),
            OsString::from("--no-tags"),
            OsString::from("origin"),
            OsString::from(format!("refs/heads/{branch}")),
        ],
        options.output_limit,
        SyncErrorStage::Fetch,
        "fetch",
    )?;
    Ok(())
}

fn checkout_fetch_head(
    root: &Path,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(), SyncError> {
    git(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [
            OsString::from("checkout"),
            OsString::from("--detach"),
            OsString::from("FETCH_HEAD"),
        ],
        options.output_limit,
    )?;
    Ok(())
}

fn ensure_group_shard_capacity(
    root: &Path,
    batches: &[ReadyBatch],
    journal: &mut UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<bool, SyncError> {
    let (bytes, files) = tree_size(root, options, deadline)?;
    let incoming = group_payload_bytes(batches)?;
    let incoming_files = u64::try_from(batches.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2);
    if bytes.saturating_add(incoming) <= options.shard_max_bytes
        && files.saturating_add(incoming_files) <= options.shard_max_files
    {
        return Ok(false);
    }
    if files == 0 {
        return Err(SyncError::new(
            SyncErrorCode::StorageBudget,
            "sealed batch exceeds the configured shard capacity",
        ));
    }
    rotate_journal_shard(root, journal, options, deadline)?;
    Ok(true)
}

fn rotate_journal_shard(
    root: &Path,
    journal: &mut UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(), SyncError> {
    let state_path = shard_state_path(root, &journal.stream_id);
    let mut state: ShardState = read_json(&state_path)?;
    state.index = state.index.saturating_add(1);
    let prefix = state
        .branch
        .rsplit_once('/')
        .map_or(state.branch.as_str(), |(prefix, _)| prefix)
        .to_owned();
    state.branch = format!("{prefix}/{:04}", state.index);
    state.remote_confirmed = false;
    if remote_ref_exists(root, &journal.remote, &state.branch, options, deadline)? {
        return Err(SyncError::new(
            SyncErrorCode::Conflict,
            "next shard already exists unexpectedly",
        ));
    }
    atomic_write_json(&state_path, &state)
        .map_err(|_| SyncError::new(SyncErrorCode::Io, "shard state write failed"))?;
    journal.branch.clone_from(&state.branch);
    journal.phase = JournalPhase::Prepared;
    journal.commit_oid = None;
    write_journal(root, journal)
}

fn tree_size(
    root: &Path,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<(u64, u64), SyncError> {
    let repository = repository_path(root);
    let output = git_allow_failure(
        root,
        options,
        deadline,
        Some(&repository),
        [
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-l"),
            OsString::from("HEAD"),
        ],
        1024 * 1024,
    )?;
    if !output.status.success() {
        return Ok((0, 0));
    }
    if output.stdout_truncated {
        return Err(SyncError::new(
            SyncErrorCode::StorageBudget,
            "shard tree listing exceeded its bound; capacity cannot be verified",
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    for line in text.lines() {
        let Some((metadata, _path)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = metadata.split_whitespace();
        let _mode = fields.next();
        let kind = fields.next();
        let _oid = fields.next();
        let size = fields.next().and_then(|value| value.parse::<u64>().ok());
        if kind == Some("blob") {
            files += 1;
            bytes = bytes.saturating_add(size.unwrap_or(0));
        }
    }
    Ok((bytes, files))
}

fn apply_group(
    root: &Path,
    batches: &[ReadyBatch],
    journal: &UploadJournal,
) -> Result<(), SyncError> {
    let repository = repository_path(root);
    let entries = journal_entries(journal);
    for (batch, entry) in batches.iter().zip(&entries) {
        for (source, relative) in [
            (&batch.events_path, &entry.events_relative_path),
            (&batch.manifest_path, &entry.manifest_relative_path),
        ] {
            let destination = safe_repository_path(&repository, relative)?;
            ensure_safe_parents(
                &repository,
                destination.parent().expect("batch path has parent"),
            )?;
            fs::create_dir_all(destination.parent().expect("batch path has parent"))?;
            if destination.exists() {
                let metadata = destination.symlink_metadata()?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(SyncError::new(
                        SyncErrorCode::Conflict,
                        "cache batch path is not a regular file",
                    ));
                }
                let existing = sha256_file(&destination)?;
                let expected = sha256_file(source)?;
                if existing == expected {
                    continue;
                }
            }
            atomic_copy(source, &destination)?;
        }
    }
    Ok(())
}

fn atomic_copy(source: &Path, destination: &Path) -> Result<(), SyncError> {
    let parent = destination
        .parent()
        .ok_or_else(|| SyncError::new(SyncErrorCode::InvalidBatch, "batch path has no parent"))?;
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| SyncError::new(SyncErrorCode::InvalidBatch, "invalid batch filename"))?;
    let temporary = parent.join(format!(".{name}.sctx-uploading"));
    if temporary.exists() {
        let metadata = temporary.symlink_metadata()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "batch temporary path is not a regular managed file",
            ));
        }
        fs::remove_file(&temporary)?;
    }
    let result = (|| {
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, destination)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn commit_group(
    root: &Path,
    batches: &[ReadyBatch],
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<String, SyncError> {
    let repository = repository_path(root);
    let entries = journal_entries(journal);
    let pathspec = root.join("state/sync-pathspec");
    let mut pathspec_bytes = Vec::new();
    for entry in &entries {
        pathspec_bytes.extend_from_slice(entry.events_relative_path.as_bytes());
        pathspec_bytes.push(0);
        pathspec_bytes.extend_from_slice(entry.manifest_relative_path.as_bytes());
        pathspec_bytes.push(0);
    }
    let mut pathspec_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&pathspec)?;
    pathspec_file.write_all(&pathspec_bytes)?;
    pathspec_file.sync_all()?;
    drop(pathspec_file);
    let mut pathspec_arg = OsString::from("--pathspec-from-file=");
    pathspec_arg.push(&pathspec);
    let add_result = git_for(
        root,
        options,
        deadline,
        Some(&repository),
        [
            OsString::from("add"),
            pathspec_arg,
            OsString::from("--pathspec-file-nul"),
        ],
        options.output_limit,
        SyncErrorStage::Commit,
        "add",
    );
    let _ = fs::remove_file(&pathspec);
    add_result?;
    git_with_identity(
        root,
        options,
        deadline,
        &repository,
        assigned_email(&batches[0].manifest)?,
        [
            OsString::from("commit"),
            OsString::from("-m"),
            OsString::from(format!("logs: {} batches", batches.len())),
        ],
    )
    .map_err(|error| error.with_stage(SyncErrorStage::Commit))?;
    let output = git_for(
        root,
        options,
        deadline,
        Some(&repository),
        [OsString::from("rev-parse"), OsString::from("HEAD")],
        options.output_limit,
        SyncErrorStage::Commit,
        "rev-parse",
    )?;
    parse_oid(&output.stdout)
}

fn verify_remote_group(
    root: &Path,
    batches: &[ReadyBatch],
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<Option<String>, SyncError> {
    if !remote_ref_exists(root, &journal.remote, &journal.branch, options, deadline)
        .map_err(|error| error.with_stage(SyncErrorStage::Verify))?
    {
        return Ok(None);
    }
    fetch_exact(root, &journal.branch, options, deadline)
        .map_err(|error| error.with_stage(SyncErrorStage::Fetch))?;
    let remote_oid = git_for(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [OsString::from("rev-parse"), OsString::from("FETCH_HEAD")],
        options.output_limit,
        SyncErrorStage::Verify,
        "rev-parse",
    )?;
    let remote_oid = parse_oid(&remote_oid.stdout)?;
    let entries = journal_entries(journal);
    let mut missing = false;
    for (batch, entry) in batches.iter().zip(&entries) {
        let events = git_allow_failure(
            root,
            options,
            deadline,
            Some(&repository_path(root)),
            [
                OsString::from("show"),
                OsString::from(format!("FETCH_HEAD:{}", entry.events_relative_path)),
            ],
            usize::try_from(batch.manifest.content_bytes)
                .unwrap_or(usize::MAX)
                .saturating_add(1),
        )
        .map_err(|error| error.with_stage(SyncErrorStage::Verify))?;
        let manifest = git_allow_failure(
            root,
            options,
            deadline,
            Some(&repository_path(root)),
            [
                OsString::from("show"),
                OsString::from(format!("FETCH_HEAD:{}", entry.manifest_relative_path)),
            ],
            1024 * 1024,
        )
        .map_err(|error| error.with_stage(SyncErrorStage::Verify))?;
        if !events.status.success() {
            if manifest.status.success() {
                return Err(SyncError::new(
                    SyncErrorCode::Conflict,
                    "the remote contains only part of a batch path pair",
                ));
            }
            missing = true;
            continue;
        }
        if events.stdout_truncated || sha256_bytes(&events.stdout) != batch.manifest.content_sha256
        {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "the remote JSONL path exists with different content",
            ));
        }
        if !manifest.status.success() || manifest.stdout_truncated {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "the remote batch manifest is missing or oversized",
            ));
        }
        let remote_manifest: BatchManifest =
            serde_json::from_slice(&manifest.stdout).map_err(|_| {
                SyncError::new(
                    SyncErrorCode::Conflict,
                    "the remote batch manifest is invalid",
                )
            })?;
        if remote_manifest != batch.manifest {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "the remote batch manifest does not match the sealed batch",
            ));
        }
    }
    Ok((!missing).then_some(remote_oid))
}

fn finish_confirmed_group(
    root: &Path,
    batches: &[ReadyBatch],
    journal: &mut UploadJournal,
    remote_oid: &str,
) -> Result<(), SyncError> {
    journal.phase = JournalPhase::RemoteConfirmed;
    journal.commit_oid = Some(remote_oid.to_owned());
    write_journal(root, journal).map_err(|error| error.with_stage(SyncErrorStage::Receipt))?;
    let entries = journal_entries(journal);
    for (index, entry) in entries.iter().enumerate() {
        let receipt = Receipt {
            schema_version: STATE_SCHEMA_VERSION,
            batch_id: entry.batch_id.clone(),
            stream_id: journal.stream_id.clone(),
            remote_digest: journal.remote_digest.clone(),
            branch: journal.branch.clone(),
            content_sha256: entry.content_sha256.clone(),
            remote_commit_oid: remote_oid.to_owned(),
            confirmed_at_unix_ms: now_unix_millis(),
        };
        let path = receipt_path(root, &entry.batch_id);
        fs::create_dir_all(path.parent().expect("receipt has parent"))?;
        atomic_write_json(&path, &receipt).map_err(|_| {
            SyncError::at(
                SyncErrorStage::Receipt,
                SyncErrorCode::Io,
                "receipt write failed",
            )
        })?;
        if let Some(stored) = journal.batches.get_mut(index) {
            stored.receipt_durable = true;
            write_journal(root, journal)
                .map_err(|error| error.with_stage(SyncErrorStage::Receipt))?;
        }
    }
    journal.phase = JournalPhase::ReceiptDurable;
    write_journal(root, journal).map_err(|error| error.with_stage(SyncErrorStage::Receipt))?;
    mark_shard_remote_confirmed(root, journal)
        .map_err(|error| error.with_stage(SyncErrorStage::Receipt))?;
    for batch in batches {
        delete_confirmed_batch(batch).map_err(|error| error.with_stage(SyncErrorStage::Cleanup))?;
    }
    remove_journal(root).map_err(|error| error.with_stage(SyncErrorStage::Cleanup))?;
    Ok(())
}

fn delete_confirmed_batch(batch: &ReadyBatch) -> Result<(), SyncError> {
    if batch.directory.symlink_metadata()?.file_type().is_symlink() {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "ready batch directory is a symlink",
        ));
    }
    fs::remove_dir_all(&batch.directory)?;
    if let Some(parent) = batch.directory.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn remote_ref_exists(
    root: &Path,
    remote: &str,
    branch: &str,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<bool, SyncError> {
    Ok(remote_ref_oid(root, remote, branch, options, deadline)?.is_some())
}

fn remote_ref_oid(
    root: &Path,
    remote: &str,
    branch: &str,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<Option<String>, SyncError> {
    let output = git_allow_failure(
        root,
        options,
        deadline,
        Some(root),
        [
            OsString::from("ls-remote"),
            OsString::from("--refs"),
            OsString::from("--heads"),
            OsString::from("--"),
            OsString::from(remote),
            OsString::from(format!("refs/heads/{branch}")),
        ],
        options.output_limit,
    )?;
    if !output.status.success() {
        return Err(git_failure(
            SyncErrorStage::Verify,
            "ls-remote",
            output.status.code(),
            &output.stderr,
        ));
    }
    if output.stdout.is_empty() {
        return Ok(None);
    }
    let oid = output
        .stdout
        .split(u8::is_ascii_whitespace)
        .next()
        .unwrap_or_default();
    parse_oid(oid).map(Some)
}

fn discover_latest_shard(
    root: &Path,
    remote: &str,
    prefix: &str,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<Option<u32>, SyncError> {
    let output = git_allow_failure(
        root,
        options,
        deadline,
        Some(root),
        [
            OsString::from("ls-remote"),
            OsString::from("--refs"),
            OsString::from("--heads"),
            OsString::from("--"),
            OsString::from(remote),
            OsString::from(format!("refs/heads/{prefix}/*")),
        ],
        1024 * 1024,
    )?;
    if !output.status.success() {
        return Err(git_failure(
            SyncErrorStage::Discover,
            "ls-remote",
            output.status.code(),
            &output.stderr,
        ));
    }
    let suffix_prefix = format!("refs/heads/{prefix}/");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, reference)| reference))
        .filter_map(|reference| reference.strip_prefix(&suffix_prefix))
        .filter_map(|suffix| suffix.parse::<u32>().ok())
        .max())
}

fn git<I>(
    root: &Path,
    options: &SyncOptions,
    deadline: Instant,
    current_dir: Option<&Path>,
    args: I,
    output_limit: usize,
) -> Result<crate::runner::CommandOutput, SyncError>
where
    I: IntoIterator<Item = OsString>,
{
    git_for(
        root,
        options,
        deadline,
        current_dir,
        args,
        output_limit,
        SyncErrorStage::Prepare,
        "command",
    )
}

#[allow(clippy::too_many_arguments)]
fn git_for<I>(
    root: &Path,
    options: &SyncOptions,
    deadline: Instant,
    current_dir: Option<&Path>,
    args: I,
    output_limit: usize,
    stage: SyncErrorStage,
    operation: &'static str,
) -> Result<crate::runner::CommandOutput, SyncError>
where
    I: IntoIterator<Item = OsString>,
{
    let output = git_allow_failure(root, options, deadline, current_dir, args, output_limit)
        .map_err(|error| error.with_stage(stage))?;
    if output.status.success() {
        if cache_marker_path(root).is_file() {
            ensure_storage_headroom_after_git(root, options)?;
        }
        Ok(output)
    } else {
        Err(git_failure(
            stage,
            operation,
            output.status.code(),
            &output.stderr,
        ))
    }
}

fn git_allow_failure<I>(
    root: &Path,
    options: &SyncOptions,
    deadline: Instant,
    current_dir: Option<&Path>,
    args: I,
    output_limit: usize,
) -> Result<crate::runner::CommandOutput, SyncError>
where
    I: IntoIterator<Item = OsString>,
{
    ensure_before_deadline(deadline)?;
    let mut all_args = vec![
        OsString::from("-c"),
        OsString::from("core.hooksPath=/dev/null"),
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from("commit.gpgSign=false"),
        OsString::from("-c"),
        OsString::from("tag.gpgSign=false"),
        OsString::from("-c"),
        OsString::from("protocol.ext.allow=never"),
    ];
    all_args.extend(args);
    let mut spec = CommandSpec::new(&options.git_binary, Duration::ZERO)
        .args(all_args)
        .deadline(deadline)
        .output_limit(output_limit)
        .disk_guard(DiskGuard {
            root: root.to_path_buf(),
            max_bytes: options.local_budget_bytes,
            min_free_bytes: options.min_free_disk_bytes,
        });
    if let Some(directory) = current_dir {
        spec = spec.current_dir(directory);
    }
    for (key, value) in git_environment() {
        spec = spec.env(key, value);
    }
    run(&spec).map_err(|error| runner_error(&error))
}

fn git_with_identity<I>(
    root: &Path,
    options: &SyncOptions,
    deadline: Instant,
    current_dir: &Path,
    email: &str,
    args: I,
) -> Result<(), SyncError>
where
    I: IntoIterator<Item = OsString>,
{
    let identity = [
        OsString::from("-c"),
        OsString::from("user.name=Shared Context Logs"),
        OsString::from("-c"),
        OsString::from(format!("user.email={email}")),
    ];
    let args = identity.into_iter().chain(args);
    git(
        root,
        options,
        deadline,
        Some(current_dir),
        args,
        options.output_limit,
    )?;
    Ok(())
}

fn runner_error(error: &RunnerError) -> SyncError {
    match error {
        RunnerError::TimedOut { .. } => SyncError::new(
            SyncErrorCode::Timeout,
            "Git exceeded the synchronization deadline",
        ),
        RunnerError::ResourceLimit { .. } => SyncError::new(
            SyncErrorCode::StorageBudget,
            "Git was stopped after reaching the local storage guard; pending spool was preserved",
        ),
        RunnerError::Spawn(_) => SyncError::at(
            SyncErrorStage::Prepare,
            SyncErrorCode::Git,
            "git process could not start",
        ),
        RunnerError::Wait(_) | RunnerError::Output(_) => SyncError::at(
            SyncErrorStage::Prepare,
            SyncErrorCode::Git,
            "git process execution failed",
        ),
    }
}

fn git_failure(
    stage: SyncErrorStage,
    operation: &str,
    exit_code: Option<i32>,
    stderr: &[u8],
) -> SyncError {
    let kind = classify_git_failure(stderr);
    let detail = exit_code.map_or_else(
        || format!("git {operation} terminated without an exit code ({kind})"),
        |code| format!("git {operation} exited with code {code} ({kind})"),
    );
    SyncError::at(stage, SyncErrorCode::Git, detail)
        .with_local_diagnostic(local_git_diagnostic(stderr))
}

/// Git's own last words, reduced to something safe to keep in a local status file.
///
/// The classifier above answers "which closed class is this"; when the answer is `unknown`, the
/// status file records a failure nobody can act on. This keeps the sentences Git wrote, which is
/// what an operator reads to decide whether a remote is gone, a credential expired, or a network
/// was down.
///
/// Three bounds, because this is durable state written by a background daemon: only the last few
/// lines (Git puts its conclusion last, and progress output first), no control characters (the
/// file is read by humans and by JSON parsers), and a hard character ceiling.
fn local_git_diagnostic(stderr: &[u8]) -> Option<String> {
    bounded_tool_diagnostic(stderr)
}

/// The last words of a failed tool, reduced to something safe to keep in a local status file.
///
/// Every place that runs a child process and then records only its own summary of the failure
/// throws this away at the moment it exists, and no later run can recover it. `sctx maintain` did
/// exactly that with `sctx logs sync`'s stderr, and its digest reported
/// `logging synchronization exited unsuccessfully` for a week.
///
/// Three bounds, because this is durable state written by a background daemon: only the last few
/// lines (a tool puts its conclusion last and its progress first), no control characters (the
/// result is read by humans and embedded in JSON and log lines), and a hard character ceiling.
///
/// The result is **local only**. It is not a closed vocabulary, so it must never be copied into a
/// telemetry Event or anything else that leaves the host.
#[must_use]
pub fn bounded_tool_diagnostic(stderr: &[u8]) -> Option<String> {
    const MAX_LINES: usize = 3;
    const MAX_CHARS: usize = 240;
    let text = String::from_utf8_lossy(stderr);
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    if lines.len() > MAX_LINES {
        lines.drain(..lines.len() - MAX_LINES);
    }
    let joined: String = lines
        .join("; ")
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_CHARS)
        .collect();
    (!joined.is_empty()).then_some(joined)
}

fn classify_git_failure(stderr: &[u8]) -> &'static str {
    let detail = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if detail.contains("permission denied")
        || detail.contains("authentication failed")
        || detail.contains("could not read username")
        || detail.contains("publickey")
    {
        "authentication_or_permission"
    } else if detail.contains("could not resolve host")
        || detail.contains("name or service not known")
        || detail.contains("nodename nor servname")
    {
        "dns"
    } else if detail.contains("timed out") || detail.contains("operation timeout") {
        "connection_timeout"
    } else if detail.contains("connection refused") || detail.contains("could not connect") {
        "connection_refused"
    } else if detail.contains("non-fast-forward") || detail.contains("fetch first") {
        "non_fast_forward"
    } else if detail.contains("protected branch")
        || detail.contains("pre-receive hook declined")
        || detail.contains("remote rejected")
    {
        "protected_branch_or_pre_receive"
    } else if detail.contains("shallow") {
        "shallow_repository"
    } else if detail.contains("index.lock")
        || detail.contains("another git process")
        || (detail.contains("unable to create") && detail.contains(".lock"))
    {
        "local_lock"
    } else if detail.contains("no space left") || detail.contains("disk full") {
        "disk_full"
    } else {
        "unknown"
    }
}

const fn default_error_stage(code: SyncErrorCode) -> SyncErrorStage {
    match code {
        SyncErrorCode::InvalidRoot | SyncErrorCode::InvalidBatch | SyncErrorCode::InvalidRemote => {
            SyncErrorStage::Validate
        }
        SyncErrorCode::Conflict
        | SyncErrorCode::RemoteHistoryMissing
        | SyncErrorCode::PendingJournal => SyncErrorStage::Recover,
        SyncErrorCode::StorageBudget => SyncErrorStage::Storage,
        SyncErrorCode::Io => SyncErrorStage::Cleanup,
        SyncErrorCode::Git | SyncErrorCode::Timeout => SyncErrorStage::Prepare,
    }
}

fn bounded_safe_detail(mut detail: String) -> String {
    const MAX_CHARS: usize = 256;
    if detail.chars().count() > MAX_CHARS {
        detail = detail.chars().take(MAX_CHARS).collect();
    }
    detail
}

const fn safe_io_detail(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "required filesystem entry was not found",
        io::ErrorKind::PermissionDenied => "filesystem permission was denied",
        io::ErrorKind::AlreadyExists => "filesystem entry already exists",
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            "filesystem data or input was invalid"
        }
        io::ErrorKind::TimedOut => "filesystem operation timed out",
        io::ErrorKind::WriteZero | io::ErrorKind::StorageFull => {
            "filesystem could not persist more data"
        }
        _ => "filesystem operation failed",
    }
}

fn validate_options(options: &SyncOptions) -> Result<(), SyncError> {
    if options.deadline.is_zero()
        || options.deadline > DEFAULT_DEADLINE
        || options.max_new_payload_bytes == 0
        || options.shard_max_bytes == 0
        || options.shard_max_files < 2
        || options.output_limit == 0
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "invalid synchronization limits",
        ));
    }
    Ok(())
}

fn validate_root(root: &Path) -> Result<PathBuf, SyncError> {
    let metadata = root.symlink_metadata().map_err(|error| {
        SyncError::new(SyncErrorCode::InvalidRoot, safe_io_detail(error.kind()))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "logs root must be a non-symlink directory",
        ));
    }
    root.canonicalize().map_err(SyncError::from)
}

fn validate_remote(remote: &str) -> Result<(), SyncError> {
    let lower = remote.to_ascii_lowercase();
    let valid_scheme = lower.starts_with("https://") || lower.starts_with("ssh://");
    let scp_like = !remote.contains("://")
        && remote
            .split_once(':')
            .is_some_and(|(host, path)| !host.is_empty() && !path.is_empty() && host.contains('@'));
    if remote.starts_with('-')
        || lower.starts_with("ext::")
        || remote.chars().any(char::is_control)
        || !(valid_scheme
            || scp_like
            || lower
                .strip_prefix("file://")
                .is_some_and(|path| Path::new(path).is_absolute() && !path.contains(".."))
            || Path::new(remote).is_absolute()
                && !remote.split('/').any(|component| component == ".."))
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRemote,
            "remote must be an HTTPS, SSH, or SCP-like Git URL",
        ));
    }
    Ok(())
}

fn validate_batch_identity(manifest: &BatchManifest) -> Result<(), SyncError> {
    for (name, value) in [
        ("batch id", manifest.batch_id.as_str()),
        ("stream id", manifest.stream.stream_id.as_str()),
        ("installation id", manifest.stream.installation_id.as_str()),
    ] {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(SyncError::new(
                SyncErrorCode::InvalidBatch,
                format!("{name} is not a safe Git path component"),
            ));
        }
    }
    if manifest.content_sha256.len() != 64
        || !manifest
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "batch content hash is invalid",
        ));
    }
    let _ = assigned_email(manifest)?;
    let _ = assigned_remote(manifest)?;
    Ok(())
}

fn assigned_email(manifest: &BatchManifest) -> Result<&str, SyncError> {
    manifest.stream.email.as_deref().ok_or_else(|| {
        SyncError::new(
            SyncErrorCode::InvalidBatch,
            "sealed batch has not been assigned an upload email",
        )
    })
}

fn assigned_remote(manifest: &BatchManifest) -> Result<&str, SyncError> {
    manifest.stream.remote.as_deref().ok_or_else(|| {
        SyncError::new(
            SyncErrorCode::InvalidBatch,
            "sealed batch has not been assigned an upload remote",
        )
    })
}

fn batch_paths(manifest: &BatchManifest) -> Result<(String, String), SyncError> {
    let day = utc_day(manifest.first_event_unix_ms.div_euclid(1000))?;
    let email = encode_component(assigned_email(manifest)?);
    let base = format!(
        "users/{email}/{}/{day}/{}",
        manifest.stream.installation_id, manifest.batch_id
    );
    Ok((format!("{base}.jsonl"), format!("{base}.manifest.json")))
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn safe_repository_path(repository: &Path, relative: &str) -> Result<PathBuf, SyncError> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "batch destination is not a safe relative path",
        ));
    }
    Ok(repository.join(relative))
}

fn ensure_safe_parents(repository: &Path, destination_parent: &Path) -> Result<(), SyncError> {
    let relative = destination_parent.strip_prefix(repository).map_err(|_| {
        SyncError::new(
            SyncErrorCode::InvalidBatch,
            "batch destination escaped cache",
        )
    })?;
    let mut path = repository.to_path_buf();
    for component in relative.components() {
        path.push(component);
        if path.exists() && path.symlink_metadata()?.file_type().is_symlink() {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "remote tree contains a symlink in the batch destination",
            ));
        }
    }
    Ok(())
}

fn acquire_lock(root: &Path) -> Result<Option<SyncLock>, SyncError> {
    let state = root.join("state");
    fs::create_dir_all(&state)?;
    let path = state.join("sync.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(SyncLock { file })),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn journal_path(root: &Path) -> PathBuf {
    root.join("state/upload-journal.json")
}

fn load_journal(root: &Path) -> Result<Option<UploadJournal>, SyncError> {
    let path = journal_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let journal: UploadJournal = read_json(&path)?;
    if journal.schema_version != STATE_SCHEMA_VERSION {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "unsupported upload journal version",
        ));
    }
    let entries = journal_entries(&journal);
    if entries.is_empty() || entries.len() > MAX_GROUP_BATCHES {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "upload journal batch group exceeds its bound",
        ));
    }
    if !journal.batches.is_empty()
        && entries.first().is_some_and(|first| {
            first.batch_id != journal.batch_id
                || first.events_relative_path != journal.events_relative_path
                || first.manifest_relative_path != journal.manifest_relative_path
                || first.content_bytes != journal.content_bytes
                || first.content_sha256 != journal.content_sha256
        })
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "upload journal compatibility fields do not match its first batch",
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    for entry in &entries {
        if !ids.insert(&entry.batch_id)
            || entry.batch_id.is_empty()
            || entry.batch_id.len() > 128
            || !entry
                .batch_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || entry.content_sha256.len() != 64
            || !entry
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SyncError::new(
                SyncErrorCode::InvalidBatch,
                "upload journal contains an invalid batch entry",
            ));
        }
    }
    Ok(Some(journal))
}

fn journal_entries(journal: &UploadJournal) -> Vec<JournalBatch> {
    if journal.batches.is_empty() {
        vec![JournalBatch {
            batch_id: journal.batch_id.clone(),
            events_relative_path: journal.events_relative_path.clone(),
            manifest_relative_path: journal.manifest_relative_path.clone(),
            content_bytes: journal.content_bytes,
            content_sha256: journal.content_sha256.clone(),
            receipt_durable: matches!(journal.phase, JournalPhase::ReceiptDurable),
        }]
    } else {
        journal.batches.clone()
    }
}

fn validate_journal_batch(
    journal: &UploadJournal,
    entry: &JournalBatch,
    batch: &ReadyBatch,
) -> Result<(), SyncError> {
    let (events_relative_path, manifest_relative_path) = batch_paths(&batch.manifest)?;
    if entry.batch_id != batch.manifest.batch_id
        || journal.stream_id != batch.manifest.stream.stream_id
        || journal.remote != batch.manifest.stream.remote.as_deref().unwrap_or_default()
        || entry.events_relative_path != events_relative_path
        || entry.manifest_relative_path != manifest_relative_path
        || entry.content_bytes != batch.manifest.content_bytes
        || entry.content_sha256 != batch.manifest.content_sha256
    {
        return Err(SyncError::new(
            SyncErrorCode::Conflict,
            "upload journal does not match the sealed batch group",
        ));
    }
    Ok(())
}

fn write_journal(root: &Path, journal: &UploadJournal) -> Result<(), SyncError> {
    atomic_write_json(&journal_path(root), journal)
        .map_err(|_| SyncError::new(SyncErrorCode::Io, "upload journal write failed"))
}

fn remove_journal(root: &Path) -> Result<(), SyncError> {
    let path = journal_path(root);
    if path.exists() {
        fs::remove_file(path)?;
        File::open(root.join("state"))?.sync_all()?;
    }
    Ok(())
}

fn receipt_path(root: &Path, batch_id: &str) -> PathBuf {
    root.join("state/receipts").join(format!("{batch_id}.json"))
}

fn load_valid_receipt_entry(
    root: &Path,
    journal: &UploadJournal,
    entry: &JournalBatch,
) -> Result<Receipt, SyncError> {
    let receipt: Receipt = read_json(&receipt_path(root, &entry.batch_id))?;
    if receipt.schema_version != STATE_SCHEMA_VERSION
        || receipt.batch_id != entry.batch_id
        || receipt.stream_id != journal.stream_id
        || receipt.remote_digest != journal.remote_digest
        || receipt.branch != journal.branch
        || receipt.content_sha256 != entry.content_sha256
        || parse_oid(receipt.remote_commit_oid.as_bytes()).is_err()
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidBatch,
            "durable receipt does not match the upload journal",
        ));
    }
    Ok(receipt)
}

fn shard_state_path(root: &Path, stream_id: &str) -> PathBuf {
    root.join("state/shards").join(format!("{stream_id}.json"))
}

fn mark_shard_remote_confirmed(root: &Path, journal: &UploadJournal) -> Result<(), SyncError> {
    let path = shard_state_path(root, &journal.stream_id);
    let mut state: ShardState = read_json(&path)?;
    if state.branch != journal.branch || state.remote_digest != journal.remote_digest {
        return Err(SyncError::new(
            SyncErrorCode::Conflict,
            "shard state changed while the upload was active",
        ));
    }
    state.remote_confirmed = true;
    atomic_write_json(&path, &state)
        .map_err(|_| SyncError::new(SyncErrorCode::Io, "shard state write failed"))
}

fn repository_path(root: &Path) -> PathBuf {
    root.join("repository")
}

fn cache_staging_path(root: &Path) -> PathBuf {
    root.join("repository.staging")
}

fn cache_marker_path(root: &Path) -> PathBuf {
    repository_path(root).join(".sctx-log-cache.json")
}

fn write_cache_ownership_at(
    path: &Path,
    root: &Path,
    journal: &UploadJournal,
) -> Result<(), SyncError> {
    let ownership = CacheOwnership {
        schema_version: STATE_SCHEMA_VERSION,
        canonical_logs_root: root.to_string_lossy().into_owned(),
        remote_digest: journal.remote_digest.clone(),
        branch: journal.branch.clone(),
        created_at_unix_ms: now_unix_millis(),
    };
    atomic_write_json(path, &ownership)
        .map_err(|_| SyncError::new(SyncErrorCode::Io, "cache ownership write failed"))
}

fn cleanup_cache_staging(root: &Path, journal: &UploadJournal) -> Result<(), SyncError> {
    let staging = cache_staging_path(root);
    if !staging.exists() {
        return Ok(());
    }
    let metadata = staging.symlink_metadata()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "cache staging path is not a managed directory",
        ));
    }
    let ownership: CacheOwnership = read_json(&staging.join("ownership.json"))?;
    if ownership.schema_version != STATE_SCHEMA_VERSION
        || ownership.canonical_logs_root != root.to_string_lossy()
        || ownership.remote_digest != journal.remote_digest
        || ownership.branch != journal.branch
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "cache staging ownership does not match the pending upload",
        ));
    }
    fs::remove_dir_all(&staging)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn read_cache_ownership(root: &Path) -> Result<CacheOwnership, SyncError> {
    let repository = repository_path(root);
    let repository_metadata = repository.symlink_metadata()?;
    let git = repository.join(".git");
    let git_metadata = git.symlink_metadata().map_err(|_| {
        SyncError::new(
            SyncErrorCode::InvalidRoot,
            "managed cache has no in-tree .git directory",
        )
    })?;
    if repository_metadata.file_type().is_symlink()
        || !repository_metadata.is_dir()
        || git_metadata.file_type().is_symlink()
        || !git_metadata.is_dir()
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "managed cache is a symlink or uses an external gitdir",
        ));
    }
    let canonical_repository = repository.canonicalize()?;
    let canonical_git = git.canonicalize()?;
    if !canonical_repository.starts_with(root) || !canonical_git.starts_with(&canonical_repository)
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "managed cache resolves outside the logs root",
        ));
    }
    let ownership: CacheOwnership = read_json(&cache_marker_path(root))?;
    if ownership.schema_version != STATE_SCHEMA_VERSION
        || ownership.canonical_logs_root != root.to_string_lossy()
    {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "cache ownership marker does not match this logs root",
        ));
    }
    Ok(ownership)
}

fn remove_managed_cache(root: &Path) -> Result<PruneReport, SyncError> {
    let repository = repository_path(root);
    if !repository.exists() {
        return Ok(PruneReport {
            removed: false,
            bytes_reclaimed: 0,
        });
    }
    let _ownership = read_cache_ownership(root)?;
    let bytes = directory_size(&repository)?;
    fs::remove_dir_all(&repository)?;
    File::open(root)?.sync_all()?;
    Ok(PruneReport {
        removed: true,
        bytes_reclaimed: bytes,
    })
}

fn prune_expired_cache_locked(root: &Path, max_age: Duration) -> Result<bool, SyncError> {
    let repository = repository_path(root);
    if !repository.exists() || journal_path(root).exists() {
        return Ok(false);
    }
    let ownership = read_cache_ownership(root)?;
    let age_ms = now_unix_millis().saturating_sub(ownership.created_at_unix_ms);
    if u128::try_from(age_ms).unwrap_or(0) < max_age.as_millis() {
        return Ok(false);
    }
    Ok(remove_managed_cache(root)?.removed)
}

fn compact_receipts(root: &Path) -> Result<(), SyncError> {
    if journal_path(root).exists() {
        return Ok(());
    }
    let receipts = root.join("state/receipts");
    if !receipts.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(&receipts)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| {
        entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    // Keep a bounded recent set. A receipt is eligible only when its source spool directory is
    // absent and no journal exists (checked above).
    for entry in entries.iter().take(entries.len().saturating_sub(1024)) {
        let batch_id = entry
            .path()
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_owned();
        if !root.join("spool/ready").join(&batch_id).exists() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn ensure_storage_headroom(
    root: &Path,
    options: &SyncOptions,
    journal_pending: bool,
) -> Result<(), SyncError> {
    let used = directory_size(root)?;
    let available = fs2::available_space(root)?;
    if used >= options.local_budget_bytes || available < options.min_free_disk_bytes {
        if !journal_pending && repository_path(root).exists() {
            let _ = remove_managed_cache(root)?;
        }
        let used = directory_size(root)?;
        let available = fs2::available_space(root)?;
        if used >= options.local_budget_bytes || available < options.min_free_disk_bytes {
            return Err(SyncError::new(
                SyncErrorCode::StorageBudget,
                "local log storage budget has no safe headroom; pending spool was preserved",
            ));
        }
    }
    Ok(())
}

fn ensure_storage_headroom_after_git(root: &Path, options: &SyncOptions) -> Result<(), SyncError> {
    let used = directory_size(root)?;
    let available = fs2::available_space(root)?;
    if used > options.local_budget_bytes || available < options.min_free_disk_bytes {
        return Err(SyncError::new(
            SyncErrorCode::StorageBudget,
            "Git cache exceeded the local storage budget; pending spool was preserved",
        ));
    }
    Ok(())
}

fn directory_size(root: &Path) -> Result<u64, SyncError> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let metadata = match entry.path().symlink_metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, SyncError> {
    if path.symlink_metadata()?.file_type().is_symlink() {
        return Err(SyncError::new(
            SyncErrorCode::InvalidRoot,
            "state file is a symlink",
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(1024 * 1024)
        .read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| SyncError::new(SyncErrorCode::InvalidBatch, "invalid state file"))
}

fn sha256_file(path: &Path) -> Result<String, SyncError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn parse_oid(bytes: &[u8]) -> Result<String, SyncError> {
    let oid = String::from_utf8_lossy(bytes).trim().to_owned();
    if !(oid.len() == 40 || oid.len() == 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SyncError::new(
            SyncErrorCode::Git,
            "Git returned an invalid object id",
        ));
    }
    Ok(oid)
}

fn ensure_before_deadline(deadline: Instant) -> Result<(), SyncError> {
    if Instant::now() >= deadline {
        Err(SyncError::new(
            SyncErrorCode::Timeout,
            "log synchronization exceeded its deadline",
        ))
    } else {
        Ok(())
    }
}

fn now_unix_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn now_unix_seconds() -> i64 {
    now_unix_millis().div_euclid(1000)
}

fn utc_month(seconds: i64) -> Result<String, SyncError> {
    let (year, month, _) = civil_date(seconds)?;
    Ok(format!("{year:04}-{month:02}"))
}

fn utc_day(seconds: i64) -> Result<String, SyncError> {
    let (year, month, day) = civil_date(seconds)?;
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

// Howard Hinnant's proleptic-Gregorian civil-from-days transform.
fn civil_date(seconds: i64) -> Result<(i64, u32, u32), SyncError> {
    let days = seconds.div_euclid(86_400);
    let z = days.checked_add(719_468).ok_or_else(|| {
        SyncError::new(
            SyncErrorCode::InvalidBatch,
            "timestamp is outside supported range",
        )
    })?;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    Ok((
        year,
        u32::try_from(month).unwrap_or_default(),
        u32::try_from(day).unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use sctx_log_service::{Collector, CollectorOptions, InitOptions, StreamBinding};
    use sctx_telemetry::{EmitOutcome, EntryPoint, Event, EventKind, Outcome};
    use serde_json::json;

    use super::*;

    #[test]
    fn civil_dates_cover_epoch_and_leap_day() {
        assert_eq!(utc_day(0).unwrap(), "1970-01-01");
        assert_eq!(utc_day(1_709_164_800).unwrap(), "2024-02-29");
    }

    #[test]
    fn storage_measurement_ignores_git_style_temporary_file_churn() {
        let root = tempfile::tempdir().unwrap();
        let churn = root.path().join("objects");
        fs::create_dir(&churn).unwrap();
        assert_eq!(directory_size(&churn.join("already-removed")).unwrap(), 0);

        let running = Arc::new(AtomicBool::new(true));
        let writer_running = Arc::clone(&running);
        let writer = thread::spawn(move || {
            let mut index = 0_u64;
            while writer_running.load(Ordering::Relaxed) {
                let temporary = churn.join(format!("tmp-{index}"));
                let _ = fs::write(&temporary, b"temporary git object");
                let _ = fs::remove_file(&temporary);
                index = index.wrapping_add(1);
            }
        });
        for _ in 0..500 {
            directory_size(root.path()).unwrap();
        }
        running.store(false, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn email_encoding_is_reversible_and_path_safe() {
        assert_eq!(
            encode_component("alice+logs@example.com"),
            "alice%2Blogs@example.com"
        );
        assert!(!encode_component("a/b@example.com").contains('/'));
    }

    #[test]
    fn executable_remote_protocol_is_rejected() {
        assert!(validate_remote("ext::sh -c bad").is_err());
        assert!(validate_remote("https://git.example/logs.git").is_ok());
    }

    #[test]
    fn git_failure_details_use_only_closed_safe_classes() {
        for (stderr, expected) in [
            (
                "Permission denied (publickey)",
                "authentication_or_permission",
            ),
            ("Could not resolve host: secret.example", "dns"),
            ("Connection timed out", "connection_timeout"),
            ("Connection refused", "connection_refused"),
            ("rejected (non-fast-forward)", "non_fast_forward"),
            (
                "pre-receive hook declined",
                "protected_branch_or_pre_receive",
            ),
            ("shallow update not allowed", "shallow_repository"),
            ("index.lock already exists", "local_lock"),
            ("No space left on device", "disk_full"),
            ("TOP_SECRET_MARKER", "unknown"),
        ] {
            assert_eq!(classify_git_failure(stderr.as_bytes()), expected);
            let error = git_failure(SyncErrorStage::Push, "push", Some(128), stderr.as_bytes());
            assert!(error.safe_detail().contains(expected));
            assert!(!error.safe_detail().contains("TOP_SECRET_MARKER"));
        }
    }

    fn run_git(args: &[&OsStr]) -> std::process::Output {
        Command::new("/usr/bin/git").args(args).output().unwrap()
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, SyncOptions) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        fs::create_dir_all(root.path().join("spool/ready")).unwrap();
        let remote_parent = tempfile::tempdir().unwrap();
        let remote = remote_parent.path().join("remote.git");
        let output = run_git(&[OsStr::new("init"), OsStr::new("--bare"), remote.as_os_str()]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let options = SyncOptions {
            min_free_disk_bytes: 0,
            local_budget_bytes: 128 * 1024 * 1024,
            ..SyncOptions::default()
        };
        (root, remote_parent, options)
    }

    fn add_batch(root: &Path, remote: &Path, batch_id: &str, payload: &str) -> BatchManifest {
        add_bound_batch(
            root,
            remote,
            batch_id,
            payload,
            "stream-a",
            "alice@example.com",
        )
    }

    fn add_bound_batch(
        root: &Path,
        remote: &Path,
        batch_id: &str,
        payload: &str,
        stream_id: &str,
        email: &str,
    ) -> BatchManifest {
        let directory = root.join("spool/ready").join(batch_id);
        fs::create_dir(&directory).unwrap();
        let event = json!({
            "schema_version": 1,
            "installation_id": "install-a",
            "stream_id": stream_id,
            "email": email,
            "platform": "test",
            "collector_version": "test",
            "received_at_unix_ms": 1_709_164_800_000_i64,
            "event": {
                "occurred_at_unix_ms": 1_709_164_800_000_i64,
                "duration_ms": null,
                "sequence": 1,
                "entry_point": "cli",
                "kind": "operation_finished",
                "outcome": "success",
                "authorization": "not_applicable",
                "invocation_id": batch_id,
                "program_version": "test",
                "operation": payload,
                "error_code": null,
                "error_family": null,
                "reason": null,
                "summary": null,
                "session_digest": null,
                "task_id": null,
                "task_session_id": null,
                "episode_id": null,
                "checkpoint_id": null,
                "operation_id": null,
                "result_count": null
            }
        });
        let mut events = serde_json::to_vec(&event).unwrap();
        events.push(b'\n');
        fs::write(directory.join("events.jsonl"), &events).unwrap();
        let manifest = BatchManifest {
            schema_version: 1,
            batch_id: batch_id.to_owned(),
            stream: StreamBinding {
                stream_id: stream_id.to_owned(),
                installation_id: "install-a".to_owned(),
                email: Some(email.to_owned()),
                remote: Some(format!("file://{}", remote.display())),
            },
            first_event_unix_ms: 1_709_164_800_000,
            last_event_unix_ms: 1_709_164_800_000,
            event_count: 1,
            content_bytes: u64::try_from(events.len()).unwrap(),
            content_sha256: sha256_bytes(&events),
            collector_version: "test".to_owned(),
            sealed_at_unix_ms: 1_709_164_800_000,
        };
        atomic_write_json(&directory.join("manifest.json"), &manifest).unwrap();
        manifest
    }

    fn journal_for(manifest: &BatchManifest, branch: &str, phase: JournalPhase) -> UploadJournal {
        let (events_relative_path, manifest_relative_path) = batch_paths(manifest).unwrap();
        let remote = assigned_remote(manifest).unwrap().to_owned();
        UploadJournal {
            schema_version: STATE_SCHEMA_VERSION,
            batch_id: manifest.batch_id.clone(),
            stream_id: manifest.stream.stream_id.clone(),
            remote_digest: sha256_bytes(remote.as_bytes()),
            remote,
            branch: branch.to_owned(),
            events_relative_path,
            manifest_relative_path,
            content_bytes: manifest.content_bytes,
            content_sha256: manifest.content_sha256.clone(),
            phase,
            commit_oid: None,
            batches: Vec::new(),
        }
    }

    #[test]
    fn local_bare_remote_upload_is_verified_before_spool_deletion() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let manifest = add_batch(root.path(), &remote, "batch-one", "first");
        let business_lock = File::create(root.path().join("unrelated-business.lock")).unwrap();
        business_lock.lock_exclusive().unwrap();
        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.outcome, SyncOutcome::Uploaded);
        assert_eq!(report.uploaded_batches, 1);
        assert_eq!(report.seal_status, SealStatus::NoCollector);
        assert!(!report.active_included);
        assert!(!root.path().join("spool/ready/batch-one").exists());
        assert!(receipt_path(root.path(), "batch-one").is_file());

        let branch = report.branch.unwrap();
        let path = batch_paths(&manifest).unwrap().0;
        let output = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("show"),
            OsStr::new(&format!("refs/heads/{branch}:{path}")),
        ]);
        assert!(output.status.success());
        assert_eq!(sha256_bytes(&output.stdout), manifest.content_sha256);
    }

    #[test]
    fn lost_push_response_recovers_the_same_batch() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-unknown", "unknown");
        let wrapper = root.path().join("git-wrapper");
        fs::write(
            &wrapper,
            "#!/bin/sh\npush=0\nfor arg in \"$@\"; do [ \"$arg\" = push ] && push=1; done\n/usr/bin/git \"$@\"\nstatus=$?\n[ $status -eq 0 ] && [ $push -eq 1 ] && exit 1\nexit $status\n",
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;
        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        assert!(!root.path().join("state/upload-journal.json").exists());
        assert!(receipt_path(root.path(), "batch-unknown").is_file());
    }

    #[test]
    fn same_remote_path_with_different_hash_is_rejected_and_spool_survives() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-conflict", "original");
        sync(root.path(), &options).unwrap();
        add_batch(root.path(), &remote, "batch-conflict", "changed");
        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::Conflict);
        assert!(root.path().join("spool/ready/batch-conflict").is_dir());
        assert!(root.path().join("state/upload-journal.json").exists());
        let status: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        assert_eq!(status.last_error.as_deref(), Some("conflict"));
    }

    #[test]
    fn full_shard_rotates_to_an_orphan_branch() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        options.shard_max_files = 2;
        add_batch(root.path(), &remote, "batch-first", "first");
        sync(root.path(), &options).unwrap();
        add_batch(root.path(), &remote, "batch-second", "second");
        let report = sync(root.path(), &options).unwrap();
        let second_branch = report.branch.unwrap();
        assert!(second_branch.ends_with("/0002"));
        let output = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("--name-only"),
            OsStr::new(&format!("refs/heads/{second_branch}")),
        ]);
        let tree = String::from_utf8(output.stdout).unwrap();
        assert!(tree.contains("batch-second"));
        assert!(!tree.contains("batch-first"));
    }

    #[test]
    fn prune_refuses_pending_journal_and_symlink_cache() {
        let (root, _remote_parent, _options) = fixture();
        fs::write(journal_path(root.path()), b"{}\n").unwrap();
        assert_eq!(
            prune_cache(root.path()).unwrap_err().code(),
            SyncErrorCode::PendingJournal
        );
        fs::remove_file(journal_path(root.path())).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), repository_path(root.path())).unwrap();
        assert_eq!(
            prune_cache(root.path()).unwrap_err().code(),
            SyncErrorCode::InvalidRoot
        );
        assert!(outside.path().is_dir());
    }

    #[test]
    fn receipt_phase_without_receipt_reconfirms_before_deleting_source() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-receipt", "first");
        let first = sync(root.path(), &options).unwrap();
        let branch = first.branch.unwrap();
        fs::remove_file(receipt_path(root.path(), "batch-receipt")).unwrap();
        let manifest = add_batch(root.path(), &remote, "batch-receipt", "first");
        write_journal(
            root.path(),
            &journal_for(&manifest, &branch, JournalPhase::ReceiptDurable),
        )
        .unwrap();

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        assert!(!root.path().join("spool/ready/batch-receipt").exists());
        assert!(receipt_path(root.path(), "batch-receipt").is_file());
    }

    #[test]
    fn missing_source_with_corrupt_receipt_keeps_the_journal() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let manifest = add_batch(root.path(), &remote, "batch-corrupt", "first");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        let journal = journal_for(&manifest, &branch, JournalPhase::ReceiptDurable);
        write_journal(root.path(), &journal).unwrap();
        fs::write(receipt_path(root.path(), "batch-corrupt"), b"{}\n").unwrap();

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::InvalidBatch);
        assert!(journal_path(root.path()).is_file());
    }

    #[test]
    fn interrupted_clone_staging_is_safely_rebuilt_on_retry() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-base", "base");
        sync(root.path(), &options).unwrap();
        prune_cache(root.path()).unwrap();
        add_batch(root.path(), &remote, "batch-after-clone", "after");

        let sentinel = root.path().join("clone-failed-once");
        let wrapper = root.path().join("git-clone-failure-wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nclone=0\nlast=\nfor arg in \"$@\"; do [ \"$arg\" = clone ] && clone=1; last=$arg; done\nif [ $clone -eq 1 ] && [ ! -e '{}' ]; then mkdir -p \"$last\"; touch \"$last/partial\"; touch '{}'; exit 1; fi\nexec /usr/bin/git \"$@\"\n",
                sentinel.display(),
                sentinel.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;
        options.max_retries = 0;

        assert_eq!(
            sync(root.path(), &options).unwrap_err().code(),
            SyncErrorCode::Git
        );
        assert!(
            cache_staging_path(root.path())
                .join("ownership.json")
                .is_file()
        );
        assert!(root.path().join("spool/ready/batch-after-clone").is_dir());
        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        assert!(!cache_staging_path(root.path()).exists());
    }

    #[test]
    fn pending_journal_refuses_to_recreate_a_deleted_confirmed_shard() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-history-base", "base");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        let pending = add_batch(root.path(), &remote, "batch-history-pending", "pending");
        write_journal(
            root.path(),
            &journal_for(&pending, &branch, JournalPhase::Prepared),
        )
        .unwrap();
        let deleted = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("update-ref"),
            OsStr::new("-d"),
            OsStr::new(&format!("refs/heads/{branch}")),
        ]);
        assert!(deleted.status.success());

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::RemoteHistoryMissing);
        assert!(
            root.path()
                .join("spool/ready/batch-history-pending")
                .is_dir()
        );
        assert!(journal_path(root.path()).is_file());
    }

    #[test]
    fn receipt_fast_path_repairs_shard_confirmation_before_source_deletion() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let manifest = add_batch(root.path(), &remote, "batch-fast-receipt", "base");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        add_batch(root.path(), &remote, "batch-fast-receipt", "base");
        write_journal(
            root.path(),
            &journal_for(&manifest, &branch, JournalPhase::ReceiptDurable),
        )
        .unwrap();
        let state_path = shard_state_path(root.path(), "stream-a");
        let mut state: ShardState = read_json(&state_path).unwrap();
        state.remote_confirmed = false;
        atomic_write_json(&state_path, &state).unwrap();

        sync(root.path(), &options).unwrap();
        let repaired: ShardState = read_json(&state_path).unwrap();
        assert!(repaired.remote_confirmed);
        assert!(!root.path().join("spool/ready/batch-fast-receipt").exists());
        assert!(!journal_path(root.path()).exists());
    }

    #[test]
    fn receipt_fast_path_preserves_source_when_confirmed_ref_disappeared() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let manifest = add_batch(root.path(), &remote, "batch-lost-receipt", "base");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        add_batch(root.path(), &remote, "batch-lost-receipt", "base");
        write_journal(
            root.path(),
            &journal_for(&manifest, &branch, JournalPhase::ReceiptDurable),
        )
        .unwrap();
        let state_path = shard_state_path(root.path(), "stream-a");
        let mut state: ShardState = read_json(&state_path).unwrap();
        state.remote_confirmed = false;
        atomic_write_json(&state_path, &state).unwrap();
        let deleted = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("update-ref"),
            OsStr::new("-d"),
            OsStr::new(&format!("refs/heads/{branch}")),
        ]);
        assert!(deleted.status.success());

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::RemoteHistoryMissing);
        assert!(root.path().join("spool/ready/batch-lost-receipt").is_dir());
        assert!(journal_path(root.path()).is_file());
    }

    #[test]
    fn receipt_fast_path_preserves_source_when_confirmed_ref_is_rewritten() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let first = add_batch(root.path(), &remote, "batch-rewritten-a", "first");
        let second = add_batch(root.path(), &remote, "batch-rewritten-b", "second");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        add_batch(root.path(), &remote, "batch-rewritten-a", "first");
        add_batch(root.path(), &remote, "batch-rewritten-b", "second");
        let mut journal = journal_for(&first, &branch, JournalPhase::ReceiptDurable);
        journal.batches = [&first, &second]
            .into_iter()
            .map(|manifest| {
                let (events_relative_path, manifest_relative_path) = batch_paths(manifest).unwrap();
                JournalBatch {
                    batch_id: manifest.batch_id.clone(),
                    events_relative_path,
                    manifest_relative_path,
                    content_bytes: manifest.content_bytes,
                    content_sha256: manifest.content_sha256.clone(),
                    receipt_durable: true,
                }
            })
            .collect();
        write_journal(root.path(), &journal).unwrap();
        fs::remove_dir_all(root.path().join("spool/ready/batch-rewritten-a")).unwrap();

        let replacement = remote_parent.path().join("replacement");
        assert!(
            run_git(&[OsStr::new("init"), replacement.as_os_str()])
                .status
                .success()
        );
        assert!(
            run_git(&[
                OsStr::new("-C"),
                replacement.as_os_str(),
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("rewriter"),
            ])
            .status
            .success()
        );
        assert!(
            run_git(&[
                OsStr::new("-C"),
                replacement.as_os_str(),
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("rewriter@example.com"),
            ])
            .status
            .success()
        );
        fs::write(replacement.join("unrelated"), b"replacement\n").unwrap();
        assert!(
            run_git(&[
                OsStr::new("-C"),
                replacement.as_os_str(),
                OsStr::new("add"),
                OsStr::new("unrelated"),
            ])
            .status
            .success()
        );
        assert!(
            run_git(&[
                OsStr::new("-C"),
                replacement.as_os_str(),
                OsStr::new("commit"),
                OsStr::new("-m"),
                OsStr::new("replacement"),
            ])
            .status
            .success()
        );
        assert!(
            run_git(&[
                OsStr::new("-C"),
                replacement.as_os_str(),
                OsStr::new("push"),
                OsStr::new("--force"),
                remote.as_os_str(),
                OsStr::new(&format!("HEAD:refs/heads/{branch}")),
            ])
            .status
            .success()
        );

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::RemoteHistoryMissing);
        assert!(root.path().join("spool/ready/batch-rewritten-b").is_dir());
        assert!(journal_path(root.path()).is_file());
    }

    #[test]
    fn missing_source_recovery_keeps_journal_when_confirmed_ref_disappeared() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let manifest = add_batch(root.path(), &remote, "batch-missing-source", "base");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        write_journal(
            root.path(),
            &journal_for(&manifest, &branch, JournalPhase::ReceiptDurable),
        )
        .unwrap();
        let state_path = shard_state_path(root.path(), "stream-a");
        let mut state: ShardState = read_json(&state_path).unwrap();
        state.remote_confirmed = false;
        atomic_write_json(&state_path, &state).unwrap();
        let deleted = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("update-ref"),
            OsStr::new("-d"),
            OsStr::new(&format!("refs/heads/{branch}")),
        ]);
        assert!(deleted.status.success());

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::RemoteHistoryMissing);
        assert!(journal_path(root.path()).is_file());
        let state: ShardState = read_json(&state_path).unwrap();
        assert!(!state.remote_confirmed);
    }

    #[test]
    fn partial_untracked_cache_copy_is_replaced_from_the_sealed_source() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-copy-base", "base");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        let pending = add_batch(root.path(), &remote, "batch-copy-pending", "pending");
        let journal = journal_for(&pending, &branch, JournalPhase::Prepared);
        write_journal(root.path(), &journal).unwrap();
        let partial =
            safe_repository_path(&repository_path(root.path()), &journal.events_relative_path)
                .unwrap();
        fs::create_dir_all(partial.parent().unwrap()).unwrap();
        fs::write(&partial, b"partial copy left by interruption").unwrap();

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        assert!(!root.path().join("spool/ready/batch-copy-pending").exists());
        assert!(!journal_path(root.path()).exists());
    }

    #[test]
    fn same_shard_batches_share_one_commit_and_push() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        for (id, payload) in [
            ("batch-a", "first"),
            ("batch-b", "second"),
            ("batch-c", "third"),
        ] {
            add_batch(root.path(), &remote, id, payload);
        }

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 3);
        let branch = report.branch.unwrap();
        let count = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new(&format!("refs/heads/{branch}")),
        ]);
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
        for id in ["batch-a", "batch-b", "batch-c"] {
            assert!(receipt_path(root.path(), id).is_file());
            assert!(!root.path().join("spool/ready").join(id).exists());
        }
    }

    #[test]
    fn later_group_failure_records_the_completed_group() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let unavailable = remote_parent.path().join("missing/remote.git");
        add_bound_batch(
            root.path(),
            &remote,
            "batch-a-good",
            "good",
            "stream-a",
            "alice@example.com",
        );
        add_bound_batch(
            root.path(),
            &unavailable,
            "batch-z-unavailable",
            "later",
            "stream-b",
            "bob@example.com",
        );
        options.max_retries = 0;

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.uploaded_batches(), 1);
        assert!(error.uploaded_bytes() > 0);
        assert!(!root.path().join("spool/ready/batch-a-good").exists());
        assert!(root.path().join("spool/ready/batch-z-unavailable").is_dir());
        let status: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        assert_eq!(status.last_attempt_uploaded_batches, 1);
        assert!(status.last_success_unix_ms.is_some());
        assert_eq!(status.last_error_code, Some(SyncErrorCode::Git));
        assert_eq!(status.last_error_retryable, Some(true));
        assert!(status.last_error_detail.as_deref().unwrap().len() <= 256);
    }

    #[test]
    fn malformed_batch_detail_never_persists_sensitive_input() {
        let (root, _remote_parent, options) = fixture();
        let directory = root.path().join("spool/ready/batch-sensitive");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("events.jsonl"), b"{}\n").unwrap();
        fs::write(
            directory.join("manifest.json"),
            br#"{"schema_version":"TOP_SECRET_MARKER"}"#,
        )
        .unwrap();

        let error = sync(root.path(), &options).unwrap_err();
        assert!(!error.safe_detail().contains("TOP_SECRET_MARKER"));
        let persisted = fs::read_to_string(root.path().join("state/upload-status.json")).unwrap();
        assert!(!persisted.contains("TOP_SECRET_MARKER"));
        assert!(persisted.contains("ready batch discovery failed"));
    }

    #[test]
    fn existing_receipt_repairs_a_missing_historical_success_time() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-history-success", "success");
        sync(root.path(), &options).unwrap();
        let mut status: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        status.last_success_unix_ms = None;
        atomic_write_json(&root.path().join("state/upload-status.json"), &status).unwrap();

        let unavailable = remote_parent.path().join("missing/remote.git");
        add_bound_batch(
            root.path(),
            &unavailable,
            "batch-history-failure",
            "failure",
            "stream-b",
            "bob@example.com",
        );
        options.max_retries = 0;
        sync(root.path(), &options).unwrap_err();

        let repaired: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        assert!(repaired.last_success_unix_ms.is_some());
        assert_eq!(repaired.last_attempt_uploaded_batches, 0);
    }

    #[test]
    fn post_push_remote_probe_failure_is_retried() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-network", "network");
        let pushed = root.path().join("push-finished");
        let failed = root.path().join("probe-failed");
        let wrapper = root.path().join("git-post-push-probe-wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\npush=0\nprobe=0\nfor arg in \"$@\"; do [ \"$arg\" = push ] && push=1; [ \"$arg\" = ls-remote ] && probe=1; done\nif [ $probe -eq 1 ] && [ -e '{}' ] && [ ! -e '{}' ]; then touch '{}'; exit 71; fi\n/usr/bin/git \"$@\"\nstatus=$?\nif [ $push -eq 1 ] && [ $status -eq 0 ]; then touch '{}'; fi\nexit $status\n",
                pushed.display(), failed.display(), failed.display(), pushed.display(),
            ),
        ).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        assert!(failed.is_file());
        assert!(receipt_path(root.path(), "batch-network").is_file());
    }

    #[test]
    fn push_runner_timeout_keeps_the_push_stage() {
        let (root, _remote_parent, mut options) = fixture();
        let wrapper = root.path().join("git-push-timeout-wrapper");
        fs::write(&wrapper, "#!/bin/sh\nsleep 4\nexec /usr/bin/git \"$@\"\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;

        let error = git_for(
            root.path(),
            &options,
            Instant::now() + Duration::from_millis(200),
            Some(root.path()),
            [OsString::from("version")],
            options.output_limit,
            SyncErrorStage::Push,
            "push",
        )
        .unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::Timeout);
        assert_eq!(error.stage(), SyncErrorStage::Push);
    }

    #[test]
    fn expired_retry_keeps_the_post_push_verify_stage() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-verify-timeout", "timeout");
        let pushed = root.path().join("push-completed");
        let wrapper = root.path().join("git-verify-timeout-wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\npush=0\nprobe=0\nfor arg in \"$@\"; do [ \"$arg\" = push ] && push=1; [ \"$arg\" = ls-remote ] && probe=1; done\nif [ $probe -eq 1 ] && [ -e '{}' ]; then sleep 8; fi\n/usr/bin/git \"$@\"\nstatus=$?\nif [ $push -eq 1 ] && [ $status -eq 0 ]; then touch '{}'; fi\nexit $status\n",
                pushed.display(),
                pushed.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;
        options.deadline = Duration::from_secs(5);
        options.max_retries = 1;

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::Timeout);
        assert_eq!(error.stage(), SyncErrorStage::Verify);
        assert!(pushed.is_file());
        assert!(
            root.path()
                .join("spool/ready/batch-verify-timeout")
                .is_dir()
        );
    }

    #[test]
    fn non_fast_forward_race_rebases_group_without_force() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-race-base", "base");
        let base = sync(root.path(), &options).unwrap();
        let branch = base.branch.unwrap();
        add_batch(root.path(), &remote, "batch-race-pending", "pending");

        let injected = remote_parent.path().join("race-injected");
        let racer = remote_parent.path().join("racer");
        let wrapper = root.path().join("git-nff-wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\npush=0\nfor arg in \"$@\"; do [ \"$arg\" = push ] && push=1; done\nif [ $push -eq 1 ] && [ ! -e '{}' ]; then\n  /usr/bin/git clone --quiet --branch '{}' 'file://{}' '{}' || exit 80\n  /usr/bin/git -C '{}' config user.name racer\n  /usr/bin/git -C '{}' config user.email racer@example.com\n  echo race > '{}/race.txt'\n  /usr/bin/git -C '{}' add race.txt\n  /usr/bin/git -C '{}' commit --quiet -m race || exit 81\n  /usr/bin/git -C '{}' push --quiet origin 'HEAD:refs/heads/{}' || exit 82\n  touch '{}'
fi\nexec /usr/bin/git \"$@\"\n",
                injected.display(),
                branch,
                remote.display(),
                racer.display(),
                racer.display(),
                racer.display(),
                racer.display(),
                racer.display(),
                racer.display(),
                racer.display(),
                branch,
                injected.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        let count = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new(&format!("refs/heads/{branch}")),
        ]);
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "3");
        let race_file = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("show"),
            OsStr::new(&format!("refs/heads/{branch}:race.txt")),
        ]);
        assert_eq!(String::from_utf8_lossy(&race_file.stdout).trim(), "race");
    }

    #[test]
    fn legacy_committed_journal_pushes_preserved_local_commit() {
        let (root, remote_parent, mut options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        add_batch(root.path(), &remote, "batch-legacy", "legacy");
        let wrapper = root.path().join("git-reject-push-wrapper");
        fs::write(
            &wrapper,
            "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = push ] && exit 72; done\nexec /usr/bin/git \"$@\"\n",
        ).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        options.git_binary = wrapper;
        options.max_retries = 0;
        assert_eq!(
            sync(root.path(), &options).unwrap_err().stage(),
            SyncErrorStage::Push
        );

        let mut journal = load_journal(root.path()).unwrap().unwrap();
        assert_eq!(journal.phase, JournalPhase::Committed);
        assert!(journal.commit_oid.is_some());
        journal.batches.clear();
        write_journal(root.path(), &journal).unwrap();
        options.git_binary = PathBuf::from("/usr/bin/git");

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 1);
        assert!(!journal_path(root.path()).exists());
        assert!(receipt_path(root.path(), "batch-legacy").is_file());
    }

    #[test]
    fn receipt_durable_group_recovers_after_partial_source_cleanup() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let first = add_batch(root.path(), &remote, "batch-clean-a", "first");
        let second = add_batch(root.path(), &remote, "batch-clean-b", "second");
        let report = sync(root.path(), &options).unwrap();
        let branch = report.branch.unwrap();
        add_batch(root.path(), &remote, "batch-clean-a", "first");
        add_batch(root.path(), &remote, "batch-clean-b", "second");
        let mut journal = journal_for(&first, &branch, JournalPhase::ReceiptDurable);
        journal.batches = [&first, &second]
            .into_iter()
            .map(|manifest| {
                let (events_relative_path, manifest_relative_path) = batch_paths(manifest).unwrap();
                JournalBatch {
                    batch_id: manifest.batch_id.clone(),
                    events_relative_path,
                    manifest_relative_path,
                    content_bytes: manifest.content_bytes,
                    content_sha256: manifest.content_sha256.clone(),
                    receipt_durable: true,
                }
            })
            .collect();
        fs::remove_dir_all(root.path().join("spool/ready/batch-clean-a")).unwrap();
        write_journal(root.path(), &journal).unwrap();

        let recovered = sync(root.path(), &options).unwrap();
        assert_eq!(recovered.uploaded_batches, 2);
        assert!(!root.path().join("spool/ready/batch-clean-b").exists());
        assert!(!journal_path(root.path()).exists());
    }

    #[test]
    fn missing_first_group_member_does_not_hide_later_remote_conflict() {
        let (root, remote_parent, options) = fixture();
        let remote = remote_parent.path().join("remote.git");
        let original = add_batch(root.path(), &remote, "batch-b", "original");
        let first = sync(root.path(), &options).unwrap();
        let branch = first.branch.unwrap();
        add_batch(root.path(), &remote, "batch-a", "missing-remotely");
        add_batch(root.path(), &remote, "batch-b", "changed");

        let error = sync(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::Conflict);
        assert!(!error.retryable());
        assert!(root.path().join("spool/ready/batch-a").is_dir());
        assert!(root.path().join("spool/ready/batch-b").is_dir());
        let path = batch_paths(&original).unwrap().0;
        let output = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("show"),
            OsStr::new(&format!("refs/heads/{branch}:{path}")),
        ]);
        assert_eq!(sha256_bytes(&output.stdout), original.content_sha256);
    }

    #[test]
    fn bounded_large_group_journal_roundtrips_and_uses_one_commit() {
        let (root, remote_parent, mut options) = fixture();
        options.local_budget_bytes = 1024 * 1024 * 1024;
        let remote = remote_parent.path().join("remote.git");
        let email = format!("{}@example.com", "a".repeat(180));
        for index in 0..80 {
            add_bound_batch(
                root.path(),
                &remote,
                &format!("batch-{index:03}"),
                "small",
                "stream-a",
                &email,
            );
        }
        let ready = list_ready_batches(root.path()).unwrap();
        let mut journal = journal_for(
            &ready[0].manifest,
            "logs/install-a/stream-a/2026-09/0001",
            JournalPhase::Prepared,
        );
        journal.batches = ready
            .iter()
            .map(|batch| {
                let (events_relative_path, manifest_relative_path) =
                    batch_paths(&batch.manifest).unwrap();
                JournalBatch {
                    batch_id: batch.manifest.batch_id.clone(),
                    events_relative_path,
                    manifest_relative_path,
                    content_bytes: batch.manifest.content_bytes,
                    content_sha256: batch.manifest.content_sha256.clone(),
                    receipt_durable: false,
                }
            })
            .collect();
        write_journal(root.path(), &journal).unwrap();
        assert_eq!(
            load_journal(root.path()).unwrap().unwrap().batches.len(),
            80
        );
        remove_journal(root.path()).unwrap();

        let report = sync(root.path(), &options).unwrap();
        assert_eq!(report.uploaded_batches, 80);
        let branch = report.branch.unwrap();
        let count = run_git(&[
            OsStr::new("--git-dir"),
            remote.as_os_str(),
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new(&format!("refs/heads/{branch}")),
        ]);
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    }

    #[test]
    fn scheduled_target_rotation_ignores_the_previous_targets_block() {
        let root = tempfile::tempdir().unwrap();
        let remote_parent = tempfile::tempdir().unwrap();
        let remote = remote_parent.path().join("remote.git");
        assert!(
            run_git(&[OsStr::new("init"), OsStr::new("--bare"), remote.as_os_str()])
                .status
                .success()
        );
        let initialized = sctx_log_service::init(
            root.path(),
            InitOptions {
                email: Some("new@example.com".to_owned()),
                remote: Some(format!("file://{}", remote.display())),
                installation_id: Some("install-scheduled".to_owned()),
                enabled: true,
            },
        )
        .unwrap();
        let blocked = UploadStatus {
            schema_version: STATE_SCHEMA_VERSION,
            automatic_retry_blocked: true,
            target_digest: Some("old-target".to_owned()),
            next_retry_unix_ms: Some(i64::MAX),
            ..UploadStatus::default()
        };
        atomic_write_json(&root.path().join("state/upload-status.json"), &blocked).unwrap();
        let options = SyncOptions {
            min_free_disk_bytes: 0,
            local_budget_bytes: 128 * 1024 * 1024,
            ..SyncOptions::from_config(&initialized.config)
        };

        let report = sync_scheduled(root.path(), &options).unwrap();
        assert_eq!(report.outcome, SyncOutcome::NoReady);
        let status: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        assert!(!status.automatic_retry_blocked);
        assert_eq!(
            status.target_digest,
            config_target_digest(&initialized.config)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn retained_stream_block_is_scoped_to_the_actual_queue_target() {
        let root = tempfile::tempdir().unwrap();
        let remote_parent = tempfile::tempdir().unwrap();
        let remote_a = remote_parent.path().join("remote-a.git");
        let remote_b = remote_parent.path().join("remote-b.git");
        for remote in [&remote_a, &remote_b] {
            assert!(
                run_git(&[OsStr::new("init"), OsStr::new("--bare"), remote.as_os_str()])
                    .status
                    .success()
            );
        }
        let initialized = sctx_log_service::init(
            root.path(),
            InitOptions {
                email: Some("current-b@example.com".to_owned()),
                remote: Some(format!("file://{}", remote_b.display())),
                installation_id: Some("install-scope".to_owned()),
                enabled: true,
            },
        )
        .unwrap();
        add_bound_batch(
            root.path(),
            &remote_a,
            "batch-a-retained",
            "retained-a",
            "stream-a-retained",
            "old-a@example.com",
        );
        add_bound_batch(
            root.path(),
            &remote_b,
            "batch-b-current",
            "current-b",
            &initialized.config.stream_id,
            "current-b@example.com",
        );

        let config_b_digest = config_target_digest(&initialized.config).unwrap();
        let scope_a = next_work_scope(root.path(), Some(&initialized.config)).unwrap();
        assert_eq!(scope_a.stream_id.as_deref(), Some("stream-a-retained"));
        let old_status = UploadStatus {
            schema_version: STATE_SCHEMA_VERSION,
            target_digest: Some(scope_a.digest.clone()),
            configured_target_digest: Some("old-config-a".to_owned()),
            automatic_retry_blocked: true,
            blocked_stream_id: Some("stream-a-retained".to_owned()),
            ..UploadStatus::default()
        };
        assert!(!scheduled_gate_is_closed(
            &old_status,
            Some(&scope_a),
            Some(&config_b_digest),
            now_unix_millis(),
        ));

        atomic_write_json(&root.path().join("state/upload-status.json"), &old_status).unwrap();
        let permanent = Err(SyncError::new(
            SyncErrorCode::Conflict,
            "the retained stream is still conflicted",
        ));
        record_upload_status_locked(
            root.path(),
            &permanent,
            Some(&scope_a),
            Some(config_b_digest.clone()),
        );
        let blocked: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        assert_eq!(
            blocked.blocked_stream_id.as_deref(),
            Some("stream-a-retained")
        );
        assert_eq!(
            blocked.target_digest.as_deref(),
            Some(scope_a.digest.as_str())
        );
        assert_eq!(
            blocked.configured_target_digest.as_deref(),
            Some(config_b_digest.as_str())
        );

        let mut options = SyncOptions {
            min_free_disk_bytes: 0,
            local_budget_bytes: 128 * 1024 * 1024,
            ..SyncOptions::from_config(&initialized.config)
        };
        assert_eq!(
            sync_scheduled(root.path(), &options).unwrap().outcome,
            SyncOutcome::SkippedNotDue
        );

        let ready = list_ready_batches(root.path()).unwrap();
        options.max_new_payload_bytes = batch_payload_bytes(&ready[0]).unwrap();
        let manual = sync(root.path(), &options).unwrap();
        assert_eq!(manual.outcome, SyncOutcome::Partial);
        assert_eq!(manual.uploaded_batches, 1);
        assert!(!root.path().join("spool/ready/batch-a-retained").exists());
        assert!(root.path().join("spool/ready/batch-b-current").is_dir());
        let after_manual: UploadStatus =
            read_json(&root.path().join("state/upload-status.json")).unwrap();
        let scope_b = next_work_scope(root.path(), Some(&initialized.config)).unwrap();
        assert_eq!(
            scope_b.stream_id.as_deref(),
            Some(initialized.config.stream_id.as_str())
        );
        assert_eq!(
            after_manual.target_digest.as_deref(),
            Some(scope_b.digest.as_str())
        );
        assert!(!after_manual.automatic_retry_blocked);
        assert!(after_manual.blocked_stream_id.is_none());
        assert!(after_manual.next_retry_unix_ms.is_some());
    }

    #[test]
    fn scheduled_status_preflight_failure_runs_no_git_command() {
        let root = tempfile::tempdir().unwrap();
        let remote_parent = tempfile::tempdir().unwrap();
        let remote = remote_parent.path().join("remote.git");
        assert!(
            run_git(&[OsStr::new("init"), OsStr::new("--bare"), remote.as_os_str()])
                .status
                .success()
        );
        let initialized = sctx_log_service::init(
            root.path(),
            InitOptions {
                email: Some("scheduled@example.com".to_owned()),
                remote: Some(format!("file://{}", remote.display())),
                installation_id: Some("install-preflight".to_owned()),
                enabled: true,
            },
        )
        .unwrap();
        fs::create_dir(root.path().join("state/upload-status.json")).unwrap();
        let invoked = root.path().join("git-was-invoked");
        let wrapper = root.path().join("git-preflight-wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\ntouch '{}'\nexec /usr/bin/git \"$@\"\n",
                invoked.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let options = SyncOptions {
            min_free_disk_bytes: 0,
            local_budget_bytes: 128 * 1024 * 1024,
            git_binary: wrapper,
            ..SyncOptions::from_config(&initialized.config)
        };

        let error = sync_scheduled(root.path(), &options).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::Io);
        assert_eq!(error.stage(), SyncErrorStage::Prepare);
        assert!(!invoked.exists());
    }

    #[test]
    fn parent_symlink_alias_uses_the_collectors_original_endpoint_identity() {
        let container = tempfile::tempdir().unwrap();
        let real_parent = container.path().join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let alias_parent = container.path().join("alias-parent");
        std::os::unix::fs::symlink(&real_parent, &alias_parent).unwrap();
        let logs_root = alias_parent.join("logs");
        let remote = container.path().join("remote.git");
        let initialized = run_git(&[OsStr::new("init"), OsStr::new("--bare"), remote.as_os_str()]);
        assert!(initialized.status.success());
        sctx_log_service::init(
            &logs_root,
            InitOptions {
                email: Some("alice@example.com".to_owned()),
                remote: Some(format!("file://{}", remote.display())),
                installation_id: Some("install-alias".to_owned()),
                enabled: true,
            },
        )
        .unwrap();
        let mut collector = Collector::open(
            &logs_root,
            CollectorOptions {
                idle_sleep: Duration::from_millis(5),
                read_buffer_bytes: 4096,
            },
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let collector_stop = Arc::clone(&stop);
        let collector_thread = thread::spawn(move || collector.run(&collector_stop));
        let event = Event::finished(
            EntryPoint::Hook,
            EventKind::HookDecision,
            "alias-invocation",
            "hook.cursor.session_start",
            Outcome::Success,
        );
        assert_eq!(
            sctx_telemetry::emit_to(&logs_root, &event),
            EmitOutcome::Sent
        );

        let report = sync(
            &logs_root,
            &SyncOptions {
                min_free_disk_bytes: 0,
                local_budget_bytes: 128 * 1024 * 1024,
                ..SyncOptions::default()
            },
        )
        .unwrap();
        stop.store(true, Ordering::Relaxed);
        collector_thread.join().unwrap().unwrap();
        assert_eq!(report.seal_status, SealStatus::Sealed);
        assert!(report.active_included);
        assert_eq!(report.uploaded_batches, 1);

        let direct_root_symlink = container.path().join("direct-logs-link");
        std::os::unix::fs::symlink(logs_root.canonicalize().unwrap(), &direct_root_symlink)
            .unwrap();
        let error = sync(&direct_root_symlink, &SyncOptions::default()).unwrap_err();
        assert_eq!(error.code(), SyncErrorCode::InvalidRoot);
    }
}
