use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use sctx_domain::{Error, ErrorKind, Result};

/// Installation-wide coordination lock acquired before every component lock.
#[derive(Clone, Debug)]
pub struct MaintenanceLock {
    path: PathBuf,
}

/// RAII ownership of one shared or exclusive installation maintenance lease.
#[derive(Debug)]
pub struct MaintenanceGuard {
    file: File,
}

impl MaintenanceLock {
    /// Creates private installation/state directories when absent, then opens the lock.
    ///
    /// # Errors
    ///
    /// Rejects symlinked or non-directory installation paths and permission failures.
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        ensure_private_directory(&root, "maintenance root")?;
        ensure_private_directory(&root.join("state"), "maintenance state")?;
        Self::open_or_create(root)
    }

    /// Opens or creates the private lock in an existing installation state directory.
    ///
    /// # Errors
    ///
    /// Rejects missing, non-directory, symlinked, or non-private state and unsafe
    /// lock files. This method never creates the installation or `state` directory.
    pub fn open_or_create(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        require_private_directory(&root, "maintenance root")?;
        let state = root.join("state");
        require_private_directory(&state, "maintenance state")?;
        let path = state.join("maintenance.lock");
        reject_symlink(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(io_error("open maintenance.lock"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(io_error("set maintenance.lock permissions"))?;
        validate_lock_file(&file)?;
        Ok(Self { path })
    }

    /// Acquires a non-blocking shared guard for one bounded business operation.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::MaintenanceBusy`] while exclusive maintenance owns
    /// the installation, or a typed storage error for an unsafe lock file.
    pub fn try_shared(&self) -> Result<MaintenanceGuard> {
        if self
            .path
            .parent()
            .is_some_and(|state| state.join("reset-journal.json").exists())
        {
            return Err(Error::new(
                ErrorKind::MaintenanceBusy,
                "Shared Context installation requires reset recovery",
            ));
        }
        self.try_lock(false)
    }

    /// Acquires a non-blocking exclusive guard for setup or destructive maintenance.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::MaintenanceBusy`] while any business operation or
    /// another maintenance operation owns the installation.
    pub fn try_exclusive(&self) -> Result<MaintenanceGuard> {
        self.try_lock(true)
    }

    /// Exact lock path, exposed for diagnosis and deterministic tests.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn try_lock(&self, exclusive: bool) -> Result<MaintenanceGuard> {
        reject_symlink(&self.path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(io_error("open existing maintenance.lock"))?;
        validate_lock_file(&file)?;
        let outcome = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        match outcome {
            Ok(()) => Ok(MaintenanceGuard { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(Error::new(
                ErrorKind::MaintenanceBusy,
                "Shared Context installation is busy with maintenance",
            )),
            Err(error) => Err(Error::new(
                ErrorKind::Io,
                format!("acquire maintenance.lock: {error}"),
            )),
        }
    }
}

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn require_private_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect maintenance directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invariant(format!(
            "{label} must be a non-symlink directory"
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(invariant(format!(
            "{label} must not be accessible by group or other"
        )));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(invariant(format!(
                "{label} must be a non-symlink directory"
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(io_error("create maintenance directory"))?;
        }
        Err(error) => return Err(io_error("inspect maintenance directory")(error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set maintenance directory permissions"))?;
    require_private_directory(path, label)
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(invariant("maintenance.lock must not be a symlink"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect maintenance.lock")(error)),
    }
}

fn validate_lock_file(file: &File) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(io_error("inspect maintenance.lock"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(invariant("maintenance.lock must be a private regular file"));
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(io_error("resolve maintenance root"))
    }
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;

    use super::*;

    fn fixture() -> (TempDir, MaintenanceLock) {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("installation");
        fs::create_dir_all(root.join("state")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("state"), fs::Permissions::from_mode(0o700)).unwrap();
        let lock = MaintenanceLock::open_or_create(root).unwrap();
        (temporary, lock)
    }

    #[test]
    fn shared_and_exclusive_guards_are_nonblocking_and_release_on_drop() {
        let (_temporary, lock) = fixture();
        let first = lock.try_shared().unwrap();
        let second = lock.try_shared().unwrap();
        assert_eq!(
            lock.try_exclusive().unwrap_err().kind(),
            ErrorKind::MaintenanceBusy
        );
        drop(first);
        drop(second);

        let exclusive = lock.try_exclusive().unwrap();
        assert_eq!(
            lock.try_shared().unwrap_err().kind(),
            ErrorKind::MaintenanceBusy
        );
        drop(exclusive);
        assert!(lock.try_shared().is_ok());
    }

    #[test]
    fn unwinding_releases_the_guard_for_immediate_recovery() {
        let (_temporary, lock) = fixture();
        let recovery_lock = lock.clone();
        let outcome = std::panic::catch_unwind(move || {
            let _exclusive = lock.try_exclusive().unwrap();
            panic!("simulated maintenance crash seam");
        });
        assert!(outcome.is_err());
        assert!(recovery_lock.try_exclusive().is_ok());
    }

    #[test]
    fn lock_is_private_and_symlink_replacement_is_rejected() {
        let (temporary, lock) = fixture();
        assert_eq!(
            fs::metadata(lock.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let outside = temporary.path().join("outside");
        fs::write(&outside, "preserve").unwrap();
        fs::remove_file(lock.path()).unwrap();
        std::os::unix::fs::symlink(&outside, lock.path()).unwrap();
        assert!(lock.try_shared().is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "preserve");
    }
}
