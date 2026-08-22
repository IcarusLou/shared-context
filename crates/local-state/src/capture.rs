use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use sctx_domain::{
    ArtifactLocator, ArtifactRef, CaptureId, Error, ErrorKind, ExternalSessionLocator, Result,
    TaskId, TaskIntentRevisionId, TaskSessionId, WorkEpisodeId,
};
use serde::{Deserialize, Serialize};

use crate::{PrivacyFindingKind, PrivacyScanner, RepositoryCatalogSnapshot};

const MAX_LIST_LIMIT: usize = 256;

/// Typed on-disk Capture schema version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureRecordVersion {
    V2,
}

/// Normalized breadcrumb categories. There is intentionally no Transcript kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreadcrumbKind {
    FileAccess,
    TestResult,
    ToolOutcome,
    Checkpoint,
}

/// Exact active Task ownership resolved when a Breadcrumb was captured.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureTaskOwner {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
}

/// Safe diagnostic retained with a Capture instead of guessing ownership/path identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureDiagnosticKind {
    NoActiveTask,
    RuntimeUnavailable,
    RepositoryNotConfigured,
    UnsafeArtifactPath,
}

/// A short structured input. Raw Agent payloads and transcripts are not accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Breadcrumb {
    pub external_session_locator: ExternalSessionLocator,
    pub task_owner: Option<CaptureTaskOwner>,
    pub kind: BreadcrumbKind,
    pub summary: String,
    pub workspace_hint: Option<PathBuf>,
    pub file_hints: Vec<PathBuf>,
    pub diagnostics: Vec<CaptureDiagnosticKind>,
}

/// TTL and byte ceilings for local capture state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturePolicy {
    pub ttl: Duration,
    pub max_entry_bytes: usize,
    pub max_total_bytes: u64,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(24 * 60 * 60),
            max_entry_bytes: 64 * 1024,
            max_total_bytes: 10 * 1024 * 1024,
        }
    }
}

impl CapturePolicy {
    fn validate(self) -> Result<()> {
        if self.ttl.is_zero() {
            return Err(invalid("capture TTL must be greater than zero"));
        }
        if self.max_entry_bytes == 0 || self.max_total_bytes == 0 {
            return Err(invalid("capture byte limits must be greater than zero"));
        }
        if self.max_entry_bytes as u64 > self.max_total_bytes {
            return Err(invalid(
                "capture max_entry_bytes must not exceed max_total_bytes",
            ));
        }
        Ok(())
    }
}

/// Exact Episode/Task reservation for one Capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureClaim {
    pub episode_id: WorkEpisodeId,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
}

/// Safe redacted Capture record returned only through local APIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRecord {
    pub version: CaptureRecordVersion,
    pub capture_id: CaptureId,
    pub recorded_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
    pub external_session_locator: ExternalSessionLocator,
    pub task_owner: Option<CaptureTaskOwner>,
    pub kind: BreadcrumbKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub privacy_findings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<CaptureDiagnosticKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<CaptureClaim>,
}

impl CaptureRecord {
    fn validate(&self) -> Result<()> {
        self.external_session_locator.validate()?;
        if self.summary.trim().is_empty() {
            return Err(invalid("capture summary must not be empty"));
        }
        if self.expires_at_unix_seconds <= self.recorded_at_unix_seconds {
            return Err(invalid("capture expiry must follow its recorded time"));
        }
        if self.task_owner.is_none()
            && !self.diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic,
                    CaptureDiagnosticKind::NoActiveTask | CaptureDiagnosticKind::RuntimeUnavailable
                )
            })
        {
            return Err(invalid(
                "ownerless Capture requires a typed ownership diagnostic",
            ));
        }
        if let Some(claim) = self.claim {
            let owner = self
                .task_owner
                .ok_or_else(|| invalid("ownerless Capture cannot be claimed"))?;
            if owner.task_session_id != claim.task_session_id || owner.task_id != claim.task_id {
                return Err(invalid("Capture claim must match its exact Task owner"));
            }
        }
        if self.file_hints.iter().any(|path| path.trim().is_empty()) {
            return Err(invalid("capture file hints must not be empty"));
        }
        Ok(())
    }
}

