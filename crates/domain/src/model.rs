use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ConflictId, ContextId, ContextRelation, Error, ErrorKind, EventId, EvidenceId, PublicationId,
    ResolutionId, Result, ReviewId, RevisionId,
};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn require_text(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    Ok(())
}

fn require_text_items(values: &[String], field: &str) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        require_text(value, &format!("{field}[{index}]"))?;
    }
    Ok(())
}

fn require_unique<T>(values: &[T], field: &str) -> Result<()>
where
    T: Eq + std::hash::Hash,
{
    let mut seen = HashSet::with_capacity(values.len());
    if values.iter().any(|value| !seen.insert(value)) {
        return Err(invalid(format!("{field} must not contain duplicates")));
    }
    Ok(())
}

/// Complete authoritative snapshot of a `ContextSpace` intent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentSnapshot {
    pub title: String,
    pub problem: String,
    pub desired_outcome: String,
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub acceptance_conditions: Vec<String>,
    pub domain_terms: Vec<String>,
}

impl IntentSnapshot {
    /// Validates required text without consulting any external system.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required intent content is empty.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.title, "intent.title")?;
        require_text(&self.problem, "intent.problem")?;
        require_text(&self.desired_outcome, "intent.desired_outcome")?;
        if self.in_scope.is_empty() {
            return Err(invalid("intent.in_scope must contain at least one item"));
        }
        if self.acceptance_conditions.is_empty() {
            return Err(invalid(
                "intent.acceptance_conditions must contain at least one item",
            ));
        }
        require_text_items(&self.in_scope, "intent.in_scope")?;
        require_text_items(&self.out_of_scope, "intent.out_of_scope")?;
        require_text_items(&self.acceptance_conditions, "intent.acceptance_conditions")?;
        require_text_items(&self.domain_terms, "intent.domain_terms")?;
        Ok(())
    }
}

/// One immutable full Intent revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentRevision {
    pub revision_id: RevisionId,
    pub parent_revision_ids: Vec<RevisionId>,
    pub intent: IntentSnapshot,
    /// True when the server proposed this Space Intent from a Task Working Intent instead of a
    /// human writing it.
    ///
    /// The flag is serialized only when it is true, so every already written event and every
    /// human-authored revision keeps byte-identical encoding, and an event written before the
    /// flag existed reads back as `false`. A human `space intent revise` writes a new revision
    /// without the flag, which is how a provisional Space stops being provisional.
    #[serde(default, skip_serializing_if = "is_not_provisional")]
    pub provisional: bool,
}

/// Keeps a `false` `provisional` flag out of the serialized form. Named for the field so the
/// serde attribute reads as the invariant it protects.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_not_provisional(provisional: &bool) -> bool {
    !*provisional
}

impl IntentRevision {
    /// Validates the snapshot and local parent-list invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for duplicate parents or invalid content.
    pub fn validate(&self) -> Result<()> {
        require_unique(
            &self.parent_revision_ids,
            "intent_revision.parent_revision_ids",
        )?;
        self.intent.validate()
    }
}

/// Supported `ContextItem` categories in V1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Decision,
    Contract,
    Issue,
    Risk,
    Validation,
    Discovery,
    Progress,
}

/// Self-contained applicability snapshot stored with a revision or conflict.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    pub domains: Vec<String>,
    pub platforms: Vec<String>,
    pub conditions: Vec<String>,
}

impl Applicability {
    /// Validates that scope values are meaningful strings.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when a scope entry is empty.
    pub fn validate(&self, field: &str) -> Result<()> {
        require_text_items(&self.domains, &format!("{field}.domains"))?;
        require_text_items(&self.platforms, &format!("{field}.platforms"))?;
        require_text_items(&self.conditions, &format!("{field}.conditions"))?;
        Ok(())
    }
}

/// Supported forms of self-contained evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    SourceSnapshot,
    ExperimentRecord,
    ArtifactSnapshot,
}

/// Caller-authored evidence content before an opaque Evidence ID is assigned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSnapshotDraft {
    pub kind: EvidenceType,
    pub supports: String,
    pub content: Value,
    pub interpretation: String,
    pub limitations: Vec<String>,
}

