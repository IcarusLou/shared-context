use std::{fs, fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Error, ErrorCode, Result,
    fs::{atomic_write, initialize_layout, layout, read_bounded},
};

const CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_RETAINED_STREAMS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub installation_id: String,
    pub stream_id: String,
    #[serde(default)]
    pub stream_created_at_unix_ms: i64,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_streams: Vec<StreamTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamTarget {
    pub stream_id: String,
    pub installation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub created_at_unix_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    pub on_maintain: bool,
    pub timeout_seconds: u64,
    pub max_new_payload_mib: u64,
    pub max_retry_count: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            on_maintain: true,
            timeout_seconds: 60,
            max_new_payload_mib: 20,
            max_retry_count: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub batch_max_mib: u64,
    pub seal_interval_seconds: u64,
    pub shard_max_mib: u64,
    pub shard_max_files: u64,
    pub spool_max_mib: u64,
    pub local_budget_mib: u64,
    pub min_free_disk_mib: u64,
    pub cache_max_age_days: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            batch_max_mib: 5,
            seal_interval_seconds: 60,
            shard_max_mib: 128,
            shard_max_files: 4096,
            spool_max_mib: 256,
            local_budget_mib: 1024,
            min_free_disk_mib: 1024,
            cache_max_age_days: 7,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitOptions {
    pub email: Option<String>,
    pub remote: Option<String>,
    pub installation_id: Option<String>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InitReport {
    pub config: Config,
    pub created: bool,
    pub rotated_stream: bool,
    pub bound_unassigned_batches: u64,
}

impl Config {
    #[must_use]
    pub fn current_target(&self) -> StreamTarget {
        StreamTarget {
            stream_id: self.stream_id.clone(),
            installation_id: self.installation_id.clone(),
            email: self.email.clone(),
            remote: self.remote.clone(),
            created_at_unix_ms: self.stream_created_at_unix_ms,
        }
    }

    #[must_use]
    pub fn target(&self, stream_id: &str) -> Option<StreamTarget> {
        if self.stream_id == stream_id {
            return Some(self.current_target());
        }
        self.retained_streams
            .iter()
            .find(|target| target.stream_id == stream_id)
            .cloned()
    }

    #[must_use]
    pub const fn is_assigned(&self) -> bool {
        self.email.is_some() && self.remote.is_some()
    }
}

/// Initializes or rotates the independent logging configuration atomically.
///
/// # Errors
/// Returns an error for invalid identity/remote values, corrupt prior config, or I/O failure.
pub fn init(root: &Path, mut options: InitOptions) -> Result<InitReport> {
    normalize_options(&mut options)?;
    let paths = initialize_layout(root)?;
    let _config_lock = acquire_config_lock(root)?;
    let _sync_lock = acquire_sync_lock(root)?;
    let existing = match load_config(root) {
        Ok(config) => Some(config),
        Err(error) if error.code() == ErrorCode::NotConfigured => None,
        Err(error) => return Err(error),
    };
    let created = existing.is_none();
    let now = now_unix_ms();
    let mut rotated_stream = false;
    let mut config = if let Some(mut config) = existing {
        if let Some(requested) = options.installation_id.as_deref() {
            if requested != config.installation_id {
                return Err(Error::new(
                    ErrorCode::InvalidConfig,
                    "installation_id is immutable",
                ));
            }
        }
        let target_changed = config.email != options.email || config.remote != options.remote;
        if target_changed {
            rotated_stream = true;
            let old = config.current_target();
            config
                .retained_streams
                .retain(|target| target.stream_id != old.stream_id);
            if stream_has_spool(root, &old.stream_id)? {
                if config.retained_streams.len() >= MAX_RETAINED_STREAMS {
                    return Err(Error::new(
                        ErrorCode::InvalidConfig,
                        "too many pending retained streams",
                    ));
                }
                config.retained_streams.push(old);
            }
            config.stream_id = Uuid::new_v4().to_string();
            config.stream_created_at_unix_ms = now;
            config.email = options.email;
            config.remote = options.remote;
        }
        config.enabled = options.enabled;
        config
    } else {
        Config {
            schema_version: CONFIG_SCHEMA_VERSION,
            enabled: options.enabled,
            email: options.email,
            remote: options.remote,
            installation_id: options
                .installation_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            stream_id: Uuid::new_v4().to_string(),
            stream_created_at_unix_ms: now,
            sync: SyncConfig::default(),
            storage: StorageConfig::default(),
            retained_streams: Vec::new(),
        }
    };
    config
        .retained_streams
        .retain(|target| stream_has_spool(root, &target.stream_id).unwrap_or(true));
    validate_config(&config)?;
    write_config(&paths.config, &config)?;
    // Binding unassigned sealed batches is intentionally a first-assignment-only operation.
    let bound_unassigned_batches = if !created && config.is_assigned() {
        crate::spool::bind_unassigned_batches(root, &config.current_target())?
    } else {
        0
    };
    if bound_unassigned_batches > 0 {
        config
            .retained_streams
            .retain(|target| stream_has_spool(root, &target.stream_id).unwrap_or(true));
        write_config(&paths.config, &config)?;
    }
    Ok(InitReport {
        config,
        created,
        rotated_stream,
        bound_unassigned_batches,
    })
}

/// Loads and strictly validates the logging configuration.
///
/// # Errors
/// Returns `NotConfigured`, `InvalidConfig`, or an I/O-related error.
pub fn load_config(root: &Path) -> Result<Config> {
    let path = layout(root).config;
    let bytes = match read_bounded(&path, 64 * 1024, ErrorCode::InvalidConfig) {
        Ok(bytes) => bytes,
        Err(_)
            if fs::symlink_metadata(&path)
                .is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Err(Error::new(
                ErrorCode::NotConfigured,
                "logging is not initialized",
            ));
        }
        Err(error) => return Err(error),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::new(ErrorCode::InvalidConfig, "log config is not UTF-8"))?;
    let config: Config = toml::from_str(text).map_err(|error| {
        Error::new(
            ErrorCode::InvalidConfig,
            format!("parse log config: {error}"),
        )
    })?;
    validate_config(&config)?;
    Ok(config)
}

/// Enables collection in the independent logging configuration.
///
/// # Errors
/// Returns an error when configuration cannot be loaded or durably updated.
pub fn enable(root: &Path) -> Result<Config> {
    set_enabled(root, true)
}
/// Disables collection while preserving all existing spool data.
///
/// # Errors
/// Returns an error when configuration cannot be loaded or durably updated.
pub fn disable(root: &Path) -> Result<Config> {
    set_enabled(root, false)
}

fn set_enabled(root: &Path, enabled: bool) -> Result<Config> {
    let _config_lock = acquire_config_lock(root)?;
    let mut config = load_config(root)?;
    config.enabled = enabled;
    write_config(&layout(root).config, &config)?;
    Ok(config)
}

fn write_config(path: &Path, config: &Config) -> Result<()> {
    let text = toml::to_string_pretty(config).map_err(|error| {
        Error::new(
            ErrorCode::InvalidConfig,
            format!("serialize log config: {error}"),
        )
    })?;
    atomic_write(path, text.as_bytes())
}

fn normalize_options(options: &mut InitOptions) -> Result<()> {
    match (&mut options.email, &options.remote) {
        (Some(email), Some(remote)) => {
            *email = normalize_email(email)?;
            validate_remote(remote)?;
        }
        (None, None) => {}
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "email and remote must be supplied together",
            ));
        }
    }
    if let Some(id) = options.installation_id.as_deref() {
        validate_id(id, "installation_id")?;
    }
    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    if config.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "unsupported log config schema_version",
        ));
    }
    validate_id(&config.installation_id, "installation_id")?;
    validate_id(&config.stream_id, "stream_id")?;
    match (&config.email, &config.remote) {
        (Some(email), Some(remote)) => {
            let _ = normalize_email(email)?;
            validate_remote(remote)?;
        }
        (None, None) => {}
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "email and remote must be configured together",
            ));
        }
    }
    if config.retained_streams.len() > MAX_RETAINED_STREAMS {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "too many retained streams",
        ));
    }
    let mut stream_ids = std::collections::BTreeSet::new();
    stream_ids.insert(config.stream_id.as_str());
    for target in &config.retained_streams {
        validate_id(&target.stream_id, "retained stream_id")?;
        validate_id(&target.installation_id, "retained installation_id")?;
        if target.email.is_some() != target.remote.is_some() {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "retained stream is partially assigned",
            ));
        }
        if let (Some(email), Some(remote)) = (&target.email, &target.remote) {
            let _ = normalize_email(email)?;
            validate_remote(remote)?;
        }
        if !stream_ids.insert(&target.stream_id) {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "duplicate stream identity",
            ));
        }
    }
    validate_sync(config.sync)?;
    validate_storage(config.storage)
}

