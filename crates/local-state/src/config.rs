use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
};

use fs2::FileExt;
use sctx_domain::{Error, ErrorKind, Result, SpaceId};
use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u32 = 1;

/// A local workspace-to-space mapping. It is never an event or reducer input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceBinding {
    workspace: PathBuf,
    space_id: SpaceId,
}

impl WorkspaceBinding {
    /// Canonical local workspace path used only to look up the mapping.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Default logical space selected for a query.
    #[must_use]
    pub const fn space_id(&self) -> SpaceId {
        self.space_id
    }

    /// Converts the binding into an explicitly non-authoritative query hint.
    #[must_use]
    pub fn query_hint(&self) -> WorkspaceQueryHint {
        WorkspaceQueryHint {
            space_id: self.space_id,
        }
    }
}

/// The only value a `WorkspaceBinding` can contribute to retrieval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceQueryHint {
    space_id: SpaceId,
}

impl WorkspaceQueryHint {
    /// Suggested `ContextSpace` filter. Query code may ignore this value.
    #[must_use]
    pub const fn space_id(self) -> SpaceId {
        self.space_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u32,
    store: String,
    #[serde(default)]
    workspace_bindings: Vec<BindingDocument>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingDocument {
    workspace: String,
    space_id: String,
}

/// Locked, atomic manager for `config.toml` under one installation root.
#[derive(Clone, Debug)]
pub struct UserConfigStore {
    root: PathBuf,
    config_path: PathBuf,
    repository: PathBuf,
    lock_path: PathBuf,
}

impl UserConfigStore {
    /// Creates or validates the one user configuration and fixed repository.
    ///
    /// # Errors
    ///
    /// Refuses symlinked/private-state paths, malformed configuration, or a
    /// configured Store different from `<root>/repository`.
    pub fn initialize(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        ensure_private_directory(&root)?;
        let state = root.join("state");
        ensure_private_directory(&state)?;
        let manager = Self {
            config_path: root.join("config.toml"),
            repository: root.join("repository"),
            lock_path: state.join("config.lock"),
            root,
        };
        let lock = manager.lock()?;
        if manager.config_path.exists() {
            manager.read_document()?;
            set_file_mode(&manager.config_path, 0o600)?;
        } else {
            manager.write_document(&ConfigDocument {
                version: CONFIG_VERSION,
                store: path_text(&manager.repository)?,
                workspace_bindings: Vec::new(),
            })?;
        }
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        Ok(manager)
    }

    /// Installation root containing the sole configured Store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The only configured Git Store path.
    #[must_use]
    pub fn repository(&self) -> &Path {
        &self.repository
    }

    /// Adds or replaces one local workspace query hint.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing workspace, malformed config, or failed
    /// atomic write.
    pub fn bind(&self, workspace: impl AsRef<Path>, space_id: SpaceId) -> Result<WorkspaceBinding> {
        let workspace = canonical_workspace(workspace.as_ref())?;
        let workspace_text = path_text(&workspace)?;
        let lock = self.lock()?;
        let mut document = self.read_document()?;
        document
            .workspace_bindings
            .retain(|binding| binding.workspace != workspace_text);
        document.workspace_bindings.push(BindingDocument {
            workspace: workspace_text,
            space_id: space_id.to_string(),
        });
        document
            .workspace_bindings
            .sort_by(|left, right| left.workspace.cmp(&right.workspace));
        self.write_document(&document)?;
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        Ok(WorkspaceBinding {
            workspace,
            space_id,
        })
    }

    /// Lists bindings in canonical workspace-path order.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration cannot be read or validated.
    pub fn list(&self) -> Result<Vec<WorkspaceBinding>> {
        let lock = self.lock()?;
        let document = self.read_document()?;
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        bindings(&document)
    }

    /// Removes one binding. Deleting a workspace path does not touch Git facts.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-canonicalizable path or failed atomic write.
    pub fn unbind(&self, workspace: impl AsRef<Path>) -> Result<Option<WorkspaceBinding>> {
        let workspace = canonical_workspace(workspace.as_ref())?;
        let workspace_text = path_text(&workspace)?;
        let lock = self.lock()?;
        let mut document = self.read_document()?;
        let position = document
            .workspace_bindings
            .iter()
            .position(|binding| binding.workspace == workspace_text);
        let removed = position.map(|position| document.workspace_bindings.remove(position));
        if removed.is_some() {
            self.write_document(&document)?;
        }
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        removed
            .map(|binding| binding_from_document(&binding))
            .transpose()
    }

    /// Resolves a binding strictly as a retrieval hint.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace/configuration cannot be validated.
    pub fn query_hint(&self, workspace: impl AsRef<Path>) -> Result<Option<WorkspaceQueryHint>> {
        let workspace = canonical_workspace(workspace.as_ref())?;
        Ok(self
            .list()?
            .into_iter()
            .find(|binding| binding.workspace == workspace)
            .map(|binding| binding.query_hint()))
    }

    fn lock(&self) -> Result<File> {
        let lock = open_private_file(&self.lock_path)?;
        lock.lock_exclusive()
            .map_err(io_error("lock config.lock"))?;
        Ok(lock)
    }

    fn read_document(&self) -> Result<ConfigDocument> {
        reject_symlink(&self.config_path)?;
        let text = fs::read_to_string(&self.config_path).map_err(io_error("read config.toml"))?;
        let mut document: ConfigDocument = toml::from_str(&text).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("parse {}: {error}", self.config_path.display()),
            )
        })?;
        if document.version != CONFIG_VERSION {
            return Err(invariant(format!(
                "unsupported config version {}",
                document.version
            )));
        }
        let expected = path_text(&self.repository)?;
        if document.store != expected {
            return Err(invariant(format!(
                "config must contain exactly the fixed Store {}; found {}",
                self.repository.display(),
                document.store
            )));
        }
        document
            .workspace_bindings
            .sort_by(|left, right| left.workspace.cmp(&right.workspace));
        let mut previous = None;
        for binding in &document.workspace_bindings {
            binding_from_document(binding)?;
            if previous == Some(binding.workspace.as_str()) {
                return Err(invariant(format!(
                    "duplicate workspace binding: {}",
                    binding.workspace
                )));
            }
            previous = Some(binding.workspace.as_str());
        }
        Ok(document)
    }

    fn write_document(&self, document: &ConfigDocument) -> Result<()> {
        let text = toml::to_string_pretty(document).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("serialize config.toml: {error}"),
            )
        })?;
        let temporary = self
            .root
            .join(format!(".config.{}.tmp", uuid::Uuid::new_v4().hyphenated()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(io_error("create temporary config"))?;
        file.write_all(text.as_bytes())
            .map_err(io_error("write temporary config"))?;
        file.sync_all().map_err(io_error("sync temporary config"))?;
        fs::rename(&temporary, &self.config_path).map_err(io_error("replace config.toml"))?;
        sync_directory(&self.root)?;
        set_file_mode(&self.config_path, 0o600)
    }
}

fn bindings(document: &ConfigDocument) -> Result<Vec<WorkspaceBinding>> {
    document
        .workspace_bindings
        .iter()
        .map(binding_from_document)
        .collect()
}

fn binding_from_document(document: &BindingDocument) -> Result<WorkspaceBinding> {
    let space_id = SpaceId::from_str(&document.space_id).map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("invalid WorkspaceBinding space_id: {error}"),
        )
    })?;
    let workspace = PathBuf::from(&document.workspace);
    if !workspace.is_absolute() {
        return Err(invariant(format!(
            "WorkspaceBinding path is not absolute: {}",
            workspace.display()
        )));
    }
    Ok(WorkspaceBinding {
        workspace,
        space_id,
    })
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("workspace {} cannot be resolved: {error}", path.display()),
        )
    })
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(io_error("make path absolute"))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {}", path.display()),
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

fn open_private_file(path: &Path) -> Result<File> {
    reject_symlink(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(io_error("open private file"))?;
    set_file_mode(path, 0o600)?;
    Ok(file)
}

fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(io_error("set private file permissions"))
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

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("sync directory"))
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{operation}: {error}"))
}