impl EvidenceSnapshotDraft {
    /// Validates that evidence is self-contained rather than a bare external pointer.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required evidence content is absent.
    pub fn validate(&self, field: &str) -> Result<()> {
        require_text(&self.supports, &format!("{field}.supports"))?;
        require_text(&self.interpretation, &format!("{field}.interpretation"))?;
        require_text_items(&self.limitations, &format!("{field}.limitations"))?;
        match &self.content {
            Value::Object(content) if !content.is_empty() => Ok(()),
            _ => Err(invalid(format!(
                "{field}.content must be a non-empty JSON object"
            ))),
        }
    }
}

/// Immutable evidence snapshot embedded in a Context revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSnapshot {
    pub evidence_id: EvidenceId,
    pub kind: EvidenceType,
    pub supports: String,
    pub content: Value,
    pub interpretation: String,
    pub limitations: Vec<String>,
}

impl EvidenceSnapshot {
    pub(crate) fn from_draft(draft: EvidenceSnapshotDraft) -> Self {
        Self {
            evidence_id: EvidenceId::new(),
            kind: draft.kind,
            supports: draft.supports,
            content: draft.content,
            interpretation: draft.interpretation,
            limitations: draft.limitations,
        }
    }

    /// Validates the authoritative evidence content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required evidence content is absent.
    pub fn validate(&self, field: &str) -> Result<()> {
        EvidenceSnapshotDraft {
            kind: self.kind,
            supports: self.supports.clone(),
            content: self.content.clone(),
            interpretation: self.interpretation.clone(),
            limitations: self.limitations.clone(),
        }
        .validate(field)
    }
}

/// Caller-authored Context content before generated revision and evidence IDs are assigned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRevisionDraft {
    pub kind: ContextKind,
    pub topic_key: Option<String>,
    /// Optional restatement of the problem this Context answers, used for retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem_view: Option<String>,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    /// Unresolved locator hints (paths, basenames, identifiers) kept as searchable text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    pub relations: Vec<ContextRelation>,
    pub evidence: Vec<EvidenceSnapshotDraft>,
}

impl ContextRevisionDraft {
    /// Validates authoritative Context content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required content or evidence is absent.
    pub fn validate(&self) -> Result<()> {
        if let Some(topic_key) = &self.topic_key {
            require_text(topic_key, "context revision topic_key")?;
        }
        if let Some(problem_view) = &self.problem_view {
            require_text(problem_view, "context revision problem_view")?;
        }
        require_text(&self.statement, "context revision statement")?;
        require_text(&self.rationale, "context revision rationale")?;
        self.applicability
            .validate("context revision applicability")?;
        require_text_items(&self.assumptions, "context revision assumptions")?;
        require_text_items(&self.recheck_when, "context revision recheck_when")?;
        require_text_items(&self.hints, "context revision hints")?;
        for (index, relation) in self.relations.iter().enumerate() {
            relation.validate().map_err(|error| {
                invalid(format!(
                    "context revision relations[{index}] is invalid: {error}"
                ))
            })?;
        }
        require_unique(
            &self
                .relations
                .iter()
                .map(|relation| (relation.target_context_id, relation.kind))
                .collect::<Vec<_>>(),
            "context revision relations target/kind",
        )?;
        if self.evidence.is_empty() {
            return Err(invalid(
                "context revision evidence must contain at least one snapshot",
            ));
        }
        for (index, evidence) in self.evidence.iter().enumerate() {
            evidence.validate(&format!("context revision evidence[{index}]"))?;
        }
        Ok(())
    }
}

/// One immutable full Context revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRevision {
    pub revision_id: RevisionId,
    pub parent_revision_ids: Vec<RevisionId>,
    pub kind: ContextKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_key: Option<String>,
    /// Optional restatement of the problem this Context answers, used for retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem_view: Option<String>,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    /// Unresolved locator hints (paths, basenames, identifiers) kept as searchable text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    pub relations: Vec<ContextRelation>,
    pub evidence: Vec<EvidenceSnapshot>,
}

