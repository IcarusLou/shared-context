use std::collections::{BTreeMap, BTreeSet, HashSet};

use sctx_domain::{
    ArtifactAssociationKind, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactResolution,
    AutoInjectionBlocker, ConflictId, ContextArtifactAssociation, ContextGovernanceStatus,
    ContextId, ContextRelationKind, ContextRevision, DomainProjection, EngineeringArtifact,
    EngineeringReference, Error, ErrorKind, ReferenceId, ReferenceRelation, RepositoryId,
    ResolutionStatus, Result, RevisionId, RevisionLifecycle, SpaceId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{RepositoryScanOutcome, RepositorySnapshot, ScanCoverage, SnapshotArtifact};

const RESOLVER_POLICY_VERSION: &str = "historical-context-snapshot-resolution-v2";

/// Build-time lifecycle/governance state of one immutable Context revision.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphContextStatus {
    Candidate,
    Accepted,
    Deprecated,
    Superseded,
    GovernanceConflict,
}

/// Typed reason why one build-time Context snapshot cannot cross automatic injection.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphContextSafetyBlocker {
    NotAccepted,
    GovernanceConflict,
    UnresolvedSemanticConflict { conflict_id: ConflictId },
    IncompleteEvidence,
}

/// Immutable automatic-injection decision made when the Graph was built.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphContextSafety {
    pub automatic_injection_eligible: bool,
    pub blockers: BTreeSet<GraphContextSafetyBlocker>,
}

/// One `ContextRelation` with its build-time target revision fixed explicitly.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct GraphContextRelation {
    pub target_space_id: SpaceId,
    pub target_context_id: ContextId,
    pub target_revision_id: RevisionId,
    pub kind: ContextRelationKind,
    pub rationale: String,
    pub supports: Vec<String>,
}

/// Self-contained immutable Context content and safety decision captured by one Graph build.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphContextSnapshot {
    pub space_id: SpaceId,
    pub space_title: String,
    pub context_id: ContextId,
    pub revision: ContextRevision,
    pub status: GraphContextStatus,
    pub evidence_completeness: u16,
    pub safety: GraphContextSafety,
    pub relations: Vec<GraphContextRelation>,
}

impl GraphContextSnapshot {
    fn validate(&self) -> Result<()> {
        if self.space_title.trim().is_empty() {
            return Err(invariant("Graph Context snapshot Space title is empty"));
        }
        self.revision.validate()?;
        let completeness = context_evidence_completeness(&self.revision);
        if self.evidence_completeness != completeness {
            return Err(invariant(
                "Graph Context snapshot Evidence completeness is not derived from its Revision",
            ));
        }
        let expected_eligible = self.status == GraphContextStatus::Accepted
            && completeness == 1_000
            && !self.revision.evidence.is_empty()
            && self.safety.blockers.is_empty();
        if self.safety.automatic_injection_eligible != expected_eligible {
            return Err(invariant(
                "Graph Context snapshot safety decision is inconsistent",
            ));
        }
        if !expected_eligible && self.safety.blockers.is_empty() {
            return Err(invariant(
                "unsafe Graph Context snapshot requires a typed safety blocker",
            ));
        }
        if self.relations.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invariant(
                "Graph Context snapshot Relations are duplicated or unsorted",
            ));
        }
        Ok(())
    }
}

