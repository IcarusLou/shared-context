use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ContextId, Error, ErrorKind, ReferenceId, RepositoryId, Result, RevisionId};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn require_text(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_optional_text(value: Option<&String>, field: &str) -> Result<()> {
    if let Some(value) = value {
        require_text(value, field)?;
    }
    Ok(())
}

fn require_text_items(values: &[String], field: &str) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        require_text(value, &format!("{field}[{index}]"))?;
    }
    Ok(())
}

/// Stable identity of one logical repository, independent of a local checkout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIdentity {
    pub repository_id: RepositoryId,
    pub canonical_name: String,
}

impl RepositoryIdentity {
    /// Validates repository identity without consulting a local Workspace.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an empty canonical name.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.canonical_name, "repository_identity.canonical_name")
    }
}

/// Supported engineering object categories.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Module,
    File,
    Symbol,
    Api,
    Schema,
    Test,
}

impl ArtifactKind {
    const fn stable_name(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::File => "file",
            Self::Symbol => "symbol",
            Self::Api => "api",
            Self::Schema => "schema",
            Self::Test => "test",
        }
    }
}

/// Canonical repository-relative path used by every deterministic Artifact locator.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoRelativePath(String);

impl RepoRelativePath {
    /// Creates a canonical repository-relative path.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for absolute, empty, backslash, or
    /// dot-segment paths.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_repo_relative_path(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<()> {
        validate_repo_relative_path(&self.0)
    }
}

fn validate_repo_relative_path(value: &str) -> Result<()> {
    require_text(value, "artifact_locator.path")?;
    if value.starts_with('/') || value.contains('\\') {
        return Err(invalid(
            "artifact_locator.path must be a canonical repository-relative path",
        ));
    }
    if value
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(invalid(
            "artifact_locator.path must not contain empty or dot segments",
        ));
    }
    Ok(())
}

/// Kind-specific, deterministic coordinates for an Artifact inside one Repository.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "locator_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactLocator {
    File {
        path: RepoRelativePath,
    },
    Module {
        path: RepoRelativePath,
    },
    Api {
        path: RepoRelativePath,
        protocol: String,
        operation: String,
        normalized_route: String,
    },
    Schema {
        path: RepoRelativePath,
        namespace: String,
        version: String,
        qualified_name: String,
    },
    Symbol {
        path: RepoRelativePath,
        language: String,
        module: String,
        enclosing_type: Option<String>,
        symbol_name: String,
        signature: String,
    },
    Test {
        path: RepoRelativePath,
        qualified_test_name: String,
    },
}

