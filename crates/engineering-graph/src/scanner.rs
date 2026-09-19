use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use sctx_domain::{
    ArtifactKey, ArtifactKind, ArtifactLocator, EngineeringArtifact, Error, ErrorKind,
    RepoRelativePath, RepositoryIdentity, Result,
};
use sha2::{Digest, Sha256};

const POLICY_VERSION: &str = "planned-paths-plus-safe-tracked-modifications-v2";
/// Global hard bound for one explicit Repository scan plan.
pub const MAX_REPOSITORY_SCAN_PLAN_PATHS: usize = 10_000;
/// Hard ceiling on one batched Git pathspec argument list, in bytes.
const MAX_GIT_PATHSPEC_ARGV_BYTES: usize = 96 * 1024;
/// Hard ceiling on captured stdout for one batched local Git call.
const MAX_GIT_STDOUT_BYTES: usize = 16 * 1024 * 1024;
/// Hard ceiling on captured stderr for one local Git call.
const MAX_GIT_STDERR_BYTES: usize = 8 * 1024;
/// Wall-clock ceiling for one local Git call when the scan carries no budget.
const GIT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Floor applied to a budget-derived Git timeout so a nearly spent budget still asks once.
const GIT_CALL_MIN_TIMEOUT: Duration = Duration::from_millis(250);
/// How often a running local Git child is checked against its wall-clock bound.
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Exact source policy used for a `RepositorySnapshot`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotSourcePolicy {
    /// Read only explicit planned tracked paths and their safe current modifications.
    PlannedPathsWithSafeTrackedModifications,
}

/// Which tracked source supplied one observation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactSourceState {
    TrackedHead,
    TrackedWorkingModification,
}

/// Language family selected by bounded lightweight extraction.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SourceLanguage {
    Rust,
    TypeScriptJavaScript,
    Swift,
    Kotlin,
    Java,
    Json,
    Yaml,
    Proto,
    Xml,
}

/// One path-level observation of an Artifact. Source contents are never retained.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArtifactObservation {
    pub path: String,
    pub line: Option<u32>,
    pub language: SourceLanguage,
    pub source_state: ArtifactSourceState,
}

/// One derived Artifact plus exact snapshot provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotArtifact {
    pub artifact: EngineeringArtifact,
    pub snapshot_generation: String,
    pub source_policy: SnapshotSourcePolicy,
    pub observations: Vec<ArtifactObservation>,
}

/// Why one tracked path did not enter the source snapshot.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SkippedFileReason {
    Missing,
    Untracked,
    IgnoredDirectory,
    Generated,
    UnsupportedLanguage,
    Symlink,
    EscapesRepository,
    Oversized,
    Binary,
    FileLimit,
    TotalByteLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkippedFileReason,
}

/// How much of a scan plan one snapshot actually inspected.
///
/// A budgeted scan may stop before the whole plan is read. The snapshot it still commits is then
/// only authoritative for the paths it reached, and every consumer that would otherwise conclude
/// "this Artifact is gone" has to know where the evidence stops. `Complete` says the plan was read
/// end to end; `Partial` names both halves explicitly so no consumer has to infer the boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanCoverage {
    /// Every planned path was inspected.
    Complete,
    /// The budget expired mid-plan; only `covered` carries evidence.
    Partial {
        /// Planned paths the scan actually inspected, sorted and deduplicated.
        covered: Vec<RepoRelativePath>,
        /// Planned paths the scan never reached, sorted and deduplicated.
        unfinished: Vec<RepoRelativePath>,
    },
}

impl ScanCoverage {
    /// Reports whether this snapshot read its whole plan.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Reports whether the snapshot carries evidence about `path`.
    ///
    /// A complete scan covers everything it planned; a partial one covers exactly what it read.
    #[must_use]
    pub fn covers_path(&self, path: &str) -> bool {
        match self {
            Self::Complete => true,
            Self::Partial { covered, .. } => covered.iter().any(|entry| entry.as_str() == path),
        }
    }

    /// Reports whether the snapshot carries evidence about anything inside directory `path`.
    ///
    /// Module Artifacts name a directory rather than a file, so a directory is covered as soon as
    /// one file beneath it was read.
    #[must_use]
    pub fn covers_directory(&self, path: &str) -> bool {
        match self {
            Self::Complete => true,
            Self::Partial { covered, .. } => covered.iter().any(|entry| {
                entry
                    .as_str()
                    .strip_prefix(path)
                    .is_some_and(|tail| tail.starts_with('/'))
            }),
        }
    }

    /// Directory prefixes that still hold at least one unread planned path.
    #[must_use]
    pub fn unfinished_prefixes(&self) -> Vec<String> {
        match self {
            Self::Complete => Vec::new(),
            Self::Partial { unfinished, .. } => unfinished
                .iter()
                .map(|path| module_name(path.as_str()))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    }

    fn digest_tag(&self) -> String {
        match self {
            Self::Complete => "coverage:complete".to_owned(),
            Self::Partial { covered, .. } => {
                format!("coverage:partial:{}", covered.len())
            }
        }
    }
}

/// Deterministic derived snapshot of one available Repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySnapshot {
    pub repository_id: sctx_domain::RepositoryId,
    pub source_policy: SnapshotSourcePolicy,
    pub policy_version: &'static str,
    pub head_tree_oid: String,
    pub generation: String,
    pub planned_paths: Vec<RepoRelativePath>,
    /// How much of `planned_paths` this snapshot is authoritative for.
    pub coverage: ScanCoverage,
    pub artifacts: Vec<SnapshotArtifact>,
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub skipped_files: Vec<SkippedFile>,
}

/// Typed availability result; an absent checkout is not a generic parser error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryScanOutcome {
    Available(RepositorySnapshot),
    Unavailable {
        repository_id: sctx_domain::RepositoryId,
        reason: String,
    },
}

