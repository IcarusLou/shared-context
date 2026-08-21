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
    pub semantic_fingerprint: SemanticFingerprint,
}

impl RepositoryIdentity {
    /// Validates repository identity without consulting a local Workspace.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for empty names or fingerprints.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.canonical_name, "repository_identity.canonical_name")?;
        self.semantic_fingerprint.validate()
    }
}

macro_rules! fingerprint {
    ($name:ident, $field:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a validated opaque fingerprint value.
            ///
            /// # Errors
            ///
            /// Returns [`ErrorKind::InvalidInput`] when the value is empty.
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                require_text(&value, $field)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            fn validate(&self) -> Result<()> {
                require_text(&self.0, $field)
            }
        }
    };
}

fingerprint!(ContentFingerprint, "engineering content fingerprint");
fingerprint!(SemanticFingerprint, "engineering semantic fingerprint");

/// Supported engineering object categories.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Repository,
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
            Self::Repository => "repository",
            Self::Module => "module",
            Self::File => "file",
            Self::Symbol => "symbol",
            Self::Api => "api",
            Self::Schema => "schema",
            Self::Test => "test",
        }
    }
}

/// Stable basis used to derive an `ArtifactKey`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum ArtifactKeyBasis {
    Logical {
        namespace: Option<String>,
        logical_name: String,
    },
    ContentFingerprint {
        fingerprint: ContentFingerprint,
    },
    SemanticFingerprint {
        fingerprint: SemanticFingerprint,
    },
}

impl ArtifactKeyBasis {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Logical {
                namespace,
                logical_name,
            } => {
                validate_optional_text(namespace.as_ref(), "artifact_key.namespace")?;
                require_text(logical_name, "artifact_key.logical_name")
            }
            Self::ContentFingerprint { fingerprint } => fingerprint.validate(),
            Self::SemanticFingerprint { fingerprint } => fingerprint.validate(),
        }
    }

    fn canonical_components(&self) -> Vec<&str> {
        match self {
            Self::Logical {
                namespace,
                logical_name,
            } => vec!["logical", namespace.as_deref().unwrap_or(""), logical_name],
            Self::ContentFingerprint { fingerprint } => {
                vec!["content", fingerprint.as_str()]
            }
            Self::SemanticFingerprint { fingerprint } => {
                vec!["semantic", fingerprint.as_str()]
            }
        }
    }
}

/// Deterministic, rebuildable identity of one `EngineeringArtifact`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactKey {
    repository_id: RepositoryId,
    kind: ArtifactKind,
    basis: ArtifactKeyBasis,
    digest: String,
}

impl ArtifactKey {
    /// Derives a deterministic key from repository, Artifact kind, and stable basis.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an empty logical/fingerprint basis.
    pub fn derive(
        repository_id: RepositoryId,
        kind: ArtifactKind,
        basis: ArtifactKeyBasis,
    ) -> Result<Self> {
        basis.validate()?;
        let digest = artifact_digest(repository_id, kind, &basis);
        Ok(Self {
            repository_id,
            kind,
            basis,
            digest,
        })
    }

    #[must_use]
    pub const fn repository_id(&self) -> RepositoryId {
        self.repository_id
    }

    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.kind
    }

    #[must_use]
    pub const fn basis(&self) -> &ArtifactKeyBasis {
        &self.basis
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Explains the stable derivation basis without locator hints.
    #[must_use]
    pub fn basis_explanation(&self) -> String {
        match &self.basis {
            ArtifactKeyBasis::Logical {
                namespace,
                logical_name,
            } => format!(
                "logical identity {}{} in repository {} as {}",
                namespace
                    .as_deref()
                    .map_or_else(String::new, |value| format!("{value}::")),
                logical_name,
                self.repository_id,
                self.kind.stable_name()
            ),
            ArtifactKeyBasis::ContentFingerprint { fingerprint } => format!(
                "content fingerprint {} in repository {} as {}",
                fingerprint.as_str(),
                self.repository_id,
                self.kind.stable_name()
            ),
            ArtifactKeyBasis::SemanticFingerprint { fingerprint } => format!(
                "semantic fingerprint {} in repository {} as {}",
                fingerprint.as_str(),
                self.repository_id,
                self.kind.stable_name()
            ),
        }
    }

    /// Validates the stored digest against the complete stable basis.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid basis or mismatched digest.
    pub fn validate(&self) -> Result<()> {
        self.basis.validate()?;
        if self.digest != artifact_digest(self.repository_id, self.kind, &self.basis) {
            return Err(invalid(
                "artifact_key.digest must match repository, kind, and stable basis",
            ));
        }
        Ok(())
    }
}