impl ArtifactLocator {
    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        match self {
            Self::File { .. } => ArtifactKind::File,
            Self::Module { .. } => ArtifactKind::Module,
            Self::Api { .. } => ArtifactKind::Api,
            Self::Schema { .. } => ArtifactKind::Schema,
            Self::Symbol { .. } => ArtifactKind::Symbol,
            Self::Test { .. } => ArtifactKind::Test,
        }
    }

    #[must_use]
    pub const fn path(&self) -> &RepoRelativePath {
        match self {
            Self::File { path }
            | Self::Module { path }
            | Self::Api { path, .. }
            | Self::Schema { path, .. }
            | Self::Symbol { path, .. }
            | Self::Test { path, .. } => path,
        }
    }

    /// Validates every required coordinate for this Artifact kind.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid path or incomplete
    /// kind-specific coordinates.
    pub fn validate(&self) -> Result<()> {
        self.path().validate()?;
        match self {
            Self::File { .. } | Self::Module { .. } => Ok(()),
            Self::Api {
                protocol,
                operation,
                normalized_route,
                ..
            } => {
                require_text(protocol, "artifact_locator.protocol")?;
                require_text(operation, "artifact_locator.operation")?;
                require_text(normalized_route, "artifact_locator.normalized_route")?;
                if normalized_route.split_whitespace().count() != 1 {
                    return Err(invalid(
                        "artifact_locator.normalized_route must not contain whitespace",
                    ));
                }
                Ok(())
            }
            Self::Schema {
                namespace,
                version,
                qualified_name,
                ..
            } => {
                require_text(namespace, "artifact_locator.namespace")?;
                require_text(version, "artifact_locator.version")?;
                require_text(qualified_name, "artifact_locator.qualified_name")
            }
            Self::Symbol {
                language,
                module,
                enclosing_type,
                symbol_name,
                signature,
                ..
            } => {
                require_text(language, "artifact_locator.language")?;
                require_text(module, "artifact_locator.module")?;
                validate_optional_text(enclosing_type.as_ref(), "artifact_locator.enclosing_type")?;
                require_text(symbol_name, "artifact_locator.symbol_name")?;
                require_text(signature, "artifact_locator.signature")
            }
            Self::Test {
                qualified_test_name,
                ..
            } => require_text(qualified_test_name, "artifact_locator.qualified_test_name"),
        }
    }

    /// Stable human-readable key accepted by exact Task Signals.
    #[must_use]
    pub fn canonical_key(&self) -> String {
        match self {
            Self::File { path } | Self::Module { path } => path.as_str().to_owned(),
            Self::Api {
                path,
                protocol,
                operation,
                normalized_route,
            } => format!(
                "{}#{protocol}:{operation}:{normalized_route}",
                path.as_str()
            ),
            Self::Schema {
                path,
                namespace,
                version,
                qualified_name,
            } => format!("{}#{namespace}@{version}:{qualified_name}", path.as_str()),
            Self::Symbol {
                path,
                language,
                module,
                enclosing_type,
                symbol_name,
                signature,
            } => format!(
                "{}#{language}:{module}:{}:{symbol_name}:{signature}",
                path.as_str(),
                enclosing_type.as_deref().unwrap_or("")
            ),
            Self::Test {
                path,
                qualified_test_name,
            } => format!("{}#{qualified_test_name}", path.as_str()),
        }
    }
}

/// Transient Repository-scoped interpretation of one Artifact Focus query.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedFocus {
    pub repository_id: RepositoryId,
    pub locator: ArtifactLocator,
}

impl ResolvedFocus {
    /// Validates the complete kind-specific locator.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for incomplete or unsafe coordinates.
    pub fn validate(&self) -> Result<()> {
        self.locator.validate()
    }
}

/// Deterministic, rebuildable identity of one `EngineeringArtifact`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactKey {
    repository_id: RepositoryId,
    locator: ArtifactLocator,
    digest: String,
}

impl ArtifactKey {
    /// Derives a deterministic key from Repository identity and exact locator.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid locator.
    pub fn derive(repository_id: RepositoryId, locator: ArtifactLocator) -> Result<Self> {
        locator.validate()?;
        let digest = artifact_digest(&repository_id, &locator)?;
        Ok(Self {
            repository_id,
            locator,
            digest,
        })
    }

    #[must_use]
    pub fn repository_id(&self) -> RepositoryId {
        self.repository_id.clone()
    }

    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.locator.kind()
    }

    #[must_use]
    pub const fn locator(&self) -> &ArtifactLocator {
        &self.locator
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Explains the deterministic locator identity.
    #[must_use]
    pub fn locator_explanation(&self) -> String {
        format!(
            "exact {} locator {} in repository {}",
            self.kind().stable_name(),
            self.locator.path().as_str(),
            self.repository_id
        )
    }

    /// Validates the stored digest against the complete deterministic locator.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid locator or mismatched digest.
    pub fn validate(&self) -> Result<()> {
        self.locator.validate()?;
        if self.digest != artifact_digest(&self.repository_id, &self.locator)? {
            return Err(invalid(
                "artifact_key.digest must match repository and deterministic locator",
            ));
        }
        Ok(())
    }
}

fn artifact_digest(repository_id: &RepositoryId, locator: &ArtifactLocator) -> Result<String> {
    let mut hasher = Sha256::new();
    for component in [
        repository_id.to_string(),
        locator.kind().stable_name().to_owned(),
        serde_json::to_string(locator)
            .map_err(|error| invalid(format!("serialize Artifact locator: {error}")))?,
    ] {
        hasher.update(component.len().to_be_bytes());
        hasher.update(component.as_bytes());
    }
    Ok(format!("art_{:x}", hasher.finalize()))
}

