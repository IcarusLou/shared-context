use std::collections::{BTreeMap, HashSet};

use sctx_domain::{
    ArtifactAssociationKind, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactResolution,
    ContextArtifactAssociation, ContextId, EngineeringArtifact, EngineeringReference, Error,
    ErrorKind, ReferenceId, ReferenceRelation, RepositoryId, ResolutionStatus, Result, RevisionId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{RepositoryScanOutcome, RepositorySnapshot, SnapshotArtifact};

const RESOLVER_POLICY_VERSION: &str = "deterministic-artifact-locator-resolution-v1";

/// Context ownership envelope around one Git-projected persistent Reference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectedEngineeringReference {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub reference: EngineeringReference,
}

impl ProjectedEngineeringReference {
    fn validate(&self) -> Result<()> {
        self.reference.validate()
    }
}

/// Typed evidence precedence used for a candidate match.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchBasis {
    ExactFilePath,
    ExactModulePath,
    ExactApiLocator,
    ExactSchemaLocator,
    ExactQualifiedSymbolLocator,
    ExactQualifiedTestLocator,
    RepositoryUnavailable,
}

impl MatchBasis {
    const fn confidence(self) -> f64 {
        match self {
            Self::ExactFilePath
            | Self::ExactModulePath
            | Self::ExactApiLocator
            | Self::ExactSchemaLocator
            | Self::ExactQualifiedSymbolLocator
            | Self::ExactQualifiedTestLocator => 1.0,
            Self::RepositoryUnavailable => 0.0,
        }
    }
}

/// Explainable evidence for one candidate at the winning precedence level.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateMatchEvidence {
    pub artifact_key: Option<ArtifactKey>,
    pub basis: MatchBasis,
    pub confidence: f64,
    pub explanation: String,
}

/// One Reference's derived resolution and optional Context association.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedReferenceProjection {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub reference_id: ReferenceId,
    pub repository_generation: Option<String>,
    pub artifact_generation: String,
    pub resolution: ArtifactResolution,
    pub association: Option<ContextArtifactAssociation>,
    pub artifacts: Vec<EngineeringArtifact>,
    pub evidence: Vec<CandidateMatchEvidence>,
}

impl ResolvedReferenceProjection {
    fn validate(&self) -> Result<()> {
        self.resolution.validate()?;
        if self.reference_id != self.resolution.reference_id {
            return Err(invariant(
                "resolved Reference identity differs from ArtifactResolution",
            ));
        }
        if let Some(association) = &self.association {
            association.validate()?;
            if association.context_id != self.context_id
                || association.revision_id != self.revision_id
                || self.resolution.resolved_artifact.as_ref() != Some(&association.artifact_key)
            {
                return Err(invariant(
                    "ContextArtifactAssociation differs from resolved Reference",
                ));
            }
        } else if self.resolution.status == ResolutionStatus::Resolved {
            return Err(invariant(
                "resolved Reference must produce ContextArtifactAssociation",
            ));
        }
        if self.evidence.iter().any(|evidence| {
            !evidence.confidence.is_finite()
                || evidence.confidence < 0.0
                || evidence.confidence > 1.0
        }) {
            return Err(invariant("resolution evidence confidence is invalid"));
        }
        let mut artifact_keys = HashSet::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !artifact_keys.insert(artifact.artifact_key.clone()) {
                return Err(invariant(
                    "resolved Engineering projection repeats an Artifact",
                ));
            }
        }
        let expected = match self.resolution.status {
            ResolutionStatus::Resolved => self
                .resolution
                .resolved_artifact
                .iter()
                .cloned()
                .collect::<HashSet<_>>(),
            ResolutionStatus::Ambiguous => self
                .resolution
                .candidates
                .iter()
                .cloned()
                .collect::<HashSet<_>>(),
            ResolutionStatus::Missing | ResolutionStatus::Unavailable => HashSet::new(),
        };
        if artifact_keys != expected {
            return Err(invariant(
                "resolved Engineering Artifacts differ from ArtifactResolution",
            ));
        }
        Ok(())
    }
}

/// Complete disposable Engineering projection for one resolver generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineeringProjection {
    pub policy_version: String,
    pub artifact_generation: String,
    pub references: Vec<ResolvedReferenceProjection>,
}

