use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use fs2::FileExt;
use sctx_domain::{
    ArtifactLocator, Error, ErrorKind, RepoRelativePath, RepositoryId, ResolvedFocus, Result,
};
use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u32 = 1;
const MAX_CATALOG_REPOSITORIES: usize = 256;
const MAX_CHECKOUTS_PER_REPOSITORY: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u32,
    store: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    repositories: Vec<RepositoryConfigDocument>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryConfigDocument {
    id: RepositoryId,
    paths: Vec<String>,
}

/// One explicitly configured local Repository identity and its checkout roots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogEntry {
    pub repository_id: RepositoryId,
    pub checkout_paths: Vec<PathBuf>,
}

/// Immutable in-memory view used for bounded Repository path mapping.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogSnapshot {
    pub repositories: Vec<RepositoryCatalogEntry>,
}

/// Result of an atomic Catalog add operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogAddOutcome {
    pub repository: RepositoryCatalogEntry,
    pub created_identity: bool,
    pub added_paths: usize,
}

/// Stable Catalog interpretation of one absolute file path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedRepositoryPath {
    pub repository_id: RepositoryId,
    pub checkout_path: PathBuf,
    pub relative_path: RepoRelativePath,
}

impl ResolvedRepositoryPath {
    /// Converts the resolved Repository identity and exact relative path into
    /// one lossless File Focus without exposing a public submission surface.
    #[must_use]
    pub fn into_resolved_file_focus(self) -> ResolvedFocus {
        ResolvedFocus {
            repository_id: self.repository_id,
            locator: ArtifactLocator::File {
                path: self.relative_path,
            },
        }
    }
}

/// Current validation state of one configured checkout path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCheckoutStatus {
    Available,
    Missing,
    Symlink,
    NotDirectory,
    NotGitRoot,
}