/// How durable Context knowledge relates to a persistent `EngineeringReference`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceRelation {
    Implements,
    Defines,
    Consumes,
    Validates,
    Constrains,
    DependsOn,
}

impl ReferenceRelation {
    const fn supports(self, kind: ArtifactKind) -> bool {
        match self {
            Self::Implements => matches!(
                kind,
                ArtifactKind::Module | ArtifactKind::File | ArtifactKind::Symbol
            ),
            Self::Defines => matches!(
                kind,
                ArtifactKind::Symbol | ArtifactKind::Api | ArtifactKind::Schema
            ),
            Self::Consumes => matches!(kind, ArtifactKind::Api | ArtifactKind::Schema),
            Self::Validates => matches!(kind, ArtifactKind::Test),
            Self::Constrains => matches!(
                kind,
                ArtifactKind::Module | ArtifactKind::Api | ArtifactKind::Schema
            ),
            Self::DependsOn => true,
        }
    }
}

/// Caller-authored content for one persistent, non-authoritative engineering observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringReferenceDraft {
    pub repository_id: RepositoryId,
    pub artifact_kind: ArtifactKind,
    pub relation: ReferenceRelation,
    pub locator: ArtifactLocator,
    pub supports: String,
    pub limitations: Vec<String>,
}

impl EngineeringReferenceDraft {
    /// Validates relation compatibility, exact locator, and explanatory text.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid engineering observation.
    pub fn validate(&self) -> Result<()> {
        EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: self.repository_id.clone(),
            artifact_kind: self.artifact_kind,
            relation: self.relation,
            locator: self.locator.clone(),
            supports: self.supports.clone(),
            limitations: self.limitations.clone(),
        }
        .validate()
    }
}

/// Persistent, non-authoritative observation connecting Context to engineering coordinates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringReference {
    pub reference_id: ReferenceId,
    pub repository_id: RepositoryId,
    pub artifact_kind: ArtifactKind,
    pub relation: ReferenceRelation,
    pub locator: ArtifactLocator,
    pub supports: String,
    pub limitations: Vec<String>,
}

impl EngineeringReference {
    /// Generates a stable Reference identity for validated observation content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid draft.
    pub fn from_draft(draft: EngineeringReferenceDraft) -> Result<Self> {
        let reference = Self {
            reference_id: ReferenceId::new(),
            repository_id: draft.repository_id,
            artifact_kind: draft.artifact_kind,
            relation: draft.relation,
            locator: draft.locator,
            supports: draft.supports,
            limitations: draft.limitations,
        };
        reference.validate()?;
        Ok(reference)
    }

    /// Validates relation compatibility and observation evidence.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for incompatible or empty observations.
    pub fn validate(&self) -> Result<()> {
        if !self.relation.supports(self.artifact_kind) {
            return Err(invalid(
                "engineering_reference relation is incompatible with artifact_kind",
            ));
        }
        self.locator.validate().map_err(|error| {
            invalid(format!("engineering_reference locator is invalid: {error}"))
        })?;
        if self.locator.kind() != self.artifact_kind {
            return Err(invalid(
                "engineering_reference artifact_kind must match locator kind",
            ));
        }
        require_text(&self.supports, "engineering_reference.supports")?;
        require_text_items(&self.limitations, "engineering_reference.limitations")
    }
}

/// One currently discovered engineering object; it is rebuildable derived state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringArtifact {
    pub repository: RepositoryIdentity,
    pub artifact_key: ArtifactKey,
    pub display_name: String,
}

impl EngineeringArtifact {
    /// Validates repository ownership and deterministic Artifact identity.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for inconsistent derived Artifact state.
    pub fn validate(&self) -> Result<()> {
        self.repository.validate()?;
        self.artifact_key.validate()?;
        require_text(&self.display_name, "engineering_artifact.display_name")?;
        if self.repository.repository_id != self.artifact_key.repository_id() {
            return Err(invalid(
                "engineering_artifact key and repository identity must match",
            ));
        }
        Ok(())
    }
}

