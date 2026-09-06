use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use sctx_log_service::{
    BatchManifest, Config, ReadyBatch, SealRequestOutcome, atomic_write_json, list_ready_batches,
    request_seal,
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

#[derive(Debug)]
pub struct SyncError {
    code: SyncErrorCode,
    message: String,
}

impl SyncError {
    #[must_use]
    pub const fn code(&self) -> SyncErrorCode {
        self.code
    }

    fn new(code: SyncErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SyncError {}

impl From<io::Error> for SyncError {
    fn from(error: io::Error) -> Self {
        Self::new(SyncErrorCode::Io, error.to_string())
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
}

struct SyncLock {
    _file: File,
}

/// Uploads sealed batches under `root` while holding only the logging sync lock.
///
/// # Errors
///
/// Returns a typed error for invalid state, unsafe paths, storage pressure, Git failure, a remote
/// conflict, or deadline expiry. Unconfirmed source batches are preserved on every error.
#[allow(clippy::too_many_lines)]
pub fn sync(root: &Path, options: &SyncOptions) -> Result<SyncReport, SyncError> {
    let result = sync_inner(root, options);
    let skipped_busy = matches!(
        &result,
        Ok(report) if report.outcome == SyncOutcome::SkippedBusy
    );
    if !skipped_busy {
        record_upload_status(root, &result);
    }
    result
}

#[allow(clippy::too_many_lines)]
fn sync_inner(root: &Path, options: &SyncOptions) -> Result<SyncReport, SyncError> {
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
    let deadline = Instant::now() + options.deadline;
    let seal_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_secs(2));
    // The FIFO endpoint identity is derived from the caller's path spelling. Keep state and Git
    // operations on the validated canonical root, but address the collector exactly as producers
    // and `logs collect --logs-root` do (for example `/tmp` versus `/private/tmp` on macOS).
    let seal_status = match request_seal(root, &seal_deadline) {
        Ok(SealRequestOutcome::Sealed) => SealStatus::Sealed,
        Ok(SealRequestOutcome::NoCollector) => SealStatus::NoCollector,
        Ok(SealRequestOutcome::TimedOut) => SealStatus::TimedOut,
        Ok(SealRequestOutcome::Rejected) => SealStatus::Rejected,
        Err(_) => SealStatus::RequestFailed,
    };
    let mut ready = list_ready_batches(&canonical_root)
        .map_err(|error| SyncError::new(SyncErrorCode::InvalidBatch, error.to_string()))?;
    let journal = load_journal(&canonical_root)?;
    if let Some(journal) = &journal {
        ready.sort_by_key(|batch| {
            (
                batch.manifest.batch_id != journal.batch_id,
                batch.manifest.sealed_at_unix_ms,
            )
        });
        if !ready
            .iter()
            .any(|batch| batch.manifest.batch_id == journal.batch_id)
        {
            if matches!(journal.phase, JournalPhase::ReceiptDurable)
                && load_valid_receipt(&canonical_root, journal).is_ok()
            {
                protect_confirmed_history(&canonical_root, journal, options, deadline)?;
                mark_shard_remote_confirmed(&canonical_root, journal)?;
                remove_journal(&canonical_root)?;
            } else {
                return Err(SyncError::new(
                    SyncErrorCode::InvalidBatch,
                    "upload journal references a missing batch without a durable receipt",
                ));
            }
        }
    }
    if ready.is_empty() {
        let cache_pruned = prune_expired_cache_locked(&canonical_root, options.cache_max_age)?;
        compact_receipts(&canonical_root)?;
        return Ok(SyncReport {
            outcome: SyncOutcome::NoReady,
            uploaded_batches: 0,
            uploaded_bytes: 0,
            remaining_batches: 0,
            branch: None,
            cache_pruned,
            seal_status,
            active_included: seal_status == SealStatus::Sealed,
        });
    }

    ensure_storage_headroom(&canonical_root, options, journal.is_some())?;
    let mut uploaded_batches = 0_u64;
    let mut uploaded_bytes = 0_u64;
    let mut last_branch = None;
    for batch in ready {
        let batch_bytes = batch
            .manifest
            .content_bytes
            .saturating_add(fs::metadata(&batch.manifest_path)?.len());
        if uploaded_batches > 0
            && uploaded_bytes.saturating_add(batch_bytes) > options.max_new_payload_bytes
        {
            break;
        }
        let branch = sync_batch(&canonical_root, &batch, options, deadline)?;
        uploaded_batches += 1;
        uploaded_bytes = uploaded_bytes.saturating_add(batch_bytes);
        last_branch = Some(branch);
        if Instant::now() >= deadline {
            break;
        }
    }
    let remaining = list_ready_batches(&canonical_root)
        .map_err(|error| SyncError::new(SyncErrorCode::InvalidBatch, error.to_string()))?
        .len() as u64;
    let cache_pruned = if remaining == 0 {
        prune_expired_cache_locked(&canonical_root, options.cache_max_age)?
    } else {
        false
    };
    if remaining == 0 {
        compact_receipts(&canonical_root)?;
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

fn record_upload_status(root: &Path, result: &Result<SyncReport, SyncError>) {
    let Ok(root) = validate_root(root) else {
        return;
    };
    let path = root.join("state/upload-status.json");
    let mut status = read_json::<UploadStatus>(&path).unwrap_or_default();
    status.schema_version = STATE_SCHEMA_VERSION;
    let now = now_unix_millis();
    status.last_attempt_unix_ms = Some(now);
    match result {
        Ok(report) => {
            if report.uploaded_batches > 0 {
                status.last_success_unix_ms = Some(now);
            }
            status.last_error = None;
        }
        Err(error) => status.last_error = Some(sync_error_code_name(error.code()).to_owned()),
    }
    // Upload status is an observational summary. Once a receipt is durable, failure to update
    // this file must not turn a completed upload into an error or reclassify the batch.
    let _ = atomic_write_json(&path, &status);
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

fn sync_batch(
    root: &Path,
    batch: &ReadyBatch,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<String, SyncError> {
    validate_batch_identity(&batch.manifest)?;
    let remote = assigned_remote(&batch.manifest)?;
    validate_remote(remote)?;
    let remote_digest = sha256_bytes(remote.as_bytes());
    let mut journal = if let Some(existing) = load_journal(root)? {
        if existing.batch_id != batch.manifest.batch_id
            || existing.stream_id != batch.manifest.stream.stream_id
            || existing.remote_digest != remote_digest
            || existing.remote != batch.manifest.stream.remote.as_deref().unwrap_or_default()
            || existing.content_sha256 != batch.manifest.content_sha256
        {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "upload journal does not match the sealed batch",
            ));
        }
        existing
    } else {
        let shard = select_shard(root, batch, options, deadline, &remote_digest)?;
        let (events_relative_path, manifest_relative_path) = batch_paths(&batch.manifest)?;
        let journal = UploadJournal {
            schema_version: STATE_SCHEMA_VERSION,
            batch_id: batch.manifest.batch_id.clone(),
            stream_id: batch.manifest.stream.stream_id.clone(),
            remote: assigned_remote(&batch.manifest)?.to_owned(),
            remote_digest,
            branch: shard.branch,
            events_relative_path,
            manifest_relative_path,
            content_bytes: batch.manifest.content_bytes,
            content_sha256: batch.manifest.content_sha256.clone(),
            phase: JournalPhase::Prepared,
            commit_oid: None,
        };
        write_journal(root, &journal)?;
        journal
    };
    protect_confirmed_history(root, &journal, options, deadline)?;

    if matches!(journal.phase, JournalPhase::ReceiptDurable) {
        if load_valid_receipt(root, &journal).is_ok() {
            mark_shard_remote_confirmed(root, &journal)?;
            delete_confirmed_batch(batch)?;
            remove_journal(root)?;
            return Ok(journal.branch);
        }
        // A phase marker alone never authorizes deletion. Reconfirm the exact remote ref and
        // actual blobs, then replace the absent/corrupt receipt before touching the source.
        journal.phase = JournalPhase::Committed;
        write_journal(root, &journal)?;
    }

    let mut retries = 0_u8;
    loop {
        ensure_before_deadline(deadline)?;
        prepare_cache(root, &journal, options, deadline)?;
        if let Some(remote_oid) = verify_remote_batch(root, batch, &journal, options, deadline)? {
            finish_confirmed(root, batch, &mut journal, &remote_oid)?;
            return Ok(journal.branch);
        }
        if ensure_shard_capacity(root, batch, &mut journal, options, deadline)? {
            let _ = remove_managed_cache(root)?;
            continue;
        }
        apply_batch(root, batch, &journal)?;
        let oid = commit_batch(root, batch, &journal, options, deadline)?;
        journal.phase = JournalPhase::Committed;
        journal.commit_oid = Some(oid);
        write_journal(root, &journal)?;
        let push = git(
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
        );
        if let Some(remote_oid) = verify_remote_batch(root, batch, &journal, options, deadline)? {
            finish_confirmed(root, batch, &mut journal, &remote_oid)?;
            return Ok(journal.branch);
        }
        if retries >= options.max_retries {
            return match push {
                Err(error) => Err(error),
                Ok(_) => Err(SyncError::new(
                    SyncErrorCode::Git,
                    "push completed but the exact remote ref did not contain the batch",
                )),
            };
        }
        retries += 1;
        let _ = remove_managed_cache(root)?;
    }
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
        .map_err(|error| SyncError::new(SyncErrorCode::Io, error.to_string()))?;
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
    git(
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

fn ensure_shard_capacity(
    root: &Path,
    batch: &ReadyBatch,
    journal: &mut UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<bool, SyncError> {
    let (bytes, files) = tree_size(root, options, deadline)?;
    let incoming = batch
        .manifest
        .content_bytes
        .saturating_add(fs::metadata(&batch.manifest_path)?.len());
    if bytes.saturating_add(incoming) <= options.shard_max_bytes
        && files.saturating_add(2) <= options.shard_max_files
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
        .map_err(|error| SyncError::new(SyncErrorCode::Io, error.to_string()))?;
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

fn apply_batch(root: &Path, batch: &ReadyBatch, journal: &UploadJournal) -> Result<(), SyncError> {
    let repository = repository_path(root);
    for (source, relative) in [
        (&batch.events_path, &journal.events_relative_path),
        (&batch.manifest_path, &journal.manifest_relative_path),
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
                    format!("cache path {relative} is not a regular file"),
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

fn commit_batch(
    root: &Path,
    batch: &ReadyBatch,
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<String, SyncError> {
    let repository = repository_path(root);
    git(
        root,
        options,
        deadline,
        Some(&repository),
        [
            OsString::from("add"),
            OsString::from("--"),
            OsString::from(&journal.events_relative_path),
            OsString::from(&journal.manifest_relative_path),
        ],
        options.output_limit,
    )?;
    git_with_identity(
        root,
        options,
        deadline,
        &repository,
        assigned_email(&batch.manifest)?,
        [
            OsString::from("commit"),
            OsString::from("-m"),
            OsString::from(format!("logs: {}", batch.manifest.batch_id)),
        ],
    )?;
    let output = git(
        root,
        options,
        deadline,
        Some(&repository),
        [OsString::from("rev-parse"), OsString::from("HEAD")],
        options.output_limit,
    )?;
    parse_oid(&output.stdout)
}

fn verify_remote_batch(
    root: &Path,
    batch: &ReadyBatch,
    journal: &UploadJournal,
    options: &SyncOptions,
    deadline: Instant,
) -> Result<Option<String>, SyncError> {
    if !remote_ref_exists(root, &journal.remote, &journal.branch, options, deadline)? {
        return Ok(None);
    }
    fetch_exact(root, &journal.branch, options, deadline)?;
    let remote_oid = git(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [OsString::from("rev-parse"), OsString::from("FETCH_HEAD")],
        options.output_limit,
    )?;
    let remote_oid = parse_oid(&remote_oid.stdout)?;
    let events = git_allow_failure(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [
            OsString::from("show"),
            OsString::from(format!("FETCH_HEAD:{}", journal.events_relative_path)),
        ],
        usize::try_from(batch.manifest.content_bytes)
            .unwrap_or(usize::MAX)
            .saturating_add(1),
    )?;
    let manifest = git_allow_failure(
        root,
        options,
        deadline,
        Some(&repository_path(root)),
        [
            OsString::from("show"),
            OsString::from(format!("FETCH_HEAD:{}", journal.manifest_relative_path)),
        ],
        1024 * 1024,
    )?;
    if !events.status.success() {
        if manifest.status.success() {
            return Err(SyncError::new(
                SyncErrorCode::Conflict,
                "the remote contains only part of the batch path pair",
            ));
        }
        return Ok(None);
    }
    if events.stdout_truncated || sha256_bytes(&events.stdout) != batch.manifest.content_sha256 {
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
        serde_json::from_slice(&manifest.stdout).map_err(|error| {
            SyncError::new(
                SyncErrorCode::Conflict,
                format!("the remote batch manifest is invalid: {error}"),
            )
        })?;
    if remote_manifest != batch.manifest {
        return Err(SyncError::new(
            SyncErrorCode::Conflict,
            "the remote batch manifest does not match the sealed batch",
        ));
    }
    Ok(Some(remote_oid))
}

fn finish_confirmed(
    root: &Path,
    batch: &ReadyBatch,
    journal: &mut UploadJournal,
    remote_oid: &str,
) -> Result<(), SyncError> {
    journal.phase = JournalPhase::RemoteConfirmed;
    journal.commit_oid = Some(remote_oid.to_owned());
    write_journal(root, journal)?;
    let receipt = Receipt {
        schema_version: STATE_SCHEMA_VERSION,
        batch_id: journal.batch_id.clone(),
        stream_id: journal.stream_id.clone(),
        remote_digest: journal.remote_digest.clone(),
        branch: journal.branch.clone(),
        content_sha256: journal.content_sha256.clone(),
        remote_commit_oid: remote_oid.to_owned(),
        confirmed_at_unix_ms: now_unix_millis(),
    };
    let path = receipt_path(root, &journal.batch_id);
    fs::create_dir_all(path.parent().expect("receipt has parent"))?;
    atomic_write_json(&path, &receipt)
        .map_err(|error| SyncError::new(SyncErrorCode::Io, error.to_string()))?;
    journal.phase = JournalPhase::ReceiptDurable;
    write_journal(root, journal)?;
    mark_shard_remote_confirmed(root, journal)?;
    delete_confirmed_batch(batch)?;
    remove_journal(root)?;
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
        return Err(git_failure("ls-remote", &output.stderr));
    }
    Ok(!output.stdout.is_empty())
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
        return Err(git_failure("ls-remote", &output.stderr));
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
    let output = git_allow_failure(root, options, deadline, current_dir, args, output_limit)?;
    if output.status.success() {
        if cache_marker_path(root).is_file() {
            ensure_storage_headroom_after_git(root, options)?;
        }
        Ok(output)
    } else {
        Err(git_failure("git", &output.stderr))
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
    run(&spec).map_err(runner_error)
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

fn runner_error(error: RunnerError) -> SyncError {
    match error {
        RunnerError::TimedOut { .. } => SyncError::new(
            SyncErrorCode::Timeout,
            "Git exceeded the synchronization deadline",
        ),
        RunnerError::ResourceLimit { .. } => SyncError::new(
            SyncErrorCode::StorageBudget,
            "Git was stopped after reaching the local storage guard; pending spool was preserved",
        ),
        other => SyncError::new(SyncErrorCode::Git, other.to_string()),
    }
}

fn git_failure(operation: &str, stderr: &[u8]) -> SyncError {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    let message = if detail.is_empty() {
        format!("{operation} failed")
    } else {
        format!("{operation} failed: {detail}")
    };
    SyncError::new(SyncErrorCode::Git, message)
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
        SyncError::new(
            SyncErrorCode::InvalidRoot,
            format!("logs root is unavailable: {error}"),
        )
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
        Ok(()) => Ok(Some(SyncLock { _file: file })),
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
    Ok(Some(journal))
}

fn write_journal(root: &Path, journal: &UploadJournal) -> Result<(), SyncError> {
    atomic_write_json(&journal_path(root), journal)
        .map_err(|error| SyncError::new(SyncErrorCode::Io, error.to_string()))
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

fn load_valid_receipt(root: &Path, journal: &UploadJournal) -> Result<Receipt, SyncError> {
    let receipt: Receipt = read_json(&receipt_path(root, &journal.batch_id))?;
    if receipt.schema_version != STATE_SCHEMA_VERSION
        || receipt.batch_id != journal.batch_id
        || receipt.stream_id != journal.stream_id
        || receipt.remote_digest != journal.remote_digest
        || receipt.branch != journal.branch
        || receipt.content_sha256 != journal.content_sha256
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
        .map_err(|error| SyncError::new(SyncErrorCode::Io, error.to_string()))
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
        .map_err(|error| SyncError::new(SyncErrorCode::Io, error.to_string()))
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
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.path().symlink_metadata()?;
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
    serde_json::from_slice(&bytes).map_err(|error| {
        SyncError::new(
            SyncErrorCode::InvalidBatch,
            format!("invalid state file {}: {error}", path.display()),
        )
    })
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
        let directory = root.join("spool/ready").join(batch_id);
        fs::create_dir(&directory).unwrap();
        let event = json!({
            "schema_version": 1,
            "installation_id": "install-a",
            "stream_id": "stream-a",
            "email": "alice@example.com",
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
                stream_id: "stream-a".to_owned(),
                installation_id: "install-a".to_owned(),
                email: Some("alice@example.com".to_owned()),
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