/// Bounded explicit Catalog diagnosis.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogDoctorReport {
    pub healthy: bool,
    pub repository_count: usize,
    pub checkout_count: usize,
    pub checkouts: Vec<RepositoryCatalogCheckoutCheck>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogCheckoutCheck {
    pub repository_id: RepositoryId,
    pub checkout_path: PathBuf,
    pub status: CatalogCheckoutStatus,
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
                repositories: Vec::new(),
            })?;
        }
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        Ok(manager)
    }

    /// Opens an existing configuration without creating paths or taking an
    /// exclusive lock. This is the bounded Hook hot-path entry.
    ///
    /// # Errors
    ///
    /// Rejects missing, non-directory, or symlinked installation state.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self> {
        let root = absolute(root.as_ref())?;
        let metadata = fs::symlink_metadata(&root).map_err(io_error("inspect config root"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invariant(
                "configuration root must be a non-symlink directory",
            ));
        }
        let state = root.join("state");
        let manager = Self {
            config_path: root.join("config.toml"),
            repository: root.join("repository"),
            lock_path: state.join("config.lock"),
            root,
        };
        reject_symlink(&manager.config_path)?;
        reject_symlink(&manager.lock_path)?;
        for path in [&manager.config_path, &manager.lock_path] {
            let metadata = fs::metadata(path).map_err(io_error("inspect private config file"))?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(invariant(
                    "existing configuration files must be private regular files",
                ));
            }
        }
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

    /// Reads the complete explicit Repository Catalog under a shared lock.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn repository_catalog(&self) -> Result<RepositoryCatalogSnapshot> {
        let lock = self.lock_shared()?;
        let document = self.read_document()?;
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        Ok(catalog_snapshot(&document))
    }

    /// Reads the complete Catalog while waiting for a concurrent explicit
    /// configuration writer. Request-serving code uses this after installation
    /// initialization; Hook hot paths keep using the non-blocking reader.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or filesystem errors.
    pub fn repository_catalog_wait(&self) -> Result<RepositoryCatalogSnapshot> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let document = self.read_document()?;
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        Ok(catalog_snapshot(&document))
    }

    /// Atomically creates a `RepositoryId` or attaches checkout paths to an existing one.
    ///
    /// `repository_id=None` is the only identity-creation path. A supplied ID must
    /// already exist in this Catalog and therefore cannot inject a new identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, unsafe, non-Git-root, cross-identity, or oversized input.
    #[allow(clippy::too_many_lines)]
    pub fn add_repository(
        &self,
        repository_id: Option<RepositoryId>,
        checkout_paths: &[PathBuf],
    ) -> Result<RepositoryCatalogAddOutcome> {
        if checkout_paths.is_empty() {
            return Err(invalid(
                "repository add requires at least one checkout path",
            ));
        }
        if checkout_paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
            return Err(invalid(format!(
                "repository add accepts at most {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths"
            )));
        }
        let paths = checkout_paths
            .iter()
            .map(|path| validate_git_checkout_root(path))
            .collect::<Result<BTreeSet<_>>>()?;
        let lock = self.lock()?;
        let mut document = self.read_document()?;
        let configured_owner = document
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .paths
                    .iter()
                    .map(move |path| (path.as_str(), repository.id))
            })
            .collect::<BTreeMap<_, _>>();
        for path in &paths {
            let text = path_text(path)?;
            if let Some(owner) = configured_owner.get(text.as_str())
                && repository_id != Some(*owner)
            {
                return Err(invalid(format!(
                    "checkout path is already configured for Repository {owner}"
                )));
            }
        }
        let (repository_id, created_identity) = if let Some(repository_id) = repository_id {
            if !document
                .repositories
                .iter()
                .any(|repository| repository.id == repository_id)
            {
                return Err(Error::new(
                    ErrorKind::RepositoryNotConfigured,
                    format!("Repository is not configured: {repository_id}"),
                ));
            }
            (repository_id, false)
        } else {
            if document.repositories.len() >= MAX_CATALOG_REPOSITORIES {
                return Err(invariant(format!(
                    "Repository Catalog exceeds {MAX_CATALOG_REPOSITORIES} identities"
                )));
            }
            (RepositoryId::new(), true)
        };
        let repository = if let Some(repository) = document
            .repositories
            .iter_mut()
            .find(|repository| repository.id == repository_id)
        {
            repository
        } else {
            document.repositories.push(RepositoryConfigDocument {
                id: repository_id,
                paths: Vec::new(),
            });
            let index = document.repositories.len().saturating_sub(1);
            document
                .repositories
                .get_mut(index)
                .ok_or_else(|| invariant("failed to insert Repository Catalog identity"))?
        };
        let before = repository.paths.len();
        repository.paths.extend(
            paths
                .iter()
                .map(|path| path_text(path))
                .collect::<Result<Vec<_>>>()?,
        );
        repository.paths.sort();
        repository.paths.dedup();
        if repository.paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
            return Err(invariant(format!(
                "Repository {repository_id} exceeds {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths"
            )));
        }
        let added_paths = repository.paths.len().saturating_sub(before);
        document
            .repositories
            .sort_by_key(|repository| repository.id.to_string());
        self.validate_document(&document)?;
        self.write_document(&document)?;
        let snapshot = catalog_snapshot(&document);
        let repository = snapshot
            .repositories
            .into_iter()
            .find(|repository| repository.repository_id == repository_id)
            .ok_or_else(|| invariant("configured Repository disappeared before commit"))?;
        FileExt::unlock(&lock).map_err(io_error("unlock config.lock"))?;
        Ok(RepositoryCatalogAddOutcome {
            repository,
            created_identity,
            added_paths,
        })
    }

    /// Validates every configured checkout without changing Catalog identity.
    ///
    /// # Errors
    ///
    /// Returns configuration or local process errors. Per-checkout drift is
    /// represented in the typed report.
    pub fn doctor_repository_catalog(&self) -> Result<RepositoryCatalogDoctorReport> {
        let catalog = self.repository_catalog()?;
        let mut checkouts = Vec::new();
        for repository in &catalog.repositories {
            for path in &repository.checkout_paths {
                checkouts.push(RepositoryCatalogCheckoutCheck {
                    repository_id: repository.repository_id,
                    checkout_path: path.clone(),
                    status: checkout_status(path)?,
                });
            }
        }
        let healthy = checkouts
            .iter()
            .all(|checkout| checkout.status == CatalogCheckoutStatus::Available);
        Ok(RepositoryCatalogDoctorReport {
            healthy,
            repository_count: catalog.repositories.len(),
            checkout_count: checkouts.len(),
            checkouts,
        })
    }

    fn lock(&self) -> Result<File> {
        let lock = open_private_file(&self.lock_path)?;
        lock.lock_exclusive()
            .map_err(io_error("lock config.lock"))?;
        Ok(lock)
    }

    fn lock_shared(&self) -> Result<File> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::try_lock_shared(&lock).map_err(io_error("try lock config.lock shared"))?;
        Ok(lock)
    }

    fn read_document(&self) -> Result<ConfigDocument> {
        reject_symlink(&self.config_path)?;
        let text = fs::read_to_string(&self.config_path).map_err(io_error("read config.toml"))?;
        let document: ConfigDocument = toml::from_str(&text).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("parse {}: {error}", self.config_path.display()),
            )
        })?;
        self.validate_document(&document)?;
        Ok(document)
    }

    fn validate_document(&self, document: &ConfigDocument) -> Result<()> {
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
        validate_repository_documents(&document.repositories)
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

impl RepositoryCatalogSnapshot {
    /// Canonicalizes and resolves one existing absolute file inside both an
    /// allowed Workspace and one configured checkout.
    ///
    /// # Errors
    ///
    /// Returns a typed not-configured error or rejects unsafe/outside paths.
    pub fn resolve_file_path(
        &self,
        file_path: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<ResolvedRepositoryPath> {
        validate_absolute_path(file_path, "file path")?;
        let metadata = fs::symlink_metadata(file_path).map_err(io_error("inspect file path"))?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("file path must not be a symlink"));
        }
        if !metadata.is_file() {
            return Err(invalid("file path must identify a regular file"));
        }
        let canonical_file = fs::canonicalize(file_path).map_err(io_error("canonicalize file"))?;
        let canonical_workspaces = canonical_workspace_roots(workspace_roots)?;
        let resolved = self.resolve_canonical_file(&canonical_file, &canonical_workspaces)?;
        if !file_path.starts_with(&resolved.checkout_path) {
            return Err(invalid(
                "file path must use the configured canonical checkout path",
            ));
        }
        reject_symlink_below_checkout(file_path, &resolved.checkout_path)?;
        Ok(resolved)
    }

    /// Resolves one Agent-declared absolute Artifact path through the configured Catalog without
    /// requiring the leaf (or a trailing path suffix) to exist.
    ///
    /// Existing components are checked for symlink escape and non-directory parents. No Git,
    /// scanner, Workspace, or source-existence claim participates in this mapping.
    ///
    /// # Errors
    ///
    /// Returns `RepositoryNotConfigured` when no configured checkout is a prefix, or rejects dot
    /// segments, symlink traversal, and an existing non-directory before the final component.
    pub fn resolve_declared_path(&self, declared_path: &Path) -> Result<ResolvedRepositoryPath> {
        validate_absolute_path(declared_path, "declared Artifact path")?;
        let mut matches = self
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .filter_map(move |checkout| {
                        declared_path
                            .starts_with(checkout)
                            .then_some((repository.repository_id, checkout))
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .1
                .components()
                .count()
                .cmp(&left.1.components().count())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some((repository_id, checkout_path)) = matches.first().copied() else {
            return Err(Error::new(
                ErrorKind::RepositoryNotConfigured,
                "declared Artifact path is not inside a configured Repository checkout",
            ));
        };
        validate_declared_components(declared_path, checkout_path)?;
        let relative = declared_path
            .strip_prefix(checkout_path)
            .map_err(|_| invariant("configured declared-path prefix disappeared"))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| invalid("Repository-relative Artifact path must be valid UTF-8"))?;
        Ok(ResolvedRepositoryPath {
            repository_id,
            checkout_path: checkout_path.clone(),
            relative_path: RepoRelativePath::new(relative)?,
        })
    }

    /// Pure longest-prefix mapping for already-canonical paths.
    ///
    /// # Errors
    ///
    /// Returns `RepositoryNotConfigured` when no configured checkout matches,
    /// or `InvalidInput` when the file is outside every allowed Workspace.
    pub fn resolve_canonical_file(
        &self,
        canonical_file: &Path,
        canonical_workspace_roots: &[PathBuf],
    ) -> Result<ResolvedRepositoryPath> {
        validate_absolute_path(canonical_file, "canonical file path")?;
        if canonical_workspace_roots.is_empty()
            || !canonical_workspace_roots
                .iter()
                .any(|workspace| canonical_file.starts_with(workspace))
        {
            return Err(invalid("file path is outside the allowed Workspace"));
        }
        let mut matches = self
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .filter_map(move |checkout| {
                        canonical_file
                            .starts_with(checkout)
                            .then_some((repository.repository_id, checkout))
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .1
                .components()
                .count()
                .cmp(&left.1.components().count())
                .then_with(|| left.0.cmp(&right.0))
        });
        let Some((repository_id, checkout_path)) = matches.first().copied() else {
            return Err(Error::new(
                ErrorKind::RepositoryNotConfigured,
                "file path is not inside a configured Repository checkout",
            ));
        };
        let relative = canonical_file
            .strip_prefix(checkout_path)
            .map_err(|_| invariant("longest-prefix Repository match disappeared"))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| invalid("Repository-relative file path must be valid UTF-8"))?;
        Ok(ResolvedRepositoryPath {
            repository_id,
            checkout_path: checkout_path.clone(),
            relative_path: RepoRelativePath::new(relative)?,
        })
    }
}

