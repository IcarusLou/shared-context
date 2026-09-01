//! Local, disposable runtime state for external Agent sessions and explicit Tasks.
//!
//! This crate owns only `state/runtime.sqlite`. It has no dependency on the
//! Context Git Store or rebuildable knowledge index. Task boundaries are
//! explicit: the runtime never guesses a new Task from Prompt or Workspace text.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sctx_domain::{
    AgentCheckpoint, AgentCheckpointId, Applicability, ArtifactRef, AutomaticContextCandidate,
    CandidateBuildId, CandidateConfirmationPlan, CandidateId, CandidateReviewStatus,
    CheckpointClaim, CheckpointClaimId, CheckpointEvidenceRef, CheckpointUnknown, ConfirmationId,
    ContextId, ContextKind, ContextRevisionRef, Error, ErrorKind, EventId, EvidenceSnapshotDraft,
    EvidenceType, ExternalSessionId, ExternalSessionLocator, ExternalSessionSnapshot,
    IntentRevisionRange, NonLocatingSignalRef, NormalizedWorkObservation, ProposedSpaceGroupKey,
    Result, RevisionId, SignalId, SpaceId, SubmissionId, TaskId, TaskIntentRevision,
    TaskIntentRevisionId, TaskSessionId, TaskSessionSnapshot, TaskSignal, TaskSignalKind,
    TaskSignalLifecycle, TaskSignalRecord, WorkEpisode, WorkEpisodeId, WorkEpisodeRef,
    WorkEpisodeStatus, WorkObservation, WorkObservationId, WorkSourceRef, WorkingIntentSnapshot,
};
use sha2::{Digest, Sha256};

pub mod reference_derivation;

pub use reference_derivation::{
    CheckoutReferenceResolver, ClaimReferenceDerivation, DerivedClaimReferences, PathCandidate,
    ResolvedReference, claim_topic_key, derive_claim_references, unresolvable,
};

const SCHEMA_VERSION: i64 = 14;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const HOOK_BUSY_TIMEOUT: Duration = Duration::from_millis(25);
const MAX_EPISODE_LIST_LIMIT: usize = 256;
/// Every this-many-th `hook_event` insert also prunes rows older than the retention window, so
/// the Hook hot path never pays for cleanup on every call.
const HOOK_EVENT_PRUNE_INTERVAL: i64 = 64;
/// Row count kept behind the newest `hook_event` id when pruning runs.
const HOOK_EVENT_RETENTION_ROWS: i64 = 5_000;
/// Upper bound on one `hook_event.detail` value, enforced before it reaches storage.
pub const MAX_HOOK_EVENT_DETAIL_CHARS: usize = 256;
/// Busy window for the `hook_event` diagnostic write only, deliberately far shorter than
/// [`HOOK_BUSY_TIMEOUT`]. Many concurrent Hook processes may all try to write a completion row
/// at once; retrying each one against a single-writer `SQLite` file for tens of milliseconds
/// would serialize them and could itself blow the Hook p99 budget. Losing an occasional
/// diagnostic row under contention is an acceptable trade for keeping the Hook path fast.
const HOOK_EVENT_BUSY_TIMEOUT: Duration = Duration::from_millis(3);
/// Maximum Context identities bound into one usage-totals query.
const USAGE_TOTALS_QUERY_CHUNK: usize = 256;
pub const DEFAULT_CANDIDATE_REVIEW_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub const MAX_CANDIDATE_REVIEW_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);
pub const MAX_CANDIDATE_REVIEW_LIST_LIMIT: usize = 100;
/// Bound on the `ExternalSession` Candidate corpus one Candidate Build may compare against.
pub const MAX_SESSION_CANDIDATE_REVIEW_SCAN: usize = 200;

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

/// Result of one automatic Hook-path Signal merge.
///
/// Counts only, deliberately: reading a full [`TaskSessionSnapshot`] means reading every Signal
/// and the current Intent revision, and doing that inside the write transaction would hold the
/// Runtime write lock for work no Hook caller uses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HookSignalMergeOutcome {
    pub inserted: usize,
    /// How many Signals this merge superseded to stay inside a [`SignalRetentionRule`] bound.
    pub retired: usize,
}

/// A bound on how many Active Signals of one group of kinds a Task may keep.
///
/// Retention is a caller-supplied policy, not a storage invariant: only the automatic
/// Hook path, which appends Signals no human reviewed, asks for it. Explicit MCP merges
/// keep every Signal they insert. Trimming supersedes the oldest Signals by
/// `signal_ordinal` and never deletes a row, so Signal history stays complete and stable
/// `SignalId`s remain resolvable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignalRetentionRule {
    /// The kinds whose Active rows share one budget.
    pub kinds: Vec<TaskSignalKind>,
    /// Maximum Active Signals retained across `kinds`.
    pub max_active: usize,
}

/// Result of explicitly creating and activating a new Task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartNewTaskOutcome {
    pub external_session_id: ExternalSessionId,
    pub previous_task_id: TaskId,
    pub snapshot: TaskSessionSnapshot,
}

/// Exact semantic disposition of one CAS-guarded Working Intent continue.
///
/// `Forked` is only reachable through [`TaskRuntime::continue_working_intent`]: a superseded CAS
/// parent whose replacement Head states a different normalized goal is treated as a concurrent
/// Agent inside one `ExternalSession` rather than a stale caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentRevisionWriteStatus {
    Created,
    AlreadyCurrent,
    Forked,
}

/// Current or newly-created Working Intent revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendIntentRevisionOutcome {
    pub revision: TaskIntentRevision,
    pub status: IntentRevisionWriteStatus,
}

/// Result of continuing one Working Intent lineage inside an `ExternalSession`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinueWorkingIntentOutcome {
    pub snapshot: TaskSessionSnapshot,
    pub revision: TaskIntentRevision,
    pub status: IntentRevisionWriteStatus,
    /// True when this continue re-selected a different retained Task as the `ActiveTask`.
    pub active_task_switched: bool,
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
    pub evidence_refs: Vec<CheckpointEvidenceRef>,
    pub inline_validations: Vec<EvidenceSnapshotDraft>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub relations: Vec<sctx_domain::ContextRelation>,
    pub engineering_references: Vec<sctx_domain::EngineeringReferenceDraft>,
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
    pub unknowns: Vec<CheckpointUnknown>,
}

/// Idempotent result of one atomic Checkpoint and Episode transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCheckpointOutcome {
    pub checkpoint: AgentCheckpoint,
    pub episode: WorkEpisodeView,
    pub created: bool,
    pub inline_observation_ids: Vec<WorkObservationId>,
}

/// One bounded, self-contained Evidence input authored directly by the Agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectEvidenceDraft {
    pub evidence_type: EvidenceType,
    pub summary: String,
    pub limitations: Vec<String>,
}

/// One model-facing Claim after transport identity fields have been removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectCheckpointClaimDraft {
    pub context_kind: sctx_domain::ContextKind,
    pub statement: String,
    pub rationale: String,
    pub conditions: Vec<String>,
    pub evidence: Vec<DirectEvidenceDraft>,
}

/// One content-addressed Checkpoint submission. Runtime resolves all lifecycle identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCheckpointSubmission {
    pub locator: ExternalSessionLocator,
    pub claims: Vec<DirectCheckpointClaimDraft>,
    pub unknowns: Vec<CheckpointUnknown>,
}

/// Durable receipt for one content-addressed Checkpoint operation and Build outbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointOperationOutcome {
    pub operation_id: String,
    pub checkpoint: AgentCheckpoint,
    pub episode: WorkEpisodeView,
    pub build: CandidateBuildView,
    pub replayed: bool,
    pub inline_observation_ids: Vec<WorkObservationId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CheckpointOperationRecord {
    operation_id: String,
    semantic_json: String,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    intent_revision_id: TaskIntentRevisionId,
    checkpoint_id: AgentCheckpointId,
    episode_id: WorkEpisodeId,
    build_id: CandidateBuildId,
}

/// Current rebuildable Candidate analysis stored outside Git knowledge facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAnalysisView {
    pub candidate: AutomaticContextCandidate,
    pub analysis_generation: u64,
}

/// Lifecycle of one Task-Intent-scoped proposed Space reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposedSpaceGroupMappingStatus {
    Reserved,
    Committed,
}

/// Runtime mapping from one exact Task Intent revision to its first confirmed new Space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProposedSpaceGroupMapping {
    pub proposed_space_group_key: ProposedSpaceGroupKey,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub candidate_id: CandidateId,
    pub space_id: SpaceId,
    pub status: ProposedSpaceGroupMappingStatus,
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

/// One Git-committed Candidate Confirmation to record in the Runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateConfirmationFinalize {
    pub candidate_id: CandidateId,
    pub operation_hash: String,
    pub confirmation_id: ConfirmationId,
    pub result_context_id: ContextId,
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
    Queued,
    Prepared,
    NeedsEvidence,
    Created,
    AlreadyExists,
    Failed,
}

impl CandidateBuildItemStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
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

/// Builder decision that one Claim restates an existing Task-owned Candidate.
///
/// A deduplicated Claim never reserves a `SubmissionId` and never reaches Git, so equal content
/// under different `SubmissionIds` still cannot converge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateBuildDuplicatePreparation {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub duplicate_of_candidate_id: CandidateId,
    pub similarity_basis_points: u16,
}

/// One durable Claim-scoped deduplication decision of a Candidate Build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateBuildDuplicateView {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub duplicate_of_candidate_id: CandidateId,
    pub similarity_basis_points: u16,
}

/// Durable deterministic build state for one exact closed Episode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBuildView {
    pub build_id: CandidateBuildId,
    pub source_episode: WorkEpisodeRef,
    pub final_checkpoint_id: AgentCheckpointId,
    pub status: CandidateBuildStatus,
    pub items: Vec<CandidateBuildItemView>,
    pub duplicates: Vec<CandidateBuildDuplicateView>,
}

/// Non-sensitive aggregate state of one `ActiveTask`'s durable Build recovery queue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CandidateBuildRecoveryStatus {
    pub pending: usize,
    pub incomplete: usize,
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

/// Retrieval entry point that returned one injected Context item to an Agent.
///
/// The three public read entry points are recorded separately so a later usage signal can be read
/// back to the exact retrieval surface that produced it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ContextInjectionSource {
    IntentUpdate,
    TaskContext,
    ArtifactFocus,
}

impl ContextInjectionSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntentUpdate => "intent_update",
            Self::TaskContext => "task_context",
            Self::ArtifactFocus => "artifact_focus",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "intent_update" => Ok(Self::IntentUpdate),
            "task_context" => Ok(Self::TaskContext),
            "artifact_focus" => Ok(Self::ArtifactFocus),
            other => Err(invariant(format!("unknown injection source {other}"))),
        }
    }
}

/// One immutable Context revision handed to an Agent by a retrieval entry point.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InjectedContext {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
}

/// One recorded injection of a Context into a Task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskInjectionRecord {
    pub task_id: TaskId,
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub injected_at_unix_seconds: u64,
    pub source: ContextInjectionSource,
}

/// What one Task did with a Context that was injected into it.
///
/// `Refuted` is the highest priority: a Context an Agent contradicted must never be downgraded
/// back to `Reused` or `Ignored` by a later write for the same Task.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextUsageOutcome {
    Ignored,
    Reused,
    Refuted,
}

impl ContextUsageOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::Reused => "reused",
            Self::Refuted => "refuted",
        }
    }

    /// Higher wins when two writes disagree about the same `(context, task)` pair.
    const fn priority(self) -> u8 {
        match self {
            Self::Ignored | Self::Reused => 0,
            Self::Refuted => 1,
        }
    }
}

/// One `(context, task)` usage decision derived by the server from a Checkpoint or Confirmation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextUsageRecord {
    pub context_id: ContextId,
    pub task_id: TaskId,
    pub outcome: ContextUsageOutcome,
}

/// Per-Context usage totals across every Task that ever had it injected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextUsageTotals {
    pub reused: u32,
    pub ignored: u32,
    pub refuted: u32,
}

impl ContextUsageTotals {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.reused == 0 && self.ignored == 0 && self.refuted == 0
    }
}

/// Disposition one `hook_event` row records for a single Hook-path decision point.
///
/// `Enabled`/`Disabled` mirror the resolved activation for that decision point; `Neutral`
/// records a degraded-but-still-successful outcome (for example a dropped attribution); and
/// `FailOpen` records a fault that forced the safe no-op path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookEventDecision {
    Enabled,
    Disabled,
    Neutral,
    FailOpen,
}

