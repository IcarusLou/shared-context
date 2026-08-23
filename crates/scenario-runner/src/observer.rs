use std::{
    error, fmt, fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::Command,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};
use sctx_scenario_contract::ObservationSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::{TempDir, tempdir};

const MAX_FINGERPRINT_ENTRIES: usize = 20_000;
const MAX_FINGERPRINT_BYTES: u64 = 512 * 1024 * 1024;

/// Closed storage entities exposed by the test-only observer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationEntity {
    ExternalSession,
    ActiveTask,
    TaskSemanticState,
    TaskSession,
    WorkEpisode,
    Checkpoint,
    CandidateBuild,
    CandidateReview,
    ConfirmationOperation,
    GitEvent,
    GitObject,
    IndexProjection,
    IndexCandidate,
    IndexCandidateForEpisode,
    IndexContext,
    IndexSpace,
    IndexConfirmation,
    GraphProjection,
    GraphContext,
    GraphReference,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationSelector {
    entity: ObservationEntity,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    session_key: Option<String>,
}

/// Typed generation carried by one observed storage surface.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ObservedGeneration {
    Number(u64),
    Opaque(String),
}

/// Bounded identity/count/status/tree/generation summary. No raw payload columns are exposed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationSummary {
    pub source: ObservationSource,
    pub entity: ObservationEntity,
    pub present: bool,
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<ObservedGeneration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
}

/// Test-only hash of the complete source tree and file bytes, never a product generation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFingerprint {
    pub entry_count: u64,
    pub byte_count: u64,
    pub sha256: String,
}

/// Observation result paired with direct proof that the source bytes/tree were unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenObservation {
    pub summary: ObservationSummary,
    pub before: StateFingerprint,
    pub after: StateFingerprint,
    pub unchanged: bool,
}

/// Safe observer failures that never include paths, SQL, or source content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverErrorKind {
    InvalidSelector,
    MissingStorage,
    UnsafeStorage,
    SourceChanged,
    ReadFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverError {
    kind: ObserverErrorKind,
    message: &'static str,
}

impl ObserverError {
    const fn new(kind: ObserverErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> ObserverErrorKind {
        self.kind
    }
}

impl fmt::Display for ObserverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl error::Error for ObserverError {}

/// Strict observer for one runner-owned root. The only child it may start is the configured Git
/// executable, and only with read-only object-database commands.
#[derive(Clone, Debug)]
pub struct ReadOnlyObserver {
    git_binary: PathBuf,
}

impl ReadOnlyObserver {
    #[must_use]
    pub fn new(git_binary: impl Into<PathBuf>) -> Self {
        Self {
            git_binary: git_binary.into(),
        }
    }

    /// Observe one fixed storage surface and prove that source names and bytes did not change.
    ///
    /// # Errors
    ///
    /// Rejects unknown selectors, symlinks, changing snapshot sources, malformed stores, Git
    /// failures, or any before/after source mismatch.
    pub fn observe(
        &self,
        root: &Path,
        source: ObservationSource,
        selector: &Value,
    ) -> Result<ProvenObservation, ObserverError> {
        let selector: ObservationSelector =
            serde_json::from_value(selector.clone()).map_err(|_| {
                ObserverError::new(
                    ObserverErrorKind::InvalidSelector,
                    "observer selector is invalid",
                )
            })?;
        let before = state_fingerprint(root)?;
        let result = match source {
            ObservationSource::Runtime => observe_runtime(root, &selector),
            ObservationSource::Git => self.observe_git(root, &selector),
            ObservationSource::Index => observe_index(root, &selector),
            ObservationSource::Graph => observe_graph(root, &selector),
        };
        let after = state_fingerprint(root)?;
        if before != after {
            return Err(ObserverError::new(
                ObserverErrorKind::SourceChanged,
                "observer source changed during a read-only observation",
            ));
        }
        Ok(ProvenObservation {
            summary: result?,
            before,
            after,
            unchanged: true,
        })
    }

