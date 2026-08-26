//! Stdio Model Context Protocol server for Shared Context V1 tools.
//!
//! The transport accepts the newline-delimited framing used by current MCP
//! clients and the `Content-Length` framing used by older fixtures. Read tools
//! are pinned to one projection snapshot or one explicitly named Git tree. The
//! durable write tool delegates ID generation and append-only enforcement to
//! the domain event constructor and [`sctx_git_store::GitStore`]; `task_context`
//! writes only disposable local Task Runtime state.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt, fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use sctx_domain::{
    AgentCheckpointId, Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactRef,
    AutomaticCandidateStatus, AutomaticContextCandidate, CandidateAnalysis,
    CandidateAnalysisStatus, CandidateBuilderProvenance, CandidateConfidence,
    CandidateConfirmationOperation, CandidateConfirmationPlan,
    CandidateConfirmationPrimaryReference, CandidatePrimarySelection, CandidateRelationAssessment,
    CandidateReviewDiagnostic, CandidateReviewStatus, CandidateReviewSummary, CandidateReviewView,
    CandidateSpaceRecommendation, CaptureEvidenceRef, CaptureId, CaptureUnknown, CheckpointClaim,
    CheckpointClaimId, ContextId, ContextKind, ContextRevisionDraft, ContextRevisionRef,
    EngineeringReferenceDraft, Error, ErrorKind, EvidenceSnapshotDraft, EvidenceType,
    ExternalSessionLocator, NormalizedBreadcrumbKind, NormalizedWorkObservation,
    OptionalCandidateEdits, REPOSITORY_ID_MAX_BYTES, REPOSITORY_ID_PATTERN, ReferenceId,
    ReferenceRelation, RepoRelativePath, RepositoryId, ResolutionStatus, ResolvedFocus, Result,
    RevisionId, SignalId, SpaceId, SpaceRecommendationId, SubmissionId, TaskId,
    TaskIntentRevisionId, TaskSessionId, TaskSessionSnapshot, TaskSignalKind, TaskSignalLifecycle,
    TaskSignalRecord, TaskSpaceAssociation, WorkEpisodeId, WorkEpisodeStatus, WorkObservation,
    WorkObservationId, WorkSourceRef, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    CandidateMatchEvidence, CatalogRepositorySpec, EngineeringProjectionStore,
    EngineeringReferenceResolver, MAX_REPOSITORY_SCAN_PLAN_PATHS, ProjectedEngineeringReference,
    RegisteredRepository, RepositoryAvailability, RepositoryCatalogSyncReport, RepositoryRegistry,
    RepositoryScanOutcome, RepositoryScanPlan, RepositoryScanner, ResolvedReferenceProjection,
    SkippedFileReason, build_graph_context_snapshots,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{
    AppendRequest, CandidateConfirmationWriteStatus, CandidateSubmissionRequest,
    CandidateSubmissionStatus, GitStore,
};
use sctx_index::{DomainSnapshot, ProjectionIndex};
use sctx_local_state::{
    AuthorizedSessionScope, AuthorizedSessionScopeDecision, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeStore, BreadcrumbKind, CaptureClaim, CaptureDiagnosticKind, CaptureStore,
    MaintenanceLock, PrivacyScanner, RepositoryCatalogSnapshot, UserConfigStore,
    map_capture_artifacts,
};
use sctx_search::{
    CandidateAnalysisRequest, ConflictView, ContextPackOmitted, ContextStatus,
    DEFAULT_TASK_MAX_SPACES, MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET, MAX_CANDIDATE_ANALYSIS_TOP_K,
    MAX_TASK_MAX_SPACES, MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET, MIN_TASK_CONTEXT_TOKEN_BUDGET,
    ScopeFilter, SearchEngine, SearchFilters, SearchRequest, TaskContextItem, TaskContextRequest,
    TaskGraphDiagnostic, TaskRetrievalPath,
};
use sctx_task_runtime::{
    AgentCheckpointWrite, AutomatedEpisodeBoundary, CandidateBuildItemPreparation,
    CandidateBuildItemStatus, CandidateBuildStatus, CandidateBuildView, CandidateReviewDiscard,
    CandidateReviewDiscardStatus, CandidateReviewRecord, CaptureIngestion, CheckpointBoundary,
    CheckpointClaimDraft, IntentRevisionWriteStatus, TaskRuntime, WorkEpisodeDiagnosticKind,
    WorkEpisodeView,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Protocol version advertised when a client does not provide one.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_SCAN_ARTIFACT_LIMIT: usize = 200;
const MAX_SCAN_ARTIFACT_LIMIT: usize = 1_000;
const DEFAULT_CANDIDATE_REVIEW_LIST_LIMIT: usize = 20;
const DEFAULT_CANDIDATE_REVIEW_TOKEN_BUDGET: usize = 4_096;
const MIN_CANDIDATE_REVIEW_TOKEN_BUDGET: usize = 512;
const MAX_CANDIDATE_REVIEW_TOKEN_BUDGET: usize = 32_768;
const DEFAULT_TASK_CAPTURE_LIST_LIMIT: usize = 20;
const MAX_TASK_CAPTURE_LIST_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoundary {
    Continue,
    New,
}

/// Required nullable CAS field. Unlike `Option<T>`, an omitted field fails deserialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExpectedRevisionId {
    Revision(String),
    Null(()),
}

impl ExpectedRevisionId {
    fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Revision(value) => Some(value),
            Self::Null(()) => None,
        }
    }
}

/// Lightweight Working Intent update; only the goal is required inside the snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIntentUpdateInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub task_boundary: TaskBoundary,
    pub expected_revision_id: ExpectedRevisionId,
    pub intent: WorkingIntentSnapshot,
}

/// Stable-ID Signal lifecycle update guarded by active Task and Intent revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSignalSupersedeInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub task_id: String,
    pub expected_revision_id: String,
    pub signal_ids: Vec<String>,
}

/// Explicit Episode transition requested by one Agent-authored Checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCheckpointBoundary {
    Continue,
    Close,
}

impl From<TaskCheckpointBoundary> for CheckpointBoundary {
    fn from(value: TaskCheckpointBoundary) -> Self {
        match value {
            TaskCheckpointBoundary::Continue => Self::Continue,
            TaskCheckpointBoundary::Close => Self::Close,
        }
    }
}

/// Existing or self-contained Evidence supplied for one Checkpoint Claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCheckpointEvidenceInput {
    Capture {
        capture_id: String,
    },
    Observation {
        observation_id: String,
    },
    TaskSignal {
        signal_id: String,
    },
    ContextEvidence {
        context_id: String,
        revision_id: String,
        evidence_id: String,
    },
    InlineValidation {
        evidence: EvidenceSnapshotDraft,
    },
}

/// Bounded owner-scoped view of recent Capture inputs available to one `ActiveTask`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCaptureListInput {
    pub agent_kind: String,
    pub external_session_id: String,
    #[serde(default = "default_task_capture_list_limit")]
    pub limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCaptureSummary {
    pub capture_id: CaptureId,
    pub recorded_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
    pub intent_revision_id: TaskIntentRevisionId,
    pub kind: BreadcrumbKind,
    pub summary: String,
    pub diagnostics: Vec<CaptureDiagnosticKind>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCaptureListResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub captures: Vec<TaskCaptureSummary>,
    pub truncated: bool,
}

/// Complete Claim draft without caller-owned Claim or Observation identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpointClaimInput {
    #[serde(default)]
    pub context_kind_hint: Option<ContextKind>,
    #[serde(default)]
    pub topic_key_hint: Option<String>,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    pub evidence: Vec<TaskCheckpointEvidenceInput>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub related_contexts: Vec<ContextRevisionRef>,
}

/// Strict public Agent Checkpoint request guarded by Task, Intent and Episode CAS.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpointInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub expected_task_id: String,
    pub expected_intent_revision_id: String,
    pub expected_episode_version: u64,
    pub boundary: TaskCheckpointBoundary,
    pub claims: Vec<TaskCheckpointClaimInput>,
    pub unknowns: Vec<CaptureUnknown>,
}

/// Safe, typed Checkpoint diagnostic without Agent-authored source text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCheckpointDiagnostic {
    CaptureIngested {
        capture_id: CaptureId,
        observation_id: WorkObservationId,
        inserted: bool,
    },
    InlineValidationRecorded {
        observation_id: WorkObservationId,
    },
}

/// Server-owned Checkpoint and resulting Episode boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCheckpointResponse {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_ids: Vec<CheckpointClaimId>,
    pub episode_id: WorkEpisodeId,
    pub episode_version: u64,
    pub episode_status: WorkEpisodeStatus,
    pub created: bool,
    pub diagnostics: Vec<TaskCheckpointDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_build: Option<CandidateBuildResponse>,
}

/// Aggregate state returned after deterministically building one closed Episode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateBuildResponse {
    pub build_id: sctx_domain::CandidateBuildId,
    pub episode_id: WorkEpisodeId,
    pub status: CandidateBuildResponseStatus,
    pub items: Vec<CandidateBuildItemSummary>,
}

/// Public aggregate Candidate Build status without exposing `SQLite` details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateBuildResponseStatus {
    Pending,
    Complete,
    Incomplete,
}

impl From<CandidateBuildStatus> for CandidateBuildResponseStatus {
    fn from(value: CandidateBuildStatus) -> Self {
        match value {
            CandidateBuildStatus::Pending => Self::Pending,
            CandidateBuildStatus::Complete => Self::Complete,
            CandidateBuildStatus::Incomplete => Self::Incomplete,
        }
    }
}

/// Claim-scoped Candidate result. Missing Candidate/Event IDs means no Git write occurred.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateBuildItemSummary {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub submission_id: SubmissionId,
    pub status: CandidateBuildItemResponseStatus,
    pub candidate_id: Option<sctx_domain::CandidateId>,
    pub event_id: Option<sctx_domain::EventId>,
    pub error_code: Option<String>,
    pub candidate_status: AutomaticCandidateStatus,
    pub analysis: CandidateAnalysis,
    pub confidence: CandidateConfidence,
    pub unknowns: Vec<CaptureUnknown>,
    pub space_recommendations: Vec<CandidateSpaceRecommendation>,
}

/// Internal/CLI request to recompute one Candidate's derived review state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAnalyzeInput {
    pub candidate_id: String,
    #[serde(default = "default_candidate_analysis_token_budget")]
    pub token_budget: usize,
    #[serde(default = "default_candidate_analysis_top_k")]
    pub top_k: usize,
}

/// Current Runtime-derived Candidate analysis; no field is a Git knowledge fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateAnalyzeResponse {
    pub analysis_generation: u64,
    pub candidate: AutomaticContextCandidate,
}

/// Bounded Task-local Candidate Review list request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateListInput {
    pub agent_kind: String,
    pub external_session_id: String,
    #[serde(default = "default_candidate_review_status")]
    pub status: CandidateReviewStatus,
    #[serde(default = "default_candidate_review_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_candidate_review_token_budget")]
    pub token_budget: usize,
}

/// Exact Task-local Candidate Review read request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateGetInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub candidate_id: String,
}

/// CAS-guarded explicit discard request; no confirmation fields exist.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDiscardInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub expected_task_id: String,
    pub expected_intent_revision_id: String,
    pub candidate_id: String,
    pub expected_review_version: u64,
    pub reason: String,
}

/// Why a whole Review Summary was omitted from one bounded page.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateReviewOmittedReason {
    TokenBudget,
    PayloadUnavailable,
}

/// Stable identity retained when a full Summary is omitted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateReviewOmitted {
    pub candidate_id: sctx_domain::CandidateId,
    pub reason: CandidateReviewOmittedReason,
    pub estimated_tokens: usize,
}

/// Stable page of whole untrusted Summaries; no Review content is truncated.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateListResponse {
    pub reviews: Vec<CandidateReviewSummary>,
    pub omitted: Vec<CandidateReviewOmitted>,
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    pub token_budget: usize,
}

/// Exact public discard disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDiscardResponseStatus {
    Discarded,
    AlreadyDiscarded,
}

/// Updated full Review after a discard operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateDiscardResponse {
    pub status: CandidateDiscardResponseStatus,
    pub review: CandidateReviewView,
}

/// Existing Primary Space selection accepted by `candidate_confirm`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExistingCandidatePrimaryInput {
    pub existing_space_id: String,
}

/// Server-owned proposed Space recommendation selection accepted by `candidate_confirm`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCandidatePrimaryInput {
    pub new_space_recommendation_id: String,
}

/// Exactly one existing or proposed Primary selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CandidateConfirmPrimaryInput {
    Existing(ExistingCandidatePrimaryInput),
    Proposed(NewCandidatePrimaryInput),
}

/// Strict explicit human Candidate confirmation request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub expected_task_id: String,
    pub expected_intent_revision_id: String,
    pub candidate_id: String,
    pub expected_review_version: u64,
    pub primary: CandidateConfirmPrimaryInput,
    pub related_space_ids: Vec<String>,
    #[serde(default)]
    pub edits: OptionalCandidateEdits,
}

/// Exact idempotent Candidate Confirmation result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateConfirmResponseStatus {
    Confirmed,
    AlreadyConfirmed,
}

/// Public confirmation result and explicit assessment acknowledgment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateConfirmResponse {
    pub status: CandidateConfirmResponseStatus,
    pub created: bool,
    pub candidate_id: sctx_domain::CandidateId,
    pub confirmation_id: sctx_domain::ConfirmationId,
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub primary_space_id: SpaceId,
    pub related_space_ids: Vec<SpaceId>,
    pub batch_id: sctx_git_store::BatchId,
    pub commit_oid: String,
    pub event_ids: Vec<sctx_domain::EventId>,
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub assessment_acknowledgments: Vec<CandidateRelationAssessment>,
}

/// Public Claim-scoped submission state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateBuildItemResponseStatus {
    Prepared,
    NeedsEvidence,
    Created,
    AlreadyExists,
    Failed,
}

impl From<CandidateBuildItemStatus> for CandidateBuildItemResponseStatus {
    fn from(value: CandidateBuildItemStatus) -> Self {
        match value {
            CandidateBuildItemStatus::Prepared => Self::Prepared,
            CandidateBuildItemStatus::NeedsEvidence => Self::NeedsEvidence,
            CandidateBuildItemStatus::Created => Self::Created,
            CandidateBuildItemStatus::AlreadyExists => Self::AlreadyExists,
            CandidateBuildItemStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskIntentUpdateResponse {
    #[serde(flatten)]
    pub context: TaskContextResponse,
    pub revision_status: IntentRevisionStatus,
    pub active_signals: Vec<TaskSignalRecord>,
}

/// Whether one Working Intent call created a Revision or matched current canonical semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentRevisionStatus {
    Created,
    AlreadyCurrent,
}

impl From<IntentRevisionWriteStatus> for IntentRevisionStatus {
    fn from(value: IntentRevisionWriteStatus) -> Self {
        match value {
            IntentRevisionWriteStatus::Created => Self::Created,
            IntentRevisionWriteStatus::AlreadyCurrent => Self::AlreadyCurrent,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskSignalSupersedeResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub superseded_signal_ids: Vec<SignalId>,
    pub active_signals: Vec<TaskSignalRecord>,
}

/// Client fixture selected by the stable CLI entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientKind {
    Cursor,
    Codex,
}

impl FromStr for ClientKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "cursor" => Ok(Self::Cursor),
            "codex" => Ok(Self::Codex),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("unsupported MCP client {value:?}; expected cursor or codex"),
            )),
        }
    }
}

/// Why the observable stdio session ended normally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    CleanEof,
}

/// Terminal state returned by a completed server loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServeOutcome {
    pub disconnect: DisconnectReason,
    pub requests_handled: u64,
}

/// Read-only locator and output bounds accepted by `task_context`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskContextReadInput {
    pub agent_kind: String,
    pub external_session_id: String,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default = "default_max_spaces")]
    pub max_spaces: usize,
}

impl TaskContextReadInput {
    fn locator(&self) -> Result<ExternalSessionLocator> {
        ExternalSessionLocator::new(&self.agent_kind, &self.external_session_id)
    }

    fn validate(&self) -> Result<()> {
        let _locator = self.locator()?;
        if self.token_budget < MIN_TASK_CONTEXT_TOKEN_BUDGET {
            return Err(invalid(format!(
                "task_context token_budget must be at least {MIN_TASK_CONTEXT_TOKEN_BUDGET}"
            )));
        }
        if self.max_spaces == 0 || self.max_spaces > MAX_TASK_MAX_SPACES {
            return Err(invalid(format!(
                "task_context max_spaces must be between 1 and {MAX_TASK_MAX_SPACES}"
            )));
        }
        Ok(())
    }
}

/// Required nullable string used by Symbol coordinates; omission is rejected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequiredNullableString {
    Value(String),
    Null(()),
}

impl RequiredNullableString {
    fn into_option(self) -> Option<String> {
        match self {
            Self::Value(value) => Some(value),
            Self::Null(()) => None,
        }
    }
}

/// Agent-authored kind-specific Artifact coordinates. Repository-relative path is server-owned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "locator_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactFocusQueryCoordinates {
    File,
    Module,
    Api {
        protocol: String,
        operation: String,
        normalized_route: String,
    },
    Schema {
        namespace: String,
        version: String,
        qualified_name: String,
    },
    Symbol {
        language: String,
        module: String,
        enclosing_type: RequiredNullableString,
        symbol_name: String,
        signature: String,
    },
    Test {
        qualified_test_name: String,
    },
}

impl ArtifactFocusQueryCoordinates {
    fn into_locator(self, path: RepoRelativePath) -> ArtifactLocator {
        match self {
            Self::File => ArtifactLocator::File { path },
            Self::Module => ArtifactLocator::Module { path },
            Self::Api {
                protocol,
                operation,
                normalized_route,
            } => ArtifactLocator::Api {
                path,
                protocol,
                operation,
                normalized_route,
            },
            Self::Schema {
                namespace,
                version,
                qualified_name,
            } => ArtifactLocator::Schema {
                path,
                namespace,
                version,
                qualified_name,
            },
            Self::Symbol {
                language,
                module,
                enclosing_type,
                symbol_name,
                signature,
            } => ArtifactLocator::Symbol {
                path,
                language,
                module,
                enclosing_type: enclosing_type.into_option(),
                symbol_name,
                signature,
            },
            Self::Test {
                qualified_test_name,
            } => ArtifactLocator::Test {
                path,
                qualified_test_name,
            },
        }
    }
}

/// Strict public query for resolving one Artifact Focus and immediately retrieving.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFocusQuery {
    pub agent_kind: String,
    pub external_session_id: String,
    pub expected_revision_id: String,
    pub absolute_file_path: String,
    pub locator: ArtifactFocusQueryCoordinates,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default = "default_max_spaces")]
    pub max_spaces: usize,
}

impl ArtifactFocusQuery {
    fn locator(&self) -> Result<ExternalSessionLocator> {
        ExternalSessionLocator::new(&self.agent_kind, &self.external_session_id)
    }

    fn validate_bounds(&self) -> Result<()> {
        let _locator = self.locator()?;
        if self.absolute_file_path.trim().is_empty() {
            return Err(invalid("absolute_file_path must not be empty"));
        }
        if self.token_budget < MIN_TASK_CONTEXT_TOKEN_BUDGET {
            return Err(invalid(format!(
                "task_artifact_focus token_budget must be at least {MIN_TASK_CONTEXT_TOKEN_BUDGET}"
            )));
        }
        if self.max_spaces == 0 || self.max_spaces > MAX_TASK_MAX_SPACES {
            return Err(invalid(format!(
                "task_artifact_focus max_spaces must be between 1 and {MAX_TASK_MAX_SPACES}"
            )));
        }
        Ok(())
    }
}

/// Flattened explanation index for one returned Task Context item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskContextRetrievalPaths {
    pub association_space_id: SpaceId,
    pub context_id: ContextId,
    pub paths: Vec<TaskRetrievalPath>,
}

/// Session-aware Task Context result shared by MCP and the CLI test entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContextResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub candidate_spaces: Vec<TaskSpaceAssociation>,
    pub items: Vec<TaskContextItem>,
    pub retrieval_paths: Vec<TaskContextRetrievalPaths>,
    pub graph_diagnostics: Vec<TaskGraphDiagnostic>,
    pub task_fingerprint: String,
    pub tree: String,
    pub generation: u64,
    pub artifact_generation: Option<String>,
    pub graph_context_tree_oid: Option<String>,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub omitted: Vec<ContextPackOmitted>,
}

/// Result of one request-local Focus resolution and its immediate Task Context retrieval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactFocusQueryResponse {
    pub resolved_focus: ResolvedFocus,
    pub context: TaskContextResponse,
}

/// Register-and-scan request for one canonical local Git worktree root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryScanInput {
    pub checkout_path: String,
    pub paths: Vec<String>,
    #[serde(default = "default_scan_artifact_limit")]
    pub max_artifacts: usize,
}