fn artifact_digest(
    repository_id: RepositoryId,
    kind: ArtifactKind,
    basis: &ArtifactKeyBasis,
) -> String {
    let mut hasher = Sha256::new();
    for component in std::iter::once(repository_id.to_string())
        .chain(std::iter::once(kind.stable_name().to_owned()))
        .chain(
            basis
                .canonical_components()
                .into_iter()
                .map(ToOwned::to_owned),
        )
    {
        hasher.update(component.len().to_be_bytes());
        hasher.update(component.as_bytes());
    }
    format!("art_{:x}", hasher.finalize())
}

/// Non-authoritative coordinates observed for an engineering object.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocatorHints {
    pub module: Option<String>,
    pub path: Option<String>,
    pub symbol: Option<String>,
    pub language: Option<String>,
    pub api_or_schema: Option<String>,
    pub line: Option<u32>,
    pub commit: Option<String>,
}

impl LocatorHints {
    /// Validates that at least one meaningful locator is present.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for empty locators or a line without a path.
    pub fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.module.as_ref(), "locator_hints.module"),
            (self.path.as_ref(), "locator_hints.path"),
            (self.symbol.as_ref(), "locator_hints.symbol"),
            (self.language.as_ref(), "locator_hints.language"),
            (self.api_or_schema.as_ref(), "locator_hints.api_or_schema"),
            (self.commit.as_ref(), "locator_hints.commit"),
        ] {
            validate_optional_text(value, field)?;
        }
        if self.line.is_some() && self.path.is_none() {
            return Err(invalid("locator_hints.line requires locator_hints.path"));
        }
        if self.module.is_none()
            && self.path.is_none()
            && self.symbol.is_none()
            && self.api_or_schema.is_none()
        {
            return Err(invalid(
                "locator_hints must contain module, path, symbol, or api_or_schema",
            ));
        }
        Ok(())
    }
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
    pub locator_hints: Option<LocatorHints>,
    pub content_fingerprint: Option<ContentFingerprint>,
    pub semantic_fingerprint: Option<SemanticFingerprint>,
    pub supports: String,
    pub limitations: Vec<String>,
}