fn validate_sync(sync: SyncConfig) -> Result<()> {
    if !(1..=300).contains(&sync.timeout_seconds)
        || !(1..=1024).contains(&sync.max_new_payload_mib)
        || sync.max_retry_count > 10
    {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "invalid sync timeout, payload budget, or retry limit",
        ));
    }
    Ok(())
}

fn validate_storage(storage: StorageConfig) -> Result<()> {
    let values = [
        storage.batch_max_mib,
        storage.seal_interval_seconds,
        storage.shard_max_mib,
        storage.shard_max_files,
        storage.spool_max_mib,
        storage.local_budget_mib,
        storage.min_free_disk_mib,
        storage.cache_max_age_days,
    ];
    if values.contains(&0)
        || storage.batch_max_mib > storage.spool_max_mib
        || storage.spool_max_mib > storage.local_budget_mib
        || storage.batch_max_mib > 128
        || storage.seal_interval_seconds > 3_600
        || storage.shard_max_files > 100_000
        || storage.local_budget_mib > 65_536
        || storage.cache_max_age_days > 365
    {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "invalid non-positive or inconsistent storage limits",
        ));
    }
    Ok(())
}

fn normalize_email(value: &str) -> Result<String> {
    if value.len() > 254 || value.chars().any(char::is_control) || value.contains(['/', '\\']) {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "invalid logging email",
        ));
    }
    let (local, domain) = value
        .rsplit_once('@')
        .filter(|(local, domain)| !local.is_empty() && !domain.is_empty() && !local.contains('@'))
        .ok_or_else(|| Error::new(ErrorCode::InvalidConfig, "invalid logging email"))?;
    if matches!(local, "." | "..") || domain.contains(['/', '\\', '@']) {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "invalid logging email",
        ));
    }
    Ok(format!("{local}@{}", domain.to_ascii_lowercase()))
}