/// Hard bounds applied before any source parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryScannerLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

/// Typed, deterministic, bounded set of Repository-relative paths to inspect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryScanPlan {
    repository_id: sctx_domain::RepositoryId,
    paths: Vec<RepoRelativePath>,
}

impl RepositoryScanPlan {
    /// Creates a stable sorted plan and removes duplicate paths.
    ///
    /// # Errors
    ///
    /// Returns an input error for an empty or globally oversized plan.
    pub fn new(
        repository_id: sctx_domain::RepositoryId,
        paths: Vec<RepoRelativePath>,
    ) -> Result<Self> {
        let paths = paths.into_iter().collect::<BTreeSet<_>>();
        if paths.is_empty() {
            return Err(invalid(
                "Repository ScanPlan must contain at least one path",
            ));
        }
        if paths.len() > MAX_REPOSITORY_SCAN_PLAN_PATHS {
            return Err(invalid(format!(
                "Repository ScanPlan exceeds {MAX_REPOSITORY_SCAN_PLAN_PATHS} paths"
            )));
        }
        Ok(Self {
            repository_id,
            paths: paths.into_iter().collect(),
        })
    }

    #[must_use]
    pub fn repository_id(&self) -> sctx_domain::RepositoryId {
        self.repository_id.clone()
    }

    #[must_use]
    pub fn paths(&self) -> &[RepoRelativePath] {
        &self.paths
    }
}

impl Default for RepositoryScannerLimits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Deterministic bounded Repository snapshot scanner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryScanner {
    limits: RepositoryScannerLimits,
}

impl RepositoryScanner {
    #[must_use]
    pub const fn new(limits: RepositoryScannerLimits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub const fn limits(&self) -> RepositoryScannerLimits {
        self.limits
    }

    /// Scans one tracked source snapshot without retaining full file contents.
    ///
    /// # Errors
    ///
    /// Returns typed validation, local Git, filesystem, or parsing errors.
    pub fn scan(
        &self,
        repository: &RepositoryIdentity,
        checkout_path: &Path,
        plan: &RepositoryScanPlan,
    ) -> Result<RepositoryScanOutcome> {
        self.scan_before(repository, checkout_path, plan, None)?
            .ok_or_else(|| invariant("an unbudgeted Repository scan cannot be abandoned"))
    }

    /// Scans the same snapshot under a wall-clock budget.
    ///
    /// A budget that expires mid-plan no longer throws the whole scan away. The snapshot is
    /// committed for the planned paths that were read and carries a [`ScanCoverage::Partial`]
    /// record of where the evidence stops, so the resolver can answer for what was scanned and
    /// stay silent about the rest. Discarding everything was the older way of keeping a budgeted
    /// scan from reporting unscanned Artifacts as missing; the coverage record keeps that exact
    /// guarantee while letting partial work count.
    ///
    /// `Ok(None)` survives for the degenerate case: the budget expired before a single planned
    /// path was inspected, so there is no evidence to commit at all.
    ///
    /// Tracked-state facts for the whole plan are gathered in a few batched local Git calls before
    /// any file is read, so the clock bounds file reading and parsing rather than a per-file
    /// process spawn. A single oversized file is still bounded by [`RepositoryScannerLimits`].
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::scan`].
    #[allow(clippy::too_many_lines)]
    pub fn scan_before(
        &self,
        repository: &RepositoryIdentity,
        checkout_path: &Path,
        plan: &RepositoryScanPlan,
        deadline: Option<Instant>,
    ) -> Result<Option<RepositoryScanOutcome>> {
        let expired = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
        repository.validate()?;
        if plan.repository_id != repository.repository_id {
            return Err(invalid(
                "Repository ScanPlan must belong to the scanned Repository",
            ));
        }
        if !checkout_path.exists() {
            return Ok(Some(RepositoryScanOutcome::Unavailable {
                repository_id: repository.repository_id.clone(),
                reason: "Repository checkout is unavailable".to_owned(),
            }));
        }
        let root = validate_checkout_root(checkout_path)?;
        let head_tree_oid = git_text(&root, &["rev-parse", "HEAD^{tree}"], deadline)?;
        let tracked = TrackedPathFacts::collect(&root, plan.paths(), deadline)?;
        let mut sources = Vec::new();
        let mut skipped_files = Vec::new();
        let mut covered = Vec::with_capacity(plan.paths.len());
        let mut unfinished = Vec::new();
        let mut scanned_bytes = 0_u64;
        let mut budget_expired = false;
        for planned_path in &plan.paths {
            if budget_expired || expired() {
                budget_expired = true;
                unfinished.push(planned_path.clone());
                continue;
            }
            covered.push(planned_path.clone());
            let relative = planned_path.as_str().to_owned();
            if sources.len() >= self.limits.max_files {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::FileLimit,
                });
                continue;
            }
            let Some(language) = language_for_path(&relative) else {
                let reason = if is_ignored_path(&relative) {
                    SkippedFileReason::IgnoredDirectory
                } else if is_generated_path(&relative) {
                    SkippedFileReason::Generated
                } else {
                    SkippedFileReason::UnsupportedLanguage
                };
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason,
                });
                continue;
            };
            if is_ignored_path(&relative) {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::IgnoredDirectory,
                });
                continue;
            }
            if is_generated_path(&relative) {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::Generated,
                });
                continue;
            }
            let full_path = root.join(&relative);
            let metadata = match fs::symlink_metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    skipped_files.push(SkippedFile {
                        path: relative,
                        reason: SkippedFileReason::Missing,
                    });
                    continue;
                }
                Err(error) => return Err(io_error("inspect tracked source")(error)),
            };
            if metadata.file_type().is_symlink() {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::Symlink,
                });
                continue;
            }
            if !tracked.is_tracked(&relative) {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::Untracked,
                });
                continue;
            }
            let canonical =
                fs::canonicalize(&full_path).map_err(io_error("canonicalize tracked source"))?;
            if !canonical.starts_with(&root) {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::EscapesRepository,
                });
                continue;
            }
            if metadata.len() > self.limits.max_file_bytes {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::Oversized,
                });
                continue;
            }
            if scanned_bytes.saturating_add(metadata.len()) > self.limits.max_total_bytes {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::TotalByteLimit,
                });
                continue;
            }
            let bytes = fs::read(&canonical).map_err(io_error("read tracked source"))?;
            if is_binary(&bytes) {
                skipped_files.push(SkippedFile {
                    path: relative,
                    reason: SkippedFileReason::Binary,
                });
                continue;
            }
            scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
            let source_state = tracked.source_state(&relative);
            sources.push(SourceFile {
                path: relative,
                language,
                source_state,
                version_digest: file_version_digest(&bytes),
                bytes,
            });
        }
        sources.sort_by(|left, right| left.path.cmp(&right.path));
        skipped_files.sort_by(|left, right| left.path.cmp(&right.path));
        let mut builder = ArtifactBuilder::new(repository);
        let mut parsed = 0_usize;
        for source in &sources {
            if expired() {
                break;
            }
            builder.scan_source(source)?;
            parsed += 1;
        }
        if parsed < sources.len() {
            let mut unparsed = BTreeSet::new();
            for source in sources.split_off(parsed) {
                let path = repo_path(&source.path)?;
                unparsed.insert(path.clone());
                unfinished.push(path);
            }
            covered.retain(|entry| !unparsed.contains(entry));
            unfinished.sort();
            scanned_bytes = sources
                .iter()
                .map(|source| source.bytes.len() as u64)
                .fold(0_u64, u64::saturating_add);
        }
        if covered.is_empty() {
            return Ok(None);
        }
        let coverage = if unfinished.is_empty() {
            ScanCoverage::Complete
        } else {
            ScanCoverage::Partial {
                covered,
                unfinished,
            }
        };
        let generation = snapshot_generation(
            repository,
            &head_tree_oid,
            &plan.paths,
            &coverage,
            &sources,
            &skipped_files,
        );
        let artifacts = builder.finish(&generation);
        Ok(Some(RepositoryScanOutcome::Available(RepositorySnapshot {
            repository_id: repository.repository_id.clone(),
            source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
            policy_version: POLICY_VERSION,
            head_tree_oid,
            generation,
            planned_paths: plan.paths.clone(),
            coverage,
            artifacts,
            scanned_files: sources.len(),
            scanned_bytes,
            skipped_files,
        })))
    }

    /// Reuses an identical prior snapshot, otherwise returns the same result as a scratch scan.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::scan`].
    pub fn scan_incremental(
        &self,
        previous: &RepositorySnapshot,
        repository: &RepositoryIdentity,
        checkout_path: &Path,
        plan: &RepositoryScanPlan,
    ) -> Result<RepositoryScanOutcome> {
        let current = self.scan(repository, checkout_path, plan)?;
        if let RepositoryScanOutcome::Available(snapshot) = &current
            && snapshot.generation == previous.generation
        {
            return Ok(RepositoryScanOutcome::Available(previous.clone()));
        }
        Ok(current)
    }
}