/// Current outcome of resolving a persistent reference against available code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Resolved,
    Ambiguous,
    Missing,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactResolution {
    pub reference_id: ReferenceId,
    pub repository_id: RepositoryId,
    pub status: ResolutionStatus,
    pub resolved_artifact: Option<ArtifactKey>,
    pub candidates: Vec<ArtifactKey>,
    pub explanation: String,
}

impl ArtifactResolution {
    /// Validates repository consistency and the selected `ResolutionStatus` shape.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for mixed repositories or invalid status fields.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.explanation, "artifact_resolution.explanation")?;
        let mut candidates = HashSet::with_capacity(self.candidates.len());
        for candidate in &self.candidates {
            candidate.validate()?;
            if candidate.repository_id() != self.repository_id {
                return Err(invalid(
                    "artifact_resolution candidates must share its repository identity",
                ));
            }
            if !candidates.insert(candidate) {
                return Err(invalid(
                    "artifact_resolution candidates must not contain duplicates",
                ));
            }
        }
        if let Some(artifact) = &self.resolved_artifact {
            artifact.validate()?;
            if artifact.repository_id() != self.repository_id {
                return Err(invalid(
                    "artifact_resolution resolved artifact must share its repository identity",
                ));
            }
        }
        let valid_shape = match self.status {
            ResolutionStatus::Resolved => {
                self.resolved_artifact.is_some()
                    && self.candidates.len() == 1
                    && self.resolved_artifact.as_ref() == self.candidates.first()
            }
            ResolutionStatus::Ambiguous => {
                self.resolved_artifact.is_none() && !self.candidates.is_empty()
            }
            ResolutionStatus::Missing | ResolutionStatus::Unavailable => {
                self.resolved_artifact.is_none() && self.candidates.is_empty()
            }
        };
        if !valid_shape {
            return Err(invalid(
                "artifact_resolution status is incompatible with artifact fields",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAssociationKind {
    Implements,
    DefinesContract,
    Uses,
    ValidatedBy,
    ConstrainedBy,
}

impl ArtifactAssociationKind {
    const fn supports(self, kind: ArtifactKind) -> bool {
        match self {
            Self::Implements => matches!(
                kind,
                ArtifactKind::Module | ArtifactKind::File | ArtifactKind::Symbol
            ),
            Self::DefinesContract => matches!(kind, ArtifactKind::Api | ArtifactKind::Schema),
            Self::Uses => true,
            Self::ValidatedBy => matches!(kind, ArtifactKind::Test),
            Self::ConstrainedBy => matches!(
                kind,
                ArtifactKind::Module | ArtifactKind::Api | ArtifactKind::Schema
            ),
        }
    }
}

/// Derived, rebuildable link between Context and one resolved Artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextArtifactAssociation {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub artifact_key: ArtifactKey,
    pub kind: ArtifactAssociationKind,
    pub source_reference_ids: Vec<ReferenceId>,
    pub confidence: f64,
    pub explanation: String,
}

impl ContextArtifactAssociation {
    /// Validates Artifact compatibility, confidence, sources, and explanation.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for unsupported or incomplete association state.
    pub fn validate(&self) -> Result<()> {
        self.artifact_key.validate()?;
        if !self.kind.supports(self.artifact_key.kind()) {
            return Err(invalid(
                "context_artifact_association kind is incompatible with ArtifactKind",
            ));
        }
        if !self.confidence.is_finite() || self.confidence <= 0.0 || self.confidence > 1.0 {
            return Err(invalid(
                "context_artifact_association confidence must be finite and in (0, 1]",
            ));
        }
        if self.source_reference_ids.is_empty() {
            return Err(invalid(
                "context_artifact_association requires at least one source ReferenceId",
            ));
        }
        let unique = self.source_reference_ids.iter().collect::<HashSet<_>>();
        if unique.len() != self.source_reference_ids.len() {
            return Err(invalid(
                "context_artifact_association source references must be unique",
            ));
        }
        require_text(
            &self.explanation,
            "context_artifact_association.explanation",
        )
    }
}

/// Stable knowledge edge between two Context revisions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRelationKind {
    DependsOn,
    Constrains,
    Implements,
    ValidatedBy,
    Contradicts,
    /// This revision replaces the target Context. The target keeps its accepted Git facts; only
    /// the local projection derives a `superseded_by` state from this edge.
    Supersedes,
    RelatedTo,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRelation {
    pub target_context_id: ContextId,
    pub kind: ContextRelationKind,
    pub rationale: String,
    pub supports: Vec<String>,
}

impl ContextRelation {
    /// Validates one stable Context edge without performing cycle detection.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for empty rationale or support statements.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.rationale, "context_relation.rationale")?;
        if self.supports.is_empty() {
            return Err(invalid(
                "context_relation.supports must contain at least one statement",
            ));
        }
        require_text_items(&self.supports, "context_relation.supports")?;
        if self.supports.iter().collect::<HashSet<_>>().len() != self.supports.len() {
            return Err(invalid(
                "context_relation.supports must not contain duplicates",
            ));
        }
        Ok(())
    }
}

/// Derived Engineering Graph edge kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRelationKind {
    Contains,
    Defines,
    Calls,
    Consumes,
    Produces,
    Validates,
    Implements,
    DependsOn,
}

/// One rebuildable edge between `EngineeringArtifacts`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringGraphEdge {
    pub source: ArtifactKey,
    pub target: ArtifactKey,
    pub kind: ArtifactRelationKind,
    pub explanation: String,
}

