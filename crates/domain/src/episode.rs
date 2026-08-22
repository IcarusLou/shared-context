use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    AgentCheckpointId, Applicability, ArtifactLocator, CandidateBuildId, CandidateId, CaptureId,
    CheckpointClaimId, ContextId, ContextRelationKind, ContextRevisionDraft, Error, ErrorKind,
    EvidenceId, EvidenceSnapshotDraft, IntentSnapshot, RepositoryId, Result, RevisionId, SignalId,
    SpaceId, SubmissionId, TaskId, TaskIntentRevisionId, TaskSessionId, TaskSignalKind,
    WorkEpisodeId, WorkObservationId,
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

fn require_unique<T>(values: &[T], field: &str) -> Result<()>
where
    T: Eq + std::hash::Hash,
{
    if values.iter().collect::<HashSet<_>>().len() != values.len() {
        return Err(invalid(format!("{field} must not contain duplicates")));
    }
    Ok(())
}

/// Closed, ordered range of Task Intent revisions observed by one Work Episode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentRevisionRange {
    pub revision_ids: Vec<TaskIntentRevisionId>,
}

impl IntentRevisionRange {
    /// Creates a non-empty ordered Intent revision range.
    ///
    /// # Errors
    ///
    /// Returns an input error for an empty or duplicate range.
    pub fn new(revision_ids: Vec<TaskIntentRevisionId>) -> Result<Self> {
        let range = Self { revision_ids };
        range.validate()?;
        Ok(range)
    }

    #[must_use]
    pub fn contains(&self, revision_id: TaskIntentRevisionId) -> bool {
        self.revision_ids.contains(&revision_id)
    }

    #[must_use]
    pub fn first(&self) -> TaskIntentRevisionId {
        self.revision_ids[0]
    }

    #[must_use]
    pub fn last(&self) -> TaskIntentRevisionId {
        self.revision_ids[self.revision_ids.len() - 1]
    }

    fn validate(&self) -> Result<()> {
        if self.revision_ids.is_empty() {
            return Err(invalid(
                "intent_revision_range.revision_ids must not be empty",
            ));
        }
        require_unique(&self.revision_ids, "intent_revision_range.revision_ids")
    }
}

/// Verifiable ownership tuple for one Work Episode.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEpisodeRef {
    pub episode_id: WorkEpisodeId,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
}

/// Reference to one non-locating Task Signal without retaining its raw content.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NonLocatingSignalRef {
    pub signal_id: SignalId,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub kind: TaskSignalKind,
}

impl NonLocatingSignalRef {
    fn validate_owner(&self, task_session_id: TaskSessionId, task_id: TaskId) -> Result<()> {
        if self.task_session_id != task_session_id || self.task_id != task_id {
            return Err(invalid(
                "non_locating_signal_ref must belong to the Work Episode Task",
            ));
        }
        Ok(())
    }
}

/// Stable repository-scoped Artifact coordinates used by Capture contracts.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub repository_id: RepositoryId,
    pub locator: ArtifactLocator,
}

/// Exact Task ownership carried by one redacted Capture source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSourceRef {
    pub capture_id: CaptureId,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
}

impl CaptureSourceRef {
    fn validate_owner(&self, task_session_id: TaskSessionId, task_id: TaskId) -> Result<()> {
        if self.task_session_id != task_session_id || self.task_id != task_id {
            return Err(invalid(
                "capture_source_ref must belong to the Work Episode Task",
            ));
        }
        Ok(())
    }
}

impl ArtifactRef {
    fn validate(&self) -> Result<()> {
        self.locator.validate()
    }
}

/// Immutable Context revision coordinates used as provenance or relationship targets.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRevisionRef {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
}

/// Typed Evidence provenance that never contains raw payload text.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureEvidenceRef {
    Observation {
        observation_id: WorkObservationId,
    },
    TaskSignal {
        signal_id: SignalId,
    },
    ContextEvidence {
        context_id: ContextId,
        revision_id: RevisionId,
        evidence_id: EvidenceId,
    },
}

/// Typed source coordinates for one normalized Work Observation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkSourceRef {
    Capture(CaptureSourceRef),
    TaskSignal(NonLocatingSignalRef),
    Artifact(ArtifactRef),
    ContextRevision(ContextRevisionRef),
    ContextEvidence {
        context_id: ContextId,
        revision_id: RevisionId,
        evidence_id: EvidenceId,
    },
}

impl WorkSourceRef {
    fn validate_owner(&self, task_session_id: TaskSessionId, task_id: TaskId) -> Result<()> {
        match self {
            Self::Capture(capture) => capture.validate_owner(task_session_id, task_id),
            Self::TaskSignal(signal) => signal.validate_owner(task_session_id, task_id),
            Self::Artifact(artifact) => artifact.validate(),
            Self::ContextRevision(_) | Self::ContextEvidence { .. } => Ok(()),
        }
    }
}

/// How a previously retrieved Context influenced the current Task.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextUseDisposition {
    Applied,
    Ignored,
}

/// How the Agent interacted with one Artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAction {
    Inspected,
    Modified,
}

/// Bounded normalized Breadcrumb category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedBreadcrumbKind {
    Exploration,
    Implementation,
    Validation,
    Decision,
}

/// Normalized test result without raw tool output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcomeStatus {
    Passed,
    Failed,
    Inconclusive,
}

/// Engineering meaning extracted from work, deliberately excluding raw Agent payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NormalizedWorkObservation {
    Breadcrumb {
        category: NormalizedBreadcrumbKind,
        summary: String,
    },
    TestOutcome {
        test_name: String,
        status: TestOutcomeStatus,
        summary: String,
    },
    Diff {
        summary: String,
        artifact_refs: Vec<ArtifactRef>,
    },
    ContextUse {
        context: ContextRevisionRef,
        disposition: ContextUseDisposition,
        reason: String,
    },
    Artifact {
        artifact: ArtifactRef,
        action: ArtifactAction,
        summary: String,
    },
    Interface {
        interface: ArtifactRef,
        observation: String,
    },
    Validation {
        conclusion: String,
        evidence_refs: Vec<CaptureEvidenceRef>,
    },
    InlineValidation {
        evidence: EvidenceSnapshotDraft,
    },
    UnresolvedQuestion {
        question: String,
    },
}

impl NormalizedWorkObservation {
    fn validate(&self, field: &str) -> Result<()> {
        match self {
            Self::Breadcrumb { summary, .. } => require_text(summary, &format!("{field}.summary")),
            Self::TestOutcome {
                test_name, summary, ..
            } => {
                require_text(test_name, &format!("{field}.test_name"))?;
                require_text(summary, &format!("{field}.summary"))
            }
            Self::Diff {
                summary,
                artifact_refs,
            } => {
                require_text(summary, &format!("{field}.summary"))?;
                require_unique(artifact_refs, &format!("{field}.artifact_refs"))?;
                for artifact in artifact_refs {
                    artifact.validate()?;
                }
                Ok(())
            }
            Self::ContextUse { reason, .. } => require_text(reason, &format!("{field}.reason")),
            Self::Artifact {
                artifact, summary, ..
            } => {
                artifact.validate()?;
                require_text(summary, &format!("{field}.summary"))
            }
            Self::Interface {
                interface,
                observation,
            } => {
                interface.validate()?;
                if !matches!(
                    interface.locator.kind(),
                    crate::ArtifactKind::Api | crate::ArtifactKind::Schema
                ) {
                    return Err(invalid(
                        "normalized interface observation requires an API or Schema Artifact",
                    ));
                }
                require_text(observation, &format!("{field}.observation"))
            }
            Self::Validation {
                conclusion,
                evidence_refs,
            } => {
                require_text(conclusion, &format!("{field}.conclusion"))?;
                if evidence_refs.is_empty() {
                    return Err(invalid(format!("{field}.evidence_refs must not be empty")));
                }
                require_unique(evidence_refs, &format!("{field}.evidence_refs"))
            }
            Self::InlineValidation { evidence } => evidence.validate(&format!("{field}.evidence")),
            Self::UnresolvedQuestion { question } => {
                require_text(question, &format!("{field}.question"))
            }
        }
    }
}

