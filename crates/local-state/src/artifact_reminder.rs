//! Bounded, best-effort "already reminded" state for the P4.1 Artifact focus
//! reminder experiment.
//!
//! The store holds no business fact: one file per external Session locator digest
//! records only which Repository-relative Artifacts this Session was already told
//! about. It never blocks: a busy, corrupt, oversized, or unwritable record is
//! treated as "not reminded yet" so the Hook stays neutral-or-better instead of
//! waiting for a lock.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use sctx_domain::{Error, ErrorKind, ExternalSessionLocator, Result};
use serde::{Deserialize, Serialize};

use crate::session_scope::locator_digest;

/// Maximum retained Artifact reminders per external Session. Older entries are
/// evicted first so a long Session stays bounded.
pub const ARTIFACT_REMINDER_MAX_ENTRIES: usize = 64;

/// Refuse to parse a record larger than this. A larger file is treated as
/// unusable local state, never as a partial decision.
const MAX_RECORD_BYTES: u64 = 32 * 1_024;

const RECORD_VERSION: u32 = 1;

/// Stable per-Task reminder identity of one located Artifact.
///
/// Only the Repository identity and its Repository-relative path participate;
/// no absolute path, checkout, Prompt, or Context content is stored.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactReminderKey(String);

impl ArtifactReminderKey {
    /// Builds the reminder key for one Repository-relative Artifact path.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an empty Repository or path.
    pub fn new(repository_id: &str, relative_path: &str) -> Result<Self> {
        if repository_id.trim().is_empty() || relative_path.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Artifact reminder key requires a Repository identity and relative path",
            ));
        }
        Ok(Self(format!("{repository_id}\u{1f}{relative_path}")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Result of one bounded deduplication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactReminderMark {
    /// This Session has not been reminded about the Artifact; emit the reminder.
    FirstReminder,
    /// This Session already saw the reminder; stay neutral.
    AlreadyReminded,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ReminderRecord {
    version: u32,
    /// Oldest first. Eviction is FIFO.
    artifacts: Vec<String>,
}

/// Bounded reminder deduplication under `state/artifact-reminders`.
#[derive(Clone, Debug)]
pub struct ArtifactReminderStore {
    directory: PathBuf,
}

impl ArtifactReminderStore {
    /// Opens `<root>/state/artifact-reminders`, creating it when absent.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem errors for an unusable private state directory.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let directory = root.as_ref().join("state").join("artifact-reminders");
        if let Ok(metadata) = fs::symlink_metadata(&directory) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::new(
                    ErrorKind::InvariantViolation,
                    "Artifact reminder state must be a non-symlink directory",
                ));
            }
        } else {
            fs::create_dir_all(&directory)
                .map_err(io_error("create Artifact reminder directory"))?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(io_error("restrict Artifact reminder directory"))?;
        }
        Ok(Self { directory })
    }

    /// Records that this Session was reminded about one Artifact and reports
    /// whether the reminder is the first.
    ///
    /// This never waits: an unreadable or unwritable record degrades to
    /// [`ArtifactReminderMark::FirstReminder`] without a retry, so the model may
    /// see one duplicate reminder but the Hook never blocks the tool call.
    #[must_use]
    pub fn mark_reminded(
        &self,
        locator: &ExternalSessionLocator,
        artifact: &ArtifactReminderKey,
    ) -> ArtifactReminderMark {
        let path = self.record_path(locator);
        let mut record = read_record(&path).unwrap_or_default();
        if record
            .artifacts
            .iter()
            .any(|entry| entry == artifact.as_str())
        {
            return ArtifactReminderMark::AlreadyReminded;
        }
        record.version = RECORD_VERSION;
        record.artifacts.push(artifact.as_str().to_owned());
        while record.artifacts.len() > ARTIFACT_REMINDER_MAX_ENTRIES {
            record.artifacts.remove(0);
        }
        let _ignored = write_record(&path, &record);
        ArtifactReminderMark::FirstReminder
    }

    /// Removes the reminder record for one external Session, if any.
    pub fn forget(&self, locator: &ExternalSessionLocator) {
        let _ignored = fs::remove_file(self.record_path(locator));
    }

    fn record_path(&self, locator: &ExternalSessionLocator) -> PathBuf {
        self.directory
            .join(format!("reminders-{}.json", locator_digest(locator)))
    }
}