impl HookEventDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Neutral => "neutral",
            Self::FailOpen => "fail_open",
        }
    }
}

impl std::fmt::Display for HookEventDecision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One diagnostic row bound for the `hook_event` table.
///
/// `detail` is safe-by-construction free text — never prompt or tool-output content, though an
/// absolute local path is acceptable — and is truncated to
/// [`MAX_HOOK_EVENT_DETAIL_CHARS`] characters if longer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookEventRecord {
    pub recorded_at_unix_ms: u64,
    pub agent_kind: String,
    pub external_session_id: Option<String>,
    pub event_kind: String,
    pub decision: HookEventDecision,
    pub reason: String,
    pub duration_ms: u64,
    pub detail: Option<String>,
}

impl HookEventRecord {
    fn validate(&self) -> Result<()> {
        if self.agent_kind.trim().is_empty() {
            return Err(invalid("hook_event.agent_kind must not be empty"));
        }
        if self.event_kind.trim().is_empty() {
            return Err(invalid("hook_event.event_kind must not be empty"));
        }
        if self.reason.trim().is_empty() {
            return Err(invalid("hook_event.reason must not be empty"));
        }
        if self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.chars().count() > MAX_HOOK_EVENT_DETAIL_CHARS)
        {
            return Err(invalid(format!(
                "hook_event.detail must be at most {MAX_HOOK_EVENT_DETAIL_CHARS} characters"
            )));
        }
        Ok(())
    }
}

/// One `(decision, reason)` count over a recorded-since window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookEventCount {
    pub decision: String,
    pub reason: String,
    pub count: i64,
}

/// One `hook_event` row as read back for `sctx doctor --hooks`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookEventView {
    pub recorded_at_unix_ms: u64,
    pub agent_kind: String,
    pub external_session_id: Option<String>,
    pub event_kind: String,
    pub decision: String,
    pub reason: String,
    pub duration_ms: u64,
    pub detail: Option<String>,
}

