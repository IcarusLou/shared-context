use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::{Uuid, Variant, Version};

use sctx_domain::{Error, ErrorKind, Result};

/// Opaque operational identity of one append batch.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BatchId(String);

impl BatchId {
    pub(crate) fn new() -> Self {
        Self(format!("bat_{}", Uuid::new_v4().hyphenated()))
    }

    /// Returns the batch identifier as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BatchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for BatchId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let raw = value
            .strip_prefix("bat_")
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "batch ID must start with bat_"))?;
        let uuid = Uuid::parse_str(raw).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("invalid batch ID: {error}"),
            )
        })?;
        if uuid.get_version() != Some(Version::Random)
            || uuid.get_variant() != Variant::RFC4122
            || uuid.hyphenated().to_string() != raw
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "batch ID must contain a canonical lowercase UUIDv4",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

/// Kind of immutable file carried by a batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingFileKind {
    Event,
    Object,
}

/// A journaled immutable payload and its generated repository destination.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingFile {
    pub kind: PendingFileKind,
    pub target_path: String,
    pub payload_file: String,
    pub sha256: String,
    pub size: u64,
}

/// Informational progress marker. Recovery never trusts this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    Prepared,
    Created,
    Staged,
    Committed,
    Indexed,
}

/// Durable batch journal stored outside the Git repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub(crate) version: u32,
    pub(crate) batch_id: BatchId,
    pub(crate) event_id: String,
    #[serde(default)]
    pub(crate) base_head_oid: Option<String>,
    pub(crate) phase: JournalPhase,
    pub(crate) commit_oid: Option<String>,
    pub(crate) files: Vec<PendingFile>,
}

/// A batch visible through the explicit pending API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingBatch {
    pub batch_id: BatchId,
    pub event_id: String,
    pub phase: JournalPhase,
    pub commit_oid: Option<String>,
    pub files: Vec<PendingFile>,
}

impl From<&Journal> for PendingBatch {
    fn from(journal: &Journal) -> Self {
        Self {
            batch_id: journal.batch_id.clone(),
            event_id: journal.event_id.clone(),
            phase: journal.phase,
            commit_oid: journal.commit_oid.clone(),
            files: journal.files.clone(),
        }
    }
}