/// Safe metadata returned after storing a breadcrumb.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureReceipt {
    pub capture_id: CaptureId,
    pub path: PathBuf,
    pub expires_at_unix_seconds: u64,
    pub finding_kinds: Vec<PrivacyFindingKind>,
    pub diagnostics: Vec<CaptureDiagnosticKind>,
}

/// One bounded Capture read with explicit TTL state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureRead {
    pub record: CaptureRecord,
    pub expired: bool,
}

/// Typed diagnosis for an unsafe/invalid entry preserved by list/cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureStoreDiagnostic {
    pub capture_id: Option<CaptureId>,
    pub kind: CaptureStoreDiagnosticKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureStoreDiagnosticKind {
    InvalidRecord,
    UnsafeEntry,
    Oversized,
}

/// Bounded deterministic Capture listing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureListReport {
    pub captures: Vec<CaptureRead>,
    pub diagnostics: Vec<CaptureStoreDiagnostic>,
    pub truncated: bool,
}

/// Idempotent result of reserving one Capture for an Episode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureClaimOutcome {
    pub record: CaptureRecord,
    pub newly_claimed: bool,
}

/// Safe `ArtifactRefs` and typed path diagnostics derived from Capture file hints.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureArtifactMapping {
    pub artifact_refs: Vec<ArtifactRef>,
    pub diagnostics: Vec<CaptureDiagnosticKind>,
}

/// Maps only existing safe Capture file hints through the explicit Repository Catalog.
///
/// Unconfigured or unsafe hints remain Capture provenance plus typed diagnostics;
/// no Repository identity is guessed.
#[must_use]
pub fn map_capture_artifacts(
    record: &CaptureRecord,
    catalog: &RepositoryCatalogSnapshot,
) -> CaptureArtifactMapping {
    let mut mapping = CaptureArtifactMapping::default();
    let Some(workspace_hint) = record.workspace_hint.as_deref().map(PathBuf::from) else {
        if !record.file_hints.is_empty() {
            mapping
                .diagnostics
                .push(CaptureDiagnosticKind::UnsafeArtifactPath);
        }
        return mapping;
    };
    for file_hint in &record.file_hints {
        match catalog.resolve_file_path(Path::new(file_hint), std::slice::from_ref(&workspace_hint))
        {
            Ok(resolved) => {
                let artifact = ArtifactRef {
                    repository_id: resolved.repository_id,
                    locator: ArtifactLocator::File {
                        path: resolved.relative_path,
                    },
                };
                if !mapping.artifact_refs.contains(&artifact) {
                    mapping.artifact_refs.push(artifact);
                }
            }
            Err(error) if error.kind() == ErrorKind::RepositoryNotConfigured => mapping
                .diagnostics
                .push(CaptureDiagnosticKind::RepositoryNotConfigured),
            Err(_) => mapping
                .diagnostics
                .push(CaptureDiagnosticKind::UnsafeArtifactPath),
        }
    }
    mapping.diagnostics.sort();
    mapping.diagnostics.dedup();
    mapping
}

/// Result of TTL cleanup. Invalid/symlink entries are diagnosed and preserved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CleanupReport {
    pub removed: Vec<PathBuf>,
    pub diagnostics: Vec<CaptureStoreDiagnostic>,
    pub reclaimed_bytes: u64,
}

/// Private, bounded `state/capture` storage outside the Git repository.
#[derive(Clone, Debug)]
pub struct CaptureStore {
    directory: PathBuf,
    lock_path: PathBuf,
    policy: CapturePolicy,
    scanner: PrivacyScanner,
}

impl CaptureStore {
    /// Opens capture storage under `<root>/state/capture` with default limits.
    ///
    /// # Errors
    ///
    /// Refuses symlinked state paths and permission setup failures.
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self> {
        Self::with_policy(root, CapturePolicy::default())
    }

