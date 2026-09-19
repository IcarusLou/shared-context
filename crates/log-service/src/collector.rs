use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use fs2::FileExt;
use sctx_telemetry::{Event, EventKind, FrameDecoder};

use crate::{
    BatchManifest, Config, Error, ErrorCode, ReadyBatch, Result, StreamBinding, atomic_write_json,
    diagnostics::HookDiagnosticAccumulator,
    load_config,
    spool::{ActiveBatch, StoredEvent, now_unix_ms, spool_bytes},
    status::{CollectorStatus, load_collector_status},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollectorOptions {
    pub idle_sleep: Duration,
    pub read_buffer_bytes: usize,
}

impl Default for CollectorOptions {
    fn default() -> Self {
        Self {
            idle_sleep: Duration::from_millis(25),
            read_buffer_bytes: 4096,
        }
    }
}

pub struct Collector {
    root: PathBuf,
    config: Config,
    reader: File,
    _dummy_writer: File,
    decoder: FrameDecoder,
    read_buffer: Vec<u8>,
    active: Option<ActiveBatch>,
    status: CollectorStatus,
    spool_usage: u64,
    options: CollectorOptions,
    diagnostics: HookDiagnosticAccumulator,
    last_housekeeping_unix_ms: i64,
    _collector_lock: File,
}

impl Collector {
    /// Opens the sole collector instance and its managed FIFO.
    ///
    /// # Errors
    /// Returns an error for invalid configuration, unsafe endpoints, or an existing collector.
    pub fn open(root: &Path, options: CollectorOptions) -> Result<Self> {
        let config = load_config(root)?;
        if !config.enabled {
            return Err(Error::new(ErrorCode::Endpoint, "logging is disabled"));
        }
        let endpoint = prepare_endpoint(root)?;
        let collector_lock = acquire_collector_lock(root)?;
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&endpoint)
            .map_err(|error| Error::io("open collector FIFO reader", error))?;
        verify_fifo(&reader, root)?;
        let dummy_writer = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&endpoint)
            .map_err(|error| Error::io("open collector FIFO dummy writer", error))?;
        verify_fifo(&dummy_writer, root)?;
        let binding = StreamBinding::from(&config.current_target());
        let active = ActiveBatch::open_or_create(root, binding)?;
        let spool_usage = spool_bytes(root)?;
        let read_buffer_bytes = options.read_buffer_bytes.clamp(512, 4096);
        Ok(Self {
            root: root.to_path_buf(),
            config,
            reader,
            _dummy_writer: dummy_writer,
            decoder: FrameDecoder::new(),
            read_buffer: vec![0; read_buffer_bytes],
            active: Some(active),
            status: load_collector_status(root),
            spool_usage,
            options,
            diagnostics: HookDiagnosticAccumulator::load(root),
            last_housekeeping_unix_ms: 0,
            _collector_lock: collector_lock,
        })
    }

    /// Drains one currently available FIFO chunk without waiting for producers.
    ///
    /// # Errors
    /// Returns an error only when the FIFO itself cannot be read; persistence failures are drops.
    pub fn collect_once(&mut self) -> Result<usize> {
        self.housekeeping_best_effort();
        if !self.config.enabled {
            return Ok(0);
        }
        let read = match self.reader.read(&mut self.read_buffer) {
            Ok(0) => {
                self.handle_seal_request();
                self.heartbeat_best_effort();
                return Ok(0);
            }
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                self.handle_seal_request();
                self.heartbeat_best_effort();
                return Ok(0);
            }
            Err(error) => return Err(Error::io("read collector FIFO", error)),
        };
        let decoded = self.decoder.push(&self.read_buffer[..read]);
        let mut accepted = 0usize;
        for item in decoded {
            match item {
                Ok(event) => {
                    accepted += 1;
                    self.status.accepted = self.status.accepted.saturating_add(1);
                    self.persist_best_effort(&event);
                }
                Err(_) => self.status.invalid_frames = self.status.invalid_frames.saturating_add(1),
            }
        }
        self.status.last_heartbeat_unix_ms = Some(now_unix_ms());
        self.handle_seal_request();
        self.write_status_best_effort();
        Ok(accepted)
    }

    /// Durably publishes the current active batch as one ready directory.
    ///
    /// # Errors
    /// Returns an error when active data cannot be validated, synced, or atomically renamed.
    pub fn seal(&mut self) -> Result<Option<ReadyBatch>> {
        let Some(active) = self.active.take() else {
            return Ok(None);
        };
        let sealed = active.seal(&self.root)?;
        self.active = Some(ActiveBatch::open_or_create(
            &self.root,
            StreamBinding::from(&self.config.current_target()),
        )?);
        self.diagnostics.checkpoint()?;
        Ok(sealed)
    }

    /// Runs collection until stopped or configuration is disabled.
    ///
    /// # Errors
    /// Returns an error when the FIFO receive path fails.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            let accepted = self.collect_once()?;
            if accepted == 0 {
                if self.should_seal_by_time() {
                    self.seal_best_effort();
                }
                if !self.config.enabled {
                    break;
                }
                thread::sleep(self.options.idle_sleep.min(Duration::from_secs(1)));
            }
        }
        self.seal().map(|_| ()).or_else(|error| {
            self.status.last_error = Some(error.code());
            self.write_status_best_effort();
            Ok(())
        })
    }

    fn persist_best_effort(&mut self, event: &Event) {
        if self.storage_pressure() {
            self.status.observable_dropped = self.status.observable_dropped.saturating_add(1);
            self.status.storage_pressure = true;
            return;
        }
        self.status.storage_pressure = false;
        let binding = self.active.as_ref().map_or_else(
            || StreamBinding::from(&self.config.current_target()),
            |active| active.binding.clone(),
        );
        let stored = stored_event(&binding, event);
        let result = self
            .active
            .as_mut()
            .ok_or_else(|| Error::new(ErrorCode::Io, "active batch unavailable"))
            .and_then(|active| active.append(&stored));
        match result {
            Ok(bytes) => {
                self.spool_usage = self.spool_usage.saturating_add(bytes);
                self.status.persisted = self.status.persisted.saturating_add(1);
                if event.kind == EventKind::HookDecision {
                    if let Err(error) = self.diagnostics.record(event) {
                        self.status.last_error = Some(error.code());
                    }
                }
                if self.should_seal_by_size() {
                    self.seal_best_effort();
                }
            }
            Err(error) => {
                self.status.observable_dropped = self.status.observable_dropped.saturating_add(1);
                self.status.last_error = Some(error.code());
            }
        }
    }

    fn storage_pressure(&self) -> bool {
        let mib = 1024_u64 * 1024;
        if self.spool_usage >= self.config.storage.spool_max_mib.saturating_mul(mib) {
            return true;
        }
        fs2::available_space(&self.root).map_or(true, |bytes| {
            bytes < self.config.storage.min_free_disk_mib.saturating_mul(mib)
        })
    }

    fn should_seal_by_size(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.bytes
                >= self
                    .config
                    .storage
                    .batch_max_mib
                    .saturating_mul(1024 * 1024)
        })
    }

    fn should_seal_by_time(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.events > 0
                && now_unix_ms().saturating_sub(active.created_at_unix_ms)
                    >= i64::try_from(
                        self.config
                            .storage
                            .seal_interval_seconds
                            .saturating_mul(1000),
                    )
                    .unwrap_or(i64::MAX)
        })
    }

    fn seal_best_effort(&mut self) {
        if let Err(error) = self.seal() {
            self.status.last_error = Some(error.code());
            self.active = ActiveBatch::open_or_create(
                &self.root,
                StreamBinding::from(&self.config.current_target()),
            )
            .ok();
        }
    }

    fn write_status_best_effort(&self) {
        let _ignored = atomic_write_json(&crate::layout(&self.root).collector_status, &self.status);
    }

    fn housekeeping_best_effort(&mut self) {
        let now = now_unix_ms();
        if now.saturating_sub(self.last_housekeeping_unix_ms) < 1_000 {
            return;
        }
        self.last_housekeeping_unix_ms = now;
        if let Ok(new_config) = load_config(&self.root) {
            let new_binding = StreamBinding::from(&new_config.current_target());
            let changed = self
                .active
                .as_ref()
                .is_none_or(|active| active.binding != new_binding);
            if changed {
                let old_was_unassigned = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.binding.email.is_none());
                let mut sealed_old = false;
                if let Some(active) = self.active.take() {
                    match active.seal(&self.root) {
                        Ok(_) => sealed_old = true,
                        Err(error) => self.status.last_error = Some(error.code()),
                    }
                }
                if sealed_old
                    && old_was_unassigned
                    && new_binding.email.is_some()
                    && let Ok(Some(_sync_lock)) = crate::config::try_acquire_sync_lock(&self.root)
                {
                    if let Err(error) = crate::spool::bind_unassigned_batches(
                        &self.root,
                        &new_config.current_target(),
                    ) {
                        self.status.last_error = Some(error.code());
                    }
                }
                self.active = ActiveBatch::open_or_create(&self.root, new_binding).ok();
            }
            self.config = new_config;
        }
        if let Ok(bytes) = spool_bytes(&self.root) {
            self.spool_usage = bytes;
        }
        if let Err(error) = self.diagnostics.checkpoint_if_due() {
            self.status.last_error = Some(error.code());
        }
        self.heartbeat_best_effort();
    }

    fn handle_seal_request(&mut self) {
        let Some(request) = crate::control::read_pending(&self.root) else {
            return;
        };
        let mut drained = false;
        for _ in 0..16 {
            match self.reader.read(&mut self.read_buffer) {
                Ok(0) => {
                    drained = true;
                    break;
                }
                Ok(read) => {
                    for item in self.decoder.push(&self.read_buffer[..read]) {
                        match item {
                            Ok(event) => {
                                self.status.accepted = self.status.accepted.saturating_add(1);
                                self.persist_best_effort(&event);
                            }
                            Err(_) => {
                                self.status.invalid_frames =
                                    self.status.invalid_frames.saturating_add(1);
                            }
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                Err(_) => break,
            }
        }
        let result = if drained {
            self.seal()
        } else {
            Err(Error::new(
                ErrorCode::Endpoint,
                "seal drain budget exhausted",
            ))
        };
        if let Err(error) = &result {
            self.status.last_error = Some(error.code());
        }
        if let Err(error) = crate::control::acknowledge(&self.root, &request, &result) {
            self.status.last_error = Some(error.code());
        }
    }

    fn heartbeat_best_effort(&mut self) {
        let now = now_unix_ms();
        if self
            .status
            .last_heartbeat_unix_ms
            .is_none_or(|last| now.saturating_sub(last) >= 5_000)
        {
            self.status.last_heartbeat_unix_ms = Some(now);
            self.write_status_best_effort();
        }
    }
}

fn stored_event(target: &StreamBinding, event: &Event) -> StoredEvent {
    StoredEvent {
        schema_version: 1,
        installation_id: target.installation_id.clone(),
        stream_id: target.stream_id.clone(),
        email: target.email.clone(),
        platform: std::env::consts::OS.to_owned(),
        collector_version: env!("CARGO_PKG_VERSION").to_owned(),
        received_at_unix_ms: now_unix_ms(),
        event: Event::normalized(event.clone()),
    }
}

fn acquire_collector_lock(root: &Path) -> Result<File> {
    let path = crate::layout(root).state.join("collector.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::io("open collector lock", error))?;
    file.try_lock_exclusive()
        .map_err(|_| Error::new(ErrorCode::Endpoint, "collector is already running"))?;
    Ok(file)
}

fn prepare_endpoint(root: &Path) -> Result<PathBuf> {
    let endpoint = sctx_telemetry::default_endpoint(root);
    let runtime = endpoint
        .parent()
        .ok_or_else(|| Error::new(ErrorCode::InvalidPath, "endpoint has no parent"))?;
    let runtime_base = runtime
        .parent()
        .ok_or_else(|| Error::new(ErrorCode::InvalidPath, "runtime has no parent"))?;
    create_private_runtime(runtime_base, root)?;
    create_private_runtime(runtime, root)?;
    match fs::symlink_metadata(&endpoint) {
        Ok(metadata) if metadata.file_type().is_fifo() => {}
        Ok(_) => {
            return Err(Error::new(
                ErrorCode::Endpoint,
                "collector endpoint exists but is not a FIFO",
            ));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let output = Command::new("mkfifo")
                .arg("-m")
                .arg("600")
                .arg(&endpoint)
                .output()
                .map_err(|source| Error::io("run mkfifo", source))?;
            if !output.status.success() {
                return Err(Error::new(ErrorCode::Endpoint, "mkfifo failed"));
            }
        }
        Err(error) => return Err(Error::io("inspect collector endpoint", error)),
    }
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))
        .map_err(|error| Error::io("protect collector endpoint", error))?;
    Ok(endpoint)
}

fn create_private_runtime(path: &Path, root: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(Error::io("create telemetry runtime directory", error)),
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::io("inspect runtime directory", error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            "telemetry runtime path is not a real directory",
        ));
    }
    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(Error::new(
            ErrorCode::Permission,
            "telemetry runtime owner differs from the effective user",
        ));
    }
    if let Ok(root_metadata) = fs::metadata(root) {
        if root_metadata.uid() != metadata.uid() {
            return Err(Error::new(
                ErrorCode::Permission,
                "telemetry runtime owner differs from logs root owner",
            ));
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| Error::io("protect telemetry runtime directory", error))
}

fn verify_fifo(file: &File, root: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| Error::io("inspect open FIFO", error))?;
    if !metadata.file_type().is_fifo() || metadata.mode().trailing_zeros() < 6 {
        return Err(Error::new(
            ErrorCode::Endpoint,
            "collector endpoint is not a private FIFO",
        ));
    }
    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(Error::new(
            ErrorCode::Permission,
            "collector endpoint owner differs from the effective user",
        ));
    }
    if let Ok(root_metadata) = fs::metadata(root) {
        if metadata.uid() != root_metadata.uid() {
            return Err(Error::new(
                ErrorCode::Permission,
                "collector endpoint owner differs from logs root owner",
            ));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _manifest_type_is_public(_: &BatchManifest) {}