impl EngineeringProjection {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.policy_version != RESOLVER_POLICY_VERSION {
            return Err(invariant(
                "Engineering projection policy version is unsupported",
            ));
        }
        if self.artifact_generation.trim().is_empty() {
            return Err(invariant("Engineering projection generation is empty"));
        }
        let mut ids = HashSet::with_capacity(self.references.len());
        for reference in &self.references {
            reference.validate()?;
            if reference.artifact_generation != self.artifact_generation {
                return Err(invariant(
                    "resolved Reference carries a stale artifact generation",
                ));
            }
            if !ids.insert(reference.reference_id) {
                return Err(invariant("Engineering projection repeats a ReferenceId"));
            }
        }
        if self
            .references
            .windows(2)
            .any(|pair| pair[0].reference_id > pair[1].reference_id)
        {
            return Err(invariant(
                "Engineering projection References are not sorted",
            ));
        }
        Ok(())
    }
}

/// Deterministic Reference-to-Artifact resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct EngineeringReferenceResolver;

impl EngineeringReferenceResolver {
    /// Resolves References against current Repository snapshots.
    ///
    /// # Errors
    ///
    /// Returns typed validation errors for duplicate/mixed inputs or invalid
    /// derived associations.
    pub fn resolve(
        &self,
        references: &[ProjectedEngineeringReference],
        snapshots: &[RepositoryScanOutcome],
    ) -> Result<EngineeringProjection> {
        let snapshots = snapshot_map(snapshots)?;
        let mut ordered = references.to_vec();
        ordered.sort_by_key(|reference| reference.reference.reference_id);
        let mut seen = HashSet::with_capacity(ordered.len());
        let mut resolved = Vec::with_capacity(ordered.len());
        for projected in &ordered {
            projected.validate()?;
            if !seen.insert(projected.reference.reference_id) {
                return Err(invalid("projected References must not repeat ReferenceId"));
            }
            resolved.push(resolve_one(
                projected,
                snapshots.get(&projected.reference.repository_id),
            )?);
        }
        let generation = projection_generation(&ordered, &snapshots, &resolved)?;
        for reference in &mut resolved {
            generation.clone_into(&mut reference.artifact_generation);
        }
        let projection = EngineeringProjection {
            policy_version: RESOLVER_POLICY_VERSION.to_owned(),
            artifact_generation: generation,
            references: resolved,
        };
        projection.validate()?;
        Ok(projection)
    }

    /// Resolves incrementally with scratch-equivalent output.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::resolve`].
    pub fn resolve_incremental(
        &self,
        previous: &EngineeringProjection,
        references: &[ProjectedEngineeringReference],
        snapshots: &[RepositoryScanOutcome],
    ) -> Result<EngineeringProjection> {
        previous.validate()?;
        self.resolve(references, snapshots)
    }
}

enum SnapshotState<'a> {
    Available(&'a RepositorySnapshot),
    Unavailable(&'a str),
}

fn snapshot_map(
    snapshots: &[RepositoryScanOutcome],
) -> Result<BTreeMap<RepositoryId, SnapshotState<'_>>> {
    let mut map = BTreeMap::new();
    for snapshot in snapshots {
        let (repository_id, state) = match snapshot {
            RepositoryScanOutcome::Available(snapshot) => {
                (snapshot.repository_id, SnapshotState::Available(snapshot))
            }
            RepositoryScanOutcome::Unavailable {
                repository_id,
                reason,
            } => (*repository_id, SnapshotState::Unavailable(reason)),
        };
        if map.insert(repository_id, state).is_some() {
            return Err(invalid(
                "Repository scan outcomes must not repeat RepositoryId",
            ));
        }
    }
    Ok(map)
}

fn resolve_one(
    projected: &ProjectedEngineeringReference,
    snapshot: Option<&SnapshotState<'_>>,
) -> Result<ResolvedReferenceProjection> {
    let reference = &projected.reference;
    match snapshot {
        Some(SnapshotState::Unavailable(reason)) => projection_for_status(
            projected,
            None,
            ResolutionStatus::Unavailable,
            None,
            vec![],
            vec![CandidateMatchEvidence {
                artifact_key: None,
                basis: MatchBasis::RepositoryUnavailable,
                confidence: 0.0,
                explanation: format!("Repository unavailable: {reason}"),
            }],
            format!("Repository unavailable: {reason}"),
        ),
        None => projection_for_status(
            projected,
            None,
            ResolutionStatus::Unavailable,
            None,
            vec![],
            vec![CandidateMatchEvidence {
                artifact_key: None,
                basis: MatchBasis::RepositoryUnavailable,
                confidence: 0.0,
                explanation: "Repository snapshot unavailable".to_owned(),
            }],
            "Repository snapshot unavailable".to_owned(),
        ),
        Some(SnapshotState::Available(snapshot)) => {
            let candidates = winning_candidates(reference, &snapshot.artifacts);
            match candidates.as_slice() {
                [] => projection_for_status(
                    projected,
                    Some(snapshot.generation.clone()),
                    ResolutionStatus::Missing,
                    None,
                    vec![],
                    vec![],
                    "The deterministic Artifact locator is missing from the Repository snapshot"
                        .to_owned(),
                ),
                [candidate] if !candidate_is_ambiguous(candidate) => {
                    projection_for_resolved(projected, snapshot, candidate)
                }
                _ => projection_for_ambiguous(projected, snapshot, &candidates),
            }
        }
    }
}