    /// Opens capture storage with explicit TTL and size policy.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid policy or an I/O error when the
    /// private directory cannot be secured to mode `0700`.
    pub fn with_policy(root: impl AsRef<Path>, policy: CapturePolicy) -> Result<Self> {
        policy.validate()?;
        let root = std::path::absolute(root.as_ref()).map_err(io_error("make root absolute"))?;
        let state = root.join("state");
        ensure_private_directory(&root)?;
        ensure_private_directory(&state)?;
        let directory = state.join("capture");
        ensure_private_directory(&directory)?;
        Ok(Self {
            directory,
            lock_path: state.join("capture.lock"),
            policy,
            scanner: PrivacyScanner::default(),
        })
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Redacts supported Secret/PII signatures and stores one normalized breadcrumb.
    ///
    /// # Errors
    ///
    /// Rejects oversized entries/aggregate state, invalid ownership, or unsafe
    /// filesystem entries. Raw input is never included in diagnostics.
    pub fn capture(&self, breadcrumb: &Breadcrumb) -> Result<CaptureReceipt> {
        self.capture_at(breadcrumb, SystemTime::now())
    }

    /// Reads one typed Capture by ID without following filesystem input.
    ///
    /// # Errors
    ///
    /// Rejects missing, unsafe, oversized, mismatched, or invalid records.
    pub fn read(&self, capture_id: CaptureId) -> Result<CaptureRead> {
        let lock = self.lock()?;
        let read = self.read_at(capture_id, SystemTime::now())?;
        FileExt::unlock(&lock).map_err(io_error("unlock capture.lock"))?;
        Ok(read)
    }

    /// Lists at most `limit` valid Capture records plus typed unsafe diagnostics.
    ///
    /// # Errors
    ///
    /// Rejects an invalid bound or inaccessible directory.
    pub fn list(&self, limit: usize) -> Result<CaptureListReport> {
        if limit == 0 || limit > MAX_LIST_LIMIT {
            return Err(invalid(format!(
                "capture list limit must be between 1 and {MAX_LIST_LIMIT}"
            )));
        }
        let lock = self.lock()?;
        let report = self.list_at(limit, SystemTime::now())?;
        FileExt::unlock(&lock).map_err(io_error("unlock capture.lock"))?;
        Ok(report)
    }

    /// Idempotently reserves a Capture for its exact owned Work Episode.
    ///
    /// A claim is retained after crashes and never removes the source record.
    /// Runtime ingestion remains independently idempotent by `CaptureId`.
    ///
    /// # Errors
    ///
    /// Rejects expiry, ownerless, cross-Task, cross-Episode, or unsafe records.
    pub fn claim(&self, capture_id: CaptureId, claim: CaptureClaim) -> Result<CaptureClaimOutcome> {
        let lock = self.lock()?;
        let mut read = self.read_at(capture_id, SystemTime::now())?;
        if read.expired {
            return Err(invalid("expired Capture cannot be claimed"));
        }
        let owner = read
            .record
            .task_owner
            .ok_or_else(|| invalid("Capture has no ActiveTask owner"))?;
        if owner.task_session_id != claim.task_session_id || owner.task_id != claim.task_id {
            return Err(invalid("Capture cannot be claimed by another Task"));
        }
        let newly_claimed = match read.record.claim {
            None => {
                read.record.claim = Some(claim);
                true
            }
            Some(existing) if existing == claim => false,
            Some(_) => return Err(invalid("Capture is already claimed by another Episode")),
        };
        if newly_claimed {
            read.record.validate()?;
            self.replace_record(&read.record)?;
        }
        FileExt::unlock(&lock).map_err(io_error("unlock capture.lock"))?;
        Ok(CaptureClaimOutcome {
            record: read.record,
            newly_claimed,
        })
    }

    /// Removes only expired valid capture records from `state/capture`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the directory cannot be inspected or synchronized.
    pub fn cleanup_expired(&self) -> Result<CleanupReport> {
        let lock = self.lock()?;
        let report = self.cleanup_expired_at(SystemTime::now())?;
        FileExt::unlock(&lock).map_err(io_error("unlock capture.lock"))?;
        Ok(report)
    }

    fn capture_at(&self, breadcrumb: &Breadcrumb, now: SystemTime) -> Result<CaptureReceipt> {
        breadcrumb.external_session_locator.validate()?;
        if breadcrumb.summary.trim().is_empty() {
            return Err(invalid("breadcrumb summary must not be empty"));
        }
        let lock = self.lock()?;
        let now_seconds = unix_seconds(now)?;
        let expires_at = now
            .checked_add(self.policy.ttl)
            .ok_or_else(|| invalid("capture TTL overflows system time"))?;
        let expires_at_seconds = unix_seconds(expires_at)?;
        let mut finding_kinds = BTreeSet::new();
        let summary = self.redact_field(&breadcrumb.summary, &mut finding_kinds)?;
        let workspace_hint = breadcrumb
            .workspace_hint
            .as_deref()
            .map(path_text)
            .transpose()?
            .map(|path| self.redact_field(&path, &mut finding_kinds))
            .transpose()?;
        let file_hints = breadcrumb
            .file_hints
            .iter()
            .map(|path| {
                let path = path_text(path)?;
                self.redact_field(&path, &mut finding_kinds)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut diagnostics = breadcrumb
            .diagnostics
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if breadcrumb.task_owner.is_none()
            && !diagnostics.contains(&CaptureDiagnosticKind::RuntimeUnavailable)
        {
            diagnostics.insert(CaptureDiagnosticKind::NoActiveTask);
        }
        let capture_id = CaptureId::new();
        let record = CaptureRecord {
            version: CaptureRecordVersion::V2,
            capture_id,
            recorded_at_unix_seconds: now_seconds,
            expires_at_unix_seconds: expires_at_seconds,
            external_session_locator: breadcrumb.external_session_locator.clone(),
            task_owner: breadcrumb.task_owner,
            kind: breadcrumb.kind,
            summary,
            workspace_hint,
            file_hints,
            privacy_findings: finding_kinds
                .iter()
                .map(|kind| kind.code().to_owned())
                .collect(),
            diagnostics: diagnostics.into_iter().collect(),
            claim: None,
        };
        record.validate()?;
        let bytes = self.serialize_record(&record)?;
        self.cleanup_expired_at(now)?;
        let current_size = self.current_size()?;
        if current_size.saturating_add(bytes.len() as u64) > self.policy.max_total_bytes {
            return Err(invalid(format!(
                "capture state would exceed aggregate limit of {} bytes",
                self.policy.max_total_bytes
            )));
        }
        let path = self.capture_path(capture_id);
        write_private_new(&path, &bytes)?;
        sync_directory(&self.directory)?;
        FileExt::unlock(&lock).map_err(io_error("unlock capture.lock"))?;
        Ok(CaptureReceipt {
            capture_id,
            path,
            expires_at_unix_seconds: expires_at_seconds,
            finding_kinds: finding_kinds.into_iter().collect(),
            diagnostics: record.diagnostics,
        })
    }

    fn read_at(&self, capture_id: CaptureId, now: SystemTime) -> Result<CaptureRead> {
        let path = self.capture_path(capture_id);
        let record = self.read_record_path(&path)?;
        if record.capture_id != capture_id {
            return Err(invariant("Capture filename and record identity differ"));
        }
        Ok(CaptureRead {
            expired: record.expires_at_unix_seconds <= unix_seconds(now)?,
            record,
        })
    }

    fn list_at(&self, limit: usize, now: SystemTime) -> Result<CaptureListReport> {
        let mut entries = sorted_entries(&self.directory)?;
        let truncated = entries.len() > limit;
        entries.truncate(limit);
        let now = unix_seconds(now)?;
        let mut report = CaptureListReport {
            truncated,
            ..CaptureListReport::default()
        };
        for entry in entries {
            let path = entry.path();
            let capture_id = capture_id_from_path(&path);
            match self.read_record_path(&path) {
                Ok(record) => report.captures.push(CaptureRead {
                    expired: record.expires_at_unix_seconds <= now,
                    record,
                }),
                Err(error) => report.diagnostics.push(CaptureStoreDiagnostic {
                    capture_id,
                    kind: diagnostic_kind(&path, self.policy.max_entry_bytes, &error),
                }),
            }
        }
        Ok(report)
    }

    fn cleanup_expired_at(&self, now: SystemTime) -> Result<CleanupReport> {
        let now = unix_seconds(now)?;
        let mut report = CleanupReport::default();
        for entry in sorted_entries(&self.directory)? {
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(io_error("inspect capture entry"))?;
            let capture_id = capture_id_from_path(&path);
            let record = match self.read_record_path(&path) {
                Ok(record) => record,
                Err(error) => {
                    report.diagnostics.push(CaptureStoreDiagnostic {
                        capture_id,
                        kind: diagnostic_kind(&path, self.policy.max_entry_bytes, &error),
                    });
                    continue;
                }
            };
            if record.expires_at_unix_seconds <= now {
                fs::remove_file(&path).map_err(io_error("remove expired capture"))?;
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(metadata.len());
                report.removed.push(path);
            }
        }
        if !report.removed.is_empty() {
            sync_directory(&self.directory)?;
        }
        Ok(report)
    }

    fn read_record_path(&self, path: &Path) -> Result<CaptureRecord> {
        let metadata = fs::symlink_metadata(path).map_err(io_error("inspect capture record"))?;
        if !metadata.file_type().is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            return Err(invalid("unsafe capture entry"));
        }
        if metadata.len() > self.policy.max_entry_bytes as u64 {
            return Err(invalid("oversized capture entry"));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid("capture entry permissions are not private"));
        }
        let mut file = File::open(path).map_err(io_error("open capture record"))?;
        let capacity = usize::try_from(metadata.len())
            .map_err(|_| invalid("capture entry length exceeds platform bounds"))?;
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)
            .map_err(io_error("read capture record"))?;
        let record: CaptureRecord =
            serde_json::from_slice(&bytes).map_err(|_| invalid("capture record is invalid"))?;
        record.validate()?;
        Ok(record)
    }

