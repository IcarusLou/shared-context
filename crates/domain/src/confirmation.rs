use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Applicability, CandidateId, ConfirmationId, ConflictParticipant, ContextCandidate, ContextId,
    ContextKind, ContextRelation, ContextRevision, ContextRevisionDraft, EngineeringReference,
    EngineeringReferenceDraft, Error, ErrorKind, EventId, EvidenceSnapshotDraft, IntentRevision,
    IntentSnapshot, Publication, PublicationAction, PublicationDraft, PublicationId, Result,
    RevisionId, SemanticConflict, SemanticConflictDraft, SpaceAssociationId, SpaceId,
    SpaceRecommendationId, SubmissionId, WorkEpisodeRef,
};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn require_unique<T>(values: &[T], field: &str) -> Result<()>
where
    T: Eq + std::hash::Hash,
{
    if values.iter().collect::<HashSet<_>>().len() != values.len() {
        return Err(invalid(format!("{field} must not contain duplicates")));
    }
    Ok(())
}

/// Who decided one Candidate disposition.
///
/// It is provenance, never domain semantics: it is recorded in event `annotations` and in the
/// local Runtime, and it never reaches an event payload, the Confirmation operation hash, or any
/// replay identity. `Human` is the default and the only value a client that never states one
/// produces, so every disposition written before this existed reads back as a human decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// An explicit human choice; the default for every request that omits the field.
    #[default]
    Human,
    /// The session Agent acting inside the server-verified automatic permission surface.
    AgentPolicy,
}

impl DecisionSource {
    /// Stable wire and storage name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::AgentPolicy => "agent_policy",
        }
    }

    /// Parses the stable wire name.
    ///
    /// # Errors
    ///
    /// Rejects any spelling other than `human` or `agent_policy`.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "human" => Ok(Self::Human),
            "agent_policy" => Ok(Self::AgentPolicy),
            other => Err(invalid(format!(
                "decision_source must be human or agent_policy, not {other}"
            ))),
        }
    }
}

impl std::fmt::Display for DecisionSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Exactly one Primary Space choice supplied when confirming a Candidate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidatePrimarySelection {
    Existing { space_id: SpaceId },
    ProposedNew { intent: IntentSnapshot },
}

/// Public selection identity included in one confirmation operation hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateConfirmationPrimaryReference {
    ExistingSpace {
        space_id: SpaceId,
    },
    ProposedRecommendation {
        recommendation_id: SpaceRecommendationId,
    },
}

impl CandidatePrimarySelection {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Existing { .. } => Ok(()),
            Self::ProposedNew { intent } => intent.validate(),
        }
    }
}

/// Explicit replacement for the only nullable Candidate content field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TopicKeyEdit {
    Set { value: String },
    Clear,
}

/// Explicit replacement for the nullable Candidate `problem_view` field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProblemViewEdit {
    Set { value: String },
    Clear,
}

/// Field-level replacements applied to the complete Candidate draft at confirmation.
///
/// Absence preserves the Candidate field; [`TopicKeyEdit::Clear`] explicitly removes the topic.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionalCandidateEdits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ContextKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_key: Option<TopicKeyEdit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem_view: Option<ProblemViewEdit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applicability: Option<Applicability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assumptions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recheck_when: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relations: Option<Vec<ContextRelation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<EvidenceSnapshotDraft>>,
}

impl OptionalCandidateEdits {
    /// True when no field replacement was supplied at all.
    ///
    /// Every field is optional and absence preserves the Candidate, so "no edits" and "the default
    /// value" are the same fact.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Applies only supplied replacements and validates the resulting complete draft.
    ///
    /// # Errors
    ///
    /// Returns an input error when the edited result is not a valid `ContextRevisionDraft`.
    pub fn apply(&self, source: &ContextRevisionDraft) -> Result<ContextRevisionDraft> {
        let result = ContextRevisionDraft {
            kind: self.kind.unwrap_or(source.kind),
            topic_key: match &self.topic_key {
                Some(TopicKeyEdit::Set { value }) => Some(value.clone()),
                Some(TopicKeyEdit::Clear) => None,
                None => source.topic_key.clone(),
            },
            problem_view: match &self.problem_view {
                Some(ProblemViewEdit::Set { value }) => Some(value.clone()),
                Some(ProblemViewEdit::Clear) => None,
                None => source.problem_view.clone(),
            },
            statement: self
                .statement
                .clone()
                .unwrap_or_else(|| source.statement.clone()),
            rationale: self
                .rationale
                .clone()
                .unwrap_or_else(|| source.rationale.clone()),
            applicability: self
                .applicability
                .clone()
                .unwrap_or_else(|| source.applicability.clone()),
            assumptions: self
                .assumptions
                .clone()
                .unwrap_or_else(|| source.assumptions.clone()),
            recheck_when: self
                .recheck_when
                .clone()
                .unwrap_or_else(|| source.recheck_when.clone()),
            hints: self.hints.clone().unwrap_or_else(|| source.hints.clone()),
            relations: self
                .relations
                .clone()
                .unwrap_or_else(|| source.relations.clone()),
            evidence: self
                .evidence
                .clone()
                .unwrap_or_else(|| source.evidence.clone()),
        };
        result.validate()?;
        Ok(result)
    }
}