/// One server-identified normalized observation with explicit Task and Intent ownership.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkObservation {
    pub observation_id: WorkObservationId,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub source_refs: Vec<WorkSourceRef>,
    pub observation: NormalizedWorkObservation,
}

impl WorkObservation {
    /// Creates a normalized observation with a server-owned ID.
    ///
    /// # Errors
    ///
    /// Returns an input error for missing sources or incomplete engineering meaning.
    pub fn from_parts(
        task_session_id: TaskSessionId,
        task_id: TaskId,
        intent_revision_id: TaskIntentRevisionId,
        source_refs: Vec<WorkSourceRef>,
        observation: NormalizedWorkObservation,
    ) -> Result<Self> {
        let value = Self {
            observation_id: WorkObservationId::new(),
            task_session_id,
            task_id,
            intent_revision_id,
            source_refs,
            observation,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        if self.source_refs.is_empty()
            && !matches!(
                self.observation,
                NormalizedWorkObservation::InlineValidation { .. }
            )
        {
            return Err(invalid("work_observation.source_refs must not be empty"));
        }
        require_unique(&self.source_refs, "work_observation.source_refs")?;
        for source in &self.source_refs {
            source.validate_owner(self.task_session_id, self.task_id)?;
        }
        self.observation.validate("work_observation.observation")
    }
}

/// Explicit lifecycle boundary of one Work Episode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkEpisodeStatus {
    Open,
    Closed {
        final_checkpoint_id: AgentCheckpointId,
    },
}

/// Task-local aggregation of typed references and normalized observations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEpisode {
    pub episode_id: WorkEpisodeId,
    pub version: u64,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revisions: IntentRevisionRange,
    pub signal_refs: Vec<NonLocatingSignalRef>,
    pub observations: Vec<WorkObservation>,
    pub status: WorkEpisodeStatus,
}

impl WorkEpisode {
    /// Opens a provisional Episode with server-owned identity.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid Intent range or cross-Task Signal reference.
    pub fn open(
        task_session_id: TaskSessionId,
        task_id: TaskId,
        intent_revisions: IntentRevisionRange,
        signal_refs: Vec<NonLocatingSignalRef>,
    ) -> Result<Self> {
        let episode = Self {
            episode_id: WorkEpisodeId::new(),
            version: 0,
            task_session_id,
            task_id,
            intent_revisions,
            signal_refs,
            observations: Vec::new(),
            status: WorkEpisodeStatus::Open,
        };
        episode.validate()?;
        Ok(episode)
    }

    #[must_use]
    pub const fn ownership(&self) -> WorkEpisodeRef {
        WorkEpisodeRef {
            episode_id: self.episode_id,
            task_session_id: self.task_session_id,
            task_id: self.task_id,
        }
    }

    /// Adds one owned observation while the Episode is open.
    ///
    /// # Errors
    ///
    /// Rejects closed Episodes, duplicate IDs, cross-Task ownership, or an Intent revision outside
    /// the Episode range.
    pub fn add_observation(&mut self, observation: WorkObservation) -> Result<()> {
        if self.status != WorkEpisodeStatus::Open {
            return Err(invalid("closed Work Episode cannot accept observations"));
        }
        self.validate_observation(&observation)?;
        if self
            .observations
            .iter()
            .any(|existing| existing.observation_id == observation.observation_id)
        {
            return Err(invalid(
                "work_episode.observations must not repeat ObservationId",
            ));
        }
        self.observations.push(observation);
        self.version = self
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("Work Episode version overflow"))?;
        Ok(())
    }

    /// Closes an Episode at one owned final Checkpoint.
    ///
    /// # Errors
    ///
    /// Rejects already-closed Episodes or a Checkpoint from another owner/Intent range.
    pub fn close(&mut self, checkpoint: &AgentCheckpoint) -> Result<()> {
        if self.status != WorkEpisodeStatus::Open {
            return Err(invalid("Work Episode is already closed"));
        }
        checkpoint.validate_against_episode(self)?;
        self.status = WorkEpisodeStatus::Closed {
            final_checkpoint_id: checkpoint.checkpoint_id,
        };
        self.version = self
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("Work Episode version overflow"))?;
        self.validate()
    }

    /// Validates all ownership, range, lifecycle, and duplicate invariants.
    ///
    /// # Errors
    ///
    /// Returns an input error when the Episode cannot be safely consumed by a Builder.
    pub fn validate(&self) -> Result<()> {
        self.intent_revisions.validate()?;
        require_unique(&self.signal_refs, "work_episode.signal_refs")?;
        for signal in &self.signal_refs {
            signal.validate_owner(self.task_session_id, self.task_id)?;
        }
        let mut observation_ids = HashSet::with_capacity(self.observations.len());
        for observation in &self.observations {
            if !observation_ids.insert(observation.observation_id) {
                return Err(invalid(
                    "work_episode.observations must not repeat ObservationId",
                ));
            }
            self.validate_observation(observation)?;
        }
        Ok(())
    }

    fn validate_observation(&self, observation: &WorkObservation) -> Result<()> {
        observation.validate()?;
        if observation.task_session_id != self.task_session_id
            || observation.task_id != self.task_id
        {
            return Err(invalid(
                "work_observation must belong to the Work Episode Task",
            ));
        }
        if !self
            .intent_revisions
            .contains(observation.intent_revision_id)
        {
            return Err(invalid(
                "work_observation Intent revision is outside the Work Episode range",
            ));
        }
        if observation.source_refs.iter().any(|source| {
            matches!(
                source,
                WorkSourceRef::TaskSignal(signal) if !self.signal_refs.contains(signal)
            )
        }) {
            return Err(invalid(
                "work_observation Task Signal source must belong to the Work Episode",
            ));
        }
        Ok(())
    }
}

/// Structured unresolved engineering question retained by a Checkpoint or Candidate.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureUnknown {
    pub statement: String,
    pub blocking: bool,
    pub recheck_when: Vec<String>,
}

impl CaptureUnknown {
    fn validate(&self, field: &str) -> Result<()> {
        require_text(&self.statement, &format!("{field}.statement"))?;
        require_text_items(&self.recheck_when, &format!("{field}.recheck_when"))
    }
}

/// One server-identified structured engineering claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointClaim {
    pub claim_id: CheckpointClaimId,
    pub context_kind_hint: Option<crate::ContextKind>,
    pub topic_key_hint: Option<String>,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    pub evidence_refs: Vec<CaptureEvidenceRef>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub related_contexts: Vec<ContextRevisionRef>,
}

impl CheckpointClaim {
    /// Creates a complete Claim with a server-owned identity.
    ///
    /// # Errors
    ///
    /// Returns an input error for missing statement/rationale/Evidence or invalid references.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        context_kind_hint: Option<crate::ContextKind>,
        topic_key_hint: Option<String>,
        statement: impl Into<String>,
        rationale: impl Into<String>,
        applicability: Applicability,
        assumptions: Vec<String>,
        recheck_when: Vec<String>,
        evidence_refs: Vec<CaptureEvidenceRef>,
        artifact_refs: Vec<ArtifactRef>,
        related_contexts: Vec<ContextRevisionRef>,
    ) -> Result<Self> {
        let claim = Self {
            claim_id: CheckpointClaimId::new(),
            context_kind_hint,
            topic_key_hint,
            statement: statement.into(),
            rationale: rationale.into(),
            applicability,
            assumptions,
            recheck_when,
            evidence_refs,
            artifact_refs,
            related_contexts,
        };
        claim.validate("checkpoint_claim")?;
        Ok(claim)
    }

    fn validate(&self, field: &str) -> Result<()> {
        if let Some(topic_key_hint) = &self.topic_key_hint {
            require_text(topic_key_hint, &format!("{field}.topic_key_hint"))?;
        }
        require_text(&self.statement, &format!("{field}.statement"))?;
        require_text(&self.rationale, &format!("{field}.rationale"))?;
        self.applicability
            .validate(&format!("{field}.applicability"))?;
        require_text_items(&self.assumptions, &format!("{field}.assumptions"))?;
        require_text_items(&self.recheck_when, &format!("{field}.recheck_when"))?;
        if self.evidence_refs.is_empty() {
            return Err(invalid(format!("{field}.evidence_refs must not be empty")));
        }
        require_unique(&self.evidence_refs, &format!("{field}.evidence_refs"))?;
        require_unique(&self.artifact_refs, &format!("{field}.artifact_refs"))?;
        for artifact in &self.artifact_refs {
            artifact.validate()?;
        }
        require_unique(&self.related_contexts, &format!("{field}.related_contexts"))
    }
}