impl EngineeringGraphEdge {
    /// Validates one typed graph edge without performing cycle detection.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for self-edges or incompatible endpoint kinds.
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        self.target.validate()?;
        if self.source == self.target {
            return Err(invalid("engineering graph edge cannot be self-referential"));
        }
        let compatible = match self.kind {
            ArtifactRelationKind::Contains => {
                self.source.repository_id() == self.target.repository_id()
                    && self.source.kind() == ArtifactKind::Module
            }
            ArtifactRelationKind::Defines => {
                self.source.repository_id() == self.target.repository_id()
                    && matches!(
                        self.source.kind(),
                        ArtifactKind::Module | ArtifactKind::File
                    )
                    && matches!(
                        self.target.kind(),
                        ArtifactKind::Symbol | ArtifactKind::Api | ArtifactKind::Schema
                    )
            }
            ArtifactRelationKind::Calls => {
                self.source.kind() == ArtifactKind::Symbol
                    && matches!(self.target.kind(), ArtifactKind::Symbol | ArtifactKind::Api)
            }
            ArtifactRelationKind::Consumes => {
                matches!(
                    self.source.kind(),
                    ArtifactKind::Module | ArtifactKind::File | ArtifactKind::Symbol
                ) && matches!(self.target.kind(), ArtifactKind::Api | ArtifactKind::Schema)
            }
            ArtifactRelationKind::Produces => {
                matches!(
                    self.source.kind(),
                    ArtifactKind::Module | ArtifactKind::File | ArtifactKind::Symbol
                ) && matches!(self.target.kind(), ArtifactKind::Api | ArtifactKind::Schema)
            }
            ArtifactRelationKind::Validates => self.source.kind() == ArtifactKind::Test,
            ArtifactRelationKind::Implements => {
                matches!(
                    self.source.kind(),
                    ArtifactKind::Module | ArtifactKind::File | ArtifactKind::Symbol
                ) && matches!(self.target.kind(), ArtifactKind::Api | ArtifactKind::Schema)
            }
            ArtifactRelationKind::DependsOn => true,
        };
        if !compatible {
            return Err(invalid(
                "engineering graph edge kind is incompatible with source/target Artifacts",
            ));
        }
        require_text(&self.explanation, "engineering_graph_edge.explanation")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn repository(name: &str) -> RepositoryIdentity {
        RepositoryIdentity {
            repository_id: RepositoryId::new(),
            canonical_name: name.to_owned(),
        }
    }

    fn path(value: &str) -> RepoRelativePath {
        RepoRelativePath::new(value).unwrap()
    }

    fn module(repository_id: RepositoryId, value: &str) -> ArtifactKey {
        ArtifactKey::derive(repository_id, ArtifactLocator::Module { path: path(value) }).unwrap()
    }