/// Bounded derived Artifact description. Full source is never retained or returned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub artifact_key: ArtifactKey,
    pub kind: ArtifactKind,
    pub display_name: String,
    pub locator: ArtifactLocator,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryScanResponse {
    pub repository_id: RepositoryId,
    pub canonical_name: String,
    pub checkout_path: PathBuf,
    pub status: String,
    pub repository_generation: Option<String>,
    pub artifact_generation: Option<String>,
    pub head_tree_oid: Option<String>,
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub planned_path_count: usize,
    pub artifact_count: usize,
    pub omitted_artifact_count: usize,
    pub skipped_file_count: usize,
    pub skipped_paths: Vec<SkippedPathSummary>,
    pub artifacts: Vec<ArtifactSummary>,
    pub unavailable_reason: Option<String>,
    pub tree: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkippedPathSummary {
    pub path: String,
    pub reason: String,
}

/// Persistent engineering observation input. All new identities remain server-owned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringReferenceRecordInput {
    pub context_id: String,
    pub revision_id: String,
    pub repository_id: String,
    pub artifact_kind: ArtifactKind,
    pub relation: ReferenceRelation,
    pub locator: ArtifactLocator,
    pub supports: String,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EngineeringReferenceRecordResponse {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub repository_id: RepositoryId,
    pub reference_id: ReferenceId,
    pub event_id: sctx_domain::EventId,
    pub batch_id: String,
    pub commit_oid: String,
    pub tree: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssociationExplainInput {
    pub reference_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssociationExplainResponse {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub reference_id: ReferenceId,
    pub repository_id: RepositoryId,
    pub status: ResolutionStatus,
    pub resolved_artifact: Option<ArtifactKey>,
    pub ambiguity_candidates: Vec<ArtifactKey>,
    pub evidence: Vec<CandidateMatchEvidence>,
    pub graph_paths: Vec<Vec<String>>,
    pub explanation: String,
    pub artifact_generation: String,
    pub context_tree_oid: Option<String>,
    pub tree: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssociationRebuildInput {
    #[serde(default)]
    pub diagnose_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryRebuildSummary {
    pub repository_id: RepositoryId,
    pub status: String,
    pub checkout_path: Option<PathBuf>,
    pub planned_path_count: usize,
    pub repository_generation: Option<String>,
    pub artifact_count: usize,
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolutionStatusCounts {
    pub resolved: usize,
    pub ambiguous: usize,
    pub missing: usize,
    pub unavailable: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssociationRebuildResponse {
    pub diagnose_only: bool,
    pub stored: bool,
    pub artifact_generation: String,
    pub context_tree_oid: String,
    pub reference_count: usize,
    pub repositories: Vec<RepositoryRebuildSummary>,
    pub status_counts: ResolutionStatusCounts,
    pub tree: String,
    pub generation: u64,
}

impl RepositoryScanInput {
    fn validate(&self) -> Result<()> {
        if self.checkout_path.trim().is_empty() {
            return Err(invalid("repository_scan.checkout_path must not be empty"));
        }
        if self.paths.is_empty() {
            return Err(invalid(
                "repository_scan.paths must contain at least one Repository-relative path",
            ));
        }
        if self.max_artifacts == 0 || self.max_artifacts > MAX_SCAN_ARTIFACT_LIMIT {
            return Err(invalid(format!(
                "repository_scan.max_artifacts must be between 1 and {MAX_SCAN_ARTIFACT_LIMIT}"
            )));
        }
        Ok(())
    }
}

impl EngineeringReferenceRecordInput {
    fn ids(&self) -> Result<(ContextId, RevisionId, RepositoryId)> {
        Ok((
            parse_id_value(&self.context_id, "context_id")?,
            parse_id_value(&self.revision_id, "revision_id")?,
            parse_id_value(&self.repository_id, "repository_id")?,
        ))
    }

    fn draft(&self, repository_id: RepositoryId) -> Result<EngineeringReferenceDraft> {
        if self.limitations.is_empty() {
            return Err(invalid(
                "engineering_reference_record.limitations must contain at least one limitation",
            ));
        }
        let draft = EngineeringReferenceDraft {
            repository_id,
            artifact_kind: self.artifact_kind,
            relation: self.relation,
            locator: self.locator.clone(),
            supports: self.supports.clone(),
            limitations: self.limitations.clone(),
        };
        draft.validate()?;
        Ok(draft)
    }
}

/// Typed failures for transport state that cannot be represented as a JSON-RPC response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorKind {
    Io,
    InvalidFrame,
    UnexpectedEof,
    FrameTooLarge,
}

/// User-presentable typed stdio transport error.
#[derive(Debug)]
pub struct TransportError {
    kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> TransportErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameStyle {
    Newline,
    ContentLength,
}

struct Frame {
    body: Vec<u8>,
    style: FrameStyle,
}

struct Runtime {
    root: PathBuf,
    store: GitStore,
    index: ProjectionIndex,
    repositories: RepositoryRegistry,
    engineering_graph: Option<EngineeringProjectionStore>,
    tasks: TaskRuntime,
    catalog: RepositoryCatalogSnapshot,
}

#[derive(Clone)]
struct ClaimBuildMaterial {
    checkpoint_id: AgentCheckpointId,
    claim_id: CheckpointClaimId,
    draft: Option<ContextRevisionDraft>,
    confidence: CandidateConfidence,
    unknowns: Vec<CaptureUnknown>,
    error_code: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
struct CheckpointCaptureIngestion {
    capture_id: CaptureId,
    observation_id: WorkObservationId,
    inserted: bool,
}

impl ClaimBuildMaterial {
    fn preparation(
        &self,
        source_episode: &sctx_domain::WorkEpisodeRef,
    ) -> CandidateBuildItemPreparation {
        match &self.draft {
            Some(draft) => CandidateBuildItemPreparation {
                checkpoint_id: self.checkpoint_id,
                claim_id: self.claim_id,
                content_hash: Some(sctx_domain::candidate_submission_content_hash(
                    source_episode,
                    draft,
                )),
                status: CandidateBuildItemStatus::Prepared,
                error_code: None,
            },
            None => CandidateBuildItemPreparation {
                checkpoint_id: self.checkpoint_id,
                claim_id: self.claim_id,
                content_hash: None,
                status: CandidateBuildItemStatus::NeedsEvidence,
                error_code: self.error_code.map(str::to_owned),
            },
        }
    }
}

impl Runtime {
    fn open(root: &Path) -> Result<Self> {
        let _store = GitStore::open_existing(root)?;
        let catalog = UserConfigStore::open_existing(root)?.repository_catalog_wait()?;
        Self::open_with_catalog(root, catalog)
    }

    /// Opens business state against the exact Catalog snapshot that authorized
    /// this call. Public MCP dispatch must never re-read a newer Catalog here.
    fn open_with_catalog(root: &Path, catalog: RepositoryCatalogSnapshot) -> Result<Self> {
        let base_store = GitStore::open_existing(root)?;
        let index = ProjectionIndex::for_store(&base_store);
        let store = base_store
            .with_candidate_submission_index(Arc::new(index.clone()))
            .with_candidate_confirmation_index(Arc::new(index.clone()));
        let repositories = RepositoryRegistry::initialize(root)?;
        sync_repository_catalog_snapshot(&repositories, &catalog)?;
        let engineering_graph = EngineeringProjectionStore::initialize(root).ok();
        let tasks = TaskRuntime::initialize(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            store,
            index,
            repositories,
            engineering_graph,
            tasks,
            catalog,
        })
    }

    fn snapshot(&self) -> Result<DomainSnapshot> {
        self.index.domain_snapshot()
    }

    fn task_context_readonly(&self, input: &TaskContextReadInput) -> Result<TaskContextResponse> {
        input.validate()?;
        let locator = input.locator()?;
        let snapshot = self
            .tasks
            .read_snapshot_by_locator(&locator)?
            .ok_or_else(|| {
                invalid("task_context requires task_intent_update to establish an ActiveTask")
            })?;
        build_task_context_response(
            &self.index,
            self.engineering_graph.as_ref(),
            &snapshot,
            None,
            input.token_budget,
            input.max_spaces,
        )
    }

    fn task_capture_list(&self, input: &TaskCaptureListInput) -> Result<TaskCaptureListResponse> {
        if input.limit == 0 || input.limit > MAX_TASK_CAPTURE_LIST_LIMIT {
            return Err(invalid(format!(
                "task_capture_list limit must be between 1 and {MAX_TASK_CAPTURE_LIST_LIMIT}"
            )));
        }
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let active = self
            .tasks
            .read_snapshot_by_locator(&locator)?
            .ok_or_else(|| invalid("task_capture_list requires an existing ActiveTask"))?;
        let report = CaptureStore::initialize(&self.root)?.list_unclaimed_for_task(
            &locator,
            active.task_session_id,
            active.task_id,
            input.limit,
        )?;
        let captures = report
            .captures
            .into_iter()
            .map(|capture| {
                let owner = capture
                    .record
                    .task_owner
                    .ok_or_else(|| invariant("owner-scoped Capture listing returned no owner"))?;
                Ok(TaskCaptureSummary {
                    capture_id: capture.record.capture_id,
                    recorded_at_unix_seconds: capture.record.recorded_at_unix_seconds,
                    expires_at_unix_seconds: capture.record.expires_at_unix_seconds,
                    intent_revision_id: owner.intent_revision_id,
                    kind: capture.record.kind,
                    summary: capture.record.summary,
                    diagnostics: capture.record.diagnostics,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(TaskCaptureListResponse {
            task_session_id: active.task_session_id,
            task_id: active.task_id,
            captures,
            truncated: report.truncated,
        })
    }

    fn task_artifact_focus(
        &self,
        input: &ArtifactFocusQuery,
    ) -> Result<ArtifactFocusQueryResponse> {
        input.validate_bounds()?;
        let session_locator = input.locator()?;
        let active = self
            .tasks
            .read_snapshot_by_locator(&session_locator)?
            .ok_or_else(|| {
                invalid(
                    "task_artifact_focus requires task_intent_update to establish an ActiveTask",
                )
            })?;
        require_expected_revision(&active, Some(input.expected_revision_id.as_str()))?;
        let resolved = self
            .catalog
            .resolve_declared_path(Path::new(&input.absolute_file_path))?;
        let resolved_focus = ResolvedFocus {
            repository_id: resolved.repository_id,
            locator: input.locator.clone().into_locator(resolved.relative_path),
        };
        resolved_focus.validate()?;
        let context = build_task_context_response(
            &self.index,
            self.engineering_graph.as_ref(),
            &active,
            Some(resolved_focus.clone()),
            input.token_budget,
            input.max_spaces,
        )?;
        Ok(ArtifactFocusQueryResponse {
            resolved_focus,
            context,
        })
    }

    fn task_intent_update(
        &self,
        input: &TaskIntentUpdateInput,
    ) -> Result<TaskIntentUpdateResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let active = self.tasks.read_snapshot_by_locator(&locator)?;
        input.intent.validate()?;
        let (snapshot, revision_status) = match input.task_boundary {
            TaskBoundary::Continue => {
                let active = active.ok_or_else(|| {
                    invalid("task_boundary=continue requires an existing ActiveTask")
                })?;
                let parent = input
                    .expected_revision_id
                    .as_deref()
                    .ok_or_else(|| {
                        invalid(
                            "expected_revision_id must be non-null when continuing an ActiveTask",
                        )
                    })?
                    .parse::<TaskIntentRevisionId>()
                    .map_err(|error| invalid(format!("invalid expected_revision_id: {error}")))?;
                let outcome = self.tasks.append_intent_revision(
                    active.task_session_id,
                    parent,
                    input.intent.clone(),
                )?;
                (
                    self.tasks
                        .read_snapshot(active.task_session_id)?
                        .ok_or_else(|| invariant("updated ActiveTask disappeared"))?,
                    outcome.status.into(),
                )
            }
            TaskBoundary::New => {
                if let Some(active) = active {
                    require_expected_revision(&active, input.expected_revision_id.as_deref())?;
                    (
                        self.tasks
                            .start_new_task(&locator, active.task_id, &input.intent, Vec::new())?
                            .snapshot,
                        IntentRevisionStatus::Created,
                    )
                } else {
                    if input.expected_revision_id.as_deref().is_some() {
                        return Err(invalid(
                            "expected_revision_id must be null when no ExternalSession exists",
                        ));
                    }
                    let task_id = TaskId::new();
                    let outcome = self.tasks.open_or_create(
                        locator,
                        task_id,
                        input.intent.clone(),
                        Vec::new(),
                    )?;
                    (
                        outcome.snapshot,
                        if outcome.created {
                            IntentRevisionStatus::Created
                        } else {
                            IntentRevisionStatus::AlreadyCurrent
                        },
                    )
                }
            }
        };
        let context = build_task_context_response(
            &self.index,
            self.engineering_graph.as_ref(),
            &snapshot,
            None,
            default_token_budget(),
            default_max_spaces(),
        )?;
        Ok(TaskIntentUpdateResponse {
            active_signals: active_signal_records(&self.tasks, snapshot.task_session_id)?,
            context,
            revision_status,
        })
    }

    fn task_signal_supersede(
        &self,
        input: &TaskSignalSupersedeInput,
    ) -> Result<TaskSignalSupersedeResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let active = self
            .tasks
            .read_snapshot_by_locator(&locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask"))?;
        let task_id = input
            .task_id
            .parse::<TaskId>()
            .map_err(|error| invalid(format!("invalid task_id: {error}")))?;
        if task_id != active.task_id {
            return Err(invalid("task_id does not identify the ActiveTask"));
        }
        require_expected_revision(&active, Some(&input.expected_revision_id))?;
        let signal_ids = input
            .signal_ids
            .iter()
            .map(|value| {
                value
                    .parse::<SignalId>()
                    .map_err(|error| invalid(format!("invalid signal_id: {error}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let outcome =
            self.tasks
                .supersede_signals(active.task_session_id, active.task_id, signal_ids)?;
        let revision_id = outcome
            .snapshot
            .current_intent_revision()
            .ok_or_else(|| invariant("ActiveTask has no Intent Head"))?
            .revision_id;
        Ok(TaskSignalSupersedeResponse {
            task_session_id: active.task_session_id,
            task_id: active.task_id,
            intent_revision_id: revision_id,
            superseded_signal_ids: outcome.superseded_signal_ids,
            active_signals: active_signal_records(&self.tasks, active.task_session_id)?,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn task_checkpoint(&self, input: &TaskCheckpointInput) -> Result<TaskCheckpointResponse> {
        let input_json = serde_json::to_string(input).map_err(|error| {
            invalid(format!(
                "serialize task_checkpoint privacy boundary: {error}"
            ))
        })?;
        let privacy = PrivacyScanner::default().scan(&input_json)?;
        if !privacy.is_clean() {
            return Err(Error::new(
                ErrorKind::PrivacyRejected,
                format!(
                    "task_checkpoint rejected Secret/PII categories: {}",
                    privacy.diagnostic_codes().join(",")
                ),
            ));
        }
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let expected_task_id = parse_id_value(&input.expected_task_id, "expected_task_id")?;
        let expected_intent_revision_id = parse_id_value(
            &input.expected_intent_revision_id,
            "expected_intent_revision_id",
        )?;
        let snapshot = self.snapshot()?;
        if checkpoint_contains_capture(input) {
            self.validate_checkpoint_claim_inputs(input, &snapshot)?;
        }
        let (capture_observations, effective_episode_version, capture_diagnostics) = self
            .ingest_checkpoint_captures(
                input,
                &locator,
                expected_task_id,
                expected_intent_revision_id,
            )?;
        let mut claims = Vec::with_capacity(input.claims.len());
        for claim in &input.claims {
            let mut evidence_refs = Vec::new();
            let mut inline_validations = Vec::new();
            for evidence in &claim.evidence {
                match evidence {
                    TaskCheckpointEvidenceInput::Capture { capture_id } => {
                        let capture_id = parse_id_value::<CaptureId>(capture_id, "capture_id")?;
                        let observation_id = capture_observations
                            .get(&capture_id)
                            .copied()
                            .ok_or_else(|| invariant("Checkpoint Capture was not ingested"))?;
                        evidence_refs.push(CaptureEvidenceRef::Observation { observation_id });
                    }
                    TaskCheckpointEvidenceInput::Observation { observation_id } => {
                        evidence_refs.push(CaptureEvidenceRef::Observation {
                            observation_id: parse_id_value(observation_id, "observation_id")?,
                        });
                    }
                    TaskCheckpointEvidenceInput::TaskSignal { signal_id } => {
                        evidence_refs.push(CaptureEvidenceRef::TaskSignal {
                            signal_id: parse_id_value(signal_id, "signal_id")?,
                        });
                    }
                    TaskCheckpointEvidenceInput::ContextEvidence {
                        context_id,
                        revision_id,
                        evidence_id,
                    } => {
                        let reference = CaptureEvidenceRef::ContextEvidence {
                            context_id: parse_id_value(context_id, "context_id")?,
                            revision_id: parse_id_value(revision_id, "revision_id")?,
                            evidence_id: parse_id_value(evidence_id, "evidence_id")?,
                        };
                        validate_context_evidence_ref(&snapshot, &reference)?;
                        evidence_refs.push(reference);
                    }
                    TaskCheckpointEvidenceInput::InlineValidation { evidence } => {
                        evidence.validate("task_checkpoint.inline_validation")?;
                        inline_validations.push(evidence.clone());
                    }
                }
            }
            for artifact in &claim.artifact_refs {
                artifact.locator.validate()?;
                if !self
                    .catalog
                    .repositories
                    .iter()
                    .any(|entry| entry.repository_id == artifact.repository_id)
                {
                    return Err(invalid(format!(
                        "Checkpoint Artifact Repository does not exist: {}",
                        artifact.repository_id
                    )));
                }
            }
            for context in &claim.related_contexts {
                validate_context_revision_ref(&snapshot, *context)?;
            }
            claims.push(CheckpointClaimDraft {
                context_kind_hint: claim.context_kind_hint,
                topic_key_hint: claim.topic_key_hint.clone(),
                statement: claim.statement.clone(),
                rationale: claim.rationale.clone(),
                applicability: claim.applicability.clone(),
                assumptions: claim.assumptions.clone(),
                recheck_when: claim.recheck_when.clone(),
                evidence_refs,
                inline_validations,
                artifact_refs: claim.artifact_refs.clone(),
                related_contexts: claim.related_contexts.clone(),
            });
        }
        if input.boundary == TaskCheckpointBoundary::Close
            && claims.is_empty()
            && input.unknowns.is_empty()
        {
            let boundary = self.tasks.close_checkpointed_work_episode_cas(
                &locator,
                expected_task_id,
                expected_intent_revision_id,
                input.expected_episode_version,
            )?;
            let AutomatedEpisodeBoundary::Closed { episode, .. } = boundary else {
                return Err(invalid(
                    "empty close requires an existing current-Intent Agent Checkpoint",
                ));
            };
            let checkpoint = episode
                .checkpoints
                .last()
                .ok_or_else(|| invariant("closed Episode lacks its final Agent Checkpoint"))?;
            let candidate_build = Some(self.build_closed_episode(episode.episode.episode_id)?);
            return Ok(TaskCheckpointResponse {
                checkpoint_id: checkpoint.checkpoint_id,
                claim_ids: checkpoint
                    .claims
                    .iter()
                    .map(|claim| claim.claim_id)
                    .collect(),
                episode_id: episode.episode.episode_id,
                episode_version: episode.episode.version,
                episode_status: episode.episode.status,
                created: false,
                diagnostics: Vec::new(),
                candidate_build,
            });
        }
        let outcome = self.tasks.write_agent_checkpoint(&AgentCheckpointWrite {
            locator,
            expected_task_id,
            expected_intent_revision_id,
            expected_episode_version: effective_episode_version,
            boundary: input.boundary.into(),
            claims,
            unknowns: input.unknowns.clone(),
        })?;
        let candidate_build = if input.boundary == TaskCheckpointBoundary::Close {
            Some(self.build_closed_episode(outcome.episode.episode.episode_id)?)
        } else {
            None
        };
        Ok(TaskCheckpointResponse {
            checkpoint_id: outcome.checkpoint.checkpoint_id,
            claim_ids: outcome
                .checkpoint
                .claims
                .iter()
                .map(|claim| claim.claim_id)
                .collect(),
            episode_id: outcome.episode.episode.episode_id,
            episode_version: outcome.episode.episode.version,
            episode_status: outcome.episode.episode.status,
            created: outcome.created,
            diagnostics: capture_diagnostics
                .into_iter()
                .map(|capture| TaskCheckpointDiagnostic::CaptureIngested {
                    capture_id: capture.capture_id,
                    observation_id: capture.observation_id,
                    inserted: capture.inserted,
                })
                .chain(
                    outcome
                        .inline_observation_ids
                        .into_iter()
                        .map(
                            |observation_id| TaskCheckpointDiagnostic::InlineValidationRecorded {
                                observation_id,
                            },
                        ),
                )
                .collect(),
            candidate_build,
        })
    }

    fn validate_checkpoint_claim_inputs(
        &self,
        input: &TaskCheckpointInput,
        snapshot: &DomainSnapshot,
    ) -> Result<()> {
        for claim in &input.claims {
            if claim.evidence.is_empty() {
                return Err(invalid("Checkpoint Claim Evidence must not be empty"));
            }
            let mut evidence_keys = BTreeSet::new();
            for evidence in &claim.evidence {
                let evidence_key = serde_json::to_string(evidence).map_err(|error| {
                    Error::new(
                        ErrorKind::Io,
                        format!("serialize Checkpoint Evidence identity: {error}"),
                    )
                })?;
                if !evidence_keys.insert(evidence_key) {
                    return Err(invalid("Checkpoint Claim Evidence must be unique"));
                }
                match evidence {
                    TaskCheckpointEvidenceInput::Capture { capture_id } => {
                        let _capture_id = parse_id_value::<CaptureId>(capture_id, "capture_id")?;
                    }
                    TaskCheckpointEvidenceInput::Observation { observation_id } => {
                        let _observation_id =
                            parse_id_value::<WorkObservationId>(observation_id, "observation_id")?;
                    }
                    TaskCheckpointEvidenceInput::TaskSignal { signal_id } => {
                        let _signal_id = parse_id_value::<SignalId>(signal_id, "signal_id")?;
                    }
                    TaskCheckpointEvidenceInput::ContextEvidence {
                        context_id,
                        revision_id,
                        evidence_id,
                    } => {
                        let reference = CaptureEvidenceRef::ContextEvidence {
                            context_id: parse_id_value(context_id, "context_id")?,
                            revision_id: parse_id_value(revision_id, "revision_id")?,
                            evidence_id: parse_id_value(evidence_id, "evidence_id")?,
                        };
                        validate_context_evidence_ref(snapshot, &reference)?;
                    }
                    TaskCheckpointEvidenceInput::InlineValidation { evidence } => {
                        evidence.validate("task_checkpoint.inline_validation")?;
                    }
                }
            }
            for artifact in &claim.artifact_refs {
                artifact.locator.validate()?;
                if !self
                    .catalog
                    .repositories
                    .iter()
                    .any(|entry| entry.repository_id == artifact.repository_id)
                {
                    return Err(invalid(format!(
                        "Checkpoint Artifact Repository does not exist: {}",
                        artifact.repository_id
                    )));
                }
            }
            for context in &claim.related_contexts {
                validate_context_revision_ref(snapshot, *context)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_checkpoint_captures(
        &self,
        input: &TaskCheckpointInput,
        locator: &ExternalSessionLocator,
        expected_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
    ) -> Result<(
        BTreeMap<CaptureId, WorkObservationId>,
        u64,
        Vec<CheckpointCaptureIngestion>,
    )> {
        let mut capture_ids = Vec::new();
        let mut seen = HashSet::new();
        for evidence in input.claims.iter().flat_map(|claim| &claim.evidence) {
            if let TaskCheckpointEvidenceInput::Capture { capture_id } = evidence {
                let capture_id = parse_id_value::<CaptureId>(capture_id, "capture_id")?;
                if seen.insert(capture_id) {
                    capture_ids.push(capture_id);
                }
            }
        }
        if capture_ids.is_empty() {
            return Ok((BTreeMap::new(), input.expected_episode_version, Vec::new()));
        }
        let captures = CaptureStore::initialize(&self.root)?;
        let active = self
            .tasks
            .read_snapshot_by_locator(locator)?
            .ok_or_else(|| invalid("Checkpoint Capture target is unavailable"))?;
        if active.task_id != expected_task_id
            || active
                .current_intent_revision()
                .map(|value| value.revision_id)
                != Some(expected_intent_revision_id)
        {
            return Err(invalid("Checkpoint Capture target is unavailable"));
        }

        let mut records = Vec::with_capacity(capture_ids.len());
        let mut claimed_episode_id = None;
        for capture_id in &capture_ids {
            let capture = captures.read(*capture_id)?;
            let owner = capture
                .record
                .task_owner
                .ok_or_else(|| invalid("Checkpoint Capture target is unavailable"))?;
            if capture.expired
                || capture.record.external_session_locator != *locator
                || owner.task_session_id != active.task_session_id
                || owner.task_id != active.task_id
            {
                return Err(invalid("Checkpoint Capture target is unavailable"));
            }
            if let Some(claim) = capture.record.claim {
                if claim.task_session_id != active.task_session_id
                    || claim.task_id != active.task_id
                {
                    return Err(invalid("Checkpoint Capture target is unavailable"));
                }
                match claimed_episode_id {
                    None => claimed_episode_id = Some(claim.episode_id),
                    Some(existing) if existing == claim.episode_id => {}
                    Some(_) => {
                        return Err(invalid(
                            "Checkpoint Captures are claimed by different Work Episodes",
                        ));
                    }
                }
            }
            records.push(capture.record);
        }

        let episode = if let Some(episode_id) = claimed_episode_id {
            self.tasks
                .read_work_episode(episode_id)?
                .ok_or_else(|| invalid("Checkpoint Capture Work Episode is unavailable"))?
        } else {
            let opened = self.tasks.open_work_episode(
                locator,
                expected_task_id,
                expected_intent_revision_id,
            )?;
            if opened.episode.episode.version != input.expected_episode_version {
                return Err(Error::new(
                    ErrorKind::StaleState,
                    "expected Work Episode version is stale",
                ));
            }
            opened.episode
        };
        if episode.episode.task_session_id != active.task_session_id
            || episode.episode.task_id != active.task_id
        {
            return Err(invariant("Checkpoint Capture Episode ownership changed"));
        }
        let claim = CaptureClaim {
            episode_id: episode.episode.episode_id,
            task_session_id: active.task_session_id,
            task_id: active.task_id,
        };
        let mut observations = BTreeMap::new();
        let mut diagnostics = Vec::with_capacity(records.len());
        for (offset, record) in records.into_iter().enumerate() {
            let claimed = captures.claim(record.capture_id, claim)?.record;
            let owner = claimed
                .task_owner
                .ok_or_else(|| invariant("claimed Capture lost its exact owner"))?;
            let mapping = map_capture_artifacts(&claimed, &self.catalog);
            let expected_episode_version = input
                .expected_episode_version
                .checked_add(
                    u64::try_from(offset)
                        .map_err(|_| invalid("Capture count exceeds Episode version bounds"))?,
                )
                .ok_or_else(|| invalid("Capture ingestion overflows Episode version"))?;
            let outcome = self.tasks.ingest_capture(&CaptureIngestion {
                capture_id: claimed.capture_id,
                episode_id: episode.episode.episode_id,
                expected_episode_version,
                task_session_id: active.task_session_id,
                task_id: active.task_id,
                intent_revision_id: owner.intent_revision_id,
                additional_sources: mapping
                    .artifact_refs
                    .into_iter()
                    .map(WorkSourceRef::Artifact)
                    .collect(),
                observation: NormalizedWorkObservation::Breadcrumb {
                    category: normalized_capture_kind(claimed.kind),
                    summary: claimed.summary,
                },
                diagnostics: capture_runtime_diagnostics(&mapping.diagnostics),
            })?;
            observations.insert(claimed.capture_id, outcome.observation_id);
            diagnostics.push(CheckpointCaptureIngestion {
                capture_id: claimed.capture_id,
                observation_id: outcome.observation_id,
                inserted: outcome.inserted,
            });
        }
        let effective_episode_version = input
            .expected_episode_version
            .checked_add(
                u64::try_from(capture_ids.len())
                    .map_err(|_| invalid("Capture count exceeds Episode version bounds"))?,
            )
            .ok_or_else(|| invalid("Capture ingestion overflows Episode version"))?;
        Ok((observations, effective_episode_version, diagnostics))
    }

    #[allow(clippy::too_many_lines)]
    fn build_closed_episode(&self, episode_id: WorkEpisodeId) -> Result<CandidateBuildResponse> {
        let episode = self
            .tasks
            .read_work_episode(episode_id)?
            .ok_or_else(|| invalid("Candidate Builder source Work Episode does not exist"))?;
        let WorkEpisodeStatus::Closed {
            final_checkpoint_id,
        } = episode.episode.status
        else {
            return Err(invalid(
                "Candidate Builder source Work Episode must be closed",
            ));
        };
        let final_checkpoint = episode
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == final_checkpoint_id)
            .ok_or_else(|| invariant("closed Episode final Checkpoint disappeared"))?;
        let claim_count = episode
            .checkpoints
            .iter()
            .map(|checkpoint| checkpoint.claims.len())
            .sum::<usize>();
        let materials = if claim_count == 0 {
            Vec::new()
        } else {
            let snapshot = self.snapshot()?;
            let signals = self
                .tasks
                .read_signal_history(episode.episode.task_session_id)?;
            episode
                .checkpoints
                .iter()
                .flat_map(|checkpoint| {
                    checkpoint.claims.iter().map(|claim| {
                        build_claim_material(
                            &episode,
                            final_checkpoint,
                            checkpoint,
                            claim,
                            &signals,
                            &snapshot,
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        let preparations = materials
            .iter()
            .map(|material| material.preparation(&episode.episode.ownership()))
            .collect::<Vec<_>>();
        let mut build = self
            .tasks
            .prepare_candidate_build(episode_id, &preparations)?;
        let materials = materials
            .iter()
            .map(|material| (material.claim_id, material))
            .collect::<BTreeMap<_, _>>();
        for item in build.items.clone() {
            if item.status.is_finalized() {
                if let Some(candidate_id) = item.candidate_id
                    && self.tasks.read_candidate_analysis(candidate_id)?.is_none()
                {
                    self.analyze_candidate(&CandidateAnalyzeInput {
                        candidate_id: candidate_id.to_string(),
                        token_budget: default_candidate_analysis_token_budget(),
                        top_k: default_candidate_analysis_top_k(),
                    })?;
                }
                continue;
            }
            if item.status == CandidateBuildItemStatus::NeedsEvidence {
                continue;
            }
            let material = materials
                .get(&item.claim_id)
                .ok_or_else(|| invariant("Candidate Build material disappeared"))?;
            let draft = material
                .draft
                .as_ref()
                .ok_or_else(|| invariant("prepared Candidate Build item lacks a draft"))?;
            match self.store.submit_candidate(CandidateSubmissionRequest {
                submission_id: item.submission_id,
                source_episode: episode.episode.ownership(),
                content: draft.clone(),
            }) {
                Ok(outcome) => {
                    let submission_status = match outcome.status {
                        CandidateSubmissionStatus::Created => CandidateBuildItemStatus::Created,
                        CandidateSubmissionStatus::AlreadyExists => {
                            CandidateBuildItemStatus::AlreadyExists
                        }
                    };
                    build = self.tasks.record_candidate_build_item_result(
                        build.build_id,
                        item.submission_id,
                        submission_status,
                        Some(outcome.record.candidate_id),
                        Some(outcome.record.event_id),
                        None,
                    )?;
                    self.analyze_candidate(&CandidateAnalyzeInput {
                        candidate_id: outcome.record.candidate_id.to_string(),
                        token_budget: default_candidate_analysis_token_budget(),
                        top_k: default_candidate_analysis_top_k(),
                    })?;
                }
                Err(error) => {
                    let error_code = candidate_builder_error_code(&error);
                    build = self.tasks.record_candidate_build_item_result(
                        build.build_id,
                        item.submission_id,
                        CandidateBuildItemStatus::Failed,
                        None,
                        None,
                        Some(error_code),
                    )?;
                }
            }
        }
        let mut response = candidate_build_response(build, &materials)?;
        for item in &mut response.items {
            let Some(candidate_id) = item.candidate_id else {
                continue;
            };
            if let Some(view) = self.tasks.read_candidate_analysis(candidate_id)? {
                item.candidate_status = view.candidate.status;
                item.analysis = view.candidate.analysis;
                item.confidence = view.candidate.confidence;
                item.unknowns = view.candidate.unknowns;
                item.space_recommendations = view.candidate.space_recommendations;
            }
        }
        Ok(response)
    }

    #[allow(clippy::too_many_lines)]
    fn analyze_candidate(&self, input: &CandidateAnalyzeInput) -> Result<CandidateAnalyzeResponse> {
        if input.token_budget < MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET
            || input.token_budget > MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET
            || input.top_k == 0
            || input.top_k > MAX_CANDIDATE_ANALYSIS_TOP_K
        {
            return Err(invalid("Candidate analysis bounds are unsafe"));
        }
        let candidate_id = parse_id_value(&input.candidate_id, "candidate_id")?;
        let snapshot = self.snapshot()?;
        let persisted = snapshot
            .projection
            .candidates
            .get(&candidate_id)
            .map(|projection| projection.candidate.clone())
            .ok_or_else(|| invalid(format!("Candidate does not exist: {candidate_id}")))?;
        let episode = self
            .tasks
            .read_work_episode(persisted.source_episode.episode_id)?
            .ok_or_else(|| invalid("Candidate source Work Episode does not exist"))?;
        if episode.episode.ownership() != persisted.source_episode {
            return Err(invariant("Candidate source Episode ownership changed"));
        }
        let build = self
            .tasks
            .read_candidate_build(episode.episode.episode_id)?
            .ok_or_else(|| invalid("Candidate has no Builder provenance"))?;
        let item = build
            .items
            .iter()
            .find(|item| item.candidate_id == Some(candidate_id))
            .ok_or_else(|| invalid("Candidate is not a finalized Builder item"))?;
        let checkpoint = episode
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == item.checkpoint_id)
            .ok_or_else(|| invariant("Candidate source Checkpoint disappeared"))?;
        let claim = checkpoint
            .claims
            .iter()
            .find(|claim| claim.claim_id == item.claim_id)
            .ok_or_else(|| invariant("Candidate source Claim disappeared"))?;
        let final_checkpoint = episode
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == build.final_checkpoint_id)
            .ok_or_else(|| invariant("Candidate final Checkpoint disappeared"))?;
        let task = self
            .tasks
            .read_snapshot(episode.episode.task_session_id)?
            .ok_or_else(|| invalid("Candidate source Task disappeared"))?;
        let source_intent_id = episode.episode.intent_revisions.last();
        let source_intent = task
            .intent_revisions
            .iter()
            .find(|revision| revision.revision_id == source_intent_id)
            .map(|revision| revision.working_intent.clone())
            .ok_or_else(|| invariant("Candidate source Intent revision disappeared"))?;
        let signal_history = self
            .tasks
            .read_signal_history(episode.episode.task_session_id)?;
        let material = build_claim_material(
            &episode,
            final_checkpoint,
            checkpoint,
            claim,
            &signal_history,
            &snapshot,
        );
        let engine = self.engineering_graph.as_ref().map_or_else(
            || SearchEngine::new(self.index.clone()),
            |graph| SearchEngine::with_engineering_graph(self.index.clone(), graph.clone()),
        );
        let derived = engine.analyze_candidate(&CandidateAnalysisRequest {
            candidate: persisted.clone(),
            source_task_id: task.task_id,
            source_working_intent: source_intent,
            source_task_signals: task.task_signals.clone(),
            explicit_related_contexts: claim.related_contexts.clone(),
            artifact_refs: claim.artifact_refs.clone(),
            token_budget: input.token_budget,
            top_k: input.top_k,
        });
        let mut checkpoint_ids = vec![item.checkpoint_id];
        if !checkpoint_ids.contains(&build.final_checkpoint_id) {
            checkpoint_ids.push(build.final_checkpoint_id);
        }
        let observation_ids = claim
            .evidence_refs
            .iter()
            .filter_map(|evidence| match evidence {
                CaptureEvidenceRef::Observation { observation_id } => Some(*observation_id),
                CaptureEvidenceRef::TaskSignal { .. }
                | CaptureEvidenceRef::ContextEvidence { .. } => None,
            })
            .collect::<Vec<_>>();
        let provenance = CandidateBuilderProvenance {
            build_id: build.build_id,
            source_episode: episode.episode.ownership(),
            checkpoint_ids,
            observation_ids,
        };
        let unknowns = material.unknowns;
        let (analysis, recommendations, confidence, mut status) = match derived {
            Ok(result) => (
                result.analysis,
                result.space_recommendations,
                result.confidence,
                result.candidate_status,
            ),
            Err(error) => (
                CandidateAnalysis {
                    status: CandidateAnalysisStatus::Failed,
                    error_code: Some(candidate_analysis_error_code(&error).to_owned()),
                    ..CandidateAnalysis::default()
                },
                Vec::new(),
                CandidateConfidence {
                    basis_points: 0,
                    rationale: "Candidate analysis failed and remains retryable".to_owned(),
                },
                AutomaticCandidateStatus::Draft,
            ),
        };
        if unknowns.iter().any(|unknown| unknown.blocking)
            && status == AutomaticCandidateStatus::ReadyForReview
        {
            status = AutomaticCandidateStatus::NeedsEvidence;
        }
        let candidate = AutomaticContextCandidate::from_persisted_candidate(
            &persisted,
            &episode.episode,
            &episode.checkpoints,
            provenance,
            analysis,
            recommendations,
            confidence,
            unknowns,
            status,
        )?;
        let view = self.tasks.replace_candidate_analysis(&candidate)?;
        Ok(CandidateAnalyzeResponse {
            analysis_generation: view.analysis_generation,
            candidate: view.candidate,
        })
    }

    fn candidate_list(&self, input: &CandidateListInput) -> Result<CandidateListResponse> {
        if input.token_budget < MIN_CANDIDATE_REVIEW_TOKEN_BUDGET
            || input.token_budget > MAX_CANDIDATE_REVIEW_TOKEN_BUDGET
        {
            return Err(invalid(
                "Candidate Review token budget is outside the safe bound",
            ));
        }
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let page = self.tasks.list_candidate_reviews(
            &locator,
            input.status,
            input.limit,
            input.cursor.as_deref(),
        )?;
        let snapshot = self.snapshot()?;
        let mut reviews = Vec::new();
        let mut omitted = Vec::new();
        let mut estimated_tokens = 0_usize;
        for record in page.records {
            if !snapshot
                .projection
                .candidates
                .contains_key(&record.candidate_id)
            {
                omitted.push(CandidateReviewOmitted {
                    candidate_id: record.candidate_id,
                    reason: CandidateReviewOmittedReason::PayloadUnavailable,
                    estimated_tokens: 0,
                });
                continue;
            }
            let summary =
                CandidateReviewSummary::from(self.candidate_review_view(&record, &snapshot)?);
            let tokens = estimate_candidate_review_tokens(&summary)?;
            if estimated_tokens.saturating_add(tokens) > input.token_budget {
                omitted.push(CandidateReviewOmitted {
                    candidate_id: record.candidate_id,
                    reason: CandidateReviewOmittedReason::TokenBudget,
                    estimated_tokens: tokens,
                });
            } else {
                estimated_tokens = estimated_tokens.saturating_add(tokens);
                reviews.push(summary);
            }
        }
        Ok(CandidateListResponse {
            reviews,
            omitted,
            next_cursor: page.next_cursor,
            estimated_tokens,
            token_budget: input.token_budget,
        })
    }

    fn candidate_get(&self, input: &CandidateGetInput) -> Result<CandidateReviewView> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let candidate_id = parse_id_value(&input.candidate_id, "candidate_id")?;
        let record = self
            .tasks
            .read_candidate_review(&locator, candidate_id)?
            .ok_or_else(|| invalid("Candidate Review does not exist for the ActiveTask"))?;
        let snapshot = self.snapshot()?;
        self.candidate_review_view(&record, &snapshot)
    }

    fn candidate_discard(&self, input: &CandidateDiscardInput) -> Result<CandidateDiscardResponse> {
        let scan = PrivacyScanner::default().scan(&input.reason)?;
        if !scan.is_clean() {
            return Err(Error::new(
                ErrorKind::PrivacyRejected,
                "Candidate discard reason failed the privacy boundary",
            ));
        }
        let outcome = self
            .tasks
            .discard_candidate_review(&CandidateReviewDiscard {
                locator: ExternalSessionLocator::new(
                    &input.agent_kind,
                    &input.external_session_id,
                )?,
                expected_task_id: parse_id_value(&input.expected_task_id, "expected_task_id")?,
                expected_intent_revision_id: parse_id_value(
                    &input.expected_intent_revision_id,
                    "expected_intent_revision_id",
                )?,
                candidate_id: parse_id_value(&input.candidate_id, "candidate_id")?,
                expected_review_version: input.expected_review_version,
                reason: input.reason.clone(),
            })?;
        let snapshot = self.snapshot()?;
        let review = self.candidate_review_view(&outcome.record, &snapshot)?;
        Ok(CandidateDiscardResponse {
            status: match outcome.status {
                CandidateReviewDiscardStatus::Discarded => {
                    CandidateDiscardResponseStatus::Discarded
                }
                CandidateReviewDiscardStatus::AlreadyDiscarded => {
                    CandidateDiscardResponseStatus::AlreadyDiscarded
                }
            },
            review,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn candidate_confirm(&self, input: &CandidateConfirmInput) -> Result<CandidateConfirmResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let expected_task_id = parse_id_value(&input.expected_task_id, "expected_task_id")?;
        let expected_intent_revision_id = parse_id_value(
            &input.expected_intent_revision_id,
            "expected_intent_revision_id",
        )?;
        let candidate_id = parse_id_value(&input.candidate_id, "candidate_id")?;
        let review_record = self
            .tasks
            .read_candidate_review(&locator, candidate_id)?
            .ok_or_else(|| invalid("Candidate Review does not exist for the ActiveTask"))?;
        if !matches!(
            review_record.status,
            CandidateReviewStatus::Pending | CandidateReviewStatus::Confirmed
        ) {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Only an owned Pending Candidate Review can be confirmed",
            ));
        }
        let snapshot = self.snapshot()?;
        let review = self.candidate_review_view(&review_record, &snapshot)?;
        if review.analysis.status != CandidateAnalysisStatus::Complete
            || review.analysis_generation.is_none()
            || matches!(
                review.candidate_status,
                AutomaticCandidateStatus::Draft | AutomaticCandidateStatus::NeedsEvidence
            )
        {
            return Err(invalid(
                "Candidate Confirmation requires complete current analysis and reviewable Evidence",
            ));
        }
        let (primary_reference, resolved_primary, primary_space_id) = match &input.primary {
            CandidateConfirmPrimaryInput::Existing(existing) => {
                let space_id = parse_id_value(&existing.existing_space_id, "existing_space_id")?;
                if !snapshot.projection.spaces.contains_key(&space_id) {
                    return Err(invalid(
                        "Candidate Confirmation Primary Space does not exist",
                    ));
                }
                (
                    CandidateConfirmationPrimaryReference::ExistingSpace { space_id },
                    CandidatePrimarySelection::Existing { space_id },
                    Some(space_id),
                )
            }
            CandidateConfirmPrimaryInput::Proposed(proposed) => {
                let recommendation_id = parse_id_value::<SpaceRecommendationId>(
                    &proposed.new_space_recommendation_id,
                    "new_space_recommendation_id",
                )?;
                let intent = review
                    .space_recommendations
                    .iter()
                    .find_map(|recommendation| match recommendation {
                        CandidateSpaceRecommendation::ProposedNewSpaceIntent {
                            recommendation_id: actual,
                            proposed_new_space_intent,
                            ..
                        } if *actual == recommendation_id => {
                            Some(proposed_new_space_intent.clone())
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        invalid(
                            "new_space_recommendation_id is not an exact current proposed recommendation",
                        )
                    })?;
                (
                    CandidateConfirmationPrimaryReference::ProposedRecommendation {
                        recommendation_id,
                    },
                    CandidatePrimarySelection::ProposedNew { intent },
                    None,
                )
            }
        };
        let related_space_ids = input
            .related_space_ids
            .iter()
            .map(|value| parse_id_value::<SpaceId>(value, "related_space_ids"))
            .collect::<Result<Vec<_>>>()?;
        if related_space_ids.iter().collect::<BTreeSet<_>>().len() != related_space_ids.len() {
            return Err(invalid("related_space_ids must be unique"));
        }
        if primary_space_id.is_some_and(|primary| related_space_ids.contains(&primary)) {
            return Err(invalid("Related Spaces must not include Primary"));
        }
        if related_space_ids
            .iter()
            .any(|space_id| !snapshot.projection.spaces.contains_key(space_id))
        {
            return Err(invalid(
                "Candidate Confirmation Related Space does not exist",
            ));
        }
        let persisted = snapshot
            .projection
            .candidates
            .get(&candidate_id)
            .map(|projection| &projection.candidate)
            .ok_or_else(|| invalid("Candidate Confirmation payload is unavailable"))?;
        let final_draft = input.edits.apply(&persisted.content)?;
        let final_json = serde_json::to_string(&final_draft).map_err(|error| {
            Error::new(ErrorKind::Io, format!("serialize final draft: {error}"))
        })?;
        let scan = PrivacyScanner::default().scan(&final_json)?;
        if !scan.is_clean() {
            return Err(Error::new(
                ErrorKind::PrivacyRejected,
                "Candidate Confirmation final draft failed the privacy boundary",
            ));
        }
        let operation = CandidateConfirmationOperation {
            candidate_id,
            review_parent_version: input.expected_review_version,
            analysis_generation: review.analysis_generation.unwrap_or_default(),
            primary: primary_reference,
            related_space_ids,
            edits: input.edits.clone(),
        };
        let proposed_plan =
            CandidateConfirmationPlan::reserve(persisted, operation, resolved_primary)?;
        let reservation = self.tasks.reserve_candidate_confirmation(
            &locator,
            expected_task_id,
            expected_intent_revision_id,
            &proposed_plan,
        )?;
        let plan = reservation.operation.plan;
        let write = self.store.confirm_candidate(&plan)?;
        let finalized = self.tasks.finalize_candidate_confirmation(
            candidate_id,
            &plan.operation_hash,
            write.record.confirmation_id,
            write.record.result_context_id,
        )?;
        let snapshot = self.snapshot()?;
        let status = if finalized.already_confirmed
            || write.status == CandidateConfirmationWriteStatus::AlreadyExists
        {
            CandidateConfirmResponseStatus::AlreadyConfirmed
        } else {
            CandidateConfirmResponseStatus::Confirmed
        };
        Ok(CandidateConfirmResponse {
            status,
            created: status == CandidateConfirmResponseStatus::Confirmed,
            candidate_id,
            confirmation_id: plan.confirmation.confirmation_id,
            context_id: plan.result_context_id,
            revision_id: plan.result_revision.revision_id,
            primary_space_id: plan.confirmation.primary_space_id,
            related_space_ids: plan.confirmation.related_space_ids,
            batch_id: write.append.batch_id,
            commit_oid: write.append.commit_oid,
            event_ids: write.append.event_ids,
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            assessment_acknowledgments: review.analysis.assessments,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn candidate_review_view(
        &self,
        record: &CandidateReviewRecord,
        snapshot: &DomainSnapshot,
    ) -> Result<CandidateReviewView> {
        let persisted = snapshot
            .projection
            .candidates
            .get(&record.candidate_id)
            .map(|projection| &projection.candidate)
            .ok_or_else(|| invalid("Candidate Review payload is unavailable"))?;
        if persisted.submission_id != record.submission_id
            || persisted.source_episode != record.source_episode
        {
            return Err(invariant(
                "Candidate Review identity disagrees with its finalized Git Candidate",
            ));
        }
        let episode = self
            .tasks
            .read_work_episode(record.source_episode.episode_id)?
            .ok_or_else(|| invalid("Candidate Review source Episode does not exist"))?;
        let checkpoint = episode
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == record.checkpoint_id)
            .ok_or_else(|| invariant("Candidate Review source Checkpoint disappeared"))?;
        let final_checkpoint = episode
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == record.final_checkpoint_id)
            .ok_or_else(|| invariant("Candidate Review final Checkpoint disappeared"))?;
        let claim = checkpoint
            .claims
            .iter()
            .find(|claim| claim.claim_id == record.claim_id)
            .ok_or_else(|| invariant("Candidate Review source Claim disappeared"))?;
        let signals = self
            .tasks
            .read_signal_history(record.source_episode.task_session_id)?;
        let material = build_claim_material(
            &episode,
            final_checkpoint,
            checkpoint,
            claim,
            &signals,
            snapshot,
        );
        if material.draft.as_ref() != Some(&persisted.content) {
            return Err(invariant(
                "Candidate Review source Claim no longer reconstructs the persisted draft",
            ));
        }
        let analysis_view = self.tasks.read_candidate_analysis(record.candidate_id)?;
        let (analysis, recommendations, confidence, unknowns, candidate_status, generation) =
            analysis_view.map_or_else(
                || {
                    (
                        CandidateAnalysis::default(),
                        Vec::new(),
                        material.confidence,
                        material.unknowns,
                        AutomaticCandidateStatus::Draft,
                        None,
                    )
                },
                |view| {
                    (
                        view.candidate.analysis,
                        view.candidate.space_recommendations,
                        view.candidate.confidence,
                        view.candidate.unknowns,
                        view.candidate.status,
                        Some(view.analysis_generation),
                    )
                },
            );
        let diagnostics = if record.status == CandidateReviewStatus::Expired {
            vec![CandidateReviewDiagnostic::ReviewExpired]
        } else {
            match analysis.status {
                CandidateAnalysisStatus::Pending => {
                    vec![CandidateReviewDiagnostic::AnalysisPending]
                }
                CandidateAnalysisStatus::Failed => {
                    vec![CandidateReviewDiagnostic::AnalysisFailed {
                        error_code: analysis.error_code.clone().unwrap_or_default(),
                    }]
                }
                CandidateAnalysisStatus::Complete => Vec::new(),
            }
        };
        let ready_for_review = record.status == CandidateReviewStatus::Pending
            && analysis.status == CandidateAnalysisStatus::Complete
            && matches!(
                candidate_status,
                AutomaticCandidateStatus::NeedsSpaceReview
                    | AutomaticCandidateStatus::ExactDuplicateReview
                    | AutomaticCandidateStatus::PotentialContradictionReview
                    | AutomaticCandidateStatus::ReadyForReview
            );
        let view = CandidateReviewView {
            candidate_id: record.candidate_id,
            submission_id: record.submission_id,
            source_episode: record.source_episode,
            build_id: record.build_id,
            final_checkpoint_id: record.final_checkpoint_id,
            checkpoint_id: record.checkpoint_id,
            claim_id: record.claim_id,
            content: persisted.content.clone(),
            analysis,
            space_recommendations: recommendations,
            confidence,
            unknowns,
            candidate_status,
            review_status: record.status,
            review_version: record.review_version,
            analysis_generation: generation,
            created_at_unix_seconds: record.created_at_unix_seconds,
            expires_at_unix_seconds: record.expires_at_unix_seconds,
            discarded_at_unix_seconds: record.discarded_at_unix_seconds,
            expired_at_unix_seconds: record.expired_at_unix_seconds,
            discard_reason: record.discard_reason.clone(),
            confirmation_id: record.confirmation_id,
            result_context_id: record.result_context_id,
            ready_for_review,
            diagnostics,
            untrusted_data: true,
        };
        view.validate()?;
        Ok(view)
    }

    fn repository_scan(&self, input: &RepositoryScanInput) -> Result<RepositoryScanResponse> {
        input.validate()?;
        let paths = input
            .paths
            .iter()
            .map(RepoRelativePath::new)
            .collect::<Result<Vec<_>>>()?;
        let requested = fs::canonicalize(&input.checkout_path).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("canonicalize scan checkout: {error}"),
            )
        })?;
        let registered = self
            .repositories
            .resolve_by_checkout_path(&requested)?
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::RepositoryNotConfigured,
                    "repository_scan checkout is not configured in the Repository Catalog",
                )
            })?;
        let locator = registered
            .locators
            .iter()
            .find(|locator| {
                locator.availability == RepositoryAvailability::Available
                    && locator.checkout_path == requested
                    && locator.checkout_path.exists()
            })
            .ok_or_else(|| invalid("configured Repository has no available local checkout"))?;
        let plan = RepositoryScanPlan::new(registered.identity.repository_id.clone(), paths)?;
        let outcome = RepositoryScanner::default().scan(
            &registered.identity,
            &locator.checkout_path,
            &plan,
        )?;
        let metadata = self.index.synchronize()?.metadata;
        Ok(repository_scan_response(
            &registered,
            &locator.checkout_path,
            outcome,
            input.max_artifacts,
            metadata.indexed_tree_oid,
            metadata.projection_generation,
        ))
    }

    fn engineering_reference_record(
        &self,
        input: &EngineeringReferenceRecordInput,
    ) -> Result<EngineeringReferenceRecordResponse> {
        let (context_id, revision_id, repository_id) = input.ids()?;
        let snapshot = self.snapshot()?;
        let (space_id, context) =
            find_context(&snapshot, None, context_id).map_err(|failure| failure.error)?;
        if !context.revisions.contains_key(&revision_id) {
            return Err(invalid(format!(
                "revision {revision_id} does not belong to Context {context_id}"
            )));
        }
        if self.repositories.resolve_by_id(&repository_id)?.is_none() {
            return Err(invalid(format!(
                "Repository does not exist: {repository_id}"
            )));
        }
        let event = Event::engineering_reference_recorded(
            context_id,
            revision_id,
            input.draft(repository_id.clone())?,
            None,
        )?;
        let reference_id = match event.payload() {
            EventPayload::EngineeringReferenceRecorded { reference, .. } => reference.reference_id,
            _ => unreachable!(),
        };
        let event_id = event.event_id();
        let append = self.store.append_event(AppendRequest::event(event))?;
        let metadata = self.index.synchronize()?.metadata;
        debug_assert!(snapshot.projection.spaces.contains_key(&space_id));
        Ok(EngineeringReferenceRecordResponse {
            context_id,
            revision_id,
            repository_id,
            reference_id,
            event_id,
            batch_id: append.batch_id.to_string(),
            commit_oid: append.commit_oid,
            tree: metadata.indexed_tree_oid,
            generation: metadata.projection_generation,
        })
    }

    fn association_rebuild(
        &self,
        input: &AssociationRebuildInput,
    ) -> Result<AssociationRebuildResponse> {
        let engineering_graph = self
            .engineering_graph
            .as_ref()
            .ok_or_else(|| unavailable("Engineering projection storage is unavailable"))?;
        let snapshot = self.snapshot()?;
        let references = snapshot
            .projection
            .engineering_references
            .values()
            .map(|projection| ProjectedEngineeringReference {
                context_id: projection.context_id,
                revision_id: projection.revision_id,
                reference: projection.reference.clone(),
            })
            .collect::<Vec<_>>();
        let repositories = self.repositories.list()?;
        let (scan_outcomes, repository_summaries) =
            scan_registered_repositories(&repositories, &references)?;
        let context_snapshots = build_graph_context_snapshots(&snapshot.projection, &references)?;
        let previous = engineering_graph.read_projection()?;
        let projection = previous.as_ref().map_or_else(
            || {
                EngineeringReferenceResolver.resolve(
                    &references,
                    &scan_outcomes,
                    &context_snapshots,
                )
            },
            |previous| {
                EngineeringReferenceResolver.resolve_incremental(
                    previous,
                    &references,
                    &scan_outcomes,
                    &context_snapshots,
                )
            },
        )?;
        if !input.diagnose_only {
            engineering_graph
                .rebuild_for_context_tree(&projection, Some(&snapshot.metadata.indexed_tree_oid))?;
        }
        let status_counts = resolution_status_counts(&projection.references);
        Ok(AssociationRebuildResponse {
            diagnose_only: input.diagnose_only,
            stored: !input.diagnose_only,
            artifact_generation: projection.artifact_generation,
            context_tree_oid: snapshot.metadata.indexed_tree_oid.clone(),
            reference_count: projection.references.len(),
            repositories: repository_summaries,
            status_counts,
            tree: snapshot.metadata.indexed_tree_oid,
            generation: snapshot.metadata.projection_generation,
        })
    }

    fn association_explain(
        &self,
        input: &AssociationExplainInput,
    ) -> Result<AssociationExplainResponse> {
        let reference_id = parse_id_value(&input.reference_id, "reference_id")?;
        let graph = self
            .engineering_graph
            .as_ref()
            .ok_or_else(|| unavailable("Engineering projection storage is unavailable"))?
            .read_snapshot()?
            .ok_or_else(|| {
                unavailable("Engineering projection is unavailable; run association_rebuild")
            })?;
        let projected = graph
            .projection
            .references
            .iter()
            .find(|reference| reference.reference_id == reference_id)
            .ok_or_else(|| invalid(format!("Reference is not projected: {reference_id}")))?;
        let metadata = self.index.synchronize()?.metadata;
        Ok(association_explain_response(
            projected,
            graph.context_tree_oid,
            metadata.indexed_tree_oid,
            metadata.projection_generation,
        ))
    }
}

fn repository_scan_response(
    repository: &RegisteredRepository,
    checkout_path: &Path,
    outcome: RepositoryScanOutcome,
    max_artifacts: usize,
    tree: String,
    generation: u64,
) -> RepositoryScanResponse {
    match outcome {
        RepositoryScanOutcome::Available(snapshot) => {
            let artifact_count = snapshot.artifacts.len();
            let artifacts = snapshot
                .artifacts
                .iter()
                .take(max_artifacts)
                .map(artifact_summary)
                .collect::<Vec<_>>();
            RepositoryScanResponse {
                repository_id: repository.identity.repository_id.clone(),
                canonical_name: repository.identity.canonical_name.clone(),
                checkout_path: checkout_path.to_path_buf(),
                status: "available".to_owned(),
                repository_generation: Some(snapshot.generation.clone()),
                artifact_generation: Some(snapshot.generation),
                head_tree_oid: Some(snapshot.head_tree_oid),
                scanned_files: snapshot.scanned_files,
                scanned_bytes: snapshot.scanned_bytes,
                planned_path_count: snapshot.planned_paths.len(),
                artifact_count,
                omitted_artifact_count: artifact_count.saturating_sub(artifacts.len()),
                skipped_file_count: snapshot.skipped_files.len(),
                skipped_paths: snapshot
                    .skipped_files
                    .iter()
                    .map(|skipped| SkippedPathSummary {
                        path: skipped.path.clone(),
                        reason: skipped_reason_name(skipped.reason).to_owned(),
                    })
                    .collect(),
                artifacts,
                unavailable_reason: None,
                tree,
                generation,
            }
        }
        RepositoryScanOutcome::Unavailable {
            repository_id,
            reason,
        } => RepositoryScanResponse {
            repository_id,
            canonical_name: repository.identity.canonical_name.clone(),
            checkout_path: checkout_path.to_path_buf(),
            status: "unavailable".to_owned(),
            repository_generation: None,
            artifact_generation: None,
            head_tree_oid: None,
            scanned_files: 0,
            scanned_bytes: 0,
            planned_path_count: 0,
            artifact_count: 0,
            omitted_artifact_count: 0,
            skipped_file_count: 0,
            skipped_paths: Vec::new(),
            artifacts: Vec::new(),
            unavailable_reason: Some(reason),
            tree,
            generation,
        },
    }
}

fn artifact_summary(artifact: &sctx_engineering_graph::SnapshotArtifact) -> ArtifactSummary {
    ArtifactSummary {
        artifact_key: artifact.artifact.artifact_key.clone(),
        kind: artifact.artifact.artifact_key.kind(),
        display_name: artifact.artifact.display_name.clone(),
        locator: artifact.artifact.artifact_key.locator().clone(),
    }
}

fn scan_registered_repositories(
    repositories: &[RegisteredRepository],
    references: &[ProjectedEngineeringReference],
) -> Result<(Vec<RepositoryScanOutcome>, Vec<RepositoryRebuildSummary>)> {
    let scanner = RepositoryScanner::default();
    let repositories = repositories
        .iter()
        .map(|repository| (repository.identity.repository_id.clone(), repository))
        .collect::<BTreeMap<_, _>>();
    let mut paths_by_repository = BTreeMap::<RepositoryId, Vec<RepoRelativePath>>::new();
    for reference in references {
        paths_by_repository
            .entry(reference.reference.repository_id.clone())
            .or_default()
            .push(reference.reference.locator.path().clone());
    }
    let mut outcomes = Vec::with_capacity(paths_by_repository.len());
    let mut summaries = Vec::with_capacity(paths_by_repository.len());
    for (repository_id, paths) in paths_by_repository {
        let plan = RepositoryScanPlan::new(repository_id.clone(), paths)?;
        let Some(repository) = repositories.get(&repository_id).copied() else {
            let reason = "Repository is not registered".to_owned();
            outcomes.push(RepositoryScanOutcome::Unavailable {
                repository_id: repository_id.clone(),
                reason: reason.clone(),
            });
            summaries.push(RepositoryRebuildSummary {
                repository_id,
                status: "unavailable".to_owned(),
                checkout_path: None,
                planned_path_count: plan.paths().len(),
                repository_generation: None,
                artifact_count: 0,
                unavailable_reason: Some(reason),
            });
            continue;
        };
        let locator = repository.locators.iter().find(|locator| {
            locator.availability == RepositoryAvailability::Available
                && locator.checkout_path.exists()
        });
        let outcome = if let Some(locator) = locator {
            scanner.scan(&repository.identity, &locator.checkout_path, &plan)?
        } else {
            RepositoryScanOutcome::Unavailable {
                repository_id: repository.identity.repository_id.clone(),
                reason: "Repository has no available registered checkout".to_owned(),
            }
        };
        let summary = match &outcome {
            RepositoryScanOutcome::Available(snapshot) => RepositoryRebuildSummary {
                repository_id: snapshot.repository_id.clone(),
                status: "available".to_owned(),
                checkout_path: locator.map(|locator| locator.checkout_path.clone()),
                planned_path_count: plan.paths().len(),
                repository_generation: Some(snapshot.generation.clone()),
                artifact_count: snapshot.artifacts.len(),
                unavailable_reason: None,
            },
            RepositoryScanOutcome::Unavailable {
                repository_id,
                reason,
            } => RepositoryRebuildSummary {
                repository_id: repository_id.clone(),
                status: "unavailable".to_owned(),
                checkout_path: None,
                planned_path_count: plan.paths().len(),
                repository_generation: None,
                artifact_count: 0,
                unavailable_reason: Some(reason.clone()),
            },
        };
        outcomes.push(outcome);
        summaries.push(summary);
    }
    Ok((outcomes, summaries))
}

fn skipped_reason_name(reason: SkippedFileReason) -> &'static str {
    match reason {
        SkippedFileReason::Missing => "missing",
        SkippedFileReason::Untracked => "untracked",
        SkippedFileReason::IgnoredDirectory => "ignored_directory",
        SkippedFileReason::Generated => "generated",
        SkippedFileReason::UnsupportedLanguage => "unsupported_language",
        SkippedFileReason::Symlink => "symlink",
        SkippedFileReason::EscapesRepository => "escapes_repository",
        SkippedFileReason::Oversized => "oversized",
        SkippedFileReason::Binary => "binary",
        SkippedFileReason::FileLimit => "file_limit",
        SkippedFileReason::TotalByteLimit => "total_byte_limit",
    }
}

fn resolution_status_counts(references: &[ResolvedReferenceProjection]) -> ResolutionStatusCounts {
    let mut counts = ResolutionStatusCounts {
        resolved: 0,
        ambiguous: 0,
        missing: 0,
        unavailable: 0,
    };
    for reference in references {
        match reference.resolution.status {
            ResolutionStatus::Resolved => counts.resolved += 1,
            ResolutionStatus::Ambiguous => counts.ambiguous += 1,
            ResolutionStatus::Missing => counts.missing += 1,
            ResolutionStatus::Unavailable => counts.unavailable += 1,
        }
    }
    counts
}

fn association_explain_response(
    projected: &ResolvedReferenceProjection,
    context_tree_oid: Option<String>,
    tree: String,
    generation: u64,
) -> AssociationExplainResponse {
    let candidates = if projected.resolution.candidates.is_empty() {
        projected
            .resolution
            .resolved_artifact
            .iter()
            .cloned()
            .collect::<Vec<_>>()
    } else {
        projected.resolution.candidates.clone()
    };
    let graph_paths = if candidates.is_empty() {
        vec![vec![
            format!("reference:{}", projected.reference_id),
            format!("repository:{}", projected.resolution.repository_id),
            format!("context:{}", projected.context_id),
            format!("revision:{}", projected.revision_id),
        ]]
    } else {
        candidates
            .iter()
            .map(|artifact| {
                vec![
                    format!("reference:{}", projected.reference_id),
                    format!("repository:{}", projected.resolution.repository_id),
                    format!("artifact:{:?}:{}", artifact.kind(), artifact.digest()),
                    format!("context:{}", projected.context_id),
                    format!("revision:{}", projected.revision_id),
                ]
            })
            .collect()
    };
    AssociationExplainResponse {
        context_id: projected.context_id,
        revision_id: projected.revision_id,
        reference_id: projected.reference_id,
        repository_id: projected.resolution.repository_id.clone(),
        status: projected.resolution.status,
        resolved_artifact: projected.resolution.resolved_artifact.clone(),
        ambiguity_candidates: projected.resolution.candidates.clone(),
        evidence: projected.evidence.clone(),
        graph_paths,
        explanation: projected.resolution.explanation.clone(),
        artifact_generation: projected.artifact_generation.clone(),
        context_tree_oid,
        tree,
        generation,
    }
}

fn require_expected_revision(
    snapshot: &TaskSessionSnapshot,
    expected: Option<&str>,
) -> Result<TaskIntentRevisionId> {
    let expected = expected.ok_or_else(|| {
        invalid("expected_revision_id must be non-null when an ActiveTask exists")
    })?;
    let expected = expected
        .parse::<TaskIntentRevisionId>()
        .map_err(|error| invalid(format!("invalid expected_revision_id: {error}")))?;
    let actual = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("ActiveTask has no Intent Head"))?
        .revision_id;
    if expected != actual {
        return Err(invalid(format!(
            "expected_revision_id is stale; current Intent Head is {actual}"
        )));
    }
    Ok(actual)
}

fn active_signal_records(
    runtime: &TaskRuntime,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignalRecord>> {
    Ok(runtime
        .read_signal_history(task_session_id)?
        .into_iter()
        .filter(|record| record.lifecycle == TaskSignalLifecycle::Active)
        .collect())
}

fn build_claim_material(
    episode: &WorkEpisodeView,
    final_checkpoint: &sctx_domain::AgentCheckpoint,
    checkpoint: &sctx_domain::AgentCheckpoint,
    claim: &CheckpointClaim,
    signals: &[TaskSignalRecord],
    snapshot: &DomainSnapshot,
) -> ClaimBuildMaterial {
    let mut evidence = Vec::new();
    let mut observation_ids = Vec::new();
    let mut visited_observations = HashSet::new();
    let evidence_result = claim.evidence_refs.iter().try_for_each(|reference| {
        collect_candidate_evidence(
            reference,
            claim,
            episode,
            signals,
            snapshot,
            &mut visited_observations,
            &mut observation_ids,
            &mut evidence,
        )
    });
    deduplicate_candidate_evidence(&mut evidence);
    let mut unknowns = checkpoint.unknowns.clone();
    if checkpoint.checkpoint_id != final_checkpoint.checkpoint_id {
        unknowns.extend(final_checkpoint.unknowns.clone());
    }
    let kind = claim.context_kind_hint.unwrap_or(ContextKind::Discovery);
    let mut confidence = if claim.context_kind_hint.is_some() {
        8_000_u16
    } else {
        unknowns.push(CaptureUnknown {
            statement: "Context kind was not explicitly classified; conservative Discovery fallback applied"
                .to_owned(),
            blocking: false,
            recheck_when: vec!["Before Candidate confirmation".to_owned()],
        });
        4_500
    };
    if matches!(kind, ContextKind::Decision | ContextKind::Contract)
        && claim.topic_key_hint.is_none()
    {
        unknowns.push(CaptureUnknown {
            statement: "Decision or Contract topic key remains unclassified".to_owned(),
            blocking: false,
            recheck_when: vec!["Before Candidate confirmation".to_owned()],
        });
        confidence = confidence.saturating_sub(1_000);
    }
    let mut seen_unknowns = HashSet::new();
    unknowns.retain(|unknown| seen_unknowns.insert(unknown.clone()));
    let error_code = evidence_result.err().or({
        if evidence.is_empty() {
            Some("insufficient_evidence")
        } else {
            None
        }
    });
    let draft = error_code.is_none().then(|| ContextRevisionDraft {
        kind,
        topic_key: claim.topic_key_hint.clone(),
        statement: claim.statement.clone(),
        rationale: claim.rationale.clone(),
        applicability: claim.applicability.clone(),
        assumptions: claim.assumptions.clone(),
        recheck_when: claim.recheck_when.clone(),
        relations: Vec::new(),
        evidence,
    });
    ClaimBuildMaterial {
        checkpoint_id: checkpoint.checkpoint_id,
        claim_id: claim.claim_id,
        draft,
        confidence: CandidateConfidence {
            basis_points: confidence,
            rationale: if claim.context_kind_hint.is_some() {
                "Claim structure and self-contained Evidence were preserved deterministically"
                    .to_owned()
            } else {
                "Evidence is complete, but Context kind uses the conservative Discovery fallback"
                    .to_owned()
            },
        },
        unknowns,
        error_code,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_candidate_evidence(
    reference: &CaptureEvidenceRef,
    claim: &CheckpointClaim,
    episode: &WorkEpisodeView,
    signals: &[TaskSignalRecord],
    snapshot: &DomainSnapshot,
    visited_observations: &mut HashSet<WorkObservationId>,
    observation_ids: &mut Vec<WorkObservationId>,
    evidence: &mut Vec<EvidenceSnapshotDraft>,
) -> std::result::Result<(), &'static str> {
    match reference {
        CaptureEvidenceRef::Observation { observation_id } => {
            let observation = episode
                .episode
                .observations
                .iter()
                .find(|observation| observation.observation_id == *observation_id)
                .ok_or("observation_not_found")?;
            collect_observation_evidence(
                observation,
                claim,
                episode,
                signals,
                snapshot,
                visited_observations,
                observation_ids,
                evidence,
            )
        }
        CaptureEvidenceRef::TaskSignal { signal_id } => {
            let signal = signals
                .iter()
                .find(|signal| signal.signal_id == *signal_id)
                .ok_or("task_signal_not_found")?;
            if signal.task_session_id != episode.episode.task_session_id
                || signal.task_id != episode.episode.task_id
            {
                return Err("task_signal_owner_mismatch");
            }
            let evidence_kind = match signal.signal.kind {
                TaskSignalKind::Diff => EvidenceType::ArtifactSnapshot,
                TaskSignalKind::TestOutcome => EvidenceType::ExperimentRecord,
                TaskSignalKind::Prompt | TaskSignalKind::Workspace => {
                    return Err("task_signal_not_engineering_evidence");
                }
            };
            evidence.push(EvidenceSnapshotDraft {
                kind: evidence_kind,
                supports: claim.statement.clone(),
                content: json!({
                    "signal_id": signal.signal_id,
                    "kind": signal.signal.kind,
                    "normalized_content": signal.signal.content,
                }),
                interpretation: "The Claim explicitly cites this normalized Task Signal"
                    .to_owned(),
                limitations: vec![
                    "A Task Signal records Task-local work and does not independently verify external state"
                        .to_owned(),
                ],
            });
            Ok(())
        }
        CaptureEvidenceRef::ContextEvidence {
            context_id,
            revision_id,
            evidence_id,
        } => {
            let source = snapshot
                .projection
                .spaces
                .values()
                .find_map(|space| space.contexts.get(context_id))
                .and_then(|context| context.revisions.get(revision_id))
                .and_then(|revision| {
                    revision
                        .revision
                        .evidence
                        .iter()
                        .find(|source| source.evidence_id == *evidence_id)
                })
                .ok_or("context_evidence_not_found")?;
            evidence.push(EvidenceSnapshotDraft {
                kind: source.kind,
                supports: source.supports.clone(),
                content: source.content.clone(),
                interpretation: source.interpretation.clone(),
                limitations: source.limitations.clone(),
            });
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_observation_evidence(
    observation: &WorkObservation,
    claim: &CheckpointClaim,
    episode: &WorkEpisodeView,
    signals: &[TaskSignalRecord],
    snapshot: &DomainSnapshot,
    visited_observations: &mut HashSet<WorkObservationId>,
    observation_ids: &mut Vec<WorkObservationId>,
    evidence: &mut Vec<EvidenceSnapshotDraft>,
) -> std::result::Result<(), &'static str> {
    if !visited_observations.insert(observation.observation_id) {
        return Ok(());
    }
    observation_ids.push(observation.observation_id);
    match &observation.observation {
        NormalizedWorkObservation::InlineValidation { evidence: inline } => {
            evidence.push(inline.clone());
            Ok(())
        }
        NormalizedWorkObservation::UnresolvedQuestion { .. } => {
            Err("unresolved_question_not_evidence")
        }
        NormalizedWorkObservation::Validation {
            conclusion,
            evidence_refs,
        } => {
            for reference in evidence_refs {
                collect_candidate_evidence(
                    reference,
                    claim,
                    episode,
                    signals,
                    snapshot,
                    visited_observations,
                    observation_ids,
                    evidence,
                )?;
            }
            evidence.push(EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: claim.statement.clone(),
                content: json!({
                    "observation_id": observation.observation_id,
                    "conclusion": conclusion,
                }),
                interpretation: "The Claim cites this normalized validation conclusion".to_owned(),
                limitations: normalized_observation_limitations(observation),
            });
            Ok(())
        }
        normalized => {
            let kind = match normalized {
                NormalizedWorkObservation::TestOutcome { .. } => EvidenceType::ExperimentRecord,
                NormalizedWorkObservation::Diff { .. }
                | NormalizedWorkObservation::Artifact { .. }
                | NormalizedWorkObservation::Interface { .. } => EvidenceType::ArtifactSnapshot,
                NormalizedWorkObservation::Breadcrumb { .. }
                | NormalizedWorkObservation::ContextUse { .. } => EvidenceType::SourceSnapshot,
                NormalizedWorkObservation::Validation { .. }
                | NormalizedWorkObservation::InlineValidation { .. }
                | NormalizedWorkObservation::UnresolvedQuestion { .. } => {
                    unreachable!("special Observation variants were handled above")
                }
            };
            evidence.push(EvidenceSnapshotDraft {
                kind,
                supports: claim.statement.clone(),
                content: json!({
                    "observation_id": observation.observation_id,
                    "normalized": normalized,
                }),
                interpretation: "The Claim explicitly cites this normalized Work Observation"
                    .to_owned(),
                limitations: normalized_observation_limitations(observation),
            });
            Ok(())
        }
    }
}

fn normalized_observation_limitations(observation: &WorkObservation) -> Vec<String> {
    if observation
        .source_refs
        .iter()
        .any(|source| matches!(source, WorkSourceRef::Capture(_)))
    {
        vec![
            "Only normalized engineering meaning is preserved; the raw Capture payload is excluded"
                .to_owned(),
        ]
    } else {
        vec!["The snapshot excludes raw transcript and tool output".to_owned()]
    }
}

fn deduplicate_candidate_evidence(evidence: &mut Vec<EvidenceSnapshotDraft>) {
    let mut seen = HashSet::with_capacity(evidence.len());
    evidence.retain(|snapshot| {
        serde_json::to_string(snapshot).map_or(true, |encoded| seen.insert(encoded))
    });
}

fn candidate_builder_error_code(error: &Error) -> &'static str {
    match error.kind() {
        ErrorKind::IdempotencyKeyConflict => "idempotency_key_conflict",
        ErrorKind::PrivacyRejected => "privacy_rejected",
        ErrorKind::InvalidInput if error.message().contains("privacy gate rejected") => {
            "privacy_rejected"
        }
        ErrorKind::InvalidInput => "candidate_rejected",
        ErrorKind::StaleState => "stale_state",
        ErrorKind::Conflict => "conflict",
        ErrorKind::Io => "io_error",
        ErrorKind::External => "external_error",
        ErrorKind::InvariantViolation => "invariant_violation",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::RepositoryNotConfigured => "repository_not_configured",
        _ => "candidate_write_failed",
    }
}

fn candidate_analysis_error_code(error: &Error) -> &'static str {
    match error.kind() {
        ErrorKind::StaleState => "analysis_projection_changed",
        ErrorKind::InvalidInput => "analysis_invalid_input",
        ErrorKind::Io => "analysis_storage_failed",
        ErrorKind::External => "analysis_dependency_unavailable",
        ErrorKind::InvariantViolation => "analysis_invariant",
        ErrorKind::RepositoryNotConfigured => "analysis_repository_unavailable",
        _ => "analysis_failed",
    }
}

fn candidate_build_response(
    view: CandidateBuildView,
    materials: &BTreeMap<CheckpointClaimId, &ClaimBuildMaterial>,
) -> Result<CandidateBuildResponse> {
    Ok(CandidateBuildResponse {
        build_id: view.build_id,
        episode_id: view.source_episode.episode_id,
        status: view.status.into(),
        items: view
            .items
            .into_iter()
            .map(|item| {
                let material = materials.get(&item.claim_id).ok_or_else(|| {
                    invariant("persisted Build item lost immutable Claim material")
                })?;
                Ok(CandidateBuildItemSummary {
                    checkpoint_id: item.checkpoint_id,
                    claim_id: item.claim_id,
                    submission_id: item.submission_id,
                    status: item.status.into(),
                    candidate_id: item.candidate_id,
                    event_id: item.event_id,
                    error_code: item.error_code,
                    candidate_status: if item.status == CandidateBuildItemStatus::NeedsEvidence {
                        AutomaticCandidateStatus::NeedsEvidence
                    } else {
                        AutomaticCandidateStatus::Draft
                    },
                    analysis: CandidateAnalysis::default(),
                    confidence: material.confidence.clone(),
                    unknowns: material.unknowns.clone(),
                    space_recommendations: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn sync_repository_catalog(
    root: &Path,
    registry: &RepositoryRegistry,
) -> Result<RepositoryCatalogSyncReport> {
    let catalog = UserConfigStore::initialize(root)?.repository_catalog()?;
    sync_repository_catalog_snapshot(registry, &catalog)
}

fn sync_repository_catalog_snapshot(
    registry: &RepositoryRegistry,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<RepositoryCatalogSyncReport> {
    let specifications = catalog
        .repositories
        .iter()
        .map(|repository| CatalogRepositorySpec {
            repository_id: repository.repository_id.clone(),
            checkout_paths: repository.checkout_paths.clone(),
        })
        .collect::<Vec<_>>();
    registry.sync_catalog(&specifications)
}

/// Synchronizes the disposable Repository Registry from the authoritative local Catalog.
///
/// # Errors
///
/// Returns typed Catalog, Git validation, locking, or Registry errors.
pub fn sync_repository_catalog_at_root(
    root: impl AsRef<Path>,
) -> Result<RepositoryCatalogSyncReport> {
    let root = root.as_ref();
    let registry = RepositoryRegistry::initialize(root)?;
    sync_repository_catalog(root, &registry)
}

/// Reads a Context Pack for an already-authoritative `ActiveTask` without mutation.
///
/// # Errors
///
/// Returns an input error when no Working Intent update established the Task.
pub fn task_context_readonly_at_root(
    root: impl AsRef<Path>,
    input: &TaskContextReadInput,
) -> Result<TaskContextResponse> {
    Runtime::open(root.as_ref())?.task_context_readonly(input)
}

/// Resolves one request-local Artifact Focus under `ActiveTask` Intent CAS and returns its Pack.
///
/// # Errors
///
/// Returns typed Session, CAS, Catalog path, locator, Runtime, or Search errors.
pub fn task_artifact_focus_at_root(
    root: impl AsRef<Path>,
    input: &ArtifactFocusQuery,
) -> Result<ArtifactFocusQueryResponse> {
    Runtime::open(root.as_ref())?.task_artifact_focus(input)
}

/// Applies a lightweight Working Intent CAS update and returns its Context Pack.
///
/// # Errors
///
/// Returns typed validation, CAS, runtime, or Search errors.
pub fn task_intent_update_at_root(
    root: impl AsRef<Path>,
    input: &TaskIntentUpdateInput,
) -> Result<TaskIntentUpdateResponse> {
    Runtime::open(root.as_ref())?.task_intent_update(input)
}

/// Supersedes active Signal IDs under exact Task and Intent CAS guards.
///
/// # Errors
///
/// Returns typed validation, CAS, or runtime errors.
pub fn task_signal_supersede_at_root(
    root: impl AsRef<Path>,
    input: &TaskSignalSupersedeInput,
) -> Result<TaskSignalSupersedeResponse> {
    Runtime::open(root.as_ref())?.task_signal_supersede(input)
}

/// Persists one explicit Agent-authored Checkpoint and builds Candidates only at a close boundary.
///
/// # Errors
///
/// Returns typed Session/Task/Intent/Episode CAS, reference, privacy or storage errors.
pub fn task_checkpoint_at_root(
    root: impl AsRef<Path>,
    input: &TaskCheckpointInput,
) -> Result<TaskCheckpointResponse> {
    Runtime::open(root.as_ref())?.task_checkpoint(input)
}

/// Retries the deterministic Candidate Build for one exact persisted closed Episode.
///
/// This is an internal/CLI recovery boundary, not a public MCP Tool.
///
/// # Errors
///
/// Returns typed Episode, Evidence snapshot, runtime reservation, privacy, or Writer errors.
pub fn build_closed_episode_at_root(
    root: impl AsRef<Path>,
    episode_id: WorkEpisodeId,
) -> Result<CandidateBuildResponse> {
    Runtime::open(root.as_ref())?.build_closed_episode(episode_id)
}

/// Recomputes and replaces one Candidate's Runtime-derived review analysis.
///
/// # Errors
///
/// Returns typed Candidate, Builder provenance, retrieval, Graph, budget, or storage errors.
pub fn candidate_analyze_at_root(
    root: impl AsRef<Path>,
    input: &CandidateAnalyzeInput,
) -> Result<CandidateAnalyzeResponse> {
    Runtime::open(root.as_ref())?.analyze_candidate(input)
}

/// Lists bounded whole Candidate Review summaries for one exact `ActiveTask`.
///
/// # Errors
///
/// Returns typed Session, cursor, budget, Runtime, index, or Review assembly errors.
pub fn candidate_list_at_root(
    root: impl AsRef<Path>,
    input: &CandidateListInput,
) -> Result<CandidateListResponse> {
    Runtime::open(root.as_ref())?.candidate_list(input)
}

/// Gets one complete untrusted Candidate Review for one exact `ActiveTask`.
///
/// # Errors
///
/// Returns typed Session, ownership, Runtime, index, or Review assembly errors.
pub fn candidate_get_at_root(
    root: impl AsRef<Path>,
    input: &CandidateGetInput,
) -> Result<CandidateReviewView> {
    Runtime::open(root.as_ref())?.candidate_get(input)
}

/// Explicitly discards one Pending Candidate Review under Task/Intent/Review CAS.
///
/// # Errors
///
/// Returns typed privacy, ownership, conflict, stale, lifecycle, or storage errors.
pub fn candidate_discard_at_root(
    root: impl AsRef<Path>,
    input: &CandidateDiscardInput,
) -> Result<CandidateDiscardResponse> {
    Runtime::open(root.as_ref())?.candidate_discard(input)
}

/// Confirms one owned Pending Candidate Review into an atomic knowledge fact closure.
///
/// # Errors
///
/// Returns typed ownership, CAS, analysis, selection, privacy, Writer, index, or recovery errors.
pub fn candidate_confirm_at_root(
    root: impl AsRef<Path>,
    input: &CandidateConfirmInput,
) -> Result<CandidateConfirmResponse> {
    Runtime::open(root.as_ref())?.candidate_confirm(input)
}

/// Registers and scans one canonical local Git Repository without returning source text.
///
/// # Errors
///
/// Returns typed validation, Registry, scanner, or projection errors.
pub fn repository_scan_at_root(
    root: impl AsRef<Path>,
    input: &RepositoryScanInput,
) -> Result<RepositoryScanResponse> {
    Runtime::open(root.as_ref())?.repository_scan(input)
}

/// Appends one server-identified persistent Engineering Reference Event.
///
/// # Errors
///
/// Returns typed target, Repository, privacy, Writer, or projection errors.
pub fn engineering_reference_record_at_root(
    root: impl AsRef<Path>,
    input: &EngineeringReferenceRecordInput,
) -> Result<EngineeringReferenceRecordResponse> {
    Runtime::open(root.as_ref())?.engineering_reference_record(input)
}

/// Rebuilds or diagnoses the current Engineering projection from registered Repositories.
///
/// # Errors
///
/// Returns typed Registry, scanner, resolver, projection, or Context snapshot errors.
pub fn association_rebuild_at_root(
    root: impl AsRef<Path>,
    input: &AssociationRebuildInput,
) -> Result<AssociationRebuildResponse> {
    Runtime::open(root.as_ref())?.association_rebuild(input)
}

/// Explains one current Reference resolution without selecting ambiguous candidates.
///
/// # Errors
///
/// Returns typed projection availability, identity, or storage errors.
pub fn association_explain_at_root(
    root: impl AsRef<Path>,
    input: &AssociationExplainInput,
) -> Result<AssociationExplainResponse> {
    Runtime::open(root.as_ref())?.association_explain(input)
}

fn build_task_context_response(
    index: &ProjectionIndex,
    engineering_graph: Option<&EngineeringProjectionStore>,
    snapshot: &TaskSessionSnapshot,
    resolved_focus: Option<ResolvedFocus>,
    token_budget: usize,
    max_spaces: usize,
) -> Result<TaskContextResponse> {
    let current = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("Task Session has no current Intent revision"))?;
    let mut request = TaskContextRequest::automatic(
        snapshot.task_id,
        current.working_intent.clone(),
        snapshot.task_signals.clone(),
        token_budget,
    );
    request.resolved_focus = resolved_focus;
    request.max_spaces = max_spaces;
    let pack = if let Some(engineering_graph) = engineering_graph {
        SearchEngine::with_engineering_graph(index.clone(), engineering_graph.clone())
            .task_context_pack(&request)?
    } else {
        SearchEngine::new(index.clone()).task_context_pack(&request)?
    };
    let retrieval_paths = pack
        .items
        .iter()
        .map(|item| TaskContextRetrievalPaths {
            association_space_id: item.association_space_id,
            context_id: item.context.context_id,
            paths: item.retrieval_paths.clone(),
        })
        .collect();
    Ok(TaskContextResponse {
        task_session_id: snapshot.task_session_id,
        task_id: snapshot.task_id,
        intent_revision_id: current.revision_id,
        candidate_spaces: pack.associations,
        items: pack.items,
        retrieval_paths,
        graph_diagnostics: pack.graph_diagnostics,
        task_fingerprint: pack.task_fingerprint,
        tree: pack.indexed_tree_oid,
        generation: pack.projection_generation,
        artifact_generation: pack.artifact_generation,
        graph_context_tree_oid: pack.graph_context_tree_oid,
        token_budget: pack.token_budget,
        estimated_tokens: pack.estimated_tokens,
        omitted: pack.omitted,
    })
}

/// Stateful MCP request dispatcher for one stdio session.
pub struct McpServer {
    root: PathBuf,
    runtime: Option<Runtime>,
    client: ClientKind,
    initialized: bool,
    authorization_linearization_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Debug)]
struct AuthorizedCallSnapshot {
    catalog: RepositoryCatalogSnapshot,
    _scope: AuthorizedSessionScope,
}

impl McpServer {
    /// Constructs a lazy server without opening Store, Index, Graph, or Task Runtime state.
    ///
    /// # Errors
    ///
    /// Returns an I/O error only when the installation root cannot be made absolute.
    pub fn new(root: impl AsRef<Path>, client: ClientKind) -> Result<Self> {
        let root = std::path::absolute(root.as_ref()).map_err(|error| {
            Error::new(ErrorKind::Io, format!("make MCP root absolute: {error}"))
        })?;
        Ok(Self {
            root,
            runtime: None,
            client,
            initialized: false,
            authorization_linearization_hook: None,
        })
    }

    /// Installs a controlled test seam immediately after authorization has
    /// linearized and before any business Runtime is opened.
    #[doc(hidden)]
    pub fn set_authorization_linearization_hook(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.authorization_linearization_hook = Some(hook);
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("authorized MCP dispatch must open Runtime")
    }

    /// Runs requests until stdin reaches a clean EOF.
    ///
    /// # Errors
    ///
    /// Returns a typed transport failure for invalid framing, truncated frames,
    /// or an I/O failure. JSON and JSON-RPC failures are written to the peer and
    /// do not terminate the session.
    pub fn serve<R: BufRead, W: Write>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
    ) -> std::result::Result<ServeOutcome, TransportError> {
        let mut requests_handled = 0_u64;
        loop {
            let Some(frame) = read_frame(reader)? else {
                return Ok(ServeOutcome {
                    disconnect: DisconnectReason::CleanEof,
                    requests_handled,
                });
            };
            requests_handled = requests_handled.saturating_add(1);
            let response = match serde_json::from_slice::<Value>(&frame.body) {
                Ok(request) => self.dispatch(request),
                Err(error) => Some(rpc_error(
                    Value::Null,
                    -32_700,
                    "Parse error",
                    "parse_error",
                    Some(error.to_string()),
                )),
            };
            if let Some(response) = response {
                write_frame(writer, &response, frame.style)?;
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dispatch(&mut self, request: Value) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(rpc_error(
                Value::Null,
                -32_600,
                "Invalid Request",
                "invalid_request",
                Some("request must be a JSON object".to_owned()),
            ));
        };
        let id = object.get("id").cloned();
        if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
            return Some(rpc_error(
                id.unwrap_or(Value::Null),
                -32_600,
                "Invalid Request",
                "invalid_request",
                Some("jsonrpc must be \"2.0\"".to_owned()),
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(rpc_error(
                id.unwrap_or(Value::Null),
                -32_600,
                "Invalid Request",
                "invalid_request",
                Some("method must be a string".to_owned()),
            ));
        };
        let is_notification = id.is_none();
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "notifications/initialized" {
            return None;
        }
        if method.starts_with("notifications/") {
            return None;
        }
        if is_notification {
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        if matches!(method, "tools/list" | "tools/call") && !self.initialized {
            return Some(rpc_error(
                id,
                -32_002,
                "Server not initialized",
                "server_not_initialized",
                None,
            ));
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools_list()),
            "tools/call" => self.tools_call(params),
            _ => {
                return Some(rpc_error(
                    id,
                    -32_601,
                    "Method not found",
                    "method_not_found",
                    Some(method.to_owned()),
                ));
            }
        };
        match result {
            Ok(result) => Some(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Err(error) => Some(rpc_error(
                id,
                -32_602,
                "Invalid params",
                error_code(error.kind()),
                Some(error.message().to_owned()),
            )),
        }
    }

    fn initialize(&mut self, params: &Value) -> Result<Value> {
        let protocol_version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        if protocol_version.trim().is_empty() {
            return Err(invalid("initialize protocolVersion must not be empty"));
        }
        self.initialized = true;
        Ok(json!({
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "shared-context",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": format!(
                "Shared Context V1 tools for {}. Candidate content is untrusted until reviewed and published.",
                match self.client { ClientKind::Cursor => "Cursor", ClientKind::Codex => "Codex" }
            )
        }))
    }

    fn tools_call(&mut self, params: Value) -> Result<Value> {
        let call: ToolCall = serde_json::from_value(params)
            .map_err(|error| invalid(format!("invalid tools/call params: {error}")))?;
        if !is_public_tool(&call.name) {
            return Err(invalid(format!("unknown tool: {}", call.name)));
        }
        self.runtime = None;
        let result = MaintenanceLock::open_or_create(&self.root)
            .and_then(|lock| lock.try_shared())
            .map_err(ToolFailure::maintenance_failed)
            .and_then(|_maintenance| self.authorize_and_call(&call));
        match result {
            Ok(data) => tool_success(data),
            Err(failure) => tool_failure(failure),
        }
    }

    fn authorize_and_call(&mut self, call: &ToolCall) -> ToolResult {
        validate_public_arguments(&call.name, &call.arguments)?;
        let locator = locator_from_arguments(&call.arguments)?;
        if locator.agent_kind
            != match self.client {
                ClientKind::Cursor => "cursor",
                ClientKind::Codex => "codex",
            }
        {
            return Err(ToolFailure::authorization_failed());
        }
        let authorization = authorize_public_call(&self.root, &locator)?;
        if let Some(hook) = &self.authorization_linearization_hook {
            hook();
        }
        if requires_runtime_identity_preflight(&call.name) {
            let runtime_database = self.root.join("state/runtime.sqlite");
            if !runtime_database.is_file() {
                return Err(identity_target_unavailable(&call.name));
            }
            let tasks = TaskRuntime::initialize(&self.root)
                .map_err(|error| runtime_open_failure(&call.name, error))?;
            authorize_runtime_identity_target(&call.name, &call.arguments, &tasks)?;
        }
        let runtime = Runtime::open_with_catalog(&self.root, authorization.catalog.clone())
            .map_err(|error| runtime_open_failure(&call.name, error))?;
        self.runtime = Some(runtime);
        let arguments = call.arguments.clone();
        match call.name.as_str() {
            "task_intent_update" => self.task_intent_update(arguments),
            "task_capture_list" => self.task_capture_list(arguments),
            "task_artifact_focus" => self.task_artifact_focus(arguments),
            "task_signal_supersede" => self.task_signal_supersede(arguments),
            "task_checkpoint" => self.task_checkpoint(arguments),
            "task_context" => self.task_context(arguments),
            "repository_scan" => self.repository_scan(arguments),
            "engineering_reference_record" => self.engineering_reference_record(arguments),
            "association_explain" => self.association_explain(arguments),
            "association_rebuild" => self.association_rebuild(arguments),
            "context_search" => self.context_search(arguments),
            "context_get" => self.context_get(arguments),
            "candidate_list" => self.candidate_list(arguments),
            "candidate_get" => self.candidate_get(arguments),
            "candidate_discard" => self.candidate_discard(arguments),
            "candidate_confirm" => self.candidate_confirm(arguments),
            "space_list" => self.space_list(arguments),
            _ => unreachable!("public tool was validated before dispatch"),
        }
    }

    fn context_search(&self, arguments: Value) -> ToolResult {
        let input: SearchInput = decode_arguments(arguments)?;
        let response =
            SearchEngine::new(self.runtime().index.clone()).search(&input.into_request()?)?;
        let conflicts = collect_conflicts(&response.results);
        let mut data = serde_json::to_value(response).map_err(serialization_failure)?;
        insert_fields(
            &mut data,
            [
                (
                    "conflicts",
                    serde_json::to_value(conflicts).map_err(serialization_failure)?,
                ),
                (
                    "match_reason",
                    json!("structured_filters_and_full_text_rank"),
                ),
            ],
        )?;
        Ok(data)
    }

    fn task_context(&self, arguments: Value) -> ToolResult {
        let input: TaskContextReadInput = decode_arguments(arguments)?;
        input.validate()?;
        let response = self
            .runtime()
            .task_context_readonly(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_capture_list(&self, arguments: Value) -> ToolResult {
        let input: TaskCaptureListInput = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_capture_list(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_intent_update(&self, arguments: Value) -> ToolResult {
        let input: TaskIntentUpdateInput = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_intent_update(&input)
            .map_err(ToolFailure::intent_update_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_artifact_focus(&self, arguments: Value) -> ToolResult {
        let input: ArtifactFocusQuery = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_artifact_focus(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_signal_supersede(&self, arguments: Value) -> ToolResult {
        let input: TaskSignalSupersedeInput = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_signal_supersede(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_checkpoint(&self, arguments: Value) -> ToolResult {
        let input: TaskCheckpointInput = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_checkpoint(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn repository_scan(&self, arguments: Value) -> ToolResult {
        let input = decode_arguments::<McpRepositoryScanInput>(arguments)?.into_inner();
        let response = self
            .runtime()
            .repository_scan(&input)
            .map_err(ToolFailure::engineering_graph_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn engineering_reference_record(&self, arguments: Value) -> ToolResult {
        let input = decode_arguments::<McpEngineeringReferenceRecordInput>(arguments)?.into_inner();
        let response = self
            .runtime()
            .engineering_reference_record(&input)
            .map_err(ToolFailure::engineering_graph_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn association_explain(&self, arguments: Value) -> ToolResult {
        let input: McpAssociationExplainInput = decode_arguments(arguments)?;
        let input = AssociationExplainInput {
            reference_id: input.reference_id,
        };
        let response = self
            .runtime()
            .association_explain(&input)
            .map_err(ToolFailure::engineering_graph_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn association_rebuild(&self, arguments: Value) -> ToolResult {
        let input: McpAssociationRebuildInput = decode_arguments(arguments)?;
        let input = AssociationRebuildInput {
            diagnose_only: input.diagnose_only,
        };
        let response = self
            .runtime()
            .association_rebuild(&input)
            .map_err(ToolFailure::engineering_graph_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn context_get(&self, arguments: Value) -> ToolResult {
        let input: GetInput = decode_arguments(arguments)?;
        let context_id = parse_id::<ContextId>(&input.context_id, "context_id")?;
        let revision_id = input
            .revision_id
            .as_deref()
            .map(|value| parse_id::<RevisionId>(value, "revision_id"))
            .transpose()?;
        let space_id = input
            .space_id
            .as_deref()
            .map(|value| parse_id::<SpaceId>(value, "space_id"))
            .transpose()?;
        let snapshot = self.runtime().snapshot()?;
        let (found_space_id, context) = find_context(&snapshot, space_id, context_id)?;
        let value = if let Some(revision_id) = revision_id {
            let revision = context.revisions.get(&revision_id).ok_or_else(|| {
                invalid(format!(
                    "revision {revision_id} does not belong to Context {context_id}"
                ))
            })?;
            serde_json::to_value(revision).map_err(serialization_failure)?
        } else {
            serde_json::to_value(context).map_err(serialization_failure)?
        };
        Ok(json!({
            "indexed_tree_oid": snapshot.metadata.indexed_tree_oid,
            "projection_generation": snapshot.metadata.projection_generation,
            "space_id": found_space_id,
            "context_id": context_id,
            "context": value,
            "conflicts": context_conflicts(&snapshot, context_id),
            "match_reason": {"kind": "exact_context_id", "context_id": context_id},
        }))
    }

    fn space_list(&self, arguments: Value) -> ToolResult {
        let _: SessionInput = decode_arguments(arguments)?;
        let snapshot = self.runtime().snapshot()?;
        let spaces = snapshot
            .projection
            .spaces
            .values()
            .map(|space| {
                let titles = space
                    .intent
                    .heads
                    .iter()
                    .filter_map(|id| space.intent.revisions.get(id))
                    .map(|revision| revision.intent.title.clone())
                    .collect::<Vec<_>>();
                json!({
                    "space_id": space.space_id,
                    "intent_heads": space.intent.heads,
                    "titles": titles,
                    "context_count": space.contexts.len(),
                    "conflicts": if space.intent.heads.len() > 1 {
                        vec![json!({"kind": "intent", "status": "open", "heads": space.intent.heads})]
                    } else { Vec::new() },
                    "match_reason": "available_space",
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "indexed_tree_oid": snapshot.metadata.indexed_tree_oid,
            "projection_generation": snapshot.metadata.projection_generation,
            "spaces": spaces,
            "conflicts": snapshot.projection.spaces.values().filter(|space| space.intent.heads.len() > 1).count(),
            "match_reason": "all_available_spaces",
        }))
    }

    fn candidate_list(&self, arguments: Value) -> ToolResult {
        let input: CandidateListInput = decode_arguments(arguments)?;
        self.runtime()
            .candidate_list(&input)
            .and_then(|response| {
                serde_json::to_value(response).map_err(|error| {
                    Error::new(
                        ErrorKind::Io,
                        format!("serialize Candidate Review list: {error}"),
                    )
                })
            })
            .map_err(ToolFailure::candidate_review_failed)
    }

    fn candidate_get(&self, arguments: Value) -> ToolResult {
        let input: CandidateGetInput = decode_arguments(arguments)?;
        self.runtime()
            .candidate_get(&input)
            .and_then(|response| {
                serde_json::to_value(response).map_err(|error| {
                    Error::new(
                        ErrorKind::Io,
                        format!("serialize Candidate Review: {error}"),
                    )
                })
            })
            .map_err(ToolFailure::candidate_review_failed)
    }

    fn candidate_discard(&self, arguments: Value) -> ToolResult {
        let input: CandidateDiscardInput = decode_arguments(arguments)?;
        self.runtime()
            .candidate_discard(&input)
            .and_then(|response| {
                serde_json::to_value(response).map_err(|error| {
                    Error::new(
                        ErrorKind::Io,
                        format!("serialize Candidate discard response: {error}"),
                    )
                })
            })
            .map_err(ToolFailure::candidate_review_failed)
    }

    fn candidate_confirm(&self, arguments: Value) -> ToolResult {
        let input: CandidateConfirmInput = decode_arguments(arguments)?;
        self.runtime()
            .candidate_confirm(&input)
            .and_then(|response| {
                serde_json::to_value(response).map_err(|error| {
                    Error::new(
                        ErrorKind::Io,
                        format!("serialize Candidate Confirmation response: {error}"),
                    )
                })
            })
            .map_err(ToolFailure::candidate_review_failed)
    }
}

/// Runs a server using process stdio.
///
/// # Errors
///
/// Returns initialization errors or typed transport errors mapped to the shared
/// external-error category for the CLI boundary.
pub fn serve_stdio(root: impl AsRef<Path>, client: ClientKind) -> Result<ServeOutcome> {
    let mut server = McpServer::new(root, client)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    server
        .serve(&mut stdin.lock(), &mut stdout.lock())
        .map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("MCP transport {:?}: {}", error.kind(), error.message()),
            )
        })
}

type ToolResult = std::result::Result<Value, ToolFailure>;

struct ToolFailure {
    code: &'static str,
    error: Error,
}

impl ToolFailure {
    fn maintenance_failed(error: Error) -> Self {
        let busy = error.kind() == ErrorKind::MaintenanceBusy;
        drop(error);
        Self {
            code: if busy {
                "maintenance_busy"
            } else {
                "maintenance_unavailable"
            },
            error: Error::new(
                if busy {
                    ErrorKind::MaintenanceBusy
                } else {
                    ErrorKind::External
                },
                if busy {
                    "Shared Context installation is busy with maintenance"
                } else {
                    "Shared Context maintenance coordination is unavailable"
                },
            ),
        }
    }

    fn authorization_failed() -> Self {
        Self {
            code: "session_not_authorized",
            error: Error::new(
                ErrorKind::External,
                "Shared Context MCP call is not authorized for this Agent Session",
            ),
        }
    }

    fn task_context_failed(error: Error) -> Self {
        let code = match error.kind() {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::InvariantViolation => "task_context_invariant",
            ErrorKind::Io => "task_context_storage_failed",
            ErrorKind::External => "task_runtime_conflict",
            ErrorKind::Conflict => "checkpoint_conflict",
            ErrorKind::StaleState => "checkpoint_stale",
            ErrorKind::PrivacyRejected => "privacy_rejected",
            ErrorKind::Unsupported => "task_context_unsupported",
            _ => "task_context_failed",
        };
        Self { code, error }
    }

    fn intent_update_failed(error: Error) -> Self {
        let code = match error.kind() {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::Conflict => "intent_conflict",
            ErrorKind::StaleState => "intent_stale",
            ErrorKind::InvariantViolation => "intent_invariant",
            ErrorKind::Io => "intent_storage_failed",
            _ => "intent_update_failed",
        };
        Self { code, error }
    }

    fn engineering_graph_failed(error: Error) -> Self {
        let code = match error.kind() {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::InvariantViolation => "engineering_graph_invariant",
            ErrorKind::Io => "engineering_graph_storage_failed",
            ErrorKind::External => "engineering_graph_unavailable",
            ErrorKind::Unsupported => "engineering_graph_unsupported",
            ErrorKind::RepositoryNotConfigured => "repository_not_configured",
            _ => "engineering_graph_failed",
        };
        Self { code, error }
    }

    fn candidate_review_failed(error: Error) -> Self {
        let code = match error.kind() {
            ErrorKind::InvalidInput => "candidate_review_invalid",
            ErrorKind::InvariantViolation => "candidate_review_invariant",
            ErrorKind::Io => "candidate_review_storage_failed",
            ErrorKind::Conflict => "candidate_review_conflict",
            ErrorKind::StaleState => "candidate_review_stale",
            ErrorKind::PrivacyRejected => "privacy_rejected",
            _ => "candidate_review_failed",
        };
        Self { code, error }
    }

    fn task_target_failed(error: Error) -> Self {
        if error.kind() == ErrorKind::InvalidInput {
            Self {
                code: "task_target_unavailable",
                error: Error::new(ErrorKind::External, "Task target is unavailable"),
            }
        } else {
            Self::task_context_failed(error)
        }
    }

    fn candidate_target_failed(error: Error) -> Self {
        if error.kind() == ErrorKind::InvalidInput {
            Self {
                code: "candidate_review_unavailable",
                error: Error::new(
                    ErrorKind::External,
                    "Candidate Review target is unavailable",
                ),
            }
        } else {
            Self::candidate_review_failed(error)
        }
    }
}

impl From<Error> for ToolFailure {
    fn from(error: Error) -> Self {
        Self {
            code: error_code(error.kind()),
            error,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpRepositoryScanInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    checkout_path: String,
    paths: Vec<String>,
    #[serde(default = "default_scan_artifact_limit")]
    max_artifacts: usize,
}

impl McpRepositoryScanInput {
    fn into_inner(self) -> RepositoryScanInput {
        RepositoryScanInput {
            checkout_path: self.checkout_path,
            paths: self.paths,
            max_artifacts: self.max_artifacts,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpEngineeringReferenceRecordInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    context_id: String,
    revision_id: String,
    repository_id: String,
    artifact_kind: ArtifactKind,
    relation: ReferenceRelation,
    locator: ArtifactLocator,
    supports: String,
    limitations: Vec<String>,
}

impl McpEngineeringReferenceRecordInput {
    fn into_inner(self) -> EngineeringReferenceRecordInput {
        EngineeringReferenceRecordInput {
            context_id: self.context_id,
            revision_id: self.revision_id,
            repository_id: self.repository_id,
            artifact_kind: self.artifact_kind,
            relation: self.relation,
            locator: self.locator,
            supports: self.supports,
            limitations: self.limitations,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpAssociationExplainInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    reference_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpAssociationRebuildInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    #[serde(default)]
    diagnose_only: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
struct GetInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    context_id: String,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    revision_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    space_ids: Vec<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    conditions: Vec<String>,
    #[serde(default)]
    kinds: Vec<ContextKind>,
    #[serde(default)]
    statuses: Vec<ContextStatus>,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    cursor: Option<String>,
}

impl SearchInput {
    fn into_request(self) -> ToolResultSearch {
        Ok(SearchRequest {
            query: self.query,
            filters: SearchFilters {
                space_ids: self
                    .space_ids
                    .iter()
                    .map(|value| parse_id(value, "space_ids"))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                scope: ScopeFilter {
                    domains: self.domains,
                    platforms: self.platforms,
                    conditions: self.conditions,
                },
                kinds: self.kinds,
                statuses: self.statuses,
            },
            page_size: self.page_size,
            cursor: self.cursor,
        })
    }
}

type ToolResultSearch = std::result::Result<SearchRequest, ToolFailure>;

fn is_public_tool(name: &str) -> bool {
    matches!(
        name,
        "task_intent_update"
            | "task_capture_list"
            | "task_artifact_focus"
            | "task_signal_supersede"
            | "task_checkpoint"
            | "task_context"
            | "repository_scan"
            | "engineering_reference_record"
            | "association_explain"
            | "association_rebuild"
            | "context_search"
            | "context_get"
            | "candidate_list"
            | "candidate_get"
            | "candidate_discard"
            | "candidate_confirm"
            | "space_list"
    )
}

fn runtime_open_failure(name: &str, error: Error) -> ToolFailure {
    match name {
        "task_intent_update" => ToolFailure::intent_update_failed(error),
        "task_capture_list"
        | "task_artifact_focus"
        | "task_signal_supersede"
        | "task_checkpoint"
        | "task_context" => ToolFailure::task_context_failed(error),
        "repository_scan"
        | "engineering_reference_record"
        | "association_explain"
        | "association_rebuild" => ToolFailure::engineering_graph_failed(error),
        "candidate_list" | "candidate_get" | "candidate_discard" | "candidate_confirm" => {
            ToolFailure::candidate_review_failed(error)
        }
        _ => ToolFailure::from(error),
    }
}

fn validate_public_arguments(
    name: &str,
    arguments: &Value,
) -> std::result::Result<(), ToolFailure> {
    macro_rules! decode {
        ($input:ty) => {{
            let _: $input = decode_arguments(arguments.clone())?;
        }};
    }
    match name {
        "task_intent_update" => decode!(TaskIntentUpdateInput),
        "task_capture_list" => decode!(TaskCaptureListInput),
        "task_artifact_focus" => decode!(ArtifactFocusQuery),
        "task_signal_supersede" => decode!(TaskSignalSupersedeInput),
        "task_checkpoint" => decode!(TaskCheckpointInput),
        "task_context" => decode!(TaskContextReadInput),
        "repository_scan" => decode!(McpRepositoryScanInput),
        "engineering_reference_record" => decode!(McpEngineeringReferenceRecordInput),
        "association_explain" => decode!(McpAssociationExplainInput),
        "association_rebuild" => decode!(McpAssociationRebuildInput),
        "context_search" => decode!(SearchInput),
        "context_get" => decode!(GetInput),
        "candidate_list" => decode!(CandidateListInput),
        "candidate_get" => decode!(CandidateGetInput),
        "candidate_discard" => decode!(CandidateDiscardInput),
        "candidate_confirm" => decode!(CandidateConfirmInput),
        "space_list" => decode!(SessionInput),
        _ => unreachable!("public tool name was checked"),
    }
    Ok(())
}

fn locator_from_arguments(
    arguments: &Value,
) -> std::result::Result<ExternalSessionLocator, ToolFailure> {
    let object = arguments
        .as_object()
        .ok_or_else(ToolFailure::authorization_failed)?;
    let agent_kind = object
        .get("agent_kind")
        .and_then(Value::as_str)
        .ok_or_else(ToolFailure::authorization_failed)?;
    let external_session_id = object
        .get("external_session_id")
        .and_then(Value::as_str)
        .ok_or_else(ToolFailure::authorization_failed)?;
    ExternalSessionLocator::new(agent_kind, external_session_id)
        .map_err(|_| ToolFailure::authorization_failed())
}

fn authorize_public_call(
    root: &Path,
    locator: &ExternalSessionLocator,
) -> std::result::Result<AuthorizedCallSnapshot, ToolFailure> {
    // The successful nonblocking lease classification against this exact Catalog is the call's
    // authorization linearization point. Later expiry, SessionEnd, or Catalog replacement affects
    // the next call; this call carries the frozen Catalog and allowed Repository identities.
    let authorization = || -> Result<AuthorizedCallSnapshot> {
        let catalog = UserConfigStore::open_existing(root)?.repository_catalog()?;
        let store = AuthorizedSessionScopeStore::initialize(root)?;
        match store.try_read(locator, &catalog)? {
            AuthorizedSessionScopeRead::Current(scope)
                if !matches!(scope.decision, AuthorizedSessionScopeDecision::Disabled)
                    && !scope.allowed_repository_ids.is_empty() =>
            {
                Ok(AuthorizedCallSnapshot {
                    catalog,
                    _scope: scope,
                })
            }
            AuthorizedSessionScopeRead::Missing
            | AuthorizedSessionScopeRead::Expired
            | AuthorizedSessionScopeRead::StaleCatalog
            | AuthorizedSessionScopeRead::Current(_) => Err(unavailable("unauthorized")),
        }
    };
    authorization().map_err(|_| ToolFailure::authorization_failed())
}

fn authorize_runtime_identity_target(
    name: &str,
    arguments: &Value,
    tasks: &TaskRuntime,
) -> std::result::Result<(), ToolFailure> {
    match name {
        "task_capture_list" => {
            let input: TaskCaptureListInput = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolFailure::from(invalid(error.to_string())))?;
            let locator =
                ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)
                    .map_err(ToolFailure::from)?;
            if tasks
                .read_snapshot_by_locator(&locator)
                .map_err(ToolFailure::task_context_failed)?
                .is_none()
            {
                return Err(ToolFailure::task_target_failed(invalid(
                    "Task target is unavailable",
                )));
            }
        }
        "task_signal_supersede" => {
            let input: TaskSignalSupersedeInput = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolFailure::from(invalid(error.to_string())))?;
            let locator =
                ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)
                    .map_err(ToolFailure::from)?;
            let expected_task_id =
                parse_id_value(&input.task_id, "task_id").map_err(ToolFailure::from)?;
            let active = tasks
                .read_snapshot_by_locator(&locator)
                .map_err(ToolFailure::task_context_failed)?;
            if active.as_ref().map(|task| task.task_id) != Some(expected_task_id) {
                return Err(ToolFailure::task_target_failed(invalid(
                    "Task target is unavailable",
                )));
            }
        }
        "task_checkpoint" => {
            let input: TaskCheckpointInput = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolFailure::from(invalid(error.to_string())))?;
            let locator =
                ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)
                    .map_err(ToolFailure::from)?;
            let expected_task_id = parse_id_value(&input.expected_task_id, "expected_task_id")
                .map_err(ToolFailure::from)?;
            let active = tasks
                .read_snapshot_by_locator(&locator)
                .map_err(ToolFailure::task_context_failed)?;
            if active.as_ref().map(|task| task.task_id) != Some(expected_task_id) {
                return Err(ToolFailure::task_target_failed(invalid(
                    "Task target is unavailable",
                )));
            }
        }
        "candidate_get" => {
            let input: CandidateGetInput = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolFailure::from(invalid(error.to_string())))?;
            require_owned_candidate(
                tasks,
                &input.agent_kind,
                &input.external_session_id,
                &input.candidate_id,
            )?;
        }
        "candidate_discard" => {
            let input: CandidateDiscardInput = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolFailure::from(invalid(error.to_string())))?;
            require_owned_task_and_candidate(
                tasks,
                &input.agent_kind,
                &input.external_session_id,
                &input.expected_task_id,
                &input.candidate_id,
            )?;
        }
        "candidate_confirm" => {
            let input: CandidateConfirmInput = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolFailure::from(invalid(error.to_string())))?;
            require_owned_task_and_candidate(
                tasks,
                &input.agent_kind,
                &input.external_session_id,
                &input.expected_task_id,
                &input.candidate_id,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn require_owned_task_and_candidate(
    tasks: &TaskRuntime,
    agent_kind: &str,
    external_session_id: &str,
    expected_task_id: &str,
    candidate_id: &str,
) -> std::result::Result<(), ToolFailure> {
    let locator =
        ExternalSessionLocator::new(agent_kind, external_session_id).map_err(ToolFailure::from)?;
    let expected_task_id =
        parse_id_value(expected_task_id, "expected_task_id").map_err(ToolFailure::from)?;
    let active = tasks
        .read_snapshot_by_locator(&locator)
        .map_err(ToolFailure::candidate_review_failed)?;
    if active.as_ref().map(|task| task.task_id) != Some(expected_task_id) {
        return Err(ToolFailure::candidate_target_failed(invalid(
            "Candidate Review target is unavailable",
        )));
    }
    require_owned_candidate(tasks, agent_kind, external_session_id, candidate_id)
}

fn require_owned_candidate(
    tasks: &TaskRuntime,
    agent_kind: &str,
    external_session_id: &str,
    candidate_id: &str,
) -> std::result::Result<(), ToolFailure> {
    let locator =
        ExternalSessionLocator::new(agent_kind, external_session_id).map_err(ToolFailure::from)?;
    let candidate_id = parse_id_value(candidate_id, "candidate_id").map_err(ToolFailure::from)?;
    let owned = tasks
        .read_candidate_review(&locator, candidate_id)
        .map_err(ToolFailure::candidate_review_failed)?;
    if owned.is_none() {
        return Err(ToolFailure::candidate_target_failed(invalid(
            "Candidate Review target is unavailable",
        )));
    }
    Ok(())
}

fn requires_runtime_identity_preflight(name: &str) -> bool {
    matches!(
        name,
        "task_capture_list"
            | "task_signal_supersede"
            | "task_checkpoint"
            | "candidate_get"
            | "candidate_discard"
            | "candidate_confirm"
    )
}

fn identity_target_unavailable(name: &str) -> ToolFailure {
    if matches!(
        name,
        "task_capture_list" | "task_signal_supersede" | "task_checkpoint"
    ) {
        ToolFailure::task_target_failed(invalid("Task target is unavailable"))
    } else {
        ToolFailure::candidate_target_failed(invalid("Candidate Review target is unavailable"))
    }
}

#[allow(clippy::too_many_lines)]
fn tools_list() -> Value {
    json!({"tools": [
        tool_schema(
            "task_intent_update",
            "CAS-record a lightweight Working Intent snapshot, optionally start a new explicit Task, and return its TaskContextPack.",
            task_intent_update_schema()
        ),
        tool_schema(
            "task_capture_list",
            "List recent live unclaimed Capture summaries owned by the exact ActiveTask. Captures remain non-factual until explicitly cited by task_checkpoint.",
            task_capture_list_schema()
        ),
        tool_schema(
            "task_artifact_focus",
            "Declare one current File/Module/Symbol/API/Schema/Test focus under ActiveTask CAS and immediately retrieve exact historical Graph context. Repository identity and relative path are resolved by the configured local Catalog.",
            task_artifact_focus_schema()
        ),
        tool_schema(
            "task_signal_supersede",
            "Supersede stable active Signal IDs under exact Task and Intent CAS guards.",
            task_signal_supersede_schema()
        ),
        tool_schema(
            "task_checkpoint",
            "Persist one explicit Agent-authored Claim/Unknown checkpoint under Task, Intent and Episode CAS; close deterministically builds unassigned Candidate drafts, and an empty close reuses an existing current Checkpoint after Hook failure.",
            task_checkpoint_schema()
        ),
        tool_schema(
            "task_context",
            "Read the Context Pack for an existing authoritative ActiveTask without changing runtime state.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1},
                    "token_budget": {"type": "integer", "minimum": MIN_TASK_CONTEXT_TOKEN_BUDGET, "default": 2000},
                    "max_spaces": {"type": "integer", "minimum": 1, "maximum": MAX_TASK_MAX_SPACES, "default": DEFAULT_TASK_MAX_SPACES}
                }
            })
        ),
        tool_schema(
            "repository_scan",
            "Register and scan one canonical local Git Repository, returning bounded Artifact summaries without source text.",
            repository_scan_schema()
        ),
        tool_schema(
            "engineering_reference_record",
            "Record one verified engineering observation for an existing Context revision; Reference/Event identity and storage path are server-owned.",
            engineering_reference_record_schema()
        ),
        tool_schema(
            "association_explain",
            "Explain one current Engineering Reference resolution, evidence, ambiguity candidates, and graph paths without selecting a candidate.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id", "reference_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1},
                    "reference_id": id_schema("ref_")
                }
            })
        ),
        tool_schema(
            "association_rebuild",
            "Rebuild or diagnose Engineering Reference resolution from current registered local Repositories.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1},
                    "diagnose_only": {"type": "boolean", "default": false}
                }
            })
        ),
        tool_schema(
            "context_search",
            "Search Context revisions with stable filters, pagination, conflicts, and match reasons.",
            search_schema()
        ),
        tool_schema(
            "context_get",
            "Get one Context or immutable revision from a deterministic Git tree.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id", "context_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1},
                    "context_id": id_schema("ctx_"),
                    "space_id": id_schema("spc_"),
                    "revision_id": id_schema("rev_")
                }
            })
        ),
        tool_schema(
            "candidate_list",
            "List whole untrusted automatic Candidate Review summaries for the exact ActiveTask; Pending is the default lifecycle filter.",
            candidate_list_schema()
        ),
        tool_schema(
            "candidate_get",
            "Get one complete untrusted automatic Candidate Review without retyping its draft or Evidence.",
            candidate_get_schema()
        ),
        tool_schema(
            "candidate_discard",
            "Explicitly discard one Pending automatic Candidate Review under Task, Intent, and Review-version CAS. This never confirms or publishes Context.",
            candidate_discard_schema()
        ),
        tool_schema(
            "candidate_confirm",
            "Explicitly confirm one owned Pending Candidate Review into one atomic Context/Space fact closure. All generated identities and any proposed new Space Intent are server-owned.",
            candidate_confirm_schema()
        ),
        tool_schema(
            "space_list",
            "List available ContextSpaces from a deterministic Git tree.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1}
                }
            })
        )
    ]})
}

fn repository_scan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "checkout_path", "paths"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "checkout_path": {"type": "string", "minLength": 1},
            "paths": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_REPOSITORY_SCAN_PLAN_PATHS,
                "items": {"type": "string", "minLength": 1}
            },
            "max_artifacts": {"type": "integer", "minimum": 1, "maximum": MAX_SCAN_ARTIFACT_LIMIT, "default": DEFAULT_SCAN_ARTIFACT_LIMIT}
        }
    })
}

fn task_artifact_focus_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "agent_kind", "external_session_id", "expected_revision_id",
            "absolute_file_path", "locator"
        ],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "expected_revision_id": id_schema("tir_"),
            "absolute_file_path": {"type": "string", "minLength": 1},
            "locator": task_artifact_focus_coordinates_schema(),
            "token_budget": {"type": "integer", "minimum": MIN_TASK_CONTEXT_TOKEN_BUDGET, "default": 2000},
            "max_spaces": {"type": "integer", "minimum": 1, "maximum": MAX_TASK_MAX_SPACES, "default": DEFAULT_TASK_MAX_SPACES}
        }
    })
}

fn task_artifact_focus_coordinates_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind"],
                "properties": {"locator_kind": {"const": "file"}}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind"],
                "properties": {"locator_kind": {"const": "module"}}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "protocol", "operation", "normalized_route"],
                "properties": {
                    "locator_kind": {"const": "api"},
                    "protocol": {"type": "string", "minLength": 1},
                    "operation": {"type": "string", "minLength": 1},
                    "normalized_route": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "namespace", "version", "qualified_name"],
                "properties": {
                    "locator_kind": {"const": "schema"},
                    "namespace": {"type": "string", "minLength": 1},
                    "version": {"type": "string", "minLength": 1},
                    "qualified_name": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": [
                    "locator_kind", "language", "module", "enclosing_type",
                    "symbol_name", "signature"
                ],
                "properties": {
                    "locator_kind": {"const": "symbol"},
                    "language": {"type": "string", "minLength": 1},
                    "module": {"type": "string", "minLength": 1},
                    "enclosing_type": {"type": ["string", "null"], "minLength": 1},
                    "symbol_name": {"type": "string", "minLength": 1},
                    "signature": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "qualified_test_name"],
                "properties": {
                    "locator_kind": {"const": "test"},
                    "qualified_test_name": {"type": "string", "minLength": 1}
                }
            }
        ]
    })
}

fn engineering_reference_record_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "context_id", "revision_id", "repository_id", "artifact_kind", "relation", "locator", "supports", "limitations"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "context_id": id_schema("ctx_"),
            "revision_id": id_schema("rev_"),
            "repository_id": repository_id_schema(),
            "artifact_kind": {"type": "string", "enum": ["module", "file", "symbol", "api", "schema", "test"]},
            "relation": {"type": "string", "enum": ["implements", "defines", "consumes", "validates", "constrains", "depends_on"]},
            "locator": artifact_locator_input_schema(),
            "supports": {"type": "string", "minLength": 1},
            "limitations": {"type": "array", "minItems": 1, "items": {"type": "string", "minLength": 1}}
        }
    })
}

fn artifact_locator_input_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "path"],
                "properties": {"locator_kind": {"const": "file"}, "path": {"type": "string", "minLength": 1}}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "path"],
                "properties": {"locator_kind": {"const": "module"}, "path": {"type": "string", "minLength": 1}}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "path", "protocol", "operation", "normalized_route"],
                "properties": {
                    "locator_kind": {"const": "api"}, "path": {"type": "string", "minLength": 1},
                    "protocol": {"type": "string", "minLength": 1}, "operation": {"type": "string", "minLength": 1},
                    "normalized_route": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "path", "namespace", "version", "qualified_name"],
                "properties": {
                    "locator_kind": {"const": "schema"}, "path": {"type": "string", "minLength": 1},
                    "namespace": {"type": "string", "minLength": 1}, "version": {"type": "string", "minLength": 1},
                    "qualified_name": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "path", "language", "module", "enclosing_type", "symbol_name", "signature"],
                "properties": {
                    "locator_kind": {"const": "symbol"}, "path": {"type": "string", "minLength": 1},
                    "language": {"type": "string", "minLength": 1}, "module": {"type": "string", "minLength": 1},
                    "enclosing_type": {"type": ["string", "null"], "minLength": 1},
                    "symbol_name": {"type": "string", "minLength": 1}, "signature": {"type": "string", "minLength": 1}
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["locator_kind", "path", "qualified_test_name"],
                "properties": {
                    "locator_kind": {"const": "test"}, "path": {"type": "string", "minLength": 1},
                    "qualified_test_name": {"type": "string", "minLength": 1}
                }
            }
        ]
    })
}

fn task_intent_update_schema() -> Value {
    let intent_list = || {
        json!({
            "type": "array",
            "maxItems": sctx_domain::MAX_WORKING_INTENT_ITEMS_PER_FIELD,
            "uniqueItems": true,
            "items": {
                "type": "string",
                "minLength": 1,
                "maxLength": sctx_domain::MAX_WORKING_INTENT_ITEM_BYTES
            }
        })
    };
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "task_boundary", "expected_revision_id", "intent"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "task_boundary": {"type": "string", "enum": ["continue", "new"]},
            "expected_revision_id": {
                "anyOf": [id_schema("tir_"), {"type": "null"}]
            },
            "intent": {
                "type": "object",
                "additionalProperties": false,
                "required": ["goal"],
                "properties": {
                    "goal": {"type": "string", "minLength": 1, "maxLength": sctx_domain::MAX_WORKING_INTENT_TEXT_BYTES},
                    "current_direction": {"type": "string", "minLength": 1, "maxLength": sctx_domain::MAX_WORKING_INTENT_TEXT_BYTES},
                    "in_scope": intent_list(),
                    "out_of_scope": intent_list(),
                    "domains": intent_list(),
                    "platforms": intent_list(),
                    "constraints": intent_list(),
                    "acceptance_conditions": intent_list(),
                    "artifact_hints": intent_list(),
                    "interface_hints": intent_list(),
                    "open_questions": intent_list()
                }
            }
        }
    })
}