impl EngineeringReferenceDraft {
    /// Validates relation compatibility, locator/fingerprint evidence, and explanatory text.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid engineering observation.
    pub fn validate(&self) -> Result<()> {
        EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: self.repository_id,
            artifact_kind: self.artifact_kind,
            relation: self.relation,
            locator_hints: self.locator_hints.clone(),
            content_fingerprint: self.content_fingerprint.clone(),
            semantic_fingerprint: self.semantic_fingerprint.clone(),
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
    pub locator_hints: Option<LocatorHints>,
    pub content_fingerprint: Option<ContentFingerprint>,
    pub semantic_fingerprint: Option<SemanticFingerprint>,
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
            locator_hints: draft.locator_hints,
            content_fingerprint: draft.content_fingerprint,
            semantic_fingerprint: draft.semantic_fingerprint,
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
        if let Some(locator) = &self.locator_hints {
            locator.validate().map_err(|error| {
                invalid(format!(
                    "engineering_reference locator_hints are invalid: {error}"
                ))
            })?;
        }
        if let Some(fingerprint) = &self.content_fingerprint {
            fingerprint.validate()?;
        }
        if let Some(fingerprint) = &self.semantic_fingerprint {
            fingerprint.validate()?;
        }
        if self.locator_hints.is_none()
            && self.content_fingerprint.is_none()
            && self.semantic_fingerprint.is_none()
        {
            return Err(invalid(
                "engineering_reference requires locator hints or a fingerprint",
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
    pub locator_hints: LocatorHints,
    pub content_fingerprint: Option<ContentFingerprint>,
    pub semantic_fingerprint: Option<SemanticFingerprint>,
}

impl EngineeringArtifact {
    /// Validates repository ownership, `ArtifactKey` basis, and current locators.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for inconsistent derived Artifact state.
    pub fn validate(&self) -> Result<()> {
        self.repository.validate()?;
        self.artifact_key.validate()?;
        require_text(&self.display_name, "engineering_artifact.display_name")?;
        self.locator_hints.validate()?;
        if let Some(fingerprint) = &self.content_fingerprint {
            fingerprint.validate()?;
        }
        if let Some(fingerprint) = &self.semantic_fingerprint {
            fingerprint.validate()?;
        }
        if self.repository.repository_id != self.artifact_key.repository_id() {
            return Err(invalid(
                "engineering_artifact key and repository identity must match",
            ));
        }
        match self.artifact_key.basis() {
            ArtifactKeyBasis::ContentFingerprint { fingerprint }
                if self.content_fingerprint.as_ref() != Some(fingerprint) =>
            {
                return Err(invalid(
                    "engineering_artifact content fingerprint must match ArtifactKey basis",
                ));
            }
            ArtifactKeyBasis::SemanticFingerprint { fingerprint }
                if self.semantic_fingerprint.as_ref() != Some(fingerprint) =>
            {
                return Err(invalid(
                    "engineering_artifact semantic fingerprint must match ArtifactKey basis",
                ));
            }
            _ => {}
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
    Stale,
    Unavailable,
    Unresolved,
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
                self.resolved_artifact.is_none() && self.candidates.len() >= 2
            }
            ResolutionStatus::Stale => {
                self.resolved_artifact.is_some() && self.candidates.is_empty()
            }
            ResolutionStatus::Unavailable | ResolutionStatus::Unresolved => {
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
            Self::Uses => !matches!(kind, ArtifactKind::Repository),
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRelationKind {
    DependsOn,
    Constrains,
    Implements,
    Validates,
    ConflictsWith,
    Supersedes,
    RelatedTo,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRelation {
    pub source_context_id: ContextId,
    pub source_revision_id: RevisionId,
    pub target_context_id: ContextId,
    pub target_revision_id: RevisionId,
    pub kind: ContextRelationKind,
    pub rationale: String,
}

impl ContextRelation {
    /// Validates one stable Context edge without performing cycle detection.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for self-relations or empty rationale.
    pub fn validate(&self) -> Result<()> {
        if self.source_context_id == self.target_context_id {
            return Err(invalid("context_relation cannot target the same Context"));
        }
        require_text(&self.rationale, "context_relation.rationale")
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
                    && matches!(
                        self.source.kind(),
                        ArtifactKind::Repository | ArtifactKind::Module
                    )
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
            ArtifactRelationKind::Validates => {
                self.source.kind() == ArtifactKind::Test
                    && self.target.kind() != ArtifactKind::Repository
            }
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
            semantic_fingerprint: SemanticFingerprint::new(format!("repo:{name}")).unwrap(),
        }
    }

    fn locator(path: &str, symbol: Option<&str>, language: &str) -> LocatorHints {
        LocatorHints {
            path: Some(path.to_owned()),
            symbol: symbol.map(ToOwned::to_owned),
            language: Some(language.to_owned()),
            ..LocatorHints::default()
        }
    }

    fn logical(
        repository_id: RepositoryId,
        kind: ArtifactKind,
        namespace: &str,
        name: &str,
    ) -> ArtifactKey {
        ArtifactKey::derive(
            repository_id,
            kind,
            ArtifactKeyBasis::Logical {
                namespace: Some(namespace.to_owned()),
                logical_name: name.to_owned(),
            },
        )
        .unwrap()
    }

    #[test]
    fn file_move_and_symbol_rename_keep_fingerprint_derived_identity() {
        let repository = repository("search-web");
        let content = ContentFingerprint::new("sha256:file-content").unwrap();
        let file_key = ArtifactKey::derive(
            repository.repository_id,
            ArtifactKind::File,
            ArtifactKeyBasis::ContentFingerprint {
                fingerprint: content.clone(),
            },
        )
        .unwrap();
        let before = EngineeringArtifact {
            repository: repository.clone(),
            artifact_key: file_key.clone(),
            display_name: "old/search.ts".to_owned(),
            locator_hints: locator("old/search.ts", None, "typescript"),
            content_fingerprint: Some(content.clone()),
            semantic_fingerprint: None,
        };
        let after = EngineeringArtifact {
            locator_hints: locator("new/search.ts", None, "typescript"),
            display_name: "new/search.ts".to_owned(),
            ..before.clone()
        };
        assert!(before.validate().is_ok());
        assert!(after.validate().is_ok());
        assert_eq!(before.artifact_key, after.artifact_key);

        let semantic = SemanticFingerprint::new("ast:search-handler").unwrap();
        let symbol_key = ArtifactKey::derive(
            repository.repository_id,
            ArtifactKind::Symbol,
            ArtifactKeyBasis::SemanticFingerprint {
                fingerprint: semantic.clone(),
            },
        )
        .unwrap();
        let old_symbol = EngineeringArtifact {
            repository: repository.clone(),
            artifact_key: symbol_key,
            display_name: "oldSearch".to_owned(),
            locator_hints: locator("src/search.ts", Some("oldSearch"), "typescript"),
            content_fingerprint: None,
            semantic_fingerprint: Some(semantic),
        };
        let renamed = EngineeringArtifact {
            display_name: "newSearch".to_owned(),
            locator_hints: locator("src/search.ts", Some("newSearch"), "typescript"),
            ..old_symbol.clone()
        };
        assert!(old_symbol.validate().is_ok());
        assert!(renamed.validate().is_ok());
        assert_eq!(old_symbol.artifact_key, renamed.artifact_key);
    }

    #[test]
    fn logical_keys_distinguish_modules_but_unify_cross_language_contracts() {
        let repository = repository("cross-client");
        let first = logical(
            repository.repository_id,
            ArtifactKind::Symbol,
            "feed",
            "Result",
        );
        let second = logical(
            repository.repository_id,
            ArtifactKind::Symbol,
            "search",
            "Result",
        );
        assert_ne!(first, second);

        let api_from_swift = logical(
            repository.repository_id,
            ArtifactKind::Api,
            "search-v2",
            "SearchEndpoint",
        );
        let api_from_typescript = logical(
            repository.repository_id,
            ArtifactKind::Api,
            "search-v2",
            "SearchEndpoint",
        );
        assert_eq!(api_from_swift, api_from_typescript);
        assert!(api_from_swift.basis_explanation().contains("search-v2"));
    }

    #[test]
    fn artifact_key_serialization_has_no_locator_identity() {
        let key = logical(
            RepositoryId::new(),
            ArtifactKind::Api,
            "search-v2",
            "SearchEndpoint",
        );
        let value = serde_json::to_value(key).unwrap();
        let fields = value.as_object().unwrap();
        for forbidden in ["path", "line", "commit", "workspace"] {
            assert!(fields.keys().all(|field| !field.contains(forbidden)));
        }
    }

    #[test]
    fn references_reject_empty_evidence_and_invalid_relation_combinations() {
        assert!(ContentFingerprint::new(" ").is_err());
        assert!(SemanticFingerprint::new("").is_err());
        assert!(LocatorHints::default().validate().is_err());
        assert!(
            LocatorHints {
                line: Some(42),
                symbol: Some("Search".to_owned()),
                ..LocatorHints::default()
            }
            .validate()
            .is_err()
        );

        let invalid = EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: RepositoryId::new(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Consumes,
            locator_hints: Some(locator("src/search.ts", None, "typescript")),
            content_fingerprint: None,
            semantic_fingerprint: None,
            supports: "the implementation lives in this file".to_owned(),
            limitations: vec!["the file may move".to_owned()],
        };
        assert!(invalid.validate().is_err());

        let valid = EngineeringReference {
            artifact_kind: ArtifactKind::Api,
            locator_hints: None,
            semantic_fingerprint: Some(SemanticFingerprint::new("api:search-v2").unwrap()),
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
        let first = logical(repository, ArtifactKind::Symbol, "module-a", "Result");
        let second = logical(repository, ArtifactKind::Symbol, "module-b", "Result");
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
            candidates: vec![
                first,
                logical(
                    RepositoryId::new(),
                    ArtifactKind::Symbol,
                    "module-c",
                    "Result",
                ),
            ],
            ..ambiguous
        };
        assert!(mixed.validate().is_err());
    }

    #[test]
    fn derived_associations_validate_sources_confidence_and_artifact_kind() {
        let key = logical(
            RepositoryId::new(),
            ArtifactKind::Api,
            "search-v2",
            "SearchEndpoint",
        );
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
        let a = logical(repository, ArtifactKind::Module, "module", "A");
        let b = logical(repository, ArtifactKind::Module, "module", "B");
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
            source: logical(repository, ArtifactKind::Api, "api", "A"),
            target: logical(repository, ArtifactKind::File, "file", "B"),
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
            source_context_id: first,
            source_revision_id: RevisionId::new(),
            target_context_id: second,
            target_revision_id: RevisionId::new(),
            kind: ContextRelationKind::DependsOn,
            rationale: "first requires the second Contract".to_owned(),
        };
        let backward = ContextRelation {
            source_context_id: second,
            target_context_id: first,
            rationale: "second validates the first Decision".to_owned(),
            ..forward.clone()
        };
        assert!(forward.validate().is_ok());
        assert!(backward.validate().is_ok());

        let self_edge = ContextRelation {
            target_context_id: first,
            ..forward
        };
        assert!(self_edge.validate().is_err());
    }

    #[test]
    fn arbitrary_deserialized_fingerprint_still_fails_nested_validation() {
        let empty: ContentFingerprint =
            serde_json::from_value(Value::String(String::new())).unwrap();
        let reference = EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: RepositoryId::new(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator_hints: None,
            content_fingerprint: Some(empty),
            semantic_fingerprint: None,
            supports: "the content fingerprint locates the file".to_owned(),
            limitations: Vec::new(),
        };
        assert!(reference.validate().is_err());
    }
}