/// Pure confirmation input; it contains no generated identity, Space route, or governance fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationRequest {
    pub candidate_id: CandidateId,
    pub primary: CandidatePrimarySelection,
    pub related_space_ids: Vec<SpaceId>,
    pub edits: OptionalCandidateEdits,
}

/// Authoritative semantics of one human confirmation operation, excluding generated identities.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationOperation {
    pub candidate_id: CandidateId,
    pub review_parent_version: u64,
    pub analysis_generation: u64,
    pub primary: CandidateConfirmationPrimaryReference,
    pub related_space_ids: Vec<SpaceId>,
    pub edits: OptionalCandidateEdits,
}

impl CandidateConfirmationOperation {
    /// Returns the canonical semantic operation hash used for retry/conflict detection.
    ///
    /// # Panics
    ///
    /// Panics only if JSON-backed domain values unexpectedly fail serialization.
    #[must_use]
    pub fn operation_hash(&self) -> String {
        let bytes = serde_json::to_vec(self)
            .expect("serializing Candidate Confirmation operation cannot fail");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    /// Validates the non-generated operation boundary.
    ///
    /// # Errors
    ///
    /// Rejects zero versions, duplicate Related Spaces, or Primary/Related overlap.
    pub fn validate(&self) -> Result<()> {
        if self.review_parent_version == 0 || self.analysis_generation == 0 {
            return Err(invalid(
                "Candidate Confirmation operation requires positive Review and analysis generations",
            ));
        }
        require_unique(
            &self.related_space_ids,
            "candidate_confirmation_operation.related_space_ids",
        )?;
        if let CandidateConfirmationPrimaryReference::ExistingSpace { space_id } = self.primary
            && self.related_space_ids.contains(&space_id)
        {
            return Err(invalid(
                "Candidate Confirmation operation Related Spaces must not include Primary",
            ));
        }
        Ok(())
    }
}

impl CandidateConfirmationRequest {
    /// Validates selection shape and produces the final complete Candidate content.
    ///
    /// # Errors
    ///
    /// Rejects a different Candidate, duplicate/Primary Related Spaces, an incomplete proposed
    /// Space Intent, or invalid field replacements.
    pub fn final_draft(&self, candidate: &ContextCandidate) -> Result<ContextRevisionDraft> {
        if self.candidate_id != candidate.candidate_id {
            return Err(invalid(
                "Candidate Confirmation request names a different Candidate",
            ));
        }
        self.primary.validate()?;
        require_unique(
            &self.related_space_ids,
            "candidate_confirmation.related_space_ids",
        )?;
        if let CandidatePrimarySelection::Existing { space_id } = self.primary
            && self.related_space_ids.contains(&space_id)
        {
            return Err(invalid(
                "Candidate Confirmation Related Spaces must not include Primary",
            ));
        }
        self.edits.apply(&candidate.content)
    }
}

/// Why one Context-to-Space association snapshot exists.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextSpaceAssociationOrigin {
    CandidateConfirmation { candidate_id: CandidateId },
    Correction,
}

/// Caller-independent inputs for one complete Context-to-Space association snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSpaceAssociationDraft {
    pub context_id: ContextId,
    pub primary_space_id: SpaceId,
    pub related_space_ids: Vec<SpaceId>,
    pub previous_association_ids: Vec<SpaceAssociationId>,
    pub origin: ContextSpaceAssociationOrigin,
}