fn task_signal_supersede_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "task_id", "expected_revision_id", "signal_ids"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "task_id": id_schema("tsk_"),
            "expected_revision_id": id_schema("tir_"),
            "signal_ids": {"type": "array", "minItems": 1, "items": id_schema("sig_")}
        }
    })
}

fn task_capture_list_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TASK_CAPTURE_LIST_LIMIT,
                "default": DEFAULT_TASK_CAPTURE_LIST_LIMIT
            }
        }
    })
}

#[allow(clippy::too_many_lines)]
fn task_checkpoint_schema() -> Value {
    let string_list = || json!({"type": "array", "items": {"type": "string", "minLength": 1}});
    let context_revision = || {
        json!({
            "type": "object", "additionalProperties": false,
            "required": ["context_id", "revision_id"],
            "properties": {"context_id": id_schema("ctx_"), "revision_id": id_schema("rev_")}
        })
    };
    let evidence_snapshot = || {
        json!({
            "type": "object", "additionalProperties": false,
            "required": ["kind", "supports", "content", "interpretation", "limitations"],
            "properties": {
                "kind": {"type": "string", "enum": ["source_snapshot", "experiment_record", "artifact_snapshot"]},
                "supports": {"type": "string", "minLength": 1},
                "content": {"type": "object", "minProperties": 1},
                "interpretation": {"type": "string", "minLength": 1},
                "limitations": string_list()
            }
        })
    };
    let evidence = json!({
        "oneOf": [
            {
                "type": "object", "additionalProperties": false,
                "required": ["kind", "capture_id"],
                "properties": {"kind": {"const": "capture"}, "capture_id": id_schema("cap_")}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["kind", "observation_id"],
                "properties": {"kind": {"const": "observation"}, "observation_id": id_schema("wob_")}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["kind", "signal_id"],
                "properties": {"kind": {"const": "task_signal"}, "signal_id": id_schema("sig_")}
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["kind", "context_id", "revision_id", "evidence_id"],
                "properties": {
                    "kind": {"const": "context_evidence"}, "context_id": id_schema("ctx_"),
                    "revision_id": id_schema("rev_"), "evidence_id": id_schema("evd_")
                }
            },
            {
                "type": "object", "additionalProperties": false,
                "required": ["kind", "evidence"],
                "properties": {"kind": {"const": "inline_validation"}, "evidence": evidence_snapshot()}
            }
        ]
    });
    let artifact_ref = json!({
        "type": "object", "additionalProperties": false,
        "required": ["repository_id", "locator"],
        "properties": {"repository_id": repository_id_schema(), "locator": artifact_locator_input_schema()}
    });
    let claim = json!({
        "type": "object", "additionalProperties": false,
        "required": ["statement", "rationale", "applicability", "assumptions", "recheck_when", "evidence", "artifact_refs", "related_contexts"],
        "properties": {
            "context_kind_hint": kind_schema(),
            "topic_key_hint": {"type": "string", "minLength": 1},
            "statement": {"type": "string", "minLength": 1},
            "rationale": {"type": "string", "minLength": 1},
            "applicability": {
                "type": "object", "additionalProperties": false,
                "required": ["domains", "platforms", "conditions"],
                "properties": {"domains": string_list(), "platforms": string_list(), "conditions": string_list()}
            },
            "assumptions": string_list(),
            "recheck_when": string_list(),
            "evidence": {"type": "array", "minItems": 1, "items": evidence},
            "artifact_refs": {"type": "array", "items": artifact_ref},
            "related_contexts": {"type": "array", "items": context_revision()}
        }
    });
    let unknown = json!({
        "type": "object", "additionalProperties": false,
        "required": ["statement", "blocking", "recheck_when"],
        "properties": {
            "statement": {"type": "string", "minLength": 1},
            "blocking": {"type": "boolean"},
            "recheck_when": string_list()
        }
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "expected_task_id", "expected_intent_revision_id", "expected_episode_version", "boundary", "claims", "unknowns"],
        "anyOf": [
            {"properties": {"claims": {"minItems": 1}}},
            {"properties": {"unknowns": {"minItems": 1}}},
            {
                "properties": {
                    "boundary": {"const": "close"},
                    "claims": {"maxItems": 0},
                    "unknowns": {"maxItems": 0}
                }
            }
        ],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "expected_task_id": id_schema("tsk_"),
            "expected_intent_revision_id": id_schema("tir_"),
            "expected_episode_version": {"type": "integer", "minimum": 0},
            "boundary": {"type": "string", "enum": ["continue", "close"]},
            "claims": {"type": "array", "items": claim},
            "unknowns": {"type": "array", "items": unknown}
        }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn tool_schema(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "query": {"type": "string", "default": ""},
            "space_ids": {"type": "array", "items": id_schema("spc_")},
            "domains": string_array_schema(),
            "platforms": string_array_schema(),
            "conditions": string_array_schema(),
            "kinds": kind_array_schema(),
            "statuses": {"type": "array", "items": {"type": "string", "enum": ["candidate", "accepted", "deprecated", "superseded", "governance_conflict"]}},
            "page_size": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20},
            "cursor": {"type": "string"}
        }
    })
}