fn winning_candidates<'a>(
    reference: &EngineeringReference,
    artifacts: &'a [SnapshotArtifact],
) -> Vec<Candidate<'a>> {
    let mut candidates = artifacts
        .iter()
        .filter(|artifact| {
            artifact.artifact.repository.repository_id == reference.repository_id
                && artifact.artifact.artifact_key.kind() == reference.artifact_kind
        })
        .filter(|artifact| artifact.artifact.artifact_key.locator() == &reference.locator)
        .map(|artifact| Candidate {
            artifact,
            basis: locator_match_basis(&reference.locator),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.artifact
            .artifact
            .artifact_key
            .digest()
            .cmp(right.artifact.artifact.artifact_key.digest())
    });
    candidates
}

struct Candidate<'a> {
    artifact: &'a SnapshotArtifact,
    basis: MatchBasis,
}

fn candidate_is_ambiguous(candidate: &Candidate<'_>) -> bool {
    !matches!(
        candidate.artifact.artifact.artifact_key.kind(),
        ArtifactKind::File | ArtifactKind::Module
    ) && candidate.artifact.observations.len() > 1
}

const fn locator_match_basis(locator: &ArtifactLocator) -> MatchBasis {
    match locator {
        ArtifactLocator::File { .. } => MatchBasis::ExactFilePath,
        ArtifactLocator::Module { .. } => MatchBasis::ExactModulePath,
        ArtifactLocator::Api { .. } => MatchBasis::ExactApiLocator,
        ArtifactLocator::Schema { .. } => MatchBasis::ExactSchemaLocator,
        ArtifactLocator::Symbol { .. } => MatchBasis::ExactQualifiedSymbolLocator,
        ArtifactLocator::Test { .. } => MatchBasis::ExactQualifiedTestLocator,
    }
}

fn projection_for_resolved(
    projected: &ProjectedEngineeringReference,
    snapshot: &RepositorySnapshot,
    candidate: &Candidate<'_>,
) -> Result<ResolvedReferenceProjection> {
    let key = candidate.artifact.artifact.artifact_key.clone();
    let confidence = candidate.basis.confidence();
    let evidence = CandidateMatchEvidence {
        artifact_key: Some(key.clone()),
        basis: candidate.basis,
        confidence,
        explanation: match_explanation(candidate.basis, &key),
    };
    let association = ContextArtifactAssociation {
        context_id: projected.context_id,
        revision_id: projected.revision_id,
        artifact_key: key.clone(),
        kind: association_kind(projected.reference.relation, key.kind()),
        source_reference_ids: vec![projected.reference.reference_id],
        confidence,
        explanation: evidence.explanation.clone(),
    };
    association.validate()?;
    projection_for_status(
        projected,
        Some(snapshot.generation.clone()),
        ResolutionStatus::Resolved,
        Some(key),
        vec![candidate.artifact.artifact.clone()],
        vec![evidence],
        "Reference resolved from highest-precedence stable evidence".to_owned(),
    )
    .map(|mut projection| {
        projection.association = Some(association);
        projection
    })
}

