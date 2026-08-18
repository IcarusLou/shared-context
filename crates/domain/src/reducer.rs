use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    ContextId, ContextRevision, EventId, EvidenceId, IntentRevision, Publication,
    PublicationAction, PublicationId, Review, ReviewId, ReviewVerdict, RevisionId, SpaceId,
};

/// Authoritative event input understood by the V1 domain reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReducerEvent {
    pub event_id: EventId,
    pub payload: ReducerPayload,
}

/// The five V1 payloads in the pure Context/Intent/Review/Publication boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReducerPayload {
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
}

/// Stable reducer diagnostic classifications.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReducerDiagnosticCode {
    DuplicateEventId,
    DuplicateRevisionId,
    DuplicateReviewId,
    DuplicatePublicationId,
    DuplicateEvidenceId,
    DuplicateSpaceCreation,
    AmbiguousContextOwner,
    MissingSpace,
    InvalidRevisionReference,
    RevisionCycle,
    InvalidReviewReference,
    InvalidPublicationReference,
    PublicationCycle,
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

/// Complete deterministic result of reducing one event multiset.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DomainProjection {
    pub spaces: BTreeMap<SpaceId, ContextSpaceProjection>,
    pub diagnostics: Vec<ReducerDiagnostic>,
}

#[derive(Clone)]
struct IntentNode {
    event_id: EventId,
    space_id: SpaceId,
    revision: IntentRevision,
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
    let mut space_creations: BTreeMap<SpaceId, Vec<IntentNode>> = BTreeMap::new();
    let mut intent_definitions: BTreeMap<RevisionId, Vec<IntentNode>> = BTreeMap::new();
    let mut context_definitions: BTreeMap<RevisionId, Vec<ContextNode>> = BTreeMap::new();
    let mut review_definitions: BTreeMap<ReviewId, Vec<ReviewNode>> = BTreeMap::new();
    let mut publication_definitions: BTreeMap<PublicationId, Vec<PublicationNode>> =
        BTreeMap::new();
    let mut evidence_definitions: BTreeMap<EvidenceId, Vec<EventId>> = BTreeMap::new();
    let mut context_owners: BTreeMap<ContextId, BTreeSet<SpaceId>> = BTreeMap::new();

    for event in events {
        events_by_id.entry(event.event_id).or_default().push(event);
        match &event.payload {
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
        let space_id = match &event.payload {
            ReducerPayload::SpaceCreated { space_id, .. }
            | ReducerPayload::SpaceIntentRevisionAdded { space_id, .. }
            | ReducerPayload::ContextRevisionAdded { space_id, .. }
            | ReducerPayload::ContextReviewed { space_id, .. }
            | ReducerPayload::ContextPublicationChanged { space_id, .. } => *space_id,
        };
        if !space_creations.contains_key(&space_id) {
            push_diagnostic(
                &mut diagnostics,
                ReducerDiagnosticCode::MissingSpace,
                space_id.to_string(),
                BTreeSet::from([event.event_id]),
                format!("event references space {space_id} without a creation event"),
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

    let mut spaces = BTreeMap::new();
    for space_id in valid_spaces {
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

    DomainProjection {
        spaces,
        diagnostics: diagnostics.into_iter().collect(),
    }
}