fn candidate_list_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "status": {
                "type": "string",
                "enum": ["pending", "discarded", "expired", "confirmed"],
                "default": "pending"
            },
            "limit": {
                "type": "integer", "minimum": 1,
                "maximum": sctx_task_runtime::MAX_CANDIDATE_REVIEW_LIST_LIMIT,
                "default": DEFAULT_CANDIDATE_REVIEW_LIST_LIMIT
            },
            "cursor": {"type": "string", "minLength": 1},
            "token_budget": {
                "type": "integer", "minimum": MIN_CANDIDATE_REVIEW_TOKEN_BUDGET,
                "maximum": MAX_CANDIDATE_REVIEW_TOKEN_BUDGET,
                "default": DEFAULT_CANDIDATE_REVIEW_TOKEN_BUDGET
            }
        }
    })
}

fn candidate_get_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "candidate_id"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "candidate_id": id_schema("cnd_")
        }
    })
}

fn candidate_discard_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "agent_kind", "external_session_id", "expected_task_id",
            "expected_intent_revision_id", "candidate_id", "expected_review_version", "reason"
        ],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "expected_task_id": id_schema("tsk_"),
            "expected_intent_revision_id": id_schema("tir_"),
            "candidate_id": id_schema("cnd_"),
            "expected_review_version": {"type": "integer", "minimum": 1},
            "reason": {"type": "string", "minLength": 1, "maxLength": 512}
        }
    })
}