impl ContextSpaceAssociationDraft {
    fn validate(&self) -> Result<()> {
        require_unique(
            &self.related_space_ids,
            "context_space_association.related_space_ids",
        )?;
        if self.related_space_ids.contains(&self.primary_space_id) {
            return Err(invalid(
                "Context Space Association Related Spaces must not include Primary",
            ));
        }
        require_unique(
            &self.previous_association_ids,
            "context_space_association.previous_association_ids",
        )?;
        match self.origin {
            ContextSpaceAssociationOrigin::CandidateConfirmation { .. }
                if !self.previous_association_ids.is_empty() =>
            {
                Err(invalid(
                    "Candidate-origin Context Space Association must be initial",
                ))
            }
            ContextSpaceAssociationOrigin::Correction
                if self.previous_association_ids.is_empty() =>
            {
                Err(invalid(
                    "corrected Context Space Association requires a causal parent",
                ))
            }
            ContextSpaceAssociationOrigin::CandidateConfirmation { .. }
            | ContextSpaceAssociationOrigin::Correction => Ok(()),
        }
    }
}

/// One immutable, causally revisable Context organization fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSpaceAssociation {
    pub association_id: SpaceAssociationId,
    pub context_id: ContextId,
    pub primary_space_id: SpaceId,
    pub related_space_ids: Vec<SpaceId>,
    pub previous_association_ids: Vec<SpaceAssociationId>,
    pub origin: ContextSpaceAssociationOrigin,
}

impl ContextSpaceAssociation {
    /// Creates an immutable association with a server-owned identity.
    ///
    /// # Errors
    ///
    /// Rejects duplicate/Primary Related Spaces or an invalid causal boundary.
    pub fn from_draft(draft: ContextSpaceAssociationDraft) -> Result<Self> {
        draft.validate()?;
        Ok(Self {
            association_id: SpaceAssociationId::new(),
            context_id: draft.context_id,
            primary_space_id: draft.primary_space_id,
            related_space_ids: draft.related_space_ids,
            previous_association_ids: draft.previous_association_ids,
            origin: draft.origin,
        })
    }

    /// Validates parsed association content.
    ///
    /// # Errors
    ///
    /// Returns an input error for invalid Space membership or causality.
    pub fn validate(&self) -> Result<()> {
        ContextSpaceAssociationDraft {
            context_id: self.context_id,
            primary_space_id: self.primary_space_id,
            related_space_ids: self.related_space_ids.clone(),
            previous_association_ids: self.previous_association_ids.clone(),
            origin: self.origin,
        }
        .validate()
    }
}

/// Exact sibling Events that causally close one Candidate confirmation fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationCausalRefs {
    pub space_created_event_id: Option<EventId>,
    pub context_revision_event_id: EventId,
    pub space_association_event_id: EventId,
    pub publication_event_id: EventId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engineering_reference_event_ids: Vec<EventId>,
}

impl CandidateConfirmationCausalRefs {
    fn validate(&self) -> Result<()> {
        let mut ids = vec![
            self.context_revision_event_id,
            self.space_association_event_id,
            self.publication_event_id,
        ];
        if let Some(space_created_event_id) = self.space_created_event_id {
            ids.push(space_created_event_id);
        }
        ids.extend(self.engineering_reference_event_ids.iter().copied());
        require_unique(&ids, "candidate_confirmation.causal_refs")
    }
}

/// Complete confirmation fact before the server assigns `ConfirmationId`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationDraft {
    pub candidate_id: CandidateId,
    pub submission_id: SubmissionId,
    pub source_episode: WorkEpisodeRef,
    pub result_context_id: ContextId,
    pub result_revision_id: RevisionId,
    pub primary_space_id: SpaceId,
    pub related_space_ids: Vec<SpaceId>,
    pub space_association_id: SpaceAssociationId,
    pub publication_id: PublicationId,
    pub created_space_id: Option<SpaceId>,
    pub edits: OptionalCandidateEdits,
    pub final_content_hash: String,
    pub causal_refs: CandidateConfirmationCausalRefs,
}

impl CandidateConfirmationDraft {
    fn validate(&self) -> Result<()> {
        require_unique(
            &self.related_space_ids,
            "candidate_confirmation.related_space_ids",
        )?;
        if self.related_space_ids.contains(&self.primary_space_id) {
            return Err(invalid(
                "Candidate Confirmation Related Spaces must not include Primary",
            ));
        }
        match (
            self.created_space_id,
            self.causal_refs.space_created_event_id,
        ) {
            (Some(created), Some(_)) if created == self.primary_space_id => {}
            (None, None) => {}
            _ => {
                return Err(invalid(
                    "Candidate Confirmation created Space and causal Event must both identify new Primary",
                ));
            }
        }
        if !valid_content_hash(&self.final_content_hash) {
            return Err(invalid(
                "Candidate Confirmation final content hash must be canonical sha256",
            ));
        }
        self.causal_refs.validate()
    }
}