    fn observe_git(
        &self,
        root: &Path,
        selector: &ObservationSelector,
    ) -> Result<ObservationSummary, ObserverError> {
        if selector.session_key.is_some() {
            return Err(invalid_selector());
        }
        if !matches!(
            selector.entity,
            ObservationEntity::GitEvent | ObservationEntity::GitObject
        ) || selector.identity.as_deref().is_some_and(str::is_empty)
        {
            return Err(invalid_selector());
        }
        let repository = root.join("repository");
        if !repository.is_dir() {
            return Ok(absent(ObservationSource::Git, selector.entity));
        }
        reject_symlink(&repository)?;
        let head = self.git_output(&repository, &["rev-parse", "--verify", "HEAD"])?;
        let tree = self.git_output(&repository, &["rev-parse", "HEAD^{tree}"])?;
        let prefix = match selector.entity {
            ObservationEntity::GitEvent => "events",
            ObservationEntity::GitObject => "objects",
            _ => unreachable!(),
        };
        let paths = self.git_output_bytes(
            &repository,
            &["ls-tree", "-r", "-z", "--name-only", "HEAD", "--", prefix],
        )?;
        let mut count = 0_u64;
        let mut matched = None;
        for path in paths
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            if selector.identity.as_ref().is_none_or(|identity| {
                path.rsplit(|byte| *byte == b'/')
                    .next()
                    .is_some_and(|name| {
                        name == identity.as_bytes()
                            || name
                                .strip_suffix(b".json")
                                .is_some_and(|stem| stem == identity.as_bytes())
                    })
            }) {
                count = count.saturating_add(1);
                if count == 1 {
                    matched.clone_from(&selector.identity);
                }
            }
        }
        Ok(ObservationSummary {
            source: ObservationSource::Git,
            entity: selector.entity,
            present: true,
            count,
            identity: matched.or(Some(head)),
            status: None,
            tree: Some(tree),
            generation: None,
            schema_version: None,
        })
    }

    fn git_output(&self, repository: &Path, arguments: &[&str]) -> Result<String, ObserverError> {
        let bytes = self.git_output_bytes(repository, arguments)?;
        let value = String::from_utf8(bytes).map_err(|_| read_failed())?;
        let value = value.trim().to_owned();
        if value.len() > 128 || value.is_empty() {
            return Err(read_failed());
        }
        Ok(value)
    }

    fn git_output_bytes(
        &self,
        repository: &Path,
        arguments: &[&str],
    ) -> Result<Vec<u8>, ObserverError> {
        let output = Command::new(&self.git_binary)
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|_| read_failed())?;
        if !output.status.success() || output.stdout.len() > 2 * 1024 * 1024 {
            return Err(read_failed());
        }
        Ok(output.stdout)
    }
}

/// Hash every relative entry name, type, length, and file/symlink bytes under an isolated root.
///
/// # Errors
///
/// Returns a bounded read error for inaccessible or over-capacity trees.
pub fn state_fingerprint(root: &Path) -> Result<StateFingerprint, ObserverError> {
    if !root.exists() {
        return Ok(StateFingerprint {
            entry_count: 0,
            byte_count: 0,
            sha256: format!("{:x}", Sha256::digest([])),
        });
    }
    reject_symlink(root)?;
    let mut entries = Vec::<PathBuf>::new();
    collect_entries(root, root, &mut entries)?;
    entries.sort();
    if entries.len() > MAX_FINGERPRINT_ENTRIES {
        return Err(ObserverError::new(
            ObserverErrorKind::ReadFailed,
            "observer source tree exceeds the entry limit",
        ));
    }
    let mut digest = Sha256::new();
    let mut byte_count = 0_u64;
    for relative in &entries {
        let absolute = root.join(relative);
        let metadata = fs::symlink_metadata(&absolute).map_err(|_| read_failed())?;
        hash_os_text(&mut digest, relative.as_os_str());
        if metadata.file_type().is_symlink() {
            digest.update(b"symlink\0");
            hash_os_text(
                &mut digest,
                fs::read_link(&absolute)
                    .map_err(|_| read_failed())?
                    .as_os_str(),
            );
        } else if metadata.is_dir() {
            digest.update(b"directory\0");
        } else if metadata.is_file() {
            digest.update(b"file\0");
            digest.update(metadata.len().to_le_bytes());
            byte_count = byte_count.saturating_add(metadata.len());
            if byte_count > MAX_FINGERPRINT_BYTES {
                return Err(ObserverError::new(
                    ObserverErrorKind::ReadFailed,
                    "observer source bytes exceed the fingerprint limit",
                ));
            }
            let mut file = fs::File::open(&absolute).map_err(|_| read_failed())?;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file.read(&mut buffer).map_err(|_| read_failed())?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
        } else {
            return Err(ObserverError::new(
                ObserverErrorKind::UnsafeStorage,
                "observer source contains an unsupported file type",
            ));
        }
    }
    Ok(StateFingerprint {
        entry_count: entries.len() as u64,
        byte_count,
        sha256: format!("{:x}", digest.finalize()),
    })
}