fn projection_for_ambiguous(
    projected: &ProjectedEngineeringReference,
    snapshot: &RepositorySnapshot,
    candidates: &[Candidate<'_>],
) -> Result<ResolvedReferenceProjection> {
    let evidence = candidates
        .iter()
        .map(|candidate| CandidateMatchEvidence {
            artifact_key: Some(candidate.artifact.artifact.artifact_key.clone()),
            basis: candidate.basis,
            confidence: candidate.basis.confidence(),
            explanation: match_explanation(
                candidate.basis,
                &candidate.artifact.artifact.artifact_key,
            ),
        })
        .collect::<Vec<_>>();
    projection_for_status(
        projected,
        Some(snapshot.generation.clone()),
        ResolutionStatus::Ambiguous,
        None,
        candidates
            .iter()
            .map(|candidate| candidate.artifact.artifact.clone())
            .collect(),
        evidence,
        "Multiple Artifacts share equal highest-precedence evidence; lower path/time hints were not used to choose"
            .to_owned(),
    )
}

fn projection_for_status(
    projected: &ProjectedEngineeringReference,
    repository_generation: Option<String>,
    status: ResolutionStatus,
    resolved_artifact: Option<ArtifactKey>,
    artifacts: Vec<EngineeringArtifact>,
    evidence: Vec<CandidateMatchEvidence>,
    explanation: String,
) -> Result<ResolvedReferenceProjection> {
    let candidates = if status == ResolutionStatus::Resolved {
        resolved_artifact.iter().cloned().collect()
    } else if status == ResolutionStatus::Ambiguous {
        evidence
            .iter()
            .filter_map(|evidence| evidence.artifact_key.clone())
            .collect()
    } else {
        Vec::new()
    };
    let projection = ResolvedReferenceProjection {
        context_id: projected.context_id,
        revision_id: projected.revision_id,
        reference_id: projected.reference.reference_id,
        repository_generation,
        artifact_generation: String::new(),
        resolution: ArtifactResolution {
            reference_id: projected.reference.reference_id,
            repository_id: projected.reference.repository_id,
            status,
            resolved_artifact,
            candidates,
            explanation,
        },
        association: None,
        artifacts,
        evidence,
    };
    projection.resolution.validate()?;
    Ok(projection)
}

fn association_kind(relation: ReferenceRelation, kind: ArtifactKind) -> ArtifactAssociationKind {
    match relation {
        ReferenceRelation::Defines if matches!(kind, ArtifactKind::Api | ArtifactKind::Schema) => {
            ArtifactAssociationKind::DefinesContract
        }
        ReferenceRelation::Implements | ReferenceRelation::Defines => {
            ArtifactAssociationKind::Implements
        }
        ReferenceRelation::Consumes | ReferenceRelation::DependsOn => ArtifactAssociationKind::Uses,
        ReferenceRelation::Validates => ArtifactAssociationKind::ValidatedBy,
        ReferenceRelation::Constrains => ArtifactAssociationKind::ConstrainedBy,
    }
}

fn match_explanation(basis: MatchBasis, key: &ArtifactKey) -> String {
    format!(
        "matched {} using {}",
        key.locator_explanation(),
        match basis {
            MatchBasis::ExactFilePath => "exact repository-relative File path",
            MatchBasis::ExactModulePath => "exact repository-relative Module path",
            MatchBasis::ExactApiLocator => "exact protocol, operation, and route",
            MatchBasis::ExactSchemaLocator => "exact namespace, version, and qualified Schema",
            MatchBasis::ExactQualifiedSymbolLocator => "exact qualified Symbol signature",
            MatchBasis::ExactQualifiedTestLocator => "exact qualified Test name",
            MatchBasis::RepositoryUnavailable => "repository availability",
        }
    )
}

fn projection_generation(
    references: &[ProjectedEngineeringReference],
    snapshots: &BTreeMap<RepositoryId, SnapshotState<'_>>,
    resolved: &[ResolvedReferenceProjection],
) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_component(&mut hasher, RESOLVER_POLICY_VERSION);
    for reference in references {
        hash_component(&mut hasher, &reference.reference.reference_id.to_string());
        hash_component(
            &mut hasher,
            &serde_json::to_string(reference)
                .map_err(|error| invariant(format!("serialize projected Reference: {error}")))?,
        );
    }
    for (repository_id, snapshot) in snapshots {
        hash_component(&mut hasher, &repository_id.to_string());
        match snapshot {
            SnapshotState::Available(snapshot) => hash_component(&mut hasher, &snapshot.generation),
            SnapshotState::Unavailable(reason) => hash_component(&mut hasher, reason),
        }
    }
    for reference in resolved {
        hash_component(&mut hasher, &reference.reference_id.to_string());
        hash_component(&mut hasher, &format!("{:?}", reference.resolution.status));
        for evidence in &reference.evidence {
            if let Some(artifact_key) = &evidence.artifact_key {
                hash_component(&mut hasher, artifact_key.digest());
            }
            hash_component(&mut hasher, &format!("{:?}", evidence.basis));
        }
    }
    Ok(format!("eng_{:x}", hasher.finalize()))
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