fn candidate_confirm_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "agent_kind", "external_session_id", "expected_task_id",
            "expected_intent_revision_id", "candidate_id", "expected_review_version",
            "primary", "related_space_ids"
        ],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "expected_task_id": id_schema("tsk_"),
            "expected_intent_revision_id": id_schema("tir_"),
            "candidate_id": id_schema("cnd_"),
            "expected_review_version": {"type": "integer", "minimum": 1},
            "primary": {
                "oneOf": [
                    {
                        "type": "object", "additionalProperties": false,
                        "required": ["existing_space_id"],
                        "properties": {"existing_space_id": id_schema("spc_")}
                    },
                    {
                        "type": "object", "additionalProperties": false,
                        "required": ["new_space_recommendation_id"],
                        "properties": {
                            "new_space_recommendation_id": id_schema("rec_")
                        }
                    }
                ]
            },
            "related_space_ids": {
                "type": "array", "uniqueItems": true,
                "items": id_schema("spc_")
            },
            "edits": candidate_edits_schema()
        }
    })
}

fn candidate_edits_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": kind_schema(),
            "topic_key": {
                "oneOf": [
                    {
                        "type": "object", "additionalProperties": false,
                        "required": ["action", "value"],
                        "properties": {
                            "action": {"const": "set"},
                            "value": {"type": "string", "minLength": 1}
                        }
                    },
                    {
                        "type": "object", "additionalProperties": false,
                        "required": ["action"],
                        "properties": {"action": {"const": "clear"}}
                    }
                ]
            },
            "statement": {"type": "string", "minLength": 1},
            "rationale": {"type": "string", "minLength": 1},
            "applicability": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "domains": string_array_schema(),
                    "platforms": string_array_schema(),
                    "conditions": string_array_schema()
                }
            },
            "assumptions": string_array_schema(),
            "recheck_when": string_array_schema(),
            "relations": {
                "type": "array",
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["target_context_id", "kind", "rationale", "supports"],
                    "properties": {
                        "target_context_id": id_schema("ctx_"),
                        "kind": {"type": "string", "enum": ["depends_on", "supersedes", "contradicts", "related_to"]},
                        "rationale": {"type": "string", "minLength": 1},
                        "supports": {"type": "array", "minItems": 1, "items": {"type": "string", "minLength": 1}}
                    }
                }
            },
            "evidence": {
                "type": "array", "minItems": 1,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["kind", "supports", "content", "interpretation", "limitations"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["source_snapshot", "experiment_record", "artifact_snapshot"]},
                        "supports": {"type": "string", "minLength": 1},
                        "content": {"type": "object", "minProperties": 1},
                        "interpretation": {"type": "string", "minLength": 1},
                        "limitations": string_array_schema()
                    }
                }
            }
        }
    })
}