fn collect_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), ObserverError> {
    for entry in fs::read_dir(directory).map_err(|_| read_failed())? {
        let entry = entry.map_err(|_| read_failed())?;
        let absolute = entry.path();
        let relative = absolute
            .strip_prefix(root)
            .map_err(|_| read_failed())?
            .to_path_buf();
        entries.push(relative.clone());
        if fs::symlink_metadata(&absolute)
            .map_err(|_| read_failed())?
            .is_dir()
        {
            collect_entries(root, &absolute, entries)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn hash_os_text(digest: &mut Sha256, value: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt as _;
    digest.update(value.as_bytes());
    digest.update(b"\0");
}

#[cfg(not(unix))]
fn hash_os_text(digest: &mut Sha256, value: &std::ffi::OsStr) {
    digest.update(value.to_string_lossy().as_bytes());
    digest.update(b"\0");
}

fn observe_runtime(
    root: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.entity == ObservationEntity::ActiveTask {
        return observe_active_task(&root.join("state/runtime.sqlite"), selector);
    }
    if selector.entity == ObservationEntity::TaskSemanticState {
        return observe_task_semantic_state(&root.join("state/runtime.sqlite"), selector);
    }
    let spec = match selector.entity {
        ObservationEntity::ExternalSession => {
            TableSpec::new("external_session", "external_session_id", None)
        }
        ObservationEntity::TaskSession => TableSpec::new("task_session", "task_session_id", None),
        ObservationEntity::WorkEpisode => {
            TableSpec::new("work_episode", "episode_id", Some("status"))
        }
        ObservationEntity::Checkpoint => {
            TableSpec::new("agent_checkpoint", "checkpoint_id", Some("boundary"))
        }
        ObservationEntity::CandidateBuild => {
            TableSpec::new("candidate_build", "build_id", Some("status"))
        }
        ObservationEntity::CandidateReview => {
            TableSpec::new("candidate_review", "candidate_id", Some("status"))
        }
        ObservationEntity::ConfirmationOperation => TableSpec::new(
            "candidate_confirmation_operation",
            "candidate_id",
            Some("status"),
        ),
        _ => return Err(invalid_selector()),
    };
    observe_table_database(
        &root.join("state/runtime.sqlite"),
        ObservationSource::Runtime,
        selector,
        spec,
        None,
    )
}

fn observe_index(
    root: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.entity == ObservationEntity::IndexProjection {
        return observe_index_projection(&root.join("state/index.sqlite"), selector);
    }
    if selector.entity == ObservationEntity::IndexCandidateForEpisode {
        return observe_index_candidate_for_episode(&root.join("state/index.sqlite"), selector);
    }
    let spec = match selector.entity {
        ObservationEntity::IndexCandidate => {
            TableSpec::new("context_candidate", "candidate_id", None)
        }
        ObservationEntity::IndexContext => {
            TableSpec::new("context_item", "context_id", Some("governance_status"))
        }
        ObservationEntity::IndexSpace => TableSpec::new("space_projection", "space_id", None),
        ObservationEntity::IndexConfirmation => {
            TableSpec::new("candidate_confirmation", "confirmation_id", None)
        }
        _ => return Err(invalid_selector()),
    };
    observe_table_database(
        &root.join("state/index.sqlite"),
        ObservationSource::Index,
        selector,
        spec,
        Some(index_metadata),
    )
}

fn observe_graph(
    root: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.entity == ObservationEntity::GraphProjection {
        return observe_graph_projection(&root.join("state/engineering.sqlite"), selector);
    }
    let spec = match selector.entity {
        ObservationEntity::GraphContext => {
            TableSpec::new("graph_context_snapshot", "context_id", None)
        }
        ObservationEntity::GraphReference => {
            TableSpec::new("resolved_reference", "reference_id", None)
        }
        _ => return Err(invalid_selector()),
    };
    observe_table_database(
        &root.join("state/engineering.sqlite"),
        ObservationSource::Graph,
        selector,
        spec,
        Some(graph_metadata),
    )
}

#[derive(Clone, Copy)]
struct TableSpec {
    table: &'static str,
    identity: &'static str,
    status: Option<&'static str>,
}

impl TableSpec {
    const fn new(
        table: &'static str,
        identity: &'static str,
        status: Option<&'static str>,
    ) -> Self {
        Self {
            table,
            identity,
            status,
        }
    }
}

type MetadataReader =
    fn(&Connection) -> Result<(Option<String>, Option<ObservedGeneration>), ObserverError>;

fn observe_table_database(
    database: &Path,
    source: ObservationSource,
    selector: &ObservationSelector,
    spec: TableSpec,
    metadata_reader: Option<MetadataReader>,
) -> Result<ObservationSummary, ObserverError> {
    if selector.session_key.is_some() {
        return Err(invalid_selector());
    }
    let Some(snapshot) = DatabaseSnapshot::copy(database)? else {
        return Ok(absent(source, selector.entity));
    };
    let connection = snapshot.open()?;
    let (count, identity) = table_count(&connection, spec, selector.identity.as_deref())?;
    let status = match (spec.status, identity.as_deref()) {
        (Some(column), Some(identity)) => connection
            .query_row(
                &format!(
                    "SELECT {column} FROM {} WHERE {} = ?1",
                    spec.table, spec.identity
                ),
                [identity],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| read_failed())?,
        _ => None,
    };
    let (tree, generation) =
        metadata_reader.map_or(Ok((None, None)), |reader| reader(&connection))?;
    let schema_version = schema_version(&connection)?;
    finish_read(&connection)?;
    Ok(ObservationSummary {
        source,
        entity: selector.entity,
        present: true,
        count,
        identity,
        status,
        tree,
        generation,
        schema_version: Some(schema_version),
    })
}

fn observe_active_task(
    database: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.identity.is_some() || selector.session_key.as_deref().is_none_or(str::is_empty) {
        return Err(invalid_selector());
    }
    let Some(snapshot) = DatabaseSnapshot::copy(database)? else {
        return Ok(absent(ObservationSource::Runtime, selector.entity));
    };
    let connection = snapshot.open()?;
    let (count, identity): (i64, Option<String>) = connection
        .query_row(
            "SELECT COUNT(*), MIN(active_task_id) FROM external_session WHERE external_session_key = ?1",
            [selector.session_key.as_deref().unwrap_or_default()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| read_failed())?;
    let schema_version = schema_version(&connection)?;
    finish_read(&connection)?;
    Ok(ObservationSummary {
        source: ObservationSource::Runtime,
        entity: selector.entity,
        present: true,
        count: u64::try_from(count).map_err(|_| read_failed())?,
        identity,
        status: None,
        tree: None,
        generation: None,
        schema_version: Some(schema_version),
    })
}

fn observe_task_semantic_state(
    database: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.identity.is_some() || selector.session_key.as_deref().is_none_or(str::is_empty) {
        return Err(invalid_selector());
    }
    let Some(snapshot) = DatabaseSnapshot::copy(database)? else {
        return Ok(absent(ObservationSource::Runtime, selector.entity));
    };
    let connection = snapshot.open()?;
    let state = connection
        .query_row(
            "SELECT external_session.active_task_id,
                    task_session.current_intent_revision_id,
                    (SELECT COUNT(*) FROM task_intent_revision
                     WHERE task_intent_revision.task_session_id = task_session.task_session_id)
             FROM external_session
             JOIN task_session
               ON task_session.task_session_id = external_session.active_task_session_id
             WHERE external_session.external_session_key = ?1",
            [selector.session_key.as_deref().unwrap_or_default()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| read_failed())?;
    let schema_version = schema_version(&connection)?;
    finish_read(&connection)?;
    let (identity, status, count) = state.map_or((None, None, 0), |(task, revision, count)| {
        (
            Some(task),
            Some(revision),
            u64::try_from(count).unwrap_or(0),
        )
    });
    Ok(ObservationSummary {
        source: ObservationSource::Runtime,
        entity: selector.entity,
        present: true,
        count,
        identity,
        status,
        tree: None,
        generation: None,
        schema_version: Some(schema_version),
    })
}

fn observe_index_candidate_for_episode(
    database: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.session_key.is_some() || selector.identity.as_deref().is_none_or(str::is_empty) {
        return Err(invalid_selector());
    }
    let Some(snapshot) = DatabaseSnapshot::copy(database)? else {
        return Ok(absent(ObservationSource::Index, selector.entity));
    };
    let connection = snapshot.open()?;
    let (count, identity): (i64, Option<String>) = connection
        .query_row(
            "SELECT COUNT(*), CASE WHEN COUNT(*) = 1 THEN MIN(candidate_id) END
             FROM context_candidate WHERE source_episode_id = ?1",
            [selector.identity.as_deref().unwrap_or_default()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| read_failed())?;
    let (tree, generation) = index_metadata(&connection)?;
    let schema_version = schema_version(&connection)?;
    finish_read(&connection)?;
    Ok(ObservationSummary {
        source: ObservationSource::Index,
        entity: selector.entity,
        present: true,
        count: u64::try_from(count).map_err(|_| read_failed())?,
        identity,
        status: None,
        tree,
        generation,
        schema_version: Some(schema_version),
    })
}

fn table_count(
    connection: &Connection,
    spec: TableSpec,
    selected_identity: Option<&str>,
) -> Result<(u64, Option<String>), ObserverError> {
    let (count, identity): (i64, Option<String>) = if let Some(identity) = selected_identity {
        connection
            .query_row(
                &format!(
                    "SELECT COUNT(*), MIN({}) FROM {} WHERE {} = ?1",
                    spec.identity, spec.table, spec.identity
                ),
                [identity],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| read_failed())?
    } else {
        connection
            .query_row(
                &format!(
                    "SELECT COUNT(*), CASE WHEN COUNT(*) = 1 THEN MIN({}) END FROM {}",
                    spec.identity, spec.table
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| read_failed())?
    };
    Ok((u64::try_from(count).map_err(|_| read_failed())?, identity))
}

fn observe_index_projection(
    database: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.identity.is_some() || selector.session_key.is_some() {
        return Err(invalid_selector());
    }
    let Some(snapshot) = DatabaseSnapshot::copy(database)? else {
        return Ok(absent(ObservationSource::Index, selector.entity));
    };
    let connection = snapshot.open()?;
    let (tree, generation) = index_metadata(&connection)?;
    let count = count_rows(&connection, "source_file")?;
    let schema_version = schema_version(&connection)?;
    finish_read(&connection)?;
    Ok(ObservationSummary {
        source: ObservationSource::Index,
        entity: selector.entity,
        present: true,
        count,
        identity: None,
        status: None,
        tree,
        generation,
        schema_version: Some(schema_version),
    })
}

fn observe_graph_projection(
    database: &Path,
    selector: &ObservationSelector,
) -> Result<ObservationSummary, ObserverError> {
    if selector.identity.is_some() || selector.session_key.is_some() {
        return Err(invalid_selector());
    }
    let Some(snapshot) = DatabaseSnapshot::copy(database)? else {
        return Ok(absent(ObservationSource::Graph, selector.entity));
    };
    let connection = snapshot.open()?;
    let (tree, generation) = graph_metadata(&connection)?;
    let count = count_rows(&connection, "graph_context_snapshot")?
        .saturating_add(count_rows(&connection, "resolved_reference")?);
    let schema_version = schema_version(&connection)?;
    finish_read(&connection)?;
    Ok(ObservationSummary {
        source: ObservationSource::Graph,
        entity: selector.entity,
        present: true,
        count,
        identity: None,
        status: None,
        tree,
        generation,
        schema_version: Some(schema_version),
    })
}

fn index_metadata(
    connection: &Connection,
) -> Result<(Option<String>, Option<ObservedGeneration>), ObserverError> {
    let tree = meta_value(connection, "meta", "indexed_tree_oid")?;
    let generation = meta_value(connection, "meta", "projection_generation")?
        .map(|value| value.parse::<u64>().map(ObservedGeneration::Number))
        .transpose()
        .map_err(|_| read_failed())?;
    Ok((tree, generation))
}

fn graph_metadata(
    connection: &Connection,
) -> Result<(Option<String>, Option<ObservedGeneration>), ObserverError> {
    connection
        .query_row(
            "SELECT context_tree_oid, artifact_generation FROM projection_meta WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    Some(ObservedGeneration::Opaque(row.get::<_, String>(1)?)),
                ))
            },
        )
        .optional()
        .map(|value| value.unwrap_or((None, None)))
        .map_err(|_| read_failed())
}

fn meta_value(
    connection: &Connection,
    table: &'static str,
    key: &'static str,
) -> Result<Option<String>, ObserverError> {
    connection
        .query_row(
            &format!("SELECT value FROM {table} WHERE key = ?1"),
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| read_failed())
}

fn count_rows(connection: &Connection, table: &'static str) -> Result<u64, ObserverError> {
    let count = connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| read_failed())?;
    u64::try_from(count).map_err(|_| read_failed())
}

fn schema_version(connection: &Connection) -> Result<u64, ObserverError> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|_| read_failed())?;
    u64::try_from(version).map_err(|_| read_failed())
}

fn finish_read(connection: &Connection) -> Result<(), ObserverError> {
    connection
        .execute_batch("ROLLBACK")
        .map_err(|_| read_failed())
}

struct DatabaseSnapshot {
    _temporary: TempDir,
    database: PathBuf,
}

impl DatabaseSnapshot {
    fn copy(source: &Path) -> Result<Option<Self>, ObserverError> {
        if !source.exists() {
            return Ok(None);
        }
        reject_symlink(source)?;
        if sidecar(source, "-journal").exists() {
            return Err(ObserverError::new(
                ObserverErrorKind::UnsafeStorage,
                "observer refuses an active rollback journal",
            ));
        }
        for _ in 0..3 {
            let before = database_files_fingerprint(source)?;
            let temporary = tempdir().map_err(|_| read_failed())?;
            let database = temporary.path().join("snapshot.sqlite");
            fs::copy(source, &database).map_err(|_| read_failed())?;
            for suffix in ["-wal", "-shm"] {
                let source_sidecar = sidecar(source, suffix);
                if source_sidecar.exists() {
                    reject_symlink(&source_sidecar)?;
                    fs::copy(&source_sidecar, sidecar(&database, suffix))
                        .map_err(|_| read_failed())?;
                }
            }
            let after = database_files_fingerprint(source)?;
            if before == after {
                return Ok(Some(Self {
                    _temporary: temporary,
                    database,
                }));
            }
        }
        Err(ObserverError::new(
            ObserverErrorKind::SourceChanged,
            "SQLite source changed while creating a read-only snapshot",
        ))
    }

    fn open(&self) -> Result<Connection, ObserverError> {
        let connection = Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|_| read_failed())?;
        connection
            .execute_batch("PRAGMA query_only = ON; BEGIN DEFERRED")
            .map_err(|_| read_failed())?;
        Ok(connection)
    }
}