    fn replace_record(&self, record: &CaptureRecord) -> Result<()> {
        let path = self.capture_path(record.capture_id);
        reject_symlink(&path)?;
        let bytes = self.serialize_record(record)?;
        let temporary = self.directory.join(format!(
            ".{}.{}.tmp",
            record.capture_id,
            uuid::Uuid::new_v4()
        ));
        write_private_new(&temporary, &bytes)?;
        fs::rename(&temporary, &path).map_err(io_error("replace capture record"))?;
        sync_directory(&self.directory)
    }

    fn serialize_record(&self, record: &CaptureRecord) -> Result<Vec<u8>> {
        let mut bytes =
            serde_json::to_vec_pretty(record).map_err(|_| invalid("serialize capture record"))?;
        bytes.push(b'\n');
        if bytes.len() > self.policy.max_entry_bytes {
            return Err(invalid(format!(
                "capture record is {} bytes; entry limit is {} bytes",
                bytes.len(),
                self.policy.max_entry_bytes
            )));
        }
        Ok(bytes)
    }

    fn redact_field(
        &self,
        value: &str,
        findings: &mut BTreeSet<PrivacyFindingKind>,
    ) -> Result<String> {
        let redacted = self.scanner.redact(value)?;
        findings.extend(redacted.finding_kinds);
        Ok(redacted.text)
    }