fn kind_schema() -> Value {
    json!({"type": "string", "enum": ["decision", "contract", "issue", "risk", "validation", "discovery", "progress"]})
}

fn kind_array_schema() -> Value {
    json!({"type": "array", "items": kind_schema()})
}

fn string_array_schema() -> Value {
    json!({"type": "array", "items": {"type": "string", "minLength": 1}})
}

fn id_schema(prefix: &str) -> Value {
    json!({"type": "string", "pattern": format!("^{prefix}[0-9a-fA-F-]+$")})
}

fn repository_id_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": REPOSITORY_ID_MAX_BYTES,
        "pattern": REPOSITORY_ID_PATTERN
    })
}

fn read_frame<R: BufRead>(reader: &mut R) -> std::result::Result<Option<Frame>, TransportError> {
    let mut first = String::new();
    loop {
        first.clear();
        let bytes = reader
            .read_line(&mut first)
            .map_err(transport_io("read MCP frame"))?;
        if bytes == 0 {
            return Ok(None);
        }
        trim_line_ending(&mut first);
        if !first.is_empty() {
            break;
        }
    }
    if first.to_ascii_lowercase().starts_with("content-length:") {
        let length = first
            .split_once(':')
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .ok_or_else(|| {
                TransportError::new(TransportErrorKind::InvalidFrame, "invalid Content-Length")
            })?;
        if length > MAX_FRAME_BYTES {
            return Err(TransportError::new(
                TransportErrorKind::FrameTooLarge,
                format!("MCP frame is {length} bytes; maximum is {MAX_FRAME_BYTES}"),
            ));
        }
        loop {
            let mut header = String::new();
            let bytes = reader
                .read_line(&mut header)
                .map_err(transport_io("read MCP header"))?;
            if bytes == 0 {
                return Err(TransportError::new(
                    TransportErrorKind::UnexpectedEof,
                    "MCP peer disconnected inside headers",
                ));
            }
            trim_line_ending(&mut header);
            if header.is_empty() {
                break;
            }
            if !header.contains(':') {
                return Err(TransportError::new(
                    TransportErrorKind::InvalidFrame,
                    format!("invalid MCP header: {header}"),
                ));
            }
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                TransportError::new(
                    TransportErrorKind::UnexpectedEof,
                    "MCP peer disconnected inside a message body",
                )
            } else {
                TransportError::new(TransportErrorKind::Io, format!("read MCP body: {error}"))
            }
        })?;
        Ok(Some(Frame {
            body,
            style: FrameStyle::ContentLength,
        }))
    } else {
        if first.len() > MAX_FRAME_BYTES {
            return Err(TransportError::new(
                TransportErrorKind::FrameTooLarge,
                format!("MCP frame exceeds {MAX_FRAME_BYTES} bytes"),
            ));
        }
        Ok(Some(Frame {
            body: first.into_bytes(),
            style: FrameStyle::Newline,
        }))
    }
}