    fn api(repository_id: RepositoryId, route: &str) -> ArtifactKey {
        ArtifactKey::derive(
            repository_id,
            ArtifactLocator::Api {
                path: path("api/search.yaml"),
                protocol: "http".to_owned(),
                operation: "GET".to_owned(),
                normalized_route: route.to_owned(),
            },
        )
        .unwrap()
    }

    fn symbol(repository_id: RepositoryId, module: &str, name: &str) -> ArtifactKey {
        ArtifactKey::derive(
            repository_id,
            ArtifactLocator::Symbol {
                path: path("src/search.ts"),
                language: "typescript".to_owned(),
                module: module.to_owned(),
                enclosing_type: None,
                symbol_name: name.to_owned(),
                signature: format!("{name}()"),
            },
        )
        .unwrap()
    }

    #[test]
    fn path_move_and_symbol_rename_change_deterministic_identity() {
        let repository = repository("search-web");
        let file_key = ArtifactKey::derive(
            repository.repository_id.clone(),
            ArtifactLocator::File {
                path: path("old/search.ts"),
            },
        )
        .unwrap();
        let before = EngineeringArtifact {
            repository: repository.clone(),
            artifact_key: file_key,
            display_name: "old/search.ts".to_owned(),
        };
        let after = EngineeringArtifact {
            artifact_key: ArtifactKey::derive(
                repository.repository_id.clone(),
                ArtifactLocator::File {
                    path: path("new/search.ts"),
                },
            )
            .unwrap(),
            display_name: "new/search.ts".to_owned(),
            ..before.clone()
        };
        assert!(before.validate().is_ok());
        assert!(after.validate().is_ok());
        assert_ne!(before.artifact_key, after.artifact_key);

        let old_symbol = EngineeringArtifact {
            repository: repository.clone(),
            artifact_key: symbol(repository.repository_id.clone(), "search", "oldSearch"),
            display_name: "oldSearch".to_owned(),
        };
        let renamed = EngineeringArtifact {
            artifact_key: symbol(repository.repository_id.clone(), "search", "newSearch"),
            display_name: "newSearch".to_owned(),
            ..old_symbol.clone()
        };
        assert!(old_symbol.validate().is_ok());
        assert!(renamed.validate().is_ok());
        assert_ne!(old_symbol.artifact_key, renamed.artifact_key);
    }

    #[test]
    fn locator_keys_distinguish_modules_and_normalize_contract_coordinates() {
        let repository = repository("cross-client");
        let first = symbol(repository.repository_id.clone(), "feed", "Result");
        let second = symbol(repository.repository_id.clone(), "search", "Result");
        assert_ne!(first, second);

        let api_from_swift = api(repository.repository_id.clone(), "/v2/search");
        let api_from_typescript = api(repository.repository_id.clone(), "/v2/search");
        assert_eq!(api_from_swift, api_from_typescript);
        assert!(
            api_from_swift
                .locator_explanation()
                .contains("api/search.yaml")
        );
    }

    #[test]
    fn artifact_key_serialization_exposes_only_deterministic_locator_identity() {
        let key = api(RepositoryId::new(), "/v2/search");
        let value = serde_json::to_value(key).unwrap();
        assert_eq!(value["locator"]["path"], "api/search.yaml");
        let encoded = value.to_string();
        assert!(!encoded.contains("content_"));
        assert!(!encoded.contains("semantic_"));
    }

    #[test]
    fn references_require_canonical_kind_specific_locator_and_compatible_relation() {
        assert!(RepoRelativePath::new("/absolute/search.ts").is_err());
        assert!(RepoRelativePath::new("src/../search.ts").is_err());

        let invalid = EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: RepositoryId::new(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Consumes,
            locator: ArtifactLocator::File {
                path: path("src/search.ts"),
            },
            supports: "the implementation lives in this file".to_owned(),
            limitations: vec!["the file may move".to_owned()],
        };
        assert!(invalid.validate().is_err());

        let valid = EngineeringReference {
            artifact_kind: ArtifactKind::Api,
            relation: ReferenceRelation::Consumes,
            locator: ArtifactLocator::Api {
                path: path("api/search.yaml"),
                protocol: "http".to_owned(),
                operation: "GET".to_owned(),
                normalized_route: "/v2/search".to_owned(),
            },
            ..invalid
        };
        assert!(valid.validate().is_ok());

        let mut invalid_support = valid;
        invalid_support.supports = " ".to_owned();
        assert!(invalid_support.validate().is_err());
    }

