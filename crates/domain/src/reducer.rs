use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    Applicability, CandidateConfirmation, CandidateId, ConfirmationId, ConflictId,
    ConflictParticipant, ConflictResolution, ContextCandidate, ContextId, ContextKind,
    ContextRevision, ContextSpaceAssociation, ContextSpaceAssociationOrigin, EngineeringReference,
    EventId, EvidenceId, IntentRevision, Publication, PublicationAction, PublicationId,
    ReferenceId, ResolutionId, Review, ReviewId, ReviewVerdict, RevisionId, SemanticConflict,
    SpaceAssociationId, SpaceId, SubmissionId, WorkEpisodeRef, context_revision_as_draft,
    context_revision_content_hash,
};

/// Authoritative event input understood by the V1 domain reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReducerEvent {
    pub event_id: EventId,
    pub payload: ReducerPayload,
}

/// The V1 payloads in the pure domain projection boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReducerPayload {
    ContextCandidateCreated {
        candidate: ContextCandidate,
    },
    CandidateConfirmed {
        confirmation: Box<CandidateConfirmation>,
    },
    SpaceCreated {
        space_id: SpaceId,
        intent_revision: IntentRevision,
    },
    SpaceIntentRevisionAdded {
        space_id: SpaceId,
        intent_revision: IntentRevision,
    },
    ContextRevisionAdded {
        space_id: SpaceId,
        context_id: ContextId,
        revision: ContextRevision,
    },
    ContextReviewed {
        space_id: SpaceId,
        context_id: ContextId,
        review: Review,
    },
    ContextPublicationChanged {
        space_id: SpaceId,
        context_id: ContextId,
        publication: Publication,
    },
    ContextSpaceAssociationChanged {
        association: ContextSpaceAssociation,
    },
    SemanticConflictOpened {
        space_id: SpaceId,
        conflict: SemanticConflict,
    },
    SemanticConflictResolutionAdded {
        space_id: SpaceId,
        conflict_id: ConflictId,
        resolution: ConflictResolution,
    },
    EngineeringReferenceRecorded {
        context_id: ContextId,
        revision_id: RevisionId,
        reference: EngineeringReference,
    },
}

/// Stable reducer diagnostic classifications.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReducerDiagnosticCode {
    DuplicateEventId,
    DuplicateCandidateId,
    DuplicateSubmissionId,
    DuplicateConfirmationId,
    DuplicateSpaceAssociationId,
    CandidateConfirmationConflict,
    DuplicateRevisionId,
    DuplicateReviewId,
    DuplicatePublicationId,
    DuplicateEvidenceId,
    DuplicateConflictId,
    DuplicateResolutionId,
    DuplicateReferenceId,
    DuplicateSpaceCreation,
    AmbiguousContextOwner,
    MissingSpace,
    InvalidRevisionReference,
    InvalidContextRelation,
    InvalidContextRelationTarget,
    RevisionCycle,
    InvalidReviewReference,
    InvalidPublicationReference,
    PublicationCycle,
    InvalidSpaceAssociationReference,
    SpaceAssociationCycle,
    InvalidCandidateConfirmation,
    InvalidSemanticConflictReference,
    InvalidResolutionReference,
    ResolutionCycle,
    InvalidEngineeringReference,
    InvalidEngineeringReferenceTarget,
}

/// A deterministic explanation for quarantined input.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ReducerDiagnostic {
    pub code: ReducerDiagnosticCode,
    pub entity_id: String,
    pub event_ids: BTreeSet<EventId>,
    pub message: String,
}

/// Aggregated review result. Contradictory reviews remain visible as `Mixed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSummary {
    Unreviewed,
    Approved,
    Rejected,
    Mixed,
}

/// Publication-derived lifecycle of one complete Context revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionLifecycle {
    Candidate,
    Accepted,
    Deprecated,
    Superseded,
}

/// Governance state of one Context item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ContextGovernanceStatus {
    Unpublished,
    Accepted {
        publication_id: PublicationId,
        revision_id: RevisionId,
    },
    Deprecated {
        publication_id: PublicationId,
        revision_id: RevisionId,
    },
    GovernanceConflict {
        publication_ids: BTreeSet<PublicationId>,
    },
}

/// Stable reasons why a Context is excluded from automatic injection.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoInjectionBlocker {
    NotAccepted,
    GovernanceConflict,
    UnresolvedSemanticConflict(ConflictId),
}

/// Explicit eligibility projection; an empty blocker set is the only eligible state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AutoInjectionEligibility {
    pub eligible: bool,
    pub blockers: BTreeSet<AutoInjectionBlocker>,
}

/// Projection of one immutable Context revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RevisionProjection {
    pub revision: ContextRevision,
    pub is_head: bool,
    pub review_summary: ReviewSummary,
    pub review_event_ids: BTreeSet<EventId>,
    pub lifecycle: RevisionLifecycle,
}

/// Projection of one Context item and both of its independent DAG dimensions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextProjection {
    pub context_id: ContextId,
    pub revisions: BTreeMap<RevisionId, RevisionProjection>,
    pub revision_heads: BTreeSet<RevisionId>,
    pub reviews: BTreeMap<EventId, Review>,
    pub publications: BTreeMap<PublicationId, Publication>,
    pub publication_heads: BTreeSet<PublicationId>,
    pub governance: ContextGovernanceStatus,
    pub auto_injection: AutoInjectionEligibility,
}

/// Deterministic pair of accepted Contexts requiring human semantic review.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SemanticConflictCandidate {
    pub space_id: SpaceId,
    pub topic_key: String,
    pub participants: Vec<ConflictParticipant>,
}

/// Reasons a confirmed semantic conflict is still open.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticConflictOpenReason {
    NoResolution,
    ResolutionConflict,
    PublicationHeadsNotConverged,
    ResolutionDoesNotCoverCurrentPublicationHeads,
}

/// Current state of one confirmed semantic conflict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SemanticConflictStatus {
    Open {
        reasons: BTreeSet<SemanticConflictOpenReason>,
    },
    Resolved {
        resolution_id: ResolutionId,
    },
}

/// Confirmed conflict plus its independently reduced Resolution DAG.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SemanticConflictProjection {
    pub space_id: SpaceId,
    pub conflict: SemanticConflict,
    pub resolutions: BTreeMap<ResolutionId, ConflictResolution>,
    pub resolution_heads: BTreeSet<ResolutionId>,
    pub status: SemanticConflictStatus,
}

/// Intent DAG projection for a `ContextSpace`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IntentProjection {
    pub revisions: BTreeMap<RevisionId, IntentRevision>,
    pub heads: BTreeSet<RevisionId>,
}

/// Projection of one valid `ContextSpace`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSpaceProjection {
    pub space_id: SpaceId,
    pub intent: IntentProjection,
    pub contexts: BTreeMap<ContextId, ContextProjection>,
}

/// Projection of one unassigned Candidate creation event.
///
/// Candidate projections deliberately have no Space, publication, conflict, or
/// automatic-injection state. Confirmation is a later lifecycle operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CandidateProjection {
    pub event_id: EventId,
    pub candidate: ContextCandidate,
}

/// Unique valid Candidate submission used by the idempotency index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CandidateSubmissionProjection {
    pub submission_id: SubmissionId,
    pub candidate_id: CandidateId,
    pub event_id: EventId,
    pub source_episode: WorkEpisodeRef,
    pub content_hash: String,
}

/// Submission-local authoritative conflict; unrelated submissions remain usable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CandidateSubmissionConflict {
    pub submission_id: SubmissionId,
    pub event_ids: BTreeSet<EventId>,
    pub candidate_ids: BTreeSet<CandidateId>,
    pub content_hashes: BTreeSet<String>,
}

/// One valid Candidate confirmation fact and its defining Event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CandidateConfirmationProjection {
    pub event_id: EventId,
    pub confirmation: CandidateConfirmation,
}

/// Multiple Confirmation Events for one Candidate; no winner is selected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CandidateConfirmationConflict {
    pub candidate_id: CandidateId,
    pub confirmation_ids: BTreeSet<ConfirmationId>,
    pub event_ids: BTreeSet<EventId>,
}

/// One valid Context-to-Space association fact and its defining Event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSpaceAssociationProjection {
    pub event_id: EventId,
    pub association: ContextSpaceAssociation,
}

/// Multiple causal Association Heads for one Context; no winner is selected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSpaceAssociationConflict {
    pub context_id: ContextId,
    pub head_ids: BTreeSet<SpaceAssociationId>,
}

/// One valid persistent engineering observation and its resolved Context owner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EngineeringReferenceProjection {
    pub event_id: EventId,
    pub space_id: SpaceId,
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub reference: EngineeringReference,
}

