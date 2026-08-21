use std::collections::{BTreeMap, HashSet};

use sctx_domain::{
    ArtifactAssociationKind, ArtifactKey, ArtifactKeyBasis, ArtifactKind, ArtifactResolution,
    ContextArtifactAssociation, ContextId, EngineeringReference, Error, ErrorKind, ReferenceId,
    ReferenceRelation, RepositoryId, ResolutionStatus, Result, RevisionId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{RepositoryScanOutcome, RepositorySnapshot, SnapshotArtifact};

const RESOLVER_POLICY_VERSION: &str = "engineering-reference-resolution-v1";

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
    PathHint,
    ContentFingerprint,
    SemanticFingerprint,
    QualifiedSymbolLogicalKey,
    StableApiSchemaLogicalKey,
    PreviousResolvedArtifact,
    RepositoryUnavailable,
}

impl MatchBasis {
    const fn rank(self) -> u8 {
        match self {
            Self::PathHint => 1,
            Self::ContentFingerprint => 2,
            Self::SemanticFingerprint => 3,
            Self::QualifiedSymbolLogicalKey => 4,
            Self::StableApiSchemaLogicalKey => 5,
            Self::PreviousResolvedArtifact | Self::RepositoryUnavailable => 0,
        }
    }

    const fn confidence(self) -> f64 {
        match self {
            Self::StableApiSchemaLogicalKey => 1.0,
            Self::QualifiedSymbolLogicalKey => 0.95,
            Self::SemanticFingerprint => 0.90,
            Self::ContentFingerprint => 0.80,
            Self::PathHint => 0.40,
            Self::PreviousResolvedArtifact => 0.30,
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
        previous: Option<&EngineeringProjection>,
    ) -> Result<EngineeringProjection> {
        let snapshots = snapshot_map(snapshots)?;
        let previous = previous
            .map(|projection| {
                projection.validate().map(|()| {
                    projection
                        .references
                        .iter()
                        .map(|reference| (reference.reference_id, reference))
                        .collect::<BTreeMap<_, _>>()
                })
            })
            .transpose()?
            .unwrap_or_default();
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
                previous.get(&projected.reference.reference_id).copied(),
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
        self.resolve(references, snapshots, Some(previous))
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
    previous: Option<&ResolvedReferenceProjection>,
) -> Result<ResolvedReferenceProjection> {
    let reference = &projected.reference;
    match snapshot {
        Some(SnapshotState::Unavailable(reason)) => projection_for_status(
            projected,
            None,
            ResolutionStatus::Unavailable,
            None,
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
                [] => {
                    let previous_artifact = previous.and_then(|previous| {
                        matches!(
                            previous.resolution.status,
                            ResolutionStatus::Resolved | ResolutionStatus::Stale
                        )
                        .then(|| previous.resolution.resolved_artifact.clone())
                        .flatten()
                    });
                    if let Some(previous_artifact) = previous_artifact {
                        projection_for_status(
                            projected,
                            Some(snapshot.generation.clone()),
                            ResolutionStatus::Stale,
                            Some(previous_artifact.clone()),
                            vec![CandidateMatchEvidence {
                                artifact_key: Some(previous_artifact),
                                basis: MatchBasis::PreviousResolvedArtifact,
                                confidence: MatchBasis::PreviousResolvedArtifact.confidence(),
                                explanation:
                                    "previously resolved Artifact is absent from current snapshot"
                                        .to_owned(),
                            }],
                            "Previously resolved Artifact is stale in current Repository snapshot"
                                .to_owned(),
                        )
                    } else {
                        projection_for_status(
                            projected,
                            Some(snapshot.generation.clone()),
                            ResolutionStatus::Unresolved,
                            None,
                            vec![],
                            "No Artifact matched the persistent Reference".to_owned(),
                        )
                    }
                }
                [candidate] => projection_for_resolved(projected, snapshot, candidate),
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
        .filter_map(|artifact| {
            match_basis(reference, artifact).map(|basis| Candidate { artifact, basis })
        })
        .collect::<Vec<_>>();
    let Some(winning_rank) = candidates
        .iter()
        .map(|candidate| candidate.basis.rank())
        .max()
    else {
        return Vec::new();
    };
    candidates.retain(|candidate| candidate.basis.rank() == winning_rank);
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

fn match_basis(
    reference: &EngineeringReference,
    candidate: &SnapshotArtifact,
) -> Option<MatchBasis> {
    let artifact = &candidate.artifact;
    if matches!(
        reference.artifact_kind,
        ArtifactKind::Api | ArtifactKind::Schema
    ) && reference
        .locator_hints
        .as_ref()
        .and_then(|hints| hints.api_or_schema.as_deref())
        .zip(logical_name(&artifact.artifact_key))
        .is_some_and(|(expected, actual)| normalized(expected) == normalized(actual))
    {
        return Some(MatchBasis::StableApiSchemaLogicalKey);
    }
    if reference.artifact_kind == ArtifactKind::Symbol
        && qualified_symbol_matches(reference, artifact)
    {
        return Some(MatchBasis::QualifiedSymbolLogicalKey);
    }
    if reference
        .semantic_fingerprint
        .as_ref()
        .zip(artifact.semantic_fingerprint.as_ref())
        .is_some_and(|(expected, actual)| expected == actual)
    {
        return Some(MatchBasis::SemanticFingerprint);
    }
    if reference
        .content_fingerprint
        .as_ref()
        .zip(artifact.content_fingerprint.as_ref())
        .is_some_and(|(expected, actual)| expected == actual)
    {
        return Some(MatchBasis::ContentFingerprint);
    }
    if reference
        .locator_hints
        .as_ref()
        .and_then(|hints| hints.path.as_deref())
        .zip(artifact.locator_hints.path.as_deref())
        .is_some_and(|(expected, actual)| normalized_path(expected) == normalized_path(actual))
    {
        return Some(MatchBasis::PathHint);
    }
    None
}

fn qualified_symbol_matches(
    reference: &EngineeringReference,
    artifact: &sctx_domain::EngineeringArtifact,
) -> bool {
    let Some(hints) = &reference.locator_hints else {
        return false;
    };
    let Some(expected_symbol) = hints.symbol.as_deref() else {
        return false;
    };
    let Some(expected_module) = hints.module.as_deref() else {
        return false;
    };
    artifact
        .locator_hints
        .symbol
        .as_deref()
        .zip(artifact.locator_hints.module.as_deref())
        .is_some_and(|(symbol, module)| {
            normalized(symbol) == normalized(expected_symbol)
                && normalized_path(module) == normalized_path(expected_module)
        })
}

fn logical_name(key: &ArtifactKey) -> Option<&str> {
    match key.basis() {
        ArtifactKeyBasis::Logical { logical_name, .. } => Some(logical_name),
        ArtifactKeyBasis::ContentFingerprint { .. }
        | ArtifactKeyBasis::SemanticFingerprint { .. } => None,
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
        key.basis_explanation(),
        match basis {
            MatchBasis::StableApiSchemaLogicalKey => "stable API/Schema logical key",
            MatchBasis::QualifiedSymbolLogicalKey => "qualified Symbol logical key",
            MatchBasis::SemanticFingerprint => "semantic fingerprint",
            MatchBasis::ContentFingerprint => "content fingerprint",
            MatchBasis::PathHint => "low-weight path hint",
            MatchBasis::PreviousResolvedArtifact => "previous resolution",
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

fn normalized(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn normalized_path(value: &str) -> String {
    normalized(&value.replace('\\', "/"))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}