impl Default for RepositoryScanner {
    fn default() -> Self {
        Self::new(RepositoryScannerLimits::default())
    }
}

struct SourceFile {
    path: String,
    language: SourceLanguage,
    source_state: ArtifactSourceState,
    version_digest: String,
    bytes: Vec<u8>,
}

struct ArtifactBuilder<'a> {
    repository: &'a RepositoryIdentity,
    artifacts: BTreeMap<String, SnapshotArtifact>,
}

impl<'a> ArtifactBuilder<'a> {
    /// Builds Artifacts before the snapshot generation is known.
    ///
    /// A budgeted scan can only name its generation once it knows how much of the plan it read,
    /// and it only knows that once parsing has stopped, so the generation is stamped in
    /// [`Self::finish`] rather than carried through the build.
    fn new(repository: &'a RepositoryIdentity) -> Self {
        Self {
            repository,
            artifacts: BTreeMap::new(),
        }
    }

    fn scan_source(&mut self, source: &SourceFile) -> Result<()> {
        let text = std::str::from_utf8(&source.bytes)
            .map_err(|error| invalid(format!("tracked source is not UTF-8: {error}")))?;
        let module = module_name(&source.path);
        self.insert(
            artifact(
                self.repository,
                ArtifactLocator::File {
                    path: repo_path(&source.path)?,
                },
                &source.path,
            )?,
            observation(source, None),
        );
        if let Some(module_path) = module_path(&source.path) {
            self.insert(
                artifact(
                    self.repository,
                    ArtifactLocator::Module {
                        path: repo_path(&module_path)?,
                    },
                    &module,
                )?,
                observation(source, None),
            );
        }
        for discovery in extract(source.language, &source.path, text) {
            let locator = match discovery.kind {
                ArtifactKind::Symbol => ArtifactLocator::Symbol {
                    path: repo_path(&source.path)?,
                    language: language_name(source.language).to_owned(),
                    module: module.clone(),
                    enclosing_type: None,
                    symbol_name: discovery.name.clone(),
                    signature: normalize_signature(&discovery.source),
                },
                ArtifactKind::Test => ArtifactLocator::Test {
                    path: repo_path(&source.path)?,
                    qualified_test_name: format!("{module}::{}", discovery.name),
                },
                ArtifactKind::Api => ArtifactLocator::Api {
                    path: repo_path(&source.path)?,
                    protocol: api_protocol(&discovery.source).to_owned(),
                    operation: api_operation(&discovery.source).to_owned(),
                    normalized_route: normalize_api_key(&discovery.name),
                },
                ArtifactKind::Schema => ArtifactLocator::Schema {
                    path: repo_path(&source.path)?,
                    namespace: module.clone(),
                    version: schema_version(&discovery.source),
                    qualified_name: format!("{module}::{}", discovery.name),
                },
                _ => continue,
            };
            self.insert(
                artifact(self.repository, locator, &discovery.name)?,
                observation(source, Some(discovery.line)),
            );
        }
        Ok(())
    }