impl ContextRevision {
    /// Assigns fresh revision and evidence IDs to validated content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for invalid content or duplicate parents.
    pub fn from_draft(
        parent_revision_ids: Vec<RevisionId>,
        draft: ContextRevisionDraft,
    ) -> Result<Self> {
        draft.validate()?;
        require_unique(&parent_revision_ids, "revision.parent_revision_ids")?;
        Ok(Self {
            revision_id: RevisionId::new(),
            parent_revision_ids,
            kind: draft.kind,
            topic_key: draft.topic_key,
            problem_view: draft.problem_view,
            statement: draft.statement,
            rationale: draft.rationale,
            applicability: draft.applicability,
            assumptions: draft.assumptions,
            recheck_when: draft.recheck_when,
            hints: draft.hints,
            relations: draft.relations,
            evidence: draft
                .evidence
                .into_iter()
                .map(EvidenceSnapshot::from_draft)
                .collect(),
        })
    }

    /// Validates parsed revision content and local uniqueness.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for invalid content or duplicate IDs.
    pub fn validate(&self) -> Result<()> {
        require_unique(&self.parent_revision_ids, "revision.parent_revision_ids")?;
        require_unique(
            &self
                .evidence
                .iter()
                .map(|evidence| evidence.evidence_id)
                .collect::<Vec<_>>(),
            "revision.evidence evidence_id",
        )?;
        ContextRevisionDraft {
            kind: self.kind,
            topic_key: self.topic_key.clone(),
            problem_view: self.problem_view.clone(),
            statement: self.statement.clone(),
            rationale: self.rationale.clone(),
            applicability: self.applicability.clone(),
            assumptions: self.assumptions.clone(),
            recheck_when: self.recheck_when.clone(),
            hints: self.hints.clone(),
            relations: self.relations.clone(),
            evidence: self
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
        .validate()
    }
}

/// Immutable review conclusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    Reject,
}

/// Review input before a Review ID is assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewDraft {
    pub revision_id: RevisionId,
    pub verdict: ReviewVerdict,
    pub reason: String,
}

/// Immutable review payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub review_id: ReviewId,
    pub revision_id: RevisionId,
    pub verdict: ReviewVerdict,
    pub reason: String,
}

impl Review {
    /// Assigns a fresh Review ID.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the review reason is empty.
    pub fn from_draft(draft: ReviewDraft) -> Result<Self> {
        require_text(&draft.reason, "review.reason")?;
        Ok(Self {
            review_id: ReviewId::new(),
            revision_id: draft.revision_id,
            verdict: draft.verdict,
            reason: draft.reason,
        })
    }

    /// Validates parsed review content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the review reason is empty.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.reason, "review.reason")
    }
}

/// V1 publication actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationAction {
    Publish,
    Withdraw,
}

/// Publication input before a Publication ID is assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationDraft {
    pub previous_publication_ids: Vec<PublicationId>,
    pub action: PublicationAction,
    pub revision_id: RevisionId,
    pub review_event_ids: Vec<EventId>,
}

/// Immutable publication transition payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub publication_id: PublicationId,
    pub previous_publication_ids: Vec<PublicationId>,
    pub action: PublicationAction,
    pub revision_id: RevisionId,
    pub review_event_ids: Vec<EventId>,
}

impl Publication {
    /// Assigns a fresh Publication ID.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when a causal or review ID is duplicated.
    pub fn from_draft(draft: PublicationDraft) -> Result<Self> {
        require_unique(
            &draft.previous_publication_ids,
            "publication.previous_publication_ids",
        )?;
        require_unique(&draft.review_event_ids, "publication.review_event_ids")?;
        Ok(Self {
            publication_id: PublicationId::new(),
            previous_publication_ids: draft.previous_publication_ids,
            action: draft.action,
            revision_id: draft.revision_id,
            review_event_ids: draft.review_event_ids,
        })
    }

    /// Validates parsed publication content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when a causal or review ID is duplicated.
    pub fn validate(&self) -> Result<()> {
        require_unique(
            &self.previous_publication_ids,
            "publication.previous_publication_ids",
        )?;
        require_unique(&self.review_event_ids, "publication.review_event_ids")
    }
}

/// One accepted Context/Revision/Publication head involved in a semantic conflict.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictParticipant {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub publication_id: PublicationId,
}

/// Conflict input before a Conflict ID is assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticConflictDraft {
    pub participants: Vec<ConflictParticipant>,
    pub reason: String,
    pub applicability: Applicability,
}