fn validate_remote(value: &str) -> Result<()> {
    let lowered = value.to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 2048
        || value.chars().any(char::is_control)
        || lowered.starts_with("ext::")
        || value.contains(['?', '#'])
    {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            "invalid logging remote",
        ));
    }
    if let Some(rest) = value.strip_prefix("https://") {
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty() || authority.contains('@') {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "remote must not contain inline credentials",
            ));
        }
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("ssh://") {
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty()
            || authority
                .split_once('@')
                .is_some_and(|(userinfo, _)| userinfo.contains(':'))
        {
            return Err(Error::new(
                ErrorCode::InvalidConfig,
                "remote must not contain inline credentials",
            ));
        }
        return Ok(());
    }
    if let Some(path) = value.strip_prefix("file://") {
        if Path::new(path).is_absolute() && !path.contains("..") {
            return Ok(());
        }
    }
    if Path::new(value).is_absolute() && !value.split('/').any(|part| part == "..") {
        return Ok(());
    }
    if let Some((host, path)) = value.split_once(':') {
        if !host.is_empty()
            && !path.is_empty()
            && !host.chars().any(char::is_whitespace)
            && !path.chars().any(char::is_whitespace)
            && !value.contains("//")
            && !value.starts_with('-')
        {
            return Ok(());
        }
    }
    Err(Error::new(
        ErrorCode::InvalidConfig,
        "logging remote must be HTTPS, SSH, SCP-like, or an absolute local path",
    ))
}

fn validate_id(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::new(
            ErrorCode::InvalidConfig,
            format!("invalid {field}"),
        ));
    }
    Ok(())
}

fn stream_has_spool(root: &Path, stream_id: &str) -> Result<bool> {
    let paths = layout(root);
    for directory in [&paths.active, &paths.ready] {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::io("inspect spool streams", error)),
        };
        for entry in entries.flatten().take(10_000) {
            for name in ["binding.json", "manifest.json"] {
                let path = entry.path().join(name);
                if path
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() <= 1024 * 1024)
                    && let Ok(text) = fs::read_to_string(path)
                    && text.contains(stream_id)
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn acquire_config_lock(root: &Path) -> Result<std::fs::File> {
    let path = layout(root).state.join("config.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| Error::io("open log config lock", error))?;
    file.lock_exclusive()
        .map_err(|error| Error::io("lock log config", error))?;
    Ok(file)
}

fn acquire_sync_lock(root: &Path) -> Result<std::fs::File> {
    let path = layout(root).state.join("sync.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| Error::io("open log sync lock", error))?;
    file.lock_exclusive()
        .map_err(|error| Error::io("lock log sync", error))?;
    Ok(file)
}

pub(crate) fn try_acquire_sync_lock(root: &Path) -> Result<Option<std::fs::File>> {
    let path = layout(root).state.join("sync.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| Error::io("open log sync lock", error))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(Error::io("try log sync lock", error)),
    }
}

/// Produces a reversible, single safe path component for an email identity.
///
/// # Errors
/// Returns an error if `email` is not a valid configured identity.
pub fn encode_email_path(email: &str) -> Result<String> {
    let normalized = normalize_email(email)?;
    let mut encoded = String::with_capacity(normalized.len());
    for byte in normalized.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@') {
            encoded.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    if matches!(encoded.as_str(), "." | "..") {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            "email identity is not a safe path component",
        ));
    }
    Ok(encoded)
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