/// Immutable fact that one Candidate became one accepted Context revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmation {
    pub confirmation_id: ConfirmationId,
    pub candidate_id: CandidateId,
    pub submission_id: SubmissionId,
    pub source_episode: WorkEpisodeRef,
    pub result_context_id: ContextId,
    pub result_revision_id: RevisionId,
    pub primary_space_id: SpaceId,
    pub related_space_ids: Vec<SpaceId>,
    pub space_association_id: SpaceAssociationId,
    pub publication_id: PublicationId,
    pub created_space_id: Option<SpaceId>,
    pub edits: OptionalCandidateEdits,
    pub final_content_hash: String,
    pub causal_refs: CandidateConfirmationCausalRefs,
}

impl CandidateConfirmation {
    /// Creates a Confirmation with a server-owned identity.
    ///
    /// # Errors
    ///
    /// Rejects inconsistent Space selection, duplicate causal references, or an invalid hash.
    pub fn from_draft(draft: CandidateConfirmationDraft) -> Result<Self> {
        draft.validate()?;
        Ok(Self {
            confirmation_id: ConfirmationId::new(),
            candidate_id: draft.candidate_id,
            submission_id: draft.submission_id,
            source_episode: draft.source_episode,
            result_context_id: draft.result_context_id,
            result_revision_id: draft.result_revision_id,
            primary_space_id: draft.primary_space_id,
            related_space_ids: draft.related_space_ids,
            space_association_id: draft.space_association_id,
            publication_id: draft.publication_id,
            created_space_id: draft.created_space_id,
            edits: draft.edits,
            final_content_hash: draft.final_content_hash,
            causal_refs: draft.causal_refs,
        })
    }

    /// Validates parsed Confirmation content without resolving cross-Event facts.
    ///
    /// # Errors
    ///
    /// Returns an input error for invalid local selection, hash, or causality.
    pub fn validate(&self) -> Result<()> {
        CandidateConfirmationDraft {
            candidate_id: self.candidate_id,
            submission_id: self.submission_id,
            source_episode: self.source_episode,
            result_context_id: self.result_context_id,
            result_revision_id: self.result_revision_id,
            primary_space_id: self.primary_space_id,
            related_space_ids: self.related_space_ids.clone(),
            space_association_id: self.space_association_id,
            publication_id: self.publication_id,
            created_space_id: self.created_space_id,
            edits: self.edits.clone(),
            final_content_hash: self.final_content_hash.clone(),
            causal_refs: self.causal_refs.clone(),
        }
        .validate()
    }
}

/// Server-owned Event identities for one atomic Confirmation fact closure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationPlanEventIds {
    pub space_created_event_id: Option<EventId>,
    pub context_revision_event_id: EventId,
    pub space_association_event_id: EventId,
    pub publication_event_id: EventId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engineering_reference_event_ids: Vec<EventId>,
    /// One Event identity per automatically opened `SemanticConflict`, in plan order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_conflict_event_ids: Vec<EventId>,
    pub confirmation_event_id: EventId,
}

/// Other accepted side of a semantic conflict this Confirmation must open.
///
/// The confirming revision is not named here: it does not exist until
/// [`CandidateConfirmationPlan::reserve`] generates it, and naming it twice would let a caller
/// disagree with the reserved plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConflictOpeningDraft {
    pub target: ConflictParticipant,
    pub reason: String,
}

/// One `SemanticConflict` reserved inside a Confirmation plan and written in the same batch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConflictOpening {
    pub space_id: SpaceId,
    pub conflict: SemanticConflict,
}

/// Server-owned new Space fact reserved inside one Confirmation plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationNewSpace {
    pub space_id: SpaceId,
    pub intent_revision: IntentRevision,
}

/// Complete stable fact closure reserved before one atomic Git confirmation write.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationPlan {
    pub operation: CandidateConfirmationOperation,
    pub operation_hash: String,
    pub new_space: Option<CandidateConfirmationNewSpace>,
    pub result_context_id: ContextId,
    pub result_revision: ContextRevision,
    pub space_association: ContextSpaceAssociation,
    pub publication: Publication,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engineering_references: Vec<EngineeringReference>,
    /// Semantic conflicts opened by the confirming revision's own `contradicts` Relations.
    ///
    /// They are part of the reserved closure, so a same-content replay of the Confirmation
    /// resolves to the identical plan hash and writes no second conflict Event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opened_conflicts: Vec<SemanticConflictOpening>,
    pub confirmation: CandidateConfirmation,
    pub event_ids: CandidateConfirmationPlanEventIds,
}