/// Agent-authored checkpoint of claims and unknowns under exact Episode ownership.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCheckpoint {
    pub checkpoint_id: AgentCheckpointId,
    pub episode_id: WorkEpisodeId,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub claims: Vec<CheckpointClaim>,
    pub unknowns: Vec<CaptureUnknown>,
}

impl AgentCheckpoint {
    /// Creates a Checkpoint with server-owned identity.
    ///
    /// # Errors
    ///
    /// Rejects empty Checkpoints, invalid Claims, or duplicate Unknowns.
    pub fn from_parts(
        episode: &WorkEpisode,
        intent_revision_id: TaskIntentRevisionId,
        claims: Vec<CheckpointClaim>,
        unknowns: Vec<CaptureUnknown>,
    ) -> Result<Self> {
        let checkpoint = Self {
            checkpoint_id: AgentCheckpointId::new(),
            episode_id: episode.episode_id,
            task_session_id: episode.task_session_id,
            task_id: episode.task_id,
            intent_revision_id,
            claims,
            unknowns,
        };
        checkpoint.validate_against_episode(episode)?;
        Ok(checkpoint)
    }

    /// Validates Checkpoint ownership and complete Claim/Unknown content.
    ///
    /// # Errors
    ///
    /// Returns an input error for cross-Episode ownership or invalid content.
    pub fn validate_against_episode(&self, episode: &WorkEpisode) -> Result<()> {
        if self.episode_id != episode.episode_id
            || self.task_session_id != episode.task_session_id
            || self.task_id != episode.task_id
        {
            return Err(invalid(
                "agent_checkpoint must belong to the supplied Work Episode",
            ));
        }
        if !episode.intent_revisions.contains(self.intent_revision_id) {
            return Err(invalid(
                "agent_checkpoint Intent revision is outside the Work Episode range",
            ));
        }
        if self.claims.is_empty() && self.unknowns.is_empty() {
            return Err(invalid(
                "agent_checkpoint must contain at least one Claim or Unknown",
            ));
        }
        let mut claim_ids = HashSet::with_capacity(self.claims.len());
        let mut claim_meanings = HashSet::with_capacity(self.claims.len());
        let observation_ids = episode
            .observations
            .iter()
            .map(|observation| observation.observation_id)
            .collect::<HashSet<_>>();
        let signal_ids = episode
            .signal_refs
            .iter()
            .map(|signal| signal.signal_id)
            .collect::<HashSet<_>>();
        for claim in &self.claims {
            claim.validate("agent_checkpoint.claim")?;
            if !claim_ids.insert(claim.claim_id)
                || !claim_meanings.insert((&claim.statement, &claim.rationale))
            {
                return Err(invalid("agent_checkpoint.claims must not repeat Claims"));
            }
            if claim.evidence_refs.iter().any(|evidence| match evidence {
                CaptureEvidenceRef::Observation { observation_id } => {
                    !observation_ids.contains(observation_id)
                }
                CaptureEvidenceRef::TaskSignal { signal_id } => !signal_ids.contains(signal_id),
                CaptureEvidenceRef::ContextEvidence { .. } => false,
            }) {
                return Err(invalid(
                    "agent_checkpoint Evidence must belong to the source Work Episode when Task-local",
                ));
            }
        }
        require_unique(&self.unknowns, "agent_checkpoint.unknowns")?;
        for unknown in &self.unknowns {
            unknown.validate("agent_checkpoint.unknown")?;
        }
        Ok(())
    }
}

/// Builder-owned, verifiable provenance for one automatic Candidate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateBuilderProvenance {
    pub build_id: CandidateBuildId,
    pub source_episode: WorkEpisodeRef,
    pub checkpoint_ids: Vec<AgentCheckpointId>,
    pub observation_ids: Vec<WorkObservationId>,
}

impl CandidateBuilderProvenance {
    /// Creates provenance for one Builder run.
    ///
    /// # Errors
    ///
    /// Requires at least one Checkpoint and no duplicate source IDs. Observation sources are
    /// optional when a Claim is grounded directly by Task Signal or immutable Context Evidence.
    pub fn from_parts(
        source_episode: WorkEpisodeRef,
        checkpoint_ids: Vec<AgentCheckpointId>,
        observation_ids: Vec<WorkObservationId>,
    ) -> Result<Self> {
        let provenance = Self {
            build_id: CandidateBuildId::new(),
            source_episode,
            checkpoint_ids,
            observation_ids,
        };
        provenance.validate()?;
        Ok(provenance)
    }

    fn validate(&self) -> Result<()> {
        if self.checkpoint_ids.is_empty() {
            return Err(invalid(
                "candidate_builder_provenance requires a Checkpoint source",
            ));
        }
        require_unique(
            &self.checkpoint_ids,
            "candidate_builder_provenance.checkpoint_ids",
        )?;
        require_unique(
            &self.observation_ids,
            "candidate_builder_provenance.observation_ids",
        )
    }
}

/// Lifecycle of one rebuildable Candidate review analysis.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateAnalysisStatus {
    Pending,
    Complete,
    Failed,
}

/// Non-authoritative relationship assessed against one immutable Context revision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateAssessmentRelation {
    ExactDuplicate,
    Supports,
    Revises,
    PotentialContradiction,
    UnresolvedRelated,
    Novel,
}

/// Typed comparison or retrieval evidence supporting one Candidate assessment.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateAssessmentPath {
    CanonicalDraftEquality,
    StatementEquality,
    TopicEquality {
        topic_key: String,
    },
    ExplicitRelatedContext,
    ExactArtifactGraph {
        artifact: ArtifactRef,
    },
    ContextRelationHop {
        relation: ContextRelationKind,
        depth: u8,
    },
    ContextFullText {
        matched_terms: Vec<String>,
    },
    ScopeOverlap {
        domains: Vec<String>,
        platforms: Vec<String>,
        conditions: Vec<String>,
    },
    SafetyDiagnostic {
        reason: String,
    },
    NoSufficientCandidate,
}

impl CandidateAssessmentPath {
    fn validate(&self, field: &str) -> Result<()> {
        match self {
            Self::CanonicalDraftEquality
            | Self::StatementEquality
            | Self::ExplicitRelatedContext
            | Self::NoSufficientCandidate => Ok(()),
            Self::TopicEquality { topic_key } => {
                require_text(topic_key, &format!("{field}.topic_key"))
            }
            Self::ExactArtifactGraph { artifact } => artifact.validate(),
            Self::ContextRelationHop { depth, .. } => {
                if *depth == 0 || *depth > 2 {
                    return Err(invalid(format!("{field}.depth must be one or two")));
                }
                Ok(())
            }
            Self::ContextFullText { matched_terms } => {
                if matched_terms.is_empty() {
                    return Err(invalid(format!("{field}.matched_terms must not be empty")));
                }
                require_text_items(matched_terms, &format!("{field}.matched_terms"))
            }
            Self::ScopeOverlap {
                domains,
                platforms,
                conditions,
            } => {
                if domains.is_empty() && platforms.is_empty() && conditions.is_empty() {
                    return Err(invalid(format!("{field} requires an overlapping scope")));
                }
                require_text_items(domains, &format!("{field}.domains"))?;
                require_text_items(platforms, &format!("{field}.platforms"))?;
                require_text_items(conditions, &format!("{field}.conditions"))
            }
            Self::SafetyDiagnostic { reason } => require_text(reason, &format!("{field}.reason")),
        }
    }
}

/// One evidence-linked review assessment; it never creates an authoritative Context relation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRelationAssessment {
    pub relation: CandidateAssessmentRelation,
    pub target: Option<ContextRevisionRef>,
    pub confidence: CandidateConfidence,
    pub paths: Vec<CandidateAssessmentPath>,
    pub reasons: Vec<String>,
}

