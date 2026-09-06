use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::Serialize;
use uuid::Uuid;

use crate::{Error, ErrorCode, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub endpoint: PathBuf,
    pub collector_status: PathBuf,
    pub hook_diagnostics: PathBuf,
    pub spool: PathBuf,
    pub active: PathBuf,
    pub ready: PathBuf,
    pub repository: PathBuf,
}

#[must_use]
pub fn layout(root: &Path) -> Layout {
    let state = root.join("state");
    let spool = root.join("spool");
    Layout {
        root: root.to_path_buf(),
        config: root.join("config.toml"),
        endpoint: sctx_telemetry::default_endpoint(root),
        collector_status: state.join("collector-status.json"),
        hook_diagnostics: state.join("hook-diagnostics.json"),
        active: spool.join("active"),
        ready: spool.join("ready"),
        repository: root.join("repository"),
        state,
        spool,
    }
}

pub(crate) fn initialize_layout(root: &Path) -> Result<Layout> {
    if !root.is_absolute() {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            "logs root must be absolute",
        ));
    }
    reject_symlink(root)?;
    create_private_dir(root)?;
    let paths = layout(root);
    for directory in [&paths.state, &paths.spool, &paths.active, &paths.ready] {
        reject_symlink(directory)?;
        create_private_dir(directory)?;
    }
    Ok(paths)
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir(path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| Error::io(format!("create directory {}", path.display()), error))?;
    if !fs::metadata(path)
        .map_err(|error| Error::io(format!("inspect directory {}", path.display()), error))?
        .is_dir()
    {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            format!("managed directory is not a directory: {}", path.display()),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        Error::io(
            format!("set directory permissions {}", path.display()),
            error,
        )
    })
}

pub(crate) fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::new(
            ErrorCode::InvalidPath,
            format!("managed path must not be a symlink: {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(format!("inspect path {}", path.display()), error)),
    }
}

/// Durably replaces one JSON file using a sibling temporary file and rename.
///
/// # Errors
/// Returns an error for unsafe paths, serialization failures, or failed filesystem operations.
pub fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| Error::new(ErrorCode::InvalidInput, format!("serialize JSON: {error}")))?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

/// Reads one regular file without following symlinks or waiting on special files.
///
/// # Errors
/// Returns `InvalidInput` when the path is not a regular file or exceeds `limit`, and a typed
/// filesystem error when the file cannot be opened or read.
pub fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    read_bounded(path, limit, ErrorCode::InvalidInput)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new(ErrorCode::InvalidPath, "path has no parent"))?;
    reject_symlink(parent)?;
    reject_symlink(path)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| {
                Error::io(
                    format!("create temporary file {}", temporary.display()),
                    error,
                )
            })?;
        file.write_all(bytes)
            .map_err(|error| Error::io("write temporary file", error))?;
        file.sync_all()
            .map_err(|error| Error::io("sync temporary file", error))?;
        fs::rename(&temporary, path).map_err(|error| Error::io("publish atomic file", error))?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ignored = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| Error::io(format!("sync directory {}", path.display()), error))
}

pub(crate) fn read_bounded(path: &Path, limit: u64, code: ErrorCode) -> Result<Vec<u8>> {
    reject_symlink(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| Error::io(format!("read {}", path.display()), error))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::io(format!("inspect {}", path.display()), error))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error::new(
            code,
            format!(
                "file is not regular or exceeds bounded read limit: {}",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| Error::io(format!("read {}", path.display()), error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(Error::new(code, "file grew beyond bounded read limit"));
    }
    Ok(bytes)
}
