use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use sctx_telemetry::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    Error, ErrorCode, Result, StreamTarget,
    fs::{atomic_write_json, layout, read_bounded, reject_symlink, sync_dir},
};

const BATCH_SCHEMA_VERSION: u32 = 1;
const MAX_BATCH_READ_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamBinding {
    pub stream_id: String,
    pub installation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

impl From<&StreamTarget> for StreamBinding {
    fn from(target: &StreamTarget) -> Self {
        Self {
            stream_id: target.stream_id.clone(),
            installation_id: target.installation_id.clone(),
            email: target.email.clone(),
            remote: target.remote.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchManifest {
    pub schema_version: u32,
    pub batch_id: String,
    pub stream: StreamBinding,
    pub first_event_unix_ms: i64,
    pub last_event_unix_ms: i64,
    pub event_count: u64,
    pub content_bytes: u64,
    pub content_sha256: String,
    pub collector_version: String,
    pub sealed_at_unix_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadyBatch {
    pub directory: PathBuf,
    pub events_path: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: BatchManifest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredEvent {
    pub schema_version: u32,
    pub installation_id: String,
    pub stream_id: String,
    pub email: Option<String>,
    pub platform: String,
    pub collector_version: String,
    pub received_at_unix_ms: i64,
    pub event: Event,
}

pub(crate) struct ActiveBatch {
    pub id: String,
    pub directory: PathBuf,
    pub binding: StreamBinding,
    pub created_at_unix_ms: i64,
    file: File,
    pub bytes: u64,
    pub events: u64,
}

impl ActiveBatch {
    pub fn open_or_create(root: &Path, binding: StreamBinding) -> Result<Self> {
        let paths = layout(root);
        let mut candidates = fs::read_dir(&paths.active)
            .map_err(|error| Error::io("read active spool", error))?
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".building"))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        candidates.sort();
        if candidates.len() > 1 {
            return Err(Error::new(
                ErrorCode::CorruptBatch,
                "multiple active batches require repair",
            ));
        }
        if let Some(directory) = candidates.into_iter().next() {
            return Self::recover(directory);
        }
        Self::create(&paths.active, binding)
    }

    fn create(active_root: &Path, binding: StreamBinding) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let directory = active_root.join(format!("{id}.building"));
        fs::create_dir(&directory).map_err(|error| Error::io("create active batch", error))?;
        fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| Error::io("protect active batch", error))?;
        atomic_write_json(&directory.join("binding.json"), &binding)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("events.jsonl"))
            .map_err(|error| Error::io("create active events", error))?;
        sync_dir(&directory)?;
        Ok(Self {
            id,
            directory,
            binding,
            created_at_unix_ms: now_unix_ms(),
            file,
            bytes: 0,
            events: 0,
        })
    }

    fn recover(directory: PathBuf) -> Result<Self> {
        reject_symlink(&directory)?;
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".building"))
            .ok_or_else(|| Error::new(ErrorCode::CorruptBatch, "invalid active batch name"))?;
        validate_batch_id(name)?;
        let binding: StreamBinding =
            read_json(&directory.join("binding.json"), ErrorCode::CorruptBatch).or_else(|_| {
                read_json::<BatchManifest>(
                    &directory.join("manifest.json"),
                    ErrorCode::CorruptBatch,
                )
                .map(|manifest| manifest.stream)
            })?;
        let events_path = directory.join("events.jsonl");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&events_path)
            .map_err(|error| Error::io("open active events", error))?;
        let metadata_len = file
            .metadata()
            .map_err(|error| Error::io("inspect active events", error))?
            .len();
        if metadata_len > MAX_BATCH_READ_BYTES {
            return Err(Error::new(
                ErrorCode::CorruptBatch,
                "active batch exceeds recovery limit",
            ));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata_len).unwrap_or(0));
        Read::by_ref(&mut file)
            .take(MAX_BATCH_READ_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| Error::io("recover active events", error))?;
        let mut valid_end = 0usize;
        let mut events = 0u64;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            if serde_json::from_slice::<StoredEvent>(&line[..line.len() - 1]).is_err() {
                break;
            }
            valid_end += line.len();
            events += 1;
        }
        file.set_len(u64::try_from(valid_end).unwrap_or(u64::MAX))
            .map_err(|error| Error::io("truncate incomplete active tail", error))?;
        file.seek(SeekFrom::End(0))
            .map_err(|error| Error::io("seek active events", error))?;
        let created_at_unix_ms = directory
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or_else(now_unix_ms);
        Ok(Self {
            id: name.to_owned(),
            directory,
            binding,
            created_at_unix_ms,
            file,
            bytes: u64::try_from(valid_end).unwrap_or(u64::MAX),
            events,
        })
    }

    pub fn append(&mut self, event: &StoredEvent) -> Result<u64> {
        let mut bytes = serde_json::to_vec(event).map_err(|error| {
            Error::new(ErrorCode::InvalidInput, format!("serialize event: {error}"))
        })?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .map_err(|error| Error::io("append active event", error))?;
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.events = self.events.saturating_add(1);
        Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    pub fn seal(mut self, root: &Path) -> Result<Option<ReadyBatch>> {
        if self.events == 0 {
            drop(self.file);
            fs::remove_file(self.directory.join("events.jsonl"))
                .map_err(|error| Error::io("remove empty active events", error))?;
            fs::remove_file(self.directory.join("binding.json"))
                .map_err(|error| Error::io("remove empty active binding", error))?;
            fs::remove_dir(&self.directory)
                .map_err(|error| Error::io("remove empty active batch", error))?;
            sync_dir(&layout(root).active)?;
            return Ok(None);
        }
        self.file
            .flush()
            .map_err(|error| Error::io("flush active batch", error))?;
        self.file
            .sync_all()
            .map_err(|error| Error::io("sync active batch", error))?;
        drop(self.file);
        let events_path = self.directory.join("events.jsonl");
        let bytes = read_bounded(&events_path, MAX_BATCH_READ_BYTES, ErrorCode::CorruptBatch)?;
        let mut first = None;
        let mut last = None;
        let mut count = 0u64;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let stored: StoredEvent = serde_json::from_slice(line).map_err(|error| {
                Error::new(
                    ErrorCode::CorruptBatch,
                    format!("invalid active event: {error}"),
                )
            })?;
            verify_stored_binding(&stored, &self.binding)?;
            first.get_or_insert(stored.event.occurred_at_unix_ms);
            last = Some(stored.event.occurred_at_unix_ms);
            count = count.saturating_add(1);
        }
        let manifest = BatchManifest {
            schema_version: BATCH_SCHEMA_VERSION,
            batch_id: self.id.clone(),
            stream: self.binding,
            first_event_unix_ms: first.unwrap_or(0),
            last_event_unix_ms: last.unwrap_or(0),
            event_count: count,
            content_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            content_sha256: hex_sha256(&bytes),
            collector_version: env!("CARGO_PKG_VERSION").to_owned(),
            sealed_at_unix_ms: now_unix_ms(),
        };
        atomic_write_json(&self.directory.join("manifest.json"), &manifest)?;
        sync_dir(&self.directory)?;
        let ready_root = layout(root).ready;
        let destination = ready_root.join(&self.id);
        fs::rename(&self.directory, &destination)
            .map_err(|error| Error::io("publish ready batch", error))?;
        sync_dir(&layout(root).active)?;
        sync_dir(&ready_root)?;
        if fs::remove_file(destination.join("binding.json")).is_ok() {
            sync_dir(&destination)?;
        }
        load_ready_batch(&destination).map(Some)
    }
}