impl CandidateRelationAssessment {
    fn validate(&self, field: &str) -> Result<()> {
        match (self.relation, self.target) {
            (CandidateAssessmentRelation::Novel, Some(_)) => {
                return Err(invalid(format!("{field}.novel must not have a target")));
            }
            (CandidateAssessmentRelation::Novel, None) | (_, Some(_)) => {}
            (_, None) => return Err(invalid(format!("{field} requires a target"))),
        }
        self.confidence.validate(&format!("{field}.confidence"))?;
        if self.paths.is_empty() || self.reasons.is_empty() {
            return Err(invalid(format!("{field} requires typed paths and reasons")));
        }
        require_unique(&self.paths, &format!("{field}.paths"))?;
        for (index, path) in self.paths.iter().enumerate() {
            path.validate(&format!("{field}.paths[{index}]"))?;
        }
        require_text_items(&self.reasons, &format!("{field}.reasons"))?;
        if self.relation == CandidateAssessmentRelation::ExactDuplicate
            && !self
                .paths
                .contains(&CandidateAssessmentPath::CanonicalDraftEquality)
        {
            return Err(invalid(format!(
                "{field}.exact_duplicate requires canonical draft equality"
            )));
        }
        Ok(())
    }
}

/// Generation-pinned, rebuildable Candidate review assessment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAnalysis {
    pub status: CandidateAnalysisStatus,
    pub assessments: Vec<CandidateRelationAssessment>,
    pub context_tree_oid: Option<String>,
    pub context_generation: Option<u64>,
    pub graph_context_tree_oid: Option<String>,
    pub artifact_generation: Option<String>,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub omitted_target_count: usize,
    pub error_code: Option<String>,
}

impl Default for CandidateAnalysis {
    fn default() -> Self {
        Self {
            status: CandidateAnalysisStatus::Pending,
            assessments: Vec::new(),
            context_tree_oid: None,
            context_generation: None,
            graph_context_tree_oid: None,
            artifact_generation: None,
            token_budget: 0,
            estimated_tokens: 0,
            omitted_target_count: 0,
            error_code: None,
        }
    }
}

impl CandidateAnalysis {
    /// Validates analysis lifecycle, provenance and target uniqueness.
    ///
    /// # Errors
    ///
    /// Returns an input error for incomplete provenance, duplicate targets, or an invalid Novel
    /// assessment.
    pub fn validate(&self) -> Result<()> {
        match self.status {
            CandidateAnalysisStatus::Pending => {
                if !self.assessments.is_empty() || self.error_code.is_some() {
                    return Err(invalid(
                        "pending Candidate analysis cannot contain results or an error",
                    ));
                }
                return Ok(());
            }
            CandidateAnalysisStatus::Failed => {
                if !self.assessments.is_empty()
                    || self.error_code.as_deref().is_none_or(str::is_empty)
                {
                    return Err(invalid(
                        "failed Candidate analysis requires only a typed error code",
                    ));
                }
                return Ok(());
            }
            CandidateAnalysisStatus::Complete => {}
        }
        if self.context_tree_oid.as_deref().is_none_or(str::is_empty)
            || self.context_generation.is_none()
            || self.assessments.is_empty()
            || self.error_code.is_some()
        {
            return Err(invalid(
                "complete Candidate analysis requires Context provenance and assessments",
            ));
        }
        if self.estimated_tokens > self.token_budget {
            return Err(invalid(
                "Candidate analysis estimated tokens exceed its budget",
            ));
        }
        let mut targets = HashSet::new();
        let mut novel = 0_usize;
        for (index, assessment) in self.assessments.iter().enumerate() {
            assessment.validate(&format!("candidate_analysis.assessments[{index}]"))?;
            if assessment.relation == CandidateAssessmentRelation::Novel {
                novel = novel.saturating_add(1);
            } else if !targets.insert(assessment.target) {
                return Err(invalid(
                    "candidate_analysis must not assess one target more than once",
                ));
            }
        }
        if novel > 0 && (novel != 1 || self.assessments.len() != 1) {
            return Err(invalid(
                "novel Candidate analysis must be one exclusive assessment",
            ));
        }
        Ok(())
    }
}

/// Deterministic confidence with a human-readable basis.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfidence {
    pub basis_points: u16,
    pub rationale: String,
}

impl CandidateConfidence {
    fn validate(&self, field: &str) -> Result<()> {
        if self.basis_points > 10_000 {
            return Err(invalid(format!(
                "{field}.basis_points must be between 0 and 10000"
            )));
        }
        require_text(&self.rationale, &format!("{field}.rationale"))
    }
}

/// Existing Space relationship recommended for later human confirmation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedSpaceRole {
    Primary,
    Related,
}

/// Typed evidence for a non-binding Candidate Space recommendation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateSpaceRecommendationPath {
    CandidateTarget {
        relation: CandidateAssessmentRelation,
        target: ContextRevisionRef,
    },
    SourceTaskAssociation {
        reason: String,
    },
    SpaceIntentMatch {
        matched_terms: Vec<String>,
    },
    IntentConflict {
        head_count: usize,
    },
    ProposedFromCandidate,
    ManualReview,
}

impl CandidateSpaceRecommendationPath {
    fn validate(&self, field: &str) -> Result<()> {
        match self {
            Self::CandidateTarget { relation, .. } => {
                if *relation == CandidateAssessmentRelation::Novel {
                    return Err(invalid(format!(
                        "{field}.candidate_target cannot use novel"
                    )));
                }
                Ok(())
            }
            Self::SourceTaskAssociation { reason } => {
                require_text(reason, &format!("{field}.reason"))
            }
            Self::SpaceIntentMatch { matched_terms } => {
                if matched_terms.is_empty() {
                    return Err(invalid(format!("{field}.matched_terms must not be empty")));
                }
                require_text_items(matched_terms, &format!("{field}.matched_terms"))
            }
            Self::IntentConflict { head_count } => {
                if *head_count < 2 {
                    return Err(invalid(format!("{field}.head_count must be at least two")));
                }
                Ok(())
            }
            Self::ProposedFromCandidate | Self::ManualReview => Ok(()),
        }
    }
}

/// Candidate Space recommendation; it never establishes ownership.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateSpaceRecommendation {
    Existing {
        space_id: SpaceId,
        role: RecommendedSpaceRole,
        rationale: String,
        confidence: CandidateConfidence,
        paths: Vec<CandidateSpaceRecommendationPath>,
    },
    ProposedNewSpaceIntent {
        proposed_new_space_intent: IntentSnapshot,
        rationale: String,
        confidence: CandidateConfidence,
        paths: Vec<CandidateSpaceRecommendationPath>,
    },
}

impl CandidateSpaceRecommendation {
    /// Recommends an existing Space without selecting it.
    pub fn existing(
        space_id: SpaceId,
        role: RecommendedSpaceRole,
        rationale: impl Into<String>,
        confidence: CandidateConfidence,
    ) -> Self {
        Self::Existing {
            space_id,
            role,
            rationale: rationale.into(),
            confidence,
            paths: vec![CandidateSpaceRecommendationPath::ManualReview],
        }
    }

    /// Recommends a proposed new Space Intent without creating a Space.
    pub fn proposed_new(
        intent: IntentSnapshot,
        rationale: impl Into<String>,
        confidence: CandidateConfidence,
    ) -> Self {
        Self::ProposedNewSpaceIntent {
            proposed_new_space_intent: intent,
            rationale: rationale.into(),
            confidence,
            paths: vec![CandidateSpaceRecommendationPath::ProposedFromCandidate],
        }
    }

    /// Recommends an existing Space with explicit derived paths.
    pub fn existing_with_paths(
        space_id: SpaceId,
        role: RecommendedSpaceRole,
        rationale: impl Into<String>,
        confidence: CandidateConfidence,
        paths: Vec<CandidateSpaceRecommendationPath>,
    ) -> Self {
        Self::Existing {
            space_id,
            role,
            rationale: rationale.into(),
            confidence,
            paths,
        }
    }