    fn insert(&mut self, artifact: EngineeringArtifact, observation: ArtifactObservation) {
        let key = artifact.artifact_key.digest().to_owned();
        if let Some(existing) = self.artifacts.get_mut(&key) {
            if !existing.observations.contains(&observation) {
                existing.observations.push(observation);
                existing.observations.sort();
            }
            return;
        }
        self.artifacts.insert(
            key,
            SnapshotArtifact {
                artifact,
                snapshot_generation: String::new(),
                source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
                observations: vec![observation],
            },
        );
    }

    fn finish(self, generation: &str) -> Vec<SnapshotArtifact> {
        self.artifacts
            .into_values()
            .map(|mut artifact| {
                generation.clone_into(&mut artifact.snapshot_generation);
                artifact
            })
            .collect()
    }
}

fn artifact(
    repository: &RepositoryIdentity,
    locator: ArtifactLocator,
    display_name: &str,
) -> Result<EngineeringArtifact> {
    let artifact = EngineeringArtifact {
        repository: repository.clone(),
        artifact_key: ArtifactKey::derive(repository.repository_id.clone(), locator)?,
        display_name: display_name.to_owned(),
    };
    artifact.validate()?;
    Ok(artifact)
}

fn observation(source: &SourceFile, line: Option<u32>) -> ArtifactObservation {
    ArtifactObservation {
        path: source.path.clone(),
        line,
        language: source.language,
        source_state: source.source_state,
    }
}

#[derive(Clone)]
struct Discovery {
    kind: ArtifactKind,
    name: String,
    line: u32,
    source: String,
}

fn extract(language: SourceLanguage, path: &str, text: &str) -> Vec<Discovery> {
    match language {
        SourceLanguage::Rust => extract_code(text, path, language, &rust_declaration),
        SourceLanguage::TypeScriptJavaScript => {
            extract_code(text, path, language, &typescript_declaration)
        }
        SourceLanguage::Swift => extract_code(text, path, language, &swift_declaration),
        SourceLanguage::Kotlin => extract_code(text, path, language, &kotlin_declaration),
        SourceLanguage::Java => extract_code(text, path, language, &java_declaration),
        SourceLanguage::Json => extract_json(path, text),
        SourceLanguage::Yaml => extract_yaml(path, text),
        SourceLanguage::Proto => extract_proto(text),
        // Android resource and manifest XML carries no symbol a deterministic locator could name,
        // so it is an Artifact at File and Module level only. Nothing is guessed from its markup.
        SourceLanguage::Xml => Vec::new(),
    }
}

type DeclarationParser = dyn Fn(&str, bool) -> Vec<(ArtifactKind, String)>;

fn extract_code(
    text: &str,
    _path: &str,
    language: SourceLanguage,
    parser: &DeclarationParser,
) -> Vec<Discovery> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut discoveries = Vec::new();
    let mut test_annotation = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if matches!(language, SourceLanguage::Rust) && trimmed.starts_with("#[test")
            || matches!(language, SourceLanguage::Kotlin | SourceLanguage::Java)
                && trimmed == "@Test"
        {
            test_annotation = true;
            continue;
        }
        for (kind, name) in parser(trimmed, test_annotation) {
            let source = declaration_source(&lines, index);
            discoveries.push(Discovery {
                kind,
                source,
                name,
                line: line_number(index),
            });
        }
        test_annotation = false;
        for api in api_keys(trimmed) {
            discoveries.push(Discovery {
                kind: ArtifactKind::Api,
                name: api.clone(),
                line: line_number(index),
                source: trimmed.to_owned(),
            });
        }
        if matches!(language, SourceLanguage::TypeScriptJavaScript)
            && let Some(name) = test_call_name(trimmed)
        {
            discoveries.push(Discovery {
                kind: ArtifactKind::Test,
                name: name.clone(),
                line: line_number(index),
                source: trimmed.to_owned(),
            });
        }
    }
    discoveries
}

fn rust_declaration(line: &str, is_test: bool) -> Vec<(ArtifactKind, String)> {
    let line = strip_prefixes(line, &["pub ", "pub(crate) ", "async ", "unsafe "]);
    declaration_tokens(
        line,
        &[
            (
                "fn ",
                if is_test {
                    ArtifactKind::Test
                } else {
                    ArtifactKind::Symbol
                },
            ),
            ("struct ", ArtifactKind::Symbol),
            ("enum ", ArtifactKind::Symbol),
            ("trait ", ArtifactKind::Symbol),
            ("type ", ArtifactKind::Symbol),
            ("mod ", ArtifactKind::Module),
        ],
    )
}

fn typescript_declaration(line: &str, _is_test: bool) -> Vec<(ArtifactKind, String)> {
    let line = strip_prefixes(line, &["export ", "default ", "declare ", "async "]);
    declaration_tokens(
        line,
        &[
            ("function ", ArtifactKind::Symbol),
            ("class ", ArtifactKind::Symbol),
            ("interface ", ArtifactKind::Schema),
            ("type ", ArtifactKind::Schema),
            ("enum ", ArtifactKind::Schema),
            ("const ", ArtifactKind::Symbol),
        ],
    )
}