    #[test]
    fn resolution_models_ambiguity_unavailable_and_mixed_repository_rejection() {
        let repository = RepositoryId::new();
        let first = symbol(repository.clone(), "module-a", "Result");
        let second = symbol(repository.clone(), "module-b", "Result");
        let ambiguous = ArtifactResolution {
            reference_id: ReferenceId::new(),
            repository_id: repository,
            status: ResolutionStatus::Ambiguous,
            resolved_artifact: None,
            candidates: vec![first.clone(), second],
            explanation: "same symbol name exists in two modules".to_owned(),
        };
        assert!(ambiguous.validate().is_ok());

        let unavailable = ArtifactResolution {
            status: ResolutionStatus::Unavailable,
            candidates: vec![],
            explanation: "repository checkout is unavailable".to_owned(),
            ..ambiguous.clone()
        };
        assert!(unavailable.validate().is_ok());

        let mixed = ArtifactResolution {
            candidates: vec![first, symbol(RepositoryId::new(), "module-c", "Result")],
            ..ambiguous
        };
        assert!(mixed.validate().is_err());
    }

    #[test]
    fn derived_associations_validate_sources_confidence_and_artifact_kind() {
        let key = api(RepositoryId::new(), "/v2/search");
        let association = ContextArtifactAssociation {
            context_id: ContextId::new(),
            revision_id: RevisionId::new(),
            artifact_key: key,
            kind: ArtifactAssociationKind::DefinesContract,
            source_reference_ids: vec![ReferenceId::new()],
            confidence: 0.9,
            explanation: "the Contract reference resolved exactly".to_owned(),
        };
        assert!(association.validate().is_ok());

        let mut invalid = association;
        invalid.kind = ArtifactAssociationKind::ValidatedBy;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn graph_edges_allow_cycles_but_reject_invalid_typed_combinations() {
        let repository = RepositoryId::new();
        let a = module(repository.clone(), "src/a");
        let b = module(repository.clone(), "src/b");
        let forward = EngineeringGraphEdge {
            source: a.clone(),
            target: b.clone(),
            kind: ArtifactRelationKind::DependsOn,
            explanation: "A imports B".to_owned(),
        };
        let backward = EngineeringGraphEdge {
            source: b,
            target: a,
            kind: ArtifactRelationKind::DependsOn,
            explanation: "B calls back into A".to_owned(),
        };
        assert!(forward.validate().is_ok());
        assert!(backward.validate().is_ok());

        let invalid = EngineeringGraphEdge {
            source: api(repository.clone(), "/a"),
            target: ArtifactKey::derive(
                repository,
                ArtifactLocator::File {
                    path: path("src/b.rs"),
                },
            )
            .unwrap(),
            kind: ArtifactRelationKind::Calls,
            explanation: "invalid call direction".to_owned(),
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn stable_context_relations_allow_multi_node_cycles() {
        let first = ContextId::new();
        let second = ContextId::new();
        let forward = ContextRelation {
            target_context_id: second,
            kind: ContextRelationKind::DependsOn,
            rationale: "first requires the second Contract".to_owned(),
            supports: vec!["the first Context consumes the second contract".to_owned()],
        };
        let backward = ContextRelation {
            target_context_id: first,
            rationale: "second validates the first Decision".to_owned(),
            ..forward.clone()
        };
        assert!(forward.validate().is_ok());
        assert!(backward.validate().is_ok());

        let invalid = ContextRelation {
            supports: Vec::new(),
            ..forward
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn arbitrary_deserialized_locator_still_fails_nested_validation() {
        let invalid_path: RepoRelativePath =
            serde_json::from_value(Value::String("../outside.rs".to_owned())).unwrap();
        let reference = EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: RepositoryId::new(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator: ArtifactLocator::File { path: invalid_path },
            supports: "the exact path locates the file".to_owned(),
            limitations: Vec::new(),
        };
        assert!(reference.validate().is_err());
    }
}