fn write_frame<W: Write>(
    writer: &mut W,
    response: &Value,
    style: FrameStyle,
) -> std::result::Result<(), TransportError> {
    let body = serde_json::to_vec(response).map_err(|error| {
        TransportError::new(
            TransportErrorKind::Io,
            format!("serialize MCP response: {error}"),
        )
    })?;
    match style {
        FrameStyle::Newline => {
            writer
                .write_all(&body)
                .map_err(transport_io("write MCP response"))?;
            writer
                .write_all(b"\n")
                .map_err(transport_io("write MCP delimiter"))?;
        }
        FrameStyle::ContentLength => {
            writer
                .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                .map_err(transport_io("write MCP header"))?;
            writer
                .write_all(&body)
                .map_err(transport_io("write MCP response"))?;
        }
    }
    writer.flush().map_err(transport_io("flush MCP response"))
}

fn trim_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn transport_io(context: &'static str) -> impl FnOnce(io::Error) -> TransportError {
    move |error| TransportError::new(TransportErrorKind::Io, format!("{context}: {error}"))
}

#[allow(clippy::needless_pass_by_value)]
fn rpc_error(
    id: Value,
    code: i64,
    message: &str,
    typed_code: &str,
    detail: Option<String>,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": {"code": typed_code, "detail": detail}
        }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn tool_success(data: Value) -> Result<Value> {
    let text = serde_json::to_string_pretty(&data)
        .map_err(|error| Error::new(ErrorKind::Io, format!("serialize tool result: {error}")))?;
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": data,
        "isError": false,
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn tool_failure(failure: ToolFailure) -> Result<Value> {
    let data = json!({
        "error": {
            "code": failure.code,
            "kind": error_code(failure.error.kind()),
            "message": failure.error.message(),
        }
    });
    let text = serde_json::to_string_pretty(&data)
        .map_err(|error| Error::new(ErrorKind::Io, format!("serialize tool error: {error}")))?;
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": data,
        "isError": true,
    }))
}

