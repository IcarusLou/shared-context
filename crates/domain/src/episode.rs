use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{
    CandidateId, ContextId, ContextRevisionDraft, Error, ErrorKind, EvidenceSnapshotDraft, Result,
    TaskId, TaskIntent, TaskSignal, WorkEpisodeId,
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
    let mut seen = HashSet::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        require_text(value, &format!("{field}[{index}]"))?;
        if !seen.insert(value) {
            return Err(invalid(format!("{field} must not contain duplicates")));
        }
    }
    Ok(())
}

/// How a previously retrieved Context influenced the current Task.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextUseDisposition {
    Applied,
    Ignored,
}

/// How the Agent interacted with an engineering artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAction {
    Inspected,
    Modified,
}

/// One normalized observation from an Agent's work process.
///
/// Observations retain engineering meaning rather than raw tool activity. Raw
/// transcripts and complete tool output are deliberately outside this model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkObservation {
    ContextUse {
        context_id: ContextId,
        disposition: ContextUseDisposition,
        reason: String,
    },
    Artifact {
        artifact_ref: String,
        action: ArtifactAction,
        summary: String,
    },
    Diff {
        summary: String,
        artifact_refs: Vec<String>,
    },
    Interface {
        interface_ref: String,
        observation: String,
    },
    Validation {
        conclusion: String,
        evidence: Vec<EvidenceSnapshotDraft>,
    },
    AgentCheckpoint {
        claims: Vec<ContextRevisionDraft>,
        unknowns: Vec<String>,
    },
    UnresolvedQuestion {
        question: String,
    },
}

impl WorkObservation {
    /// Validates that normalized observations preserve their engineering meaning.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when meaningful text, validation
    /// evidence, or checkpoint content is absent.
    pub fn validate(&self, field: &str) -> Result<()> {
        match self {
            Self::ContextUse { reason, .. } => require_text(reason, &format!("{field}.reason")),
            Self::Artifact {
                artifact_ref,
                summary,
                ..
            } => {
                require_text(artifact_ref, &format!("{field}.artifact_ref"))?;
                require_text(summary, &format!("{field}.summary"))
            }
            Self::Diff {
                summary,
                artifact_refs,
            } => {
                require_text(summary, &format!("{field}.summary"))?;
                require_text_items(artifact_refs, &format!("{field}.artifact_refs"))
            }
            Self::Interface {
                interface_ref,
                observation,
            } => {
                require_text(interface_ref, &format!("{field}.interface_ref"))?;
                require_text(observation, &format!("{field}.observation"))
            }
            Self::Validation {
                conclusion,
                evidence,
            } => {
                require_text(conclusion, &format!("{field}.conclusion"))?;
                if evidence.is_empty() {
                    return Err(invalid(format!(
                        "{field}.evidence must contain at least one snapshot"
                    )));
                }
                for (index, snapshot) in evidence.iter().enumerate() {
                    snapshot.validate(&format!("{field}.evidence[{index}]"))?;
                }
                Ok(())
            }
            Self::AgentCheckpoint { claims, unknowns } => {
                if claims.is_empty() && unknowns.is_empty() {
                    return Err(invalid(format!(
                        "{field} must contain at least one claim or unknown"
                    )));
                }
                for (index, claim) in claims.iter().enumerate() {
                    claim.validate().map_err(|error| {
                        invalid(format!("{field}.claims[{index}]: {}", error.message()))
                    })?;
                }
                require_text_items(unknowns, &format!("{field}.unknowns"))
            }
            Self::UnresolvedQuestion { question } => {
                require_text(question, &format!("{field}.question"))
            }
        }
    }
}

/// Task-local aggregation of intent, signals, and normalized work observations.
///
/// A Work Episode is runtime state, not durable Context knowledge. Its Task
/// Intent carries the sole Task identity and cannot carry a Space route.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEpisode {
    pub episode_id: WorkEpisodeId,
    pub task_intent: TaskIntent,
    pub task_signals: Vec<TaskSignal>,
    pub observations: Vec<WorkObservation>,
}

impl WorkEpisode {
    /// Creates one validated Work Episode with a fresh identity.
    ///
    /// Empty signal and observation collections are valid for a provisional
    /// episode; populated entries must be meaningful and internally valid.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the Task Intent, a Task Signal,
    /// or a structured observation is invalid.
    pub fn from_parts(
        task_intent: TaskIntent,
        task_signals: Vec<TaskSignal>,
        observations: Vec<WorkObservation>,
    ) -> Result<Self> {
        let episode = Self {
            episode_id: WorkEpisodeId::new(),
            task_intent,
            task_signals,
            observations,
        };
        episode.validate()?;
        Ok(episode)
    }