fn database_files_fingerprint(database: &Path) -> Result<String, ObserverError> {
    let mut digest = Sha256::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = if suffix.is_empty() {
            database.to_path_buf()
        } else {
            sidecar(database, suffix)
        };
        digest.update(suffix.as_bytes());
        if path.exists() {
            reject_symlink(&path)?;
            let mut file = fs::File::open(path).map_err(|_| read_failed())?;
            io::copy(&mut file, &mut DigestWriter(&mut digest)).map_err(|_| read_failed())?;
        } else {
            digest.update(b"missing");
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

struct DigestWriter<'a>(&'a mut Sha256);

impl io::Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut value = database.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn reject_symlink(path: &Path) -> Result<(), ObserverError> {
    if fs::symlink_metadata(path)
        .map_err(|_| read_failed())?
        .file_type()
        .is_symlink()
    {
        Err(ObserverError::new(
            ObserverErrorKind::UnsafeStorage,
            "observer refuses symlinked storage",
        ))
    } else {
        Ok(())
    }
}

fn absent(source: ObservationSource, entity: ObservationEntity) -> ObservationSummary {
    ObservationSummary {
        source,
        entity,
        present: false,
        count: 0,
        identity: None,
        status: None,
        tree: None,
        generation: None,
        schema_version: None,
    }
}

fn invalid_selector() -> ObserverError {
    ObserverError::new(
        ObserverErrorKind::InvalidSelector,
        "observer entity does not belong to the selected source",
    )
}

fn read_failed() -> ObserverError {
    ObserverError::new(
        ObserverErrorKind::ReadFailed,
        "read-only observation failed",
    )
}
