//! Local, disposable runtime state for external Agent sessions and explicit Tasks.
//!
//! This crate owns only `state/runtime.sqlite`. It has no dependency on the
//! Context Git Store or rebuildable knowledge index. Task boundaries are
//! explicit: the runtime never guesses a new Task from Prompt or Workspace text.

use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sctx_domain::{
    AgentCheckpoint, AgentCheckpointId, Applicability, ArtifactRef, AutomaticContextCandidate,
    CandidateBuildId, CandidateConfirmationPlan, CandidateId, CandidateReviewStatus,
    CaptureEvidenceRef, CaptureId, CaptureSourceRef, CaptureUnknown, CheckpointClaim,
    CheckpointClaimId, ConfirmationId, ContextId, ContextRevisionRef, Error, ErrorKind, EventId,
    EvidenceSnapshotDraft, ExternalSessionId, ExternalSessionLocator, ExternalSessionSnapshot,
    IntentRevisionRange, NonLocatingSignalRef, NormalizedWorkObservation, Result, SignalId,
    SubmissionId, TaskId, TaskIntent, TaskIntentDraft, TaskIntentRevision, TaskIntentRevisionId,
    TaskSessionId, TaskSessionSnapshot, TaskSignal, TaskSignalKind, TaskSignalLifecycle,
    TaskSignalRecord, WorkEpisode, WorkEpisodeId, WorkEpisodeRef, WorkEpisodeStatus,
    WorkObservation, WorkObservationId, WorkSourceRef, WorkingIntentSnapshot,
};

const SCHEMA_VERSION: i64 = 11;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EPISODE_LIST_LIMIT: usize = 256;
pub const DEFAULT_CANDIDATE_REVIEW_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub const MAX_CANDIDATE_REVIEW_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);
pub const MAX_CANDIDATE_REVIEW_LIST_LIMIT: usize = 100;

/// Result of atomically locating or creating one `ExternalSession`'s first Task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSessionOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub created: bool,
}

/// Result of atomically merging normalized Signals into the `ActiveTask`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeSignalsOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub inserted: usize,
    pub inserted_signal_ids: Vec<SignalId>,
}

/// Result of explicitly creating and activating a new Task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartNewTaskOutcome {
    pub external_session_id: ExternalSessionId,
    pub previous_task_id: TaskId,
    pub snapshot: TaskSessionSnapshot,
}

/// Exact semantic disposition of one CAS-guarded Working Intent continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentRevisionWriteStatus {
    Created,
    AlreadyCurrent,
}

/// Current or newly-created Working Intent revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendIntentRevisionOutcome {
    pub revision: TaskIntentRevision,
    pub status: IntentRevisionWriteStatus,
}

/// Result of explicitly selecting a retained Task as active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchActiveTaskOutcome {
    pub external_session_id: ExternalSessionId,
    pub previous_task_id: TaskId,
    pub snapshot: TaskSessionSnapshot,
    pub switched: bool,
}

/// Result of superseding identified Signals without deleting history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupersedeSignalsOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub superseded_signal_ids: Vec<SignalId>,
}

/// Persisted Work Episode plus safe runtime diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkEpisodeView {
    pub episode: WorkEpisode,
    pub checkpoints: Vec<AgentCheckpoint>,
    pub diagnostics: Vec<WorkEpisodeDiagnostic>,
}

/// Whether a Checkpoint preserves the open Episode or closes its final boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointBoundary {
    Continue,
    Close,
}

impl CheckpointBoundary {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Close => "close",
        }
    }
}

/// Complete Agent-authored Claim content before server IDs and inline Observation IDs exist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointClaimDraft {
    pub context_kind_hint: Option<sctx_domain::ContextKind>,
    pub topic_key_hint: Option<String>,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    pub evidence_refs: Vec<CaptureEvidenceRef>,
    pub inline_validations: Vec<EvidenceSnapshotDraft>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub related_contexts: Vec<ContextRevisionRef>,
}

/// One strict Checkpoint write under `ActiveTask`, Intent and Episode-version CAS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCheckpointWrite {
    pub locator: ExternalSessionLocator,
    pub expected_task_id: TaskId,
    pub expected_intent_revision_id: TaskIntentRevisionId,
    pub expected_episode_version: u64,
    pub boundary: CheckpointBoundary,
    pub claims: Vec<CheckpointClaimDraft>,
    pub unknowns: Vec<CaptureUnknown>,
}

/// Idempotent result of one atomic Checkpoint and Episode transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCheckpointOutcome {
    pub checkpoint: AgentCheckpoint,
    pub episode: WorkEpisodeView,
    pub created: bool,
    pub inline_observation_ids: Vec<WorkObservationId>,
}

/// Current rebuildable Candidate analysis stored outside Git knowledge facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAnalysisView {
    pub candidate: AutomaticContextCandidate,
    pub analysis_generation: u64,
}

/// Minimal durable discovery/audit record for one finalized automatic Candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReviewRecord {
    pub candidate_id: CandidateId,
    pub submission_id: SubmissionId,
    pub source_episode: WorkEpisodeRef,
    pub build_id: CandidateBuildId,
    pub final_checkpoint_id: AgentCheckpointId,
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub review_version: u64,
    pub status: CandidateReviewStatus,
    pub discard_reason: Option<String>,
    pub created_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
    pub discarded_at_unix_seconds: Option<u64>,
    pub expired_at_unix_seconds: Option<u64>,
    pub confirmation_id: Option<sctx_domain::ConfirmationId>,
    pub result_context_id: Option<sctx_domain::ContextId>,
}

/// Bounded stable page of Review records owned by one exact `ActiveTask`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReviewPage {
    pub records: Vec<CandidateReviewRecord>,
    pub next_cursor: Option<String>,
}

/// CAS-guarded explicit discard command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReviewDiscard {
    pub locator: ExternalSessionLocator,
    pub expected_task_id: TaskId,
    pub expected_intent_revision_id: TaskIntentRevisionId,
    pub candidate_id: CandidateId,
    pub expected_review_version: u64,
    pub reason: String,
}

/// Exact idempotent result of one discard command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateReviewDiscardStatus {
    Discarded,
    AlreadyDiscarded,
}

/// Updated Review record plus exact discard disposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReviewDiscardOutcome {
    pub record: CandidateReviewRecord,
    pub status: CandidateReviewDiscardStatus,
}

/// Runtime-only expiration report; Git knowledge is never changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReviewCleanup {
    pub expired_candidate_ids: Vec<CandidateId>,
    pub removed_analysis_count: usize,
}

/// Durable recovery state of one exact Candidate Confirmation operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateConfirmationOperationStatus {
    Reserved,
    Committed,
}

/// Persisted server-owned Confirmation operation and complete stable plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateConfirmationOperationView {
    pub candidate_id: CandidateId,
    pub review_parent_version: u64,
    pub operation_hash: String,
    pub plan: CandidateConfirmationPlan,
    pub status: CandidateConfirmationOperationStatus,
}

/// Result of reserving or re-reading one Confirmation operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateConfirmationReservation {
    pub operation: CandidateConfirmationOperationView,
    pub created: bool,
}

/// Result of finalizing Runtime after the Git fact closure is committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateConfirmationFinalizeOutcome {
    pub operation: CandidateConfirmationOperationView,
    pub review: CandidateReviewRecord,
    pub already_confirmed: bool,
}

/// Aggregate state of one deterministic build over an immutable closed Episode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateBuildStatus {
    Pending,
    Complete,
    Incomplete,
}

impl CandidateBuildStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
        }
    }
}

/// Durable state of one Claim-scoped Candidate creation operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateBuildItemStatus {
    Prepared,
    NeedsEvidence,
    Created,
    AlreadyExists,
    Failed,
}

impl CandidateBuildItemStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::NeedsEvidence => "needs_evidence",
            Self::Created => "created",
            Self::AlreadyExists => "already_exists",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub const fn is_finalized(self) -> bool {
        matches!(self, Self::Created | Self::AlreadyExists)
    }
}

/// Builder-computed Claim readiness persisted before any Git write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBuildItemPreparation {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub content_hash: Option<String>,
    pub status: CandidateBuildItemStatus,
    pub error_code: Option<String>,
}

/// One durable Claim-scoped build item and its stable submission identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBuildItemView {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub submission_id: SubmissionId,
    pub content_hash: Option<String>,
    pub status: CandidateBuildItemStatus,
    pub candidate_id: Option<CandidateId>,
    pub event_id: Option<EventId>,
    pub error_code: Option<String>,
}

/// Durable deterministic build state for one exact closed Episode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBuildView {
    pub build_id: CandidateBuildId,
    pub source_episode: WorkEpisodeRef,
    pub final_checkpoint_id: AgentCheckpointId,
    pub status: CandidateBuildStatus,
    pub items: Vec<CandidateBuildItemView>,
}

/// Result of explicitly opening at most one Episode for an `ActiveTask`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenWorkEpisodeOutcome {
    pub episode: WorkEpisodeView,
    pub created: bool,
}

/// Result of explicitly advancing ordered Intent/Signal references.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvanceWorkEpisodeOutcome {
    pub episode: WorkEpisodeView,
    pub added_intent_revisions: usize,
    pub added_signal_refs: usize,
}

/// Result of one CAS-guarded normalized Observation append.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendWorkObservationOutcome {
    pub episode: WorkEpisodeView,
    pub observation_id: WorkObservationId,
}

/// Safe diagnostic category stored with an Episode.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkEpisodeDiagnosticKind {
    CaptureRepositoryNotConfigured,
    CaptureUnsafeArtifactPath,
}

/// One persisted safe Episode diagnostic; it never contains source payload text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkEpisodeDiagnostic {
    pub capture_id: CaptureId,
    pub kind: WorkEpisodeDiagnosticKind,
}

/// Normalized, already-redacted Capture ingestion request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureIngestion {
    pub capture_id: CaptureId,
    pub episode_id: WorkEpisodeId,
    pub expected_episode_version: u64,
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub additional_sources: Vec<WorkSourceRef>,
    pub observation: NormalizedWorkObservation,
    pub diagnostics: Vec<WorkEpisodeDiagnosticKind>,
}

/// Idempotent Capture-to-Observation commit result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestCaptureOutcome {
    pub episode: WorkEpisodeView,
    pub observation_id: WorkObservationId,
    pub inserted: bool,
}

/// Prepared close boundary consumed later by Checkpoint persistence (#157).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpisodeClosePreparation {
    pub ownership: WorkEpisodeRef,
    pub version: u64,
    pub final_intent_revision_id: TaskIntentRevisionId,
    pub observation_ids: Vec<WorkObservationId>,
}

/// Result of attempting to solidify the `ActiveTask`'s current Work Episode from an already
/// persisted Agent Checkpoint. Lifecycle Hooks never synthesize Claims or Unknowns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutomatedEpisodeBoundary {
    NoActiveTask,
    NoEpisode {
        task_session_id: TaskSessionId,
        task_id: TaskId,
        intent_revision_id: TaskIntentRevisionId,
    },
    CheckpointRequired {
        episode: WorkEpisodeView,
        intent_revision_id: TaskIntentRevisionId,
    },
    Closed {
        episode: WorkEpisodeView,
        newly_closed: bool,
    },
}

/// Verifiable source-Episode status for later Candidate admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceEpisodeVerification {
    pub ownership: WorkEpisodeRef,
    pub version: u64,
    pub status: WorkEpisodeStatus,
    pub observation_count: usize,
}

/// Owner of the installation-local `state/runtime.sqlite` database.
#[derive(Clone, Debug)]
pub struct TaskRuntime {
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
}

impl TaskRuntime {
    /// Initializes runtime state below `home/.shared-context`.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem, `SQLite`, or schema-version errors.
    pub fn initialize_for_home(home: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(home.as_ref().join(".shared-context"))
    }

    /// Initializes runtime state at an explicit installation root.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem, `SQLite`, or schema-version errors.
    pub fn initialize(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let state = root.join("state");
        fs::create_dir_all(&state).map_err(io_error("create task runtime state directory"))?;
        let runtime = Self {
            database: state.join("runtime.sqlite"),
            root,
            state,
        };
        let _connection = runtime.open_connection()?;
        Ok(runtime)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Opens the `ActiveTask` or creates an `ExternalSession` with its first Task.
    /// Existing Sessions are never implicitly switched to a new Task.
    ///
    /// # Errors
    ///
    /// Returns typed validation or storage errors.
    pub fn open_or_create_working(
        &self,
        locator: ExternalSessionLocator,
        task_id: TaskId,
        initial_intent: WorkingIntentSnapshot,
        signals: Vec<TaskSignal>,
    ) -> Result<OpenSessionOutcome> {
        locator.validate()?;
        initial_intent.validate()?;
        let requested_hash = initial_intent.canonical_semantic_hash()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin open-or-create transaction")?;
        if let Some(task_session_id) = find_active_task_by_locator(&transaction, &locator)? {
            let snapshot = require_snapshot(&transaction, task_session_id)?;
            let current = snapshot
                .current_intent_revision()
                .ok_or_else(|| invariant("existing ActiveTask has no Working Intent Head"))?;
            if snapshot.intent_revisions.len() != 1
                || current.parent_revision_id.is_some()
                || current.semantic_hash != requested_hash
            {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "ExternalSession was concurrently initialized with different Working Intent; read the current Revision and retry explicit new with CAS",
                ));
            }
            transaction
                .commit()
                .map_err(sql_error("commit existing ExternalSession transaction"))?;
            return Ok(OpenSessionOutcome {
                snapshot,
                created: false,
            });
        }

        let external_session_id = ExternalSessionId::new();
        let snapshot =
            TaskSessionSnapshot::from_initial(locator, task_id, initial_intent, signals)?;
        insert_external_session(
            &transaction,
            external_session_id,
            &snapshot.external_session_locator,
            &snapshot,
        )?;
        insert_task(&transaction, external_session_id, 0, &snapshot)?;
        let persisted = require_snapshot(&transaction, snapshot.task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit new ExternalSession transaction"))?;
        Ok(OpenSessionOutcome {
            snapshot: persisted,
            created: true,
        })
    }

    /// Mechanical pre-#167 adapter into Working Intent authority.
    ///
    /// # Errors
    ///
    /// Returns Working Intent validation or Runtime storage errors.
    #[allow(clippy::needless_pass_by_value)]
    pub fn open_or_create(
        &self,
        locator: ExternalSessionLocator,
        initial_intent: TaskIntent,
        signals: Vec<TaskSignal>,
    ) -> Result<OpenSessionOutcome> {
        let task_id = initial_intent.task_id;
        let working = TaskIntentDraft::from(&initial_intent).to_working_intent()?;
        self.open_or_create_working(locator, task_id, working, signals)
    }

