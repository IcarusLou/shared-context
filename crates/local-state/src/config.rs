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
    ArtifactLocator, Error, ErrorKind, RepoRelativePath, RepositoryGroupId, RepositoryId,
    ResolvedFocus, Result,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CONFIG_VERSION: u32 = 1;
const MAX_CATALOG_REPOSITORIES: usize = 256;
const MAX_CHECKOUTS_PER_REPOSITORY: usize = 32;
const MAX_REPOSITORY_GROUPS: usize = 256;
const MAX_REPOSITORIES_PER_GROUP: usize = MAX_CATALOG_REPOSITORIES;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    version: u32,
    store: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    repositories: Vec<RepositoryConfigDocument>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    repository_groups: Vec<RepositoryGroupConfigDocument>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryConfigDocument {
    id: RepositoryId,
    paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryGroupConfigDocument {
    id: RepositoryGroupId,
    root: String,
    members: Vec<RepositoryId>,
}

/// One explicitly configured local Repository identity and its checkout roots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogEntry {
    pub repository_id: RepositoryId,
    pub checkout_paths: Vec<PathBuf>,
}

/// One explicitly configured local activation group and its closed Repository membership.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryGroupCatalogEntry {
    pub repository_group_id: RepositoryGroupId,
    pub root_path: PathBuf,
    pub member_repository_ids: Vec<RepositoryId>,
}

/// Immutable in-memory view used for bounded Repository path mapping.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogSnapshot {
    pub repositories: Vec<RepositoryCatalogEntry>,
    pub repository_groups: Vec<RepositoryGroupCatalogEntry>,
}

/// Deterministic local revision of the complete explicit `RepositoryCatalog`.
///
/// This is a SHA-256 digest of a canonically ordered serialization of configured
/// Repository IDs, checkout paths, Group IDs, roots, and members. Computing it is
/// pure: it performs no filesystem scan and never invokes Git.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryCatalogRevision(String);

impl RepositoryCatalogRevision {
    /// Stable textual representation used only by private local state.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let Some(digest) = self.0.strip_prefix("sha256:") else {
            return Err(invalid("Repository Catalog revision has an invalid prefix"));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid(
                "Repository Catalog revision is not canonical SHA-256",
            ));
        }
        Ok(())
    }
}

/// Result of an atomic Catalog add operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogAddOutcome {
    pub repository: RepositoryCatalogEntry,
    pub created_identity: bool,
    pub added_paths: usize,
}

/// Result of one atomic, semantically idempotent `RepositoryGroup` add operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryGroupCatalogAddOutcome {
    pub repository_group: RepositoryGroupCatalogEntry,
    pub created: bool,
}

/// Result of one atomic, semantically idempotent `RepositoryGroup` update.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryGroupCatalogUpdateOutcome {
    pub repository_group: RepositoryGroupCatalogEntry,
    pub changed: bool,
}

/// Result of one atomic, semantically idempotent `RepositoryGroup` removal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryGroupCatalogRemoveOutcome {
    pub repository_group_id: RepositoryGroupId,
    pub removed: bool,
}

/// Local `SessionStart` authorization decision for one canonical startup directory.
///
/// Absolute paths are backend-only event-attribution metadata. Callers must not
/// place them in model prompts, MCP responses, reports, or durable Context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivationScopeDecision {
    Direct {
        repository_id: RepositoryId,
        checkout_path: PathBuf,
    },
    Group {
        repository_group_id: RepositoryGroupId,
        root_path: PathBuf,
    },
    Disabled,
}

/// Bounded local authorization scope; it never represents durable knowledge identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActivationScope {
    pub decision: ActivationScopeDecision,
    pub allowed_repository_ids: Vec<RepositoryId>,
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

/// Current validation state of one explicitly configured `RepositoryGroup` root.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRepositoryGroupStatus {
    Available,
    Missing,
    Symlink,
    NotDirectory,
    NonCanonical,
}

/// CLI-only structural Catalog view. This is never an activation authorization input.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogInspection {
    pub catalog: RepositoryCatalogSnapshot,
    pub repository_groups: Vec<RepositoryCatalogGroupCheck>,
}