    /// Recommends a complete proposed Space Intent with explicit derived paths.
    pub fn proposed_new_with_paths(
        intent: IntentSnapshot,
        rationale: impl Into<String>,
        confidence: CandidateConfidence,
        paths: Vec<CandidateSpaceRecommendationPath>,
    ) -> Self {
        Self::ProposedNewSpaceIntent {
            proposed_new_space_intent: intent,
            rationale: rationale.into(),
            confidence,
            paths,
        }
    }

    fn validate(&self, field: &str) -> Result<()> {
        match self {
            Self::Existing {
                rationale,
                confidence,
                paths,
                ..
            } => {
                require_text(rationale, &format!("{field}.rationale"))?;
                confidence.validate(&format!("{field}.confidence"))?;
                validate_recommendation_paths(paths, field)
            }
            Self::ProposedNewSpaceIntent {
                proposed_new_space_intent,
                rationale,
                confidence,
                paths,
                ..
            } => {
                proposed_new_space_intent.validate()?;
                require_text(rationale, &format!("{field}.rationale"))?;
                confidence.validate(&format!("{field}.confidence"))?;
                validate_recommendation_paths(paths, field)
            }
        }
    }
}

fn validate_recommendation_paths(
    paths: &[CandidateSpaceRecommendationPath],
    field: &str,
) -> Result<()> {
    if paths.is_empty() {
        return Err(invalid(format!("{field}.paths must not be empty")));
    }
    require_unique(paths, &format!("{field}.paths"))?;
    for (index, path) in paths.iter().enumerate() {
        path.validate(&format!("{field}.paths[{index}]"))?;
    }
    Ok(())
}

fn validate_recommendations(values: &[CandidateSpaceRecommendation]) -> Result<()> {
    let mut existing_spaces = HashSet::new();
    let mut primary_count = 0_usize;
    let mut proposed_count = 0_usize;
    for (index, recommendation) in values.iter().enumerate() {
        recommendation.validate(&format!("space_recommendations[{index}]"))?;
        match recommendation {
            CandidateSpaceRecommendation::Existing { space_id, role, .. } => {
                if !existing_spaces.insert(*space_id) {
                    return Err(invalid(
                        "space_recommendations must not repeat an existing Space",
                    ));
                }
                primary_count += usize::from(*role == RecommendedSpaceRole::Primary);
            }
            CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. } => {
                proposed_count += 1;
            }
        }
    }
    if primary_count > 1 || proposed_count > 1 {
        return Err(invalid(
            "space_recommendations allow at most one Primary and one proposed new Space",
        ));
    }
    Ok(())
}

/// Current readiness of an automatic Candidate draft.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticCandidateStatus {
    Draft,
    NeedsEvidence,
    NeedsSpaceReview,
    ExactDuplicateReview,
    PotentialContradictionReview,
    ReadyForReview,
}

/// Builder-produced unowned Candidate with verifiable Episode provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticContextCandidate {
    pub candidate_id: CandidateId,
    pub source_episode: WorkEpisodeRef,
    pub content: ContextRevisionDraft,
    pub builder_provenance: CandidateBuilderProvenance,
    pub analysis: CandidateAnalysis,
    pub space_recommendations: Vec<CandidateSpaceRecommendation>,
    pub confidence: CandidateConfidence,
    pub unknowns: Vec<CaptureUnknown>,
    pub status: AutomaticCandidateStatus,
}

impl AutomaticContextCandidate {
    /// Creates a Builder Candidate with no selected Space.
    ///
    /// # Errors
    ///
    /// Rejects an open/mismatched Episode, missing Builder sources, incomplete Context Evidence,
    /// invalid analysis/recommendations, or a Ready status with blocking unknowns.
    #[allow(clippy::too_many_arguments)]
    pub fn from_builder(
        episode: &WorkEpisode,
        checkpoints: &[AgentCheckpoint],
        content: ContextRevisionDraft,
        builder_provenance: CandidateBuilderProvenance,
        analysis: CandidateAnalysis,
        space_recommendations: Vec<CandidateSpaceRecommendation>,
        confidence: CandidateConfidence,
        unknowns: Vec<CaptureUnknown>,
        status: AutomaticCandidateStatus,
    ) -> Result<Self> {
        let candidate = Self {
            candidate_id: CandidateId::new(),
            source_episode: episode.ownership(),
            content,
            builder_provenance,
            analysis,
            space_recommendations,
            confidence,
            unknowns,
            status,
        };
        candidate.validate_against_sources(episode, checkpoints)?;
        Ok(candidate)
    }