/// Complete deterministic result of reducing one event multiset.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DomainProjection {
    pub candidates: BTreeMap<CandidateId, CandidateProjection>,
    pub candidate_submissions: BTreeMap<SubmissionId, CandidateSubmissionProjection>,
    pub candidate_submission_conflicts: BTreeMap<SubmissionId, CandidateSubmissionConflict>,
    pub candidate_confirmations: BTreeMap<ConfirmationId, CandidateConfirmationProjection>,
    pub candidate_confirmation_conflicts: BTreeMap<CandidateId, CandidateConfirmationConflict>,
    pub context_space_associations: BTreeMap<SpaceAssociationId, ContextSpaceAssociationProjection>,
    pub context_space_association_heads: BTreeMap<ContextId, BTreeSet<SpaceAssociationId>>,
    pub context_space_association_conflicts: BTreeMap<ContextId, ContextSpaceAssociationConflict>,
    pub spaces: BTreeMap<SpaceId, ContextSpaceProjection>,
    pub engineering_references: BTreeMap<ReferenceId, EngineeringReferenceProjection>,
    pub semantic_conflict_candidates: Vec<SemanticConflictCandidate>,
    pub semantic_conflicts: BTreeMap<ConflictId, SemanticConflictProjection>,
    pub quarantined_event_ids: BTreeSet<EventId>,
    pub diagnostics: Vec<ReducerDiagnostic>,
}

#[derive(Clone)]
struct IntentNode {
    event_id: EventId,
    space_id: SpaceId,
    revision: IntentRevision,
}

#[derive(Clone)]
struct CandidateNode {
    event_id: EventId,
    candidate: ContextCandidate,
}

#[derive(Clone)]
struct CandidateConfirmationNode {
    event_id: EventId,
    confirmation: CandidateConfirmation,
}

#[derive(Clone)]
struct ContextSpaceAssociationNode {
    event_id: EventId,
    association: ContextSpaceAssociation,
}

#[derive(Clone)]
struct ContextNode {
    event_id: EventId,
    space_id: SpaceId,
    context_id: ContextId,
    revision: ContextRevision,
}

#[derive(Clone)]
struct ReviewNode {
    event_id: EventId,
    space_id: SpaceId,
    context_id: ContextId,
    review: Review,
}

#[derive(Clone)]
struct PublicationNode {
    event_id: EventId,
    space_id: SpaceId,
    context_id: ContextId,
    publication: Publication,
}

#[derive(Clone)]
struct ConflictNode {
    event_id: EventId,
    space_id: SpaceId,
    conflict: SemanticConflict,
}

#[derive(Clone)]
struct ResolutionNode {
    event_id: EventId,
    space_id: SpaceId,
    conflict_id: ConflictId,
    resolution: ConflictResolution,
}

#[derive(Clone)]
struct EngineeringReferenceNode {
    event_id: EventId,
    context_id: ContextId,
    revision_id: RevisionId,
    reference: EngineeringReference,
}

struct DagNode<K> {
    event_id: EventId,
    parents: Vec<K>,
}

fn event_ids<T>(definitions: &[T], get: impl Fn(&T) -> EventId) -> BTreeSet<EventId> {
    definitions.iter().map(get).collect()
}

fn push_diagnostic(
    diagnostics: &mut BTreeSet<ReducerDiagnostic>,
    code: ReducerDiagnosticCode,
    entity_id: impl Into<String>,
    event_ids: BTreeSet<EventId>,
    message: impl Into<String>,
) {
    diagnostics.insert(ReducerDiagnostic {
        code,
        entity_id: entity_id.into(),
        event_ids,
        message: message.into(),
    });
}

fn validate_dag<K>(
    nodes: &BTreeMap<K, DagNode<K>>,
    invalid_reference_code: ReducerDiagnosticCode,
    cycle_code: ReducerDiagnosticCode,
    entity_name: &str,
    diagnostics: &mut BTreeSet<ReducerDiagnostic>,
) -> BTreeSet<K>
where
    K: Copy + Ord + ToString + std::fmt::Display,
{
    let mut invalid = BTreeSet::new();

    loop {
        let newly_invalid: Vec<_> = nodes
            .iter()
            .filter(|(id, _)| !invalid.contains(*id))
            .filter(|(_, node)| {
                node.parents
                    .iter()
                    .any(|parent| !nodes.contains_key(parent) || invalid.contains(parent))
            })
            .map(|(id, _)| *id)
            .collect();
        if newly_invalid.is_empty() {
            break;
        }
        for id in newly_invalid {
            invalid.insert(id);
            let node = &nodes[&id];
            push_diagnostic(
                diagnostics,
                invalid_reference_code,
                id.to_string(),
                BTreeSet::from([node.event_id]),
                format!("{entity_name} {id} has a missing, quarantined, or cross-aggregate parent"),
            );
        }
    }

    let mut valid = BTreeSet::new();
    loop {
        let ready: Vec<_> = nodes
            .iter()
            .filter(|(id, _)| !invalid.contains(*id) && !valid.contains(*id))
            .filter(|(_, node)| node.parents.iter().all(|parent| valid.contains(parent)))
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            break;
        }
        valid.extend(ready);
    }

    for (id, node) in nodes {
        if !invalid.contains(id) && !valid.contains(id) {
            push_diagnostic(
                diagnostics,
                cycle_code,
                id.to_string(),
                BTreeSet::from([node.event_id]),
                format!("{entity_name} {id} is cyclic or depends on a cycle"),
            );
        }
    }
    valid
}

fn heads<K>(nodes: &BTreeMap<K, DagNode<K>>, valid: &BTreeSet<K>) -> BTreeSet<K>
where
    K: Copy + Ord,
{
    let referenced: BTreeSet<_> = valid
        .iter()
        .flat_map(|id| nodes[id].parents.iter().copied())
        .collect();
    valid.difference(&referenced).copied().collect()
}

fn scope_dimension_overlaps(left: &[String], right: &[String]) -> bool {
    // An omitted dimension is intentionally unrestricted. Otherwise V1 only claims overlap
    // when the self-contained snapshots share an exact value; no mutable taxonomy or NLP is read.
    left.is_empty()
        || right.is_empty()
        || left
            .iter()
            .any(|left| right.iter().any(|right| left == right))
}

fn applicability_overlaps(left: &Applicability, right: &Applicability) -> bool {
    scope_dimension_overlaps(&left.domains, &right.domains)
        && scope_dimension_overlaps(&left.platforms, &right.platforms)
        && scope_dimension_overlaps(&left.conditions, &right.conditions)
}

fn semantic_candidate_participant(
    context: &ContextProjection,
) -> Option<(ConflictParticipant, &ContextRevision)> {
    let ContextGovernanceStatus::Accepted {
        publication_id,
        revision_id,
    } = context.governance
    else {
        return None;
    };
    let revision = &context.revisions.get(&revision_id)?.revision;
    if !matches!(revision.kind, ContextKind::Decision | ContextKind::Contract) {
        return None;
    }
    Some((
        ConflictParticipant {
            context_id: context.context_id,
            revision_id,
            publication_id,
        },
        revision,
    ))
}

fn conflict_candidates(
    spaces: &BTreeMap<SpaceId, ContextSpaceProjection>,
) -> Vec<SemanticConflictCandidate> {
    let mut candidates = BTreeSet::new();
    for (space_id, space) in spaces {
        let accepted: Vec<_> = space
            .contexts
            .values()
            .filter_map(semantic_candidate_participant)
            .collect();
        for (index, (left_participant, left_revision)) in accepted.iter().enumerate() {
            for (right_participant, right_revision) in accepted.iter().skip(index + 1) {
                let (Some(left_topic), Some(right_topic)) = (
                    left_revision.topic_key.as_ref(),
                    right_revision.topic_key.as_ref(),
                ) else {
                    continue;
                };
                if left_topic == right_topic
                    && applicability_overlaps(
                        &left_revision.applicability,
                        &right_revision.applicability,
                    )
                {
                    candidates.insert(SemanticConflictCandidate {
                        space_id: *space_id,
                        topic_key: left_topic.clone(),
                        participants: vec![left_participant.clone(), right_participant.clone()],
                    });
                }
            }
        }
    }
    candidates.into_iter().collect()
}

