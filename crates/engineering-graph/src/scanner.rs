use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use sctx_domain::{
    ArtifactKey, ArtifactKeyBasis, ArtifactKind, ContentFingerprint, EngineeringArtifact, Error,
    ErrorKind, LocatorHints, RepositoryIdentity, Result, SemanticFingerprint,
};
use sha2::{Digest, Sha256};

const POLICY_VERSION: &str = "tracked-head-plus-safe-tracked-modifications-v1";

/// Exact source policy used for a `RepositorySnapshot`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotSourcePolicy {
    /// Enumerate Git tracked files; read safe current bytes for tracked modifications;
    /// exclude untracked and deleted files.
    TrackedHeadWithSafeTrackedModifications,
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
    Json,
    Yaml,
    Proto,
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
    Deleted,
    IgnoredDirectory,
    Generated,
    UnsupportedLanguage,
    Symlink,
    EscapesRepository,
    Oversized,
    Binary,
    FileLimit,
    TotalByteLimit,
    InvalidUtf8Path,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkippedFileReason,
}

/// Deterministic derived snapshot of one available Repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySnapshot {
    pub repository_id: sctx_domain::RepositoryId,
    pub source_policy: SnapshotSourcePolicy,
    pub policy_version: &'static str,
    pub head_tree_oid: String,
    pub generation: String,
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
    /// Returns typed validation, local Git, filesystem, or fingerprint errors.
    #[allow(clippy::too_many_lines)]
    pub fn scan(
        &self,
        repository: &RepositoryIdentity,
        checkout_path: &Path,
    ) -> Result<RepositoryScanOutcome> {
        repository.validate()?;
        if !checkout_path.exists() {
            return Ok(RepositoryScanOutcome::Unavailable {
                repository_id: repository.repository_id,
                reason: "Repository checkout is unavailable".to_owned(),
            });
        }
        let root = validate_checkout_root(checkout_path)?;
        let head_tree_oid = git_text(&root, &["rev-parse", "HEAD^{tree}"])?;
        let tracked = tracked_paths(&root)?;
        let modified = modified_paths(&root)?;
        let mut sources = Vec::new();
        let mut skipped_files = Vec::new();
        let mut scanned_bytes = 0_u64;
        for raw_path in tracked {
            let relative = match String::from_utf8(raw_path) {
                Ok(value) => value,
                Err(error) => {
                    skipped_files.push(SkippedFile {
                        path: String::from_utf8_lossy(error.as_bytes()).into_owned(),
                        reason: SkippedFileReason::InvalidUtf8Path,
                    });
                    continue;
                }
            };
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
                        reason: SkippedFileReason::Deleted,
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
            let source_state = if modified.contains(&relative) {
                ArtifactSourceState::TrackedWorkingModification
            } else {
                ArtifactSourceState::TrackedHead
            };
            sources.push(SourceFile {
                path: relative,
                language,
                source_state,
                content_fingerprint: content_fingerprint(&bytes)?,
                bytes,
            });
        }
        sources.sort_by(|left, right| left.path.cmp(&right.path));
        skipped_files.sort_by(|left, right| left.path.cmp(&right.path));
        let generation = snapshot_generation(repository, &head_tree_oid, &sources);
        let mut builder = ArtifactBuilder::new(repository, &generation);
        for source in &sources {
            builder.scan_source(source)?;
        }
        let artifacts = builder.finish();
        Ok(RepositoryScanOutcome::Available(RepositorySnapshot {
            repository_id: repository.repository_id,
            source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
            policy_version: POLICY_VERSION,
            head_tree_oid,
            generation,
            artifacts,
            scanned_files: sources.len(),
            scanned_bytes,
            skipped_files,
        }))
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
    ) -> Result<RepositoryScanOutcome> {
        let current = self.scan(repository, checkout_path)?;
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
    content_fingerprint: ContentFingerprint,
    bytes: Vec<u8>,
}

struct ArtifactBuilder<'a> {
    repository: &'a RepositoryIdentity,
    generation: &'a str,
    artifacts: BTreeMap<String, SnapshotArtifact>,
}