impl CandidateConfirmationPlan {
    /// Generates every stable identity and causal reference for one Confirmation operation.
    ///
    /// # Errors
    ///
    /// Rejects invalid selection resolution, edits, membership, or resulting fact closure.
    #[allow(clippy::too_many_lines)]
    pub fn reserve(
        candidate: &ContextCandidate,
        operation: CandidateConfirmationOperation,
        resolved_primary: CandidatePrimarySelection,
        engineering_reference_drafts: Vec<EngineeringReferenceDraft>,
        conflict_openings: Vec<SemanticConflictOpeningDraft>,
    ) -> Result<Self> {
        operation.validate()?;
        if operation.candidate_id != candidate.candidate_id {
            return Err(invalid(
                "Candidate Confirmation operation names a different Candidate",
            ));
        }
        match (&operation.primary, &resolved_primary) {
            (
                CandidateConfirmationPrimaryReference::ExistingSpace { space_id: expected },
                CandidatePrimarySelection::Existing { space_id: actual },
            ) if expected == actual => {}
            (
                CandidateConfirmationPrimaryReference::ProposedRecommendation { .. },
                CandidatePrimarySelection::ProposedNew { .. },
            ) => {}
            _ => {
                return Err(invalid(
                    "Candidate Confirmation primary reference and resolved selection disagree",
                ));
            }
        }
        let request = CandidateConfirmationRequest {
            candidate_id: operation.candidate_id,
            primary: resolved_primary.clone(),
            related_space_ids: operation.related_space_ids.clone(),
            edits: operation.edits.clone(),
        };
        let final_draft = request.final_draft(candidate)?;
        let result_revision = ContextRevision::from_draft(Vec::new(), final_draft.clone())?;
        let result_context_id = ContextId::new();
        for (index, reference) in engineering_reference_drafts.iter().enumerate() {
            reference.validate()?;
            if engineering_reference_drafts[..index].contains(reference) {
                return Err(invalid(
                    "Candidate Confirmation Engineering References must not contain duplicates",
                ));
            }
        }
        let engineering_references = engineering_reference_drafts
            .into_iter()
            .map(EngineeringReference::from_draft)
            .collect::<Result<Vec<_>>>()?;
        let engineering_reference_event_ids = engineering_references
            .iter()
            .map(|_| EventId::new())
            .collect::<Vec<_>>();
        let (primary_space_id, new_space, space_created_event_id) = match resolved_primary {
            CandidatePrimarySelection::Existing { space_id } => (space_id, None, None),
            CandidatePrimarySelection::ProposedNew { intent } => {
                let space_id = SpaceId::new();
                (
                    space_id,
                    Some(CandidateConfirmationNewSpace {
                        space_id,
                        intent_revision: IntentRevision {
                            revision_id: RevisionId::new(),
                            parent_revision_ids: Vec::new(),
                            intent,
                            // The server proposed this Space from the Task Working Intent; no
                            // human has named it yet, so the Space starts provisional.
                            provisional: true,
                        },
                    }),
                    Some(EventId::new()),
                )
            }
        };
        let context_revision_event_id = EventId::new();
        let space_association_event_id = EventId::new();
        let publication_event_id = EventId::new();
        let confirmation_event_id = EventId::new();
        let space_association =
            ContextSpaceAssociation::from_draft(ContextSpaceAssociationDraft {
                context_id: result_context_id,
                primary_space_id,
                related_space_ids: operation.related_space_ids.clone(),
                previous_association_ids: Vec::new(),
                origin: ContextSpaceAssociationOrigin::CandidateConfirmation {
                    candidate_id: candidate.candidate_id,
                },
            })?;
        let publication = Publication::from_draft(PublicationDraft {
            previous_publication_ids: Vec::new(),
            action: PublicationAction::Publish,
            revision_id: result_revision.revision_id,
            review_event_ids: Vec::new(),
        })?;
        let mut opened_conflicts = Vec::with_capacity(conflict_openings.len());
        let mut conflict_targets = Vec::with_capacity(conflict_openings.len());
        for opening in conflict_openings {
            if opening.target.context_id == result_context_id
                || opening.target.publication_id == publication.publication_id
            {
                return Err(invalid(
                    "Candidate Confirmation conflict target must be a different accepted Context",
                ));
            }
            if conflict_targets.contains(&opening.target.context_id) {
                return Err(invalid(
                    "Candidate Confirmation conflict targets must not contain duplicates",
                ));
            }
            conflict_targets.push(opening.target.context_id);
            opened_conflicts.push(SemanticConflictOpening {
                space_id: primary_space_id,
                conflict: SemanticConflict::from_draft(SemanticConflictDraft {
                    participants: vec![
                        ConflictParticipant {
                            context_id: result_context_id,
                            revision_id: result_revision.revision_id,
                            publication_id: publication.publication_id,
                        },
                        opening.target,
                    ],
                    reason: opening.reason,
                    applicability: result_revision.applicability.clone(),
                })?,
            });
        }
        let semantic_conflict_event_ids = opened_conflicts
            .iter()
            .map(|_| EventId::new())
            .collect::<Vec<_>>();
        let confirmation = CandidateConfirmation::from_draft(CandidateConfirmationDraft {
            candidate_id: candidate.candidate_id,
            submission_id: candidate.submission_id,
            source_episode: candidate.source_episode,
            result_context_id,
            result_revision_id: result_revision.revision_id,
            primary_space_id,
            related_space_ids: operation.related_space_ids.clone(),
            space_association_id: space_association.association_id,
            publication_id: publication.publication_id,
            created_space_id: new_space.as_ref().map(|space| space.space_id),
            edits: operation.edits.clone(),
            final_content_hash: context_revision_content_hash(&final_draft),
            causal_refs: CandidateConfirmationCausalRefs {
                space_created_event_id,
                context_revision_event_id,
                space_association_event_id,
                publication_event_id,
                engineering_reference_event_ids: engineering_reference_event_ids.clone(),
            },
        })?;
        let plan = Self {
            operation_hash: operation.operation_hash(),
            operation,
            new_space,
            result_context_id,
            result_revision,
            space_association,
            publication,
            engineering_references,
            opened_conflicts,
            confirmation,
            event_ids: CandidateConfirmationPlanEventIds {
                space_created_event_id,
                context_revision_event_id,
                space_association_event_id,
                publication_event_id,
                engineering_reference_event_ids,
                semantic_conflict_event_ids,
                confirmation_event_id,
            },
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Hashes the complete server-owned plan including generated identities.
    ///
    /// # Panics
    ///
    /// Panics only if JSON-backed domain values unexpectedly fail serialization.
    #[must_use]
    pub fn plan_hash(&self) -> String {
        let bytes =
            serde_json::to_vec(self).expect("serializing Candidate Confirmation plan cannot fail");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    /// Returns the exact number of immutable Events materialized by this plan.
    #[must_use]
    pub fn expected_event_count(&self) -> usize {
        4 + usize::from(self.new_space.is_some())
            + self.engineering_references.len()
            + self.opened_conflicts.len()
    }

    /// Validates all local identities and causal references in the reserved closure.
    ///
    /// # Errors
    ///
    /// Rejects any inconsistent generated identity, content hash, or selection fact.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<()> {
        self.operation.validate()?;
        if self.operation_hash != self.operation.operation_hash()
            || self.operation.candidate_id != self.confirmation.candidate_id
            || self.result_context_id != self.confirmation.result_context_id
            || self.result_revision.revision_id != self.confirmation.result_revision_id
            || self.space_association.association_id != self.confirmation.space_association_id
            || self.publication.publication_id != self.confirmation.publication_id
            || self.publication.revision_id != self.result_revision.revision_id
            || self.space_association.context_id != self.result_context_id
            || self.space_association.primary_space_id != self.confirmation.primary_space_id
            || self.space_association.related_space_ids != self.confirmation.related_space_ids
            || self.event_ids.engineering_reference_event_ids
                != self
                    .confirmation
                    .causal_refs
                    .engineering_reference_event_ids
            || self.event_ids.engineering_reference_event_ids.len()
                != self.engineering_references.len()
            || self.event_ids.semantic_conflict_event_ids.len() != self.opened_conflicts.len()
            || context_revision_content_hash(&context_revision_as_draft(&self.result_revision))
                != self.confirmation.final_content_hash
        {
            return Err(invalid(
                "Candidate Confirmation plan fact identities are inconsistent",
            ));
        }
        self.result_revision.validate()?;
        self.space_association.validate()?;
        self.publication.validate()?;
        let mut reference_ids = HashSet::new();
        for (index, reference) in self.engineering_references.iter().enumerate() {
            reference.validate()?;
            if !reference_ids.insert(reference.reference_id)
                || self.engineering_references[..index]
                    .iter()
                    .any(|existing| same_engineering_reference_content(existing, reference))
            {
                return Err(invalid(
                    "Candidate Confirmation Engineering References must be unique",
                ));
            }
        }
        let mut conflict_ids = HashSet::new();
        let mut conflict_targets = HashSet::new();
        for opening in &self.opened_conflicts {
            opening.conflict.validate()?;
            if opening.space_id != self.confirmation.primary_space_id
                || !conflict_ids.insert(opening.conflict.conflict_id)
            {
                return Err(invalid(
                    "Candidate Confirmation opened conflicts must be unique and Primary-scoped",
                ));
            }
            let mut sides = opening.conflict.participants.iter();
            let Some(source) = sides.next() else {
                return Err(invalid(
                    "Candidate Confirmation conflict has no participants",
                ));
            };
            if source.context_id != self.result_context_id
                || source.revision_id != self.result_revision.revision_id
                || source.publication_id != self.publication.publication_id
            {
                return Err(invalid(
                    "Candidate Confirmation conflict must name the confirming revision first",
                ));
            }
            if opening.conflict.applicability != self.result_revision.applicability {
                return Err(invalid(
                    "Candidate Confirmation conflict applicability must be the confirming revision's",
                ));
            }
            for side in sides {
                if !conflict_targets.insert(side.context_id) {
                    return Err(invalid(
                        "Candidate Confirmation conflict targets must not repeat",
                    ));
                }
            }
        }
        self.confirmation.validate()?;
        if self.event_ids.space_created_event_id
            != self.confirmation.causal_refs.space_created_event_id
            || self.event_ids.context_revision_event_id
                != self.confirmation.causal_refs.context_revision_event_id
            || self.event_ids.space_association_event_id
                != self.confirmation.causal_refs.space_association_event_id
            || self.event_ids.publication_event_id
                != self.confirmation.causal_refs.publication_event_id
        {
            return Err(invalid(
                "Candidate Confirmation plan Event identities are inconsistent",
            ));
        }
        match (&self.new_space, self.confirmation.created_space_id) {
            (Some(space), Some(created)) if space.space_id == created => {
                space.intent_revision.validate()?;
            }
            (None, None) => {}
            _ => {
                return Err(invalid(
                    "Candidate Confirmation plan new Space identity is inconsistent",
                ));
            }
        }
        let mut event_ids = vec![
            self.event_ids.context_revision_event_id,
            self.event_ids.space_association_event_id,
            self.event_ids.publication_event_id,
            self.event_ids.confirmation_event_id,
        ];
        if let Some(event_id) = self.event_ids.space_created_event_id {
            event_ids.push(event_id);
        }
        event_ids.extend(
            self.event_ids
                .engineering_reference_event_ids
                .iter()
                .copied(),
        );
        event_ids.extend(self.event_ids.semantic_conflict_event_ids.iter().copied());
        require_unique(&event_ids, "candidate_confirmation_plan.event_ids")
    }
}

fn same_engineering_reference_content(
    left: &EngineeringReference,
    right: &EngineeringReference,
) -> bool {
    left.repository_id == right.repository_id
        && left.artifact_kind == right.artifact_kind
        && left.relation == right.relation
        && left.locator == right.locator
        && left.supports == right.supports
        && left.limitations == right.limitations
}

/// Hashes only authoritative Context draft semantics, excluding generated Revision/Evidence IDs.
///
/// # Panics
///
/// Panics only if serializing the JSON-backed domain draft unexpectedly fails.
#[must_use]
pub fn context_revision_content_hash(draft: &ContextRevisionDraft) -> String {
    let bytes = serde_json::to_vec(draft)
        .expect("serializing authoritative Context revision content cannot fail");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Reconstructs the authoritative draft semantics of an immutable Context revision.
#[must_use]
pub fn context_revision_as_draft(revision: &ContextRevision) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: revision.kind,
        topic_key: revision.topic_key.clone(),
        problem_view: revision.problem_view.clone(),
        statement: revision.statement.clone(),
        rationale: revision.rationale.clone(),
        applicability: revision.applicability.clone(),
        assumptions: revision.assumptions.clone(),
        recheck_when: revision.recheck_when.clone(),
        hints: revision.hints.clone(),
        relations: revision.relations.clone(),
        evidence: revision
            .evidence
            .iter()
            .map(|evidence| EvidenceSnapshotDraft {
                kind: evidence.kind,
                supports: evidence.supports.clone(),
                content: evidence.content.clone(),
                interpretation: evidence.interpretation.clone(),
                limitations: evidence.limitations.clone(),
            })
            .collect(),
    }
}

fn valid_content_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{EvidenceType, TaskId, TaskSessionId, WorkEpisodeId};