/// Bounded explicit Catalog diagnosis.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogDoctorReport {
    pub healthy: bool,
    pub repository_count: usize,
    pub checkout_count: usize,
    pub repository_group_count: usize,
    pub checkouts: Vec<RepositoryCatalogCheckoutCheck>,
    pub repository_groups: Vec<RepositoryCatalogGroupCheck>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogCheckoutCheck {
    pub repository_id: RepositoryId,
    pub checkout_path: PathBuf,
    pub status: CatalogCheckoutStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryCatalogGroupCheck {
    pub repository_group_id: RepositoryGroupId,
    pub root_path: PathBuf,
    pub member_repository_ids: Vec<RepositoryId>,
    pub status: CatalogRepositoryGroupStatus,
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
        let outcome = (|| {
            if manager.config_path.exists() {
                manager.read_document()?;
                set_file_mode(&manager.config_path, 0o600)?;
            } else {
                manager.write_document(&ConfigDocument {
                    version: CONFIG_VERSION,
                    store: path_text(&manager.repository)?,
                    repositories: Vec::new(),
                    repository_groups: Vec::new(),
                })?;
            }
            Ok(())
        })();
        finish_locked(&lock, outcome)?;
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
        let outcome = self
            .read_document()
            .map(|document| catalog_snapshot(&document));
        finish_locked(&lock, outcome)
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
        let outcome = self
            .read_document()
            .map(|document| catalog_snapshot(&document));
        finish_locked(&lock, outcome)
    }

    /// Reads a narrow structural view for local CLI list, doctor, and repair flows.
    ///
    /// Unlike [`Self::repository_catalog`], a missing or drifted Group root is
    /// represented as a typed status rather than failing the whole read. The
    /// returned Snapshot must never be used to authorize an Agent session.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or unexpected filesystem errors.
    pub fn inspect_repository_catalog(&self) -> Result<RepositoryCatalogInspection> {
        let lock = open_private_file(&self.lock_path)?;
        FileExt::lock_shared(&lock).map_err(io_error("lock config.lock shared"))?;
        let outcome = (|| {
            let document = self.read_document_for_repair()?;
            let catalog = catalog_snapshot(&document);
            let repository_groups = inspect_repository_groups(&catalog.repository_groups)?;
            Ok(RepositoryCatalogInspection {
                catalog,
                repository_groups,
            })
        })();
        finish_locked(&lock, outcome)
    }

    /// Resolves the local `SessionStart` `ActivationScope` under the Catalog read lock.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or path validation failures. A
    /// failure never becomes an inferred authorization decision.
    pub fn resolve_activation_scope(
        &self,
        canonical_startup_cwd: &Path,
    ) -> Result<ActivationScope> {
        self.repository_catalog()?
            .resolve_activation_scope(canonical_startup_cwd)
    }

    /// Atomically creates an explicitly named `RepositoryId` or attaches checkout
    /// paths to the existing exact identity.
    ///
    /// # Errors
    ///
    /// Rejects empty, unsafe, non-Git-root, cross-identity, or oversized input.
    #[allow(clippy::too_many_lines)]
    pub fn add_repository(
        &self,
        repository_id: RepositoryId,
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
        let outcome = (|| {
            let mut document = self.read_document()?;
            let configured_owner = document
                .repositories
                .iter()
                .flat_map(|repository| {
                    let repository_id = repository.id.clone();
                    repository
                        .paths
                        .iter()
                        .map(move |path| (path.as_str(), repository_id.clone()))
                })
                .collect::<BTreeMap<_, _>>();
            for path in &paths {
                let text = path_text(path)?;
                if let Some(owner) = configured_owner.get(text.as_str())
                    && &repository_id != owner
                {
                    return Err(invalid(format!(
                        "checkout path is already configured for Repository {owner}"
                    )));
                }
            }
            let created_identity = !document
                .repositories
                .iter()
                .any(|repository| repository.id == repository_id);
            if created_identity {
                if document.repositories.len() >= MAX_CATALOG_REPOSITORIES {
                    return Err(invariant(format!(
                        "Repository Catalog exceeds {MAX_CATALOG_REPOSITORIES} identities"
                    )));
                }
                if let Some(conflict) = document.repositories.iter().find(|repository| {
                    repository
                        .id
                        .as_str()
                        .eq_ignore_ascii_case(repository_id.as_str())
                }) {
                    return Err(invalid(format!(
                        "Repository ID differs only by ASCII case from configured identity {}",
                        conflict.id
                    )));
                }
            }
            let lookup_id = repository_id.clone();
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
                    "Repository {} exceeds {MAX_CHECKOUTS_PER_REPOSITORY} checkout paths",
                    repository.id
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
                .find(|repository| repository.repository_id == lookup_id)
                .ok_or_else(|| invariant("configured Repository disappeared before commit"))?;
            Ok(RepositoryCatalogAddOutcome {
                repository,
                created_identity,
                added_paths,
            })
        })();
        finish_locked(&lock, outcome)
    }

    /// Atomically creates one explicit local `RepositoryGroup`.
    ///
    /// The Group ID is always generated by this store. Repeating an add with
    /// the same canonical root and exact membership returns the existing Group;
    /// reusing a root with different membership is rejected.
    ///
    /// # Errors
    ///
    /// Rejects empty or duplicate membership, unknown Repository identities,
    /// unsafe roots, members without a checkout strictly below the root, and
    /// conflicting or oversized Groups.
    pub fn add_repository_group(
        &self,
        group_root: &Path,
        member_repository_ids: &[RepositoryId],
    ) -> Result<RepositoryGroupCatalogAddOutcome> {
        if member_repository_ids.is_empty() {
            return Err(invalid(
                "RepositoryGroup requires at least one member Repository",
            ));
        }
        if member_repository_ids.len() > MAX_REPOSITORIES_PER_GROUP {
            return Err(invalid(format!(
                "RepositoryGroup accepts at most {MAX_REPOSITORIES_PER_GROUP} members"
            )));
        }
        let members = member_repository_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if members.len() != member_repository_ids.len() {
            return Err(invalid(
                "RepositoryGroup member Repository identities must be unique",
            ));
        }
        let root_path = validate_repository_group_root(group_root)?;
        let root = path_text(&root_path)?;
        let members = members.into_iter().collect::<Vec<_>>();

        let lock = self.lock()?;
        let outcome = (|| {
            let mut document = self.read_document()?;
            validate_repository_group_members(&document.repositories, &root_path, &members)?;
            validate_current_repository_group_members(
                &document.repositories,
                &root_path,
                &members,
            )?;

            if let Some(existing) = document
                .repository_groups
                .iter()
                .find(|group| group.root == root)
            {
                if existing.members != members {
                    return Err(invalid(format!(
                        "RepositoryGroup root already has different membership: {}",
                        root_path.display()
                    )));
                }
                let snapshot = catalog_snapshot(&document);
                let repository_group = snapshot
                    .repository_groups
                    .into_iter()
                    .find(|group| group.repository_group_id == existing.id)
                    .ok_or_else(|| {
                        invariant("configured RepositoryGroup disappeared before return")
                    })?;
                return Ok(RepositoryGroupCatalogAddOutcome {
                    repository_group,
                    created: false,
                });
            }
            if document.repository_groups.len() >= MAX_REPOSITORY_GROUPS {
                return Err(invariant(format!(
                    "Repository Catalog exceeds {MAX_REPOSITORY_GROUPS} RepositoryGroups"
                )));
            }

            let repository_group_id = RepositoryGroupId::new();
            document
                .repository_groups
                .push(RepositoryGroupConfigDocument {
                    id: repository_group_id,
                    root,
                    members,
                });
            document
                .repository_groups
                .sort_by_key(|group| group.id.to_string());
            self.validate_document(&document)?;
            self.write_document(&document)?;
            let snapshot = catalog_snapshot(&document);
            let repository_group = snapshot
                .repository_groups
                .into_iter()
                .find(|group| group.repository_group_id == repository_group_id)
                .ok_or_else(|| invariant("configured RepositoryGroup disappeared before commit"))?;
            Ok(RepositoryGroupCatalogAddOutcome {
                repository_group,
                created: true,
            })
        })();
        finish_locked(&lock, outcome)
    }

    /// Atomically replaces the root and/or exact membership of one configured Group.
    ///
    /// This is an explicit repair path: it can structurally read a Catalog whose
    /// current Group root has drifted, while fully validating the resulting Group
    /// before writing it back. No Repository identity is created or removed.
    ///
    /// # Errors
    ///
    /// Rejects an unknown Group, an empty update, unsafe replacement values,
    /// unknown members, conflicting roots, or members without an available
    /// checkout strictly below the resulting root.
    pub fn update_repository_group(
        &self,
        repository_group_id: RepositoryGroupId,
        replacement_root: Option<&Path>,
        replacement_member_repository_ids: Option<&[RepositoryId]>,
    ) -> Result<RepositoryGroupCatalogUpdateOutcome> {
        if replacement_root.is_none() && replacement_member_repository_ids.is_none() {
            return Err(invalid(
                "RepositoryGroup update requires --root and/or member Repository identities",
            ));
        }
        let replacement_root = replacement_root
            .map(validate_repository_group_root)
            .transpose()?;
        let replacement_members = replacement_member_repository_ids
            .map(validate_repository_group_member_input)
            .transpose()?;

        let lock = self.lock()?;
        let outcome = (|| {
            let mut document = self.read_document_for_repair()?;
            let index = document
                .repository_groups
                .iter()
                .position(|group| group.id == repository_group_id)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::RepositoryNotConfigured,
                        format!("RepositoryGroup is not configured: {repository_group_id}"),
                    )
                })?;
            let existing = document.repository_groups[index].clone();
            let root_path = replacement_root.unwrap_or_else(|| PathBuf::from(&existing.root));
            validate_repository_group_root(&root_path)?;
            let members = replacement_members.unwrap_or(existing.members);
            validate_repository_group_members(&document.repositories, &root_path, &members)?;
            validate_current_repository_group_members(
                &document.repositories,
                &root_path,
                &members,
            )?;
            let root = path_text(&root_path)?;
            let changed = document.repository_groups[index].root != root
                || document.repository_groups[index].members != members;
            document.repository_groups[index].root = root;
            document.repository_groups[index].members = members;
            validate_document_structure(&document, &self.repository)?;
            if changed {
                self.write_document(&document)?;
            }
            let repository_group = catalog_snapshot(&document)
                .repository_groups
                .into_iter()
                .find(|group| group.repository_group_id == repository_group_id)
                .ok_or_else(|| invariant("updated RepositoryGroup disappeared before return"))?;
            Ok(RepositoryGroupCatalogUpdateOutcome {
                repository_group,
                changed,
            })
        })();
        finish_locked(&lock, outcome)
    }

    /// Atomically removes only one local `RepositoryGroup` activation entry.
    ///
    /// Repeating removal of the same ID succeeds with `removed=false`. This
    /// operation never deletes Repository identities, checkouts, Runtime,
    /// Capture, durable Context, or files below the configured root.
    ///
    /// # Errors
    ///
    /// Returns typed configuration, locking, or atomic-write failures.
    pub fn remove_repository_group(
        &self,
        repository_group_id: RepositoryGroupId,
    ) -> Result<RepositoryGroupCatalogRemoveOutcome> {
        let lock = self.lock()?;
        let outcome = (|| {
            let mut document = self.read_document_for_repair()?;
            let before = document.repository_groups.len();
            document
                .repository_groups
                .retain(|group| group.id != repository_group_id);
            let removed = document.repository_groups.len() != before;
            validate_document_structure(&document, &self.repository)?;
            if removed {
                self.write_document(&document)?;
            }
            Ok(RepositoryGroupCatalogRemoveOutcome {
                repository_group_id,
                removed,
            })
        })();
        finish_locked(&lock, outcome)
    }

    /// Validates every configured checkout without changing Catalog identity.
    ///
    /// # Errors
    ///
    /// Returns configuration or local process errors. Per-checkout drift is
    /// represented in the typed report.
    pub fn doctor_repository_catalog(&self) -> Result<RepositoryCatalogDoctorReport> {
        let inspection = self.inspect_repository_catalog()?;
        let catalog = inspection.catalog;
        let mut checkouts = Vec::new();
        for repository in &catalog.repositories {
            for path in &repository.checkout_paths {
                checkouts.push(RepositoryCatalogCheckoutCheck {
                    repository_id: repository.repository_id.clone(),
                    checkout_path: path.clone(),
                    status: checkout_status(path)?,
                });
            }
        }
        let healthy = checkouts
            .iter()
            .all(|checkout| checkout.status == CatalogCheckoutStatus::Available)
            && inspection
                .repository_groups
                .iter()
                .all(|group| group.status == CatalogRepositoryGroupStatus::Available);
        Ok(RepositoryCatalogDoctorReport {
            healthy,
            repository_count: catalog.repositories.len(),
            checkout_count: checkouts.len(),
            repository_group_count: inspection.repository_groups.len(),
            checkouts,
            repository_groups: inspection.repository_groups,
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
        let document = self.read_document_for_repair()?;
        validate_repository_group_roots(&document.repository_groups)?;
        Ok(document)
    }

    fn read_document_for_repair(&self) -> Result<ConfigDocument> {
        reject_symlink(&self.config_path)?;
        let text = fs::read_to_string(&self.config_path).map_err(io_error("read config.toml"))?;
        let document: ConfigDocument = toml::from_str(&text).map_err(|error| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("parse {}: {error}", self.config_path.display()),
            )
        })?;
        validate_document_structure(&document, &self.repository)?;
        Ok(document)
    }

    fn validate_document(&self, document: &ConfigDocument) -> Result<()> {
        validate_document_structure(document, &self.repository)?;
        validate_repository_group_roots(&document.repository_groups)
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
    /// Computes the semantic revision used to invalidate local Session leases.
    ///
    /// Ordering differences in caller-built Snapshots do not change the result.
    /// The revision changes when any configured Repository identity, checkout,
    /// Group root, or Group membership changes. No path is inspected and Git is
    /// never executed.
    ///
    /// # Errors
    ///
    /// Returns an input error when a configured path cannot be serialized.
    pub fn revision(&self) -> Result<RepositoryCatalogRevision> {
        let mut canonical = self.clone();
        canonical
            .repositories
            .sort_by_key(|repository| repository.repository_id.clone());
        for repository in &mut canonical.repositories {
            repository.checkout_paths.sort();
        }
        canonical
            .repository_groups
            .sort_by_key(|group| group.repository_group_id);
        for group in &mut canonical.repository_groups {
            group.member_repository_ids.sort();
        }
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|_| invalid("serialize Repository Catalog revision source"))?;
        let digest = Sha256::digest(bytes);
        Ok(RepositoryCatalogRevision(format!("sha256:{digest:x}")))
    }

    /// Resolves one canonical Agent startup directory into local authorization.
    ///
    /// Direct checkout ownership takes precedence and uses longest-prefix
    /// matching. Group authorization requires exact equality with an explicit
    /// `RepositoryGroup` root. Every other location is Disabled.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical startup directory or ambiguous Catalog ownership.
    /// Repository and Group health belongs to Catalog configuration and doctor
    /// operations; this `SessionStart` resolver does not invoke Git.
    pub fn resolve_activation_scope(
        &self,
        canonical_startup_cwd: &Path,
    ) -> Result<ActivationScope> {
        validate_canonical_directory(canonical_startup_cwd, "Agent startup directory")?;

        let mut direct_matches = self
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .checkout_paths
                    .iter()
                    .filter_map(move |checkout| {
                        canonical_startup_cwd
                            .starts_with(checkout)
                            .then_some((repository.repository_id.clone(), checkout))
                    })
            })
            .collect::<Vec<_>>();
        direct_matches.sort_by(|left, right| {
            right
                .1
                .components()
                .count()
                .cmp(&left.1.components().count())
                .then_with(|| left.0.cmp(&right.0))
        });
        if let Some((repository_id, checkout_path)) = direct_matches.first().cloned() {
            if direct_matches.iter().skip(1).any(|(other_id, other_path)| {
                *other_path == checkout_path && other_id != &repository_id
            }) {
                return Err(invariant(
                    "ActivationScope has ambiguous checkout ownership",
                ));
            }
            return Ok(ActivationScope {
                decision: ActivationScopeDecision::Direct {
                    repository_id: repository_id.clone(),
                    checkout_path: checkout_path.clone(),
                },
                allowed_repository_ids: vec![repository_id],
            });
        }

        let matching_groups = self
            .repository_groups
            .iter()
            .filter(|group| group.root_path == canonical_startup_cwd)
            .collect::<Vec<_>>();
        if matching_groups.len() > 1 {
            return Err(invariant(
                "ActivationScope has duplicate RepositoryGroup roots",
            ));
        }
        if let Some(group) = matching_groups.first().copied() {
            return Ok(ActivationScope {
                decision: ActivationScopeDecision::Group {
                    repository_group_id: group.repository_group_id,
                    root_path: group.root_path.clone(),
                },
                allowed_repository_ids: group.member_repository_ids.clone(),
            });
        }

        Ok(ActivationScope {
            decision: ActivationScopeDecision::Disabled,
            allowed_repository_ids: Vec::new(),
        })
    }

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
                            .then_some((repository.repository_id.clone(), checkout))
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
        let Some((repository_id, checkout_path)) = matches.first().cloned() else {
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
                            .then_some((repository.repository_id.clone(), checkout))
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
        let Some((repository_id, checkout_path)) = matches.first().cloned() else {
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
                repository_id: repository.id.clone(),
                checkout_paths: repository.paths.iter().map(PathBuf::from).collect(),
            })
            .collect(),
        repository_groups: document
            .repository_groups
            .iter()
            .map(|group| RepositoryGroupCatalogEntry {
                repository_group_id: group.id,
                root_path: PathBuf::from(&group.root),
                member_repository_ids: group.members.clone(),
            })
            .collect(),
    }
}