/// Lists every ready batch after fully validating its identity and contents.
///
/// # Errors
/// Returns an error when the spool cannot be read or any published batch is corrupt.
pub fn list_ready_batches(root: &Path) -> Result<Vec<ReadyBatch>> {
    let ready = layout(root).ready;
    let entries = match fs::read_dir(&ready) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io("read ready spool", error)),
    };
    let mut directories = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    directories.sort();
    directories
        .into_iter()
        .map(|directory| load_ready_batch(&directory))
        .collect()
}

/// Loads one batch and verifies its directory identity, size, count, and SHA-256.
///
/// # Errors
/// Returns an error for unsafe paths, malformed manifests, or content mismatch.
pub fn load_ready_batch(directory: &Path) -> Result<ReadyBatch> {
    reject_symlink(directory)?;
    let metadata =
        fs::metadata(directory).map_err(|error| Error::io("inspect ready batch", error))?;
    if !metadata.is_dir() {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "ready batch is not a directory",
        ));
    }
    let manifest_path = directory.join("manifest.json");
    let events_path = directory.join("events.jsonl");
    reject_symlink(&manifest_path)?;
    reject_symlink(&events_path)?;
    let manifest: BatchManifest = read_json(&manifest_path, ErrorCode::CorruptBatch)?;
    if manifest.schema_version != BATCH_SCHEMA_VERSION {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "unsupported batch schema",
        ));
    }
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name != manifest.batch_id {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "batch directory and manifest identity differ",
        ));
    }
    validate_batch_id(&manifest.batch_id)?;
    if manifest.stream.email.is_some() != manifest.stream.remote.is_some() {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "batch stream target is only partially assigned",
        ));
    }
    let bytes = read_bounded(&events_path, MAX_BATCH_READ_BYTES, ErrorCode::CorruptBatch)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "ready events has incomplete tail",
        ));
    }
    if manifest.content_bytes != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        || manifest.content_sha256 != hex_sha256(&bytes)
    {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "ready batch content hash or size differs",
        ));
    }
    let mut count = 0u64;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let stored: StoredEvent = serde_json::from_slice(line).map_err(|error| {
            Error::new(
                ErrorCode::CorruptBatch,
                format!("invalid ready event: {error}"),
            )
        })?;
        verify_stored_binding(&stored, &manifest.stream)?;
        count = count.saturating_add(1);
    }
    if count != manifest.event_count {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "ready event count differs",
        ));
    }
    Ok(ReadyBatch {
        directory: directory.to_path_buf(),
        events_path,
        manifest_path,
        manifest,
    })
}