fn swift_declaration(line: &str, _is_test: bool) -> Vec<(ArtifactKind, String)> {
    let line = strip_prefixes(
        line,
        &["public ", "private ", "internal ", "open ", "static "],
    );
    let mut result = declaration_tokens(
        line,
        &[
            ("func ", ArtifactKind::Symbol),
            ("class ", ArtifactKind::Symbol),
            ("struct ", ArtifactKind::Symbol),
            ("enum ", ArtifactKind::Schema),
            ("protocol ", ArtifactKind::Schema),
        ],
    );
    for (kind, name) in &mut result {
        if *kind == ArtifactKind::Symbol && name.starts_with("test") {
            *kind = ArtifactKind::Test;
        }
    }
    result
}

fn kotlin_declaration(line: &str, is_test: bool) -> Vec<(ArtifactKind, String)> {
    let line = strip_prefixes(
        line,
        &[
            "public ",
            "private ",
            "internal ",
            "protected ",
            "suspend ",
            "data ",
        ],
    );
    declaration_tokens(
        line,
        &[
            (
                "fun ",
                if is_test {
                    ArtifactKind::Test
                } else {
                    ArtifactKind::Symbol
                },
            ),
            ("class ", ArtifactKind::Symbol),
            ("object ", ArtifactKind::Symbol),
            ("interface ", ArtifactKind::Schema),
            ("enum class ", ArtifactKind::Schema),
        ],
    )
}

/// Extracts top-level Java declarations with the same shape as the Kotlin parser.
///
/// Java has no keyword introducing a method, so a method is recognised structurally: once the
/// modifiers are stripped, a declaration reads as a return type followed by the name and an
/// argument list. Anything that does not read that way is left alone rather than guessed at.
fn java_declaration(line: &str, is_test: bool) -> Vec<(ArtifactKind, String)> {
    let line = strip_prefixes(
        line,
        &[
            "public ",
            "private ",
            "protected ",
            "abstract ",
            "final ",
            "static ",
            "synchronized ",
            "native ",
            "strictfp ",
            "default ",
        ],
    );
    let declared = declaration_tokens(
        line,
        &[
            ("class ", ArtifactKind::Symbol),
            ("record ", ArtifactKind::Symbol),
            ("interface ", ArtifactKind::Schema),
            ("enum ", ArtifactKind::Schema),
            ("@interface ", ArtifactKind::Schema),
        ],
    );
    if !declared.is_empty() {
        return declared;
    }
    java_method_name(line).map_or_else(Vec::new, |name| {
        vec![(
            if is_test {
                ArtifactKind::Test
            } else {
                ArtifactKind::Symbol
            },
            name,
        )]
    })
}

/// Reads the name out of a Java method declaration, or nothing when the line is not one.
fn java_method_name(line: &str) -> Option<String> {
    let head = line.split_once('(')?.0;
    if !(line.ends_with('{') || line.ends_with(';')) {
        return None;
    }
    let mut tokens = head.split_whitespace().collect::<Vec<_>>();
    let name = tokens.pop()?;
    // A bare `name(...)` is a constructor or a call, and a control keyword is neither: both would
    // be a guess about a symbol Java never declared here.
    if tokens.is_empty()
        || matches!(
            name,
            "if" | "for" | "while" | "switch" | "catch" | "return" | "new" | "synchronized"
        )
    {
        return None;
    }
    let identifier = identifier(name)?;
    (identifier == name
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_'))
    .then(|| name.to_owned())
}

fn declaration_tokens(
    line: &str,
    patterns: &[(&str, ArtifactKind)],
) -> Vec<(ArtifactKind, String)> {
    patterns
        .iter()
        .find_map(|(prefix, kind)| {
            line.strip_prefix(prefix)
                .and_then(|tail| identifier(tail).map(|name| vec![(*kind, name.to_owned())]))
        })
        .unwrap_or_default()
}

fn strip_prefixes<'a>(mut line: &'a str, prefixes: &[&str]) -> &'a str {
    loop {
        let Some(prefix) = prefixes.iter().find(|prefix| line.starts_with(**prefix)) else {
            return line;
        };
        line = &line[prefix.len()..];
    }
}

fn identifier(value: &str) -> Option<&str> {
    let end = value
        .char_indices()
        .take_while(|(_, character)| character.is_alphanumeric() || *character == '_')
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    Some(&value[..end])
}

fn declaration_source(lines: &[&str], start: usize) -> String {
    let mut result = String::new();
    let mut balance = 0_i32;
    let mut saw_brace = false;
    for line in lines.iter().skip(start).take(32) {
        result.push_str(line);
        result.push('\n');
        for character in line.chars() {
            if character == '{' {
                balance += 1;
                saw_brace = true;
            } else if character == '}' {
                balance -= 1;
            }
        }
        if saw_brace && balance <= 0 {
            break;
        }
        if !saw_brace {
            break;
        }
    }
    result
}

fn api_keys(line: &str) -> Vec<String> {
    if ![
        "fetch",
        "url",
        "route",
        "get(",
        "post(",
        "requestmapping",
        "endpoint",
    ]
    .iter()
    .any(|token| line.to_ascii_lowercase().contains(token))
    {
        return Vec::new();
    }
    quoted_values(line)
        .into_iter()
        .filter(|value| value.starts_with('/') || value.contains("://"))
        .map(|value| normalize_api_key(&value))
        .collect()
}

fn test_call_name(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if !(lower.starts_with("test(") || lower.starts_with("it(") || lower.starts_with("describe(")) {
        return None;
    }
    quoted_values(line).into_iter().next()
}