    fn capture_path(&self, capture_id: CaptureId) -> PathBuf {
        self.directory.join(format!("{capture_id}.json"))
    }

    fn current_size(&self) -> Result<u64> {
        let mut size = 0_u64;
        for entry in fs::read_dir(&self.directory).map_err(io_error("read capture directory"))? {
            let entry = entry.map_err(io_error("read capture entry"))?;
            let metadata =
                fs::symlink_metadata(entry.path()).map_err(io_error("inspect capture entry"))?;
            if metadata.file_type().is_file() {
                size = size.saturating_add(metadata.len());
            }
        }
        Ok(size)
    }

    fn lock(&self) -> Result<File> {
        reject_symlink(&self.lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock_path)
            .map_err(io_error("open capture.lock"))?;
        fs::set_permissions(&self.lock_path, fs::Permissions::from_mode(0o600))
            .map_err(io_error("set capture.lock permissions"))?;
        lock.lock_exclusive()
            .map_err(io_error("lock capture.lock"))?;
        Ok(lock)
    }
}

fn sorted_entries(directory: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(directory)
        .map_err(io_error("read capture directory"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(io_error("read capture entry"))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn capture_id_from_path(path: &Path) -> Option<CaptureId> {
    path.file_stem()?.to_str()?.parse().ok()
}

fn diagnostic_kind(
    path: &Path,
    max_entry_bytes: usize,
    _error: &Error,
) -> CaptureStoreDiagnosticKind {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.len() > max_entry_bytes as u64 => {
            CaptureStoreDiagnosticKind::Oversized
        }
        Ok(metadata) if !metadata.file_type().is_file() => CaptureStoreDiagnosticKind::UnsafeEntry,
        _ => CaptureStoreDiagnosticKind::InvalidRecord,
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(io_error("inspect private directory"))?;
        if !metadata.file_type().is_dir() {
            return Err(invariant(format!(
                "private state path is not a directory: {}",
                path.display()
            )));
        }
    } else {
        fs::create_dir_all(path).map_err(io_error("create private directory"))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set private directory permissions"))
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invariant(format!(
            "refusing symlinked private state path: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("inspect {}: {error}", path.display()),
        )),
    }
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error("create capture record"))?;
    file.write_all(bytes)
        .map_err(io_error("write capture record"))?;
    file.sync_all().map_err(io_error("sync capture record"))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("sync directory"))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        invalid(format!(
            "breadcrumb path is not valid UTF-8: {}",
            path.display()
        ))
    })
}