fn validate_document_structure(document: &ConfigDocument, repository: &Path) -> Result<()> {
    if document.version != CONFIG_VERSION {
        return Err(invariant(format!(
            "unsupported config version {}",
            document.version
        )));
    }
    let expected = path_text(repository)?;
    if document.store != expected {
        return Err(invariant(format!(
            "config must contain exactly the fixed Store {}; found {}",
            repository.display(),
            document.store
        )));
    }
    validate_repository_documents(&document.repositories)?;
    validate_repository_group_documents_structure(
        &document.repository_groups,
        &document.repositories,
    )
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
        if !ids.insert(repository.id.clone()) {
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
            if let Some(owner) = paths.insert(path.clone(), repository.id.clone()) {
                return Err(invalid(format!(
                    "checkout path belongs to multiple Repository identities: {owner} and {}",
                    repository.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_repository_group_documents_structure(
    groups: &[RepositoryGroupConfigDocument],
    repositories: &[RepositoryConfigDocument],
) -> Result<()> {
    if groups.len() > MAX_REPOSITORY_GROUPS {
        return Err(invariant(format!(
            "Repository Catalog exceeds {MAX_REPOSITORY_GROUPS} RepositoryGroups"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for group in groups {
        if !ids.insert(group.id) {
            return Err(invalid(format!(
                "duplicate RepositoryGroup identity: {}",
                group.id
            )));
        }
        if !roots.insert(&group.root) {
            return Err(invalid(format!(
                "duplicate RepositoryGroup root: {}",
                group.root
            )));
        }
        let root = Path::new(&group.root);
        validate_absolute_path(root, "RepositoryGroup configured root")?;
        if group.members.is_empty() {
            return Err(invalid(format!(
                "RepositoryGroup {} requires at least one member Repository",
                group.id
            )));
        }
        if group.members.len() > MAX_REPOSITORIES_PER_GROUP {
            return Err(invariant(format!(
                "RepositoryGroup {} exceeds {MAX_REPOSITORIES_PER_GROUP} members",
                group.id
            )));
        }
        let members = group.members.iter().cloned().collect::<BTreeSet<_>>();
        if members.len() != group.members.len() {
            return Err(invalid(format!(
                "RepositoryGroup {} contains duplicate member Repository identities",
                group.id
            )));
        }
        if !group.members.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(invalid(format!(
                "RepositoryGroup {} members must use canonical sorted order",
                group.id
            )));
        }
        validate_repository_group_members(repositories, root, &group.members)?;
    }
    Ok(())
}

fn validate_repository_group_roots(groups: &[RepositoryGroupConfigDocument]) -> Result<()> {
    for group in groups {
        let root = Path::new(&group.root);
        validate_repository_group_root(root).map_err(|error| {
            Error::new(
                error.kind(),
                format!(
                    "RepositoryGroup {} configured root {} is invalid: {}",
                    group.id,
                    root.display(),
                    error.message()
                ),
            )
        })?;
    }
    Ok(())
}

fn validate_repository_group_member_input(
    member_repository_ids: &[RepositoryId],
) -> Result<Vec<RepositoryId>> {
    if member_repository_ids.is_empty() {
        return Err(invalid(
            "RepositoryGroup requires at least one member Repository",
        ));
    }
    if member_repository_ids.len() > MAX_REPOSITORIES_PER_GROUP {
        return Err(invalid(format!(
            "RepositoryGroup accepts at most {MAX_REPOSITORIES_PER_GROUP} members"
        )));
    }
    let members = member_repository_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if members.len() != member_repository_ids.len() {
        return Err(invalid(
            "RepositoryGroup member Repository identities must be unique",
        ));
    }
    Ok(members.into_iter().collect())
}

fn inspect_repository_groups(
    groups: &[RepositoryGroupCatalogEntry],
) -> Result<Vec<RepositoryCatalogGroupCheck>> {
    groups
        .iter()
        .map(|group| {
            Ok(RepositoryCatalogGroupCheck {
                repository_group_id: group.repository_group_id,
                root_path: group.root_path.clone(),
                member_repository_ids: group.member_repository_ids.clone(),
                status: repository_group_root_status(&group.root_path)?,
            })
        })
        .collect()
}

fn validate_repository_group_members(
    repositories: &[RepositoryConfigDocument],
    root: &Path,
    members: &[RepositoryId],
) -> Result<()> {
    for member in members {
        let repository = repositories
            .iter()
            .find(|repository| &repository.id == member)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::RepositoryNotConfigured,
                    format!("RepositoryGroup member is not configured: {member}"),
                )
            })?;
        if !repository.paths.iter().any(|checkout| {
            let checkout = Path::new(checkout);
            checkout != root && checkout.starts_with(root)
        }) {
            return Err(invalid(format!(
                "RepositoryGroup member {member} has no checkout strictly below {}",
                root.display()
            )));
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

fn validate_repository_group_root(path: &Path) -> Result<PathBuf> {
    validate_canonical_directory(path, "RepositoryGroup root")?;
    Ok(path.to_path_buf())
}

fn repository_group_root_status(path: &Path) -> Result<CatalogRepositoryGroupStatus> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CatalogRepositoryGroupStatus::Missing);
        }
        Err(error) => return Err(io_error("inspect RepositoryGroup root")(error)),
    };
    if metadata.file_type().is_symlink() {
        return Ok(CatalogRepositoryGroupStatus::Symlink);
    }
    if !metadata.is_dir() {
        return Ok(CatalogRepositoryGroupStatus::NotDirectory);
    }
    let canonical =
        fs::canonicalize(path).map_err(io_error("canonicalize RepositoryGroup root"))?;
    Ok(if canonical == path {
        CatalogRepositoryGroupStatus::Available
    } else {
        CatalogRepositoryGroupStatus::NonCanonical
    })
}

fn validate_canonical_directory(path: &Path, field: &str) -> Result<()> {
    validate_absolute_path(path, field)?;
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect canonical directory"))?;
    if metadata.file_type().is_symlink() {
        return Err(invalid(format!("{field} must not be a symlink")));
    }
    if !metadata.is_dir() {
        return Err(invalid(format!("{field} must be a directory")));
    }
    let canonical = fs::canonicalize(path).map_err(io_error("canonicalize directory"))?;
    if canonical != path {
        return Err(invalid(format!("{field} must use its canonical path")));
    }
    Ok(())
}

fn current_repository_checkout_available(path: &Path) -> Result<bool> {
    if checkout_status(path)? != CatalogCheckoutStatus::Available {
        return Ok(false);
    }
    Ok(fs::canonicalize(path).map_err(io_error("canonicalize configured checkout"))? == path)
}

fn validate_current_repository_group_members(
    repositories: &[RepositoryConfigDocument],
    root: &Path,
    members: &[RepositoryId],
) -> Result<()> {
    for member in members {
        let repository = repositories
            .iter()
            .find(|repository| &repository.id == member)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::RepositoryNotConfigured,
                    format!("RepositoryGroup member is not configured: {member}"),
                )
            })?;
        let mut available = false;
        for checkout in repository
            .paths
            .iter()
            .map(Path::new)
            .filter(|checkout| *checkout != root && checkout.starts_with(root))
        {
            if current_repository_checkout_available(checkout)? {
                available = true;
                break;
            }
        }
        if !available {
            return Err(invariant(format!(
                "RepositoryGroup member {member} has no available checkout strictly below {}",
                root.display()
            )));
        }
    }
    Ok(())
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

fn finish_locked<T>(lock: &File, outcome: Result<T>) -> Result<T> {
    FileExt::unlock(lock).map_err(io_error("unlock config.lock"))?;
    outcome
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
