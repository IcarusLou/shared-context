use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Applicability, CandidateId, ConfirmationId, ContextCandidate, ContextId, ContextKind,
    ContextRelation, ContextRevision, ContextRevisionDraft, Error, ErrorKind, EventId,
    EvidenceSnapshotDraft, IntentSnapshot, PublicationId, Result, RevisionId, SpaceAssociationId,
    SpaceId, SubmissionId, WorkEpisodeRef,
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

/// Exactly one Primary Space choice supplied when confirming a Candidate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidatePrimarySelection {
    Existing { space_id: SpaceId },
    ProposedNew { intent: IntentSnapshot },
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
    pub relations: Option<Vec<ContextRelation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<EvidenceSnapshotDraft>>,
}

impl OptionalCandidateEdits {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmationCausalRefs {
    pub space_created_event_id: Option<EventId>,
    pub context_revision_event_id: EventId,
    pub space_association_event_id: EventId,
    pub publication_event_id: EventId,
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
            causal_refs: self.causal_refs,
        }
        .validate()
    }
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
        statement: revision.statement.clone(),
        rationale: revision.rationale.clone(),
        applicability: revision.applicability.clone(),
        assumptions: revision.assumptions.clone(),
        recheck_when: revision.recheck_when.clone(),
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