fn read_record(path: &Path) -> Option<ReminderRecord> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES
    {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let record: ReminderRecord = serde_json::from_slice(&bytes).ok()?;
    (record.version == RECORD_VERSION && record.artifacts.len() <= ARTIFACT_REMINDER_MAX_ENTRIES)
        .then_some(record)
}

fn write_record(path: &Path, record: &ReminderRecord) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("serialize Artifact reminder record: {error}"),
        )
    })?;
    let temporary = path.with_extension("json.tmp");
    let _ignored = fs::remove_file(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(io_error("create temporary Artifact reminder record"))?;
    let outcome = file
        .write_all(&bytes)
        .map_err(io_error("write Artifact reminder record"));
    drop(file);
    if outcome.is_err() {
        let _ignored = fs::remove_file(&temporary);
        return outcome;
    }
    fs::rename(&temporary, path).map_err(io_error("replace Artifact reminder record"))
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locator() -> ExternalSessionLocator {
        ExternalSessionLocator::new("codex", "session-artifact-reminder").unwrap()
    }

    #[test]
    fn first_reminder_is_recorded_and_the_second_is_suppressed() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ArtifactReminderStore::open(temporary.path()).unwrap();
        let key = ArtifactReminderKey::new("FE", "src/contract.rs").unwrap();

        assert_eq!(
            store.mark_reminded(&locator(), &key),
            ArtifactReminderMark::FirstReminder
        );
        assert_eq!(
            store.mark_reminded(&locator(), &key),
            ArtifactReminderMark::AlreadyReminded
        );

        let other = ArtifactReminderKey::new("FE", "src/other.rs").unwrap();
        assert_eq!(
            store.mark_reminded(&locator(), &other),
            ArtifactReminderMark::FirstReminder
        );
    }

    #[test]
    fn record_stays_bounded_and_evicts_the_oldest_artifact_first() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ArtifactReminderStore::open(temporary.path()).unwrap();
        let locator = locator();
        for index in 0..=ARTIFACT_REMINDER_MAX_ENTRIES {
            let key = ArtifactReminderKey::new("FE", &format!("src/file-{index}.rs")).unwrap();
            assert_eq!(
                store.mark_reminded(&locator, &key),
                ArtifactReminderMark::FirstReminder
            );
        }
        let record = read_record(&store.record_path(&locator)).unwrap();
        assert_eq!(record.artifacts.len(), ARTIFACT_REMINDER_MAX_ENTRIES);
        assert!(
            !record
                .artifacts
                .iter()
                .any(|entry| entry.ends_with("file-0.rs"))
        );

        let evicted = ArtifactReminderKey::new("FE", "src/file-0.rs").unwrap();
        assert_eq!(
            store.mark_reminded(&locator, &evicted),
            ArtifactReminderMark::FirstReminder
        );
    }

    #[test]
    fn corrupt_state_degrades_to_one_extra_reminder_without_failing() {
        let temporary = tempfile::tempdir().unwrap();
        let store = ArtifactReminderStore::open(temporary.path()).unwrap();
        let locator = locator();
        let key = ArtifactReminderKey::new("FE", "src/contract.rs").unwrap();
        assert_eq!(
            store.mark_reminded(&locator, &key),
            ArtifactReminderMark::FirstReminder
        );
        fs::write(store.record_path(&locator), b"{not json").unwrap();
        assert_eq!(
            store.mark_reminded(&locator, &key),
            ArtifactReminderMark::FirstReminder
        );
    }

    #[test]
    fn reminder_key_requires_repository_identity_and_relative_path() {
        assert!(ArtifactReminderKey::new("", "src/contract.rs").is_err());
        assert!(ArtifactReminderKey::new("FE", "  ").is_err());
    }
}