fn decode_arguments<T: for<'de> Deserialize<'de>>(
    arguments: Value,
) -> std::result::Result<T, ToolFailure> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolFailure::from(invalid(format!("invalid tool arguments: {error}"))))
}

fn parse_id<T>(value: &str, field: &str) -> std::result::Result<T, ToolFailure>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse()
        .map_err(|error| invalid(format!("invalid {field}: {error}")).into())
}

fn find_context(
    snapshot: &DomainSnapshot,
    space_id: Option<SpaceId>,
    context_id: ContextId,
) -> std::result::Result<(SpaceId, &sctx_domain::ContextProjection), ToolFailure> {
    if let Some(space_id) = space_id {
        let context = snapshot
            .projection
            .spaces
            .get(&space_id)
            .and_then(|space| space.contexts.get(&context_id))
            .ok_or_else(|| {
                ToolFailure::from(invalid(format!(
                    "Context {context_id} does not belong to Space {space_id}"
                )))
            })?;
        return Ok((space_id, context));
    }
    snapshot
        .projection
        .spaces
        .iter()
        .find_map(|(space_id, space)| {
            space
                .contexts
                .get(&context_id)
                .map(|context| (*space_id, context))
        })
        .ok_or_else(|| ToolFailure::from(invalid(format!("Context does not exist: {context_id}"))))
}

fn context_conflicts(snapshot: &DomainSnapshot, context_id: ContextId) -> Vec<Value> {
    let mut conflicts = Vec::new();
    for conflict in snapshot.projection.semantic_conflicts.values() {
        if conflict
            .conflict
            .participants
            .iter()
            .any(|participant| participant.context_id == context_id)
        {
            conflicts.push(json!({
                "kind": "semantic",
                "conflict_id": conflict.conflict.conflict_id,
                "status": conflict.status,
                "participants": conflict.conflict.participants,
                "reason": conflict.conflict.reason,
            }));
        }
    }
    if let Some(context) = snapshot
        .projection
        .spaces
        .values()
        .find_map(|space| space.contexts.get(&context_id))
        && context.publication_heads.len() > 1
    {
        conflicts.push(json!({
            "kind": "governance",
            "status": "open",
            "publication_heads": context.publication_heads,
            "reason": "multiple publication heads",
        }));
    }
    conflicts
}

fn collect_conflicts(results: &[sctx_search::SearchResult]) -> Vec<ConflictView> {
    let mut by_id = BTreeMap::new();
    for conflict in results.iter().flat_map(|result| &result.conflicts) {
        by_id
            .entry(conflict.conflict_id.clone())
            .or_insert_with(|| conflict.clone());
    }
    by_id.into_values().collect()
}

fn insert_fields<const N: usize>(
    data: &mut Value,
    fields: [(&str, Value); N],
) -> std::result::Result<(), ToolFailure> {
    let object = data
        .as_object_mut()
        .ok_or_else(|| ToolFailure::from(invariant("serialized response is not an object")))?;
    for (name, value) in fields {
        object.insert(name.to_owned(), value);
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn serialization_failure(error: serde_json::Error) -> ToolFailure {
    ToolFailure::from(Error::new(
        ErrorKind::Io,
        format!("serialize MCP response: {error}"),
    ))
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

const fn default_page_size() -> usize {
    20
}

const fn default_task_capture_list_limit() -> usize {
    DEFAULT_TASK_CAPTURE_LIST_LIMIT
}

const fn normalized_capture_kind(kind: BreadcrumbKind) -> NormalizedBreadcrumbKind {
    match kind {
        BreadcrumbKind::FileAccess => NormalizedBreadcrumbKind::Exploration,
        BreadcrumbKind::TestResult => NormalizedBreadcrumbKind::Validation,
        BreadcrumbKind::ToolOutcome => NormalizedBreadcrumbKind::Implementation,
        BreadcrumbKind::Checkpoint => NormalizedBreadcrumbKind::Decision,
    }
}

fn checkpoint_contains_capture(input: &TaskCheckpointInput) -> bool {
    input.claims.iter().any(|claim| {
        claim
            .evidence
            .iter()
            .any(|evidence| matches!(evidence, TaskCheckpointEvidenceInput::Capture { .. }))
    })
}

fn capture_runtime_diagnostics(
    diagnostics: &[CaptureDiagnosticKind],
) -> Vec<WorkEpisodeDiagnosticKind> {
    diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            CaptureDiagnosticKind::RepositoryNotConfigured => {
                Some(WorkEpisodeDiagnosticKind::CaptureRepositoryNotConfigured)
            }
            CaptureDiagnosticKind::UnsafeArtifactPath => {
                Some(WorkEpisodeDiagnosticKind::CaptureUnsafeArtifactPath)
            }
            CaptureDiagnosticKind::NoActiveTask | CaptureDiagnosticKind::RuntimeUnavailable => None,
        })
        .collect()
}

const fn default_token_budget() -> usize {
    2_000
}

const fn default_max_spaces() -> usize {
    DEFAULT_TASK_MAX_SPACES
}

const fn default_scan_artifact_limit() -> usize {
    DEFAULT_SCAN_ARTIFACT_LIMIT
}

const fn default_candidate_analysis_token_budget() -> usize {
    4_096
}

const fn default_candidate_analysis_top_k() -> usize {
    16
}

const fn default_candidate_review_status() -> CandidateReviewStatus {
    CandidateReviewStatus::Pending
}

const fn default_candidate_review_list_limit() -> usize {
    DEFAULT_CANDIDATE_REVIEW_LIST_LIMIT
}

const fn default_candidate_review_token_budget() -> usize {
    DEFAULT_CANDIDATE_REVIEW_TOKEN_BUDGET
}

fn estimate_candidate_review_tokens(summary: &CandidateReviewSummary) -> Result<usize> {
    let bytes = serde_json::to_vec(summary)
        .map_err(|error| Error::new(ErrorKind::Io, format!("serialize Review summary: {error}")))?;
    Ok(bytes.len().div_ceil(4).max(1))
}

const fn error_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvariantViolation => "invariant_violation",
        ErrorKind::Io => "io_error",
        ErrorKind::External => "external_error",
        ErrorKind::Conflict => "conflict",
        ErrorKind::StaleState => "stale_state",
        ErrorKind::PrivacyRejected => "privacy_rejected",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::RepositoryNotConfigured => "repository_not_configured",
        ErrorKind::IdempotencyKeyConflict => "idempotency_key_conflict",
        ErrorKind::MaintenanceBusy => "maintenance_busy",
        _ => "unknown_error",
    }
}

fn validate_context_revision_ref(
    snapshot: &DomainSnapshot,
    reference: ContextRevisionRef,
) -> Result<()> {
    let (_, context) =
        find_context(snapshot, None, reference.context_id).map_err(|failure| failure.error)?;
    if !context.revisions.contains_key(&reference.revision_id) {
        return Err(invalid(format!(
            "Revision {} does not belong to Context {} in the selected Index snapshot",
            reference.revision_id, reference.context_id
        )));
    }
    Ok(())
}

fn validate_context_evidence_ref(
    snapshot: &DomainSnapshot,
    reference: &CaptureEvidenceRef,
) -> Result<()> {
    let CaptureEvidenceRef::ContextEvidence {
        context_id,
        revision_id,
        evidence_id,
    } = reference
    else {
        return Err(invariant(
            "Context Evidence validator received a non-Context reference",
        ));
    };
    let (_, context) =
        find_context(snapshot, None, *context_id).map_err(|failure| failure.error)?;
    let revision = context.revisions.get(revision_id).ok_or_else(|| {
        invalid(format!(
            "Revision {revision_id} does not belong to Context {context_id} in the selected Index snapshot"
        ))
    })?;
    if !revision
        .revision
        .evidence
        .iter()
        .any(|evidence| evidence.evidence_id == *evidence_id)
    {
        return Err(invalid(format!(
            "Evidence {evidence_id} does not belong to Context {context_id} Revision {revision_id}"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn unavailable(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::External, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn parse_id_value<T>(value: &str, field: &str) -> Result<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse()
        .map_err(|error| invalid(format!("invalid {field}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_update_conflict_and_stale_errors_have_intent_specific_codes() {
        let conflict = ToolFailure::intent_update_failed(Error::new(
            ErrorKind::Conflict,
            "divergent initial Working Intent",
        ));
        let stale = ToolFailure::intent_update_failed(Error::new(
            ErrorKind::StaleState,
            "stale Working Intent parent",
        ));
        assert_eq!(conflict.code, "intent_conflict");
        assert_eq!(stale.code, "intent_stale");
    }
}