    /// Restores the automatic Candidate view around the exact Candidate identity returned by the
    /// persisted submission boundary.
    ///
    /// # Errors
    ///
    /// Rejects mismatched Episode ownership or invalid Builder metadata and content.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted_candidate(
        persisted: &ContextCandidate,
        episode: &WorkEpisode,
        checkpoints: &[AgentCheckpoint],
        builder_provenance: CandidateBuilderProvenance,
        analysis: CandidateAnalysis,
        space_recommendations: Vec<CandidateSpaceRecommendation>,
        confidence: CandidateConfidence,
        unknowns: Vec<CaptureUnknown>,
        status: AutomaticCandidateStatus,
    ) -> Result<Self> {
        let candidate = Self {
            candidate_id: persisted.candidate_id,
            source_episode: persisted.source_episode,
            content: persisted.content.clone(),
            builder_provenance,
            analysis,
            space_recommendations,
            confidence,
            unknowns,
            status,
        };
        if persisted.source_episode != episode.ownership() {
            return Err(invalid(
                "persisted Candidate source Episode ownership does not match",
            ));
        }
        candidate.validate_against_sources(episode, checkpoints)?;
        Ok(candidate)
    }

    /// Validates all Builder/Episode/Checkpoint ownership and Candidate content.
    ///
    /// # Errors
    ///
    /// Returns an input error for incomplete content, invalid readiness, or unverifiable source
    /// ownership.
    pub fn validate_against_sources(
        &self,
        episode: &WorkEpisode,
        checkpoints: &[AgentCheckpoint],
    ) -> Result<()> {
        episode.validate()?;
        if !matches!(episode.status, WorkEpisodeStatus::Closed { .. }) {
            return Err(invalid(
                "automatic Context Candidate requires a closed Work Episode",
            ));
        }
        if self.source_episode != episode.ownership()
            || self.builder_provenance.source_episode != episode.ownership()
        {
            return Err(invalid(
                "automatic Context Candidate source Episode ownership does not match",
            ));
        }
        self.builder_provenance.validate()?;
        let WorkEpisodeStatus::Closed {
            final_checkpoint_id,
        } = episode.status
        else {
            unreachable!("open Episode was rejected above")
        };
        if !self
            .builder_provenance
            .checkpoint_ids
            .contains(&final_checkpoint_id)
        {
            return Err(invalid(
                "Candidate Builder provenance must include the Episode final Checkpoint",
            ));
        }
        self.content.validate()?;
        self.analysis.validate()?;
        validate_recommendations(&self.space_recommendations)?;
        self.confidence.validate("candidate.confidence")?;
        require_unique(&self.unknowns, "candidate.unknowns")?;
        for unknown in &self.unknowns {
            unknown.validate("candidate.unknown")?;
        }
        let checkpoint_map = checkpoints
            .iter()
            .map(|checkpoint| (checkpoint.checkpoint_id, checkpoint))
            .collect::<std::collections::BTreeMap<_, _>>();
        if checkpoint_map.len() != checkpoints.len() {
            return Err(invalid("Candidate source Checkpoints must not repeat IDs"));
        }
        for checkpoint_id in &self.builder_provenance.checkpoint_ids {
            let checkpoint = checkpoint_map
                .get(checkpoint_id)
                .ok_or_else(|| invalid("Candidate Builder provenance Checkpoint does not exist"))?;
            checkpoint.validate_against_episode(episode)?;
        }
        let observation_ids = episode
            .observations
            .iter()
            .map(|observation| observation.observation_id)
            .collect::<BTreeSet<_>>();
        if self
            .builder_provenance
            .observation_ids
            .iter()
            .any(|observation_id| !observation_ids.contains(observation_id))
        {
            return Err(invalid(
                "Candidate Builder provenance Observation does not belong to source Episode",
            ));
        }
        if self.status == AutomaticCandidateStatus::ReadyForReview {
            if self.unknowns.iter().any(|unknown| unknown.blocking) {
                return Err(invalid(
                    "ReadyForReview Candidate cannot contain blocking unknowns",
                ));
            }
            if self.confidence.basis_points == 0 {
                return Err(invalid(
                    "ReadyForReview Candidate requires non-zero confidence",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_auto_injection_eligible(&self) -> bool {
        false
    }
}

/// Existing Git/Event Candidate content retained outside the M4 automatic Builder contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCandidate {
    pub candidate_id: CandidateId,
    pub submission_id: SubmissionId,
    pub source_episode: WorkEpisodeRef,
    pub content: ContextRevisionDraft,
}

impl ContextCandidate {
    /// Creates an unowned Candidate for one stable submission operation.
    ///
    /// # Errors
    ///
    /// Returns an input error when the Episode or Candidate content is invalid.
    pub fn from_episode(
        submission_id: SubmissionId,
        episode: &WorkEpisode,
        content: ContextRevisionDraft,
    ) -> Result<Self> {
        episode.validate()?;
        if !matches!(episode.status, WorkEpisodeStatus::Closed { .. }) {
            return Err(invalid("Context Candidate requires a closed Work Episode"));
        }
        content.validate()?;
        Ok(Self {
            candidate_id: CandidateId::new(),
            submission_id,
            source_episode: episode.ownership(),
            content,
        })
    }

    /// Creates an Event Candidate after the service verified its closed Episode.
    /// Candidate identity remains server-generated.
    ///
    /// # Errors
    ///
    /// Returns an input error when the complete Context draft is invalid.
    pub fn from_verified_submission(
        submission_id: SubmissionId,
        source_episode: WorkEpisodeRef,
        content: ContextRevisionDraft,
    ) -> Result<Self> {
        content.validate()?;
        Ok(Self {
            candidate_id: CandidateId::new(),
            submission_id,
            source_episode,
            content,
        })
    }

    /// Authoritative retry hash excluding Submission/Candidate IDs and annotations.
    ///
    /// # Panics
    ///
    /// Panics only if serializing validated domain content unexpectedly fails.
    #[must_use]
    pub fn submission_content_hash(&self) -> String {
        candidate_submission_content_hash(&self.source_episode, &self.content)
    }

    #[must_use]
    pub const fn source_episode_id(&self) -> WorkEpisodeId {
        self.source_episode.episode_id
    }

    /// Validates the Candidate content.
    ///
    /// # Errors
    ///
    /// Returns an input error when the Context draft is incomplete.
    pub fn validate(&self) -> Result<()> {
        self.content.validate()
    }

    /// Validates the Candidate content and its source Episode identity.
    ///
    /// # Errors
    ///
    /// Returns an input error for invalid content or a different source Episode.
    pub fn validate_against_episode(&self, episode: &WorkEpisode) -> Result<()> {
        self.validate()?;
        episode.validate()?;
        if self.source_episode != episode.ownership() {
            return Err(invalid(
                "context_candidate.source_episode must identify the supplied episode owner",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_auto_injection_eligible(&self) -> bool {
        false
    }
}

/// Hashes exactly the closed Episode ownership and complete authoritative draft.
///
/// # Panics
///
/// Panics only if serialization of these JSON-backed domain values unexpectedly fails.
#[must_use]
pub fn candidate_submission_content_hash(
    source_episode: &WorkEpisodeRef,
    content: &ContextRevisionDraft,
) -> String {
    let bytes = serde_json::to_vec(&(source_episode, content))
        .expect("serializing Candidate submission content cannot fail");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        ArtifactLocator, ContextKind, EvidenceSnapshotDraft, EvidenceType, RepoRelativePath,
    };

    fn artifact(path: &str) -> ArtifactRef {
        ArtifactRef {
            repository_id: RepositoryId::new(),
            locator: ArtifactLocator::File {
                path: RepoRelativePath::new(path).unwrap(),
            },
        }
    }

    fn signal(task_session_id: TaskSessionId, task_id: TaskId) -> NonLocatingSignalRef {
        NonLocatingSignalRef {
            signal_id: SignalId::new(),
            task_session_id,
            task_id,
            kind: TaskSignalKind::Diff,
        }
    }

    fn content() -> ContextRevisionDraft {
        ContextRevisionDraft {
            kind: ContextKind::Decision,
            topic_key: Some("capture/fallback-owner".to_owned()),
            statement: "Keep fallback ownership server-side".to_owned(),
            rationale: "Every client consumes one contract".to_owned(),
            applicability: Applicability {
                domains: vec!["search".to_owned()],
                platforms: vec!["fe".to_owned(), "ios".to_owned()],
                conditions: vec!["v2".to_owned()],
            },
            assumptions: vec!["The response remains versioned".to_owned()],
            recheck_when: vec!["The v3 contract ships".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "The compatibility suite passed".to_owned(),
                content: json!({"result": "passed"}),
                interpretation: "The decision is supported".to_owned(),
                limitations: vec!["Synthetic fixture".to_owned()],
            }],
        }
    }

    fn unknown(blocking: bool) -> CaptureUnknown {
        CaptureUnknown {
            statement: "Confirm the v3 rollout date".to_owned(),
            blocking,
            recheck_when: vec!["The rollout plan changes".to_owned()],
        }
    }

    fn confidence(points: u16) -> CandidateConfidence {
        CandidateConfidence {
            basis_points: points,
            rationale: "Derived from normalized Evidence".to_owned(),
        }
    }

    fn assessment(
        relation: CandidateAssessmentRelation,
        target: Option<ContextRevisionRef>,
        paths: Vec<CandidateAssessmentPath>,
    ) -> CandidateRelationAssessment {
        CandidateRelationAssessment {
            relation,
            target,
            confidence: confidence(8_000),
            paths,
            reasons: vec!["Typed Candidate review assessment".to_owned()],
        }
    }

    fn complete_analysis(assessments: Vec<CandidateRelationAssessment>) -> CandidateAnalysis {
        CandidateAnalysis {
            status: CandidateAnalysisStatus::Complete,
            assessments,
            context_tree_oid: Some("tree".to_owned()),
            context_generation: Some(1),
            graph_context_tree_oid: None,
            artifact_generation: None,
            token_budget: 1_000,
            estimated_tokens: 100,
            omitted_target_count: 0,
            error_code: None,
        }
    }

    fn novel_analysis() -> CandidateAnalysis {
        complete_analysis(vec![assessment(
            CandidateAssessmentRelation::Novel,
            None,
            vec![CandidateAssessmentPath::NoSufficientCandidate],
        )])
    }

    fn proposed_intent(title: &str) -> IntentSnapshot {
        IntentSnapshot {
            title: title.to_owned(),
            problem: "The Candidate has no suitable existing Space".to_owned(),
            desired_outcome: "Create a governance container after confirmation".to_owned(),
            in_scope: vec!["search fallback".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["Candidate can be reviewed".to_owned()],
            domain_terms: vec!["fallback".to_owned()],
        }
    }

    struct CaptureFixture {
        episode: WorkEpisode,
        checkpoint: AgentCheckpoint,
        observation_id: WorkObservationId,
    }

    fn capture_fixture() -> CaptureFixture {
        let task_session_id = TaskSessionId::new();
        let task_id = TaskId::new();
        let revisions = IntentRevisionRange::new(vec![
            TaskIntentRevisionId::new(),
            TaskIntentRevisionId::new(),
        ])
        .unwrap();
        let signal = signal(task_session_id, task_id);
        let mut episode =
            WorkEpisode::open(task_session_id, task_id, revisions.clone(), vec![signal]).unwrap();
        let observation = WorkObservation::from_parts(
            task_session_id,
            task_id,
            revisions.last(),
            vec![WorkSourceRef::Artifact(artifact("src/search.ts"))],
            NormalizedWorkObservation::Artifact {
                artifact: artifact("src/search.ts"),
                action: ArtifactAction::Inspected,
                summary: "The client consumes a server-owned fallback".to_owned(),
            },
        )
        .unwrap();
        let observation_id = observation.observation_id;
        episode.add_observation(observation).unwrap();
        let claim = CheckpointClaim::from_parts(
            Some(ContextKind::Decision),
            Some("capture/fallback-owner".to_owned()),
            "Keep fallback ownership server-side",
            "Every client consumes one contract",
            Applicability::default(),
            Vec::new(),
            vec!["The v3 contract ships".to_owned()],
            vec![CaptureEvidenceRef::Observation { observation_id }],
            vec![artifact("src/search.ts")],
            Vec::new(),
        )
        .unwrap();
        let checkpoint =
            AgentCheckpoint::from_parts(&episode, revisions.last(), vec![claim], Vec::new())
                .unwrap();
        episode.close(&checkpoint).unwrap();
        CaptureFixture {
            episode,
            checkpoint,
            observation_id,
        }
    }

    fn provenance(fixture: &CaptureFixture) -> CandidateBuilderProvenance {
        CandidateBuilderProvenance::from_parts(
            fixture.episode.ownership(),
            vec![fixture.checkpoint.checkpoint_id],
            vec![fixture.observation_id],
        )
        .unwrap()
    }

    #[test]
    fn episode_open_close_and_serialization_keep_only_typed_normalized_inputs() {
        let fixture = capture_fixture();
        assert!(matches!(
            fixture.episode.status,
            WorkEpisodeStatus::Closed {
                final_checkpoint_id
            } if final_checkpoint_id == fixture.checkpoint.checkpoint_id
        ));
        assert_eq!(fixture.episode.intent_revisions.revision_ids.len(), 2);
        assert_eq!(fixture.episode.signal_refs.len(), 1);
        assert_eq!(fixture.episode.observations.len(), 1);

        let mut closed = fixture.episode.clone();
        let extra = WorkObservation::from_parts(
            closed.task_session_id,
            closed.task_id,
            closed.intent_revisions.last(),
            vec![WorkSourceRef::Artifact(artifact("src/extra.ts"))],
            NormalizedWorkObservation::UnresolvedQuestion {
                question: "Does v3 preserve the fallback?".to_owned(),
            },
        )
        .unwrap();
        assert!(closed.add_observation(extra).is_err());
        assert!(closed.close(&fixture.checkpoint).is_err());

        let serialized = serde_json::to_string(&fixture.episode).unwrap();
        for forbidden in [
            "task_intent",
            "prompt_raw",
            "transcript",
            "tool_output",
            "raw_payload",
            "space_id",
            "workspace_path",
        ] {
            assert!(!serialized.contains(forbidden), "serialized {forbidden}");
        }
    }

    #[test]
    fn empty_and_cross_owner_episode_checkpoint_inputs_are_rejected() {
        assert!(IntentRevisionRange::new(Vec::new()).is_err());
        let revision = TaskIntentRevisionId::new();
        assert!(IntentRevisionRange::new(vec![revision, revision]).is_err());

        let session = TaskSessionId::new();
        let task = TaskId::new();
        let range = IntentRevisionRange::new(vec![TaskIntentRevisionId::new()]).unwrap();
        let wrong_signal = signal(TaskSessionId::new(), task);
        assert!(WorkEpisode::open(session, task, range.clone(), vec![wrong_signal]).is_err());
        let mut episode = WorkEpisode::open(session, task, range.clone(), Vec::new()).unwrap();
        let cross_task = WorkObservation::from_parts(
            session,
            TaskId::new(),
            range.first(),
            vec![WorkSourceRef::Artifact(artifact("src/cross.ts"))],
            NormalizedWorkObservation::Breadcrumb {
                category: NormalizedBreadcrumbKind::Exploration,
                summary: "Cross Task observation".to_owned(),
            },
        )
        .unwrap();
        assert!(episode.add_observation(cross_task).is_err());
        let outside_revision = WorkObservation::from_parts(
            session,
            task,
            TaskIntentRevisionId::new(),
            vec![WorkSourceRef::Artifact(artifact("src/outside.ts"))],
            NormalizedWorkObservation::UnresolvedQuestion {
                question: "Outside revision?".to_owned(),
            },
        )
        .unwrap();
        assert!(episode.add_observation(outside_revision).is_err());
        let unlisted_signal = signal(session, task);
        let unlisted_signal_source = WorkObservation::from_parts(
            session,
            task,
            range.first(),
            vec![WorkSourceRef::TaskSignal(unlisted_signal)],
            NormalizedWorkObservation::Breadcrumb {
                category: NormalizedBreadcrumbKind::Exploration,
                summary: "Signal is not part of this Episode".to_owned(),
            },
        )
        .unwrap();
        assert!(episode.add_observation(unlisted_signal_source).is_err());

        let empty_checkpoint =
            AgentCheckpoint::from_parts(&episode, range.first(), Vec::new(), Vec::new());
        assert!(empty_checkpoint.is_err());
        let incomplete_validation = WorkObservation::from_parts(
            session,
            task,
            range.first(),
            vec![WorkSourceRef::Artifact(artifact("src/test.ts"))],
            NormalizedWorkObservation::Validation {
                conclusion: "The test passed".to_owned(),
                evidence_refs: Vec::new(),
            },
        );
        assert!(incomplete_validation.is_err());
    }

    #[test]
    fn duplicate_sources_claims_and_recommendations_are_rejected() {
        let fixture = capture_fixture();
        let source = WorkSourceRef::Artifact(artifact("src/duplicate.ts"));
        assert!(
            WorkObservation::from_parts(
                fixture.episode.task_session_id,
                fixture.episode.task_id,
                fixture.episode.intent_revisions.last(),
                vec![source.clone(), source],
                NormalizedWorkObservation::Breadcrumb {
                    category: NormalizedBreadcrumbKind::Decision,
                    summary: "Duplicate source".to_owned(),
                },
            )
            .is_err()
        );

        let claim = fixture.checkpoint.claims[0].clone();
        let duplicate_claims = AgentCheckpoint::from_parts(
            &fixture.episode,
            fixture.episode.intent_revisions.last(),
            vec![claim.clone(), claim],
            Vec::new(),
        );
        assert!(duplicate_claims.is_err());

        let mut unowned_evidence = fixture.checkpoint.claims[0].clone();
        unowned_evidence.claim_id = CheckpointClaimId::new();
        unowned_evidence.evidence_refs = vec![CaptureEvidenceRef::Observation {
            observation_id: WorkObservationId::new(),
        }];
        assert!(
            AgentCheckpoint::from_parts(
                &fixture.episode,
                fixture.episode.intent_revisions.last(),
                vec![unowned_evidence],
                Vec::new(),
            )
            .is_err()
        );

        assert!(
            CandidateBuilderProvenance::from_parts(
                fixture.episode.ownership(),
                vec![
                    fixture.checkpoint.checkpoint_id,
                    fixture.checkpoint.checkpoint_id
                ],
                vec![fixture.observation_id],
            )
            .is_err()
        );

        let space = SpaceId::new();
        let recommendation = CandidateSpaceRecommendation::existing(
            space,
            RecommendedSpaceRole::Primary,
            "Primary owner",
            confidence(9_000),
        );
        let candidate = AutomaticContextCandidate::from_builder(
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            content(),
            provenance(&fixture),
            novel_analysis(),
            vec![recommendation.clone(), recommendation],
            confidence(9_000),
            Vec::new(),
            AutomaticCandidateStatus::Draft,
        );
        assert!(candidate.is_err());

        let two_primary = AutomaticContextCandidate::from_builder(
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            content(),
            provenance(&fixture),
            novel_analysis(),
            vec![
                CandidateSpaceRecommendation::existing(
                    SpaceId::new(),
                    RecommendedSpaceRole::Primary,
                    "First",
                    confidence(8_000),
                ),
                CandidateSpaceRecommendation::existing(
                    SpaceId::new(),
                    RecommendedSpaceRole::Primary,
                    "Second",
                    confidence(8_000),
                ),
            ],
            confidence(8_000),
            Vec::new(),
            AutomaticCandidateStatus::Draft,
        );
        assert!(two_primary.is_err());
    }

    #[test]
    fn candidate_builder_must_include_the_episode_final_checkpoint() {
        let fixture = capture_fixture();
        let alternate_checkpoint = AgentCheckpoint::from_parts(
            &fixture.episode,
            fixture.episode.intent_revisions.last(),
            fixture.checkpoint.claims.clone(),
            Vec::new(),
        )
        .unwrap();
        let without_final_checkpoint = CandidateBuilderProvenance::from_parts(
            fixture.episode.ownership(),
            vec![alternate_checkpoint.checkpoint_id],
            vec![fixture.observation_id],
        )
        .unwrap();
        assert!(
            AutomaticContextCandidate::from_builder(
                &fixture.episode,
                std::slice::from_ref(&alternate_checkpoint),
                content(),
                without_final_checkpoint,
                novel_analysis(),
                Vec::new(),
                confidence(8_000),
                Vec::new(),
                AutomaticCandidateStatus::ReadyForReview,
            )
            .is_err()
        );
    }

    #[test]
    fn checkpoint_claim_and_candidate_require_complete_evidence() {
        assert!(
            CheckpointClaim::from_parts(
                None,
                None,
                "Claim without Evidence",
                "Cannot be grounded",
                Applicability::default(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .is_err()
        );

        let fixture = capture_fixture();
        let mut missing = content();
        missing.evidence.clear();
        assert!(
            AutomaticContextCandidate::from_builder(
                &fixture.episode,
                std::slice::from_ref(&fixture.checkpoint),
                missing,
                provenance(&fixture),
                novel_analysis(),
                Vec::new(),
                confidence(5_000),
                vec![unknown(true)],
                AutomaticCandidateStatus::NeedsEvidence,
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_allows_no_space_multiple_recommendations_and_proposed_new_intent() {
        let fixture = capture_fixture();
        let no_space = AutomaticContextCandidate::from_builder(
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            content(),
            provenance(&fixture),
            novel_analysis(),
            Vec::new(),
            confidence(7_000),
            Vec::new(),
            AutomaticCandidateStatus::NeedsSpaceReview,
        )
        .unwrap();
        assert!(no_space.space_recommendations.is_empty());
        assert!(!no_space.is_auto_injection_eligible());
        let serialized = serde_json::to_value(&no_space).unwrap();
        assert!(serialized.get("space_id").is_none());
        assert!(serialized.get("selected_space_id").is_none());

        let recommendations = vec![
            CandidateSpaceRecommendation::existing(
                SpaceId::new(),
                RecommendedSpaceRole::Primary,
                "Best existing owner",
                confidence(9_000),
            ),
            CandidateSpaceRecommendation::existing(
                SpaceId::new(),
                RecommendedSpaceRole::Related,
                "Related contract",
                confidence(7_500),
            ),
            CandidateSpaceRecommendation::proposed_new(
                proposed_intent("New fallback requirement"),
                "No exact Requirement exists",
                confidence(6_000),
            ),
        ];
        let candidate = AutomaticContextCandidate::from_builder(
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            content(),
            provenance(&fixture),
            novel_analysis(),
            recommendations,
            confidence(9_000),
            Vec::new(),
            AutomaticCandidateStatus::ReadyForReview,
        )
        .unwrap();
        assert_eq!(candidate.space_recommendations.len(), 3);
        assert!(!candidate.is_auto_injection_eligible());
        let encoded = serde_json::to_string(&candidate).unwrap();
        assert!(encoded.contains("proposed_new_space_intent"));
        assert!(!encoded.contains("selected_space"));
        assert!(!encoded.contains("workspace"));
        assert!(!encoded.contains("transcript"));
        assert!(!encoded.contains("tool_output"));
        assert!(!encoded.contains("raw_payload"));
    }

    #[test]
    fn analysis_novelty_exclusivity_and_relationship_coexistence_are_explicit() {
        let target = ContextRevisionRef {
            context_id: ContextId::new(),
            revision_id: RevisionId::new(),
        };
        assert!(novel_analysis().validate().is_ok());
        assert!(CandidateAnalysis::default().validate().is_ok());
        assert!(
            complete_analysis(vec![
                assessment(
                    CandidateAssessmentRelation::Novel,
                    None,
                    vec![CandidateAssessmentPath::NoSufficientCandidate],
                ),
                assessment(
                    CandidateAssessmentRelation::Supports,
                    Some(target),
                    vec![CandidateAssessmentPath::StatementEquality],
                ),
            ])
            .validate()
            .is_err()
        );
        assert!(
            complete_analysis(vec![assessment(
                CandidateAssessmentRelation::ExactDuplicate,
                Some(target),
                vec![CandidateAssessmentPath::StatementEquality],
            )])
            .validate()
            .is_err()
        );
        assert!(
            complete_analysis(vec![
                assessment(
                    CandidateAssessmentRelation::Supports,
                    Some(target),
                    vec![CandidateAssessmentPath::StatementEquality],
                ),
                assessment(
                    CandidateAssessmentRelation::Revises,
                    Some(target),
                    vec![CandidateAssessmentPath::ExplicitRelatedContext],
                ),
            ])
            .validate()
            .is_err()
        );
        assert!(
            complete_analysis(vec![assessment(
                CandidateAssessmentRelation::Supports,
                Some(target),
                vec![CandidateAssessmentPath::StatementEquality],
            )])
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn automatic_candidate_source_ownership_and_status_are_verifiable() {
        let fixture = capture_fixture();
        let mut unkeyed_decision = content();
        unkeyed_decision.topic_key = None;
        assert!(unkeyed_decision.validate().is_ok());
        let persisted =
            ContextCandidate::from_episode(SubmissionId::new(), &fixture.episode, unkeyed_decision)
                .unwrap();
        let restored = AutomaticContextCandidate::from_persisted_candidate(
            &persisted,
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            provenance(&fixture),
            novel_analysis(),
            Vec::new(),
            confidence(7_000),
            Vec::new(),
            AutomaticCandidateStatus::Draft,
        )
        .unwrap();
        assert_eq!(restored.candidate_id, persisted.candidate_id);

        let mut candidate = AutomaticContextCandidate::from_builder(
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            content(),
            provenance(&fixture),
            novel_analysis(),
            Vec::new(),
            confidence(9_000),
            Vec::new(),
            AutomaticCandidateStatus::ReadyForReview,
        )
        .unwrap();
        candidate.source_episode.task_id = TaskId::new();
        assert!(
            candidate
                .validate_against_sources(
                    &fixture.episode,
                    std::slice::from_ref(&fixture.checkpoint)
                )
                .is_err()
        );

        let mut blocking = AutomaticContextCandidate::from_builder(
            &fixture.episode,
            std::slice::from_ref(&fixture.checkpoint),
            content(),
            provenance(&fixture),
            novel_analysis(),
            Vec::new(),
            confidence(9_000),
            vec![unknown(false)],
            AutomaticCandidateStatus::Draft,
        )
        .unwrap();
        blocking.status = AutomaticCandidateStatus::ReadyForReview;
        blocking.unknowns[0].blocking = true;
        assert!(
            blocking
                .validate_against_sources(
                    &fixture.episode,
                    std::slice::from_ref(&fixture.checkpoint)
                )
                .is_err()
        );
        blocking.unknowns[0].blocking = false;
        blocking.confidence.basis_points = 0;
        assert!(
            blocking
                .validate_against_sources(
                    &fixture.episode,
                    std::slice::from_ref(&fixture.checkpoint)
                )
                .is_err()
        );
    }

    #[test]
    fn legacy_git_candidate_remains_unowned_and_noninjectable() {
        let fixture = capture_fixture();
        let candidate =
            ContextCandidate::from_episode(SubmissionId::new(), &fixture.episode, content())
                .unwrap();
        assert!(candidate.validate_against_episode(&fixture.episode).is_ok());
        assert!(!candidate.is_auto_injection_eligible());
        let mut encoded = serde_json::to_value(candidate).unwrap();
        assert!(encoded.get("space_id").is_none());
        encoded
            .as_object_mut()
            .unwrap()
            .insert("space_id".to_owned(), Value::String("forged".to_owned()));
        assert!(serde_json::from_value::<ContextCandidate>(encoded).is_err());
    }
}