    /// Returns the Task whose work this episode aggregates.
    #[must_use]
    pub const fn task_id(&self) -> TaskId {
        self.task_intent.task_id
    }

    /// Validates parsed Work Episode content.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when nested Task data or an
    /// observation is invalid.
    pub fn validate(&self) -> Result<()> {
        self.task_intent.validate()?;
        TaskSignal::validate_collection(&self.task_signals)?;
        for (index, observation) in self.observations.iter().enumerate() {
            observation.validate(&format!("work_episode.observations[{index}]"))?;
        }
        Ok(())
    }
}

/// Governable Context content extracted from one Work Episode.
///
/// A Context Candidate is an unconfirmed draft, not an accepted Context revision.
/// It intentionally contains no Space route or Space recommendation. Derived
/// recommendations must be represented separately and never imply ownership.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCandidate {
    pub candidate_id: CandidateId,
    pub source_episode_id: WorkEpisodeId,
    pub content: ContextRevisionDraft,
}

impl ContextCandidate {
    /// Creates a validated, unowned Context Candidate from a Work Episode.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the source episode or governable
    /// Context content is invalid. In particular, statement, rationale, and
    /// self-contained evidence are required by [`ContextRevisionDraft`].
    pub fn from_episode(episode: &WorkEpisode, content: ContextRevisionDraft) -> Result<Self> {
        episode.validate()?;
        content.validate()?;
        Ok(Self {
            candidate_id: CandidateId::new(),
            source_episode_id: episode.episode_id,
            content,
        })
    }

    /// Validates candidate content without assigning governance or lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the governable content is invalid.
    pub fn validate(&self) -> Result<()> {
        self.content.validate()
    }

    /// Validates that this Candidate names the supplied source episode.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when either source identity differs or
    /// the Candidate/Episode content is invalid.
    pub fn validate_against_episode(&self, episode: &WorkEpisode) -> Result<()> {
        self.validate()?;
        episode.validate()?;
        if self.source_episode_id != episode.episode_id {
            return Err(invalid(
                "context_candidate.source_episode_id must identify the supplied episode",
            ));
        }
        Ok(())
    }

    /// Candidates are unconfirmed runtime drafts and are never auto-injectable.
    #[must_use]
    pub const fn is_auto_injection_eligible(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{ArtifactAction, ContextCandidate, WorkEpisode, WorkObservation};
    use crate::{
        Applicability, ContextKind, ContextRevisionDraft, ErrorKind, EvidenceSnapshotDraft,
        EvidenceType, TaskId, TaskIntent, TaskSignal, TaskSignalKind, WorkEpisodeId,
    };

    fn intent() -> TaskIntent {
        TaskIntent {
            task_id: TaskId::new(),
            goal: "Capture a compatibility decision".to_owned(),
            desired_change: "Preserve the server-owned fallback".to_owned(),
            in_scope: vec!["search response handling".to_owned()],
            out_of_scope: vec![],
            domains: vec!["search".to_owned()],
            platforms: vec!["fe".to_owned()],
            constraints: vec![],
            acceptance_conditions: vec!["old clients remain compatible".to_owned()],
            artifacts: vec!["symbol:SearchResult".to_owned()],
            interfaces: vec!["api:search-v2".to_owned()],
            unknowns: vec![],
        }
    }

    fn evidence() -> EvidenceSnapshotDraft {
        EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "The server controls fallback semantics".to_owned(),
            content: json!({"test": "compatibility", "result": "passed"}),
            interpretation: "Changing the client fallback would break old responses".to_owned(),
            limitations: vec!["Only covers search-v2".to_owned()],
        }
    }

    fn content() -> ContextRevisionDraft {
        ContextRevisionDraft {
            kind: ContextKind::Decision,
            topic_key: Some("search-fallback-owner".to_owned()),
            statement: "Keep fallback selection server-owned".to_owned(),
            rationale: "Existing clients consume the server decision".to_owned(),
            applicability: Applicability {
                domains: vec!["search".to_owned()],
                platforms: vec!["fe".to_owned(), "ios".to_owned()],
                conditions: vec!["search-v2 responses".to_owned()],
            },
            assumptions: vec!["The response contract remains versioned".to_owned()],
            recheck_when: vec!["search-v3 removes the fallback field".to_owned()],
            evidence: vec![evidence()],
        }
    }

