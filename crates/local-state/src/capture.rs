use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use sctx_domain::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

use crate::{PrivacyFindingKind, PrivacyScanner};

const CAPTURE_VERSION: u32 = 1;

/// Normalized breadcrumb categories. There is intentionally no Transcript kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreadcrumbKind {
    FileAccess,
    TestResult,
    ToolOutcome,
    Checkpoint,
}

/// A short, structured observation. Raw Agent payloads and transcripts are not accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Breadcrumb {
    pub kind: BreadcrumbKind,
    pub summary: String,
    pub workspace_hint: Option<PathBuf>,
    pub file_hints: Vec<PathBuf>,
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

/// Safe metadata returned after storing a breadcrumb.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureReceipt {
    pub capture_id: String,
    pub path: PathBuf,
    pub expires_at_unix_seconds: u64,
    pub finding_kinds: Vec<PrivacyFindingKind>,
}

/// Result of TTL cleanup. Invalid/symlink entries are diagnosed and preserved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CleanupReport {
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub reclaimed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBreadcrumb {
    version: u32,
    capture_id: String,
    recorded_at_unix_seconds: u64,
    expires_at_unix_seconds: u64,
    kind: BreadcrumbKind,
    summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    file_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    privacy_findings: Vec<String>,
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

    /// Capture directory. It is always outside `<root>/repository`.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Redacts supported Secret/PII signatures and stores one normalized breadcrumb.
    ///
    /// # Errors
    ///
    /// Rejects oversized entries/aggregate state, invalid paths, or unsafe
    /// filesystem entries. Raw input is never included in error diagnostics.
    pub fn capture(&self, breadcrumb: &Breadcrumb) -> Result<CaptureReceipt> {
        self.capture_at(breadcrumb, SystemTime::now())
    }

    /// Removes only expired, valid capture records from `state/capture`.
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
        let lock = self.lock()?;
        if breadcrumb.summary.trim().is_empty() {
            return Err(invalid("breadcrumb summary must not be empty"));
        }
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

        let capture_id = format!("cap_{}", uuid::Uuid::new_v4().hyphenated());
        let record = StoredBreadcrumb {
            version: CAPTURE_VERSION,
            capture_id: capture_id.clone(),
            recorded_at_unix_seconds: now_seconds,
            expires_at_unix_seconds: expires_at_seconds,
            kind: breadcrumb.kind,
            summary,
            workspace_hint,
            file_hints,
            privacy_findings: finding_kinds
                .iter()
                .map(|kind| kind.code().to_owned())
                .collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&record).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("serialize capture record: {error}"),
            )
        })?;
        bytes.push(b'\n');
        if bytes.len() > self.policy.max_entry_bytes {
            return Err(invalid(format!(
                "capture record is {} bytes; entry limit is {} bytes",
                bytes.len(),
                self.policy.max_entry_bytes
            )));
        }

        self.cleanup_expired_at(now)?;
        let current_size = self.current_size()?;
        if current_size.saturating_add(bytes.len() as u64) > self.policy.max_total_bytes {
            return Err(invalid(format!(
                "capture state would exceed aggregate limit of {} bytes",
                self.policy.max_total_bytes
            )));
        }
        let path = self.directory.join(format!("{capture_id}.json"));
        write_private_new(&path, &bytes)?;
        sync_directory(&self.directory)?;
        FileExt::unlock(&lock).map_err(io_error("unlock capture.lock"))?;
        Ok(CaptureReceipt {
            capture_id,
            path,
            expires_at_unix_seconds: expires_at_seconds,
            finding_kinds: finding_kinds.into_iter().collect(),
        })
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

    fn cleanup_expired_at(&self, now: SystemTime) -> Result<CleanupReport> {
        let now = unix_seconds(now)?;
        let mut report = CleanupReport::default();
        let mut entries = fs::read_dir(&self.directory)
            .map_err(io_error("read capture directory"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(io_error("read capture entry"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(io_error("inspect capture entry"))?;
            if !metadata.file_type().is_file()
                || path.extension().and_then(|extension| extension.to_str()) != Some("json")
            {
                report.skipped.push(path);
                continue;
            }
            let record = match read_record(&path) {
                Ok(record) if record.version == CAPTURE_VERSION => record,
                Ok(_) | Err(_) => {
                    report.skipped.push(path);
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

fn read_record(path: &Path) -> Result<StoredBreadcrumb> {
    let mut file = File::open(path).map_err(io_error("open capture record"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(io_error("read capture record"))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("parse capture record {}: {error}", path.display()),
        )
    })
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
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

    use tempfile::tempdir;

    use super::{Breadcrumb, BreadcrumbKind, CapturePolicy, CaptureStore};

    #[test]
    fn capture_redacts_and_uses_private_permissions_without_transcript_field() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("Shared Context 空格");
        let store = CaptureStore::initialize(&root).unwrap();
        let receipt = store
            .capture(&Breadcrumb {
                kind: BreadcrumbKind::TestResult,
                summary: "passed; contact alice@example.com; password=not-a-real-password"
                    .to_owned(),
                workspace_hint: Some(PathBuf::from("/tmp/项目 空格")),
                file_hints: vec![PathBuf::from("src/模块.rs")],
            })
            .unwrap();
        let stored = fs::read_to_string(&receipt.path).unwrap();

        assert!(!stored.contains("alice@example.com"));
        assert!(!stored.contains("not-a-real-password"));
        assert!(stored.contains("[REDACTED:email_address]"));
        assert!(!stored.contains("transcript"));
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
    fn expired_cleanup_stays_inside_capture_directory() {
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
            .capture_at(
                &Breadcrumb {
                    kind: BreadcrumbKind::Checkpoint,
                    summary: "short lived".to_owned(),
                    workspace_hint: None,
                    file_hints: Vec::new(),
                },
                recorded_at,
            )
            .unwrap();

        let report = store
            .cleanup_expired_at(recorded_at + Duration::from_secs(2))
            .unwrap();

        assert_eq!(report.removed, vec![receipt.path]);
        assert_eq!(fs::read_to_string(git_fact).unwrap(), "immutable");
    }

    #[test]
    fn byte_limits_are_enforced_before_write() {
        let temporary = tempdir().unwrap();
        let store = CaptureStore::with_policy(
            temporary.path(),
            CapturePolicy {
                ttl: Duration::from_secs(60),
                max_entry_bytes: 180,
                max_total_bytes: 300,
            },
        )
        .unwrap();
        let error = store
            .capture(&Breadcrumb {
                kind: BreadcrumbKind::ToolOutcome,
                summary: "x".repeat(256),
                workspace_hint: None,
                file_hints: Vec::new(),
            })
            .unwrap_err();

        assert!(error.message().contains("entry limit"));
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 0);
    }

    #[test]
    fn aggregate_byte_limit_is_enforced() {
        let temporary = tempdir().unwrap();
        let store = CaptureStore::with_policy(
            temporary.path(),
            CapturePolicy {
                ttl: Duration::from_secs(60),
                max_entry_bytes: 600,
                max_total_bytes: 600,
            },
        )
        .unwrap();
        let breadcrumb = Breadcrumb {
            kind: BreadcrumbKind::Checkpoint,
            summary: "x".repeat(100),
            workspace_hint: None,
            file_hints: Vec::new(),
        };
        store.capture(&breadcrumb).unwrap();

        let error = store.capture(&breadcrumb).unwrap_err();

        assert!(error.message().contains("aggregate limit"));
    }
}