fn quoted_values(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut quote = None;
    let mut start = 0;
    for (index, character) in line.char_indices() {
        match quote {
            None if matches!(character, '\'' | '"' | '`') => {
                quote = Some(character);
                start = index + character.len_utf8();
            }
            Some(expected) if character == expected => {
                values.push(line[start..index].to_owned());
                quote = None;
            }
            _ => {}
        }
    }
    values
}

fn normalize_api_key(value: &str) -> String {
    let mut value = value.trim().to_owned();
    while value.len() > 1 && value.ends_with('/') {
        value.pop();
    }
    value
}

fn extract_json(path: &str, text: &str) -> Vec<Discovery> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut discoveries = vec![discovery(
        ArtifactKind::Schema,
        value
            .get("$id")
            .or_else(|| value.get("title"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| file_stem(path)),
        1,
    )];
    if value.get("openapi").is_some() {
        if let Some(paths) = value.get("paths").and_then(serde_json::Value::as_object) {
            for (route, operations) in paths {
                if let Some(operations) = operations.as_object() {
                    for operation in operations.keys().filter(|operation| {
                        matches!(
                            operation.to_ascii_lowercase().as_str(),
                            "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
                        )
                    }) {
                        discoveries.push(Discovery {
                            kind: ArtifactKind::Api,
                            name: route.clone(),
                            line: 1,
                            source: format!("{operation}({route})"),
                        });
                    }
                }
            }
        }
        if let Some(schemas) = value
            .pointer("/components/schemas")
            .and_then(serde_json::Value::as_object)
        {
            discoveries.extend(
                schemas
                    .keys()
                    .map(|name| discovery(ArtifactKind::Schema, name, 1)),
            );
        }
    }
    discoveries
}

fn extract_yaml(path: &str, text: &str) -> Vec<Discovery> {
    let mut discoveries = vec![discovery(ArtifactKind::Schema, file_stem(path), 1)];
    let mut section = "";
    let mut current_route = None::<String>;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed == "paths:" {
            section = "paths";
            continue;
        }
        if trimmed == "schemas:" || trimmed == "components:" {
            section = if trimmed == "schemas:" {
                "schemas"
            } else {
                "components"
            };
            continue;
        }
        if section == "components" && trimmed == "schemas:" {
            section = "schemas";
            continue;
        }
        let Some(key) = trimmed.strip_suffix(':') else {
            continue;
        };
        if section == "paths" && key.starts_with('/') {
            current_route = Some(key.to_owned());
        } else if section == "paths"
            && matches!(
                key.to_ascii_lowercase().as_str(),
                "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
            )
            && let Some(route) = &current_route
        {
            discoveries.push(Discovery {
                kind: ArtifactKind::Api,
                name: route.clone(),
                line: line_number(index),
                source: format!("{key}({route})"),
            });
        } else if section == "schemas" && !key.is_empty() {
            discoveries.push(discovery(ArtifactKind::Schema, key, line_number(index)));
        }
    }
    discoveries
}

fn extract_proto(text: &str) -> Vec<Discovery> {
    let mut discoveries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        for (prefix, kind) in [
            ("message ", ArtifactKind::Schema),
            ("enum ", ArtifactKind::Schema),
            ("service ", ArtifactKind::Api),
            ("rpc ", ArtifactKind::Api),
        ] {
            if let Some(name) = trimmed.strip_prefix(prefix).and_then(identifier) {
                discoveries.push(Discovery {
                    kind,
                    name: name.to_owned(),
                    line: line_number(index),
                    source: trimmed.to_owned(),
                });
            }
        }
    }
    discoveries
}

fn discovery(kind: ArtifactKind, name: &str, line: u32) -> Discovery {
    Discovery {
        kind,
        name: name.to_owned(),
        line,
        source: name.to_owned(),
    }
}

fn line_number(index: usize) -> u32 {
    u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX)
}

fn validate_checkout_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("Repository checkout path is unsafe"));
    }
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect Repository checkout"))?;
    if metadata.file_type().is_symlink() {
        return Err(invalid("Repository checkout path must not be a symlink"));
    }
    let root = fs::canonicalize(path).map_err(io_error("canonicalize Repository checkout"))?;
    let top_level = git_text(&root, &["rev-parse", "--show-toplevel"], None)?;
    let top_level =
        fs::canonicalize(top_level.trim()).map_err(io_error("canonicalize Git worktree root"))?;
    if root != top_level {
        return Err(invalid(
            "Repository checkout path must be the worktree root",
        ));
    }
    Ok(root)
}

/// Tracked-state facts for one whole scan plan, gathered up front in batched local Git calls.
///
/// The two questions a scan asks of Git about a planned path -- is it tracked at all, and does the
/// working tree differ from `HEAD` -- used to cost one `git` process per path. On a large
/// Repository the dominant cost of either process is loading the index, which is the same work
/// whether it answers about one path or ten thousand, so a per-path spawn multiplied a fixed cost
/// by the size of the plan and made a budgeted scan of a real monorepo produce nothing at all.
/// Asking both questions once for the whole plan pays that fixed cost twice instead of `2n` times.
///
/// Pathspec arguments are batched under [`MAX_GIT_PATHSPEC_ARGV_BYTES`] so the argument list stays
/// inside every platform's limit; a plan large enough to need several batches still spawns a
/// bounded handful of processes rather than one per path.
#[derive(Debug, Default)]
struct TrackedPathFacts {
    tracked: BTreeSet<String>,
    modified: BTreeSet<String>,
}

impl TrackedPathFacts {
    fn collect(
        root: &Path,
        planned_paths: &[RepoRelativePath],
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let mut facts = Self::default();
        let requested = planned_paths
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        for batch in pathspec_batches(planned_paths) {
            let mut tracked_args = vec!["ls-files", "-z", "--"];
            tracked_args.extend(batch.iter().copied());
            facts
                .tracked
                .extend(git_nul_paths(root, &tracked_args, &requested, deadline)?);
            let mut modified_args = vec!["diff", "--name-only", "-z", "HEAD", "--"];
            modified_args.extend(batch.iter().copied());
            facts
                .modified
                .extend(git_nul_paths(root, &modified_args, &requested, deadline)?);
        }
        Ok(facts)
    }