    fn episode() -> WorkEpisode {
        WorkEpisode::from_parts(
            intent(),
            vec![TaskSignal {
                kind: TaskSignalKind::File,
                content: "src/SearchResult.tsx".to_owned(),
            }],
            vec![WorkObservation::Artifact {
                artifact_ref: "symbol:SearchResult".to_owned(),
                action: ArtifactAction::Inspected,
                summary: "The component consumes the server fallback".to_owned(),
            }],
        )
        .expect("valid Work Episode")
    }

    #[test]
    fn work_episode_aggregates_one_intent_signals_and_structured_observations() {
        let episode = episode();

        assert_eq!(episode.task_id(), episode.task_intent.task_id);
        assert_eq!(episode.task_signals.len(), 1);
        assert_eq!(episode.observations.len(), 1);
        assert!(episode.validate().is_ok());
    }

    #[test]
    fn structured_observations_reject_action_logs_without_engineering_meaning() {
        let observation = WorkObservation::Artifact {
            artifact_ref: "src/SearchResult.tsx".to_owned(),
            action: ArtifactAction::Modified,
            summary: "  ".to_owned(),
        };

        let error = observation
            .validate("observation")
            .expect_err("empty engineering summary must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.message().contains("observation.summary"));
    }

    #[test]
    fn validation_observation_requires_self_contained_evidence() {
        let observation = WorkObservation::Validation {
            conclusion: "The compatibility test passes".to_owned(),
            evidence: vec![],
        };

        let error = observation
            .validate("observation")
            .expect_err("evidence-free validation must fail");

        assert!(error.message().contains("at least one snapshot"));
    }

    #[test]
    fn candidate_is_valid_without_a_space_and_never_auto_injectable() {
        let episode = episode();
        let candidate =
            ContextCandidate::from_episode(&episode, content()).expect("valid Candidate");

        assert!(candidate.validate_against_episode(&episode).is_ok());
        assert!(!candidate.is_auto_injection_eligible());

        let serialized = serde_json::to_value(candidate).expect("serialize Candidate");
        assert!(serialized.get("space_id").is_none());
        assert!(serialized.get("space_candidates").is_none());
        assert!(serialized.get("status").is_none());
    }

    #[test]
    fn candidate_rejects_missing_statement_rationale_or_evidence() {
        let episode = episode();

        let mut missing_statement = content();
        missing_statement.statement = " ".to_owned();
        assert!(
            ContextCandidate::from_episode(&episode, missing_statement)
                .expect_err("missing statement must fail")
                .message()
                .contains("statement")
        );

        let mut missing_rationale = content();
        missing_rationale.rationale.clear();
        assert!(
            ContextCandidate::from_episode(&episode, missing_rationale)
                .expect_err("missing rationale must fail")
                .message()
                .contains("rationale")
        );

        let mut missing_evidence = content();
        missing_evidence.evidence.clear();
        assert!(
            ContextCandidate::from_episode(&episode, missing_evidence)
                .expect_err("missing evidence must fail")
                .message()
                .contains("evidence")
        );
    }

    #[test]
    fn candidate_requires_a_source_episode_in_serialized_form() {
        let episode = episode();
        let candidate =
            ContextCandidate::from_episode(&episode, content()).expect("valid Candidate");
        let mut serialized = serde_json::to_value(candidate).expect("serialize Candidate");
        serialized
            .as_object_mut()
            .expect("Candidate object")
            .remove("source_episode_id");

        let parsed = serde_json::from_value::<ContextCandidate>(serialized);

        assert!(parsed.is_err());
    }

    #[test]
    fn candidate_rejects_a_different_source_episode() {
        let episode = episode();
        let candidate =
            ContextCandidate::from_episode(&episode, content()).expect("valid Candidate");
        let mut different_episode = episode.clone();
        different_episode.episode_id = WorkEpisodeId::new();

        let error = candidate
            .validate_against_episode(&different_episode)
            .expect_err("mismatched source Episode must fail");

        assert!(error.message().contains("source_episode_id"));
    }

    #[test]
    fn candidate_schema_rejects_embedded_space_ownership() {
        let episode = episode();
        let candidate =
            ContextCandidate::from_episode(&episode, content()).expect("valid Candidate");
        let mut serialized = serde_json::to_value(candidate).expect("serialize Candidate");
        serialized
            .as_object_mut()
            .expect("Candidate object")
            .insert(
                "space_id".to_owned(),
                Value::String("spc_not-owned".to_owned()),
            );

        assert!(serde_json::from_value::<ContextCandidate>(serialized).is_err());
    }
}