impl<'a> ArtifactBuilder<'a> {
    fn new(repository: &'a RepositoryIdentity, generation: &'a str) -> Self {
        Self {
            repository,
            generation,
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
                ArtifactKind::File,
                ArtifactKeyBasis::ContentFingerprint {
                    fingerprint: source.content_fingerprint.clone(),
                },
                &source.path,
                LocatorHints {
                    path: Some(source.path.clone()),
                    language: Some(language_name(source.language).to_owned()),
                    ..LocatorHints::default()
                },
                Some(source.content_fingerprint.clone()),
                Some(semantic_fingerprint(text)?),
            )?,
            observation(source, None),
        );
        self.insert(
            artifact(
                self.repository,
                ArtifactKind::Module,
                logical_basis("module", &module),
                &module,
                LocatorHints {
                    module: Some(module.clone()),
                    path: Some(module_path(&source.path)),
                    language: Some(language_name(source.language).to_owned()),
                    ..LocatorHints::default()
                },
                None,
                None,
            )?,
            observation(source, None),
        );
        for discovery in extract(source.language, &source.path, text) {
            let (namespace, locator) = match discovery.kind {
                ArtifactKind::Symbol | ArtifactKind::Test => (
                    module.as_str(),
                    LocatorHints {
                        module: Some(module.clone()),
                        path: Some(source.path.clone()),
                        symbol: Some(discovery.name.clone()),
                        language: Some(language_name(source.language).to_owned()),
                        line: Some(discovery.line),
                        ..LocatorHints::default()
                    },
                ),
                ArtifactKind::Api => (
                    "api",
                    LocatorHints {
                        path: Some(source.path.clone()),
                        language: Some(language_name(source.language).to_owned()),
                        api_or_schema: Some(discovery.name.clone()),
                        line: Some(discovery.line),
                        ..LocatorHints::default()
                    },
                ),
                ArtifactKind::Schema => (
                    "schema",
                    LocatorHints {
                        path: Some(source.path.clone()),
                        language: Some(language_name(source.language).to_owned()),
                        api_or_schema: Some(discovery.name.clone()),
                        line: Some(discovery.line),
                        ..LocatorHints::default()
                    },
                ),
                _ => continue,
            };
            self.insert(
                artifact(
                    self.repository,
                    discovery.kind,
                    logical_basis(namespace, &discovery.name),
                    &discovery.name,
                    locator,
                    Some(content_fingerprint(discovery.source.as_bytes())?),
                    Some(semantic_fingerprint(&discovery.semantic_source)?),
                )?,
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
                snapshot_generation: self.generation.to_owned(),
                source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
                observations: vec![observation],
            },
        );
    }

    fn finish(self) -> Vec<SnapshotArtifact> {
        self.artifacts.into_values().collect()
    }
}