    fn is_tracked(&self, relative: &str) -> bool {
        self.tracked.contains(relative)
    }

    fn source_state(&self, relative: &str) -> ArtifactSourceState {
        if self.modified.contains(relative) {
            ArtifactSourceState::TrackedWorkingModification
        } else {
            ArtifactSourceState::TrackedHead
        }
    }
}

/// Splits a plan into pathspec argument batches that stay under the argument-list ceiling.
fn pathspec_batches(planned_paths: &[RepoRelativePath]) -> Vec<Vec<&str>> {
    let mut batches = Vec::new();
    let mut batch: Vec<&str> = Vec::new();
    let mut bytes = 0_usize;
    for path in planned_paths {
        let value = path.as_str();
        if !batch.is_empty() && bytes.saturating_add(value.len() + 1) > MAX_GIT_PATHSPEC_ARGV_BYTES
        {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes = bytes.saturating_add(value.len() + 1);
        batch.push(value);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// Runs one batched Git query and keeps only the NUL-separated paths the plan actually asked for.
///
/// Git resolves each argument as a pathspec, so a batched query can report paths outside the plan
/// (a planned directory expands to its contents). Intersecting with `requested` keeps the batched
/// answer identical to what the per-path queries used to return.
fn git_nul_paths(
    root: &Path,
    args: &[&str],
    requested: &BTreeSet<String>,
    deadline: Option<Instant>,
) -> Result<Vec<String>> {
    let run = bounded_git(root, args, deadline, MAX_GIT_STDOUT_BYTES)?;
    if !matches!(run.code, Some(0)) {
        return Err(invalid(format!(
            "local Git snapshot command failed: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        )));
    }
    let text = String::from_utf8(run.stdout)
        .map_err(|error| invalid(format!("Git output is not UTF-8: {error}")))?;
    Ok(text
        .split('\0')
        .filter(|value| !value.is_empty())
        .filter(|value| requested.contains(*value))
        .map(ToOwned::to_owned)
        .collect())
}

fn git_text(root: &Path, args: &[&str], deadline: Option<Instant>) -> Result<String> {
    let run = bounded_git(root, args, deadline, MAX_GIT_STDERR_BYTES)?;
    if !matches!(run.code, Some(0)) {
        return Err(invalid(format!(
            "local Git snapshot command failed: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        )));
    }
    String::from_utf8(run.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|error| invalid(format!("Git output is not UTF-8: {error}")))
}

/// One completed local Git call, with both streams already capped.
struct BoundedGitRun {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs one local Git command under a wall-clock timeout and a captured-output ceiling.
///
/// A scan is a hot path that a stuck or pathologically chatty `git` must never be able to hang, so
/// both streams are drained by their own reader and truncated at a fixed ceiling, and a child that
/// outlives its timeout is killed and reaped rather than waited on.
fn bounded_git(
    root: &Path,
    args: &[&str],
    deadline: Option<Instant>,
    max_stdout_bytes: usize,
) -> Result<BoundedGitRun> {
    let timeout = git_call_timeout(deadline);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("--literal-pathspecs")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_error("run local Git snapshot command"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| invariant("local Git stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| invariant("local Git stderr was not captured"))?;
    let stdout_reader = std::thread::spawn(move || read_capped(stdout, max_stdout_bytes));
    let stderr_reader = std::thread::spawn(move || read_capped(stderr, MAX_GIT_STDERR_BYTES));
    let call_deadline = Instant::now().checked_add(timeout);
    let code = loop {
        if let Some(status) = child.try_wait().map_err(io_error("await local Git"))? {
            break status.code();
        }
        if call_deadline.is_some_and(|limit| Instant::now() >= limit) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid(
                "local Git snapshot command exceeded its wall-clock bound",
            ));
        }
        std::thread::sleep(GIT_POLL_INTERVAL);
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| invariant("local Git stdout reader panicked"))?
        .map_err(io_error("read local Git output"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| invariant("local Git stderr reader panicked"))?
        .map_or_else(|_| Vec::new(), |captured| captured.bytes);
    if stdout.capped {
        return Err(invalid(
            "local Git snapshot command produced more output than the scanner accepts",
        ));
    }
    Ok(BoundedGitRun {
        code,
        stdout: stdout.bytes,
        stderr,
    })
}

/// Derives one Git call's timeout from the scan budget it has to fit inside.
fn git_call_timeout(deadline: Option<Instant>) -> Duration {
    deadline.map_or(GIT_CALL_TIMEOUT, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .max(GIT_CALL_MIN_TIMEOUT)
            .min(GIT_CALL_TIMEOUT)
    })
}

/// One child stream drained to its end, plus whether the ceiling dropped anything.
struct CapturedStream {
    bytes: Vec<u8>,
    capped: bool,
}

/// Drains one child stream to its end, keeping at most `limit` bytes.
///
/// A short read is never treated as the end of the stream: an interrupted read is retried and any
/// other failure is reported. Half of `git ls-files` looks exactly like a complete answer in which
/// the missing paths are untracked, and a scan that quietly believed that would report Artifacts
/// as missing from a Repository that has them.
fn read_capped(mut stream: impl Read, limit: usize) -> std::io::Result<CapturedStream> {
    let mut kept = Vec::new();
    let mut capped = false;
    let mut chunk = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..read.min(room)]);
                capped = capped || read > room;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
            Err(error) => return Err(error),
        }
    }
    Ok(CapturedStream {
        bytes: kept,
        capped,
    })
}

fn language_for_path(path: &str) -> Option<SourceLanguage> {
    let lower = path.to_ascii_lowercase();
    if [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return Some(SourceLanguage::TypeScriptJavaScript);
    }
    [
        (".rs", SourceLanguage::Rust),
        (".swift", SourceLanguage::Swift),
        (".kt", SourceLanguage::Kotlin),
        (".kts", SourceLanguage::Kotlin),
        (".java", SourceLanguage::Java),
        (".json", SourceLanguage::Json),
        (".yaml", SourceLanguage::Yaml),
        (".yml", SourceLanguage::Yaml),
        (".proto", SourceLanguage::Proto),
        (".xml", SourceLanguage::Xml),
    ]
    .into_iter()
    .find_map(|(suffix, language)| lower.ends_with(suffix).then_some(language))
}

fn language_name(language: SourceLanguage) -> &'static str {
    match language {
        SourceLanguage::Rust => "rust",
        SourceLanguage::TypeScriptJavaScript => "typescript-javascript",
        SourceLanguage::Swift => "swift",
        SourceLanguage::Kotlin => "kotlin",
        SourceLanguage::Java => "java",
        SourceLanguage::Json => "json",
        SourceLanguage::Yaml => "yaml",
        SourceLanguage::Proto => "proto",
        SourceLanguage::Xml => "xml",
    }
}

fn is_ignored_path(path: &str) -> bool {
    path.split('/').any(|component| {
        matches!(
            component.to_ascii_lowercase().as_str(),
            ".git"
                | "target"
                | "node_modules"
                | "vendor"
                | "vendors"
                | "dist"
                | "build"
                | "pods"
                | "deriveddata"
                | ".env"
                | ".secrets"
                | "secrets"
                | ".credentials"
                | "credentials"
                | ".ssh"
                | ".aws"
        )
    })
}

fn is_generated_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains(".generated.")
        || lower.ends_with(".min.js")
        || lower.ends_with("package-lock.json")
        || lower.ends_with("pnpm-lock.yaml")
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8 * 1024).any(|byte| *byte == 0) || std::str::from_utf8(bytes).is_err()
}

fn module_name(path: &str) -> String {
    Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(
            || "root".to_owned(),
            |parent| parent.to_string_lossy().into_owned(),
        )
}

fn module_path(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy().into_owned())
}

fn file_stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("schema")
}

fn repo_path(value: &str) -> Result<RepoRelativePath> {
    RepoRelativePath::new(value)
}

fn file_version_digest(bytes: &[u8]) -> String {
    format!("version:{:x}", Sha256::digest(bytes))
}

fn normalize_signature(value: &str) -> String {
    value
        .lines()
        .next()
        .unwrap_or(value)
        .split(['{', '='])
        .next()
        .unwrap_or(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn api_protocol(source: &str) -> &'static str {
    if source.to_ascii_lowercase().contains("rpc ") {
        "grpc"
    } else {
        "http"
    }
}

fn api_operation(source: &str) -> &'static str {
    let lower = source.to_ascii_lowercase();
    for (needle, operation) in [
        ("post(", "POST"),
        ("put(", "PUT"),
        ("patch(", "PATCH"),
        ("delete(", "DELETE"),
        ("get(", "GET"),
        ("fetch", "GET"),
        ("rpc ", "RPC"),
    ] {
        if lower.contains(needle) {
            return operation;
        }
    }
    "ANY"
}

fn schema_version(source: &str) -> String {
    source
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '.')
        .find(|part| part.starts_with('v') && part[1..].chars().all(|value| value.is_ascii_digit()))
        .unwrap_or("unversioned")
        .to_owned()
}