/// Captures immutable Context revisions, build-time safety, and relation target revisions.
///
/// # Errors
///
/// Returns an invariant error when a reduced projection has inconsistent ownership.
#[allow(clippy::too_many_lines)]
pub fn build_graph_context_snapshots(
    projection: &DomainProjection,
    references: &[ProjectedEngineeringReference],
) -> Result<Vec<GraphContextSnapshot>> {
    const MAX_RELATION_DEPTH: u8 = 2;
    let owners = projection
        .spaces
        .iter()
        .flat_map(|(space_id, space)| {
            space
                .contexts
                .values()
                .map(move |context| (context.context_id, (*space_id, space, context)))
        })
        .collect::<BTreeMap<_, _>>();
    let accepted_targets = projection
        .spaces
        .iter()
        .flat_map(|(space_id, space)| {
            space.contexts.values().filter_map(move |context| {
                let ContextGovernanceStatus::Accepted { revision_id, .. } = context.governance
                else {
                    return None;
                };
                Some((context.context_id, (*space_id, revision_id)))
            })
        })
        .collect::<BTreeMap<_, _>>();
    let mut depths = BTreeMap::<(SpaceId, ContextId, RevisionId), u8>::new();
    let mut roots = references
        .iter()
        .map(|reference| {
            let Some((space_id, _space, context)) = owners.get(&reference.context_id) else {
                return Err(invariant(
                    "projected Engineering Reference Context owner is missing",
                ));
            };
            if !context.revisions.contains_key(&reference.revision_id) {
                return Err(invariant(
                    "projected Engineering Reference Revision is missing from its Context",
                ));
            }
            Ok((*space_id, reference.context_id, reference.revision_id))
        })
        .collect::<Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    let mut queue = std::collections::VecDeque::from(roots);
    for root in &queue {
        depths.insert(*root, 0);
    }
    while let Some((space_id, context_id, revision_id)) = queue.pop_front() {
        let depth = depths[&(space_id, context_id, revision_id)];
        if depth >= MAX_RELATION_DEPTH {
            continue;
        }
        let (_owner_space_id, _space, context) = owners[&context_id];
        let revision = context
            .revisions
            .get(&revision_id)
            .ok_or_else(|| invariant("Graph relation source Revision disappeared during build"))?;
        if graph_context_status(context, revision.lifecycle) != GraphContextStatus::Accepted {
            continue;
        }
        for relation in &revision.revision.relations {
            let Some((target_space_id, target_revision_id)) =
                accepted_targets.get(&relation.target_context_id)
            else {
                continue;
            };
            let target = (
                *target_space_id,
                relation.target_context_id,
                *target_revision_id,
            );
            let next_depth = depth + 1;
            if depths
                .get(&target)
                .is_none_or(|existing| next_depth < *existing)
            {
                depths.insert(target, next_depth);
                queue.push_back(target);
            }
        }
    }
    let included = depths.keys().copied().collect::<BTreeSet<_>>();
    let mut snapshots = Vec::with_capacity(included.len());
    for ((space_id, context_id, revision_id), depth) in depths {
        let (_owner_space_id, space, context) = owners[&context_id];
        let revision = context
            .revisions
            .get(&revision_id)
            .ok_or_else(|| invariant("Graph Context Revision disappeared during snapshot build"))?;
        let status = graph_context_status(context, revision.lifecycle);
        let (evidence_completeness, safety) =
            graph_context_safety(context, &revision.revision, status);
        let relations = if status == GraphContextStatus::Accepted && depth < MAX_RELATION_DEPTH {
            revision
                .revision
                .relations
                .iter()
                .filter_map(|relation| {
                    let (target_space_id, target_revision_id) =
                        accepted_targets.get(&relation.target_context_id)?;
                    included
                        .contains(&(
                            *target_space_id,
                            relation.target_context_id,
                            *target_revision_id,
                        ))
                        .then(|| GraphContextRelation {
                            target_space_id: *target_space_id,
                            target_context_id: relation.target_context_id,
                            target_revision_id: *target_revision_id,
                            kind: relation.kind,
                            rationale: relation.rationale.clone(),
                            supports: relation.supports.clone(),
                        })
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        let snapshot = GraphContextSnapshot {
            space_id,
            space_title: graph_space_title(space)?,
            context_id,
            revision: revision.revision.clone(),
            status,
            evidence_completeness,
            safety,
            relations,
        };
        snapshot.validate()?;
        snapshots.push(snapshot);
    }
    snapshots.sort_by_key(|snapshot| (snapshot.context_id, snapshot.revision.revision_id));
    Ok(snapshots)
}

fn graph_context_safety(
    context: &sctx_domain::ContextProjection,
    revision: &ContextRevision,
    status: GraphContextStatus,
) -> (u16, GraphContextSafety) {
    let mut blockers = context
        .auto_injection
        .blockers
        .iter()
        .map(|blocker| match blocker {
            AutoInjectionBlocker::NotAccepted => GraphContextSafetyBlocker::NotAccepted,
            AutoInjectionBlocker::GovernanceConflict => {
                GraphContextSafetyBlocker::GovernanceConflict
            }
            AutoInjectionBlocker::UnresolvedSemanticConflict(conflict_id) => {
                GraphContextSafetyBlocker::UnresolvedSemanticConflict {
                    conflict_id: *conflict_id,
                }
            }
        })
        .collect::<BTreeSet<_>>();
    if status != GraphContextStatus::Accepted {
        blockers.insert(GraphContextSafetyBlocker::NotAccepted);
    }
    let evidence_completeness = context_evidence_completeness(revision);
    if evidence_completeness != 1_000 || revision.evidence.is_empty() {
        blockers.insert(GraphContextSafetyBlocker::IncompleteEvidence);
    }
    (
        evidence_completeness,
        GraphContextSafety {
            automatic_injection_eligible: status == GraphContextStatus::Accepted
                && blockers.is_empty(),
            blockers,
        },
    )
}

fn graph_space_title(space: &sctx_domain::ContextSpaceProjection) -> Result<String> {
    let titles = space
        .intent
        .heads
        .iter()
        .filter_map(|revision_id| space.intent.revisions.get(revision_id))
        .map(|revision| revision.intent.title.clone())
        .collect::<BTreeSet<_>>();
    if titles.is_empty() {
        return Err(invariant("Graph Context Space has no Intent title"));
    }
    Ok(titles.into_iter().collect::<Vec<_>>().join(" | "))
}

fn graph_context_status(
    context: &sctx_domain::ContextProjection,
    lifecycle: RevisionLifecycle,
) -> GraphContextStatus {
    if matches!(
        context.governance,
        ContextGovernanceStatus::GovernanceConflict { .. }
    ) {
        return GraphContextStatus::GovernanceConflict;
    }
    match lifecycle {
        RevisionLifecycle::Candidate => GraphContextStatus::Candidate,
        RevisionLifecycle::Accepted => GraphContextStatus::Accepted,
        RevisionLifecycle::Deprecated => GraphContextStatus::Deprecated,
        RevisionLifecycle::Superseded => GraphContextStatus::Superseded,
    }
}

fn context_evidence_completeness(revision: &ContextRevision) -> u16 {
    if revision.evidence.is_empty() {
        return 0;
    }
    let total = revision
        .evidence
        .iter()
        .map(|item| {
            usize::from(!item.supports.trim().is_empty()) * 250
                + usize::from(
                    item.content
                        .as_object()
                        .is_some_and(|content| !content.is_empty()),
                ) * 250
                + usize::from(!item.interpretation.trim().is_empty()) * 250
                + usize::from(!item.limitations.is_empty()) * 250
        })
        .sum::<usize>();
    u16::try_from(total / revision.evidence.len()).unwrap_or(u16::MAX)
}

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
    /// The locator this Reference names, kept whether or not anything resolved.
    ///
    /// A `Missing` resolution otherwise projects to a Repository id and a sentence: the Graph
    /// knows something stopped resolving and cannot say what it was pointing at, which is exactly
    /// the fact a reader needs to decide whether the break is worth a human's attention. Absent on
    /// rows written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<ArtifactLocator>,
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

/// One Repository scan in this generation that stopped before reading its whole plan.
///
/// The projection records the boundary rather than hiding it: References inside the unread range
/// resolve to `Unavailable`, and this row says which range that was, so a reader can tell an
/// Artifact that is genuinely gone from one the scan simply never reached.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IncompleteRepositoryScan {
    pub repository_id: RepositoryId,
    pub repository_generation: String,
    pub scanned_paths: usize,
    pub unfinished_paths: usize,
    /// Directory prefixes that still hold at least one unread planned path.
    pub unfinished_prefixes: Vec<String>,
}

/// Complete disposable Engineering projection for one resolver generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineeringProjection {
    pub policy_version: String,
    pub artifact_generation: String,
    pub contexts: Vec<GraphContextSnapshot>,
    pub references: Vec<ResolvedReferenceProjection>,
    /// Scans that ran out of budget mid-plan. Absent on projections written before this existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete_scans: Vec<IncompleteRepositoryScan>,
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
        let mut context_keys = BTreeMap::new();
        for context in &self.contexts {
            context.validate()?;
            if context_keys
                .insert(
                    (context.context_id, context.revision.revision_id),
                    context.space_id,
                )
                .is_some()
            {
                return Err(invariant(
                    "Engineering projection repeats a Context Revision snapshot",
                ));
            }
        }
        if self.contexts.windows(2).any(|pair| {
            (pair[0].context_id, pair[0].revision.revision_id)
                > (pair[1].context_id, pair[1].revision.revision_id)
        }) {
            return Err(invariant(
                "Engineering projection Context snapshots are not sorted",
            ));
        }
        for context in &self.contexts {
            for relation in &context.relations {
                if context_keys.get(&(relation.target_context_id, relation.target_revision_id))
                    != Some(&relation.target_space_id)
                {
                    return Err(invariant(
                        "Engineering projection Relation target snapshot is missing",
                    ));
                }
            }
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
            if !context_keys.contains_key(&(reference.context_id, reference.revision_id)) {
                return Err(invariant(
                    "resolved Reference lacks its immutable Context Revision snapshot",
                ));
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
        contexts: &[GraphContextSnapshot],
    ) -> Result<EngineeringProjection> {
        let mut contexts = contexts.to_vec();
        contexts.sort_by_key(|snapshot| (snapshot.context_id, snapshot.revision.revision_id));
        for context in &contexts {
            context.validate()?;
        }
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
        let generation = projection_generation(&ordered, &snapshots, &contexts, &resolved)?;
        for reference in &mut resolved {
            generation.clone_into(&mut reference.artifact_generation);
        }
        let projection = EngineeringProjection {
            policy_version: RESOLVER_POLICY_VERSION.to_owned(),
            artifact_generation: generation,
            contexts,
            references: resolved,
            incomplete_scans: incomplete_scans(&snapshots),
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
        contexts: &[GraphContextSnapshot],
    ) -> Result<EngineeringProjection> {
        previous.validate()?;
        self.resolve(references, snapshots, contexts)
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
            RepositoryScanOutcome::Available(snapshot) => (
                snapshot.repository_id.clone(),
                SnapshotState::Available(snapshot),
            ),
            RepositoryScanOutcome::Unavailable {
                repository_id,
                reason,
            } => (repository_id.clone(), SnapshotState::Unavailable(reason)),
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
                // A budgeted scan can stop before it reaches every planned path. Outside the range
                // it actually read, the snapshot holds no evidence either way, and "no Artifact
                // found" would be a statement about the budget rather than about the Repository.
                [] if !snapshot_covers(snapshot, &reference.locator) => projection_for_status(
                    projected,
                    Some(snapshot.generation.clone()),
                    ResolutionStatus::Unavailable,
                    None,
                    vec![],
                    vec![CandidateMatchEvidence {
                        artifact_key: None,
                        basis: MatchBasis::RepositoryUnavailable,
                        confidence: 0.0,
                        explanation: UNSCANNED_EXPLANATION.to_owned(),
                    }],
                    UNSCANNED_EXPLANATION.to_owned(),
                ),
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

/// Collects the scans in this generation that stopped before reading their whole plan.
fn incomplete_scans(
    snapshots: &BTreeMap<RepositoryId, SnapshotState<'_>>,
) -> Vec<IncompleteRepositoryScan> {
    snapshots
        .iter()
        .filter_map(|(repository_id, state)| {
            let SnapshotState::Available(snapshot) = state else {
                return None;
            };
            let ScanCoverage::Partial {
                covered,
                unfinished,
            } = &snapshot.coverage
            else {
                return None;
            };
            Some(IncompleteRepositoryScan {
                repository_id: repository_id.clone(),
                repository_generation: snapshot.generation.clone(),
                scanned_paths: covered.len(),
                unfinished_paths: unfinished.len(),
                unfinished_prefixes: snapshot.coverage.unfinished_prefixes(),
            })
        })
        .collect()
}

/// Sentence used whenever a Reference falls outside what a budgeted scan actually read.
const UNSCANNED_EXPLANATION: &str =
    "The Repository snapshot stopped before this path; the scan says nothing about it";

/// Reports whether one snapshot carries evidence about the range a locator names.
///
/// A Module locator names a directory and is answered by anything read beneath it; every other
/// locator names one file and is answered only by that file.
fn snapshot_covers(snapshot: &RepositorySnapshot, locator: &ArtifactLocator) -> bool {
    let path = locator.path().as_str();
    if matches!(locator, ArtifactLocator::Module { .. }) {
        snapshot.coverage.covers_directory(path)
    } else {
        snapshot.coverage.covers_path(path)
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
        locator: Some(projected.reference.locator.clone()),
        repository_generation,
        artifact_generation: String::new(),
        resolution: ArtifactResolution {
            reference_id: projected.reference.reference_id,
            repository_id: projected.reference.repository_id.clone(),
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
    contexts: &[GraphContextSnapshot],
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
    for context in contexts {
        hash_component(
            &mut hasher,
            &serde_json::to_string(context)
                .map_err(|error| invariant(format!("serialize Graph Context snapshot: {error}")))?,
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