    /// Explicitly creates and activates a new runtime-owned Task.
    ///
    /// # Errors
    ///
    /// Returns an input error for a missing Session, stale `ActiveTask` CAS guard,
    /// or invalid Task content and Signals.
    pub fn start_new_task_working(
        &self,
        locator: &ExternalSessionLocator,
        expected_active_task_id: TaskId,
        initial_intent: &WorkingIntentSnapshot,
        signals: Vec<TaskSignal>,
    ) -> Result<StartNewTaskOutcome> {
        locator.validate()?;
        initial_intent.validate()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin new Task transaction")?;
        let external = read_external_identity(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSessionLocator does not identify a runtime Session"))?;
        require_expected_active(external.active_task_id, expected_active_task_id)?;

        let task_id = TaskId::new();
        let snapshot = TaskSessionSnapshot::from_initial(
            locator.clone(),
            task_id,
            initial_intent.clone(),
            signals,
        )?;
        let ordinal = next_task_ordinal(&transaction, external.external_session_id)?;
        insert_task(
            &transaction,
            external.external_session_id,
            ordinal,
            &snapshot,
        )?;
        compare_and_switch(
            &transaction,
            external.external_session_id,
            expected_active_task_id,
            snapshot.task_session_id,
            snapshot.task_id,
        )?;
        let persisted = require_snapshot(&transaction, snapshot.task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit new Task transaction"))?;
        Ok(StartNewTaskOutcome {
            external_session_id: external.external_session_id,
            previous_task_id: expected_active_task_id,
            snapshot: persisted,
        })
    }

    /// Mechanical pre-#167 adapter into a new Task with Working Intent authority.
    ///
    /// # Errors
    ///
    /// Returns Working Intent validation, CAS, or Runtime storage errors.
    pub fn start_new_task(
        &self,
        locator: &ExternalSessionLocator,
        expected_active_task_id: TaskId,
        initial_intent: &TaskIntentDraft,
        signals: Vec<TaskSignal>,
    ) -> Result<StartNewTaskOutcome> {
        self.start_new_task_working(
            locator,
            expected_active_task_id,
            &initial_intent.to_working_intent()?,
            signals,
        )
    }

    /// Explicitly switches to a retained historical Task using an `ActiveTask` CAS guard.
    ///
    /// # Errors
    ///
    /// Returns an input error for missing/stale/cross-Session identities.
    pub fn switch_active_task(
        &self,
        locator: &ExternalSessionLocator,
        expected_active_task_id: TaskId,
        target_task_id: TaskId,
    ) -> Result<SwitchActiveTaskOutcome> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin ActiveTask switch transaction")?;
        let external = read_external_identity(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSessionLocator does not identify a runtime Session"))?;
        require_expected_active(external.active_task_id, expected_active_task_id)?;
        let target_task_session_id = find_task_in_external_session(
            &transaction,
            external.external_session_id,
            target_task_id,
        )?
        .ok_or_else(|| invalid("target_task_id is not retained by this ExternalSession"))?;
        let switched = target_task_id != expected_active_task_id;
        if switched {
            compare_and_switch(
                &transaction,
                external.external_session_id,
                expected_active_task_id,
                target_task_session_id,
                target_task_id,
            )?;
        }
        let snapshot = require_snapshot(&transaction, target_task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit ActiveTask switch transaction"))?;
        Ok(SwitchActiveTaskOutcome {
            external_session_id: external.external_session_id,
            previous_task_id: expected_active_task_id,
            snapshot,
            switched,
        })
    }

    /// Appends one Intent revision only to the current `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Returns an input error for inactive/cross-Task/stale-parent data.
    pub fn append_working_intent_revision(
        &self,
        task_session_id: TaskSessionId,
        parent_revision_id: TaskIntentRevisionId,
        working_intent: WorkingIntentSnapshot,
    ) -> Result<AppendIntentRevisionOutcome> {
        working_intent.validate()?;
        let semantic_hash = working_intent.canonical_semantic_hash()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Intent append transaction")?;
        let (task_id, current_revision_id) = read_active_task_head(&transaction, task_session_id)?
            .ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        if parent_revision_id != current_revision_id {
            let current = read_intent_revision(&transaction, current_revision_id)?;
            if current.parent_revision_id == Some(parent_revision_id)
                && current.semantic_hash == semantic_hash
            {
                transaction.commit().map_err(sql_error(
                    "commit concurrent already-current Intent transaction",
                ))?;
                return Ok(AppendIntentRevisionOutcome {
                    revision: current,
                    status: IntentRevisionWriteStatus::AlreadyCurrent,
                });
            }
            reject_invalid_parent(&transaction, task_session_id, task_id, parent_revision_id)?;
        }
        let current = read_intent_revision(&transaction, current_revision_id)?;
        if current.semantic_hash == semantic_hash {
            transaction
                .commit()
                .map_err(sql_error("commit already-current Intent transaction"))?;
            return Ok(AppendIntentRevisionOutcome {
                revision: current,
                status: IntentRevisionWriteStatus::AlreadyCurrent,
            });
        }
        let revision = TaskIntentRevision::successor(&current, working_intent)?;
        let ordinal = read_revision_ordinal(&transaction, current_revision_id)?
            .checked_add(1)
            .ok_or_else(|| invariant("Task Intent revision ordinal overflow"))?;
        insert_intent_revision(&transaction, task_session_id, ordinal, &revision)?;
        let changed = transaction
            .execute(
                "UPDATE task_session SET current_intent_revision_id = ?1
                 WHERE task_session_id = ?2 AND current_intent_revision_id = ?3",
                params![
                    revision.revision_id.to_string(),
                    task_session_id.to_string(),
                    parent_revision_id.to_string(),
                ],
            )
            .map_err(sql_error("advance Task Intent Head"))?;
        if changed != 1 {
            return Err(invariant(
                "Task Intent Head changed inside write transaction",
            ));
        }
        transaction
            .commit()
            .map_err(sql_error("commit Intent append transaction"))?;
        Ok(AppendIntentRevisionOutcome {
            revision,
            status: IntentRevisionWriteStatus::Created,
        })
    }

    /// Mechanical pre-#167 adapter for legacy Task-bound Intent input.
    ///
    /// # Errors
    ///
    /// Returns ownership, Working Intent validation, CAS, or storage errors.
    #[allow(clippy::needless_pass_by_value)]
    pub fn append_intent_revision(
        &self,
        task_session_id: TaskSessionId,
        parent_revision_id: TaskIntentRevisionId,
        intent: TaskIntent,
    ) -> Result<AppendIntentRevisionOutcome> {
        let snapshot = self
            .read_snapshot(task_session_id)?
            .ok_or_else(|| invalid("Task Session does not exist"))?;
        if intent.task_id != snapshot.task_id {
            return Err(invalid("Task Intent belongs to another Task"));
        }
        self.append_working_intent_revision(
            task_session_id,
            parent_revision_id,
            TaskIntentDraft::from(&intent).to_working_intent()?,
        )
    }

    /// Merges normalized Signals only into the current `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Returns an input error for an inactive Task or invalid Signal.
    pub fn merge_signals(
        &self,
        task_session_id: TaskSessionId,
        signals: Vec<TaskSignal>,
    ) -> Result<MergeSignalsOutcome> {
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Signal merge transaction")?;
        let (task_id, _) =
            read_active_task_head(&transaction, task_session_id)?.ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        let outcome =
            merge_signals_in_transaction(&transaction, task_session_id, task_id, &signals)?;
        transaction
            .commit()
            .map_err(sql_error("commit Signal merge transaction"))?;
        Ok(outcome)
    }

    /// Finds one `ExternalSession` and merges Signals into its `ActiveTask`.
    /// A missing locator returns `None` and never creates a Task.
    ///
    /// # Errors
    ///
    /// Returns typed input or storage errors.
    pub fn merge_signals_by_locator(
        &self,
        locator: &ExternalSessionLocator,
        signals: Vec<TaskSignal>,
    ) -> Result<Option<MergeSignalsOutcome>> {
        locator.validate()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin locator Signal merge transaction")?;
        let Some(task_session_id) = find_active_task_by_locator(&transaction, locator)? else {
            transaction
                .commit()
                .map_err(sql_error("commit missing locator transaction"))?;
            return Ok(None);
        };
        let (task_id, _) = read_active_task_head(&transaction, task_session_id)?
            .ok_or_else(|| invariant("located ActiveTask is not active"))?;
        let outcome =
            merge_signals_in_transaction(&transaction, task_session_id, task_id, &signals)?;
        transaction
            .commit()
            .map_err(sql_error("commit locator Signal merge transaction"))?;
        Ok(Some(outcome))
    }

    /// Supersedes stable Signal IDs without deleting history.
    ///
    /// # Errors
    ///
    /// Returns an input error for stale Task identity or invalid Signal IDs.
    pub fn supersede_signals(
        &self,
        task_session_id: TaskSessionId,
        expected_active_task_id: TaskId,
        signal_ids: Vec<SignalId>,
    ) -> Result<SupersedeSignalsOutcome> {
        require_unique_signal_ids(&signal_ids)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Signal supersede transaction")?;
        let (task_id, _) =
            read_active_task_head(&transaction, task_session_id)?.ok_or_else(|| {
                invalid("task_session_id does not identify the ExternalSession ActiveTask")
            })?;
        require_expected_active(task_id, expected_active_task_id)?;
        for signal_id in &signal_ids {
            let record = read_signal_record(&transaction, *signal_id)?
                .ok_or_else(|| invalid(format!("Signal does not exist: {signal_id}")))?;
            record.validate_for_task(task_session_id, task_id)?;
            if record.lifecycle != TaskSignalLifecycle::Active {
                return Err(invalid(format!(
                    "Signal is already superseded: {signal_id}"
                )));
            }
        }
        for signal_id in &signal_ids {
            let changed = transaction
                .execute(
                    "UPDATE task_signal SET lifecycle = 'superseded'
                     WHERE signal_id = ?1 AND lifecycle = 'active'",
                    [signal_id.to_string()],
                )
                .map_err(sql_error("supersede Task Signal"))?;
            if changed != 1 {
                return Err(invariant(
                    "Signal lifecycle changed inside write transaction",
                ));
            }
        }
        let snapshot = require_snapshot(&transaction, task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Signal supersede transaction"))?;
        Ok(SupersedeSignalsOutcome {
            snapshot,
            superseded_signal_ids: signal_ids,
        })
    }

    /// Explicitly opens the `ActiveTask`'s sole open Work Episode.
    ///
    /// Repeated and concurrent calls converge on the existing open Episode.
    /// Hook ingestion never calls this method implicitly.
    ///
    /// # Errors
    ///
    /// Rejects missing/stale/cross-Task ownership or storage failures.
    pub fn open_work_episode(
        &self,
        locator: &ExternalSessionLocator,
        expected_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
    ) -> Result<OpenWorkEpisodeOutcome> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Work Episode open transaction")?;
        let external = read_external_identity(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask for Work Episode"))?;
        require_expected_active(external.active_task_id, expected_task_id)?;
        let (task_id, current_revision_id) =
            read_active_task_head(&transaction, external.active_task_session_id)?
                .ok_or_else(|| invariant("located ActiveTask is not active"))?;
        if current_revision_id != expected_intent_revision_id {
            return Err(invalid("expected Intent revision is stale"));
        }
        if let Some(episode_id) = find_open_episode(&transaction, external.active_task_session_id)?
        {
            let episode = require_episode_view(&transaction, episode_id)?;
            transaction
                .commit()
                .map_err(sql_error("commit existing Work Episode transaction"))?;
            return Ok(OpenWorkEpisodeOutcome {
                episode,
                created: false,
            });
        }
        let episode_id = WorkEpisodeId::new();
        let episode_ordinal = next_episode_ordinal(&transaction, external.active_task_session_id)?;
        transaction
            .execute(
                "INSERT INTO work_episode (
                    episode_id, task_session_id, task_id, version, status,
                    final_checkpoint_id, episode_ordinal
                 ) VALUES (?1, ?2, ?3, 0, 'open', NULL, ?4)",
                params![
                    episode_id.to_string(),
                    external.active_task_session_id.to_string(),
                    task_id.to_string(),
                    episode_ordinal,
                ],
            )
            .map_err(sql_error("insert Work Episode"))?;
        insert_all_missing_episode_refs(
            &transaction,
            episode_id,
            external.active_task_session_id,
            task_id,
        )?;
        let episode = require_episode_view(&transaction, episode_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Work Episode open transaction"))?;
        Ok(OpenWorkEpisodeOutcome {
            episode,
            created: true,
        })
    }

    /// Explicitly advances one open Episode to every currently persisted Intent
    /// revision and Signal reference of its exact Task.
    ///
    /// # Errors
    ///
    /// Rejects stale Episode version, closed/missing Episode, or storage failures.
    pub fn advance_work_episode_refs(
        &self,
        episode_id: WorkEpisodeId,
        expected_version: u64,
    ) -> Result<AdvanceWorkEpisodeOutcome> {
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Work Episode ref advance")?;
        let (task_session_id, task_id, version, status) =
            require_episode_head(&transaction, episode_id)?;
        require_open_episode_version(version, &status, expected_version)?;
        require_task_is_active(&transaction, task_session_id, task_id)?;
        let (added_intent_revisions, added_signal_refs) =
            insert_all_missing_episode_refs(&transaction, episode_id, task_session_id, task_id)?;
        if added_intent_revisions > 0 || added_signal_refs > 0 {
            advance_episode_version(&transaction, episode_id, expected_version)?;
        }
        let episode = require_episode_view(&transaction, episode_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Work Episode ref advance"))?;
        Ok(AdvanceWorkEpisodeOutcome {
            episode,
            added_intent_revisions,
            added_signal_refs,
        })
    }

    /// Appends a normalized non-Capture observation under Episode-version CAS.
    ///
    /// Capture sources must use [`Self::ingest_capture`] so `CaptureId`
    /// idempotency cannot be bypassed.
    ///
    /// # Errors
    ///
    /// Rejects stale/closed ownership, Capture sources, invalid meaning, or storage failures.
    pub fn append_work_observation(
        &self,
        episode_id: WorkEpisodeId,
        expected_version: u64,
        intent_revision_id: TaskIntentRevisionId,
        source_refs: Vec<WorkSourceRef>,
        observation: NormalizedWorkObservation,
    ) -> Result<AppendWorkObservationOutcome> {
        if source_refs
            .iter()
            .any(|source| matches!(source, WorkSourceRef::Capture(_)))
        {
            return Err(invalid(
                "Capture sources require the idempotent ingest_capture API",
            ));
        }
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Work Observation append")?;
        let observation_id = append_observation_in_transaction(
            &transaction,
            episode_id,
            expected_version,
            intent_revision_id,
            source_refs,
            observation,
        )?;
        let episode = require_episode_view(&transaction, episode_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Work Observation append"))?;
        Ok(AppendWorkObservationOutcome {
            episode,
            observation_id,
        })
    }

    /// Atomically and idempotently converts one already-claimed redacted Capture
    /// into one server-identified Work Observation.
    ///
    /// # Errors
    ///
    /// Rejects stale/closed/cross-Task input or conflicting Capture reuse.
    #[allow(clippy::too_many_lines)]
    pub fn ingest_capture(&self, input: &CaptureIngestion) -> Result<IngestCaptureOutcome> {
        if input
            .additional_sources
            .iter()
            .any(|source| matches!(source, WorkSourceRef::Capture(_)))
        {
            return Err(invalid(
                "Capture ingestion supplies its Capture source server-side",
            ));
        }
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Capture ingestion")?;
        if let Some((episode_id, observation_id, task_session_id, task_id)) =
            read_capture_ingestion(&transaction, input.capture_id)?
        {
            if episode_id != input.episode_id
                || task_session_id != input.task_session_id
                || task_id != input.task_id
            {
                return Err(invalid(
                    "CaptureId is already ingested by another Task/Episode",
                ));
            }
            let episode = require_episode_view(&transaction, episode_id)?;
            let observation = episode
                .episode
                .observations
                .iter()
                .find(|observation| observation.observation_id == observation_id)
                .ok_or_else(|| invariant("Capture ingestion Observation disappeared"))?;
            let mut expected_sources = Vec::with_capacity(input.additional_sources.len() + 1);
            expected_sources.push(WorkSourceRef::Capture(CaptureSourceRef {
                capture_id: input.capture_id,
                task_session_id: input.task_session_id,
                task_id: input.task_id,
            }));
            expected_sources.extend(input.additional_sources.clone());
            let actual_diagnostics = episode
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.capture_id == input.capture_id)
                .map(|diagnostic| diagnostic.kind)
                .collect::<BTreeSet<_>>();
            let expected_diagnostics = input.diagnostics.iter().copied().collect::<BTreeSet<_>>();
            if observation.intent_revision_id != input.intent_revision_id
                || observation.source_refs != expected_sources
                || observation.observation != input.observation
                || actual_diagnostics != expected_diagnostics
            {
                return Err(invalid(
                    "CaptureId retry content differs from persisted ingestion",
                ));
            }
            transaction
                .commit()
                .map_err(sql_error("commit idempotent Capture ingestion"))?;
            return Ok(IngestCaptureOutcome {
                episode,
                observation_id,
                inserted: false,
            });
        }
        let (task_session_id, task_id, version, status) =
            require_episode_head(&transaction, input.episode_id)?;
        require_open_episode_version(version, &status, input.expected_episode_version)?;
        if task_session_id != input.task_session_id || task_id != input.task_id {
            return Err(invalid("Capture ingestion owner differs from Work Episode"));
        }
        let mut source_refs = Vec::with_capacity(input.additional_sources.len() + 1);
        source_refs.push(WorkSourceRef::Capture(CaptureSourceRef {
            capture_id: input.capture_id,
            task_session_id,
            task_id,
        }));
        source_refs.extend(input.additional_sources.clone());
        let observation_id = append_observation_in_transaction(
            &transaction,
            input.episode_id,
            input.expected_episode_version,
            input.intent_revision_id,
            source_refs,
            input.observation.clone(),
        )?;
        transaction
            .execute(
                "INSERT INTO capture_ingestion (
                    capture_id, episode_id, observation_id, task_session_id, task_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    input.capture_id.to_string(),
                    input.episode_id.to_string(),
                    observation_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                ],
            )
            .map_err(sql_error("record Capture ingestion"))?;
        insert_episode_diagnostics(
            &transaction,
            input.episode_id,
            input.capture_id,
            &input.diagnostics,
        )?;
        let episode = require_episode_view(&transaction, input.episode_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Capture ingestion"))?;
        Ok(IngestCaptureOutcome {
            episode,
            observation_id,
            inserted: true,
        })
    }

    /// Atomically opens or reuses the `ActiveTask` Episode, advances its typed
    /// references, records inline Validation observations and persists one
    /// server-identified Agent Checkpoint.
    ///
    /// Semantic retries are keyed by Episode and parent version. Identical
    /// content returns the original Checkpoint; different content conflicts.
    ///
    /// # Errors
    ///
    /// Rejects stale ownership/version guards, invalid Task-local references,
    /// incomplete Checkpoint content, or conflicting semantic retries.
    #[allow(clippy::too_many_lines)]
    pub fn write_agent_checkpoint(
        &self,
        input: &AgentCheckpointWrite,
    ) -> Result<AgentCheckpointOutcome> {
        input.locator.validate()?;
        if input.claims.is_empty() && input.unknowns.is_empty() {
            return Err(invalid(
                "agent_checkpoint must contain at least one Claim or Unknown",
            ));
        }
        let semantic_json = checkpoint_semantic_json(input)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Agent Checkpoint transaction")?;
        let external = read_external_identity(&transaction, &input.locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask for Agent Checkpoint"))?;
        require_expected_active(external.active_task_id, input.expected_task_id)?;
        let (task_id, current_revision_id) =
            read_active_task_head(&transaction, external.active_task_session_id)?
                .ok_or_else(|| invariant("located ActiveTask is not active"))?;
        if current_revision_id != input.expected_intent_revision_id {
            return Err(stale("expected Intent revision is stale"));
        }

        let open_episode = find_open_episode(&transaction, external.active_task_session_id)?;
        let retry_episode = if open_episode.is_none() {
            find_latest_checkpoint_episode(
                &transaction,
                external.active_task_session_id,
                input.expected_episode_version,
            )?
        } else {
            None
        };
        if let Some(episode_id) = open_episode.or(retry_episode) {
            if let Some((checkpoint, persisted_semantics)) =
                read_checkpoint_by_parent(&transaction, episode_id, input.expected_episode_version)?
            {
                if persisted_semantics != semantic_json {
                    return Err(conflict(
                        "Agent Checkpoint parent version already contains different content",
                    ));
                }
                let episode = require_episode_view(&transaction, episode_id)?;
                let inline_observation_ids = inline_observation_ids(&checkpoint, &input.claims)?;
                transaction
                    .commit()
                    .map_err(sql_error("commit idempotent Agent Checkpoint retry"))?;
                return Ok(AgentCheckpointOutcome {
                    checkpoint,
                    episode,
                    created: false,
                    inline_observation_ids,
                });
            }
        }

        let episode_id = if let Some(episode_id) = open_episode {
            episode_id
        } else {
            if input.expected_episode_version != 0 {
                return Err(stale("expected Work Episode version is stale"));
            }
            insert_open_episode(&transaction, external.active_task_session_id, task_id)?
        };
        let (task_session_id, episode_task_id, version, status) =
            require_episode_head(&transaction, episode_id)?;
        require_open_episode_version(version, &status, input.expected_episode_version)?;
        if task_session_id != external.active_task_session_id || episode_task_id != task_id {
            return Err(invariant("ActiveTask Work Episode ownership changed"));
        }
        insert_all_missing_episode_refs(&transaction, episode_id, task_session_id, task_id)?;
        let episode_before = require_episode_view(&transaction, episode_id)?.episode;
        validate_checkpoint_task_local_refs(&episode_before, &input.claims)?;

        let mut claims = Vec::with_capacity(input.claims.len());
        let mut inserted_inline_observations = Vec::new();
        for claim in &input.claims {
            let mut evidence_refs = claim.evidence_refs.clone();
            for evidence in &claim.inline_validations {
                evidence.validate("agent_checkpoint.inline_validation")?;
                let observation = WorkObservation::from_parts(
                    task_session_id,
                    task_id,
                    current_revision_id,
                    Vec::new(),
                    NormalizedWorkObservation::InlineValidation {
                        evidence: evidence.clone(),
                    },
                )?;
                insert_observation_rows(&transaction, episode_id, &observation)?;
                evidence_refs.push(CaptureEvidenceRef::Observation {
                    observation_id: observation.observation_id,
                });
                inserted_inline_observations.push(observation.observation_id);
            }
            claims.push(CheckpointClaim::from_parts(
                claim.context_kind_hint,
                claim.topic_key_hint.clone(),
                claim.statement.clone(),
                claim.rationale.clone(),
                claim.applicability.clone(),
                claim.assumptions.clone(),
                claim.recheck_when.clone(),
                evidence_refs,
                claim.artifact_refs.clone(),
                claim.related_contexts.clone(),
            )?);
        }
        let episode_with_inline = require_episode_view(&transaction, episode_id)?.episode;
        let checkpoint = AgentCheckpoint::from_parts(
            &episode_with_inline,
            current_revision_id,
            claims,
            input.unknowns.clone(),
        )?;
        let checkpoint_json =
            serde_json::to_string(&checkpoint).map_err(json_error("serialize Agent Checkpoint"))?;
        let checkpoint_ordinal = next_checkpoint_ordinal(&transaction, episode_id)?;
        transaction
            .execute(
                "INSERT INTO agent_checkpoint (
                    checkpoint_id, episode_id, task_session_id, task_id,
                    intent_revision_id, parent_episode_version, boundary,
                    semantic_json, checkpoint_json, checkpoint_ordinal
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    checkpoint.checkpoint_id.to_string(),
                    episode_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                    current_revision_id.to_string(),
                    i64::try_from(input.expected_episode_version)
                        .map_err(|_| invalid("Work Episode version exceeds SQLite range"))?,
                    input.boundary.as_str(),
                    semantic_json,
                    checkpoint_json,
                    checkpoint_ordinal,
                ],
            )
            .map_err(sql_error("insert Agent Checkpoint"))?;
        match input.boundary {
            CheckpointBoundary::Continue => {
                advance_episode_version(&transaction, episode_id, input.expected_episode_version)?;
            }
            CheckpointBoundary::Close => {
                let mut validation_episode = episode_with_inline;
                validation_episode.close(&checkpoint)?;
                close_episode_version(
                    &transaction,
                    episode_id,
                    input.expected_episode_version,
                    checkpoint.checkpoint_id,
                )?;
            }
        }
        let episode = require_episode_view(&transaction, episode_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Agent Checkpoint transaction"))?;
        Ok(AgentCheckpointOutcome {
            checkpoint,
            episode,
            created: true,
            inline_observation_ids: inserted_inline_observations,
        })
    }

    /// Reads one persisted Work Episode by server-owned ID.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_work_episode(&self, episode_id: WorkEpisodeId) -> Result<Option<WorkEpisodeView>> {
        read_episode_view(&self.open_connection()?, episode_id)
    }

    /// Lists a bounded ordered Task-local Episode history.
    ///
    /// # Errors
    ///
    /// Rejects invalid bounds or storage failures.
    pub fn list_work_episodes(
        &self,
        task_session_id: TaskSessionId,
        limit: usize,
    ) -> Result<Vec<WorkEpisodeView>> {
        if limit == 0 || limit > MAX_EPISODE_LIST_LIMIT {
            return Err(invalid(format!(
                "Work Episode list limit must be between 1 and {MAX_EPISODE_LIST_LIMIT}"
            )));
        }
        let connection = self.open_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT episode_id FROM work_episode
                 WHERE task_session_id = ?1 ORDER BY episode_ordinal ASC LIMIT ?2",
            )
            .map_err(sql_error("prepare Work Episode list"))?;
        let limit = i64::try_from(limit).map_err(|_| invalid("Episode list limit overflow"))?;
        let ids = statement
            .query_map(params![task_session_id.to_string(), limit], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql_error("query Work Episode list"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error("read Work Episode list row"))?;
        drop(statement);
        ids.into_iter()
            .map(|value| {
                let episode_id = parse_id(&value, "work_episode.episode_id")?;
                read_episode_view(&connection, episode_id)?
                    .ok_or_else(|| invariant("listed Work Episode disappeared"))
            })
            .collect()
    }

    /// Prepares, but does not commit, a final Checkpoint close boundary.
    ///
    /// # Errors
    ///
    /// Rejects stale or closed Episodes.
    pub fn prepare_work_episode_close(
        &self,
        episode_id: WorkEpisodeId,
        expected_version: u64,
    ) -> Result<EpisodeClosePreparation> {
        let view = self
            .read_work_episode(episode_id)?
            .ok_or_else(|| invalid("Work Episode does not exist"))?;
        if view.episode.version != expected_version
            || view.episode.status != WorkEpisodeStatus::Open
        {
            return Err(invalid("Work Episode close version/status is stale"));
        }
        Ok(EpisodeClosePreparation {
            ownership: view.episode.ownership(),
            version: view.episode.version,
            final_intent_revision_id: view.episode.intent_revisions.last(),
            observation_ids: view
                .episode
                .observations
                .iter()
                .map(|observation| observation.observation_id)
                .collect(),
        })
    }

    /// Closes the `ActiveTask`'s open Episode at its latest current-Intent Checkpoint.
    ///
    /// This is the narrow lifecycle-Hook boundary: it may advance ordered Intent/Signal refs and
    /// close an Episode, but it never creates a Checkpoint, Claim, Unknown, Observation, or
    /// Candidate. A duplicate call returns the latest already-closed Episode so the application
    /// service can recover a missing Builder step idempotently.
    ///
    /// # Errors
    ///
    /// Returns typed locator, storage, ownership, or persisted-state errors. Missing Tasks,
    /// Episodes, and current Checkpoints are ordinary typed outcomes rather than failures.
    pub fn close_checkpointed_work_episode(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<AutomatedEpisodeBoundary> {
        self.close_checkpointed_work_episode_guarded(locator, None)
    }

    /// Explicit CAS fallback for closing an already persisted Checkpoint when a lifecycle Hook is
    /// unavailable or its completion is uncertain.
    ///
    /// # Errors
    ///
    /// Returns stale state when Task, Intent, or Episode version changed. It never creates a new
    /// Checkpoint or accepts caller-authored Claim content.
    pub fn close_checkpointed_work_episode_cas(
        &self,
        locator: &ExternalSessionLocator,
        expected_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
        expected_episode_version: u64,
    ) -> Result<AutomatedEpisodeBoundary> {
        self.close_checkpointed_work_episode_guarded(
            locator,
            Some((
                expected_task_id,
                expected_intent_revision_id,
                expected_episode_version,
            )),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn close_checkpointed_work_episode_guarded(
        &self,
        locator: &ExternalSessionLocator,
        expected: Option<(TaskId, TaskIntentRevisionId, u64)>,
    ) -> Result<AutomatedEpisodeBoundary> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin automated Episode boundary")?;
        let Some(external) = read_external_identity(&transaction, locator)? else {
            transaction
                .commit()
                .map_err(sql_error("commit missing automated ActiveTask"))?;
            return Ok(AutomatedEpisodeBoundary::NoActiveTask);
        };
        let (task_id, intent_revision_id) =
            read_active_task_head(&transaction, external.active_task_session_id)?
                .ok_or_else(|| invariant("located automated ActiveTask is not active"))?;
        if task_id != external.active_task_id {
            return Err(invariant(
                "ExternalSession ActiveTask identity disagrees with its TaskSession",
            ));
        }
        if expected.is_some_and(|(expected_task_id, expected_intent_revision_id, _)| {
            expected_task_id != task_id || expected_intent_revision_id != intent_revision_id
        }) {
            return Err(stale(
                "automated Episode Task/Intent ownership CAS is stale",
            ));
        }
        let Some(episode_id) = find_open_episode(&transaction, external.active_task_session_id)?
        else {
            let latest = find_latest_episode(&transaction, external.active_task_session_id)?
                .map(|episode_id| require_episode_view(&transaction, episode_id))
                .transpose()?;
            transaction
                .commit()
                .map_err(sql_error("commit duplicate automated Episode boundary"))?;
            return Ok(match latest {
                Some(episode)
                    if matches!(episode.episode.status, WorkEpisodeStatus::Closed { .. }) =>
                {
                    if expected.is_some_and(|(_, _, expected_version)| {
                        expected_version
                            .checked_add(1)
                            .is_none_or(|closed_version| closed_version != episode.episode.version)
                    }) {
                        return Err(stale("automated closed Episode version CAS is stale"));
                    }
                    AutomatedEpisodeBoundary::Closed {
                        episode,
                        newly_closed: false,
                    }
                }
                Some(_) => {
                    return Err(invariant(
                        "latest open Work Episode was absent from the open-Episode lookup",
                    ));
                }
                None => AutomatedEpisodeBoundary::NoEpisode {
                    task_session_id: external.active_task_session_id,
                    task_id,
                    intent_revision_id,
                },
            });
        };
        let episode = require_episode_view(&transaction, episode_id)?;
        if expected
            .is_some_and(|(_, _, expected_version)| expected_version != episode.episode.version)
        {
            return Err(stale("automated open Episode version CAS is stale"));
        }
        let Some(checkpoint) = episode.checkpoints.last() else {
            transaction
                .commit()
                .map_err(sql_error("commit missing automated Checkpoint"))?;
            return Ok(AutomatedEpisodeBoundary::CheckpointRequired {
                episode,
                intent_revision_id,
            });
        };
        if checkpoint.intent_revision_id != intent_revision_id {
            transaction
                .commit()
                .map_err(sql_error("commit stale automated Checkpoint"))?;
            return Ok(AutomatedEpisodeBoundary::CheckpointRequired {
                episode,
                intent_revision_id,
            });
        }
        insert_all_missing_episode_refs(
            &transaction,
            episode_id,
            external.active_task_session_id,
            task_id,
        )?;
        let mut validation_episode = require_episode_view(&transaction, episode_id)?.episode;
        validation_episode.close(checkpoint)?;
        close_episode_version(
            &transaction,
            episode_id,
            episode.episode.version,
            checkpoint.checkpoint_id,
        )?;
        let episode = require_episode_view(&transaction, episode_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit automated Episode boundary"))?;
        Ok(AutomatedEpisodeBoundary::Closed {
            episode,
            newly_closed: true,
        })
    }

    /// Verifies that a later Candidate source Episode exists with typed owner/status.
    ///
    /// # Errors
    ///
    /// Returns storage/invariant errors; absence remains `None`.
    pub fn verify_source_episode(
        &self,
        episode_id: WorkEpisodeId,
    ) -> Result<Option<SourceEpisodeVerification>> {
        Ok(self
            .read_work_episode(episode_id)?
            .map(|view| SourceEpisodeVerification {
                ownership: view.episode.ownership(),
                version: view.episode.version,
                status: view.episode.status,
                observation_count: view.episode.observations.len(),
            }))
    }

    /// Reserves one stable `BuildId` and one stable `SubmissionId` per exact Checkpoint Claim before
    /// any Candidate Git write.
    ///
    /// Semantic retries reuse the persisted identities. A previously incomplete Evidence item may
    /// become prepared when the same immutable source resolves from a later healthy Index read.
    ///
    /// # Errors
    ///
    /// Rejects an open/missing Episode, a non-exhaustive Claim set, invalid readiness metadata, or
    /// deterministic content drift for an already prepared item.
    pub fn prepare_candidate_build(
        &self,
        episode_id: WorkEpisodeId,
        items: &[CandidateBuildItemPreparation],
    ) -> Result<CandidateBuildView> {
        validate_build_preparations(items)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Build preparation")?;
        let episode = require_episode_view(&transaction, episode_id)?;
        let WorkEpisodeStatus::Closed {
            final_checkpoint_id,
        } = episode.episode.status
        else {
            return Err(invalid(
                "Candidate Builder requires an exact closed Work Episode",
            ));
        };
        validate_build_claim_coverage(&episode, items)?;
        let build_id = if let Some(build_id) = read_candidate_build_id(&transaction, episode_id)? {
            build_id
        } else {
            let build_id = CandidateBuildId::new();
            transaction
                .execute(
                    "INSERT INTO candidate_build (
                            build_id, episode_id, task_session_id, task_id,
                            final_checkpoint_id, status
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        build_id.to_string(),
                        episode_id.to_string(),
                        episode.episode.task_session_id.to_string(),
                        episode.episode.task_id.to_string(),
                        final_checkpoint_id.to_string(),
                        CandidateBuildStatus::Pending.as_str(),
                    ],
                )
                .map_err(sql_error("insert Candidate Build"))?;
            build_id
        };
        upsert_candidate_build_items(&transaction, build_id, items)?;
        refresh_candidate_build_status(&transaction, build_id)?;
        let view = require_candidate_build_view(&transaction, build_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Build preparation"))?;
        Ok(view)
    }

    /// Records the result of one #117 Candidate submission after the Git boundary returns.
    ///
    /// # Errors
    ///
    /// Rejects unknown/mismatched items, incomplete success identity, or a conflicting finalized
    /// result for the same stable `SubmissionId`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_candidate_build_item_result(
        &self,
        build_id: CandidateBuildId,
        submission_id: SubmissionId,
        status: CandidateBuildItemStatus,
        candidate_id: Option<CandidateId>,
        event_id: Option<EventId>,
        error_code: Option<&str>,
    ) -> Result<CandidateBuildView> {
        self.record_candidate_build_item_result_at(
            build_id,
            submission_id,
            status,
            candidate_id,
            event_id,
            error_code,
            DEFAULT_CANDIDATE_REVIEW_TTL,
            unix_seconds(SystemTime::now())?,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_candidate_build_item_result_at(
        &self,
        build_id: CandidateBuildId,
        submission_id: SubmissionId,
        status: CandidateBuildItemStatus,
        candidate_id: Option<CandidateId>,
        event_id: Option<EventId>,
        error_code: Option<&str>,
        review_ttl: Duration,
        now_unix_seconds: u64,
    ) -> Result<CandidateBuildView> {
        validate_build_item_result(status, candidate_id, event_id, error_code)?;
        if review_ttl.is_zero() || review_ttl > MAX_CANDIDATE_REVIEW_TTL {
            return Err(invalid("Candidate Review TTL is outside the safe bound"));
        }
        let expires_at_unix_seconds = now_unix_seconds
            .checked_add(review_ttl.as_secs())
            .ok_or_else(|| invalid("Candidate Review expiration overflows Unix time"))?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Build result")?;
        let existing = read_candidate_build_item(&transaction, build_id, submission_id)?
            .ok_or_else(|| invalid("Candidate Build item does not exist"))?;
        if existing.status.is_finalized() {
            if existing.candidate_id != candidate_id || existing.event_id != event_id {
                return Err(invariant(
                    "finalized Candidate Build item identity changed across retry",
                ));
            }
        } else {
            transaction
                .execute(
                    "UPDATE candidate_build_item
                     SET status = ?1, candidate_id = ?2, event_id = ?3, error_code = ?4
                     WHERE build_id = ?5 AND submission_id = ?6",
                    params![
                        status.as_str(),
                        candidate_id.map(|id| id.to_string()),
                        event_id.map(|id| id.to_string()),
                        error_code,
                        build_id.to_string(),
                        submission_id.to_string(),
                    ],
                )
                .map_err(sql_error("update Candidate Build item result"))?;
        }
        if status.is_finalized() {
            let candidate_id = candidate_id
                .ok_or_else(|| invariant("finalized Candidate Build item lacks CandidateId"))?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO candidate_review (
                        candidate_id, submission_id, episode_id, task_session_id, task_id,
                        build_id, final_checkpoint_id, checkpoint_id, claim_id,
                        review_version, status, discard_reason, created_at_unix_seconds,
                        expires_at_unix_seconds, discarded_at_unix_seconds,
                        expired_at_unix_seconds, confirmation_id, result_context_id
                     )
                     SELECT ?1, item.submission_id, build.episode_id, build.task_session_id,
                            build.task_id, build.build_id, build.final_checkpoint_id,
                            item.checkpoint_id, item.claim_id, 1, 'pending', NULL, ?2, ?3,
                            NULL, NULL, NULL, NULL
                     FROM candidate_build_item AS item
                     JOIN candidate_build AS build ON build.build_id = item.build_id
                     WHERE item.build_id = ?4 AND item.submission_id = ?5
                       AND item.candidate_id = ?1
                       AND item.status IN ('created', 'already_exists')",
                    params![
                        candidate_id.to_string(),
                        i64::try_from(now_unix_seconds).map_err(|_| invalid(
                            "Candidate Review timestamp exceeds SQLite range"
                        ))?,
                        i64::try_from(expires_at_unix_seconds).map_err(|_| invalid(
                            "Candidate Review expiration exceeds SQLite range"
                        ))?,
                        build_id.to_string(),
                        submission_id.to_string(),
                    ],
                )
                .map_err(sql_error("initialize Candidate Review"))?;
        }
        refresh_candidate_build_status(&transaction, build_id)?;
        let view = require_candidate_build_view(&transaction, build_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Build result"))?;
        Ok(view)
    }

    /// Reads one persisted Candidate Build by its closed Episode identity.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_candidate_build(
        &self,
        episode_id: WorkEpisodeId,
    ) -> Result<Option<CandidateBuildView>> {
        let connection = self.open_connection()?;
        read_candidate_build_id(&connection, episode_id)?
            .map(|build_id| require_candidate_build_view(&connection, build_id))
            .transpose()
    }

    /// Atomically replaces the current rebuildable review analysis for one persisted Candidate.
    ///
    /// # Errors
    ///
    /// Rejects a Candidate absent from the finalized Builder result, mismatched Episode sources,
    /// invalid derived review state, or storage failures.
    pub fn replace_candidate_analysis(
        &self,
        candidate: &AutomaticContextCandidate,
    ) -> Result<CandidateAnalysisView> {
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate analysis replacement")?;
        let episode_id = transaction
            .query_row(
                "SELECT build.episode_id
                 FROM candidate_build_item AS item
                 JOIN candidate_build AS build ON build.build_id = item.build_id
                 WHERE item.candidate_id = ?1
                   AND item.status IN ('created', 'already_exists')",
                [candidate.candidate_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error("locate Candidate analysis Builder source"))?
            .ok_or_else(|| invalid("Candidate analysis target is not a finalized Builder item"))?;
        let episode_id = parse_id(&episode_id, "candidate_analysis.episode_id")?;
        let episode = require_episode_view(&transaction, episode_id)?;
        candidate.validate_against_sources(&episode.episode, &episode.checkpoints)?;
        let prior_generation = transaction
            .query_row(
                "SELECT analysis_generation FROM candidate_analysis WHERE candidate_id = ?1",
                [candidate.candidate_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql_error("read Candidate analysis generation"))?
            .unwrap_or(0);
        let analysis_generation = prior_generation
            .checked_add(1)
            .ok_or_else(|| invariant("Candidate analysis generation overflow"))?;
        let candidate_json = serde_json::to_string(candidate)
            .map_err(json_error("serialize derived Candidate analysis"))?;
        transaction
            .execute(
                "INSERT INTO candidate_analysis (
                    candidate_id, episode_id, analysis_generation, analysis_status,
                    context_tree_oid, context_generation, graph_context_tree_oid,
                    artifact_generation, candidate_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(candidate_id) DO UPDATE SET
                    episode_id = excluded.episode_id,
                    analysis_generation = excluded.analysis_generation,
                    analysis_status = excluded.analysis_status,
                    context_tree_oid = excluded.context_tree_oid,
                    context_generation = excluded.context_generation,
                    graph_context_tree_oid = excluded.graph_context_tree_oid,
                    artifact_generation = excluded.artifact_generation,
                    candidate_json = excluded.candidate_json",
                params![
                    candidate.candidate_id.to_string(),
                    episode_id.to_string(),
                    analysis_generation,
                    candidate_analysis_status_name(candidate.analysis.status),
                    candidate.analysis.context_tree_oid,
                    candidate
                        .analysis
                        .context_generation
                        .map(|value| value.to_string()),
                    candidate.analysis.graph_context_tree_oid,
                    candidate.analysis.artifact_generation,
                    candidate_json,
                ],
            )
            .map_err(sql_error("replace Candidate analysis"))?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate analysis replacement"))?;
        Ok(CandidateAnalysisView {
            candidate: candidate.clone(),
            analysis_generation: u64::try_from(analysis_generation)
                .map_err(|_| invariant("negative Candidate analysis generation"))?,
        })
    }

    /// Reads the current rebuildable Candidate analysis by Candidate identity.
    ///
    /// # Errors
    ///
    /// Returns typed parse or storage failures; absence remains `None`.
    pub fn read_candidate_analysis(
        &self,
        candidate_id: CandidateId,
    ) -> Result<Option<CandidateAnalysisView>> {
        self.open_connection()?
            .query_row(
                "SELECT candidate_json, analysis_generation
                 FROM candidate_analysis WHERE candidate_id = ?1",
                [candidate_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(sql_error("read Candidate analysis"))?
            .map(|(candidate_json, generation)| {
                Ok(CandidateAnalysisView {
                    candidate: serde_json::from_str(&candidate_json)
                        .map_err(json_error("parse Candidate analysis"))?,
                    analysis_generation: u64::try_from(generation)
                        .map_err(|_| invariant("negative Candidate analysis generation"))?,
                })
            })
            .transpose()
    }

    /// Lists one stable bounded page of Reviews owned by the locator's exact `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Returns typed locator, cursor, bound, parse, or storage failures.
    pub fn list_candidate_reviews(
        &self,
        locator: &ExternalSessionLocator,
        status: CandidateReviewStatus,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<CandidateReviewPage> {
        locator.validate()?;
        if limit == 0 || limit > MAX_CANDIDATE_REVIEW_LIST_LIMIT {
            return Err(invalid(
                "Candidate Review list limit is outside the safe bound",
            ));
        }
        self.cleanup_expired_candidate_reviews()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin Candidate Review page"))?;
        let task_session_id = find_active_task_by_locator(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask"))?;
        let task = require_snapshot(&transaction, task_session_id)?;
        let (cursor_created_at, cursor_candidate_id) = cursor
            .map(parse_candidate_review_cursor)
            .transpose()?
            .map_or((0, None), |(created_at, candidate_id)| {
                (created_at, Some(candidate_id))
            });
        let query_limit = i64::try_from(limit.saturating_add(1))
            .map_err(|_| invalid("Candidate Review list limit exceeds SQLite range"))?;
        let mut statement = transaction
            .prepare(
                "SELECT candidate_id, submission_id, episode_id, task_session_id, task_id,
                        build_id, final_checkpoint_id, checkpoint_id, claim_id,
                        review_version, status, discard_reason, created_at_unix_seconds,
                        expires_at_unix_seconds, discarded_at_unix_seconds,
                        expired_at_unix_seconds, confirmation_id, result_context_id
                 FROM candidate_review
                 WHERE task_session_id = ?1 AND task_id = ?2 AND status = ?3
                   AND (created_at_unix_seconds > ?4 OR
                        (created_at_unix_seconds = ?4 AND candidate_id > ?5))
                 ORDER BY created_at_unix_seconds ASC, candidate_id ASC
                 LIMIT ?6",
            )
            .map_err(sql_error("prepare Candidate Review page"))?;
        let cursor_candidate = cursor_candidate_id.map_or_else(String::new, |id| id.to_string());
        let rows = statement
            .query_map(
                params![
                    task.task_session_id.to_string(),
                    task.task_id.to_string(),
                    candidate_review_status_name(status),
                    i64::try_from(cursor_created_at)
                        .map_err(|_| invalid("Candidate Review cursor exceeds SQLite range"))?,
                    cursor_candidate,
                    query_limit,
                ],
                candidate_review_row,
            )
            .map_err(sql_error("query Candidate Review page"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error("read Candidate Review page"))?;
        let mut records = rows
            .into_iter()
            .map(parse_candidate_review_record)
            .collect::<Result<Vec<_>>>()?;
        let has_more = records.len() > limit;
        records.truncate(limit);
        let next_cursor = if has_more {
            records.last().map(candidate_review_cursor)
        } else {
            None
        };
        drop(statement);
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Review page"))?;
        Ok(CandidateReviewPage {
            records,
            next_cursor,
        })
    }

    /// Reads one complete Review identity only when it belongs to the locator's `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Returns typed locator, parse, or storage failures; absence and cross-Task identity are None.
    pub fn read_candidate_review(
        &self,
        locator: &ExternalSessionLocator,
        candidate_id: CandidateId,
    ) -> Result<Option<CandidateReviewRecord>> {
        locator.validate()?;
        self.cleanup_expired_candidate_reviews()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin Candidate Review read"))?;
        let Some(task_session_id) = find_active_task_by_locator(&transaction, locator)? else {
            return Ok(None);
        };
        let task = require_snapshot(&transaction, task_session_id)?;
        let result = read_candidate_review_record(&transaction, candidate_id)?.map_or(
            Ok(None),
            |record| {
                if record.source_episode.task_session_id == task.task_session_id
                    && record.source_episode.task_id == task.task_id
                {
                    Ok(Some(record))
                } else {
                    Ok(None)
                }
            },
        )?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Review read"))?;
        Ok(result)
    }

    /// Discards one Pending Review under exact `ActiveTask`, Intent and Review-version CAS.
    ///
    /// # Errors
    ///
    /// Returns typed ownership, conflict, stale-version, lifecycle, validation, or storage errors.
    pub fn discard_candidate_review(
        &self,
        request: &CandidateReviewDiscard,
    ) -> Result<CandidateReviewDiscardOutcome> {
        request.locator.validate()?;
        let reason = request.reason.trim();
        if reason.is_empty() || reason.len() > 512 || request.expected_review_version == 0 {
            return Err(invalid(
                "Candidate discard requires a non-empty bounded reason and positive review version",
            ));
        }
        self.cleanup_expired_candidate_reviews()?;
        let now = unix_seconds(SystemTime::now())?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Review discard")?;
        let task_session_id = find_active_task_by_locator(&transaction, &request.locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask"))?;
        let task = require_snapshot(&transaction, task_session_id)?;
        if task.task_id != request.expected_task_id
            || task
                .current_intent_revision()
                .is_none_or(|revision| revision.revision_id != request.expected_intent_revision_id)
        {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate discard Task/Intent ownership CAS is stale",
            ));
        }
        let mut record = read_candidate_review_record(&transaction, request.candidate_id)?
            .ok_or_else(|| invalid("Candidate Review does not exist for the ActiveTask"))?;
        if record.source_episode.task_session_id != task.task_session_id
            || record.source_episode.task_id != task.task_id
        {
            return Err(invalid(
                "Candidate Review does not belong to the ExternalSession ActiveTask",
            ));
        }
        match record.status {
            CandidateReviewStatus::Discarded => {
                if record.discard_reason.as_deref() != Some(reason) {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "Candidate Review was already discarded with a different reason",
                    ));
                }
                transaction
                    .commit()
                    .map_err(sql_error("commit idempotent Candidate Review discard"))?;
                return Ok(CandidateReviewDiscardOutcome {
                    record,
                    status: CandidateReviewDiscardStatus::AlreadyDiscarded,
                });
            }
            CandidateReviewStatus::Expired => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "Expired Candidate Review cannot be discarded",
                ));
            }
            CandidateReviewStatus::Confirmed => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "Confirmed Candidate Review cannot be discarded",
                ));
            }
            CandidateReviewStatus::Pending => {}
        }
        if record.review_version != request.expected_review_version {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate Review version is stale",
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE candidate_review
                 SET status = 'discarded', review_version = review_version + 1,
                     discard_reason = ?1, discarded_at_unix_seconds = ?2
                 WHERE candidate_id = ?3 AND review_version = ?4 AND status = 'pending'",
                params![
                    reason,
                    i64::try_from(now)
                        .map_err(|_| invalid("Candidate discard timestamp exceeds SQLite range"))?,
                    request.candidate_id.to_string(),
                    i64::try_from(request.expected_review_version)
                        .map_err(|_| invalid("Candidate Review version exceeds SQLite range"))?,
                ],
            )
            .map_err(sql_error("discard Candidate Review"))?;
        if changed != 1 {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate Review changed during discard",
            ));
        }
        record = read_candidate_review_record(&transaction, request.candidate_id)?
            .ok_or_else(|| invariant("discarded Candidate Review disappeared"))?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Review discard"))?;
        Ok(CandidateReviewDiscardOutcome {
            record,
            status: CandidateReviewDiscardStatus::Discarded,
        })
    }

    /// Reserves one complete server-owned Confirmation plan before any Git write.
    ///
    /// # Errors
    ///
    /// Returns typed Task/Intent/Review CAS, analysis-generation, conflict, or storage errors.
    #[allow(clippy::too_many_lines)]
    pub fn reserve_candidate_confirmation(
        &self,
        locator: &ExternalSessionLocator,
        expected_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
        plan: &CandidateConfirmationPlan,
    ) -> Result<CandidateConfirmationReservation> {
        locator.validate()?;
        plan.validate()?;
        self.cleanup_expired_candidate_reviews()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Confirmation reservation")?;
        let task_session_id = find_active_task_by_locator(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask"))?;
        let task = require_snapshot(&transaction, task_session_id)?;
        if task.task_id != expected_task_id
            || task
                .current_intent_revision()
                .is_none_or(|revision| revision.revision_id != expected_intent_revision_id)
        {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate Confirmation Task/Intent ownership CAS is stale",
            ));
        }
        let review = read_candidate_review_record(&transaction, plan.operation.candidate_id)?
            .ok_or_else(|| invalid("Candidate Review does not exist for the ActiveTask"))?;
        if review.source_episode.task_session_id != task.task_session_id
            || review.source_episode.task_id != task.task_id
        {
            return Err(invalid(
                "Candidate Review does not belong to the ExternalSession ActiveTask",
            ));
        }
        if let Some(existing) =
            read_candidate_confirmation_operation(&transaction, plan.operation.candidate_id)?
        {
            if existing.operation_hash != plan.operation_hash {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "Candidate Confirmation operation already has different semantics",
                ));
            }
            transaction.commit().map_err(sql_error(
                "commit existing Candidate Confirmation reservation",
            ))?;
            return Ok(CandidateConfirmationReservation {
                operation: existing,
                created: false,
            });
        }
        if review.status != CandidateReviewStatus::Pending {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Only a Pending Candidate Review can be confirmed",
            ));
        }
        if review.review_version != plan.operation.review_parent_version {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate Review version is stale for confirmation",
            ));
        }
        let analysis = transaction
            .query_row(
                "SELECT analysis_generation, analysis_status FROM candidate_analysis
                 WHERE candidate_id = ?1",
                [plan.operation.candidate_id.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error("read Confirmation Candidate analysis"))?
            .ok_or_else(|| invalid("Candidate Confirmation requires completed analysis"))?;
        if analysis.1 != "complete"
            || nonnegative_u64(analysis.0, "candidate_analysis.analysis_generation")?
                != plan.operation.analysis_generation
        {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate Confirmation analysis generation is not current and complete",
            ));
        }
        let plan_json = serde_json::to_string(plan)
            .map_err(json_error("serialize Candidate Confirmation plan"))?;
        transaction
            .execute(
                "INSERT INTO candidate_confirmation_operation (
                    candidate_id, review_parent_version, operation_hash, plan_hash,
                    plan_json, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'reserved')",
                params![
                    plan.operation.candidate_id.to_string(),
                    i64::try_from(plan.operation.review_parent_version)
                        .map_err(|_| invalid("Candidate Review version exceeds SQLite range"))?,
                    plan.operation_hash,
                    plan.plan_hash(),
                    plan_json,
                ],
            )
            .map_err(sql_error("reserve Candidate Confirmation operation"))?;
        let operation =
            read_candidate_confirmation_operation(&transaction, plan.operation.candidate_id)?
                .ok_or_else(|| {
                    invariant("reserved Candidate Confirmation operation disappeared")
                })?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Confirmation reservation"))?;
        Ok(CandidateConfirmationReservation {
            operation,
            created: true,
        })
    }

    /// Marks a Git-committed Confirmation operation and its Review terminal in one transaction.
    ///
    /// # Errors
    ///
    /// Returns typed operation-hash, plan-identity, Review CAS, or storage errors.
    pub fn finalize_candidate_confirmation(
        &self,
        candidate_id: CandidateId,
        operation_hash: &str,
        confirmation_id: ConfirmationId,
        result_context_id: ContextId,
    ) -> Result<CandidateConfirmationFinalizeOutcome> {
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Confirmation finalize")?;
        let operation = read_candidate_confirmation_operation(&transaction, candidate_id)?
            .ok_or_else(|| invalid("Candidate Confirmation operation is not reserved"))?;
        if operation.operation_hash != operation_hash
            || operation.plan.confirmation.confirmation_id != confirmation_id
            || operation.plan.result_context_id != result_context_id
        {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Committed Candidate Confirmation does not match the reserved operation",
            ));
        }
        if operation.status == CandidateConfirmationOperationStatus::Committed {
            let review = read_candidate_review_record(&transaction, candidate_id)?
                .ok_or_else(|| invariant("confirmed Candidate Review disappeared"))?;
            if review.status != CandidateReviewStatus::Confirmed
                || review.confirmation_id != Some(confirmation_id)
                || review.result_context_id != Some(result_context_id)
            {
                return Err(invariant(
                    "committed Candidate Confirmation and Review audit disagree",
                ));
            }
            transaction.commit().map_err(sql_error(
                "commit idempotent Candidate Confirmation finalize",
            ))?;
            return Ok(CandidateConfirmationFinalizeOutcome {
                operation,
                review,
                already_confirmed: true,
            });
        }
        let changed = transaction
            .execute(
                "UPDATE candidate_review
                 SET status = 'confirmed', review_version = review_version + 1,
                     confirmation_id = ?1, result_context_id = ?2
                 WHERE candidate_id = ?3 AND status = 'pending' AND review_version = ?4",
                params![
                    confirmation_id.to_string(),
                    result_context_id.to_string(),
                    candidate_id.to_string(),
                    i64::try_from(operation.review_parent_version).map_err(|_| invalid(
                        "Candidate Review parent version exceeds SQLite range"
                    ))?,
                ],
            )
            .map_err(sql_error("confirm Candidate Review"))?;
        if changed != 1 {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate Review changed before Confirmation finalized",
            ));
        }
        transaction
            .execute(
                "UPDATE candidate_confirmation_operation SET status = 'committed'
                 WHERE candidate_id = ?1 AND status = 'reserved'",
                [candidate_id.to_string()],
            )
            .map_err(sql_error("commit Candidate Confirmation operation status"))?;
        let operation = read_candidate_confirmation_operation(&transaction, candidate_id)?
            .ok_or_else(|| invariant("committed Candidate Confirmation operation disappeared"))?;
        let review = read_candidate_review_record(&transaction, candidate_id)?
            .ok_or_else(|| invariant("confirmed Candidate Review disappeared"))?;
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Confirmation finalize"))?;
        Ok(CandidateConfirmationFinalizeOutcome {
            operation,
            review,
            already_confirmed: false,
        })
    }

    /// Expires retained Pending/Discarded Reviews and removes only heavy Runtime analysis.
    ///
    /// # Errors
    ///
    /// Returns typed clock, parse, or storage failures. Git is never opened or modified.
    pub fn cleanup_expired_candidate_reviews(&self) -> Result<CandidateReviewCleanup> {
        self.cleanup_expired_candidate_reviews_at(unix_seconds(SystemTime::now())?)
    }

    /// Deterministic cleanup boundary used by tests and maintenance orchestration.
    ///
    /// # Errors
    ///
    /// Returns typed parse or storage failures. Expired tombstones are retained permanently.
    pub fn cleanup_expired_candidate_reviews_at(
        &self,
        now_unix_seconds: u64,
    ) -> Result<CandidateReviewCleanup> {
        let now = i64::try_from(now_unix_seconds)
            .map_err(|_| invalid("Candidate Review cleanup time exceeds SQLite range"))?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Review cleanup")?;
        let mut statement = transaction
            .prepare(
                "SELECT candidate_id FROM candidate_review
                 WHERE status IN ('pending', 'discarded')
                   AND expires_at_unix_seconds <= ?1
                 ORDER BY candidate_id ASC",
            )
            .map_err(sql_error("prepare expired Candidate Reviews"))?;
        let candidate_ids = statement
            .query_map([now], |row| row.get::<_, String>(0))
            .map_err(sql_error("query expired Candidate Reviews"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error("read expired Candidate Reviews"))?
            .into_iter()
            .map(|value| parse_id::<CandidateId>(&value, "candidate_review.candidate_id"))
            .collect::<Result<Vec<_>>>()?;
        drop(statement);
        let mut removed_analysis_count = 0_usize;
        for candidate_id in &candidate_ids {
            removed_analysis_count = removed_analysis_count.saturating_add(
                transaction
                    .execute(
                        "DELETE FROM candidate_analysis WHERE candidate_id = ?1",
                        [candidate_id.to_string()],
                    )
                    .map_err(sql_error("delete expired Candidate analysis"))?,
            );
            transaction
                .execute(
                    "UPDATE candidate_review
                     SET status = 'expired', review_version = review_version + 1,
                         expired_at_unix_seconds = ?1
                     WHERE candidate_id = ?2 AND status IN ('pending', 'discarded')",
                    params![now, candidate_id.to_string()],
                )
                .map_err(sql_error("expire Candidate Review"))?;
        }
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Review cleanup"))?;
        Ok(CandidateReviewCleanup {
            expired_candidate_ids: candidate_ids,
            removed_analysis_count,
        })
    }

    /// Reads any retained Task by `TaskSessionId`.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_snapshot(
        &self,
        task_session_id: TaskSessionId,
    ) -> Result<Option<TaskSessionSnapshot>> {
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin Task snapshot transaction"))?;
        let snapshot = read_snapshot_in_transaction(&transaction, task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Task snapshot transaction"))?;
        Ok(snapshot)
    }

    /// Reads only the `ActiveTask` selected by an `ExternalSessionLocator`.
    ///
    /// # Errors
    ///
    /// Returns typed input, storage, or invariant errors.
    pub fn read_snapshot_by_locator(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<Option<TaskSessionSnapshot>> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin ActiveTask snapshot transaction"))?;
        let snapshot = find_active_task_by_locator(&transaction, locator)?
            .map(|id| read_snapshot_in_transaction(&transaction, id))
            .transpose()?
            .flatten();
        transaction
            .commit()
            .map_err(sql_error("commit ActiveTask snapshot transaction"))?;
        Ok(snapshot)
    }

    /// Reads one `ExternalSession` with its `ActiveTask` and all historical Tasks.
    ///
    /// # Errors
    ///
    /// Returns typed input, storage, or invariant errors.
    pub fn read_external_session_by_locator(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<Option<ExternalSessionSnapshot>> {
        locator.validate()?;
        let mut connection = self.open_connection()?;
        let transaction = connection
            .transaction()
            .map_err(sql_error("begin ExternalSession snapshot transaction"))?;
        let snapshot = read_external_session_in_transaction(&transaction, locator)?;
        transaction
            .commit()
            .map_err(sql_error("commit ExternalSession snapshot transaction"))?;
        Ok(snapshot)
    }

    /// Reads active and superseded Signal records for any retained Task.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn read_signal_history(
        &self,
        task_session_id: TaskSessionId,
    ) -> Result<Vec<TaskSignalRecord>> {
        read_signal_records(&self.open_connection()?, task_session_id)
    }

    fn open_connection(&self) -> Result<Connection> {
        let connection =
            Connection::open(&self.database).map_err(sql_error("open task runtime database"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(sql_error("configure task runtime busy timeout"))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error("configure task runtime journal mode"))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_error("configure task runtime synchronous mode"))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_error("enable task runtime foreign keys"))?;
        ensure_schema(&connection)?;
        Ok(connection)
    }
}