/// Owner of the installation-local `state/runtime.sqlite` database.
#[derive(Clone, Debug)]
pub struct TaskRuntime {
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
    busy_timeout: Duration,
    configure_schema_on_open: bool,
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
        Self::initialize_with_busy_timeout(root.into(), BUSY_TIMEOUT, true)
    }

    /// Initializes Runtime state with the short fail-open timeout used only by Agent Hooks.
    ///
    /// # Errors
    ///
    /// Returns typed filesystem, `SQLite`, or schema-version errors without waiting through the
    /// normal interactive-operation busy window.
    pub fn initialize_for_hook(root: impl Into<PathBuf>) -> Result<Self> {
        Self::initialize_with_busy_timeout(root.into(), HOOK_BUSY_TIMEOUT, false)
    }

    fn initialize_with_busy_timeout(
        root: PathBuf,
        busy_timeout: Duration,
        configure_existing_schema: bool,
    ) -> Result<Self> {
        let state = root.join("state");
        fs::create_dir_all(&state).map_err(io_error("create task runtime state directory"))?;
        let database = state.join("runtime.sqlite");
        let configure_schema_on_open = configure_existing_schema || !database.is_file();
        let mut runtime = Self {
            root,
            state,
            database,
            busy_timeout,
            configure_schema_on_open,
        };
        let _connection = runtime.open_connection()?;
        runtime.configure_schema_on_open = false;
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
    pub fn open_or_create(
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

    /// Explicitly creates and activates a new runtime-owned Task.
    ///
    /// # Errors
    ///
    /// Returns an input error for a missing Session, stale `ActiveTask` CAS guard,
    /// or invalid Task content and Signals.
    pub fn start_new_task(
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
    pub fn append_intent_revision(
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
        let revision = append_revision_in_transaction(
            &transaction,
            task_session_id,
            &current,
            working_intent,
        )?;
        transaction
            .commit()
            .map_err(sql_error("commit Intent append transaction"))?;
        Ok(AppendIntentRevisionOutcome {
            revision,
            status: IntentRevisionWriteStatus::Created,
        })
    }

    /// Continues one Working Intent lineage owned by this `ExternalSession`.
    ///
    /// Unlike [`Self::append_intent_revision`], the CAS parent selects the lineage instead of the
    /// current `ActiveTask`: a parent that is still the Head of any retained Task appends there and
    /// re-selects that Task, and a superseded parent whose replacement Head states a different
    /// normalized goal forks a parallel `TaskSession`. Concurrent Agents that an Agent host
    /// cannot distinguish keep separate Intent chains instead of overwriting one Head.
    ///
    /// # Errors
    ///
    /// Returns an input error for a missing Session or cross-Session parent, and a stale-state
    /// error when the parent is unknown or superseded by the same normalized goal.
    pub fn continue_working_intent(
        &self,
        locator: &ExternalSessionLocator,
        parent_revision_id: TaskIntentRevisionId,
        working_intent: WorkingIntentSnapshot,
    ) -> Result<ContinueWorkingIntentOutcome> {
        locator.validate()?;
        working_intent.validate()?;
        let semantic_hash = working_intent.canonical_semantic_hash()?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Working Intent continue transaction")?;
        let external = read_external_identity(&transaction, locator)?
            .ok_or_else(|| invalid("ExternalSessionLocator does not identify a runtime Session"))?;
        let owner = read_intent_revision_owner(&transaction, parent_revision_id)?
            .ok_or_else(|| stale("Intent parent is stale: unknown or no longer retained"))?;
        if owner.external_session != external.external_session_id {
            return Err(invalid("Intent parent belongs to another Task Session"));
        }
        let head = read_intent_revision(&transaction, owner.head_revision)?;
        let (task_session_id, task_id, revision, status) =
            if owner.head_revision == parent_revision_id {
                if head.semantic_hash == semantic_hash {
                    (
                        owner.task_session,
                        owner.task,
                        head,
                        IntentRevisionWriteStatus::AlreadyCurrent,
                    )
                } else {
                    let revision = append_revision_in_transaction(
                        &transaction,
                        owner.task_session,
                        &head,
                        working_intent,
                    )?;
                    (
                        owner.task_session,
                        owner.task,
                        revision,
                        IntentRevisionWriteStatus::Created,
                    )
                }
            } else if head.parent_revision_id == Some(parent_revision_id)
                && head.semantic_hash == semantic_hash
            {
                (
                    owner.task_session,
                    owner.task,
                    head,
                    IntentRevisionWriteStatus::AlreadyCurrent,
                )
            } else if normalized_goal(&working_intent.goal)
                == normalized_goal(&head.working_intent.goal)
            {
                return Err(stale(
                    "Intent parent is no longer the current Head of its Task lineage",
                ));
            } else {
                let task_id = TaskId::new();
                let forked = TaskSessionSnapshot::from_initial(
                    locator.clone(),
                    task_id,
                    working_intent,
                    Vec::new(),
                )?;
                let ordinal = next_task_ordinal(&transaction, external.external_session_id)?;
                insert_task(&transaction, external.external_session_id, ordinal, &forked)?;
                let revision = forked
                    .current_intent_revision()
                    .ok_or_else(|| invariant("forked TaskSession has no Working Intent Head"))?
                    .clone();
                (
                    forked.task_session_id,
                    task_id,
                    revision,
                    IntentRevisionWriteStatus::Forked,
                )
            };
        let active_task_switched = external.active_task_id != task_id;
        compare_and_switch(
            &transaction,
            external.external_session_id,
            external.active_task_id,
            task_session_id,
            task_id,
        )?;
        let snapshot = require_snapshot(&transaction, task_session_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Working Intent continue transaction"))?;
        Ok(ContinueWorkingIntentOutcome {
            snapshot,
            revision,
            status,
            active_task_switched,
        })
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
        let Some((task_session_id, task_id)) = locate_active_task(&transaction, locator)? else {
            transaction
                .commit()
                .map_err(sql_error("commit missing locator transaction"))?;
            return Ok(None);
        };
        let outcome =
            merge_signals_in_transaction(&transaction, task_session_id, task_id, &signals)?;
        transaction
            .commit()
            .map_err(sql_error("commit locator Signal merge transaction"))?;
        Ok(Some(outcome))
    }

    /// Merges Signals into one `ExternalSession`'s `ActiveTask` and trims the result to
    /// the supplied per-kind-group Active budgets, in one write transaction.
    ///
    /// This is the automatic Hook path's entry point: it appends Signals nobody reviewed,
    /// so an unbounded Task would accumulate them for the whole life of a Session. Trimming
    /// supersedes the oldest Active Signals of the affected kinds instead of deleting them,
    /// which is exactly what an explicit `task_signal_supersede` does. A missing locator
    /// returns `None` and never creates a Task.
    ///
    /// # Errors
    ///
    /// Returns typed input or storage errors.
    pub fn merge_hook_signals_by_locator(
        &self,
        locator: &ExternalSessionLocator,
        signals: Vec<TaskSignal>,
        retention: &[SignalRetentionRule],
    ) -> Result<Option<HookSignalMergeOutcome>> {
        locator.validate()?;
        let signals = normalize_signals(signals)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Hook Signal merge transaction")?;
        let Some((task_session_id, task_id)) = locate_active_task(&transaction, locator)? else {
            transaction
                .commit()
                .map_err(sql_error("commit missing locator transaction"))?;
            return Ok(None);
        };
        let inserted =
            insert_signals_in_transaction(&transaction, task_session_id, task_id, &signals)?;
        let retired = enforce_signal_retention(&transaction, task_session_id, retention)?;
        transaction
            .commit()
            .map_err(sql_error("commit Hook Signal merge transaction"))?;
        Ok(Some(HookSignalMergeOutcome {
            inserted: inserted.len(),
            retired: retired.len(),
        }))
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

    /// Appends a normalized Work Observation under Episode-version CAS.
    ///
    /// # Errors
    ///
    /// Rejects stale/closed ownership, invalid meaning, or storage failures.
    pub fn append_work_observation(
        &self,
        episode_id: WorkEpisodeId,
        expected_version: u64,
        intent_revision_id: TaskIntentRevisionId,
        source_refs: Vec<WorkSourceRef>,
        observation: NormalizedWorkObservation,
    ) -> Result<AppendWorkObservationOutcome> {
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

    /// Atomically opens or reuses the `ActiveTask` Episode, advances its typed
    /// references, records inline Validation observations and persists one
    /// server-identified Agent Checkpoint.
    ///
    /// Semantic retries are keyed by Episode and parent version. Identical
    /// content returns the original Checkpoint. Different content conflicts
    /// within an open Episode, while version zero starts a new Episode after
    /// the latest matching closed-Episode parent.
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
                if persisted_semantics == semantic_json {
                    let episode = require_episode_view(&transaction, episode_id)?;
                    let inline_observation_ids =
                        inline_observation_ids(&checkpoint, &input.claims)?;
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
                if open_episode.is_some() {
                    return Err(conflict(
                        "Agent Checkpoint parent version already contains different content",
                    ));
                }
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
                evidence_refs.push(CheckpointEvidenceRef::Observation {
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
                claim.relations.clone(),
                claim.engineering_references.clone(),
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

    /// Atomically persists or replays one content-addressed, final Checkpoint operation.
    ///
    /// Runtime resolves the exact current `ActiveTask`, Intent and Episode, closes the Episode,
    /// and reserves its Candidate Build outbox in the same transaction. The caller supplies no
    /// lifecycle CAS or idempotency key.
    ///
    /// # Errors
    ///
    /// Rejects a missing `ActiveTask`, invalid direct Evidence, task-local reference drift,
    /// operation hash collision, or storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn submit_agent_checkpoint(
        &self,
        input: &AgentCheckpointSubmission,
    ) -> Result<CheckpointOperationOutcome> {
        input.locator.validate()?;
        if input.claims.is_empty() && input.unknowns.is_empty() {
            return Err(invalid(
                "checkpoint submission must contain at least one Claim or Unknown",
            ));
        }
        let semantic_json = direct_checkpoint_semantic_json(input)?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Checkpoint operation transaction")?;
        let external = read_external_identity(&transaction, &input.locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask for Agent Checkpoint"))?;
        let (task_id, intent_revision_id) =
            read_active_task_head(&transaction, external.active_task_session_id)?
                .ok_or_else(|| invariant("located ActiveTask is not active"))?;
        if task_id != external.active_task_id {
            return Err(invariant(
                "ExternalSession ActiveTask identity disagrees with its TaskSession",
            ));
        }
        let intent = read_intent_revision(&transaction, intent_revision_id)?;
        let claims = materialize_direct_checkpoint_claims(&input.claims, &intent.working_intent)?;
        let (operation_key, operation_id) = checkpoint_operation_identity(
            external.active_task_session_id,
            task_id,
            intent_revision_id,
            &semantic_json,
        );

        if let Some(operation) = read_checkpoint_operation(&transaction, &operation_key)? {
            if operation.operation_id != operation_id
                || operation.semantic_json != semantic_json
                || operation.task_session_id != external.active_task_session_id
                || operation.task_id != task_id
                || operation.intent_revision_id != intent_revision_id
            {
                return Err(invariant(
                    "Checkpoint operation key resolved to conflicting persisted content",
                ));
            }
            let episode = require_episode_view(&transaction, operation.episode_id)?;
            let checkpoint = episode
                .checkpoints
                .iter()
                .find(|checkpoint| checkpoint.checkpoint_id == operation.checkpoint_id)
                .cloned()
                .ok_or_else(|| invariant("Checkpoint operation receipt lost its Checkpoint"))?;
            let build = require_candidate_build_view(&transaction, operation.build_id)?;
            let inline_observation_ids = inline_observation_ids(&checkpoint, &claims)?;
            transaction
                .commit()
                .map_err(sql_error("commit replayed Checkpoint operation"))?;
            return Ok(CheckpointOperationOutcome {
                operation_id,
                checkpoint,
                episode,
                build,
                replayed: true,
                inline_observation_ids,
            });
        }

        let episode_id = if let Some(episode_id) =
            find_open_episode(&transaction, external.active_task_session_id)?
        {
            episode_id
        } else {
            insert_open_episode(&transaction, external.active_task_session_id, task_id)?
        };
        let (task_session_id, episode_task_id, episode_version, status) =
            require_episode_head(&transaction, episode_id)?;
        require_open_episode_version(episode_version, &status, episode_version)?;
        if task_session_id != external.active_task_session_id || episode_task_id != task_id {
            return Err(invariant("ActiveTask Work Episode ownership changed"));
        }
        insert_all_missing_episode_refs(&transaction, episode_id, task_session_id, task_id)?;
        let episode_before = require_episode_view(&transaction, episode_id)?.episode;
        validate_checkpoint_task_local_refs(&episode_before, &claims)?;

        let mut persisted_claims = Vec::with_capacity(claims.len());
        let mut inline_observation_ids = Vec::new();
        for claim in &claims {
            let mut evidence_refs = claim.evidence_refs.clone();
            for evidence in &claim.inline_validations {
                evidence.validate("checkpoint_submission.evidence")?;
                let observation = WorkObservation::from_parts(
                    task_session_id,
                    task_id,
                    intent_revision_id,
                    Vec::new(),
                    NormalizedWorkObservation::InlineValidation {
                        evidence: evidence.clone(),
                    },
                )?;
                insert_observation_rows(&transaction, episode_id, &observation)?;
                evidence_refs.push(CheckpointEvidenceRef::Observation {
                    observation_id: observation.observation_id,
                });
                inline_observation_ids.push(observation.observation_id);
            }
            persisted_claims.push(CheckpointClaim::from_parts(
                claim.context_kind_hint,
                claim.topic_key_hint.clone(),
                claim.statement.clone(),
                claim.rationale.clone(),
                claim.applicability.clone(),
                claim.assumptions.clone(),
                claim.recheck_when.clone(),
                evidence_refs,
                claim.artifact_refs.clone(),
                claim.relations.clone(),
                claim.engineering_references.clone(),
                claim.related_contexts.clone(),
            )?);
        }
        let episode_with_inline = require_episode_view(&transaction, episode_id)?.episode;
        let checkpoint = AgentCheckpoint::from_parts(
            &episode_with_inline,
            intent_revision_id,
            persisted_claims,
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
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'close', ?7, ?8, ?9)",
                params![
                    checkpoint.checkpoint_id.to_string(),
                    episode_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                    intent_revision_id.to_string(),
                    i64::try_from(episode_version)
                        .map_err(|_| invalid("Work Episode version exceeds SQLite range"))?,
                    semantic_json,
                    checkpoint_json,
                    checkpoint_ordinal,
                ],
            )
            .map_err(sql_error("insert content-addressed Agent Checkpoint"))?;
        let mut validation_episode = episode_with_inline;
        validation_episode.close(&checkpoint)?;
        close_episode_version(
            &transaction,
            episode_id,
            episode_version,
            checkpoint.checkpoint_id,
        )?;

        let build_id = CandidateBuildId::new();
        transaction
            .execute(
                "INSERT INTO candidate_build (
                    build_id, episode_id, task_session_id, task_id,
                    final_checkpoint_id, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
                params![
                    build_id.to_string(),
                    episode_id.to_string(),
                    task_session_id.to_string(),
                    task_id.to_string(),
                    checkpoint.checkpoint_id.to_string(),
                ],
            )
            .map_err(sql_error("reserve Candidate Build outbox"))?;
        let closed_episode = require_episode_view(&transaction, episode_id)?;
        for (item_ordinal, (checkpoint_id, claim_id)) in closed_episode
            .checkpoints
            .iter()
            .flat_map(|checkpoint| {
                checkpoint
                    .claims
                    .iter()
                    .map(move |claim| (checkpoint.checkpoint_id, claim.claim_id))
            })
            .enumerate()
        {
            transaction
                .execute(
                    "INSERT INTO candidate_build_item (
                        build_id, item_ordinal, checkpoint_id, claim_id, submission_id,
                        content_hash, status, candidate_id, event_id, error_code
                     ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'queued', NULL, NULL, NULL)",
                    params![
                        build_id.to_string(),
                        i64::try_from(item_ordinal).map_err(|_| {
                            invalid("Candidate Build item ordinal exceeds SQLite range")
                        })?,
                        checkpoint_id.to_string(),
                        claim_id.to_string(),
                        SubmissionId::new().to_string(),
                    ],
                )
                .map_err(sql_error("reserve Candidate Build outbox item"))?;
        }
        transaction
            .execute(
                "INSERT INTO checkpoint_operation (
                    operation_key, operation_id, semantic_json, task_session_id, task_id,
                    intent_revision_id, checkpoint_id, episode_id, build_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    operation_key,
                    operation_id,
                    semantic_json,
                    task_session_id.to_string(),
                    task_id.to_string(),
                    intent_revision_id.to_string(),
                    checkpoint.checkpoint_id.to_string(),
                    episode_id.to_string(),
                    build_id.to_string(),
                ],
            )
            .map_err(sql_error("persist Checkpoint operation receipt"))?;
        let episode = closed_episode;
        let build = require_candidate_build_view(&transaction, build_id)?;
        transaction
            .commit()
            .map_err(sql_error("commit Checkpoint operation"))?;
        Ok(CheckpointOperationOutcome {
            operation_id,
            checkpoint,
            episode,
            build,
            replayed: false,
            inline_observation_ids,
        })
    }

    /// Path spellings one Episode's Claims still need placed, or `None` once they were placed.
    ///
    /// Candidate Build asks this first so a rebuilt Episode never spawns `git` again: the answer
    /// is `None` as soon as [`TaskRuntime::derive_episode_claim_references`] recorded the first
    /// derivation, whatever that derivation resolved.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors.
    pub fn pending_claim_reference_candidates(
        &self,
        episode_id: WorkEpisodeId,
    ) -> Result<Option<Vec<PathCandidate>>> {
        let connection = self.open_connection()?;
        if claim_references_derived(&connection, episode_id)? {
            return Ok(None);
        }
        let Some(episode) = read_episode_view(&connection, episode_id)? else {
            return Ok(None);
        };
        Ok(Some(episode_path_candidates(&episode)))
    }

    /// Derives every Claim's engineering coordinates for one Episode exactly once.
    ///
    /// This is Candidate Build work, never Checkpoint ACK work. The first call resolves each
    /// path-shaped spelling through `resolve`, writes the resulting References and topic hint back
    /// onto the persisted Claims, and records that this Episode is derived. Every later call
    /// reports that persisted answer and calls `resolve` for nothing, so a Build rerun after the
    /// checkout moved on cannot change a Candidate that already reached review.
    ///
    /// # Errors
    ///
    /// Returns typed storage or invariant errors. A resolver that places nothing is not an error:
    /// the spellings stay retrieval hints.
    pub fn derive_episode_claim_references(
        &self,
        episode_id: WorkEpisodeId,
        resolve: &dyn Fn(&PathCandidate) -> Option<ResolvedReference>,
    ) -> Result<Vec<DerivedClaimReferences>> {
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Claim derivation transaction")?;
        let episode = require_episode_view(&transaction, episode_id)?;
        let already_derived = claim_references_derived(&transaction, episode_id)?;
        let mut derivations = Vec::new();
        for checkpoint in &episode.checkpoints {
            let mut updated = checkpoint.clone();
            for claim in &mut updated.claims {
                let summaries = claim_evidence_summaries(&episode.episode, claim);
                let borrowed = summaries.iter().map(String::as_str).collect::<Vec<_>>();
                let derivation = if already_derived {
                    ClaimReferenceDerivation {
                        engineering_references: claim.engineering_references.clone(),
                        topic_key_hint: claim.topic_key_hint.clone(),
                        unresolved_hints: reference_derivation::claim_hints(
                            &claim.statement,
                            &claim.rationale,
                            &borrowed,
                            &claim.engineering_references,
                        ),
                    }
                } else {
                    let derived = derive_claim_references(
                        claim.context_kind_hint.unwrap_or(ContextKind::Discovery),
                        &claim.statement,
                        &claim.rationale,
                        &borrowed,
                        resolve,
                    );
                    claim
                        .engineering_references
                        .clone_from(&derived.engineering_references);
                    // A Claim that carried its own `topic_key_hint` keeps it: derivation fills a
                    // gap the Agent left, it does not overrule a coordinate the Agent named.
                    if claim.topic_key_hint.is_none() {
                        claim.topic_key_hint.clone_from(&derived.topic_key_hint);
                    }
                    derived
                };
                derivations.push(DerivedClaimReferences {
                    checkpoint_id: updated.checkpoint_id,
                    claim_id: claim.claim_id,
                    engineering_references: derivation.engineering_references,
                    // Always the persisted answer, so the first derivation and every later replay
                    // of it report the same topic hint.
                    topic_key_hint: claim.topic_key_hint.clone(),
                    unresolved_hints: derivation.unresolved_hints,
                    applicability_inherited: true,
                });
            }
            if already_derived {
                continue;
            }
            updated.validate_against_episode(&episode.episode)?;
            let checkpoint_json = serde_json::to_string(&updated)
                .map_err(json_error("serialize derived Agent Checkpoint"))?;
            transaction
                .execute(
                    "UPDATE agent_checkpoint SET checkpoint_json = ?1 WHERE checkpoint_id = ?2",
                    params![checkpoint_json, updated.checkpoint_id.to_string()],
                )
                .map_err(sql_error("persist derived Claim references"))?;
        }
        if !already_derived {
            transaction
                .execute(
                    "INSERT INTO checkpoint_reference_derivation (episode_id) VALUES (?1)",
                    params![episode_id.to_string()],
                )
                .map_err(sql_error("record Claim reference derivation"))?;
        }
        transaction
            .commit()
            .map_err(sql_error("commit Claim derivation transaction"))?;
        Ok(derivations)
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
        self.prepare_candidate_build_with_duplicates(episode_id, items, &[])
    }

    /// Prepares one Candidate Build whose Claims are split into submittable drafts and durable
    /// deduplication decisions.
    ///
    /// Every persisted Claim must appear exactly once across `items` and `duplicates`. A Claim that
    /// a previous preparation already classified keeps that first durable decision, so recovering
    /// or replaying the same closed Episode never produces a second Candidate for one fact.
    ///
    /// # Errors
    ///
    /// Rejects an open Episode, incomplete readiness metadata, a Claim classified twice and a
    /// duplicate target that is not an already-created Candidate of another Claim.
    pub fn prepare_candidate_build_with_duplicates(
        &self,
        episode_id: WorkEpisodeId,
        items: &[CandidateBuildItemPreparation],
        duplicates: &[CandidateBuildDuplicatePreparation],
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
        validate_build_claim_coverage(&episode, items, duplicates)?;
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
        // A Claim that already reached Git keeps its Candidate, and a Claim already recorded as a
        // duplicate keeps that decision, so recovery and replay cannot fork one fact into two.
        let submitted_claims = read_candidate_build_items(&transaction, build_id)?
            .into_iter()
            .filter(|item| item.status.is_finalized())
            .map(|item| item.claim_id)
            .collect::<BTreeSet<_>>();
        let persisted_duplicates = read_candidate_build_duplicates(&transaction, build_id)?
            .into_iter()
            .map(|duplicate| duplicate.claim_id)
            .collect::<BTreeSet<_>>();
        let items = items
            .iter()
            .filter(|item| !persisted_duplicates.contains(&item.claim_id))
            .cloned()
            .collect::<Vec<_>>();
        let duplicates = duplicates
            .iter()
            .filter(|duplicate| !submitted_claims.contains(&duplicate.claim_id))
            .copied()
            .collect::<Vec<_>>();
        for duplicate in &duplicates {
            transaction
                .execute(
                    "DELETE FROM candidate_build_item
                     WHERE build_id = ?1 AND claim_id = ?2 AND candidate_id IS NULL",
                    params![build_id.to_string(), duplicate.claim_id.to_string()],
                )
                .map_err(sql_error(
                    "release deduplicated Candidate Build outbox item",
                ))?;
        }
        upsert_candidate_build_items(&transaction, build_id, &items)?;
        insert_candidate_build_duplicates(&transaction, build_id, &duplicates)?;
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

    /// Lists a bounded set of pending or incomplete Build outboxes for the exact `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Rejects a missing Session, an invalid bound, or storage failure.
    pub fn list_recoverable_candidate_build_episodes(
        &self,
        locator: &ExternalSessionLocator,
        limit: usize,
    ) -> Result<Vec<WorkEpisodeId>> {
        locator.validate()?;
        if limit == 0 || limit > 64 {
            return Err(invalid(
                "Candidate Build recovery limit must be between 1 and 64",
            ));
        }
        let connection = self.open_connection()?;
        let external = read_external_identity(&connection, locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask for Candidate recovery"))?;
        let mut statement = connection
            .prepare(
                "SELECT build.episode_id
                 FROM candidate_build AS build
                 JOIN work_episode AS episode ON episode.episode_id = build.episode_id
                 WHERE build.task_session_id = ?1 AND build.task_id = ?2
                   AND (
                       build.status IN ('pending', 'incomplete') OR EXISTS (
                           SELECT 1
                           FROM candidate_build_item AS item
                           LEFT JOIN candidate_analysis AS analysis
                             ON analysis.candidate_id = item.candidate_id
                           WHERE item.build_id = build.build_id
                             AND item.status IN ('created', 'already_exists')
                             AND (analysis.candidate_id IS NULL
                                  OR analysis.analysis_status != 'complete')
                       )
                   )
                 ORDER BY build.recovery_attempt_generation ASC,
                          CASE build.status WHEN 'pending' THEN 0 ELSE 1 END,
                          episode.episode_ordinal ASC LIMIT ?3",
            )
            .map_err(sql_error("prepare recoverable Candidate Build list"))?;
        let limit = i64::try_from(limit)
            .map_err(|_| invalid("Candidate Build recovery limit overflows SQLite"))?;
        statement
            .query_map(
                params![
                    external.active_task_session_id.to_string(),
                    external.active_task_id.to_string(),
                    limit,
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(sql_error("query recoverable Candidate Builds"))?
            .map(|row| {
                row.map_err(sql_error("read recoverable Candidate Build row"))
                    .and_then(|value| parse_id(&value, "candidate_build.episode_id"))
            })
            .collect()
    }

    /// Persists one safe recovery attempt outcome and advances fair queue ordering.
    ///
    /// # Errors
    ///
    /// Rejects an unknown Build, unsafe error code, or storage failure.
    pub fn record_candidate_build_recovery_attempt(
        &self,
        episode_id: WorkEpisodeId,
        error_code: Option<&str>,
    ) -> Result<()> {
        if error_code.is_some_and(|code| !valid_error_code(code)) {
            return Err(invalid("Candidate Build recovery error code is invalid"));
        }
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Build recovery attempt")?;
        let changed = transaction
            .execute(
                "UPDATE candidate_build
                 SET recovery_attempt_generation = recovery_attempt_generation + 1,
                     last_recovery_status = ?1, last_recovery_error_code = ?2
                 WHERE episode_id = ?3",
                params![
                    if error_code.is_some() {
                        "failed"
                    } else {
                        "succeeded"
                    },
                    error_code,
                    episode_id.to_string(),
                ],
            )
            .map_err(sql_error("record Candidate Build recovery attempt"))?;
        if changed != 1 {
            return Err(invalid("Candidate Build recovery Episode does not exist"));
        }
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Build recovery attempt"))
    }

    /// Returns non-sensitive pending/incomplete counts for one exact `ActiveTask`.
    ///
    /// # Errors
    ///
    /// Rejects a missing Session or storage failure.
    pub fn candidate_build_recovery_status(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<CandidateBuildRecoveryStatus> {
        locator.validate()?;
        let connection = self.open_connection()?;
        let external = read_external_identity(&connection, locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask for Candidate recovery"))?;
        let (pending, incomplete) = connection
            .query_row(
                "SELECT
                    SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'incomplete' THEN 1 ELSE 0 END)
                 FROM candidate_build WHERE task_session_id = ?1 AND task_id = ?2",
                params![
                    external.active_task_session_id.to_string(),
                    external.active_task_id.to_string(),
                ],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(sql_error("read Candidate Build recovery status"))?;
        Ok(CandidateBuildRecoveryStatus {
            pending: usize::try_from(pending.unwrap_or(0))
                .map_err(|_| invariant("pending Candidate Build count is invalid"))?,
            incomplete: usize::try_from(incomplete.unwrap_or(0))
                .map_err(|_| invariant("incomplete Candidate Build count is invalid"))?,
        })
    }

    /// Checks exact `ActiveTask` ownership before target-aware Candidate recovery.
    ///
    /// # Errors
    ///
    /// Returns only locator/storage errors; missing or cross-owner Episodes are `false`.
    pub fn owns_candidate_recovery_episode(
        &self,
        locator: &ExternalSessionLocator,
        episode_id: WorkEpisodeId,
    ) -> Result<bool> {
        locator.validate()?;
        let connection = self.open_connection()?;
        let external = read_external_identity(&connection, locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask for Candidate recovery"))?;
        connection
            .query_row(
                "SELECT 1 FROM work_episode
                 WHERE episode_id = ?1 AND task_session_id = ?2 AND task_id = ?3",
                params![
                    episode_id.to_string(),
                    external.active_task_session_id.to_string(),
                    external.active_task_id.to_string(),
                ],
                |_row| Ok(()),
            )
            .optional()
            .map(|value| value.is_some())
            .map_err(sql_error("check Candidate recovery Episode ownership"))
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

    /// Reads the current reservation or committed mapping for one proposed Space group.
    ///
    /// # Errors
    ///
    /// Returns typed parse or storage failures; absence remains `None`.
    pub fn read_proposed_space_group(
        &self,
        proposed_space_group_key: ProposedSpaceGroupKey,
    ) -> Result<Option<ProposedSpaceGroupMapping>> {
        read_proposed_space_group_mapping(&self.open_connection()?, proposed_space_group_key)
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

    /// Lists Pending and Confirmed Candidate Reviews owned by every Task of one `ExternalSession`.
    ///
    /// Candidate Build uses it as the deduplication corpus: concurrent Agents that share one
    /// `external_session_id` may hold parallel Tasks, so Task-local scope alone would miss the
    /// restatements this exists to collapse.
    ///
    /// # Errors
    ///
    /// Rejects an unknown Task Session, an invalid bound, or storage failure.
    pub fn list_session_candidate_reviews(
        &self,
        task_session_id: TaskSessionId,
        limit: usize,
    ) -> Result<Vec<CandidateReviewRecord>> {
        if limit == 0 || limit > MAX_SESSION_CANDIDATE_REVIEW_SCAN {
            return Err(invalid(
                "Candidate Review session scan limit is outside the safe bound",
            ));
        }
        let connection = self.open_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT review.candidate_id, review.submission_id, review.episode_id,
                        review.task_session_id, review.task_id, review.build_id,
                        review.final_checkpoint_id, review.checkpoint_id, review.claim_id,
                        review.review_version, review.status, review.discard_reason,
                        review.created_at_unix_seconds, review.expires_at_unix_seconds,
                        review.discarded_at_unix_seconds, review.expired_at_unix_seconds,
                        review.confirmation_id, review.result_context_id
                 FROM candidate_review AS review
                 JOIN task_session AS task ON task.task_session_id = review.task_session_id
                 WHERE review.status IN ('pending', 'confirmed')
                   AND task.external_session_id = (
                       SELECT external_session_id FROM task_session WHERE task_session_id = ?1
                   )
                 ORDER BY review.created_at_unix_seconds ASC, review.candidate_id ASC
                 LIMIT ?2",
            )
            .map_err(sql_error("prepare ExternalSession Candidate Review scan"))?;
        let rows = statement
            .query_map(
                params![
                    task_session_id.to_string(),
                    i64::try_from(limit).map_err(|_| invalid(
                        "Candidate Review session scan limit exceeds SQLite range"
                    ))?,
                ],
                candidate_review_row,
            )
            .map_err(sql_error("query ExternalSession Candidate Reviews"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error("read ExternalSession Candidate Reviews"))?;
        rows.into_iter()
            .map(parse_candidate_review_record)
            .collect()
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
        self.discard_candidate_reviews(std::slice::from_ref(request))?
            .pop()
            .ok_or_else(|| invariant("Candidate Review discard produced no outcome"))
    }

    /// Applies several CAS-guarded discards inside one transaction.
    ///
    /// Every request must name the same `ExternalSession` and the same Task/Intent CAS. Any
    /// rejected member rolls the whole batch back and names the failing `CandidateId`, so a batch
    /// Review decision never leaves a partially discarded Task.
    ///
    /// # Errors
    ///
    /// Returns an input error for an empty or cross-Session batch, a stale-state error for a stale
    /// Task/Intent/Review CAS, and a conflict for an already decided Review.
    pub fn discard_candidate_reviews(
        &self,
        requests: &[CandidateReviewDiscard],
    ) -> Result<Vec<CandidateReviewDiscardOutcome>> {
        let Some(first) = requests.first() else {
            return Err(invalid("Candidate discard requires at least one Candidate"));
        };
        first.locator.validate()?;
        let mut seen = BTreeSet::new();
        for request in requests {
            if request.locator != first.locator
                || request.expected_task_id != first.expected_task_id
                || request.expected_intent_revision_id != first.expected_intent_revision_id
            {
                return Err(invalid(
                    "Candidate discard batch must share one ExternalSession and Task/Intent CAS",
                ));
            }
            if !seen.insert(request.candidate_id) {
                return Err(invalid(
                    "Candidate discard batch must not repeat a Candidate",
                ));
            }
            let reason = request.reason.trim();
            if reason.is_empty() || reason.len() > 512 || request.expected_review_version == 0 {
                return Err(invalid(
                    "Candidate discard requires a non-empty bounded reason and positive review version",
                ));
            }
        }
        self.cleanup_expired_candidate_reviews()?;
        let now = unix_seconds(SystemTime::now())?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Review discard")?;
        let task_session_id = find_active_task_by_locator(&transaction, &first.locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask"))?;
        let task = require_snapshot(&transaction, task_session_id)?;
        if task.task_id != first.expected_task_id
            || task
                .current_intent_revision()
                .is_none_or(|revision| revision.revision_id != first.expected_intent_revision_id)
        {
            return Err(Error::new(
                ErrorKind::StaleState,
                "Candidate discard Task/Intent ownership CAS is stale",
            ));
        }
        let mut outcomes = Vec::with_capacity(requests.len());
        for request in requests {
            outcomes.push(
                discard_one_candidate_review(&transaction, &task, request, now)
                    .map_err(candidate_scoped_error(request.candidate_id))?,
            );
        }
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Review discard"))?;
        Ok(outcomes)
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
        proposed_space_group_key: Option<ProposedSpaceGroupKey>,
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
            if let Some(proposed_space_group_key) = proposed_space_group_key {
                reserve_proposed_space_group(
                    &transaction,
                    proposed_space_group_key,
                    expected_task_id,
                    expected_intent_revision_id,
                    &existing.plan,
                )?;
            }
            transaction.commit().map_err(sql_error(
                "commit existing Candidate Confirmation reservation",
            ))?;
            return Ok(CandidateConfirmationReservation {
                operation: existing,
                created: false,
            });
        }
        if let Some(proposed_space_group_key) = proposed_space_group_key {
            reserve_proposed_space_group(
                &transaction,
                proposed_space_group_key,
                expected_task_id,
                expected_intent_revision_id,
                plan,
            )?;
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
        self.finalize_candidate_confirmations(&[CandidateConfirmationFinalize {
            candidate_id,
            operation_hash: operation_hash.to_owned(),
            confirmation_id,
            result_context_id,
        }])?
        .pop()
        .ok_or_else(|| invariant("Candidate Confirmation finalize produced no outcome"))
    }

    /// Marks several Git-committed Confirmations terminal inside one transaction.
    ///
    /// The Git batch that committed these Confirmations was atomic, so the Runtime side is too:
    /// a rejected member rolls every Review and operation status back and names the failing
    /// `CandidateId`. Retrying the same batch replays the Git write and finalizes again.
    ///
    /// # Errors
    ///
    /// Returns an input error for an empty or repeating batch, and typed operation-hash,
    /// plan-identity, Review CAS, or storage errors for any member.
    pub fn finalize_candidate_confirmations(
        &self,
        requests: &[CandidateConfirmationFinalize],
    ) -> Result<Vec<CandidateConfirmationFinalizeOutcome>> {
        if requests.is_empty() {
            return Err(invalid(
                "Candidate Confirmation finalize requires at least one Candidate",
            ));
        }
        let mut seen = BTreeSet::new();
        for request in requests {
            if !seen.insert(request.candidate_id) {
                return Err(invalid(
                    "Candidate Confirmation finalize batch must not repeat a Candidate",
                ));
            }
        }
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Candidate Confirmation finalize")?;
        let mut outcomes = Vec::with_capacity(requests.len());
        for request in requests {
            outcomes.push(
                finalize_one_candidate_confirmation(&transaction, request)
                    .map_err(confirmation_scoped_error(request.candidate_id))?,
            );
        }
        transaction
            .commit()
            .map_err(sql_error("commit Candidate Confirmation finalize"))?;
        Ok(outcomes)
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

    /// Records the Contexts one retrieval entry point just injected into a Task.
    ///
    /// `(task_id, context_id)` is unique: re-injecting the same Context into the same Task only
    /// refreshes `injected_at_unix_seconds`, so the first Intent revision, revision and entry
    /// point that produced the injection stay the recorded provenance.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors. Callers treat them as advisory: a failed injection record
    /// must never change the retrieval response.
    pub fn record_task_injections(
        &self,
        task_id: TaskId,
        intent_revision_id: TaskIntentRevisionId,
        source: ContextInjectionSource,
        contexts: &[InjectedContext],
    ) -> Result<usize> {
        self.record_task_injections_at(
            task_id,
            intent_revision_id,
            source,
            contexts,
            unix_seconds(SystemTime::now())?,
        )
    }

    /// Records injections against an explicit clock reading.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn record_task_injections_at(
        &self,
        task_id: TaskId,
        intent_revision_id: TaskIntentRevisionId,
        source: ContextInjectionSource,
        contexts: &[InjectedContext],
        now_unix_seconds: u64,
    ) -> Result<usize> {
        if contexts.is_empty() {
            return Ok(0);
        }
        let injected_at = i64::try_from(now_unix_seconds)
            .map_err(|_| invalid("injection timestamp exceeds the supported range"))?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Task injection record")?;
        let mut written = 0;
        for context in contexts.iter().collect::<BTreeSet<_>>() {
            written += transaction
                .execute(
                    "INSERT INTO task_injection (
                        task_id, context_id, intent_revision_id, revision_id,
                        injected_at_unix_seconds, source
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (task_id, context_id) DO UPDATE SET
                        injected_at_unix_seconds = excluded.injected_at_unix_seconds",
                    params![
                        task_id.to_string(),
                        context.context_id.to_string(),
                        intent_revision_id.to_string(),
                        context.revision_id.to_string(),
                        injected_at,
                        source.as_str(),
                    ],
                )
                .map_err(sql_error("record Task injection"))?;
        }
        transaction
            .commit()
            .map_err(sql_error("commit Task injection record"))?;
        Ok(written)
    }

    /// Reads every Context injected into one Task, ordered by Context identity.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn read_task_injections(&self, task_id: TaskId) -> Result<Vec<TaskInjectionRecord>> {
        let connection = self.open_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT context_id, intent_revision_id, revision_id,
                        injected_at_unix_seconds, source
                 FROM task_injection WHERE task_id = ?1 ORDER BY context_id ASC",
            )
            .map_err(sql_error("prepare Task injection read"))?;
        let rows = statement
            .query_map(params![task_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(sql_error("read Task injections"))?;
        let mut records = Vec::new();
        for row in rows {
            let row = row.map_err(sql_error("read Task injection row"))?;
            records.push(TaskInjectionRecord {
                task_id,
                context_id: parse_id(&row.0, "task_injection.context_id")?,
                intent_revision_id: parse_id(&row.1, "task_injection.intent_revision_id")?,
                revision_id: parse_id(&row.2, "task_injection.revision_id")?,
                injected_at_unix_seconds: u64::try_from(row.3).map_err(|_| {
                    invariant("task_injection.injected_at_unix_seconds is negative")
                })?,
                source: ContextInjectionSource::parse(&row.4)?,
            });
        }
        Ok(records)
    }

    /// Records what one Task did with the Contexts injected into it.
    ///
    /// The last write for a `(context, task)` pair wins, except that `refuted` is never
    /// downgraded. Re-deriving the same outcomes writes the same rows, so a same-content
    /// Checkpoint replay leaves the table unchanged.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn record_context_usage(&self, records: &[ContextUsageRecord]) -> Result<usize> {
        self.record_context_usage_at(records, unix_seconds(SystemTime::now())?)
    }

    /// Records usage outcomes against an explicit clock reading.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn record_context_usage_at(
        &self,
        records: &[ContextUsageRecord],
        now_unix_seconds: u64,
    ) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let recorded_at = i64::try_from(now_unix_seconds)
            .map_err(|_| invalid("Context usage timestamp exceeds the supported range"))?;
        let mut connection = self.open_connection()?;
        let transaction = immediate(&mut connection, "begin Context usage record")?;
        let mut written = 0;
        for record in records {
            written += transaction
                .execute(
                    "INSERT INTO context_usage (
                        context_id, task_id, outcome, recorded_at_unix_seconds
                     ) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (context_id, task_id) DO UPDATE SET
                        outcome = excluded.outcome,
                        recorded_at_unix_seconds = excluded.recorded_at_unix_seconds
                     WHERE ?5 >= (CASE context_usage.outcome WHEN 'refuted' THEN 1 ELSE 0 END)",
                    params![
                        record.context_id.to_string(),
                        record.task_id.to_string(),
                        record.outcome.as_str(),
                        recorded_at,
                        i64::from(record.outcome.priority()),
                    ],
                )
                .map_err(sql_error("record Context usage"))?;
        }
        transaction
            .commit()
            .map_err(sql_error("commit Context usage record"))?;
        Ok(written)
    }

    /// Aggregates the recorded usage outcomes of the requested Contexts.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn context_usage_totals(
        &self,
        context_ids: &[ContextId],
    ) -> Result<BTreeMap<ContextId, ContextUsageTotals>> {
        let mut totals = BTreeMap::new();
        let requested = context_ids.iter().copied().collect::<BTreeSet<_>>();
        if requested.is_empty() {
            return Ok(totals);
        }
        let connection = self.open_connection()?;
        for chunk in requested
            .into_iter()
            .collect::<Vec<_>>()
            .chunks(USAGE_TOTALS_QUERY_CHUNK)
        {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let mut statement = connection
                .prepare(&format!(
                    "SELECT context_id, outcome, COUNT(*) FROM context_usage
                     WHERE context_id IN ({placeholders})
                     GROUP BY context_id, outcome"
                ))
                .map_err(sql_error("prepare Context usage totals"))?;
            let parameters = chunk.iter().map(ToString::to_string).collect::<Vec<_>>();
            let rows = statement
                .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql_error("read Context usage totals"))?;
            for row in rows {
                let row = row.map_err(sql_error("read Context usage totals row"))?;
                let context_id = parse_id::<ContextId>(&row.0, "context_usage.context_id")?;
                let count = u32::try_from(row.2).unwrap_or(u32::MAX);
                let entry = totals
                    .entry(context_id)
                    .or_insert_with(ContextUsageTotals::default);
                match row.1.as_str() {
                    "reused" => entry.reused = count,
                    "ignored" => entry.ignored = count,
                    "refuted" => entry.refuted = count,
                    other => return Err(invariant(format!("unknown usage outcome {other}"))),
                }
            }
        }
        Ok(totals)
    }

    /// Records one Hook-path diagnostic row, best-effort from the caller's perspective.
    ///
    /// This is the only write on the Hook hot path: one `INSERT`, and — once per
    /// [`HOOK_EVENT_PRUNE_INTERVAL`](crate) inserts, keyed off the assigned row id so no counter
    /// needs to survive across Hook process invocations — one bounded `DELETE` that keeps the
    /// table under [`HOOK_EVENT_RETENTION_ROWS`](crate) rows. No new lock is taken.
    ///
    /// # Errors
    ///
    /// Returns typed validation or storage errors. Callers treat every error as advisory: a
    /// failed diagnostic write must never change Hook behavior.
    pub fn record_hook_event(&self, record: &HookEventRecord) -> Result<()> {
        Self::record_hook_event_at(&self.database, record)
    }

    /// Writes one Hook diagnostic row directly against `database_path`, without constructing a
    /// full [`TaskRuntime`] first (no directory creation, no schema check, no `PRAGMA
    /// foreign_keys`). A Hook-path caller that already knows the installation root can use this
    /// to record without paying for the extra validating connection open
    /// [`TaskRuntime::initialize_for_hook`] performs, which matters when many Hook processes
    /// write concurrently. The `hook_event` table must already exist — a schema that predates
    /// it, or no installation database at all, both surface as a typed error the caller
    /// degrades exactly like any other open/write failure.
    ///
    /// # Errors
    ///
    /// Returns typed validation or storage errors. Callers treat every error as advisory: a
    /// failed diagnostic write must never change Hook behavior.
    pub fn record_hook_event_at(database_path: &Path, record: &HookEventRecord) -> Result<()> {
        record.validate()?;
        // `SQLITE_OPEN_READ_WRITE` only, deliberately without `SQLITE_OPEN_CREATE`: this write
        // must never be the thing that first creates `runtime.sqlite`. `TaskRuntime::open_connection`
        // only runs `ensure_schema` (which sets `journal_mode=WAL` and creates every table) when
        // the database file did not already exist; a schema-less file this diagnostic write
        // created would silently poison every later `initialize_for_hook` call into skipping
        // schema setup, breaking the real Task Runtime tables. If the installation has not been
        // set up yet, this simply fails to open and degrades to a stderr line, same as any other
        // Runtime-unavailable case.
        //
        // A dedicated connection with its own short busy window: see [`HOOK_EVENT_BUSY_TIMEOUT`]
        // for why this write must fail fast under contention instead of inheriting the
        // interactive or Hook-authorization busy window.
        let connection = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_error("open task runtime database"))?;
        connection
            .busy_timeout(HOOK_EVENT_BUSY_TIMEOUT)
            .map_err(sql_error("configure hook event busy timeout"))?;
        connection
            .execute(
                "INSERT INTO hook_event (
                    recorded_at_unix_ms, agent_kind, external_session_id, event_kind,
                    decision, reason, duration_ms, detail
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    i64::try_from(record.recorded_at_unix_ms).unwrap_or(i64::MAX),
                    record.agent_kind,
                    record.external_session_id,
                    record.event_kind,
                    record.decision.as_str(),
                    record.reason,
                    i64::try_from(record.duration_ms).unwrap_or(i64::MAX),
                    record.detail,
                ],
            )
            .map_err(sql_error("insert hook event"))?;
        let id = connection.last_insert_rowid();
        if id > 0 && id % HOOK_EVENT_PRUNE_INTERVAL == 0 {
            let _ = connection.execute(
                "DELETE FROM hook_event WHERE id < ?1",
                params![id - HOOK_EVENT_RETENTION_ROWS],
            );
        }
        Ok(())
    }

    /// Counts `hook_event` rows recorded at or after `since_unix_ms`, grouped by decision and
    /// reason. Used by `sctx doctor --hooks`; never on the Hook hot path.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn hook_event_counts_since(&self, since_unix_ms: u64) -> Result<Vec<HookEventCount>> {
        let connection = self.open_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT decision, reason, COUNT(*) FROM hook_event
                 WHERE recorded_at_unix_ms >= ?1
                 GROUP BY decision, reason
                 ORDER BY decision ASC, reason ASC",
            )
            .map_err(sql_error("prepare hook event counts"))?;
        let rows = statement
            .query_map(params![i64::try_from(since_unix_ms).unwrap_or(0)], |row| {
                Ok(HookEventCount {
                    decision: row.get(0)?,
                    reason: row.get(1)?,
                    count: row.get::<_, i64>(2)?,
                })
            })
            .map_err(sql_error("query hook event counts"))?;
        let mut counts = Vec::new();
        for row in rows {
            let mut row = row.map_err(sql_error("read hook event count row"))?;
            row.count = row.count.max(0);
            counts.push(row);
        }
        Ok(counts)
    }

    /// Reads the most recently recorded `hook_event` rows, newest first. Used by
    /// `sctx doctor --hooks`; never on the Hook hot path.
    ///
    /// # Errors
    ///
    /// Returns typed storage errors.
    pub fn recent_hook_events(&self, limit: usize) -> Result<Vec<HookEventView>> {
        let connection = self.open_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT recorded_at_unix_ms, agent_kind, external_session_id, event_kind,
                        decision, reason, duration_ms, detail
                 FROM hook_event ORDER BY id DESC LIMIT ?1",
            )
            .map_err(sql_error("prepare recent hook events"))?;
        let rows = statement
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(HookEventView {
                    recorded_at_unix_ms: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    agent_kind: row.get(1)?,
                    external_session_id: row.get(2)?,
                    event_kind: row.get(3)?,
                    decision: row.get(4)?,
                    reason: row.get(5)?,
                    duration_ms: u64::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
                    detail: row.get(7)?,
                })
            })
            .map_err(sql_error("query recent hook events"))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(sql_error("read recent hook event row"))?);
        }
        Ok(events)
    }

    fn open_connection(&self) -> Result<Connection> {
        let connection =
            Connection::open(&self.database).map_err(sql_error("open task runtime database"))?;
        connection
            .busy_timeout(self.busy_timeout)
            .map_err(sql_error("configure task runtime busy timeout"))?;
        if self.configure_schema_on_open {
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(sql_error("configure task runtime journal mode"))?;
            connection
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(sql_error("configure task runtime synchronous mode"))?;
        }
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_error("enable task runtime foreign keys"))?;
        if self.configure_schema_on_open {
            ensure_schema(&connection)?;
        }
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
    if version == 13 {
        return migrate_schema_13_to_14(connection);
    }
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
                recovery_attempt_generation INTEGER NOT NULL DEFAULT 0
                    CHECK (recovery_attempt_generation >= 0),
                last_recovery_status TEXT CHECK (
                    last_recovery_status IS NULL OR last_recovery_status IN ('succeeded', 'failed')
                ),
                last_recovery_error_code TEXT,
                CHECK (
                    (last_recovery_status = 'failed' AND last_recovery_error_code IS NOT NULL) OR
                    (last_recovery_status IS NULL AND last_recovery_error_code IS NULL) OR
                    (last_recovery_status = 'succeeded' AND last_recovery_error_code IS NULL)
                ),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id),
                FOREIGN KEY (final_checkpoint_id, episode_id)
                    REFERENCES agent_checkpoint (checkpoint_id, episode_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS checkpoint_operation (
                operation_key TEXT PRIMARY KEY,
                operation_id TEXT NOT NULL UNIQUE,
                semantic_json TEXT NOT NULL CHECK (json_valid(semantic_json)),
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                intent_revision_id TEXT NOT NULL,
                checkpoint_id TEXT NOT NULL UNIQUE,
                episode_id TEXT NOT NULL UNIQUE,
                build_id TEXT NOT NULL UNIQUE,
                FOREIGN KEY (task_session_id) REFERENCES task_session (task_session_id),
                FOREIGN KEY (task_id) REFERENCES task_session (task_id),
                FOREIGN KEY (intent_revision_id)
                    REFERENCES task_intent_revision (revision_id),
                FOREIGN KEY (checkpoint_id, episode_id)
                    REFERENCES agent_checkpoint (checkpoint_id, episode_id),
                FOREIGN KEY (build_id) REFERENCES candidate_build (build_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS candidate_build_item (
                build_id TEXT NOT NULL,
                item_ordinal INTEGER NOT NULL CHECK (item_ordinal >= 0),
                checkpoint_id TEXT NOT NULL,
                claim_id TEXT NOT NULL,
                submission_id TEXT NOT NULL UNIQUE,
                content_hash TEXT,
                status TEXT NOT NULL CHECK (status IN (
                    'queued', 'prepared', 'needs_evidence', 'created', 'already_exists', 'failed'
                )),
                candidate_id TEXT,
                event_id TEXT,
                error_code TEXT,
                PRIMARY KEY (build_id, claim_id),
                UNIQUE (build_id, item_ordinal),
                CHECK (
                    (status = 'queued' AND content_hash IS NULL
                        AND candidate_id IS NULL AND event_id IS NULL AND error_code IS NULL) OR
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
            CREATE TABLE IF NOT EXISTS candidate_build_duplicate (
                build_id TEXT NOT NULL,
                duplicate_ordinal INTEGER NOT NULL CHECK (duplicate_ordinal >= 0),
                checkpoint_id TEXT NOT NULL,
                claim_id TEXT NOT NULL,
                duplicate_of_candidate_id TEXT NOT NULL,
                similarity_basis_points INTEGER NOT NULL CHECK (
                    similarity_basis_points BETWEEN 0 AND 10000
                ),
                PRIMARY KEY (build_id, claim_id),
                UNIQUE (build_id, duplicate_ordinal),
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
            CREATE TABLE IF NOT EXISTS proposed_space_group (
                proposed_space_group_key TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                intent_revision_id TEXT NOT NULL,
                candidate_id TEXT NOT NULL UNIQUE,
                space_id TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('reserved', 'committed')),
                UNIQUE (task_id, intent_revision_id),
                FOREIGN KEY (candidate_id) REFERENCES candidate_review (candidate_id),
                FOREIGN KEY (intent_revision_id) REFERENCES task_intent_revision (revision_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS task_injection (
                task_id TEXT NOT NULL,
                context_id TEXT NOT NULL,
                intent_revision_id TEXT NOT NULL,
                revision_id TEXT NOT NULL,
                injected_at_unix_seconds INTEGER NOT NULL CHECK (
                    injected_at_unix_seconds >= 0
                ),
                source TEXT NOT NULL CHECK (
                    source IN ('intent_update', 'task_context', 'artifact_focus')
                ),
                PRIMARY KEY (task_id, context_id),
                FOREIGN KEY (task_id) REFERENCES task_session (task_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS context_usage (
                context_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                outcome TEXT NOT NULL CHECK (
                    outcome IN ('reused', 'ignored', 'refuted')
                ),
                recorded_at_unix_seconds INTEGER NOT NULL CHECK (
                    recorded_at_unix_seconds >= 0
                ),
                PRIMARY KEY (context_id, task_id),
                FOREIGN KEY (task_id) REFERENCES task_session (task_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS checkpoint_reference_derivation (
                episode_id TEXT PRIMARY KEY,
                FOREIGN KEY (episode_id) REFERENCES work_episode (episode_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS hook_event (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                recorded_at_unix_ms INTEGER NOT NULL CHECK (recorded_at_unix_ms >= 0),
                agent_kind TEXT NOT NULL,
                external_session_id TEXT,
                event_kind TEXT NOT NULL,
                decision TEXT NOT NULL CHECK (
                    decision IN ('enabled', 'disabled', 'neutral', 'fail_open')
                ),
                reason TEXT NOT NULL,
                duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
                detail TEXT CHECK (detail IS NULL OR length(detail) <= 256)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS hook_event_recorded_at
                ON hook_event (recorded_at_unix_ms);
            PRAGMA user_version = 14;",
        )
        .map_err(sql_error("initialize task runtime schema"))
}

/// Adds the additive `hook_event` table (and its index) to an existing schema version 13
/// installation and advances `user_version` to 14, in one transaction. Every prior table and
/// its data is left untouched — this is the only supported upgrade path; every other version
/// mismatch still hard-fails in [`ensure_schema`].
fn migrate_schema_13_to_14(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS hook_event (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                recorded_at_unix_ms INTEGER NOT NULL CHECK (recorded_at_unix_ms >= 0),
                agent_kind TEXT NOT NULL,
                external_session_id TEXT,
                event_kind TEXT NOT NULL,
                decision TEXT NOT NULL CHECK (
                    decision IN ('enabled', 'disabled', 'neutral', 'fail_open')
                ),
                reason TEXT NOT NULL,
                duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
                detail TEXT CHECK (detail IS NULL OR length(detail) <= 256)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS hook_event_recorded_at
                ON hook_event (recorded_at_unix_ms);
            PRAGMA user_version = 14;
            COMMIT;",
        )
        .map_err(sql_error(
            "migrate task runtime schema from version 13 to 14",
        ))
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

fn locate_active_task(
    transaction: &Transaction<'_>,
    locator: &ExternalSessionLocator,
) -> Result<Option<(TaskSessionId, TaskId)>> {
    let Some(task_session_id) = find_active_task_by_locator(transaction, locator)? else {
        return Ok(None);
    };
    let (task_id, _) = read_active_task_head(transaction, task_session_id)?
        .ok_or_else(|| invariant("located ActiveTask is not active"))?;
    Ok(Some((task_session_id, task_id)))
}

fn merge_signals_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    signals: &[TaskSignal],
) -> Result<MergeSignalsOutcome> {
    let inserted_signal_ids =
        insert_signals_in_transaction(transaction, task_session_id, task_id, signals)?;
    let snapshot = require_snapshot(transaction, task_session_id)?;
    Ok(MergeSignalsOutcome {
        snapshot,
        inserted: inserted_signal_ids.len(),
        inserted_signal_ids,
    })
}

fn insert_signals_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    task_id: TaskId,
    signals: &[TaskSignal],
) -> Result<Vec<SignalId>> {
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
    Ok(inserted_signal_ids)
}

/// Supersedes the oldest Active Signals of each rule's kinds until the rule's budget holds.
///
/// Ordering is by `signal_ordinal`, the same monotonic append order the merge above assigns,
/// so "oldest" means "inserted first" and never depends on wall-clock time.
fn enforce_signal_retention(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    retention: &[SignalRetentionRule],
) -> Result<Vec<SignalId>> {
    let mut retired = Vec::new();
    for rule in retention {
        if rule.kinds.is_empty() {
            continue;
        }
        let placeholders = (2..2 + rule.kinds.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = transaction
            .prepare(&format!(
                "SELECT signal_id FROM task_signal
                 WHERE task_session_id = ?1 AND lifecycle = 'active' AND kind IN ({placeholders})
                 ORDER BY signal_ordinal ASC"
            ))
            .map_err(sql_error("prepare Signal retention scan"))?;
        let mut parameters = vec![task_session_id.to_string()];
        parameters.extend(
            rule.kinds
                .iter()
                .map(|kind| signal_kind_name(*kind).to_owned()),
        );
        let active = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql_error("scan Active Signals for retention"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error("read Active Signals for retention"))?;
        drop(statement);
        let Some(excess) = active.len().checked_sub(rule.max_active) else {
            continue;
        };
        for value in active.into_iter().take(excess) {
            let signal_id: SignalId = parse_id(&value, "task_signal.signal_id")?;
            let changed = transaction
                .execute(
                    "UPDATE task_signal SET lifecycle = 'superseded'
                     WHERE signal_id = ?1 AND lifecycle = 'active'",
                    [signal_id.to_string()],
                )
                .map_err(sql_error("supersede retained Task Signal"))?;
            if changed != 1 {
                return Err(invariant(
                    "Signal lifecycle changed inside write transaction",
                ));
            }
            retired.push(signal_id);
        }
    }
    Ok(retired)
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
               AND episode.status = 'closed'
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
                "relations": claim.relations,
                "engineering_references": claim.engineering_references,
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

fn direct_checkpoint_semantic_json(input: &AgentCheckpointSubmission) -> Result<String> {
    let claims = input
        .claims
        .iter()
        .map(|claim| {
            let evidence = claim
                .evidence
                .iter()
                .map(|evidence| {
                    serde_json::json!({
                        "evidence_type": evidence.evidence_type,
                        "summary": evidence.summary,
                        "limitations": evidence.limitations,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "context_kind": claim.context_kind,
                "statement": claim.statement,
                "rationale": claim.rationale,
                "conditions": claim.conditions,
                "evidence": evidence,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&serde_json::json!({
        "claims": claims,
        "unknowns": input.unknowns,
    }))
    .map_err(json_error("serialize direct Checkpoint semantics"))
}

fn materialize_direct_checkpoint_claims(
    claims: &[DirectCheckpointClaimDraft],
    intent: &WorkingIntentSnapshot,
) -> Result<Vec<CheckpointClaimDraft>> {
    claims
        .iter()
        .map(|claim| materialize_direct_checkpoint_claim(claim, intent))
        .collect()
}

/// Turns one direct Claim into its persisted shape without consulting any checkout.
///
/// The durable ACK is receipt plus outbox: it validates Evidence, inherits applicability from the
/// Working Intent, and stops there. Engineering coordinates and the topic hint stay empty until
/// Candidate Build derives them once through [`TaskRuntime::derive_episode_claim_references`].
fn materialize_direct_checkpoint_claim(
    claim: &DirectCheckpointClaimDraft,
    intent: &WorkingIntentSnapshot,
) -> Result<CheckpointClaimDraft> {
    if claim.evidence.is_empty() {
        return Err(invalid("Checkpoint Claim Evidence must not be empty"));
    }
    let applicability = Applicability {
        domains: intent.domains.clone(),
        platforms: intent.platforms.clone(),
        conditions: claim.conditions.clone(),
    };
    applicability.validate("checkpoint_submission.claim.applicability")?;
    let inline_validations = claim
        .evidence
        .iter()
        .map(|evidence| {
            if evidence.summary.trim().is_empty() {
                return Err(invalid(
                    "checkpoint_submission.claim.evidence.summary must not be empty",
                ));
            }
            let draft = EvidenceSnapshotDraft {
                kind: evidence.evidence_type,
                supports: claim.statement.clone(),
                content: serde_json::json!({"summary": evidence.summary}),
                interpretation: claim.rationale.clone(),
                limitations: evidence.limitations.clone(),
            };
            draft.validate("checkpoint_submission.claim.evidence")?;
            Ok(draft)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(CheckpointClaimDraft {
        context_kind_hint: Some(claim.context_kind),
        topic_key_hint: None,
        statement: claim.statement.clone(),
        rationale: claim.rationale.clone(),
        applicability,
        assumptions: Vec::new(),
        recheck_when: Vec::new(),
        evidence_refs: Vec::new(),
        inline_validations,
        artifact_refs: Vec::new(),
        relations: Vec::new(),
        engineering_references: Vec::new(),
        related_contexts: Vec::new(),
    })
}

fn checkpoint_operation_identity(
    task_session_id: TaskSessionId,
    task_id: TaskId,
    intent_revision_id: TaskIntentRevisionId,
    semantic_json: &str,
) -> (String, String) {
    let mut hasher = Sha256::new();
    hasher.update(b"shared-context-checkpoint-operation-v1");
    for value in [
        task_session_id.to_string(),
        task_id.to_string(),
        intent_revision_id.to_string(),
        semantic_json.to_owned(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    let operation_key = format!("sha256:{digest}");
    (operation_key.clone(), operation_key)
}

fn read_checkpoint_operation(
    connection: &Connection,
    operation_key: &str,
) -> Result<Option<CheckpointOperationRecord>> {
    let row = connection
        .query_row(
            "SELECT operation_id, semantic_json, task_session_id, task_id,
                    intent_revision_id, checkpoint_id, episode_id, build_id
             FROM checkpoint_operation WHERE operation_key = ?1",
            [operation_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Checkpoint operation"))?;
    row.map(|row| {
        Ok(CheckpointOperationRecord {
            operation_id: row.0,
            semantic_json: row.1,
            task_session_id: parse_id(&row.2, "checkpoint_operation.task_session_id")?,
            task_id: parse_id(&row.3, "checkpoint_operation.task_id")?,
            intent_revision_id: parse_id(&row.4, "checkpoint_operation.intent_revision_id")?,
            checkpoint_id: parse_id(&row.5, "checkpoint_operation.checkpoint_id")?,
            episode_id: parse_id(&row.6, "checkpoint_operation.episode_id")?,
            build_id: parse_id(&row.7, "checkpoint_operation.build_id")?,
        })
    })
    .transpose()
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
            CheckpointEvidenceRef::Observation { observation_id }
                if !observations.contains(observation_id) =>
            {
                return Err(invalid(
                    "Checkpoint Observation evidence does not belong to its Work Episode",
                ));
            }
            CheckpointEvidenceRef::TaskSignal { signal_id } if !signals.contains(signal_id) => {
                return Err(invalid(
                    "Checkpoint TaskSignal evidence does not belong to its Task",
                ));
            }
            CheckpointEvidenceRef::Observation { .. }
            | CheckpointEvidenceRef::TaskSignal { .. }
            | CheckpointEvidenceRef::ContextEvidence { .. } => {}
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
            let CheckpointEvidenceRef::Observation { observation_id } = evidence else {
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
            CandidateBuildItemStatus::Queued
            | CandidateBuildItemStatus::Created
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
    duplicates: &[CandidateBuildDuplicatePreparation],
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
        .chain(
            duplicates
                .iter()
                .map(|duplicate| (duplicate.checkpoint_id, duplicate.claim_id)),
        )
        .collect::<BTreeSet<_>>();
    if persisted.len() != items.len() + duplicates.len() || persisted != supplied {
        return Err(invalid(
            "Candidate Build must classify every persisted Checkpoint Claim exactly once",
        ));
    }
    Ok(())
}

fn insert_candidate_build_duplicates(
    transaction: &Transaction<'_>,
    build_id: CandidateBuildId,
    duplicates: &[CandidateBuildDuplicatePreparation],
) -> Result<()> {
    for duplicate in duplicates {
        let ordinal = next_ordinal(
            transaction,
            "SELECT COALESCE(MAX(duplicate_ordinal), -1) + 1
             FROM candidate_build_duplicate WHERE build_id = ?1",
            build_id.to_string(),
            "read next Candidate Build duplicate ordinal",
        )?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO candidate_build_duplicate (
                    build_id, duplicate_ordinal, checkpoint_id, claim_id,
                    duplicate_of_candidate_id, similarity_basis_points
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    build_id.to_string(),
                    ordinal,
                    duplicate.checkpoint_id.to_string(),
                    duplicate.claim_id.to_string(),
                    duplicate.duplicate_of_candidate_id.to_string(),
                    i64::from(duplicate.similarity_basis_points),
                ],
            )
            .map_err(sql_error("record Candidate Build duplicate"))?;
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
        CandidateBuildItemStatus::Queued
        | CandidateBuildItemStatus::Prepared
        | CandidateBuildItemStatus::NeedsEvidence => Err(invalid(
            "Candidate Build result requires a terminal submission status",
        )),
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
    let status = if items.iter().any(|item| {
        matches!(
            item.status,
            CandidateBuildItemStatus::Queued | CandidateBuildItemStatus::Prepared
        )
    }) {
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
        duplicates: read_candidate_build_duplicates(connection, build_id)?,
    })
}

fn read_candidate_build_duplicates(
    connection: &Connection,
    build_id: CandidateBuildId,
) -> Result<Vec<CandidateBuildDuplicateView>> {
    let mut statement = connection
        .prepare(
            "SELECT checkpoint_id, claim_id, duplicate_of_candidate_id, similarity_basis_points
             FROM candidate_build_duplicate WHERE build_id = ?1 ORDER BY duplicate_ordinal ASC",
        )
        .map_err(sql_error("prepare Candidate Build duplicates"))?;
    let rows = statement
        .query_map([build_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(sql_error("query Candidate Build duplicates"))?;
    rows.map(|row| {
        let row = row.map_err(sql_error("read Candidate Build duplicate row"))?;
        Ok(CandidateBuildDuplicateView {
            checkpoint_id: parse_id(&row.0, "candidate_build_duplicate.checkpoint_id")?,
            claim_id: parse_id(&row.1, "candidate_build_duplicate.claim_id")?,
            duplicate_of_candidate_id: parse_id(
                &row.2,
                "candidate_build_duplicate.duplicate_of_candidate_id",
            )?,
            similarity_basis_points: u16::try_from(row.3).map_err(|_| {
                invariant("Candidate Build duplicate similarity is outside the safe bound")
            })?,
        })
    })
    .collect()
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
        "queued" => Ok(CandidateBuildItemStatus::Queued),
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

type ProposedSpaceGroupRow = (String, String, String, String, String, String);

fn parse_proposed_space_group_mapping(
    row: &ProposedSpaceGroupRow,
) -> Result<ProposedSpaceGroupMapping> {
    let mapping = ProposedSpaceGroupMapping {
        proposed_space_group_key: parse_id(
            &row.0,
            "proposed_space_group.proposed_space_group_key",
        )?,
        task_id: parse_id(&row.1, "proposed_space_group.task_id")?,
        intent_revision_id: parse_id(&row.2, "proposed_space_group.intent_revision_id")?,
        candidate_id: parse_id(&row.3, "proposed_space_group.candidate_id")?,
        space_id: parse_id(&row.4, "proposed_space_group.space_id")?,
        status: match row.5.as_str() {
            "reserved" => ProposedSpaceGroupMappingStatus::Reserved,
            "committed" => ProposedSpaceGroupMappingStatus::Committed,
            _ => {
                return Err(invariant(
                    "persisted proposed Space group status is invalid",
                ));
            }
        },
    };
    if mapping.proposed_space_group_key != ProposedSpaceGroupKey::from_task(mapping.task_id) {
        return Err(invariant(
            "persisted proposed Space group key disagrees with Task ownership",
        ));
    }
    Ok(mapping)
}

fn read_proposed_space_group_mapping(
    connection: &Connection,
    proposed_space_group_key: ProposedSpaceGroupKey,
) -> Result<Option<ProposedSpaceGroupMapping>> {
    connection
        .query_row(
            "SELECT proposed_space_group_key, task_id, intent_revision_id,
                    candidate_id, space_id, status
             FROM proposed_space_group WHERE proposed_space_group_key = ?1",
            [proposed_space_group_key.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read proposed Space group mapping"))?
        .map(|row| parse_proposed_space_group_mapping(&row))
        .transpose()
}

fn read_proposed_space_group_mapping_for_candidate(
    connection: &Connection,
    candidate_id: CandidateId,
) -> Result<Option<ProposedSpaceGroupMapping>> {
    connection
        .query_row(
            "SELECT proposed_space_group_key, task_id, intent_revision_id,
                    candidate_id, space_id, status
             FROM proposed_space_group WHERE candidate_id = ?1",
            [candidate_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error("read Candidate proposed Space group mapping"))?
        .map(|row| parse_proposed_space_group_mapping(&row))
        .transpose()
}

/// Recovery guidance for every proposed Space group rejection a caller can actually repair.
const PROPOSED_SPACE_GROUP_RECOVERY: &str = "proposed Space group does not belong to this Task: use candidate_get to refresh \
     recommendations, or confirm into an existing Space with primary.existing_space_id";

fn reserve_proposed_space_group(
    transaction: &Transaction<'_>,
    proposed_space_group_key: ProposedSpaceGroupKey,
    task_id: TaskId,
    intent_revision_id: TaskIntentRevisionId,
    plan: &CandidateConfirmationPlan,
) -> Result<()> {
    // The group is bound to the Task, never to the current Intent head: governance turns
    // legitimately advance the Intent between the recommendation being generated and being
    // confirmed. `expected_intent_revision_id` still guards the head through the caller's CAS.
    if proposed_space_group_key != ProposedSpaceGroupKey::from_task(task_id) {
        return Err(invalid(PROPOSED_SPACE_GROUP_RECOVERY));
    }
    let space_id = plan
        .confirmation
        .created_space_id
        .ok_or_else(|| invalid("proposed Space group requires a new Primary Space plan"))?;
    if space_id != plan.confirmation.primary_space_id {
        return Err(invariant(
            "proposed Space group plan does not create its Primary Space",
        ));
    }
    if let Some(existing) =
        read_proposed_space_group_mapping(transaction, proposed_space_group_key)?
    {
        if existing.task_id == task_id
            && existing.intent_revision_id == intent_revision_id
            && existing.candidate_id == plan.operation.candidate_id
            && existing.space_id == space_id
        {
            return Ok(());
        }
        return Err(Error::new(
            ErrorKind::Conflict,
            "proposed Space group is already reserved by another Candidate of this Task: \
             use candidate_get to refresh recommendations, or confirm into an existing Space \
             with primary.existing_space_id",
        ));
    }
    transaction
        .execute(
            "INSERT INTO proposed_space_group (
                proposed_space_group_key, task_id, intent_revision_id,
                candidate_id, space_id, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'reserved')",
            params![
                proposed_space_group_key.to_string(),
                task_id.to_string(),
                intent_revision_id.to_string(),
                plan.operation.candidate_id.to_string(),
                space_id.to_string(),
            ],
        )
        .map_err(sql_error("reserve proposed Space group"))?;
    Ok(())
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
    }))
}

/// Whether Candidate Build already placed this Episode's Claim spellings.
fn claim_references_derived(connection: &Connection, episode_id: WorkEpisodeId) -> Result<bool> {
    connection
        .query_row(
            "SELECT 1 FROM checkpoint_reference_derivation WHERE episode_id = ?1",
            [episode_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
        .map_err(sql_error("read Claim reference derivation marker"))
}

/// Every path-shaped spelling in one Episode, so one `git ls-files` answers the whole Build.
fn episode_path_candidates(episode: &WorkEpisodeView) -> Vec<PathCandidate> {
    let mut candidates = Vec::new();
    for checkpoint in &episode.checkpoints {
        for claim in &checkpoint.claims {
            let summaries = claim_evidence_summaries(&episode.episode, claim);
            let borrowed = summaries.iter().map(String::as_str).collect::<Vec<_>>();
            candidates.extend(reference_derivation::claim_path_candidates(
                &claim.statement,
                &claim.rationale,
                &borrowed,
            ));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

/// Recovers the Evidence summaries one persisted Claim was submitted with.
///
/// Direct Checkpoint Evidence becomes an inline-validation Observation whose content carries the
/// Agent's own `summary`, so Candidate Build reads back exactly the text the ACK saw.
fn claim_evidence_summaries(episode: &WorkEpisode, claim: &CheckpointClaim) -> Vec<String> {
    claim
        .evidence_refs
        .iter()
        .filter_map(|reference| match reference {
            CheckpointEvidenceRef::Observation { observation_id } => episode
                .observations
                .iter()
                .find(|observation| observation.observation_id == *observation_id),
            CheckpointEvidenceRef::TaskSignal { .. }
            | CheckpointEvidenceRef::ContextEvidence { .. } => None,
        })
        .filter_map(|observation| match &observation.observation {
            NormalizedWorkObservation::InlineValidation { evidence } => evidence
                .content
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect()
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
    connection: &Connection,
    locator: &ExternalSessionLocator,
) -> Result<Option<ExternalIdentity>> {
    connection
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

/// Owning Task lineage of one persisted Working Intent revision.
struct IntentRevisionOwner {
    external_session: ExternalSessionId,
    task_session: TaskSessionId,
    task: TaskId,
    head_revision: TaskIntentRevisionId,
}

/// Marks one Git-committed Confirmation and its Review terminal inside an open transaction.
fn finalize_one_candidate_confirmation(
    transaction: &Transaction<'_>,
    request: &CandidateConfirmationFinalize,
) -> Result<CandidateConfirmationFinalizeOutcome> {
    let CandidateConfirmationFinalize {
        candidate_id,
        operation_hash,
        confirmation_id,
        result_context_id,
    } = request;
    let candidate_id = *candidate_id;
    let confirmation_id = *confirmation_id;
    let result_context_id = *result_context_id;
    let operation = read_candidate_confirmation_operation(transaction, candidate_id)?
        .ok_or_else(|| invalid("Candidate Confirmation operation is not reserved"))?;
    if operation.operation_hash != *operation_hash
        || operation.plan.confirmation.confirmation_id != confirmation_id
        || operation.plan.result_context_id != result_context_id
    {
        return Err(Error::new(
            ErrorKind::Conflict,
            "Committed Candidate Confirmation does not match the reserved operation",
        ));
    }
    if operation.status == CandidateConfirmationOperationStatus::Committed {
        let review = read_candidate_review_record(transaction, candidate_id)?
            .ok_or_else(|| invariant("confirmed Candidate Review disappeared"))?;
        if review.status != CandidateReviewStatus::Confirmed
            || review.confirmation_id != Some(confirmation_id)
            || review.result_context_id != Some(result_context_id)
        {
            return Err(invariant(
                "committed Candidate Confirmation and Review audit disagree",
            ));
        }
        if let Some(mapping) =
            read_proposed_space_group_mapping_for_candidate(transaction, candidate_id)?
            && mapping.status != ProposedSpaceGroupMappingStatus::Committed
        {
            return Err(invariant(
                "committed Candidate Confirmation has an uncommitted proposed Space group",
            ));
        }
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
                i64::try_from(operation.review_parent_version)
                    .map_err(|_| invalid("Candidate Review parent version exceeds SQLite range"))?,
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
    transaction
        .execute(
            "UPDATE proposed_space_group SET status = 'committed'
                 WHERE candidate_id = ?1 AND status = 'reserved'",
            [candidate_id.to_string()],
        )
        .map_err(sql_error("commit proposed Space group mapping"))?;
    let operation = read_candidate_confirmation_operation(transaction, candidate_id)?
        .ok_or_else(|| invariant("committed Candidate Confirmation operation disappeared"))?;
    let review = read_candidate_review_record(transaction, candidate_id)?
        .ok_or_else(|| invariant("confirmed Candidate Review disappeared"))?;
    Ok(CandidateConfirmationFinalizeOutcome {
        operation,
        review,
        already_confirmed: false,
    })
}

fn discard_one_candidate_review(
    transaction: &Transaction<'_>,
    task: &TaskSessionSnapshot,
    request: &CandidateReviewDiscard,
    now: u64,
) -> Result<CandidateReviewDiscardOutcome> {
    let reason = request.reason.trim();
    let mut record = read_candidate_review_record(transaction, request.candidate_id)?
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
    record = read_candidate_review_record(transaction, request.candidate_id)?
        .ok_or_else(|| invariant("discarded Candidate Review disappeared"))?;
    Ok(CandidateReviewDiscardOutcome {
        record,
        status: CandidateReviewDiscardStatus::Discarded,
    })
}

/// Names the exact batch member that rolled a Review decision back.
fn confirmation_scoped_error(candidate_id: CandidateId) -> impl Fn(Error) -> Error {
    move |error| {
        Error::new(
            error.kind(),
            format!(
                "{} (candidate {candidate_id}); no Candidate in this batch was finalized",
                error.message()
            ),
        )
    }
}

fn candidate_scoped_error(candidate_id: CandidateId) -> impl Fn(Error) -> Error {
    move |error| {
        Error::new(
            error.kind(),
            format!(
                "{} (candidate {candidate_id}); no Candidate in this batch was discarded",
                error.message()
            ),
        )
    }
}

fn read_intent_revision_owner(
    transaction: &Transaction<'_>,
    revision_id: TaskIntentRevisionId,
) -> Result<Option<IntentRevisionOwner>> {
    transaction
        .query_row(
            "SELECT task.external_session_id, task.task_session_id, task.task_id,
                    task.current_intent_revision_id
             FROM task_intent_revision AS revision
             JOIN task_session AS task ON task.task_session_id = revision.task_session_id
             WHERE revision.revision_id = ?1",
            [revision_id.to_string()],
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
        .map_err(sql_error("read Working Intent revision owner"))?
        .map(
            |(external_session_id, task_session_id, task_id, current_intent_revision_id)| {
                Ok(IntentRevisionOwner {
                    external_session: parse_id(
                        &external_session_id,
                        "task_session.external_session_id",
                    )?,
                    task_session: parse_id(&task_session_id, "task_session.task_session_id")?,
                    task: parse_id(&task_id, "task_session.task_id")?,
                    head_revision: parse_id(
                        &current_intent_revision_id,
                        "task_session.current_intent_revision_id",
                    )?,
                })
            },
        )
        .transpose()
}

/// Appends one successor revision and advances the owning Task Head in the same transaction.
fn append_revision_in_transaction(
    transaction: &Transaction<'_>,
    task_session_id: TaskSessionId,
    current: &TaskIntentRevision,
    working_intent: WorkingIntentSnapshot,
) -> Result<TaskIntentRevision> {
    let revision = TaskIntentRevision::successor(current, working_intent)?;
    let ordinal = read_revision_ordinal(transaction, current.revision_id)?
        .checked_add(1)
        .ok_or_else(|| invariant("Task Intent revision ordinal overflow"))?;
    insert_intent_revision(transaction, task_session_id, ordinal, &revision)?;
    let changed = transaction
        .execute(
            "UPDATE task_session SET current_intent_revision_id = ?1
             WHERE task_session_id = ?2 AND current_intent_revision_id = ?3",
            params![
                revision.revision_id.to_string(),
                task_session_id.to_string(),
                current.revision_id.to_string(),
            ],
        )
        .map_err(sql_error("advance Task Intent Head"))?;
    if changed != 1 {
        return Err(invariant(
            "Task Intent Head changed inside write transaction",
        ));
    }
    Ok(revision)
}

/// Deterministic goal comparison key used only for the concurrent-Agent fork decision.
///
/// It lowercases, keeps alphanumeric runs and collapses every other character into one separator.
/// It is not a retrieval tokenizer and never leaves this decision.
fn normalized_goal(goal: &str) -> String {
    let mut normalized = String::with_capacity(goal.len());
    let mut pending_separator = false;
    for character in goal.chars() {
        if character.is_alphanumeric() {
            if pending_separator && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_separator = false;
            normalized.extend(character.to_lowercase());
        } else {
            pending_separator = true;
        }
    }
    normalized
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
        return Err(stale(
            "Intent parent is no longer the ActiveTask current Head",
        ));
    }
    Err(stale(
        "Intent parent is stale: unknown or no longer retained",
    ))
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

#[cfg(test)]
mod hook_event_retention_tests {
    use tempfile::TempDir;

    use super::{
        Connection, HOOK_EVENT_PRUNE_INTERVAL, HOOK_EVENT_RETENTION_ROWS, HookEventDecision,
        HookEventRecord, TaskRuntime, params,
    };

    fn sample() -> HookEventRecord {
        HookEventRecord {
            recorded_at_unix_ms: 1,
            agent_kind: "codex".to_owned(),
            external_session_id: None,
            event_kind: "session_start".to_owned(),
            decision: HookEventDecision::Enabled,
            reason: "ok".to_owned(),
            duration_ms: 0,
            detail: None,
        }
    }

    /// Whitebox: seeds `sqlite_sequence` so the very next insert lands on an id that both
    /// crosses a [`HOOK_EVENT_PRUNE_INTERVAL`] boundary and clears the retention floor, so
    /// pruning is observable without inserting [`HOOK_EVENT_RETENTION_ROWS`]-plus real rows.
    #[test]
    fn record_hook_event_prunes_rows_older_than_the_retention_window() {
        let temporary = TempDir::new().unwrap();
        let runtime = TaskRuntime::initialize(temporary.path().join(".shared-context")).unwrap();

        // id = 1: old enough to be swept once the retention threshold clears zero.
        runtime.record_hook_event(&sample()).unwrap();

        let next_id =
            HOOK_EVENT_PRUNE_INTERVAL * (HOOK_EVENT_RETENTION_ROWS / HOOK_EVENT_PRUNE_INTERVAL + 2);
        assert_eq!(next_id % HOOK_EVENT_PRUNE_INTERVAL, 0);
        assert!(next_id - HOOK_EVENT_RETENTION_ROWS > 1);
        {
            let connection = Connection::open(runtime.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'hook_event'",
                    params![next_id - 1],
                )
                .unwrap();
        }

        // This insert lands on `next_id`, a prune-interval multiple whose retention threshold
        // is now positive, so the seeded id = 1 row falls below it and is swept.
        runtime.record_hook_event(&sample()).unwrap();

        let remaining = runtime.recent_hook_events(10_000).unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "the id = 1 row must be pruned; only the newest row remains"
        );
    }
}