fn snapshot_generation(
    repository: &RepositoryIdentity,
    head_tree_oid: &str,
    planned_paths: &[RepoRelativePath],
    coverage: &ScanCoverage,
    sources: &[SourceFile],
    skipped_files: &[SkippedFile],
) -> String {
    let mut hasher = Sha256::new();
    for component in [
        POLICY_VERSION.to_owned(),
        repository.repository_id.to_string(),
        head_tree_oid.to_owned(),
        coverage.digest_tag(),
    ] {
        hash_component(&mut hasher, &component);
    }
    for path in planned_paths {
        hash_component(&mut hasher, path.as_str());
    }
    if let ScanCoverage::Partial { covered, .. } = coverage {
        for path in covered {
            hash_component(&mut hasher, path.as_str());
        }
    }
    for source in sources {
        hash_component(&mut hasher, &source.path);
        hash_component(&mut hasher, &source.version_digest);
        hash_component(
            &mut hasher,
            match source.source_state {
                ArtifactSourceState::TrackedHead => "tracked_head",
                ArtifactSourceState::TrackedWorkingModification => "tracked_working_modification",
            },
        );
    }
    for skipped in skipped_files {
        hash_component(&mut hasher, &skipped.path);
        hash_component(&mut hasher, skipped_reason_name(skipped.reason));
    }
    format!("snap_{:x}", hasher.finalize())
}

const fn skipped_reason_name(reason: SkippedFileReason) -> &'static str {
    match reason {
        SkippedFileReason::Missing => "missing",
        SkippedFileReason::Untracked => "untracked",
        SkippedFileReason::IgnoredDirectory => "ignored_directory",
        SkippedFileReason::Generated => "generated",
        SkippedFileReason::UnsupportedLanguage => "unsupported_language",
        SkippedFileReason::Symlink => "symlink",
        SkippedFileReason::EscapesRepository => "escapes_repository",
        SkippedFileReason::Oversized => "oversized",
        SkippedFileReason::Binary => "binary",
        SkippedFileReason::FileLimit => "file_limit",
        SkippedFileReason::TotalByteLimit => "total_byte_limit",
    }
}

fn hash_component(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}