fn catalog_snapshot(document: &ConfigDocument) -> RepositoryCatalogSnapshot {
    RepositoryCatalogSnapshot {
        repositories: document
            .repositories
            .iter()
            .map(|repository| RepositoryCatalogEntry {
                repository_id: repository.id,
                checkout_paths: repository.paths.iter().map(PathBuf::from).collect(),
            })
            .collect(),
    }
}

fn validate_repository_documents(repositories: &[RepositoryConfigDocument]) -> Result<()> {
    if repositories.len() > MAX_CATALOG_REPOSITORIES {
        return Err(invariant(format!(
            "Repository Catalog exceeds {MAX_CATALOG_REPOSITORIES} identities"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeMap::<String, RepositoryId>::new();
    for repository in repositories {
        if !ids.insert(repository.id) {
            return Err(invalid(format!(
                "duplicate Repository Catalog identity: {}",
                repository.id
            )));
        }
        if repository.paths.len() > MAX_CHECKOUTS_PER_REPOSITORY {
            return Err(invariant(format!(
                "Repository {} exceeds {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths",
                repository.id
            )));
        }
        let mut own_paths = BTreeSet::new();
        for path in &repository.paths {
            let path_value = Path::new(path);
            validate_absolute_path(path_value, "Repository checkout path")?;
            if !own_paths.insert(path) {
                return Err(invalid(format!(
                    "duplicate checkout path for Repository {}: {path}",
                    repository.id
                )));
            }
            if let Some(owner) = paths.insert(path.clone(), repository.id) {
                return Err(invalid(format!(
                    "checkout path belongs to multiple Repository identities: {owner} and {}",
                    repository.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_git_checkout_root(path: &Path) -> Result<PathBuf> {
    validate_absolute_path(path, "Repository checkout path")?;
    match checkout_status(path)? {
        CatalogCheckoutStatus::Available => {
            fs::canonicalize(path).map_err(io_error("canonicalize Repository checkout"))
        }
        CatalogCheckoutStatus::Missing => Err(invalid("Repository checkout path does not exist")),
        CatalogCheckoutStatus::Symlink => {
            Err(invalid("Repository checkout path must not be a symlink"))
        }
        CatalogCheckoutStatus::NotDirectory => {
            Err(invalid("Repository checkout path must be a directory"))
        }
        CatalogCheckoutStatus::NotGitRoot => Err(invalid(
            "Repository checkout path must be a Git worktree root",
        )),
    }
}

fn checkout_status(path: &Path) -> Result<CatalogCheckoutStatus> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CatalogCheckoutStatus::Missing);
        }
        Err(error) => return Err(io_error("inspect Repository checkout")(error)),
    };
    if metadata.file_type().is_symlink() {
        return Ok(CatalogCheckoutStatus::Symlink);
    }
    if !metadata.is_dir() {
        return Ok(CatalogCheckoutStatus::NotDirectory);
    }
    let canonical = fs::canonicalize(path).map_err(io_error("canonicalize Repository checkout"))?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(io_error("inspect configured Git Repository"))?;
    if !output.status.success() {
        return Ok(CatalogCheckoutStatus::NotGitRoot);
    }
    let top_level = String::from_utf8(output.stdout)
        .map_err(|error| invalid(format!("Git top-level is not UTF-8: {error}")))?;
    let top_level = fs::canonicalize(top_level.trim())
        .map_err(io_error("canonicalize configured Git top-level"))?;
    Ok(if top_level == canonical {
        CatalogCheckoutStatus::Available
    } else {
        CatalogCheckoutStatus::NotGitRoot
    })
}

fn canonical_workspace_roots(workspace_roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    for root in workspace_roots {
        validate_absolute_path(root, "Workspace root")?;
        let metadata = fs::symlink_metadata(root).map_err(io_error("inspect Workspace root"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid("Workspace root must be a non-symlink directory"));
        }
        roots.insert(fs::canonicalize(root).map_err(io_error("canonicalize Workspace root"))?);
    }
    Ok(roots.into_iter().collect())
}

fn reject_symlink_below_checkout(path: &Path, checkout: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(checkout)
        .map_err(|_| invalid("file path is outside the configured checkout"))?;
    let mut current = checkout.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(io_error("inspect file component"))?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("file path must not traverse a symlink"));
        }
    }
    Ok(())
}

fn validate_declared_components(path: &Path, checkout: &Path) -> Result<()> {
    let checkout_metadata = match fs::symlink_metadata(checkout) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("inspect configured checkout")(error)),
    };
    if checkout_metadata.file_type().is_symlink() || !checkout_metadata.is_dir() {
        return Err(invalid(
            "configured checkout must remain a non-symlink directory",
        ));
    }
    if fs::canonicalize(checkout).map_err(io_error("canonicalize configured checkout"))? != checkout
    {
        return Err(invalid(
            "configured checkout canonical path changed or traverses a symlink",
        ));
    }
    let relative = path
        .strip_prefix(checkout)
        .map_err(|_| invalid("declared Artifact path is outside the configured checkout"))?;
    let components = relative.components().collect::<Vec<_>>();
    let mut current = checkout.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error("inspect declared Artifact component")(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(invalid(
                "declared Artifact path must not traverse a symlink",
            ));
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Err(invalid(
                "declared Artifact path traverses an existing non-directory",
            ));
        }
    }
    Ok(())
}

fn validate_absolute_path(path: &Path, field: &str) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid(format!(
            "{field} must be an absolute path without dot segments"
        )));
    }
    if path.to_str().is_none() {
        return Err(invalid(format!("{field} must be valid UTF-8")));
    }
    Ok(())
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

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{operation}: {error}"))
}