fn validate_context_relations(
    contexts: &mut BTreeMap<(SpaceId, ContextId), BTreeMap<RevisionId, ContextNode>>,
    context_heads: &mut BTreeMap<(SpaceId, ContextId), BTreeSet<RevisionId>>,
    diagnostics: &mut BTreeSet<ReducerDiagnostic>,
) {
    let mut invalid = BTreeSet::<(SpaceId, ContextId, RevisionId)>::new();
    loop {
        let existing_contexts = contexts
            .iter()
            .filter(|((space_id, context_id), revisions)| {
                revisions
                    .keys()
                    .any(|revision_id| !invalid.contains(&(*space_id, *context_id, *revision_id)))
            })
            .map(|((_, context_id), _)| *context_id)
            .collect::<BTreeSet<_>>();
        let mut newly_invalid = Vec::new();
        for ((space_id, context_id), revisions) in contexts.iter() {
            for (revision_id, node) in revisions {
                let key = (*space_id, *context_id, *revision_id);
                if invalid.contains(&key) {
                    continue;
                }
                let invalid_relation = node.revision.relations.iter().find_map(|relation| {
                    relation
                        .validate()
                        .err()
                        .map(|error| (ReducerDiagnosticCode::InvalidContextRelation, error.to_string()))
                        .or_else(|| {
                            (relation.target_context_id == *context_id
                                || !existing_contexts.contains(&relation.target_context_id))
                            .then(|| {
                                (
                                    ReducerDiagnosticCode::InvalidContextRelationTarget,
                                    format!(
                                        "target Context {} is self-referential, missing, or quarantined",
                                        relation.target_context_id
                                    ),
                                )
                            })
                        })
                });
                if let Some((code, reason)) = invalid_relation {
                    newly_invalid.push((key, node.event_id, code, reason));
                    continue;
                }
                if node
                    .revision
                    .parent_revision_ids
                    .iter()
                    .any(|parent| invalid.contains(&(*space_id, *context_id, *parent)))
                {
                    newly_invalid.push((
                        key,
                        node.event_id,
                        ReducerDiagnosticCode::InvalidRevisionReference,
                        "revision depends on a relation-quarantined parent".to_owned(),
                    ));
                }
            }
        }
        if newly_invalid.is_empty() {
            break;
        }
        for ((space_id, context_id, revision_id), event_id, code, reason) in newly_invalid {
            if invalid.insert((space_id, context_id, revision_id)) {
                push_diagnostic(
                    diagnostics,
                    code,
                    revision_id.to_string(),
                    BTreeSet::from([event_id]),
                    format!("context revision {revision_id} has an invalid relation: {reason}"),
                );
            }
        }
    }

    let keys = contexts.keys().copied().collect::<Vec<_>>();
    for key @ (space_id, context_id) in keys {
        let revisions = contexts.get_mut(&key).expect("Context key exists");
        revisions.retain(|revision_id, _| !invalid.contains(&(space_id, context_id, *revision_id)));
        if revisions.is_empty() {
            contexts.remove(&key);
            context_heads.remove(&key);
            continue;
        }
        let valid_ids = revisions.keys().copied().collect::<BTreeSet<_>>();
        let dag = revisions
            .iter()
            .map(|(revision_id, node)| {
                (
                    *revision_id,
                    DagNode {
                        event_id: node.event_id,
                        parents: node.revision.parent_revision_ids.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        context_heads.insert(key, heads(&dag, &valid_ids));
    }
}

/// Reduces a complete in-memory V1 event multiset without consulting storage or a clock.
///
/// Input order has no semantic effect. Duplicate definitions, ambiguous ownership,
/// dangling/cross-aggregate references, and cyclic nodes are quarantined as a set before
/// Heads and lifecycle states are derived.
///
/// # Panics
///
/// Panics only if an internal map assembled by this function violates its own key/reference
/// invariants; arbitrary malformed causal references are diagnosed and do not trigger a panic.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn reduce(events: &[ReducerEvent]) -> DomainProjection {
    let mut diagnostics = BTreeSet::new();
    let mut invalid_event_ids = BTreeSet::new();

    let mut events_by_id: BTreeMap<EventId, Vec<&ReducerEvent>> = BTreeMap::new();
    let mut candidate_definitions: BTreeMap<CandidateId, Vec<CandidateNode>> = BTreeMap::new();
    let mut submission_definitions: BTreeMap<SubmissionId, Vec<CandidateNode>> = BTreeMap::new();
    let mut confirmation_definitions: BTreeMap<ConfirmationId, Vec<CandidateConfirmationNode>> =
        BTreeMap::new();
    let mut confirmations_by_candidate: BTreeMap<CandidateId, Vec<CandidateConfirmationNode>> =
        BTreeMap::new();
    let mut association_definitions: BTreeMap<
        SpaceAssociationId,
        Vec<ContextSpaceAssociationNode>,
    > = BTreeMap::new();
    let mut space_creations: BTreeMap<SpaceId, Vec<IntentNode>> = BTreeMap::new();
    let mut intent_definitions: BTreeMap<RevisionId, Vec<IntentNode>> = BTreeMap::new();
    let mut context_definitions: BTreeMap<RevisionId, Vec<ContextNode>> = BTreeMap::new();
    let mut review_definitions: BTreeMap<ReviewId, Vec<ReviewNode>> = BTreeMap::new();
    let mut publication_definitions: BTreeMap<PublicationId, Vec<PublicationNode>> =
        BTreeMap::new();
    let mut conflict_definitions: BTreeMap<ConflictId, Vec<ConflictNode>> = BTreeMap::new();
    let mut resolution_definitions: BTreeMap<ResolutionId, Vec<ResolutionNode>> = BTreeMap::new();
    let mut reference_definitions: BTreeMap<ReferenceId, Vec<EngineeringReferenceNode>> =
        BTreeMap::new();
    let mut evidence_definitions: BTreeMap<EvidenceId, Vec<EventId>> = BTreeMap::new();
    let mut context_owners: BTreeMap<ContextId, BTreeSet<SpaceId>> = BTreeMap::new();

    for event in events {
        events_by_id.entry(event.event_id).or_default().push(event);
        match &event.payload {
            ReducerPayload::ContextCandidateCreated { candidate } => {
                let node = CandidateNode {
                    event_id: event.event_id,
                    candidate: candidate.clone(),
                };
                candidate_definitions
                    .entry(candidate.candidate_id)
                    .or_default()
                    .push(node.clone());
                submission_definitions
                    .entry(candidate.submission_id)
                    .or_default()
                    .push(node);
            }
            ReducerPayload::CandidateConfirmed { confirmation } => {
                let node = CandidateConfirmationNode {
                    event_id: event.event_id,
                    confirmation: confirmation.as_ref().clone(),
                };
                confirmation_definitions
                    .entry(confirmation.confirmation_id)
                    .or_default()
                    .push(node.clone());
                confirmations_by_candidate
                    .entry(confirmation.candidate_id)
                    .or_default()
                    .push(node);
            }
            ReducerPayload::SpaceCreated {
                space_id,
                intent_revision,
            } => {
                let node = IntentNode {
                    event_id: event.event_id,
                    space_id: *space_id,
                    revision: intent_revision.clone(),
                };
                space_creations
                    .entry(*space_id)
                    .or_default()
                    .push(node.clone());
                intent_definitions
                    .entry(intent_revision.revision_id)
                    .or_default()
                    .push(node);
            }
            ReducerPayload::SpaceIntentRevisionAdded {
                space_id,
                intent_revision,
            } => {
                intent_definitions
                    .entry(intent_revision.revision_id)
                    .or_default()
                    .push(IntentNode {
                        event_id: event.event_id,
                        space_id: *space_id,
                        revision: intent_revision.clone(),
                    });
            }
            ReducerPayload::ContextRevisionAdded {
                space_id,
                context_id,
                revision,
            } => {
                context_owners
                    .entry(*context_id)
                    .or_default()
                    .insert(*space_id);
                let node = ContextNode {
                    event_id: event.event_id,
                    space_id: *space_id,
                    context_id: *context_id,
                    revision: revision.clone(),
                };
                for evidence in &revision.evidence {
                    evidence_definitions
                        .entry(evidence.evidence_id)
                        .or_default()
                        .push(event.event_id);
                }
                context_definitions
                    .entry(revision.revision_id)
                    .or_default()
                    .push(node);
            }
            ReducerPayload::ContextReviewed {
                space_id,
                context_id,
                review,
            } => {
                context_owners
                    .entry(*context_id)
                    .or_default()
                    .insert(*space_id);
                review_definitions
                    .entry(review.review_id)
                    .or_default()
                    .push(ReviewNode {
                        event_id: event.event_id,
                        space_id: *space_id,
                        context_id: *context_id,
                        review: review.clone(),
                    });
            }
            ReducerPayload::ContextPublicationChanged {
                space_id,
                context_id,
                publication,
            } => {
                context_owners
                    .entry(*context_id)
                    .or_default()
                    .insert(*space_id);
                publication_definitions
                    .entry(publication.publication_id)
                    .or_default()
                    .push(PublicationNode {
                        event_id: event.event_id,
                        space_id: *space_id,
                        context_id: *context_id,
                        publication: publication.clone(),
                    });
            }
            ReducerPayload::ContextSpaceAssociationChanged { association } => {
                association_definitions
                    .entry(association.association_id)
                    .or_default()
                    .push(ContextSpaceAssociationNode {
                        event_id: event.event_id,
                        association: association.clone(),
                    });
            }
            ReducerPayload::SemanticConflictOpened { space_id, conflict } => {
                for participant in &conflict.participants {
                    context_owners
                        .entry(participant.context_id)
                        .or_default()
                        .insert(*space_id);
                }
                conflict_definitions
                    .entry(conflict.conflict_id)
                    .or_default()
                    .push(ConflictNode {
                        event_id: event.event_id,
                        space_id: *space_id,
                        conflict: conflict.clone(),
                    });
            }
            ReducerPayload::SemanticConflictResolutionAdded {
                space_id,
                conflict_id,
                resolution,
            } => {
                for result in &resolution.results {
                    context_owners
                        .entry(result.context_id)
                        .or_default()
                        .insert(*space_id);
                }
                resolution_definitions
                    .entry(resolution.resolution_id)
                    .or_default()
                    .push(ResolutionNode {
                        event_id: event.event_id,
                        space_id: *space_id,
                        conflict_id: *conflict_id,
                        resolution: resolution.clone(),
                    });
            }
            ReducerPayload::EngineeringReferenceRecorded {
                context_id,
                revision_id,
                reference,
            } => {
                reference_definitions
                    .entry(reference.reference_id)
                    .or_default()
                    .push(EngineeringReferenceNode {
                        event_id: event.event_id,
                        context_id: *context_id,
                        revision_id: *revision_id,
                        reference: reference.clone(),
                    });
            }
        }
    }

    for (id, definitions) in &events_by_id {
        if definitions.len() > 1 {
            invalid_event_ids.insert(*id);
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateEventId,
                id.to_string(),
                BTreeSet::from([*id]),
                format!("event ID {id} has multiple definitions"),
            );
        }
    }

    for (id, definitions) in &intent_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateRevisionId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("revision ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &candidate_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateCandidateId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("candidate ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &submission_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateSubmissionId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("submission ID {id} has multiple Candidate definitions"),
            );
        }
    }
    for (id, definitions) in &confirmation_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateConfirmationId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("confirmation ID {id} has multiple definitions"),
            );
        }
    }
    for (candidate_id, definitions) in &confirmations_by_candidate {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::CandidateConfirmationConflict,
                candidate_id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!(
                    "candidate {candidate_id} has multiple Confirmation Events; no result is selected"
                ),
            );
        }
    }
    for (id, definitions) in &association_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateSpaceAssociationId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("Space Association ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &context_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateRevisionId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("revision ID {id} has multiple definitions"),
            );
        }
    }
    for (id, intent_nodes) in &intent_definitions {
        if let Some(context_nodes) = context_definitions.get(id) {
            invalid_event_ids.extend(intent_nodes.iter().map(|node| node.event_id));
            invalid_event_ids.extend(context_nodes.iter().map(|node| node.event_id));
            let mut ids = event_ids(intent_nodes, |node| node.event_id);
            ids.extend(context_nodes.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateRevisionId,
                id.to_string(),
                ids,
                format!("revision ID {id} is defined by multiple aggregate kinds"),
            );
        }
    }
    for (id, definitions) in &review_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateReviewId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("review ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &publication_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicatePublicationId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("publication ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &evidence_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().copied());
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateEvidenceId,
                id.to_string(),
                definitions.iter().copied().collect(),
                format!("evidence ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &conflict_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateConflictId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("conflict ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &resolution_definitions {
        if definitions.len() > 1 {
            invalid_event_ids.extend(definitions.iter().map(|node| node.event_id));
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateResolutionId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("resolution ID {id} has multiple definitions"),
            );
        }
    }
    for (id, definitions) in &reference_definitions {
        if definitions.len() > 1 {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateReferenceId,
                id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("engineering reference ID {id} has multiple definitions"),
            );
        }
    }

    let mut invalid_spaces = BTreeSet::new();
    for (space_id, definitions) in &space_creations {
        if definitions.len() > 1 {
            invalid_spaces.insert(*space_id);
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::DuplicateSpaceCreation,
                space_id.to_string(),
                event_ids(definitions, |node| node.event_id),
                format!("space {space_id} has multiple creation events"),
            );
        }
    }

    let mut invalid_contexts = BTreeSet::new();
    for (context_id, owners) in &context_owners {
        if owners.len() > 1 {
            invalid_contexts.insert(*context_id);
            let ids = events
                .iter()
                .filter_map(|event| match &event.payload {
                    ReducerPayload::ContextRevisionAdded {
                        context_id: candidate,
                        ..
                    }
                    | ReducerPayload::ContextReviewed {
                        context_id: candidate,
                        ..
                    }
                    | ReducerPayload::ContextPublicationChanged {
                        context_id: candidate,
                        ..
                    } if candidate == context_id => Some(event.event_id),
                    ReducerPayload::SemanticConflictOpened { conflict, .. }
                        if conflict
                            .participants
                            .iter()
                            .any(|participant| participant.context_id == *context_id) =>
                    {
                        Some(event.event_id)
                    }
                    ReducerPayload::SemanticConflictResolutionAdded { resolution, .. }
                        if resolution
                            .results
                            .iter()
                            .any(|result| result.context_id == *context_id) =>
                    {
                        Some(event.event_id)
                    }
                    _ => None,
                })
                .collect();
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::AmbiguousContextOwner,
                context_id.to_string(),
                ids,
                format!("context {context_id} is referenced from multiple spaces"),
            );
        }
    }

    let valid_spaces: BTreeSet<_> = space_creations
        .iter()
        .filter(|(space_id, definitions)| {
            !invalid_spaces.contains(space_id)
                && definitions.len() == 1
                && !invalid_event_ids.contains(&definitions[0].event_id)
        })
        .map(|(space_id, _)| *space_id)
        .collect();

    for event in events {
        let referenced_spaces = match &event.payload {
            ReducerPayload::CandidateConfirmed { confirmation } => Some(
                std::iter::once(confirmation.primary_space_id)
                    .chain(confirmation.related_space_ids.iter().copied())
                    .chain(confirmation.created_space_id)
                    .collect::<BTreeSet<_>>(),
            ),
            ReducerPayload::ContextSpaceAssociationChanged { association } => Some(
                std::iter::once(association.primary_space_id)
                    .chain(association.related_space_ids.iter().copied())
                    .collect::<BTreeSet<_>>(),
            ),
            _ => None,
        };
        if let Some(referenced_spaces) = referenced_spaces {
            for space_id in referenced_spaces {
                if !valid_spaces.contains(&space_id) {
                    push_diagnostic(
                        &mut diagnostics,
                        ReducerDiagnosticCode::MissingSpace,
                        space_id.to_string(),
                        BTreeSet::from([event.event_id]),
                        format!("event references missing or quarantined space {space_id}"),
                    );
                }
            }
            continue;
        }
        let space_id = match &event.payload {
            ReducerPayload::ContextCandidateCreated { .. }
            | ReducerPayload::CandidateConfirmed { .. }
            | ReducerPayload::ContextSpaceAssociationChanged { .. }
            | ReducerPayload::EngineeringReferenceRecorded { .. } => continue,
            ReducerPayload::SpaceCreated { space_id, .. }
            | ReducerPayload::SpaceIntentRevisionAdded { space_id, .. }
            | ReducerPayload::ContextRevisionAdded { space_id, .. }
            | ReducerPayload::ContextReviewed { space_id, .. }
            | ReducerPayload::ContextPublicationChanged { space_id, .. }
            | ReducerPayload::SemanticConflictOpened { space_id, .. }
            | ReducerPayload::SemanticConflictResolutionAdded { space_id, .. } => *space_id,
        };
        let is_space_creation = matches!(event.payload, ReducerPayload::SpaceCreated { .. });
        if !valid_spaces.contains(&space_id) && !is_space_creation {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::MissingSpace,
                space_id.to_string(),
                BTreeSet::from([event.event_id]),
                format!("event references missing or quarantined space {space_id}"),
            );
        }
    }

    let mut valid_intents: BTreeMap<SpaceId, BTreeMap<RevisionId, IntentNode>> = BTreeMap::new();
    let mut intent_heads: BTreeMap<SpaceId, BTreeSet<RevisionId>> = BTreeMap::new();
    for space_id in &valid_spaces {
        let candidates: BTreeMap<_, _> = intent_definitions
            .iter()
            .filter(|(_, definitions)| definitions.len() == 1)
            .map(|(id, definitions)| (*id, definitions[0].clone()))
            .filter(|(_, node)| {
                node.space_id == *space_id && !invalid_event_ids.contains(&node.event_id)
            })
            .collect();
        let dag: BTreeMap<_, _> = candidates
            .iter()
            .map(|(id, node)| {
                (
                    *id,
                    DagNode {
                        event_id: node.event_id,
                        parents: node.revision.parent_revision_ids.clone(),
                    },
                )
            })
            .collect();
        let valid = validate_dag(
            &dag,
            ReducerDiagnosticCode::InvalidRevisionReference,
            ReducerDiagnosticCode::RevisionCycle,
            "intent revision",
            &mut diagnostics,
        );
        intent_heads.insert(*space_id, heads(&dag, &valid));
        valid_intents.insert(
            *space_id,
            candidates
                .into_iter()
                .filter(|(id, _)| valid.contains(id))
                .collect(),
        );
    }

    let mut valid_contexts: BTreeMap<(SpaceId, ContextId), BTreeMap<RevisionId, ContextNode>> =
        BTreeMap::new();
    let mut context_heads: BTreeMap<(SpaceId, ContextId), BTreeSet<RevisionId>> = BTreeMap::new();
    for (context_id, owners) in &context_owners {
        if invalid_contexts.contains(context_id) || owners.len() != 1 {
            continue;
        }
        let space_id = *owners.first().expect("single owner exists");
        if !valid_spaces.contains(&space_id) {
            continue;
        }
        let candidates: BTreeMap<_, _> = context_definitions
            .iter()
            .filter(|(_, definitions)| definitions.len() == 1)
            .map(|(id, definitions)| (*id, definitions[0].clone()))
            .filter(|(_, node)| {
                node.space_id == space_id
                    && node.context_id == *context_id
                    && !invalid_event_ids.contains(&node.event_id)
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let dag: BTreeMap<_, _> = candidates
            .iter()
            .map(|(id, node)| {
                (
                    *id,
                    DagNode {
                        event_id: node.event_id,
                        parents: node.revision.parent_revision_ids.clone(),
                    },
                )
            })
            .collect();
        let valid = validate_dag(
            &dag,
            ReducerDiagnosticCode::InvalidRevisionReference,
            ReducerDiagnosticCode::RevisionCycle,
            "context revision",
            &mut diagnostics,
        );
        if valid.is_empty() {
            continue;
        }
        let key = (space_id, *context_id);
        context_heads.insert(key, heads(&dag, &valid));
        valid_contexts.insert(
            key,
            candidates
                .into_iter()
                .filter(|(id, _)| valid.contains(id))
                .collect(),
        );
    }

    validate_context_relations(&mut valid_contexts, &mut context_heads, &mut diagnostics);

    let mut valid_reviews: BTreeMap<(SpaceId, ContextId), BTreeMap<EventId, ReviewNode>> =
        BTreeMap::new();
    for definitions in review_definitions.values().filter(|items| items.len() == 1) {
        let node = definitions[0].clone();
        let key = (node.space_id, node.context_id);
        let target_is_valid = valid_contexts
            .get(&key)
            .is_some_and(|revisions| revisions.contains_key(&node.review.revision_id));
        if invalid_event_ids.contains(&node.event_id) || !target_is_valid {
            if !invalid_event_ids.contains(&node.event_id) {
                push_diagnostic(
                    &mut diagnostics,
                    ReducerDiagnosticCode::InvalidReviewReference,
                    node.review.review_id.to_string(),
                    BTreeSet::from([node.event_id]),
                    format!(
                        "review {} does not reference a valid revision in its context",
                        node.review.review_id
                    ),
                );
            }
            continue;
        }
        valid_reviews
            .entry(key)
            .or_default()
            .insert(node.event_id, node);
    }

    let review_by_event: BTreeMap<_, _> = valid_reviews
        .values()
        .flat_map(|reviews| reviews.iter().map(|(id, node)| (*id, node.clone())))
        .collect();

    let mut valid_publications: BTreeMap<
        (SpaceId, ContextId),
        BTreeMap<PublicationId, PublicationNode>,
    > = BTreeMap::new();
    let mut publication_heads: BTreeMap<(SpaceId, ContextId), BTreeSet<PublicationId>> =
        BTreeMap::new();
    for definitions in publication_definitions
        .values()
        .filter(|items| items.len() == 1)
    {
        let node = &definitions[0];
        if !invalid_event_ids.contains(&node.event_id)
            && !valid_contexts.contains_key(&(node.space_id, node.context_id))
        {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidPublicationReference,
                node.publication.publication_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!(
                    "publication {} belongs to a missing or quarantined context",
                    node.publication.publication_id
                ),
            );
        }
    }
    for key in valid_contexts.keys() {
        let candidates: BTreeMap<_, _> = publication_definitions
            .iter()
            .filter(|(_, definitions)| definitions.len() == 1)
            .map(|(id, definitions)| (*id, definitions[0].clone()))
            .filter(|(_, node)| {
                (node.space_id, node.context_id) == *key
                    && !invalid_event_ids.contains(&node.event_id)
            })
            .filter(|(_, node)| {
                let revision_is_valid = valid_contexts[key]
                    .contains_key(&node.publication.revision_id);
                let reviews_are_valid = node.publication.review_event_ids.iter().all(|event_id| {
                    review_by_event.get(event_id).is_some_and(|review| {
                        (review.space_id, review.context_id) == *key
                            && review.review.revision_id == node.publication.revision_id
                    })
                });
                if !revision_is_valid || !reviews_are_valid {
                    push_diagnostic(
                        &mut diagnostics,
                        ReducerDiagnosticCode::InvalidPublicationReference,
                        node.publication.publication_id.to_string(),
                        BTreeSet::from([node.event_id]),
                        format!(
                            "publication {} does not reference a valid same-context revision/review",
                            node.publication.publication_id
                        ),
                    );
                }
                revision_is_valid && reviews_are_valid
            })
            .collect();
        let dag: BTreeMap<_, _> = candidates
            .iter()
            .map(|(id, node)| {
                (
                    *id,
                    DagNode {
                        event_id: node.event_id,
                        parents: node.publication.previous_publication_ids.clone(),
                    },
                )
            })
            .collect();
        let valid = validate_dag(
            &dag,
            ReducerDiagnosticCode::InvalidPublicationReference,
            ReducerDiagnosticCode::PublicationCycle,
            "publication",
            &mut diagnostics,
        );
        publication_heads.insert(*key, heads(&dag, &valid));
        valid_publications.insert(
            *key,
            candidates
                .into_iter()
                .filter(|(id, _)| valid.contains(id))
                .collect(),
        );
    }

    let mut context_space_associations = BTreeMap::new();
    let mut context_space_association_heads = BTreeMap::new();
    let mut context_space_association_conflicts = BTreeMap::new();
    let association_context_ids = association_definitions
        .values()
        .flatten()
        .map(|node| node.association.context_id)
        .collect::<BTreeSet<_>>();
    for context_id in association_context_ids {
        let owner = context_owners
            .get(&context_id)
            .filter(|owners| owners.len() == 1)
            .and_then(BTreeSet::first)
            .copied();
        let candidates = association_definitions
            .iter()
            .filter(|(_, definitions)| definitions.len() == 1)
            .map(|(id, definitions)| (*id, definitions[0].clone()))
            .filter(|(_, node)| {
                node.association.context_id == context_id
                    && !invalid_event_ids.contains(&node.event_id)
            })
            .filter(|(_, node)| {
                let spaces_exist = valid_spaces.contains(&node.association.primary_space_id)
                    && node
                        .association
                        .related_space_ids
                        .iter()
                        .all(|space_id| valid_spaces.contains(space_id));
                let context_exists = owner.is_some_and(|space_id| {
                    valid_contexts.contains_key(&(space_id, context_id))
                });
                let initial_owner_matches = !matches!(
                    node.association.origin,
                    ContextSpaceAssociationOrigin::CandidateConfirmation { .. }
                ) || owner == Some(node.association.primary_space_id);
                let origin_candidate_exists = match node.association.origin {
                    ContextSpaceAssociationOrigin::CandidateConfirmation { candidate_id } => {
                        candidate_definitions
                            .get(&candidate_id)
                            .is_some_and(|definitions| {
                                definitions.len() == 1
                                    && !invalid_event_ids.contains(&definitions[0].event_id)
                                    && submission_definitions
                                        .get(&definitions[0].candidate.submission_id)
                                        .is_some_and(|submissions| submissions.len() == 1)
                            })
                    }
                    ContextSpaceAssociationOrigin::Correction => true,
                };
                if !spaces_exist
                    || !context_exists
                    || !initial_owner_matches
                    || !origin_candidate_exists
                {
                    push_diagnostic(
                        &mut diagnostics,
                        ReducerDiagnosticCode::InvalidSpaceAssociationReference,
                        node.association.association_id.to_string(),
                        BTreeSet::from([node.event_id]),
                        format!(
                            "Space Association {} does not reference valid Spaces and an owned Context",
                            node.association.association_id
                        ),
                    );
                }
                spaces_exist && context_exists && initial_owner_matches && origin_candidate_exists
            })
            .collect::<BTreeMap<_, _>>();
        let dag = candidates
            .iter()
            .map(|(id, node)| {
                (
                    *id,
                    DagNode {
                        event_id: node.event_id,
                        parents: node.association.previous_association_ids.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let valid = validate_dag(
            &dag,
            ReducerDiagnosticCode::InvalidSpaceAssociationReference,
            ReducerDiagnosticCode::SpaceAssociationCycle,
            "Context Space Association",
            &mut diagnostics,
        );
        let current_heads = heads(&dag, &valid);
        for (association_id, node) in candidates {
            if valid.contains(&association_id) {
                context_space_associations.insert(
                    association_id,
                    ContextSpaceAssociationProjection {
                        event_id: node.event_id,
                        association: node.association,
                    },
                );
            }
        }
        if current_heads.len() > 1 {
            context_space_association_conflicts.insert(
                context_id,
                ContextSpaceAssociationConflict {
                    context_id,
                    head_ids: current_heads.clone(),
                },
            );
        }
        if !current_heads.is_empty() {
            context_space_association_heads.insert(context_id, current_heads);
        }
    }

    let mut spaces = BTreeMap::new();
    for space_id in valid_spaces.clone() {
        let intent_nodes = valid_intents.remove(&space_id).unwrap_or_default();
        let intent = IntentProjection {
            revisions: intent_nodes
                .into_iter()
                .map(|(id, node)| (id, node.revision))
                .collect(),
            heads: intent_heads.remove(&space_id).unwrap_or_default(),
        };
        let mut contexts = BTreeMap::new();
        let keys: Vec<_> = valid_contexts
            .keys()
            .filter(|(owner, _)| *owner == space_id)
            .copied()
            .collect();
        for key @ (_, context_id) in keys {
            let nodes = valid_contexts.remove(&key).unwrap_or_default();
            let revision_heads = context_heads.remove(&key).unwrap_or_default();
            let review_nodes = valid_reviews.remove(&key).unwrap_or_default();
            let publication_nodes = valid_publications.remove(&key).unwrap_or_default();
            let heads = publication_heads.remove(&key).unwrap_or_default();

            let governance = match heads.len() {
                0 => ContextGovernanceStatus::Unpublished,
                1 => {
                    let publication_id = *heads.first().expect("one head exists");
                    let publication = &publication_nodes[&publication_id].publication;
                    match publication.action {
                        PublicationAction::Publish => ContextGovernanceStatus::Accepted {
                            publication_id,
                            revision_id: publication.revision_id,
                        },
                        PublicationAction::Withdraw => ContextGovernanceStatus::Deprecated {
                            publication_id,
                            revision_id: publication.revision_id,
                        },
                    }
                }
                _ => ContextGovernanceStatus::GovernanceConflict {
                    publication_ids: heads.clone(),
                },
            };

            let mut superseded = BTreeSet::new();
            for node in publication_nodes.values() {
                if node.publication.action != PublicationAction::Publish {
                    continue;
                }
                for previous_id in &node.publication.previous_publication_ids {
                    let previous = &publication_nodes[previous_id].publication;
                    if previous.revision_id != node.publication.revision_id {
                        superseded.insert(previous.revision_id);
                    }
                }
            }

            let mut revisions = BTreeMap::new();
            for (revision_id, node) in nodes {
                let matching_reviews: Vec<_> = review_nodes
                    .iter()
                    .filter(|(_, review)| review.review.revision_id == revision_id)
                    .collect();
                let approved = matching_reviews
                    .iter()
                    .any(|(_, review)| review.review.verdict == ReviewVerdict::Approve);
                let rejected = matching_reviews
                    .iter()
                    .any(|(_, review)| review.review.verdict == ReviewVerdict::Reject);
                let review_summary = match (approved, rejected) {
                    (false, false) => ReviewSummary::Unreviewed,
                    (true, false) => ReviewSummary::Approved,
                    (false, true) => ReviewSummary::Rejected,
                    (true, true) => ReviewSummary::Mixed,
                };
                let lifecycle = match &governance {
                    ContextGovernanceStatus::Accepted {
                        revision_id: current,
                        ..
                    } if *current == revision_id => RevisionLifecycle::Accepted,
                    ContextGovernanceStatus::Deprecated {
                        revision_id: current,
                        ..
                    } if *current == revision_id => RevisionLifecycle::Deprecated,
                    _ if superseded.contains(&revision_id) => RevisionLifecycle::Superseded,
                    _ => RevisionLifecycle::Candidate,
                };
                revisions.insert(
                    revision_id,
                    RevisionProjection {
                        revision: node.revision,
                        is_head: revision_heads.contains(&revision_id),
                        review_summary,
                        review_event_ids: matching_reviews
                            .into_iter()
                            .map(|(event_id, _)| *event_id)
                            .collect(),
                        lifecycle,
                    },
                );
            }

            contexts.insert(
                context_id,
                ContextProjection {
                    context_id,
                    revisions,
                    revision_heads,
                    reviews: review_nodes
                        .into_iter()
                        .map(|(event_id, node)| (event_id, node.review))
                        .collect(),
                    publications: publication_nodes
                        .into_iter()
                        .map(|(id, node)| (id, node.publication))
                        .collect(),
                    publication_heads: heads,
                    auto_injection: {
                        let blockers = match &governance {
                            ContextGovernanceStatus::Accepted { .. } => BTreeSet::new(),
                            ContextGovernanceStatus::GovernanceConflict { .. } => {
                                BTreeSet::from([AutoInjectionBlocker::GovernanceConflict])
                            }
                            ContextGovernanceStatus::Unpublished
                            | ContextGovernanceStatus::Deprecated { .. } => {
                                BTreeSet::from([AutoInjectionBlocker::NotAccepted])
                            }
                        };
                        AutoInjectionEligibility {
                            eligible: blockers.is_empty(),
                            blockers,
                        }
                    },
                    governance,
                },
            );
        }
        spaces.insert(
            space_id,
            ContextSpaceProjection {
                space_id,
                intent,
                contexts,
            },
        );
    }

    let candidate_confirmation_conflicts = confirmations_by_candidate
        .iter()
        .filter(|(_, definitions)| definitions.len() > 1)
        .map(|(candidate_id, definitions)| {
            (
                *candidate_id,
                CandidateConfirmationConflict {
                    candidate_id: *candidate_id,
                    confirmation_ids: definitions
                        .iter()
                        .map(|node| node.confirmation.confirmation_id)
                        .collect(),
                    event_ids: definitions.iter().map(|node| node.event_id).collect(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let valid_candidate_nodes = candidate_definitions
        .iter()
        .filter(|(_, definitions)| definitions.len() == 1)
        .filter_map(|(candidate_id, definitions)| {
            let node = &definitions[0];
            let submission_is_unique = submission_definitions
                .get(&node.candidate.submission_id)
                .is_some_and(|submissions| {
                    submissions.len() == 1 && submissions[0].candidate.candidate_id == *candidate_id
                });
            (!invalid_event_ids.contains(&node.event_id) && submission_is_unique)
                .then_some((*candidate_id, node))
        })
        .collect::<BTreeMap<_, _>>();
    let mut candidate_confirmations = BTreeMap::new();
    for (candidate_id, definitions) in &confirmations_by_candidate {
        if definitions.len() != 1 {
            continue;
        }
        let node = &definitions[0];
        if invalid_event_ids.contains(&node.event_id) {
            continue;
        }
        let confirmation = &node.confirmation;
        let candidate = valid_candidate_nodes
            .get(candidate_id)
            .map(|node| &node.candidate);
        let association = context_space_associations.get(&confirmation.space_association_id);
        let result_context = spaces
            .get(&confirmation.primary_space_id)
            .and_then(|space| space.contexts.get(&confirmation.result_context_id));
        let result_revision = result_context
            .and_then(|context| context.revisions.get(&confirmation.result_revision_id));
        let publication = result_context
            .and_then(|context| context.publications.get(&confirmation.publication_id));
        let candidate_matches = candidate.is_some_and(|candidate| {
            candidate.submission_id == confirmation.submission_id
                && candidate.source_episode == confirmation.source_episode
        });
        let related = confirmation
            .related_space_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let association_matches = association.is_some_and(|projection| {
            projection.event_id == confirmation.causal_refs.space_association_event_id
                && projection.association.context_id == confirmation.result_context_id
                && projection.association.primary_space_id == confirmation.primary_space_id
                && projection
                    .association
                    .related_space_ids
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    == related
                && matches!(
                    projection.association.origin,
                    ContextSpaceAssociationOrigin::CandidateConfirmation {
                        candidate_id: origin_candidate
                    } if origin_candidate == *candidate_id
                )
        });
        let revision_event_matches = context_definitions
            .get(&confirmation.result_revision_id)
            .is_some_and(|definitions| {
                definitions.len() == 1
                    && definitions[0].event_id == confirmation.causal_refs.context_revision_event_id
                    && definitions[0].space_id == confirmation.primary_space_id
                    && definitions[0].context_id == confirmation.result_context_id
            });
        let publication_event_matches = publication_definitions
            .get(&confirmation.publication_id)
            .is_some_and(|definitions| {
                definitions.len() == 1
                    && definitions[0].event_id == confirmation.causal_refs.publication_event_id
                    && definitions[0].space_id == confirmation.primary_space_id
                    && definitions[0].context_id == confirmation.result_context_id
            });
        let causal_publication_matches = publication.is_some_and(|publication| {
            publication.action == PublicationAction::Publish
                && publication.revision_id == confirmation.result_revision_id
        });
        let created_space_matches = match confirmation.created_space_id {
            Some(space_id) => space_creations.get(&space_id).is_some_and(|definitions| {
                definitions.len() == 1
                    && Some(definitions[0].event_id)
                        == confirmation.causal_refs.space_created_event_id
            }),
            None => confirmation.causal_refs.space_created_event_id.is_none(),
        };
        let engineering_reference_events_match = confirmation
            .causal_refs
            .engineering_reference_event_ids
            .iter()
            .all(|event_id| {
                reference_definitions.values().any(|definitions| {
                    definitions.len() == 1
                        && definitions[0].event_id == *event_id
                        && definitions[0].context_id == confirmation.result_context_id
                        && definitions[0].revision_id == confirmation.result_revision_id
                        && definitions[0].reference.validate().is_ok()
                        && !invalid_event_ids.contains(event_id)
                })
            });
        let content_matches = candidate
            .and_then(|candidate| confirmation.edits.apply(&candidate.content).ok())
            .zip(result_revision)
            .is_some_and(|(edited, revision)| {
                let result = context_revision_as_draft(&revision.revision);
                context_revision_content_hash(&edited) == confirmation.final_content_hash
                    && context_revision_content_hash(&result) == confirmation.final_content_hash
            });
        let spaces_match = valid_spaces.contains(&confirmation.primary_space_id)
            && related
                .iter()
                .all(|space_id| valid_spaces.contains(space_id));
        if candidate_matches
            && association_matches
            && result_revision.is_some()
            && revision_event_matches
            && causal_publication_matches
            && publication_event_matches
            && created_space_matches
            && engineering_reference_events_match
            && content_matches
            && spaces_match
        {
            candidate_confirmations.insert(
                confirmation.confirmation_id,
                CandidateConfirmationProjection {
                    event_id: node.event_id,
                    confirmation: confirmation.clone(),
                },
            );
        } else {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidCandidateConfirmation,
                confirmation.confirmation_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!(
                    "Candidate Confirmation {} does not match its Candidate, causal association, revision, publication, Space, or final content",
                    confirmation.confirmation_id
                ),
            );
        }
    }

    let mut engineering_references = BTreeMap::new();
    for (reference_id, definitions) in &reference_definitions {
        if definitions.len() != 1 {
            continue;
        }
        let node = &definitions[0];
        if invalid_event_ids.contains(&node.event_id) {
            continue;
        }
        if let Err(error) = node.reference.validate() {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidEngineeringReference,
                reference_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!("engineering reference {reference_id} is invalid: {error}"),
            );
            continue;
        }
        let owner = spaces.iter().find_map(|(space_id, space)| {
            space
                .contexts
                .get(&node.context_id)
                .map(|context| (*space_id, context))
        });
        let Some((space_id, context)) = owner else {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidEngineeringReferenceTarget,
                reference_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!(
                    "engineering reference {reference_id} targets missing or quarantined context {}",
                    node.context_id
                ),
            );
            continue;
        };
        if !context.revisions.contains_key(&node.revision_id) {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidEngineeringReferenceTarget,
                reference_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!(
                    "engineering reference {reference_id} revision {} does not belong to context {}",
                    node.revision_id, node.context_id
                ),
            );
            continue;
        }
        engineering_references.insert(
            *reference_id,
            EngineeringReferenceProjection {
                event_id: node.event_id,
                space_id,
                context_id: node.context_id,
                revision_id: node.revision_id,
                reference: node.reference.clone(),
            },
        );
    }

    let semantic_conflict_candidates = conflict_candidates(&spaces);
    let mut valid_conflicts = BTreeMap::new();
    for (conflict_id, definitions) in &conflict_definitions {
        if definitions.len() != 1 {
            continue;
        }
        let node = definitions[0].clone();
        if invalid_event_ids.contains(&node.event_id) {
            continue;
        }

        let mut participant_revisions = Vec::new();
        let mut references_are_valid = true;
        if let Some(space) = spaces.get(&node.space_id) {
            for participant in &node.conflict.participants {
                let Some(context) = space.contexts.get(&participant.context_id) else {
                    references_are_valid = false;
                    break;
                };
                let Some(publication) = context.publications.get(&participant.publication_id)
                else {
                    references_are_valid = false;
                    break;
                };
                let Some(revision) = context.revisions.get(&participant.revision_id) else {
                    references_are_valid = false;
                    break;
                };
                if publication.action != PublicationAction::Publish
                    || publication.revision_id != participant.revision_id
                    || !matches!(
                        revision.revision.kind,
                        ContextKind::Decision | ContextKind::Contract
                    )
                {
                    references_are_valid = false;
                    break;
                }
                participant_revisions.push(&revision.revision);
            }
        } else {
            references_are_valid = false;
        }

        let same_topic_and_overlapping = references_are_valid
            && participant_revisions
                .iter()
                .enumerate()
                .all(|(index, left)| {
                    participant_revisions.iter().skip(index + 1).all(|right| {
                        left.topic_key == right.topic_key
                            && applicability_overlaps(&left.applicability, &right.applicability)
                    })
                })
            && participant_revisions.iter().all(|revision| {
                applicability_overlaps(&node.conflict.applicability, &revision.applicability)
            });
        if !same_topic_and_overlapping {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidSemanticConflictReference,
                conflict_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!(
                    "semantic conflict {conflict_id} must reference valid same-space publish nodes with one overlapping topic"
                ),
            );
            continue;
        }
        valid_conflicts.insert(*conflict_id, node);
    }

    for definitions in resolution_definitions
        .values()
        .filter(|items| items.len() == 1)
    {
        let node = &definitions[0];
        if invalid_event_ids.contains(&node.event_id) {
            continue;
        }
        if valid_conflicts
            .get(&node.conflict_id)
            .is_none_or(|conflict| conflict.space_id != node.space_id)
        {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::InvalidResolutionReference,
                node.resolution.resolution_id.to_string(),
                BTreeSet::from([node.event_id]),
                format!(
                    "resolution {} references a missing, quarantined, or cross-space conflict",
                    node.resolution.resolution_id
                ),
            );
        }
    }

    let mut semantic_conflicts = BTreeMap::new();
    for (conflict_id, conflict_node) in &valid_conflicts {
        let participant_contexts: BTreeSet<_> = conflict_node
            .conflict
            .participants
            .iter()
            .map(|participant| participant.context_id)
            .collect();
        let space = &spaces[&conflict_node.space_id];
        let publication_owners: BTreeMap<_, _> = participant_contexts
            .iter()
            .flat_map(|context_id| {
                space.contexts[context_id]
                    .publications
                    .keys()
                    .map(|publication_id| (*publication_id, *context_id))
            })
            .collect();

        let candidates: BTreeMap<_, _> = resolution_definitions
            .iter()
            .filter(|(_, definitions)| definitions.len() == 1)
            .map(|(id, definitions)| (*id, definitions[0].clone()))
            .filter(|(_, node)| {
                node.conflict_id == *conflict_id
                    && node.space_id == conflict_node.space_id
                    && !invalid_event_ids.contains(&node.event_id)
            })
            .filter(|(_, node)| {
                let result_contexts: BTreeSet<_> = node
                    .resolution
                    .results
                    .iter()
                    .map(|result| result.context_id)
                    .collect();
                let results_are_valid = result_contexts == participant_contexts
                    && node.resolution.results.iter().all(|result| {
                        space.contexts.get(&result.context_id).is_some_and(|context| {
                            context.revisions.contains_key(&result.revision_id)
                        })
                    });
                let publications_are_valid = node
                    .resolution
                    .related_publication_ids
                    .iter()
                    .all(|publication_id| publication_owners.contains_key(publication_id));
                if !results_are_valid || !publications_are_valid {
                    push_diagnostic(
                        &mut diagnostics,
                        ReducerDiagnosticCode::InvalidResolutionReference,
                        node.resolution.resolution_id.to_string(),
                        BTreeSet::from([node.event_id]),
                        format!(
                            "resolution {} must cover every participant with valid same-conflict revisions and publications",
                            node.resolution.resolution_id
                        ),
                    );
                }
                results_are_valid && publications_are_valid
            })
            .collect();
        let dag: BTreeMap<_, _> = candidates
            .iter()
            .map(|(id, node)| {
                (
                    *id,
                    DagNode {
                        event_id: node.event_id,
                        parents: node.resolution.previous_resolution_ids.clone(),
                    },
                )
            })
            .collect();
        let valid = validate_dag(
            &dag,
            ReducerDiagnosticCode::InvalidResolutionReference,
            ReducerDiagnosticCode::ResolutionCycle,
            "semantic conflict resolution",
            &mut diagnostics,
        );
        let resolution_heads = heads(&dag, &valid);
        let current_publication_heads: BTreeSet<_> = participant_contexts
            .iter()
            .flat_map(|context_id| space.contexts[context_id].publication_heads.iter().copied())
            .collect();
        let publications_converged = participant_contexts
            .iter()
            .all(|context_id| space.contexts[context_id].publication_heads.len() == 1);

        let status = if resolution_heads.len() == 1 {
            let resolution_id = *resolution_heads
                .first()
                .expect("one resolution head exists");
            let resolution = &candidates[&resolution_id].resolution;
            let related: BTreeSet<_> = resolution.related_publication_ids.iter().copied().collect();
            let mut reasons = BTreeSet::new();
            if !publications_converged {
                reasons.insert(SemanticConflictOpenReason::PublicationHeadsNotConverged);
            }
            if related != current_publication_heads {
                reasons.insert(
                    SemanticConflictOpenReason::ResolutionDoesNotCoverCurrentPublicationHeads,
                );
            }
            if reasons.is_empty() {
                SemanticConflictStatus::Resolved { resolution_id }
            } else {
                SemanticConflictStatus::Open { reasons }
            }
        } else {
            let mut reasons = BTreeSet::new();
            if resolution_heads.is_empty() {
                reasons.insert(SemanticConflictOpenReason::NoResolution);
            } else {
                reasons.insert(SemanticConflictOpenReason::ResolutionConflict);
            }
            if !publications_converged {
                reasons.insert(SemanticConflictOpenReason::PublicationHeadsNotConverged);
            }
            SemanticConflictStatus::Open { reasons }
        };

        semantic_conflicts.insert(
            *conflict_id,
            SemanticConflictProjection {
                space_id: conflict_node.space_id,
                conflict: conflict_node.conflict.clone(),
                resolutions: candidates
                    .into_iter()
                    .filter(|(id, _)| valid.contains(id))
                    .map(|(id, node)| (id, node.resolution))
                    .collect(),
                resolution_heads,
                status,
            },
        );
    }

    for (conflict_id, conflict) in &semantic_conflicts {
        if !matches!(conflict.status, SemanticConflictStatus::Open { .. }) {
            continue;
        }
        for participant in &conflict.conflict.participants {
            let eligibility = &mut spaces
                .get_mut(&conflict.space_id)
                .expect("valid conflict space exists")
                .contexts
                .get_mut(&participant.context_id)
                .expect("valid conflict participant exists")
                .auto_injection;
            eligibility
                .blockers
                .insert(AutoInjectionBlocker::UnresolvedSemanticConflict(
                    *conflict_id,
                ));
            eligibility.eligible = false;
        }
    }

    let quarantined_event_ids = diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.event_ids.iter().copied())
        .collect();
    let mut candidate_submissions = BTreeMap::new();
    let mut candidate_submission_conflicts = BTreeMap::new();
    for (submission_id, definitions) in submission_definitions {
        if definitions.len() == 1 && !invalid_event_ids.contains(&definitions[0].event_id) {
            let node = &definitions[0];
            candidate_submissions.insert(
                submission_id,
                CandidateSubmissionProjection {
                    submission_id,
                    candidate_id: node.candidate.candidate_id,
                    event_id: node.event_id,
                    source_episode: node.candidate.source_episode,
                    content_hash: node.candidate.submission_content_hash(),
                },
            );
        } else {
            candidate_submission_conflicts.insert(
                submission_id,
                CandidateSubmissionConflict {
                    submission_id,
                    event_ids: definitions.iter().map(|node| node.event_id).collect(),
                    candidate_ids: definitions
                        .iter()
                        .map(|node| node.candidate.candidate_id)
                        .collect(),
                    content_hashes: definitions
                        .iter()
                        .map(|node| node.candidate.submission_content_hash())
                        .collect(),
                },
            );
        }
    }
    let candidates = candidate_definitions
        .into_iter()
        .filter(|(_, definitions)| definitions.len() == 1)
        .filter_map(|(candidate_id, mut definitions)| {
            let node = definitions.pop().expect("one Candidate definition exists");
            (!invalid_event_ids.contains(&node.event_id)).then_some((
                candidate_id,
                CandidateProjection {
                    event_id: node.event_id,
                    candidate: node.candidate,
                },
            ))
        })
        .collect();
    DomainProjection {
        candidates,
        candidate_submissions,
        candidate_submission_conflicts,
        candidate_confirmations,
        candidate_confirmation_conflicts,
        context_space_associations,
        context_space_association_heads,
        context_space_association_conflicts,
        spaces,
        engineering_references,
        semantic_conflict_candidates,
        semantic_conflicts,
        quarantined_event_ids,
        diagnostics: diagnostics.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ContextGovernanceStatus, ContextProjection, ContextSpaceProjection, IntentProjection,
        RevisionProjection, conflict_candidates,
    };
    use crate::{
        Applicability, AutoInjectionEligibility, ContextId, ContextKind, ContextRevision,
        ContextRevisionDraft, EvidenceSnapshotDraft, EvidenceType, PublicationId, ReviewSummary,
        RevisionLifecycle, SpaceId,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn accepted_context(topic_key: Option<&str>, statement: &str) -> ContextProjection {
        let revision = ContextRevision::from_draft(
            Vec::new(),
            ContextRevisionDraft {
                kind: ContextKind::Decision,
                topic_key: topic_key.map(ToOwned::to_owned),
                problem_view: None,
                statement: statement.to_owned(),
                rationale: "the reducer only needs valid content".to_owned(),
                applicability: Applicability {
                    domains: vec!["mcp".to_owned()],
                    platforms: Vec::new(),
                    conditions: Vec::new(),
                },
                assumptions: Vec::new(),
                recheck_when: Vec::new(),
                hints: Vec::new(),
                relations: Vec::new(),
                evidence: vec![EvidenceSnapshotDraft {
                    kind: EvidenceType::ExperimentRecord,
                    supports: statement.to_owned(),
                    content: serde_json::json!({"actual": "observed"}),
                    interpretation: "the fixture holds".to_owned(),
                    limitations: Vec::new(),
                }],
            },
        )
        .unwrap();
        let revision_id = revision.revision_id;
        let publication_id = PublicationId::new();
        ContextProjection {
            context_id: ContextId::new(),
            revisions: BTreeMap::from([(
                revision_id,
                RevisionProjection {
                    revision,
                    is_head: true,
                    review_summary: ReviewSummary::Approved,
                    review_event_ids: BTreeSet::new(),
                    lifecycle: RevisionLifecycle::Accepted,
                },
            )]),
            revision_heads: BTreeSet::from([revision_id]),
            reviews: BTreeMap::new(),
            publications: BTreeMap::new(),
            publication_heads: BTreeSet::new(),
            governance: ContextGovernanceStatus::Accepted {
                publication_id,
                revision_id,
            },
            auto_injection: AutoInjectionEligibility {
                eligible: true,
                blockers: BTreeSet::new(),
            },
        }
    }

    fn one_space(contexts: Vec<ContextProjection>) -> BTreeMap<SpaceId, ContextSpaceProjection> {
        let space_id = SpaceId::new();
        BTreeMap::from([(
            space_id,
            ContextSpaceProjection {
                space_id,
                intent: IntentProjection {
                    revisions: BTreeMap::new(),
                    heads: BTreeSet::new(),
                },
                contexts: contexts
                    .into_iter()
                    .map(|context| (context.context_id, context))
                    .collect(),
            },
        )])
    }

    /// The duplicate detector keys on `topic_key`, so it only sees a pair once Candidate Build
    /// derives one. Two accepted Decisions restating one fact under the same server-derived topic
    /// are exactly the pair a reviewer has to settle.
    #[test]
    fn same_topic_key_and_overlapping_scope_is_one_duplicate_pair() {
        let spaces = one_space(vec![
            accepted_context(
                Some("decision:text:productanchorassem"),
                "ProductAnchorAssem returns before the live entry resolves",
            ),
            accepted_context(
                Some("decision:text:productanchorassem"),
                "ProductAnchorAssem 在直播入口解析前提前返回",
            ),
        ]);
        let candidates = conflict_candidates(&spaces);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].topic_key, "decision:text:productanchorassem");
        assert_eq!(candidates[0].participants.len(), 2);
    }

    /// The pre-derivation behaviour: an absent topic key is not a topic two Contexts share, so an
    /// unkeyed pair stays invisible to the detector however similar the two statements are.
    #[test]
    fn an_absent_topic_key_never_pairs() {
        let spaces = one_space(vec![
            accepted_context(
                None,
                "ProductAnchorAssem returns before the live entry resolves",
            ),
            accepted_context(None, "ProductAnchorAssem 在直播入口解析前提前返回"),
        ]);
        assert!(conflict_candidates(&spaces).is_empty());
    }
}