/// Immutable semantic-conflict declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConflict {
    pub conflict_id: ConflictId,
    pub participants: Vec<ConflictParticipant>,
    pub reason: String,
    pub applicability: Applicability,
}

impl SemanticConflict {
    /// Assigns a fresh Conflict ID.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an incomplete or duplicate participant set.
    pub fn from_draft(draft: SemanticConflictDraft) -> Result<Self> {
        Self::validate_parts(&draft.participants, &draft.reason, &draft.applicability)?;
        Ok(Self {
            conflict_id: ConflictId::new(),
            participants: draft.participants,
            reason: draft.reason,
            applicability: draft.applicability,
        })
    }

    fn validate_parts(
        participants: &[ConflictParticipant],
        reason: &str,
        applicability: &Applicability,
    ) -> Result<()> {
        if participants.len() < 2 {
            return Err(invalid(
                "semantic conflict must contain at least two participants",
            ));
        }
        require_unique(participants, "semantic_conflict.participants")?;
        require_unique(
            &participants
                .iter()
                .map(|participant| participant.context_id)
                .collect::<Vec<_>>(),
            "semantic_conflict participant context_id",
        )?;
        require_unique(
            &participants
                .iter()
                .map(|participant| participant.publication_id)
                .collect::<Vec<_>>(),
            "semantic_conflict participant publication_id",
        )?;
        require_text(reason, "semantic_conflict.reason")?;
        applicability.validate("semantic_conflict.applicability")
    }

    /// Validates parsed conflict content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an incomplete or duplicate participant set.
    pub fn validate(&self) -> Result<()> {
        Self::validate_parts(&self.participants, &self.reason, &self.applicability)
    }
}

/// How one Context participates in a semantic-conflict resolution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    Retained,
    Revised,
    Withdrawn,
    ScopeSplit,
}

/// Explicit result for one Context and revision.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictResolutionResult {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub outcome: ResolutionOutcome,
}

/// Resolution input before a Resolution ID is assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictResolutionDraft {
    pub previous_resolution_ids: Vec<ResolutionId>,
    pub related_publication_ids: Vec<PublicationId>,
    pub results: Vec<ConflictResolutionResult>,
    pub rationale: String,
}

/// Immutable semantic-conflict resolution node.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictResolution {
    pub resolution_id: ResolutionId,
    pub previous_resolution_ids: Vec<ResolutionId>,
    pub related_publication_ids: Vec<PublicationId>,
    pub results: Vec<ConflictResolutionResult>,
    pub rationale: String,
}

impl ConflictResolution {
    /// Assigns a fresh Resolution ID.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for missing or duplicate resolution references.
    pub fn from_draft(draft: ConflictResolutionDraft) -> Result<Self> {
        Self::validate_parts(
            &draft.previous_resolution_ids,
            &draft.related_publication_ids,
            &draft.results,
            &draft.rationale,
        )?;
        Ok(Self {
            resolution_id: ResolutionId::new(),
            previous_resolution_ids: draft.previous_resolution_ids,
            related_publication_ids: draft.related_publication_ids,
            results: draft.results,
            rationale: draft.rationale,
        })
    }

    fn validate_parts(
        previous_resolution_ids: &[ResolutionId],
        related_publication_ids: &[PublicationId],
        results: &[ConflictResolutionResult],
        rationale: &str,
    ) -> Result<()> {
        require_unique(
            previous_resolution_ids,
            "resolution.previous_resolution_ids",
        )?;
        if related_publication_ids.is_empty() {
            return Err(invalid(
                "resolution.related_publication_ids must contain at least one head",
            ));
        }
        require_unique(
            related_publication_ids,
            "resolution.related_publication_ids",
        )?;
        if results.is_empty() {
            return Err(invalid(
                "resolution.results must contain at least one result",
            ));
        }
        require_unique(results, "resolution.results")?;
        require_unique(
            &results
                .iter()
                .map(|result| result.context_id)
                .collect::<Vec<_>>(),
            "resolution result context_id",
        )?;
        require_text(rationale, "resolution.rationale")
    }

    /// Validates parsed resolution content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for missing or duplicate resolution references.
    pub fn validate(&self) -> Result<()> {
        Self::validate_parts(
            &self.previous_resolution_ids,
            &self.related_publication_ids,
            &self.results,
            &self.rationale,
        )
    }
}