fn artifact(
    repository: &RepositoryIdentity,
    kind: ArtifactKind,
    basis: ArtifactKeyBasis,
    display_name: &str,
    locator_hints: LocatorHints,
    content: Option<ContentFingerprint>,
    semantic: Option<SemanticFingerprint>,
) -> Result<EngineeringArtifact> {
    let artifact = EngineeringArtifact {
        repository: repository.clone(),
        artifact_key: ArtifactKey::derive(repository.repository_id, kind, basis)?,
        display_name: display_name.to_owned(),
        locator_hints,
        content_fingerprint: content,
        semantic_fingerprint: semantic,
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
    semantic_source: String,
}

fn extract(language: SourceLanguage, path: &str, text: &str) -> Vec<Discovery> {
    match language {
        SourceLanguage::Rust => extract_code(text, path, language, &rust_declaration),
        SourceLanguage::TypeScriptJavaScript => {
            extract_code(text, path, language, &typescript_declaration)
        }
        SourceLanguage::Swift => extract_code(text, path, language, &swift_declaration),
        SourceLanguage::Kotlin => extract_code(text, path, language, &kotlin_declaration),
        SourceLanguage::Json => extract_json(path, text),
        SourceLanguage::Yaml => extract_yaml(path, text),
        SourceLanguage::Proto => extract_proto(text),
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
            || matches!(language, SourceLanguage::Kotlin) && trimmed == "@Test"
        {
            test_annotation = true;
            continue;
        }
        for (kind, name) in parser(trimmed, test_annotation) {
            let source = declaration_source(&lines, index);
            discoveries.push(Discovery {
                kind,
                semantic_source: normalized_semantic(&source.replace(&name, "_symbol_")),
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
                semantic_source: format!("api:{api}"),
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
                semantic_source: format!("test:{name}"),
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
            discoveries.extend(
                paths
                    .keys()
                    .map(|name| discovery(ArtifactKind::Api, name, 1)),
            );
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
            discoveries.push(discovery(ArtifactKind::Api, key, line_number(index)));
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
                discoveries.push(discovery(kind, name, line_number(index)));
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
        semantic_source: format!("{}:{name}", kind_name(kind)),
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
    let top_level = git_text(&root, &["rev-parse", "--show-toplevel"])?;
    let top_level =
        fs::canonicalize(top_level.trim()).map_err(io_error("canonicalize Git worktree root"))?;
    if root != top_level {
        return Err(invalid(
            "Repository checkout path must be the worktree root",
        ));
    }
    Ok(root)
}

fn tracked_paths(root: &Path) -> Result<Vec<Vec<u8>>> {
    Ok(git_bytes(root, &["ls-files", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn modified_paths(root: &Path) -> Result<HashSet<String>> {
    Ok(
        git_bytes(root, &["diff", "--name-only", "-z", "HEAD", "--"])?
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
    )
}

fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(git_bytes(root, args)?)
        .map(|value| value.trim().to_owned())
        .map_err(|error| invalid(format!("Git output is not UTF-8: {error}")))
}

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(io_error("run local Git snapshot command"))?;
    if !output.status.success() {
        return Err(invalid(format!(
            "local Git snapshot command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
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
        (".json", SourceLanguage::Json),
        (".yaml", SourceLanguage::Yaml),
        (".yml", SourceLanguage::Yaml),
        (".proto", SourceLanguage::Proto),
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
        SourceLanguage::Json => "json",
        SourceLanguage::Yaml => "yaml",
        SourceLanguage::Proto => "proto",
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

fn module_path(path: &str) -> String {
    Path::new(path).parent().map_or_else(
        || ".".to_owned(),
        |parent| parent.to_string_lossy().into_owned(),
    )
}

fn file_stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("schema")
}

fn logical_basis(namespace: &str, logical_name: &str) -> ArtifactKeyBasis {
    ArtifactKeyBasis::Logical {
        namespace: Some(namespace.to_owned()),
        logical_name: logical_name.to_owned(),
    }
}

fn content_fingerprint(bytes: &[u8]) -> Result<ContentFingerprint> {
    ContentFingerprint::new(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn semantic_fingerprint(text: &str) -> Result<SemanticFingerprint> {
    SemanticFingerprint::new(format!(
        "semantic-v1:{:x}",
        Sha256::digest(normalized_semantic(text).as_bytes())
    ))
}

fn normalized_semantic(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("//")
                && !line.starts_with('#')
                && !line.starts_with("/*")
        })
        .flat_map(str::split_whitespace)
        .collect::<String>()
}

fn snapshot_generation(
    repository: &RepositoryIdentity,
    head_tree_oid: &str,
    sources: &[SourceFile],
) -> String {
    let mut hasher = Sha256::new();
    for component in [
        POLICY_VERSION.to_owned(),
        repository.repository_id.to_string(),
        head_tree_oid.to_owned(),
    ] {
        hash_component(&mut hasher, &component);
    }
    for source in sources {
        hash_component(&mut hasher, &source.path);
        hash_component(&mut hasher, source.content_fingerprint.as_str());
        hash_component(
            &mut hasher,
            match source.source_state {
                ArtifactSourceState::TrackedHead => "tracked_head",
                ArtifactSourceState::TrackedWorkingModification => "tracked_working_modification",
            },
        );
    }
    format!("snap_{:x}", hasher.finalize())
}

fn hash_component(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Repository => "repository",
        ArtifactKind::Module => "module",
        ArtifactKind::File => "file",
        ArtifactKind::Symbol => "symbol",
        ArtifactKind::Api => "api",
        ArtifactKind::Schema => "schema",
        ArtifactKind::Test => "test",
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}