fn unix_seconds(time: SystemTime) -> Result<u64> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| invalid("capture timestamp is before the Unix epoch"))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    use sctx_domain::{
        ExternalSessionLocator, TaskId, TaskIntentRevisionId, TaskSessionId, WorkEpisodeId,
    };
    use tempfile::tempdir;

    use super::{
        Breadcrumb, BreadcrumbKind, CaptureClaim, CaptureDiagnosticKind, CapturePolicy,
        CaptureStore, CaptureStoreDiagnosticKind, CaptureTaskOwner,
    };

    fn locator(key: &str) -> ExternalSessionLocator {
        ExternalSessionLocator::new("codex", key).unwrap()
    }

    fn owned_breadcrumb(summary: &str) -> Breadcrumb {
        Breadcrumb {
            external_session_locator: locator("capture-test"),
            task_owner: Some(CaptureTaskOwner {
                task_session_id: TaskSessionId::new(),
                task_id: TaskId::new(),
                intent_revision_id: TaskIntentRevisionId::new(),
            }),
            kind: BreadcrumbKind::TestResult,
            summary: summary.to_owned(),
            workspace_hint: Some(PathBuf::from("/tmp/项目 空格")),
            file_hints: vec![PathBuf::from("src/模块.rs")],
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn capture_redacts_and_uses_typed_identity_private_permissions_and_no_raw_fields() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("Shared Context 空格");
        let store = CaptureStore::initialize(&root).unwrap();
        let receipt = store
            .capture(&owned_breadcrumb(
                "passed; contact alice@example.com; password=not-a-real-password",
            ))
            .unwrap();
        let stored = fs::read_to_string(&receipt.path).unwrap();

        assert!(receipt.capture_id.to_string().starts_with("cap_"));
        assert!(!stored.contains("alice@example.com"));
        assert!(!stored.contains("not-a-real-password"));
        assert!(stored.contains("[REDACTED:email_address]"));
        for forbidden in ["transcript", "command", "tool_output", "tool_response"] {
            assert!(!stored.contains(forbidden));
        }
        assert_eq!(
            store.read(receipt.capture_id).unwrap().record.capture_id,
            receipt.capture_id
        );
        assert_eq!(
            fs::metadata(store.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(receipt.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn ownerless_capture_is_diagnostic_and_cannot_be_claimed() {
        let temporary = tempdir().unwrap();
        let store = CaptureStore::initialize(temporary.path()).unwrap();
        let receipt = store
            .capture(&Breadcrumb {
                external_session_locator: locator("no-task"),
                task_owner: None,
                kind: BreadcrumbKind::Checkpoint,
                summary: "session existed before Task".to_owned(),
                workspace_hint: None,
                file_hints: Vec::new(),
                diagnostics: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            receipt.diagnostics,
            vec![CaptureDiagnosticKind::NoActiveTask]
        );
        assert!(
            store
                .claim(
                    receipt.capture_id,
                    CaptureClaim {
                        episode_id: WorkEpisodeId::new(),
                        task_session_id: TaskSessionId::new(),
                        task_id: TaskId::new(),
                    },
                )
                .is_err()
        );
        store
            .capture(&Breadcrumb {
                external_session_locator: locator("no-task-2"),
                task_owner: None,
                kind: BreadcrumbKind::Checkpoint,
                summary: "second ownerless Capture".to_owned(),
                workspace_hint: None,
                file_hints: Vec::new(),
                diagnostics: Vec::new(),
            })
            .unwrap();
        let bounded = store.list(1).unwrap();
        assert_eq!(bounded.captures.len(), 1);
        assert!(bounded.truncated);
        assert!(store.list(0).is_err());
        assert!(store.list(257).is_err());
    }

    #[test]
    fn claim_is_idempotent_for_same_episode_and_rejects_cross_task_or_episode() {
        let temporary = tempdir().unwrap();
        let store = CaptureStore::initialize(temporary.path()).unwrap();
        let breadcrumb = owned_breadcrumb("verified result");
        let owner = breadcrumb.task_owner.unwrap();
        let receipt = store.capture(&breadcrumb).unwrap();
        let claim = CaptureClaim {
            episode_id: WorkEpisodeId::new(),
            task_session_id: owner.task_session_id,
            task_id: owner.task_id,
        };
        assert!(
            store
                .claim(receipt.capture_id, claim)
                .unwrap()
                .newly_claimed
        );
        assert!(
            !store
                .claim(receipt.capture_id, claim)
                .unwrap()
                .newly_claimed
        );
        assert!(
            store
                .claim(
                    receipt.capture_id,
                    CaptureClaim {
                        episode_id: WorkEpisodeId::new(),
                        ..claim
                    },
                )
                .is_err()
        );
        assert!(
            store
                .claim(
                    receipt.capture_id,
                    CaptureClaim {
                        task_id: TaskId::new(),
                        ..claim
                    },
                )
                .is_err()
        );

        let concurrent_receipt = store.capture(&breadcrumb).unwrap();
        let store = Arc::new(store);
        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut threads = Vec::new();
        for _ in 0..workers {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                store.claim(concurrent_receipt.capture_id, claim).unwrap()
            }));
        }
        let outcomes = threads
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.newly_claimed)
                .count(),
            1
        );
    }

    #[test]
    fn expired_cleanup_and_invalid_symlink_entries_are_bounded_and_preserved() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("home");
        let repository = root.join("repository");
        fs::create_dir_all(&repository).unwrap();
        let git_fact = repository.join("fact.json");
        fs::write(&git_fact, "immutable").unwrap();
        let store = CaptureStore::with_policy(
            &root,
            CapturePolicy {
                ttl: Duration::from_secs(1),
                max_entry_bytes: 4096,
                max_total_bytes: 8192,
            },
        )
        .unwrap();
        let recorded_at = std::time::UNIX_EPOCH + Duration::from_secs(100);
        let receipt = store
            .capture_at(&owned_breadcrumb("short lived"), recorded_at)
            .unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&git_fact, store.directory().join("unsafe.json")).unwrap();
        }

        let report = store
            .cleanup_expired_at(recorded_at + Duration::from_secs(2))
            .unwrap();

        assert_eq!(report.removed, vec![receipt.path]);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.kind == CaptureStoreDiagnosticKind::UnsafeEntry })
        );
        assert_eq!(fs::read_to_string(git_fact).unwrap(), "immutable");
        assert!(store.directory().join("unsafe.json").exists());
    }

    #[test]
    fn entry_and_aggregate_byte_limits_are_enforced_before_write() {
        let temporary = tempdir().unwrap();
        let store = CaptureStore::with_policy(
            temporary.path(),
            CapturePolicy {
                ttl: Duration::from_secs(60),
                max_entry_bytes: 700,
                max_total_bytes: 850,
            },
        )
        .unwrap();
        let error = store
            .capture(&owned_breadcrumb(&"x".repeat(2_000)))
            .unwrap_err();
        assert!(error.message().contains("entry limit"));
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 0);

        let breadcrumb = owned_breadcrumb(&"x".repeat(60));
        store.capture(&breadcrumb).unwrap();
        assert!(store.capture(&breadcrumb).is_err());
    }
}