#[derive(Clone, Copy)]
#[allow(clippy::struct_field_names)]
struct ExternalIdentity {
    external_session_id: ExternalSessionId,
    active_task_session_id: TaskSessionId,
    active_task_id: TaskId,
}

fn immediate<'a>(connection: &'a mut Connection, context: &'static str) -> Result<Transaction<'a>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error(context))
}

#[allow(clippy::too_many_lines)]
fn ensure_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(sql_error("read task runtime schema version"))?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(invariant(format!(
            "unsupported task runtime schema version {version}; expected {SCHEMA_VERSION}"
        )));
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS external_session (
                external_session_id TEXT PRIMARY KEY,
                agent_kind TEXT NOT NULL,
                external_session_key TEXT NOT NULL,
                active_task_session_id TEXT NOT NULL,
                active_task_id TEXT NOT NULL,
                UNIQUE (agent_kind, external_session_key),
                FOREIGN KEY (external_session_id, active_task_session_id, active_task_id)
                    REFERENCES task_session (external_session_id, task_session_id, task_id)
                    DEFERRABLE INITIALLY DEFERRED
            ) STRICT;
            CREATE TABLE IF NOT EXISTS task_session (
                task_session_id TEXT PRIMARY KEY,
                external_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL UNIQUE,
                current_intent_revision_id TEXT NOT NULL,
                task_ordinal INTEGER NOT NULL CHECK (task_ordinal >= 0),
                UNIQUE (external_session_id, task_session_id, task_id),
                UNIQUE (external_session_id, task_ordinal),
                FOREIGN KEY (external_session_id) REFERENCES external_session (external_session_id),
                FOREIGN KEY (task_session_id, current_intent_revision_id)
                    REFERENCES task_intent_revision (task_session_id, revision_id)
                    DEFERRABLE INITIALLY DEFERRED
            ) STRICT;
            CREATE TABLE IF NOT EXISTS task_intent_revision (
                task_session_id TEXT NOT NULL,
                revision_id TEXT PRIMARY KEY,
                parent_revision_id TEXT,
                revision_ordinal INTEGER NOT NULL CHECK (revision_ordinal >= 0),
                authority_json TEXT NOT NULL CHECK (json_valid(authority_json)),
                semantic_hash TEXT NOT NULL,
                UNIQUE (task_session_id, revision_id),
                UNIQUE (task_session_id, revision_ordinal),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id),
                FOREIGN KEY (task_session_id, parent_revision_id)
                    REFERENCES task_intent_revision (task_session_id, revision_id)
            ) STRICT;
            CREATE UNIQUE INDEX IF NOT EXISTS task_intent_one_initial
                ON task_intent_revision (task_session_id) WHERE parent_revision_id IS NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS task_intent_one_child_per_parent
                ON task_intent_revision (task_session_id, parent_revision_id)
                WHERE parent_revision_id IS NOT NULL;
            CREATE TABLE IF NOT EXISTS task_signal (
                signal_id TEXT PRIMARY KEY,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                lifecycle TEXT NOT NULL CHECK (lifecycle IN ('active', 'superseded')),
                signal_ordinal INTEGER NOT NULL CHECK (signal_ordinal >= 0),
                UNIQUE (task_session_id, signal_ordinal),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id)
            ) STRICT;
            CREATE UNIQUE INDEX IF NOT EXISTS task_signal_one_active_semantic
                ON task_signal (task_session_id, kind, content)
                WHERE lifecycle = 'active';
            CREATE TABLE IF NOT EXISTS work_episode (
                episode_id TEXT PRIMARY KEY,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                version INTEGER NOT NULL CHECK (version >= 0),
                status TEXT NOT NULL CHECK (status IN ('open', 'closed')),
                final_checkpoint_id TEXT,
                episode_ordinal INTEGER NOT NULL CHECK (episode_ordinal >= 0),
                UNIQUE (task_session_id, episode_ordinal),
                UNIQUE (episode_id, task_session_id, task_id),
                CHECK (
                    (status = 'open' AND final_checkpoint_id IS NULL) OR
                    (status = 'closed' AND final_checkpoint_id IS NOT NULL)
                ),
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id)
            ) STRICT;
            CREATE UNIQUE INDEX IF NOT EXISTS work_episode_one_open_per_task
                ON work_episode (task_session_id) WHERE status = 'open';
            CREATE TABLE IF NOT EXISTS work_episode_intent_ref (
                episode_id TEXT NOT NULL,
                revision_id TEXT NOT NULL,
                ref_ordinal INTEGER NOT NULL CHECK (ref_ordinal >= 0),
                PRIMARY KEY (episode_id, revision_id),
                UNIQUE (episode_id, ref_ordinal),
                FOREIGN KEY (episode_id) REFERENCES work_episode (episode_id),
                FOREIGN KEY (revision_id) REFERENCES task_intent_revision (revision_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS work_episode_signal_ref (
                episode_id TEXT NOT NULL,
                signal_id TEXT NOT NULL,
                ref_ordinal INTEGER NOT NULL CHECK (ref_ordinal >= 0),
                PRIMARY KEY (episode_id, signal_id),
                UNIQUE (episode_id, ref_ordinal),
                FOREIGN KEY (episode_id) REFERENCES work_episode (episode_id),
                FOREIGN KEY (signal_id) REFERENCES task_signal (signal_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS work_observation (
                observation_id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                intent_revision_id TEXT NOT NULL,
                observation_ordinal INTEGER NOT NULL CHECK (observation_ordinal >= 0),
                observation_json TEXT NOT NULL CHECK (json_valid(observation_json)),
                UNIQUE (episode_id, observation_ordinal),
                UNIQUE (observation_id, episode_id),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id),
                FOREIGN KEY (intent_revision_id)
                    REFERENCES task_intent_revision (revision_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS work_observation_source (
                observation_id TEXT NOT NULL,
                source_ordinal INTEGER NOT NULL CHECK (source_ordinal >= 0),
                source_json TEXT NOT NULL CHECK (json_valid(source_json)),
                PRIMARY KEY (observation_id, source_ordinal),
                FOREIGN KEY (observation_id) REFERENCES work_observation (observation_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS capture_ingestion (
                capture_id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL,
                observation_id TEXT NOT NULL UNIQUE,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                FOREIGN KEY (observation_id, episode_id)
                    REFERENCES work_observation (observation_id, episode_id),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS agent_checkpoint (
                checkpoint_id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                intent_revision_id TEXT NOT NULL,
                parent_episode_version INTEGER NOT NULL CHECK (parent_episode_version >= 0),
                boundary TEXT NOT NULL CHECK (boundary IN ('continue', 'close')),
                semantic_json TEXT NOT NULL CHECK (json_valid(semantic_json)),
                checkpoint_json TEXT NOT NULL CHECK (json_valid(checkpoint_json)),
                checkpoint_ordinal INTEGER NOT NULL CHECK (checkpoint_ordinal >= 0),
                UNIQUE (episode_id, parent_episode_version),
                UNIQUE (episode_id, checkpoint_ordinal),
                UNIQUE (checkpoint_id, episode_id),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id),
                FOREIGN KEY (intent_revision_id)
                    REFERENCES task_intent_revision (revision_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS candidate_build (
                build_id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL UNIQUE,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                final_checkpoint_id TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'complete', 'incomplete')),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id),
                FOREIGN KEY (final_checkpoint_id, episode_id)
                    REFERENCES agent_checkpoint (checkpoint_id, episode_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS candidate_build_item (
                build_id TEXT NOT NULL,
                item_ordinal INTEGER NOT NULL CHECK (item_ordinal >= 0),
                checkpoint_id TEXT NOT NULL,
                claim_id TEXT NOT NULL,
                submission_id TEXT NOT NULL UNIQUE,
                content_hash TEXT,
                status TEXT NOT NULL CHECK (status IN (
                    'prepared', 'needs_evidence', 'created', 'already_exists', 'failed'
                )),
                candidate_id TEXT,
                event_id TEXT,
                error_code TEXT,
                PRIMARY KEY (build_id, claim_id),
                UNIQUE (build_id, item_ordinal),
                CHECK (
                    (status = 'prepared' AND content_hash IS NOT NULL
                        AND candidate_id IS NULL AND event_id IS NULL AND error_code IS NULL) OR
                    (status = 'needs_evidence' AND content_hash IS NULL
                        AND candidate_id IS NULL AND event_id IS NULL AND error_code IS NOT NULL) OR
                    (status IN ('created', 'already_exists') AND content_hash IS NOT NULL
                        AND candidate_id IS NOT NULL AND event_id IS NOT NULL
                        AND error_code IS NULL) OR
                    (status = 'failed' AND candidate_id IS NULL AND event_id IS NULL
                        AND error_code IS NOT NULL)
                ),
                FOREIGN KEY (build_id) REFERENCES candidate_build (build_id),
                FOREIGN KEY (checkpoint_id) REFERENCES agent_checkpoint (checkpoint_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS candidate_analysis (
                candidate_id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL,
                analysis_generation INTEGER NOT NULL CHECK (analysis_generation > 0),
                analysis_status TEXT NOT NULL CHECK (
                    analysis_status IN ('pending', 'complete', 'failed')
                ),
                context_tree_oid TEXT,
                context_generation TEXT,
                graph_context_tree_oid TEXT,
                artifact_generation TEXT,
                candidate_json TEXT NOT NULL CHECK (json_valid(candidate_json)),
                FOREIGN KEY (episode_id) REFERENCES work_episode (episode_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS candidate_review (
                candidate_id TEXT PRIMARY KEY,
                submission_id TEXT NOT NULL UNIQUE,
                episode_id TEXT NOT NULL,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                build_id TEXT NOT NULL,
                final_checkpoint_id TEXT NOT NULL,
                checkpoint_id TEXT NOT NULL,
                claim_id TEXT NOT NULL,
                review_version INTEGER NOT NULL CHECK (review_version > 0),
                status TEXT NOT NULL CHECK (
                    status IN ('pending', 'discarded', 'expired', 'confirmed')
                ),
                discard_reason TEXT,
                created_at_unix_seconds INTEGER NOT NULL CHECK (created_at_unix_seconds >= 0),
                expires_at_unix_seconds INTEGER NOT NULL CHECK (
                    expires_at_unix_seconds > created_at_unix_seconds
                ),
                discarded_at_unix_seconds INTEGER,
                expired_at_unix_seconds INTEGER,
                confirmation_id TEXT,
                result_context_id TEXT,
                UNIQUE (build_id, claim_id),
                CHECK (
                    (status = 'pending' AND discard_reason IS NULL
                        AND discarded_at_unix_seconds IS NULL
                        AND expired_at_unix_seconds IS NULL
                        AND confirmation_id IS NULL AND result_context_id IS NULL) OR
                    (status = 'discarded' AND discard_reason IS NOT NULL
                        AND length(trim(discard_reason)) > 0
                        AND discarded_at_unix_seconds IS NOT NULL
                        AND expired_at_unix_seconds IS NULL
                        AND confirmation_id IS NULL AND result_context_id IS NULL) OR
                    (status = 'expired' AND expired_at_unix_seconds IS NOT NULL
                        AND confirmation_id IS NULL AND result_context_id IS NULL) OR
                    (status = 'confirmed' AND discard_reason IS NULL
                        AND discarded_at_unix_seconds IS NULL
                        AND expired_at_unix_seconds IS NULL
                        AND confirmation_id IS NOT NULL AND result_context_id IS NOT NULL)
                ),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id),
                FOREIGN KEY (build_id, claim_id)
                    REFERENCES candidate_build_item (build_id, claim_id),
                FOREIGN KEY (final_checkpoint_id, episode_id)
                    REFERENCES agent_checkpoint (checkpoint_id, episode_id),
                FOREIGN KEY (checkpoint_id, episode_id)
                    REFERENCES agent_checkpoint (checkpoint_id, episode_id)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS candidate_review_owner_status_order
                ON candidate_review (
                    task_session_id, task_id, status, created_at_unix_seconds, candidate_id
                );
            CREATE TABLE IF NOT EXISTS candidate_confirmation_operation (
                candidate_id TEXT PRIMARY KEY,
                review_parent_version INTEGER NOT NULL CHECK (review_parent_version > 0),
                operation_hash TEXT NOT NULL,
                plan_hash TEXT NOT NULL,
                plan_json TEXT NOT NULL CHECK (json_valid(plan_json)),
                status TEXT NOT NULL CHECK (status IN ('reserved', 'committed')),
                UNIQUE (candidate_id, review_parent_version),
                FOREIGN KEY (candidate_id) REFERENCES candidate_review (candidate_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS work_episode_diagnostic (
                episode_id TEXT NOT NULL,
                diagnostic_ordinal INTEGER NOT NULL CHECK (diagnostic_ordinal >= 0),
                capture_id TEXT NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN (
                    'capture_repository_not_configured',
                    'capture_unsafe_artifact_path'
                )),
                PRIMARY KEY (episode_id, diagnostic_ordinal),
                UNIQUE (episode_id, capture_id, kind),
                FOREIGN KEY (episode_id) REFERENCES work_episode (episode_id)
            ) STRICT;
            PRAGMA user_version = 11;",
        )
        .map_err(sql_error("initialize task runtime schema"))
}

fn insert_external_session(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    locator: &ExternalSessionLocator,
    task: &TaskSessionSnapshot,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO external_session (
                external_session_id, agent_kind, external_session_key,
                active_task_session_id, active_task_id
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                external_session_id.to_string(),
                locator.agent_kind,
                locator.external_session_id,
                task.task_session_id.to_string(),
                task.task_id.to_string(),
            ],
        )
        .map_err(sql_error("insert ExternalSession"))?;
    Ok(())
}

fn insert_task(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    task_ordinal: i64,
    snapshot: &TaskSessionSnapshot,
) -> Result<()> {
    snapshot.validate()?;
    let revision = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("initial Task has no Intent revision"))?;
    transaction
        .execute(
            "INSERT INTO task_session (
                task_session_id, external_session_id, task_id,
                current_intent_revision_id, task_ordinal
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                snapshot.task_session_id.to_string(),
                external_session_id.to_string(),
                snapshot.task_id.to_string(),
                revision.revision_id.to_string(),
                task_ordinal,
            ],
        )
        .map_err(sql_error("insert Task Session"))?;
    insert_intent_revision(transaction, snapshot.task_session_id, 0, revision)?;
    let _outcome = merge_signals_in_transaction(
        transaction,
        snapshot.task_session_id,
        snapshot.task_id,
        &snapshot.task_signals,
    )?;
    Ok(())
}

fn insert_intent_revision(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    ordinal: i64,
    revision: &TaskIntentRevision,
) -> Result<()> {
    let authority_json = serde_json::to_string(&revision.working_intent)
        .map_err(json_error("serialize Working Intent revision"))?;
    transaction
        .execute(
            "INSERT INTO task_intent_revision (
                task_session_id, revision_id, parent_revision_id,
                revision_ordinal, authority_json, semantic_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task_session_id.to_string(),
                revision.revision_id.to_string(),
                revision.parent_revision_id.map(|value| value.to_string()),
                ordinal,
                authority_json,
                revision.semantic_hash,
            ],
        )
        .map_err(sql_error("insert Task Intent revision"))?;
    Ok(())
}

fn merge_signals_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    signals: &[TaskSignal],
) -> Result<MergeSignalsOutcome> {
    let mut inserted_signal_ids = Vec::new();
    let mut next_ordinal = next_signal_ordinal(transaction, task_session_id)?;
    for signal in signals {
        if find_active_signal(transaction, task_session_id, signal)?.is_some() {
            continue;
        }
        let signal_id = SignalId::new();
        transaction
            .execute(
                "INSERT INTO task_signal (
                    signal_id, task_session_id, task_id, kind, content,
                    lifecycle, signal_ordinal
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
                params![
                    signal_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                    signal_kind_name(signal.kind),
                    signal.content,
                    next_ordinal,
                ],
            )
            .map_err(sql_error("insert active Task Signal"))?;
        inserted_signal_ids.push(signal_id);
        next_ordinal = next_ordinal
            .checked_add(1)
            .ok_or_else(|| invariant("Task Signal ordinal overflow"))?;
    }
    let snapshot = require_snapshot(transaction, task_session_id)?;
    Ok(MergeSignalsOutcome {
        snapshot,
        inserted: inserted_signal_ids.len(),
        inserted_signal_ids,
    })
}

fn find_open_episode(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Option<WorkEpisodeId>> {
    connection
        .query_row(
            "SELECT episode_id FROM work_episode
             WHERE task_session_id = ?1 AND status = 'open'",
            [task_session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find open Work Episode"))?
        .map(|value| parse_id(&value, "work_episode.episode_id"))
        .transpose()
}

fn find_latest_episode(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Option<WorkEpisodeId>> {
    connection
        .query_row(
            "SELECT episode_id FROM work_episode
             WHERE task_session_id = ?1
             ORDER BY episode_ordinal DESC LIMIT 1",
            [task_session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find latest Work Episode"))?
        .map(|value| parse_id(&value, "work_episode.episode_id"))
        .transpose()
}

fn insert_open_episode(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
) -> Result<WorkEpisodeId> {
    let episode_id = WorkEpisodeId::new();
    let episode_ordinal = next_episode_ordinal(transaction, task_session_id)?;
    transaction
        .execute(
            "INSERT INTO work_episode (
                episode_id, task_session_id, task_id, version, status,
                final_checkpoint_id, episode_ordinal
             ) VALUES (?1, ?2, ?3, 0, 'open', NULL, ?4)",
            params![
                episode_id.to_string(),
                task_session_id.to_string(),
                task_id.to_string(),
                episode_ordinal,
            ],
        )
        .map_err(sql_error("insert Work Episode"))?;
    insert_all_missing_episode_refs(transaction, episode_id, task_session_id, task_id)?;
    Ok(episode_id)
}

fn find_latest_checkpoint_episode(
    connection: &Connection,
    task_session_id: TaskSessionId,
    parent_version: u64,
) -> Result<Option<WorkEpisodeId>> {
    let parent_version = i64::try_from(parent_version)
        .map_err(|_| invalid("Work Episode version exceeds SQLite range"))?;
    connection
        .query_row(
            "SELECT checkpoint.episode_id
             FROM agent_checkpoint AS checkpoint
             JOIN work_episode AS episode ON episode.episode_id = checkpoint.episode_id
             WHERE episode.task_session_id = ?1
               AND checkpoint.parent_episode_version = ?2
             ORDER BY episode.episode_ordinal DESC LIMIT 1",
            params![task_session_id.to_string(), parent_version],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find Agent Checkpoint retry Episode"))?
        .map(|value| parse_id(&value, "agent_checkpoint.episode_id"))
        .transpose()
}

fn checkpoint_semantic_json(input: &AgentCheckpointWrite) -> Result<String> {
    let claims = input
        .claims
        .iter()
        .map(|claim| {
            serde_json::json!({
                "context_kind_hint": claim.context_kind_hint,
                "topic_key_hint": claim.topic_key_hint,
                "statement": claim.statement,
                "rationale": claim.rationale,
                "applicability": claim.applicability,
                "assumptions": claim.assumptions,
                "recheck_when": claim.recheck_when,
                "evidence_refs": claim.evidence_refs,
                "inline_validations": claim.inline_validations,
                "artifact_refs": claim.artifact_refs,
                "related_contexts": claim.related_contexts,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&serde_json::json!({
        "expected_task_id": input.expected_task_id,
        "expected_intent_revision_id": input.expected_intent_revision_id,
        "boundary": input.boundary.as_str(),
        "claims": claims,
        "unknowns": input.unknowns,
    }))
    .map_err(json_error("serialize Agent Checkpoint semantics"))
}

fn validate_checkpoint_task_local_refs(
    episode: &WorkEpisode,
    claims: &[CheckpointClaimDraft],
) -> Result<()> {
    let observations = episode
        .observations
        .iter()
        .map(|observation| observation.observation_id)
        .collect::<HashSet<_>>();
    let signals = episode
        .signal_refs
        .iter()
        .map(|signal| signal.signal_id)
        .collect::<HashSet<_>>();
    for evidence in claims.iter().flat_map(|claim| &claim.evidence_refs) {
        match evidence {
            CaptureEvidenceRef::Observation { observation_id }
                if !observations.contains(observation_id) =>
            {
                return Err(invalid(
                    "Checkpoint Observation evidence does not belong to its Work Episode",
                ));
            }
            CaptureEvidenceRef::TaskSignal { signal_id } if !signals.contains(signal_id) => {
                return Err(invalid(
                    "Checkpoint TaskSignal evidence does not belong to its Task",
                ));
            }
            CaptureEvidenceRef::Observation { .. }
            | CaptureEvidenceRef::TaskSignal { .. }
            | CaptureEvidenceRef::ContextEvidence { .. } => {}
        }
    }
    Ok(())
}

fn read_checkpoint_by_parent(
    connection: &Connection,
    episode_id: WorkEpisodeId,
    parent_version: u64,
) -> Result<Option<(AgentCheckpoint, String)>> {
    let parent_version = i64::try_from(parent_version)
        .map_err(|_| invalid("Work Episode version exceeds SQLite range"))?;
    connection
        .query_row(
            "SELECT checkpoint_json, semantic_json FROM agent_checkpoint
             WHERE episode_id = ?1 AND parent_episode_version = ?2",
            params![episode_id.to_string(), parent_version],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("read Agent Checkpoint retry"))?
        .map(|(checkpoint, semantics)| {
            Ok((
                serde_json::from_str(&checkpoint).map_err(json_error("parse Agent Checkpoint"))?,
                semantics,
            ))
        })
        .transpose()
}

fn inline_observation_ids(
    checkpoint: &AgentCheckpoint,
    drafts: &[CheckpointClaimDraft],
) -> Result<Vec<WorkObservationId>> {
    if checkpoint.claims.len() != drafts.len() {
        return Err(invariant("persisted Agent Checkpoint Claim count changed"));
    }
    let mut ids = Vec::new();
    for (claim, draft) in checkpoint.claims.iter().zip(drafts) {
        if claim.evidence_refs.len()
            != draft
                .evidence_refs
                .len()
                .saturating_add(draft.inline_validations.len())
        {
            return Err(invariant(
                "persisted Agent Checkpoint inline Evidence count changed",
            ));
        }
        for evidence in claim.evidence_refs.iter().skip(draft.evidence_refs.len()) {
            let CaptureEvidenceRef::Observation { observation_id } = evidence else {
                return Err(invariant(
                    "persisted inline Validation does not reference an Observation",
                ));
            };
            ids.push(*observation_id);
        }
    }
    Ok(ids)
}

fn validate_build_preparations(items: &[CandidateBuildItemPreparation]) -> Result<()> {
    let mut claim_ids = HashSet::with_capacity(items.len());
    for item in items {
        if !claim_ids.insert(item.claim_id) {
            return Err(invalid(
                "Candidate Build preparations must not repeat ClaimId",
            ));
        }
        match item.status {
            CandidateBuildItemStatus::Prepared
                if item
                    .content_hash
                    .as_deref()
                    .is_some_and(|hash| !hash.is_empty())
                    && item.error_code.is_none() => {}
            CandidateBuildItemStatus::NeedsEvidence
                if item.content_hash.is_none()
                    && item.error_code.as_deref().is_some_and(valid_error_code) => {}
            CandidateBuildItemStatus::Prepared | CandidateBuildItemStatus::NeedsEvidence => {
                return Err(invalid(
                    "Candidate Build preparation readiness metadata is incomplete",
                ));
            }
            CandidateBuildItemStatus::Created
            | CandidateBuildItemStatus::AlreadyExists
            | CandidateBuildItemStatus::Failed => {
                return Err(invalid(
                    "Candidate Build preparation cannot supply a submission result status",
                ));
            }
        }
    }
    Ok(())
}

fn valid_error_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_build_claim_coverage(
    episode: &WorkEpisodeView,
    items: &[CandidateBuildItemPreparation],
) -> Result<()> {
    let persisted = episode
        .checkpoints
        .iter()
        .flat_map(|checkpoint| {
            checkpoint
                .claims
                .iter()
                .map(move |claim| (checkpoint.checkpoint_id, claim.claim_id))
        })
        .collect::<BTreeSet<_>>();
    let supplied = items
        .iter()
        .map(|item| (item.checkpoint_id, item.claim_id))
        .collect::<BTreeSet<_>>();
    if persisted.len() != items.len() || persisted != supplied {
        return Err(invalid(
            "Candidate Build must prepare every persisted Checkpoint Claim exactly once",
        ));
    }
    Ok(())
}

fn validate_build_item_result(
    status: CandidateBuildItemStatus,
    candidate_id: Option<CandidateId>,
    event_id: Option<EventId>,
    error_code: Option<&str>,
) -> Result<()> {
    match status {
        CandidateBuildItemStatus::Created | CandidateBuildItemStatus::AlreadyExists
            if candidate_id.is_some() && event_id.is_some() && error_code.is_none() =>
        {
            Ok(())
        }
        CandidateBuildItemStatus::Failed
            if candidate_id.is_none()
                && event_id.is_none()
                && error_code.is_some_and(valid_error_code) =>
        {
            Ok(())
        }
        CandidateBuildItemStatus::Created
        | CandidateBuildItemStatus::AlreadyExists
        | CandidateBuildItemStatus::Failed => Err(invalid(
            "Candidate Build result identity or safe error code is incomplete",
        )),
        CandidateBuildItemStatus::Prepared | CandidateBuildItemStatus::NeedsEvidence => Err(
            invalid("Candidate Build result requires a terminal submission status"),
        ),
    }
}

fn read_candidate_build_id(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<Option<CandidateBuildId>> {
    connection
        .query_row(
            "SELECT build_id FROM candidate_build WHERE episode_id = ?1",
            [episode_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("read Candidate Build identity"))?
        .map(|value| parse_id(&value, "candidate_build.build_id"))
        .transpose()
}

fn upsert_candidate_build_items(
    transaction: &Transaction<'_>,
    build_id: CandidateBuildId,
    items: &[CandidateBuildItemPreparation],
) -> Result<()> {
    let existing = read_candidate_build_items(transaction, build_id)?
        .into_iter()
        .map(|item| (item.claim_id, item))
        .collect::<std::collections::BTreeMap<_, _>>();
    if !existing.is_empty() && existing.len() != items.len() {
        return Err(invariant(
            "persisted Candidate Build Claim coverage changed",
        ));
    }
    for (ordinal, item) in items.iter().enumerate() {
        if let Some(persisted) = existing.get(&item.claim_id) {
            if persisted.checkpoint_id != item.checkpoint_id {
                return Err(invariant("persisted Candidate Build Claim owner changed"));
            }
            if let (Some(expected), Some(actual)) =
                (item.content_hash.as_ref(), persisted.content_hash.as_ref())
                && expected != actual
            {
                return Err(invariant(
                    "deterministic Candidate content hash changed across retry",
                ));
            }
            if persisted.status.is_finalized()
                || (persisted.status == CandidateBuildItemStatus::Prepared
                    && item.status == CandidateBuildItemStatus::NeedsEvidence)
            {
                continue;
            }
            transaction
                .execute(
                    "UPDATE candidate_build_item
                     SET content_hash = ?1, status = ?2,
                         candidate_id = NULL, event_id = NULL, error_code = ?3
                     WHERE build_id = ?4 AND claim_id = ?5",
                    params![
                        item.content_hash.as_deref(),
                        item.status.as_str(),
                        item.error_code.as_deref(),
                        build_id.to_string(),
                        item.claim_id.to_string(),
                    ],
                )
                .map_err(sql_error("refresh Candidate Build item preparation"))?;
            continue;
        }
        let item_ordinal = i64::try_from(ordinal)
            .map_err(|_| invalid("Candidate Build item ordinal exceeds SQLite range"))?;
        transaction
            .execute(
                "INSERT INTO candidate_build_item (
                    build_id, item_ordinal, checkpoint_id, claim_id, submission_id,
                    content_hash, status, candidate_id, event_id, error_code
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8)",
                params![
                    build_id.to_string(),
                    item_ordinal,
                    item.checkpoint_id.to_string(),
                    item.claim_id.to_string(),
                    SubmissionId::new().to_string(),
                    item.content_hash.as_deref(),
                    item.status.as_str(),
                    item.error_code.as_deref(),
                ],
            )
            .map_err(sql_error("insert Candidate Build item"))?;
    }
    Ok(())
}

fn refresh_candidate_build_status(
    transaction: &Transaction<'_>,
    build_id: CandidateBuildId,
) -> Result<()> {
    let items = read_candidate_build_items(transaction, build_id)?;
    let status = if items
        .iter()
        .any(|item| item.status == CandidateBuildItemStatus::Prepared)
    {
        CandidateBuildStatus::Pending
    } else if items.iter().any(|item| {
        matches!(
            item.status,
            CandidateBuildItemStatus::NeedsEvidence | CandidateBuildItemStatus::Failed
        )
    }) {
        CandidateBuildStatus::Incomplete
    } else {
        CandidateBuildStatus::Complete
    };
    transaction
        .execute(
            "UPDATE candidate_build SET status = ?1 WHERE build_id = ?2",
            params![status.as_str(), build_id.to_string()],
        )
        .map_err(sql_error("refresh Candidate Build status"))?;
    Ok(())
}

fn require_candidate_build_view(
    connection: &Connection,
    build_id: CandidateBuildId,
) -> Result<CandidateBuildView> {
    let row = connection
        .query_row(
            "SELECT episode_id, task_session_id, task_id, final_checkpoint_id, status
             FROM candidate_build WHERE build_id = ?1",
            [build_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Candidate Build"))?
        .ok_or_else(|| invariant("Candidate Build disappeared"))?;
    Ok(CandidateBuildView {
        build_id,
        source_episode: WorkEpisodeRef {
            episode_id: parse_id(&row.0, "candidate_build.episode_id")?,
            task_session_id: parse_id(&row.1, "candidate_build.task_session_id")?,
            task_id: parse_id(&row.2, "candidate_build.task_id")?,
        },
        final_checkpoint_id: parse_id(&row.3, "candidate_build.final_checkpoint_id")?,
        status: parse_candidate_build_status(&row.4)?,
        items: read_candidate_build_items(connection, build_id)?,
    })
}

fn read_candidate_build_items(
    connection: &Connection,
    build_id: CandidateBuildId,
) -> Result<Vec<CandidateBuildItemView>> {
    let mut statement = connection
        .prepare(
            "SELECT checkpoint_id, claim_id, submission_id, content_hash, status,
                    candidate_id, event_id, error_code
             FROM candidate_build_item WHERE build_id = ?1 ORDER BY item_ordinal ASC",
        )
        .map_err(sql_error("prepare Candidate Build items"))?;
    let rows = statement
        .query_map([build_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })
        .map_err(sql_error("query Candidate Build items"))?;
    rows.map(|row| {
        let row = row.map_err(sql_error("read Candidate Build item row"))?;
        Ok(CandidateBuildItemView {
            checkpoint_id: parse_id(&row.0, "candidate_build_item.checkpoint_id")?,
            claim_id: parse_id(&row.1, "candidate_build_item.claim_id")?,
            submission_id: parse_id(&row.2, "candidate_build_item.submission_id")?,
            content_hash: row.3,
            status: parse_candidate_build_item_status(&row.4)?,
            candidate_id: row
                .5
                .as_deref()
                .map(|value| parse_id(value, "candidate_build_item.candidate_id"))
                .transpose()?,
            event_id: row
                .6
                .as_deref()
                .map(|value| parse_id(value, "candidate_build_item.event_id"))
                .transpose()?,
            error_code: row.7,
        })
    })
    .collect()
}

fn read_candidate_build_item(
    connection: &Connection,
    build_id: CandidateBuildId,
    submission_id: SubmissionId,
) -> Result<Option<CandidateBuildItemView>> {
    Ok(read_candidate_build_items(connection, build_id)?
        .into_iter()
        .find(|item| item.submission_id == submission_id))
}

fn parse_candidate_build_status(value: &str) -> Result<CandidateBuildStatus> {
    match value {
        "pending" => Ok(CandidateBuildStatus::Pending),
        "complete" => Ok(CandidateBuildStatus::Complete),
        "incomplete" => Ok(CandidateBuildStatus::Incomplete),
        _ => Err(invariant("persisted Candidate Build status is invalid")),
    }
}

fn parse_candidate_build_item_status(value: &str) -> Result<CandidateBuildItemStatus> {
    match value {
        "prepared" => Ok(CandidateBuildItemStatus::Prepared),
        "needs_evidence" => Ok(CandidateBuildItemStatus::NeedsEvidence),
        "created" => Ok(CandidateBuildItemStatus::Created),
        "already_exists" => Ok(CandidateBuildItemStatus::AlreadyExists),
        "failed" => Ok(CandidateBuildItemStatus::Failed),
        _ => Err(invariant(
            "persisted Candidate Build item status is invalid",
        )),
    }
}

const fn candidate_analysis_status_name(
    status: sctx_domain::CandidateAnalysisStatus,
) -> &'static str {
    match status {
        sctx_domain::CandidateAnalysisStatus::Pending => "pending",
        sctx_domain::CandidateAnalysisStatus::Complete => "complete",
        sctx_domain::CandidateAnalysisStatus::Failed => "failed",
    }
}

type CandidateReviewRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

fn candidate_review_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CandidateReviewRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
    ))
}

fn parse_candidate_review_record(row: CandidateReviewRow) -> Result<CandidateReviewRecord> {
    Ok(CandidateReviewRecord {
        candidate_id: parse_id(&row.0, "candidate_review.candidate_id")?,
        submission_id: parse_id(&row.1, "candidate_review.submission_id")?,
        source_episode: WorkEpisodeRef {
            episode_id: parse_id(&row.2, "candidate_review.episode_id")?,
            task_session_id: parse_id(&row.3, "candidate_review.task_session_id")?,
            task_id: parse_id(&row.4, "candidate_review.task_id")?,
        },
        build_id: parse_id(&row.5, "candidate_review.build_id")?,
        final_checkpoint_id: parse_id(&row.6, "candidate_review.final_checkpoint_id")?,
        checkpoint_id: parse_id(&row.7, "candidate_review.checkpoint_id")?,
        claim_id: parse_id(&row.8, "candidate_review.claim_id")?,
        review_version: nonnegative_u64(row.9, "candidate_review.review_version")?,
        status: parse_candidate_review_status(&row.10)?,
        discard_reason: row.11,
        created_at_unix_seconds: nonnegative_u64(
            row.12,
            "candidate_review.created_at_unix_seconds",
        )?,
        expires_at_unix_seconds: nonnegative_u64(
            row.13,
            "candidate_review.expires_at_unix_seconds",
        )?,
        discarded_at_unix_seconds: row
            .14
            .map(|value| nonnegative_u64(value, "candidate_review.discarded_at_unix_seconds"))
            .transpose()?,
        expired_at_unix_seconds: row
            .15
            .map(|value| nonnegative_u64(value, "candidate_review.expired_at_unix_seconds"))
            .transpose()?,
        confirmation_id: row
            .16
            .as_deref()
            .map(|value| parse_id(value, "candidate_review.confirmation_id"))
            .transpose()?,
        result_context_id: row
            .17
            .as_deref()
            .map(|value| parse_id(value, "candidate_review.result_context_id"))
            .transpose()?,
    })
}

fn read_candidate_review_record(
    connection: &Connection,
    candidate_id: CandidateId,
) -> Result<Option<CandidateReviewRecord>> {
    connection
        .query_row(
            "SELECT candidate_id, submission_id, episode_id, task_session_id, task_id,
                    build_id, final_checkpoint_id, checkpoint_id, claim_id,
                    review_version, status, discard_reason, created_at_unix_seconds,
                    expires_at_unix_seconds, discarded_at_unix_seconds,
                    expired_at_unix_seconds, confirmation_id, result_context_id
             FROM candidate_review WHERE candidate_id = ?1",
            [candidate_id.to_string()],
            candidate_review_row,
        )
        .optional()
        .map_err(sql_error("read Candidate Review"))?
        .map(parse_candidate_review_record)
        .transpose()
}

const fn candidate_review_status_name(status: CandidateReviewStatus) -> &'static str {
    match status {
        CandidateReviewStatus::Pending => "pending",
        CandidateReviewStatus::Discarded => "discarded",
        CandidateReviewStatus::Expired => "expired",
        CandidateReviewStatus::Confirmed => "confirmed",
    }
}

fn parse_candidate_review_status(value: &str) -> Result<CandidateReviewStatus> {
    match value {
        "pending" => Ok(CandidateReviewStatus::Pending),
        "discarded" => Ok(CandidateReviewStatus::Discarded),
        "expired" => Ok(CandidateReviewStatus::Expired),
        "confirmed" => Ok(CandidateReviewStatus::Confirmed),
        _ => Err(invariant("persisted Candidate Review status is invalid")),
    }
}

fn read_candidate_confirmation_operation(
    connection: &Connection,
    candidate_id: CandidateId,
) -> Result<Option<CandidateConfirmationOperationView>> {
    connection
        .query_row(
            "SELECT review_parent_version, operation_hash, plan_hash, plan_json, status
             FROM candidate_confirmation_operation WHERE candidate_id = ?1",
            [candidate_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Candidate Confirmation operation"))?
        .map(
            |(review_parent_version, operation_hash, plan_hash, plan_json, status)| {
                let plan: CandidateConfirmationPlan = serde_json::from_str(&plan_json)
                    .map_err(json_error("parse Candidate Confirmation plan"))?;
                plan.validate()?;
                if plan.operation_hash != operation_hash
                    || plan.plan_hash() != plan_hash
                    || plan.operation.candidate_id != candidate_id
                    || plan.operation.review_parent_version
                        != nonnegative_u64(
                            review_parent_version,
                            "candidate_confirmation_operation.review_parent_version",
                        )?
                {
                    return Err(invariant(
                        "persisted Candidate Confirmation operation metadata disagrees with plan",
                    ));
                }
                Ok(CandidateConfirmationOperationView {
                    candidate_id,
                    review_parent_version: plan.operation.review_parent_version,
                    operation_hash,
                    plan,
                    status: match status.as_str() {
                        "reserved" => CandidateConfirmationOperationStatus::Reserved,
                        "committed" => CandidateConfirmationOperationStatus::Committed,
                        _ => {
                            return Err(invariant(
                                "persisted Candidate Confirmation operation status is invalid",
                            ));
                        }
                    },
                })
            },
        )
        .transpose()
}

fn candidate_review_cursor(record: &CandidateReviewRecord) -> String {
    format!(
        "crv1:{:020}:{}",
        record.created_at_unix_seconds, record.candidate_id
    )
}

fn parse_candidate_review_cursor(value: &str) -> Result<(u64, CandidateId)> {
    let mut parts = value.splitn(3, ':');
    let (Some(version), Some(created_at), Some(candidate_id)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("Candidate Review cursor is malformed"));
    };
    if version != "crv1" || created_at.len() != 20 {
        return Err(invalid("Candidate Review cursor is malformed"));
    }
    let created_at = created_at
        .parse::<u64>()
        .map_err(|_| invalid("Candidate Review cursor timestamp is invalid"))?;
    let candidate_id = parse_id(candidate_id, "Candidate Review cursor CandidateId")?;
    Ok((created_at, candidate_id))
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| invariant(format!("{field} is negative")))
}

fn unix_seconds(time: SystemTime) -> Result<u64> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| invalid("Candidate Review timestamp is before the Unix epoch"))
}

fn next_episode_ordinal(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(episode_ordinal), -1) + 1
         FROM work_episode WHERE task_session_id = ?1",
        task_session_id.to_string(),
        "read next Work Episode ordinal",
    )
}

fn insert_all_missing_episode_refs(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
    task_session_id: TaskSessionId,
    task_id: TaskId,
) -> Result<(usize, usize)> {
    let mut next_intent_ordinal = next_episode_ref_ordinal(
        transaction,
        "work_episode_intent_ref",
        episode_id,
        "read next Episode Intent ref ordinal",
    )?;
    let mut intent_statement = transaction
        .prepare(
            "SELECT revision_id FROM task_intent_revision
             WHERE task_session_id = ?1
             ORDER BY revision_ordinal ASC",
        )
        .map_err(sql_error("prepare Task Intent refs"))?;
    let intent_ids = intent_statement
        .query_map([task_session_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(sql_error("query Task Intent refs"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("read Task Intent ref"))?;
    drop(intent_statement);
    let mut added_intent_revisions = 0_usize;
    for revision_id in intent_ids {
        let changed = transaction
            .execute(
                "INSERT OR IGNORE INTO work_episode_intent_ref (
                    episode_id, revision_id, ref_ordinal
                 ) VALUES (?1, ?2, ?3)",
                params![episode_id.to_string(), revision_id, next_intent_ordinal],
            )
            .map_err(sql_error("insert Episode Intent ref"))?;
        if changed == 1 {
            added_intent_revisions = added_intent_revisions.saturating_add(1);
            next_intent_ordinal = next_intent_ordinal
                .checked_add(1)
                .ok_or_else(|| invariant("Episode Intent ref ordinal overflow"))?;
        }
    }

    let mut next_signal_ref_ordinal = next_episode_ref_ordinal(
        transaction,
        "work_episode_signal_ref",
        episode_id,
        "read next Episode Signal ref ordinal",
    )?;
    let mut signal_statement = transaction
        .prepare(
            "SELECT signal_id FROM task_signal
             WHERE task_session_id = ?1 AND task_id = ?2
             ORDER BY signal_ordinal ASC",
        )
        .map_err(sql_error("prepare Task Signal refs"))?;
    let signal_ids = signal_statement
        .query_map(
            params![task_session_id.to_string(), task_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error("query Task Signal refs"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("read Task Signal ref"))?;
    drop(signal_statement);
    let mut added_signal_refs = 0_usize;
    for signal_id in signal_ids {
        let changed = transaction
            .execute(
                "INSERT OR IGNORE INTO work_episode_signal_ref (
                    episode_id, signal_id, ref_ordinal
                 ) VALUES (?1, ?2, ?3)",
                params![episode_id.to_string(), signal_id, next_signal_ref_ordinal],
            )
            .map_err(sql_error("insert Episode Signal ref"))?;
        if changed == 1 {
            added_signal_refs = added_signal_refs.saturating_add(1);
            next_signal_ref_ordinal = next_signal_ref_ordinal
                .checked_add(1)
                .ok_or_else(|| invariant("Episode Signal ref ordinal overflow"))?;
        }
    }
    Ok((added_intent_revisions, added_signal_refs))
}

fn next_episode_ref_ordinal(
    transaction: &Transaction<'_>,
    table: &str,
    episode_id: WorkEpisodeId,
    context: &'static str,
) -> Result<i64> {
    if !matches!(table, "work_episode_intent_ref" | "work_episode_signal_ref") {
        return Err(invariant("unsupported Episode ref table"));
    }
    transaction
        .query_row(
            &format!(
                "SELECT COALESCE(MAX(ref_ordinal), -1) + 1 FROM {table} WHERE episode_id = ?1"
            ),
            [episode_id.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error(context))
}

fn require_episode_head(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<(TaskSessionId, TaskId, u64, String)> {
    connection
        .query_row(
            "SELECT task_session_id, task_id, version, status
             FROM work_episode WHERE episode_id = ?1",
            [episode_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Work Episode head"))?
        .map(|(task_session_id, task_id, version, status)| {
            Ok((
                parse_id(&task_session_id, "work_episode.task_session_id")?,
                parse_id(&task_id, "work_episode.task_id")?,
                u64::try_from(version).map_err(|_| invariant("negative Work Episode version"))?,
                status,
            ))
        })
        .transpose()?
        .ok_or_else(|| invalid("Work Episode does not exist"))
}

fn require_open_episode_version(actual: u64, status: &str, expected: u64) -> Result<()> {
    if status != "open" {
        return Err(stale("Work Episode is not open"));
    }
    if actual != expected {
        return Err(stale("expected Work Episode version is stale"));
    }
    Ok(())
}

fn require_task_is_active(
    connection: &Connection,
    task_session_id: TaskSessionId,
    task_id: TaskId,
) -> Result<()> {
    let active = connection
        .query_row(
            "SELECT 1 FROM external_session
             WHERE active_task_session_id = ?1 AND active_task_id = ?2",
            params![task_session_id.to_string(), task_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error("verify Work Episode ActiveTask owner"))?;
    if active.is_none() {
        return Err(invalid(
            "Work Episode Task is not the ExternalSession ActiveTask",
        ));
    }
    Ok(())
}

fn advance_episode_version(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
    expected_version: u64,
) -> Result<()> {
    let expected = i64::try_from(expected_version)
        .map_err(|_| invalid("Work Episode version exceeds SQLite range"))?;
    let changed = transaction
        .execute(
            "UPDATE work_episode SET version = version + 1
             WHERE episode_id = ?1 AND version = ?2 AND status = 'open'",
            params![episode_id.to_string(), expected],
        )
        .map_err(sql_error("advance Work Episode version"))?;
    if changed != 1 {
        return Err(stale("expected Work Episode version became stale"));
    }
    Ok(())
}

fn close_episode_version(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
    expected_version: u64,
    checkpoint_id: sctx_domain::AgentCheckpointId,
) -> Result<()> {
    let expected = i64::try_from(expected_version)
        .map_err(|_| invalid("Work Episode version exceeds SQLite range"))?;
    let changed = transaction
        .execute(
            "UPDATE work_episode
             SET version = version + 1, status = 'closed', final_checkpoint_id = ?3
             WHERE episode_id = ?1 AND version = ?2 AND status = 'open'",
            params![episode_id.to_string(), expected, checkpoint_id.to_string()],
        )
        .map_err(sql_error("close Work Episode at Agent Checkpoint"))?;
    if changed != 1 {
        return Err(stale("expected Work Episode version became stale"));
    }
    Ok(())
}

fn append_observation_in_transaction(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
    expected_version: u64,
    intent_revision_id: TaskIntentRevisionId,
    source_refs: Vec<WorkSourceRef>,
    normalized: NormalizedWorkObservation,
) -> Result<WorkObservationId> {
    let (task_session_id, task_id, version, status) =
        require_episode_head(transaction, episode_id)?;
    require_open_episode_version(version, &status, expected_version)?;
    require_task_is_active(transaction, task_session_id, task_id)?;
    let mut episode = require_episode_view(transaction, episode_id)?.episode;
    let observation = WorkObservation::from_parts(
        task_session_id,
        task_id,
        intent_revision_id,
        source_refs,
        normalized,
    )?;
    episode.add_observation(observation.clone())?;
    insert_observation_rows(transaction, episode_id, &observation)?;
    advance_episode_version(transaction, episode_id, expected_version)?;
    Ok(observation.observation_id)
}

fn insert_observation_rows(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
    observation: &WorkObservation,
) -> Result<()> {
    let observation_ordinal = next_observation_ordinal(transaction, episode_id)?;
    let observation_json = serde_json::to_string(&observation.observation)
        .map_err(json_error("serialize normalized Work Observation"))?;
    transaction
        .execute(
            "INSERT INTO work_observation (
                observation_id, episode_id, task_session_id, task_id,
                intent_revision_id, observation_ordinal, observation_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                observation.observation_id.to_string(),
                episode_id.to_string(),
                observation.task_session_id.to_string(),
                observation.task_id.to_string(),
                observation.intent_revision_id.to_string(),
                observation_ordinal,
                observation_json,
            ],
        )
        .map_err(sql_error("insert Work Observation"))?;
    for (ordinal, source) in observation.source_refs.iter().enumerate() {
        let source_json = serde_json::to_string(source)
            .map_err(json_error("serialize Work Observation source"))?;
        transaction
            .execute(
                "INSERT INTO work_observation_source (
                    observation_id, source_ordinal, source_json
                 ) VALUES (?1, ?2, ?3)",
                params![
                    observation.observation_id.to_string(),
                    i64::try_from(ordinal)
                        .map_err(|_| invariant("Observation source ordinal overflow"))?,
                    source_json,
                ],
            )
            .map_err(sql_error("insert Work Observation source"))?;
    }
    Ok(())
}

fn next_observation_ordinal(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(observation_ordinal), -1) + 1
         FROM work_observation WHERE episode_id = ?1",
        episode_id.to_string(),
        "read next Work Observation ordinal",
    )
}

fn next_checkpoint_ordinal(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(checkpoint_ordinal), -1) + 1
         FROM agent_checkpoint WHERE episode_id = ?1",
        episode_id.to_string(),
        "read next Agent Checkpoint ordinal",
    )
}

fn read_capture_ingestion(
    connection: &Connection,
    capture_id: CaptureId,
) -> Result<Option<(WorkEpisodeId, WorkObservationId, TaskSessionId, TaskId)>> {
    connection
        .query_row(
            "SELECT episode_id, observation_id, task_session_id, task_id
             FROM capture_ingestion WHERE capture_id = ?1",
            [capture_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Capture ingestion"))?
        .map(|(episode_id, observation_id, task_session_id, task_id)| {
            Ok((
                parse_id(&episode_id, "capture_ingestion.episode_id")?,
                parse_id(&observation_id, "capture_ingestion.observation_id")?,
                parse_id(&task_session_id, "capture_ingestion.task_session_id")?,
                parse_id(&task_id, "capture_ingestion.task_id")?,
            ))
        })
        .transpose()
}

fn insert_episode_diagnostics(
    transaction: &Transaction<'_>,
    episode_id: WorkEpisodeId,
    capture_id: CaptureId,
    diagnostics: &[WorkEpisodeDiagnosticKind],
) -> Result<()> {
    let mut next_ordinal: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(diagnostic_ordinal), -1) + 1
             FROM work_episode_diagnostic WHERE episode_id = ?1",
            [episode_id.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error("read next Episode diagnostic ordinal"))?;
    let mut unique = HashSet::new();
    for diagnostic in diagnostics.iter().copied() {
        if !unique.insert(diagnostic) {
            continue;
        }
        let changed = transaction
            .execute(
                "INSERT OR IGNORE INTO work_episode_diagnostic (
                    episode_id, diagnostic_ordinal, capture_id, kind
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    episode_id.to_string(),
                    next_ordinal,
                    capture_id.to_string(),
                    episode_diagnostic_kind_name(diagnostic),
                ],
            )
            .map_err(sql_error("insert Work Episode diagnostic"))?;
        if changed == 1 {
            next_ordinal = next_ordinal
                .checked_add(1)
                .ok_or_else(|| invariant("Episode diagnostic ordinal overflow"))?;
        }
    }
    Ok(())
}

fn read_episode_view(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<Option<WorkEpisodeView>> {
    let row = connection
        .query_row(
            "SELECT task_session_id, task_id, version, status, final_checkpoint_id
             FROM work_episode WHERE episode_id = ?1",
            [episode_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Work Episode"))?;
    let Some((task_session_id, task_id, version, status, final_checkpoint_id)) = row else {
        return Ok(None);
    };
    let task_session_id = parse_id(&task_session_id, "work_episode.task_session_id")?;
    let task_id = parse_id(&task_id, "work_episode.task_id")?;
    let status = match status.as_str() {
        "open" if final_checkpoint_id.is_none() => WorkEpisodeStatus::Open,
        "closed" => WorkEpisodeStatus::Closed {
            final_checkpoint_id: parse_id(
                final_checkpoint_id
                    .as_deref()
                    .ok_or_else(|| invariant("closed Episode lacks final Checkpoint"))?,
                "work_episode.final_checkpoint_id",
            )?,
        },
        _ => return Err(invariant("persisted Work Episode status is invalid")),
    };
    let episode = WorkEpisode {
        episode_id,
        version: u64::try_from(version).map_err(|_| invariant("negative Work Episode version"))?,
        task_session_id,
        task_id,
        intent_revisions: IntentRevisionRange::new(read_episode_intent_refs(
            connection, episode_id,
        )?)?,
        signal_refs: read_episode_signal_refs(connection, episode_id)?,
        observations: read_episode_observations(connection, episode_id, task_session_id, task_id)?,
        status,
    };
    episode.validate().map_err(|error| {
        invariant(format!(
            "persisted Work Episode violates contract: {}",
            error.message()
        ))
    })?;
    let checkpoints = read_episode_checkpoints(connection, episode_id)?;
    for checkpoint in &checkpoints {
        checkpoint
            .validate_against_episode(&episode)
            .map_err(|error| {
                invariant(format!(
                    "persisted Agent Checkpoint violates Episode contract: {}",
                    error.message()
                ))
            })?;
    }
    if let WorkEpisodeStatus::Closed {
        final_checkpoint_id,
    } = episode.status
        && checkpoints
            .last()
            .is_none_or(|checkpoint| checkpoint.checkpoint_id != final_checkpoint_id)
    {
        return Err(invariant(
            "closed Work Episode final Checkpoint is not its persisted final Checkpoint",
        ));
    }
    Ok(Some(WorkEpisodeView {
        episode,
        checkpoints,
        diagnostics: read_episode_diagnostics(connection, episode_id)?,
    }))
}

fn require_episode_view(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<WorkEpisodeView> {
    read_episode_view(connection, episode_id)?
        .ok_or_else(|| invariant("Work Episode disappeared inside transaction"))
}

fn read_episode_checkpoints(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<Vec<AgentCheckpoint>> {
    let mut statement = connection
        .prepare(
            "SELECT checkpoint_json FROM agent_checkpoint
             WHERE episode_id = ?1 ORDER BY checkpoint_ordinal ASC",
        )
        .map_err(sql_error("prepare Agent Checkpoint history"))?;
    let rows = statement
        .query_map([episode_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(sql_error("query Agent Checkpoint history"))?;
    rows.map(|row| {
        serde_json::from_str(&row.map_err(sql_error("read Agent Checkpoint row"))?)
            .map_err(json_error("parse Agent Checkpoint history"))
    })
    .collect()
}

fn read_episode_intent_refs(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<Vec<TaskIntentRevisionId>> {
    let mut statement = connection
        .prepare(
            "SELECT revision_id FROM work_episode_intent_ref
             WHERE episode_id = ?1 ORDER BY ref_ordinal ASC",
        )
        .map_err(sql_error("prepare Episode Intent refs"))?;
    let rows = statement
        .query_map([episode_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(sql_error("query Episode Intent refs"))?;
    rows.map(|row| {
        parse_id(
            &row.map_err(sql_error("read Episode Intent ref"))?,
            "work_episode_intent_ref.revision_id",
        )
    })
    .collect()
}

fn read_episode_signal_refs(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<Vec<NonLocatingSignalRef>> {
    let mut statement = connection
        .prepare(
            "SELECT signal.signal_id, signal.task_session_id, signal.task_id, signal.kind
             FROM work_episode_signal_ref episode_ref
             JOIN task_signal signal ON signal.signal_id = episode_ref.signal_id
             WHERE episode_ref.episode_id = ?1 ORDER BY episode_ref.ref_ordinal ASC",
        )
        .map_err(sql_error("prepare Episode Signal refs"))?;
    let rows = statement
        .query_map([episode_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error("query Episode Signal refs"))?;
    rows.map(|row| {
        let (signal_id, task_session_id, task_id, kind) =
            row.map_err(sql_error("read Episode Signal ref"))?;
        Ok(NonLocatingSignalRef {
            signal_id: parse_id(&signal_id, "work_episode_signal_ref.signal_id")?,
            task_session_id: parse_id(&task_session_id, "work_episode_signal_ref.task_session_id")?,
            task_id: parse_id(&task_id, "work_episode_signal_ref.task_id")?,
            kind: parse_signal_kind(&kind)?,
        })
    })
    .collect()
}

fn read_episode_observations(
    connection: &Connection,
    episode_id: WorkEpisodeId,
    task_session_id: TaskSessionId,
    task_id: TaskId,
) -> Result<Vec<WorkObservation>> {
    let mut statement = connection
        .prepare(
            "SELECT observation_id, intent_revision_id, observation_json
             FROM work_observation WHERE episode_id = ?1
             ORDER BY observation_ordinal ASC",
        )
        .map_err(sql_error("prepare Work Observations"))?;
    let rows = statement
        .query_map([episode_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sql_error("query Work Observations"))?;
    let mut observations = Vec::new();
    for row in rows {
        let (observation_id, intent_revision_id, observation_json) =
            row.map_err(sql_error("read Work Observation"))?;
        observations.push(WorkObservation {
            observation_id: parse_id(&observation_id, "work_observation.observation_id")?,
            task_session_id,
            task_id,
            intent_revision_id: parse_id(
                &intent_revision_id,
                "work_observation.intent_revision_id",
            )?,
            source_refs: read_observation_sources(connection, &observation_id)?,
            observation: serde_json::from_str(&observation_json)
                .map_err(json_error("parse normalized Work Observation"))?,
        });
    }
    Ok(observations)
}

fn read_observation_sources(
    connection: &Connection,
    observation_id: &str,
) -> Result<Vec<WorkSourceRef>> {
    let mut statement = connection
        .prepare(
            "SELECT source_json FROM work_observation_source
             WHERE observation_id = ?1 ORDER BY source_ordinal ASC",
        )
        .map_err(sql_error("prepare Work Observation sources"))?;
    let rows = statement
        .query_map([observation_id], |row| row.get::<_, String>(0))
        .map_err(sql_error("query Work Observation sources"))?;
    rows.map(|row| {
        serde_json::from_str(&row.map_err(sql_error("read Work Observation source"))?)
            .map_err(json_error("parse Work Observation source"))
    })
    .collect()
}

fn read_episode_diagnostics(
    connection: &Connection,
    episode_id: WorkEpisodeId,
) -> Result<Vec<WorkEpisodeDiagnostic>> {
    let mut statement = connection
        .prepare(
            "SELECT capture_id, kind FROM work_episode_diagnostic
             WHERE episode_id = ?1 ORDER BY diagnostic_ordinal ASC",
        )
        .map_err(sql_error("prepare Work Episode diagnostics"))?;
    let rows = statement
        .query_map([episode_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error("query Work Episode diagnostics"))?;
    rows.map(|row| {
        let (capture_id, kind) = row.map_err(sql_error("read Work Episode diagnostic"))?;
        Ok(WorkEpisodeDiagnostic {
            capture_id: parse_id(&capture_id, "work_episode_diagnostic.capture_id")?,
            kind: parse_episode_diagnostic_kind(&kind)?,
        })
    })
    .collect()
}

const fn episode_diagnostic_kind_name(kind: WorkEpisodeDiagnosticKind) -> &'static str {
    match kind {
        WorkEpisodeDiagnosticKind::CaptureRepositoryNotConfigured => {
            "capture_repository_not_configured"
        }
        WorkEpisodeDiagnosticKind::CaptureUnsafeArtifactPath => "capture_unsafe_artifact_path",
    }
}

fn parse_episode_diagnostic_kind(value: &str) -> Result<WorkEpisodeDiagnosticKind> {
    match value {
        "capture_repository_not_configured" => {
            Ok(WorkEpisodeDiagnosticKind::CaptureRepositoryNotConfigured)
        }
        "capture_unsafe_artifact_path" => Ok(WorkEpisodeDiagnosticKind::CaptureUnsafeArtifactPath),
        _ => Err(invariant("unknown persisted Work Episode diagnostic kind")),
    }
}

fn find_active_signal(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    signal: &TaskSignal,
) -> Result<Option<SignalId>> {
    transaction
        .query_row(
            "SELECT signal_id FROM task_signal
             WHERE task_session_id = ?1 AND kind = ?2 AND content = ?3
               AND lifecycle = 'active'",
            params![
                task_session_id.to_string(),
                signal_kind_name(signal.kind),
                signal.content,
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find active Task Signal"))?
        .map(|value| parse_id(&value, "task_signal.signal_id"))
        .transpose()
}

fn read_external_identity(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<ExternalIdentity>> {
    transaction
        .query_row(
            "SELECT external_session_id, active_task_session_id, active_task_id
             FROM external_session
             WHERE agent_kind = ?1 AND external_session_key = ?2",
            params![locator.agent_kind, locator.external_session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read ExternalSession identity"))?
        .map(|(external_session_id, task_session_id, task_id)| {
            Ok(ExternalIdentity {
                external_session_id: parse_id(
                    &external_session_id,
                    "external_session.external_session_id",
                )?,
                active_task_session_id: parse_id(
                    &task_session_id,
                    "external_session.active_task_session_id",
                )?,
                active_task_id: parse_id(&task_id, "external_session.active_task_id")?,
            })
        })
        .transpose()
}

fn find_active_task_by_locator(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<TaskSessionId>> {
    Ok(read_external_identity(transaction, locator)?.map(|value| value.active_task_session_id))
}

fn find_task_in_external_session(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    task_id: TaskId,
) -> Result<Option<TaskSessionId>> {
    transaction
        .query_row(
            "SELECT task_session_id FROM task_session
             WHERE external_session_id = ?1 AND task_id = ?2",
            params![external_session_id.to_string(), task_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("find retained Task"))?
        .map(|value| parse_id(&value, "task_session.task_session_id"))
        .transpose()
}

fn require_expected_active(actual: TaskId, expected: TaskId) -> Result<()> {
    if actual != expected {
        return Err(invalid(
            "expected_active_task_id does not match the ExternalSession ActiveTask",
        ));
    }
    Ok(())
}

fn compare_and_switch(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
    expected_active_task_id: TaskId,
    target_task_session_id: TaskSessionId,
    target_task_id: TaskId,
) -> Result<()> {
    let changed = transaction
        .execute(
            "UPDATE external_session
             SET active_task_session_id = ?1, active_task_id = ?2
             WHERE external_session_id = ?3 AND active_task_id = ?4",
            params![
                target_task_session_id.to_string(),
                target_task_id.to_string(),
                external_session_id.to_string(),
                expected_active_task_id.to_string(),
            ],
        )
        .map_err(sql_error("compare and switch ActiveTask"))?;
    if changed != 1 {
        return Err(invalid("expected_active_task_id became stale"));
    }
    Ok(())
}

fn read_active_task_head(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Option<(TaskId, TaskIntentRevisionId)>> {
    let row = transaction
        .query_row(
            "SELECT task.task_id, task.current_intent_revision_id
             FROM task_session task
             JOIN external_session external
               ON external.external_session_id = task.external_session_id
              AND external.active_task_session_id = task.task_session_id
              AND external.active_task_id = task.task_id
             WHERE task.task_session_id = ?1",
            [task_session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("read ActiveTask Head"))?;
    row.map(|(task_id, revision_id)| {
        Ok((
            parse_id(&task_id, "task_session.task_id")?,
            parse_id(&revision_id, "task_session.current_intent_revision_id")?,
        ))
    })
    .transpose()
}

fn reject_invalid_parent(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    parent_revision_id: TaskIntentRevisionId,
) -> Result<()> {
    let owner = transaction
        .query_row(
            "SELECT revision.task_session_id, task.task_id
             FROM task_intent_revision AS revision
             JOIN task_session AS task ON task.task_session_id = revision.task_session_id
             WHERE revision.revision_id = ?1",
            [parent_revision_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error("inspect rejected Intent parent"))?;
    if let Some((owner_session, owner_task)) = owner {
        let owner_session: TaskSessionId = parse_id(&owner_session, "revision.task_session_id")?;
        let owner_task: TaskId = parse_id(&owner_task, "revision.task_id")?;
        if owner_session != task_session_id || owner_task != task_id {
            return Err(invalid("Intent parent belongs to another Task Session"));
        }
    }
    Err(invalid("Intent parent must be the ActiveTask current Head"))
}

fn next_task_ordinal(
    transaction: &Transaction<'_>,
    external_session_id: ExternalSessionId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(task_ordinal), -1) + 1 FROM task_session WHERE external_session_id = ?1",
        external_session_id.to_string(),
        "read next Task ordinal",
    )
}

fn next_signal_ordinal(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<i64> {
    next_ordinal(
        transaction,
        "SELECT COALESCE(MAX(signal_ordinal), -1) + 1 FROM task_signal WHERE task_session_id = ?1",
        task_session_id.to_string(),
        "read next Signal ordinal",
    )
}

fn next_ordinal(
    transaction: &Transaction<'_>,
    query: &str,
    identity: String,
    context: &'static str,
) -> Result<i64> {
    transaction
        .query_row(query, [identity], |row| row.get(0))
        .map_err(sql_error(context))
}

fn read_revision_ordinal(
    transaction: &Transaction<'_>,
    revision_id: TaskIntentRevisionId,
) -> Result<i64> {
    transaction
        .query_row(
            "SELECT revision_ordinal FROM task_intent_revision WHERE revision_id = ?1",
            [revision_id.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error("read Intent revision ordinal"))
}

fn require_snapshot(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<TaskSessionSnapshot> {
    read_snapshot_in_transaction(transaction, task_session_id)?
        .ok_or_else(|| invariant("Task disappeared inside runtime transaction"))
}

fn read_snapshot_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Option<TaskSessionSnapshot>> {
    let row = transaction
        .query_row(
            "SELECT task.task_id, external.agent_kind, external.external_session_key,
                    task.current_intent_revision_id
             FROM task_session task
             JOIN external_session external
               ON external.external_session_id = task.external_session_id
             WHERE task.task_session_id = ?1",
            [task_session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Task Session"))?;
    let Some((task_id, agent_kind, external_session_key, current_revision_id)) = row else {
        return Ok(None);
    };
    let task_id = parse_id(&task_id, "task_session.task_id")?;
    let current_revision_id = parse_id(&current_revision_id, "task_session.current_revision_id")?;
    let snapshot = TaskSessionSnapshot {
        task_session_id,
        task_id,
        external_session_locator: ExternalSessionLocator {
            agent_kind,
            external_session_id: external_session_key,
        },
        intent_revisions: read_intent_revisions(transaction, task_session_id)?,
        task_signals: read_active_task_signals(transaction, task_session_id)?,
    };
    snapshot.validate().map_err(|error| {
        invariant(format!(
            "persisted Task violates contract: {}",
            error.message()
        ))
    })?;
    if snapshot
        .current_intent_revision()
        .is_none_or(|revision| revision.revision_id != current_revision_id)
    {
        return Err(invariant(
            "persisted Intent Head differs from revision chain",
        ));
    }
    Ok(Some(snapshot))
}

fn read_external_session_in_transaction(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<ExternalSessionSnapshot>> {
    let Some(identity) = read_external_identity(transaction, locator)? else {
        return Ok(None);
    };
    let mut statement = transaction
        .prepare(
            "SELECT task_session_id FROM task_session
             WHERE external_session_id = ?1 ORDER BY task_ordinal ASC",
        )
        .map_err(sql_error("prepare Task history"))?;
    let rows = statement
        .query_map([identity.external_session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error("query Task history"))?;
    let mut tasks = Vec::new();
    for row in rows {
        let task_session_id = parse_id(
            &row.map_err(sql_error("read Task history row"))?,
            "task_session.task_session_id",
        )?;
        tasks.push(require_snapshot(transaction, task_session_id)?);
    }
    let snapshot = ExternalSessionSnapshot {
        external_session_id: identity.external_session_id,
        locator: locator.clone(),
        active_task_session_id: identity.active_task_session_id,
        active_task_id: identity.active_task_id,
        tasks,
    };
    snapshot.validate().map_err(|error| {
        invariant(format!(
            "persisted ExternalSession violates contract: {}",
            error.message()
        ))
    })?;
    Ok(Some(snapshot))
}

fn read_intent_revisions(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskIntentRevision>> {
    let mut statement = transaction
        .prepare(
            "SELECT revision_id, parent_revision_id, authority_json, semantic_hash
             FROM task_intent_revision
             WHERE task_session_id = ?1 ORDER BY revision_ordinal ASC",
        )
        .map_err(sql_error("prepare Intent revisions"))?;
    let rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error("query Intent revisions"))?;
    let mut revisions = Vec::new();
    for row in rows {
        let (revision_id, parent_revision_id, authority_json, semantic_hash) =
            row.map_err(sql_error("read Intent revision row"))?;
        let working_intent: WorkingIntentSnapshot = serde_json::from_str(&authority_json)
            .map_err(json_error("parse Working Intent authority"))?;
        let revision = TaskIntentRevision {
            revision_id: parse_id(&revision_id, "revision.revision_id")?,
            parent_revision_id: parent_revision_id
                .map(|value| parse_id(&value, "revision.parent_revision_id"))
                .transpose()?,
            working_intent,
            semantic_hash,
        };
        revision.validate()?;
        revisions.push(revision);
    }
    Ok(revisions)
}

fn read_intent_revision(
    connection: &Connection,
    revision_id: TaskIntentRevisionId,
) -> Result<TaskIntentRevision> {
    connection
        .query_row(
            "SELECT parent_revision_id, authority_json, semantic_hash
             FROM task_intent_revision WHERE revision_id = ?1",
            [revision_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Working Intent revision"))?
        .map(|(parent_revision_id, authority_json, semantic_hash)| {
            let revision = TaskIntentRevision {
                revision_id,
                parent_revision_id: parent_revision_id
                    .map(|value| parse_id(&value, "revision.parent_revision_id"))
                    .transpose()?,
                working_intent: serde_json::from_str(&authority_json)
                    .map_err(json_error("parse Working Intent authority"))?,
                semantic_hash,
            };
            revision.validate()?;
            Ok(revision)
        })
        .transpose()?
        .ok_or_else(|| invariant("ActiveTask Working Intent Head disappeared"))
}

fn read_active_task_signals(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignal>> {
    Ok(read_signal_records(connection, task_session_id)?
        .into_iter()
        .filter(|record| record.lifecycle == TaskSignalLifecycle::Active)
        .map(|record| record.signal)
        .collect())
}

fn read_signal_records(
    connection: &Connection,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignalRecord>> {
    let mut statement = connection
        .prepare(
            "SELECT signal_id, task_id, kind, content, lifecycle FROM task_signal
             WHERE task_session_id = ?1 ORDER BY signal_ordinal ASC",
        )
        .map_err(sql_error("prepare Signal history"))?;
    let rows = statement
        .query_map([task_session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(sql_error("query Signal history"))?;
    let mut records = Vec::new();
    for row in rows {
        let (signal_id, task_id, kind, content, lifecycle) =
            row.map_err(sql_error("read Signal history row"))?;
        let record = TaskSignalRecord {
            signal_id: parse_id(&signal_id, "signal.signal_id")?,
            task_session_id,
            task_id: parse_id(&task_id, "signal.task_id")?,
            signal: TaskSignal {
                kind: parse_signal_kind(&kind)?,
                content,
            },
            lifecycle: parse_signal_lifecycle(&lifecycle)?,
        };
        record.validate_for_task(task_session_id, record.task_id)?;
        records.push(record);
    }
    Ok(records)
}

fn read_signal_record(
    transaction: &Transaction<'_>,
    signal_id: SignalId,
) -> Result<Option<TaskSignalRecord>> {
    transaction
        .query_row(
            "SELECT task_session_id, task_id, kind, content, lifecycle
             FROM task_signal WHERE signal_id = ?1",
            [signal_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Signal record"))?
        .map(|(task_session_id, task_id, kind, content, lifecycle)| {
            Ok(TaskSignalRecord {
                signal_id,
                task_session_id: parse_id(&task_session_id, "signal.task_session_id")?,
                task_id: parse_id(&task_id, "signal.task_id")?,
                signal: TaskSignal {
                    kind: parse_signal_kind(&kind)?,
                    content,
                },
                lifecycle: parse_signal_lifecycle(&lifecycle)?,
            })
        })
        .transpose()
}

fn require_unique_signal_ids(signal_ids: &[SignalId]) -> Result<()> {
    if signal_ids.is_empty() {
        return Err(invalid("signal_ids must contain at least one SignalId"));
    }
    let mut unique = HashSet::with_capacity(signal_ids.len());
    if signal_ids
        .iter()
        .any(|signal_id| !unique.insert(*signal_id))
    {
        return Err(invalid("signal_ids must not contain duplicates"));
    }
    Ok(())
}

fn normalize_signals(signals: Vec<TaskSignal>) -> Result<Vec<TaskSignal>> {
    let mut normalized = Vec::with_capacity(signals.len());
    let mut seen = HashSet::with_capacity(signals.len());
    for signal in signals {
        let signal = TaskSignal {
            kind: signal.kind,
            content: signal.content.trim().to_owned(),
        };
        signal.validate()?;
        if seen.insert(signal.clone()) {
            normalized.push(signal);
        }
    }
    Ok(normalized)
}

const fn signal_kind_name(kind: TaskSignalKind) -> &'static str {
    match kind {
        TaskSignalKind::Prompt => "prompt",
        TaskSignalKind::Workspace => "workspace",
        TaskSignalKind::Diff => "diff",
        TaskSignalKind::TestOutcome => "test_outcome",
    }
}

fn parse_signal_kind(value: &str) -> Result<TaskSignalKind> {
    match value {
        "prompt" => Ok(TaskSignalKind::Prompt),
        "workspace" => Ok(TaskSignalKind::Workspace),
        "diff" => Ok(TaskSignalKind::Diff),
        "test_outcome" => Ok(TaskSignalKind::TestOutcome),
        _ => Err(invariant(format!("unknown persisted Signal kind: {value}"))),
    }
}

fn parse_signal_lifecycle(value: &str) -> Result<TaskSignalLifecycle> {
    match value {
        "active" => Ok(TaskSignalLifecycle::Active),
        "superseded" => Ok(TaskSignalLifecycle::Superseded),
        _ => Err(invariant(format!(
            "unknown persisted Signal lifecycle: {value}"
        ))),
    }
}

fn parse_id<T>(value: &str, field: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| invariant(format!("persisted {field} is invalid: {error}")))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn conflict(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Conflict, message)
}

fn stale(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::StaleState, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn json_error(context: &'static str) -> impl FnOnce(serde_json::Error) -> Error {
    move |error| Error::new(ErrorKind::InvariantViolation, format!("{context}: {error}"))
}