pub(crate) fn bind_unassigned_batches(root: &Path, target: &StreamTarget) -> Result<u64> {
    let mut count = 0u64;
    for mut batch in list_ready_batches(root)? {
        if batch.manifest.stream.email.is_none() && batch.manifest.stream.remote.is_none() {
            batch.manifest.stream = StreamBinding::from(target);
            atomic_write_json(&batch.manifest_path, &batch.manifest)?;
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

pub(crate) fn spool_bytes(root: &Path) -> Result<u64> {
    fn visit(path: &Path, total: &mut u64, depth: usize) -> Result<()> {
        if depth > 4 {
            return Ok(());
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::io("measure spool", error)),
        };
        for entry in entries.filter_map(std::result::Result::ok).take(20_000) {
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| Error::io("inspect spool entry", error))?;
            if metadata.file_type().is_symlink() {
                return Err(Error::new(
                    ErrorCode::InvalidPath,
                    "spool contains a symlink",
                ));
            }
            if metadata.is_file() {
                *total = total.saturating_add(metadata.len());
            } else if metadata.is_dir() {
                visit(&entry.path(), total, depth + 1)?;
            }
        }
        Ok(())
    }
    let mut total = 0;
    visit(&layout(root).spool, &mut total, 0)?;
    Ok(total)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, code: ErrorCode) -> Result<T> {
    let bytes = read_bounded(path, 1024 * 1024, code)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| Error::new(code, format!("parse {}: {error}", path.display())))
}

fn hex_sha256(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn validate_batch_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "invalid batch identity",
        ));
    }
    Ok(())
}

fn verify_stored_binding(event: &StoredEvent, binding: &StreamBinding) -> Result<()> {
    let initially_unassigned = event.email.is_none();
    let binding_matches = event.installation_id == binding.installation_id
        && (initially_unassigned
            || (event.stream_id == binding.stream_id && event.email == binding.email));
    if !binding_matches {
        return Err(Error::new(
            ErrorCode::CorruptBatch,
            "stored event identity differs from batch stream binding",
        ));
    }
    Ok(())
}

pub(crate) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

use std::os::unix::fs::PermissionsExt;