    fn content() -> ContextRevisionDraft {
        ContextRevisionDraft {
            problem_view: None,
            hints: Vec::new(),
            kind: ContextKind::Decision,
            topic_key: Some("confirmation/topic".to_owned()),
            statement: "Keep the confirmed behavior".to_owned(),
            rationale: "The Candidate Evidence is self-contained".to_owned(),
            applicability: Applicability {
                domains: vec!["confirmation".to_owned()],
                platforms: Vec::new(),
                conditions: Vec::new(),
            },
            assumptions: vec!["The source remains valid".to_owned()],
            recheck_when: vec!["The contract changes".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "The confirmation fixture passed".to_owned(),
                content: json!({"actual": "passed"}),
                interpretation: "The Candidate may be reviewed".to_owned(),
                limitations: Vec::new(),
            }],
        }
    }

    fn candidate() -> ContextCandidate {
        ContextCandidate::from_verified_submission(
            SubmissionId::new(),
            WorkEpisodeRef {
                episode_id: WorkEpisodeId::new(),
                task_session_id: TaskSessionId::new(),
                task_id: TaskId::new(),
            },
            content(),
        )
        .unwrap()
    }

    fn intent() -> IntentSnapshot {
        IntentSnapshot {
            title: "New confirmation Space".to_owned(),
            problem: "No existing Space owns the requirement".to_owned(),
            desired_outcome: "Govern the confirmed Context".to_owned(),
            in_scope: vec!["Candidate confirmation".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["The Context is published".to_owned()],
            domain_terms: Vec::new(),
        }
    }

    #[test]
    fn existing_or_new_primary_and_field_edits_preserve_unsupplied_draft_content() {
        let candidate = candidate();
        let primary = SpaceId::new();
        let related = vec![SpaceId::new(), SpaceId::new()];
        let existing = CandidateConfirmationRequest {
            candidate_id: candidate.candidate_id,
            primary: CandidatePrimarySelection::Existing { space_id: primary },
            related_space_ids: related,
            edits: OptionalCandidateEdits {
                statement: Some("Edited confirmed behavior".to_owned()),
                topic_key: Some(TopicKeyEdit::Clear),
                ..OptionalCandidateEdits::default()
            },
        };
        let result = existing.final_draft(&candidate).unwrap();
        assert_eq!(result.statement, "Edited confirmed behavior");
        assert_eq!(result.topic_key, None);
        assert_eq!(result.rationale, candidate.content.rationale);
        assert_eq!(result.evidence, candidate.content.evidence);

        let proposed = CandidateConfirmationRequest {
            candidate_id: candidate.candidate_id,
            primary: CandidatePrimarySelection::ProposedNew { intent: intent() },
            related_space_ids: Vec::new(),
            edits: OptionalCandidateEdits::default(),
        };
        assert_eq!(proposed.final_draft(&candidate).unwrap(), candidate.content);
    }

    #[test]
    fn invalid_selection_edits_and_association_causality_are_rejected() {
        let candidate = candidate();
        let primary = SpaceId::new();
        assert!(
            CandidateConfirmationRequest {
                candidate_id: candidate.candidate_id,
                primary: CandidatePrimarySelection::Existing { space_id: primary },
                related_space_ids: vec![primary],
                edits: OptionalCandidateEdits::default(),
            }
            .final_draft(&candidate)
            .is_err()
        );
        assert!(
            OptionalCandidateEdits {
                evidence: Some(Vec::new()),
                ..OptionalCandidateEdits::default()
            }
            .apply(&candidate.content)
            .is_err()
        );
        assert!(
            ContextSpaceAssociation::from_draft(ContextSpaceAssociationDraft {
                context_id: ContextId::new(),
                primary_space_id: primary,
                related_space_ids: Vec::new(),
                previous_association_ids: vec![SpaceAssociationId::new()],
                origin: ContextSpaceAssociationOrigin::CandidateConfirmation {
                    candidate_id: candidate.candidate_id,
                },
            })
            .is_err()
        );
        assert!(
            ContextSpaceAssociation::from_draft(ContextSpaceAssociationDraft {
                context_id: ContextId::new(),
                primary_space_id: primary,
                related_space_ids: Vec::new(),
                previous_association_ids: Vec::new(),
                origin: ContextSpaceAssociationOrigin::Correction,
            })
            .is_err()
        );
    }

    #[test]
    fn content_hash_excludes_generated_revision_and_evidence_ids() {
        let first = ContextRevision::from_draft(Vec::new(), content()).unwrap();
        let second = ContextRevision::from_draft(Vec::new(), content()).unwrap();
        assert_ne!(first.revision_id, second.revision_id);
        assert_ne!(
            first.evidence[0].evidence_id,
            second.evidence[0].evidence_id
        );
        assert_eq!(
            context_revision_content_hash(&context_revision_as_draft(&first)),
            context_revision_content_hash(&context_revision_as_draft(&second))
        );
    }
}
