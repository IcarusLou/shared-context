//! Stdio Model Context Protocol server for Shared Context V1 tools.
//!
//! The transport accepts the newline-delimited framing used by current MCP
//! clients and the `Content-Length` framing used by older fixtures. Read tools
//! are pinned to one projection snapshot or one explicitly named Git tree. The
//! durable write tool delegates ID generation and append-only enforcement to
//! the domain event constructor and [`sctx_git_store::GitStore`]; `task_context`
//! writes only disposable local Task Runtime state.

use std::{
    cell::OnceCell,
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
    CandidateAnalysisStatus, CandidateAssessmentRelation, CandidateBuilderProvenance,
    CandidateConfidence, CandidateConfirmationOperation, CandidateConfirmationPlan,
    CandidateConfirmationPrimaryReference, CandidateId, CandidatePrimarySelection,
    CandidateRelationAssessment, CandidateReviewDiagnostic, CandidateReviewStatus,
    CandidateReviewSummary, CandidateReviewView, CandidateSpaceRecommendation,
    CandidateSpaceRecommendationPath, CheckpointClaim, CheckpointClaimId, CheckpointEvidenceRef,
    CheckpointUnknown, ConflictParticipant, ContextGovernanceStatus, ContextId, ContextKind,
    ContextRelation, ContextRelationKind, ContextRevisionDraft, EngineeringReferenceDraft, Error,
    ErrorKind, EventId, EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator,
    IntentSnapshot, NormalizedWorkObservation, OptionalCandidateEdits, ProblemViewEdit,
    ProposedSpaceGroupKey, REPOSITORY_ID_MAX_BYTES, REPOSITORY_ID_PATTERN, RecommendedSpaceRole,
    ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId, ResolutionStatus,
    ResolvedFocus, Result, RevisionId, SemanticConflictOpeningDraft, SemanticConflictStatus,
    SignalId, SpaceId, SpaceRecommendationId, SubmissionId, TaskId, TaskIntentRevisionId,
    TaskSessionId, TaskSessionSnapshot, TaskSignalKind, TaskSignalLifecycle, TaskSignalRecord,
    TaskSpaceAssociation, WorkEpisodeId, WorkEpisodeRef, WorkEpisodeStatus, WorkObservation,
    WorkObservationId, WorkingIntentSnapshot, context_revision_as_draft,
    context_revision_content_hash,
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
    AppendBatchOutcome, AppendRequest, CandidateConfirmationWriteStatus,
    CandidateSubmissionRequest, CandidateSubmissionStatus, GitStore,
    MAX_CANDIDATE_SUBMISSION_BATCH,
};
use sctx_index::{DomainSnapshot, ProjectionIndex};
use sctx_local_state::{
    AuthorizedSessionScope, AuthorizedSessionScopeRead, AuthorizedSessionScopeStore,
    MaintenanceLock, PrivacyScanner, RepositoryCatalogSnapshot, UserConfigStore,
};
use sctx_search::{
    AutomaticQueryTokenExplanation, CandidateAnalysisRequest, CompactSpaceAssociation,
    CompactTaskContextItem, ConflictView, ContextPackDetailLevel, ContextPackOmitted,
    ContextStatus, ContextTtlSettings, ContextUsageCounts, DEFAULT_TASK_MAX_SPACES,
    MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET, MAX_CANDIDATE_ANALYSIS_TOP_K, MAX_TASK_MAX_SPACES,
    MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET, MIN_TASK_CONTEXT_TOKEN_BUDGET, ScopeFilter, SearchEngine,
    SearchFilters, SearchMatchMode, SearchRequest, TaskContextItem, TaskContextRequest,
    TaskGraphDiagnostic, TaskRetrievalPath, UsagePriorSource,
};
use sctx_task_runtime::{
    AgentCheckpointSubmission, CandidateBuildDuplicatePreparation, CandidateBuildItemPreparation,
    CandidateBuildItemStatus, CandidateBuildStatus, CandidateBuildView,
    CandidateConfirmationFinalize, CandidateReviewDiscard, CandidateReviewDiscardStatus,
    CandidateReviewRecord, CheckoutReferenceResolver, ContextInjectionSource, ContextUsageOutcome,
    ContextUsageRecord, DirectCheckpointClaimDraft, DirectEvidenceDraft, InjectedContext,
    IntentRevisionWriteStatus, ProposedSpaceGroupMappingStatus, TaskRuntime, WorkEpisodeView,
    reference_derivation,
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
const MAX_TASK_CHECKPOINT_BYTES: usize = 64 * 1024;
/// Normalized statement token Jaccard above which a Checkpoint Claim counts as restating one
/// injected Context.
const USAGE_REUSED_SIMILARITY_BASIS_POINTS: u16 = 6_000;
const MAX_TASK_CHECKPOINT_CLAIMS: usize = 64;
const MAX_TASK_CHECKPOINT_UNKNOWNS: usize = 64;
const MAX_TASK_CHECKPOINT_EVIDENCE_PER_CLAIM: usize = 32;
const MAX_TASK_CHECKPOINT_LIST_ITEMS: usize = 64;
const MAX_TASK_CHECKPOINT_TEXT_BYTES: usize = 4 * 1024;
const MAX_RECOVERABLE_BUILDS_PER_READ: usize = 32;
/// Token Jaccard at or above which one Claim is treated as restating an existing Candidate.
/// It is a Builder-local deduplication threshold, never a knowledge relation or analysis fact.
const CANDIDATE_DUPLICATE_SIMILARITY_BASIS_POINTS: u16 = 8_000;

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

/// One self-contained, Agent-attested Evidence draft for a Checkpoint Claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpointEvidenceInput {
    pub evidence_type: EvidenceType,
    pub summary: String,
    pub limitations: Vec<String>,
}

/// Complete Claim draft without caller-owned Claim or Observation identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpointClaimInput {
    pub context_kind: ContextKind,
    pub statement: String,
    pub rationale: String,
    pub conditions: Vec<String>,
    pub evidence: Vec<TaskCheckpointEvidenceInput>,
}

/// One unresolved Agent-authored question with no model-owned lifecycle metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpointUnknownInput {
    pub statement: String,
    pub blocking: bool,
}

/// Strict public Agent Checkpoint request. Runtime ownership and close guards are server-owned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpointInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub claims: Vec<TaskCheckpointClaimInput>,
    pub unknowns: Vec<TaskCheckpointUnknownInput>,
}

/// Safe, typed Checkpoint diagnostic without Agent-authored source text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCheckpointDiagnostic {
    InlineValidationRecorded { observation_id: WorkObservationId },
}

/// One accepted server-owned Checkpoint and resulting closed Episode boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCheckpointAcceptedResponse {
    pub status: TaskCheckpointAcceptedStatus,
    pub operation_id: String,
    pub checkpoint_id: AgentCheckpointId,
    pub claim_ids: Vec<CheckpointClaimId>,
    pub episode_id: WorkEpisodeId,
    pub episode_version: u64,
    pub episode_status: WorkEpisodeStatus,
    pub replayed: bool,
    pub diagnostics: Vec<TaskCheckpointDiagnostic>,
    pub candidate_build: CandidateBuildReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCheckpointAcceptedStatus {
    Accepted,
}

/// Durable Build outbox identity and current recovery status returned by Checkpoint ACK.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateBuildReceipt {
    pub build_id: sctx_domain::CandidateBuildId,
    pub episode_id: WorkEpisodeId,
    pub status: CandidateBuildResponseStatus,
}

/// Successful Checkpoint result. Empty submissions are explicit mutation-free no-ops.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskCheckpointResponse {
    Accepted(TaskCheckpointAcceptedResponse),
    NoOp(TaskCheckpointNoOpResponse),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCheckpointNoOpResponse {
    pub status: TaskCheckpointNoOpStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCheckpointNoOpStatus {
    NoOp,
}

impl TaskCheckpointResponse {
    /// Returns the persisted Checkpoint result, or `None` for a successful no-op.
    #[must_use]
    pub const fn accepted(&self) -> Option<&TaskCheckpointAcceptedResponse> {
        match self {
            Self::Accepted(response) => Some(response),
            Self::NoOp(_) => None,
        }
    }

    /// Consumes the response and returns the persisted Checkpoint result when one exists.
    #[must_use]
    pub fn into_accepted(self) -> Option<TaskCheckpointAcceptedResponse> {
        match self {
            Self::Accepted(response) => Some(response),
            Self::NoOp(_) => None,
        }
    }
}

/// Aggregate state returned after deterministically building one closed Episode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateBuildResponse {
    pub build_id: sctx_domain::CandidateBuildId,
    pub episode_id: WorkEpisodeId,
    pub status: CandidateBuildResponseStatus,
    pub items: Vec<CandidateBuildItemSummary>,
    pub duplicates: Vec<CandidateBuildDuplicateSummary>,
}

/// One Claim the Builder collapsed onto an existing Candidate instead of proposing it twice.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateBuildDuplicateSummary {
    pub checkpoint_id: AgentCheckpointId,
    pub claim_id: CheckpointClaimId,
    pub duplicate_of_candidate_id: sctx_domain::CandidateId,
    pub similarity_basis_points: u16,
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
    pub unknowns: Vec<CheckpointUnknown>,
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

/// Number of accepted Contexts at which a provisional Space has stopped being a scratch bucket
/// and is worth naming or folding into a human-defined Space.
pub const PROVISIONAL_SPACE_MERGE_THRESHOLD: usize = 5;

/// Upper bound on advisories one Candidate list carries. The advisory is a nudge, not a report.
const MAX_SPACE_ADVISORIES: usize = 5;

/// Non-binding hint that one server-proposed Space now deserves a human decision.
///
/// Nothing is merged or renamed automatically: the advisory only names the Space and says why it
/// showed up, and the reviewer decides whether to name it or move its Contexts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpaceAdvisory {
    pub space_id: SpaceId,
    pub title: String,
    pub reason: String,
}

/// Collects the Spaces whose single current Intent head is still the server's proposal.
///
/// A Space with conflicting Intent heads is never reported as provisional: no head won, so no
/// head can speak for the Space.
fn provisional_space_ids(snapshot: &DomainSnapshot) -> BTreeSet<SpaceId> {
    snapshot
        .projection
        .spaces
        .iter()
        .filter(|(_, space)| {
            space.intent.heads.len() == 1
                && space
                    .intent
                    .heads
                    .first()
                    .and_then(|revision_id| space.intent.revisions.get(revision_id))
                    .is_some_and(|revision| revision.provisional)
        })
        .map(|(space_id, _)| *space_id)
        .collect()
}

/// Builds the merge advisories for every provisional Space that has grown past a review nudge.
///
/// Two independent signals qualify a Space, and both are read from the same immutable projection
/// the rest of the response is read from:
///
/// * it has accumulated at least [`PROVISIONAL_SPACE_MERGE_THRESHOLD`] accepted Contexts, or
/// * an accepted Context in a different Space points at one of its Contexts with a `related_to`
///   relation, which means the knowledge is already being read as part of a named boundary.
fn provisional_space_advisories(snapshot: &DomainSnapshot) -> Vec<SpaceAdvisory> {
    let provisional = provisional_space_ids(snapshot);
    if provisional.is_empty() {
        return Vec::new();
    }
    let owners = snapshot
        .projection
        .spaces
        .iter()
        .flat_map(|(space_id, space)| {
            space
                .contexts
                .keys()
                .map(move |context_id| (*context_id, *space_id))
        })
        .collect::<BTreeMap<_, _>>();
    let mut incoming_related = BTreeMap::<SpaceId, usize>::new();
    for (space_id, space) in &snapshot.projection.spaces {
        for context in space.contexts.values() {
            let ContextGovernanceStatus::Accepted { revision_id, .. } = &context.governance else {
                continue;
            };
            let Some(revision) = context.revisions.get(revision_id) else {
                continue;
            };
            for relation in &revision.revision.relations {
                if relation.kind != ContextRelationKind::RelatedTo {
                    continue;
                }
                let Some(target_space_id) = owners.get(&relation.target_context_id).copied() else {
                    continue;
                };
                if target_space_id == *space_id || !provisional.contains(&target_space_id) {
                    continue;
                }
                *incoming_related.entry(target_space_id).or_default() += 1;
            }
        }
    }
    let mut advisories = Vec::new();
    for space_id in provisional {
        let Some(space) = snapshot.projection.spaces.get(&space_id) else {
            continue;
        };
        let accepted = space
            .contexts
            .values()
            .filter(|context| {
                matches!(context.governance, ContextGovernanceStatus::Accepted { .. })
            })
            .count();
        let referenced = incoming_related.get(&space_id).copied().unwrap_or(0);
        if accepted < PROVISIONAL_SPACE_MERGE_THRESHOLD && referenced == 0 {
            continue;
        }
        let title = space
            .intent
            .heads
            .first()
            .and_then(|revision_id| space.intent.revisions.get(revision_id))
            .map(|revision| revision.intent.title.clone())
            .unwrap_or_default();
        let referenced_clause = if referenced == 0 {
            String::new()
        } else {
            format!(" and {referenced} related Context relations from other Spaces")
        };
        advisories.push(SpaceAdvisory {
            space_id,
            title,
            reason: format!(
                "Provisional Space has {accepted} accepted Contexts{referenced_clause}; consider `sctx space intent revise` to name it or merge into a human-defined Space"
            ),
        });
        if advisories.len() == MAX_SPACE_ADVISORIES {
            break;
        }
    }
    advisories
}

/// Stable page of whole untrusted Summaries; no Review content is truncated.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateListResponse {
    pub reviews: Vec<CandidateReviewSummary>,
    pub detail_level: ContextPackDetailLevel,
    /// Compact projection of the same budgeted page; empty under `full`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_reviews: Vec<CompactCandidateReview>,
    /// Merge nudges for provisional Spaces, outside the per-Review token budget. Never present
    /// when no provisional Space has grown past the threshold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub space_advisories: Vec<SpaceAdvisory>,
    /// Spaces whose current Intent head is still the server's proposal. This is an in-process
    /// aid for marking the Full Review rows; it never reaches the wire.
    #[serde(skip)]
    pub provisional_space_ids: BTreeSet<SpaceId>,
    pub omitted: Vec<CandidateReviewOmitted>,
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    pub token_budget: usize,
    pub recovery: CandidateRecoverySummary,
}

impl CandidateListResponse {
    /// Projects the page onto the triage list an Agent reads before expanding one Review.
    #[must_use]
    pub fn compact(&self) -> CompactCandidateListResponse {
        CompactCandidateListResponse {
            reviews: self.compact_reviews.clone(),
            detail_level: ContextPackDetailLevel::Compact,
            space_advisories: self.space_advisories.clone(),
            omitted: self.omitted.clone(),
            next_cursor: self.next_cursor.clone(),
            estimated_tokens: self.estimated_tokens,
            token_budget: self.token_budget,
            recovery: self.recovery,
        }
    }
}

/// Compact Candidate Review triage page. Every entry stays addressable by `candidate_id`, and
/// `candidate_get` still returns the whole untrusted Review.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactCandidateListResponse {
    pub reviews: Vec<CompactCandidateReview>,
    pub detail_level: ContextPackDetailLevel,
    /// Merge nudges for provisional Spaces, outside the per-Review token budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub space_advisories: Vec<SpaceAdvisory>,
    pub omitted: Vec<CandidateReviewOmitted>,
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    pub token_budget: usize,
    pub recovery: CandidateRecoverySummary,
}

/// Strongest relation the analyzer found for one Candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactCandidateAssessment {
    pub relation: CandidateAssessmentRelation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_context_id: Option<ContextId>,
    pub confidence_basis_points: u16,
}

/// Non-binding primary Space recommendation, reduced to what a reviewer decides on.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactSpaceRecommendation {
    Existing {
        recommendation_id: SpaceRecommendationId,
        space_id: SpaceId,
        role: RecommendedSpaceRole,
        /// True when the recommended Space is still the server's unnamed provisional proposal.
        provisional: bool,
    },
    ProposedNewSpaceIntent {
        recommendation_id: SpaceRecommendationId,
        title: String,
        /// Always true: confirming a proposed recommendation opens a provisional Space.
        provisional: bool,
    },
}

/// One compact Candidate Review row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactCandidateReview {
    pub candidate_id: sctx_domain::CandidateId,
    pub kind: ContextKind,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_assessment: Option<CompactCandidateAssessment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_space_recommendation: Option<CompactSpaceRecommendation>,
    pub ready_for_review: bool,
    /// Non-blocking reminder that the knowledge base default language is Chinese. See
    /// [`language_hint`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_hint: Option<String>,
    /// Whether `applicability.domains`/`platforms` came from the Task Working Intent rather than
    /// from the Claim. See [`APPLICABILITY_INHERITED`].
    pub applicability_inherited: bool,
    /// Always true; the row stays marked as untrusted Agent-authored content.
    pub untrusted_data: bool,
}

/// Every Candidate reaches Candidate Build through a direct Checkpoint, and a direct Checkpoint
/// Claim carries only `conditions`: its `domains` and `platforms` are always copied from the Task
/// Working Intent. Runtime keeps no per-Claim flag, so the response reports the constant this is
/// rather than inventing a per-Candidate answer.
const APPLICABILITY_INHERITED: bool = true;

/// Advisory text offered when a Candidate statement carries no Chinese at all.
const CHINESE_KNOWLEDGE_BASE_HINT: &str =
    "knowledge base default language is Chinese; consider restating in Chinese";

/// Non-blocking language advisory for one Candidate statement.
///
/// The knowledge base is written in Chinese, but the Checkpoint contract stays language-agnostic:
/// nothing is rejected or rewritten here. A statement with no CJK character at all simply carries a
/// reminder a reviewer may act on. Code identifiers, paths, and commands legitimately stay Latin,
/// so the test is "contains no Chinese", never "contains only Chinese".
fn language_hint(statement: &str) -> Option<String> {
    (!statement.chars().any(is_cjk)).then(|| CHINESE_KNOWLEDGE_BASE_HINT.to_owned())
}

/// True for the CJK ranges a Chinese statement is written in.
const fn is_cjk(character: char) -> bool {
    matches!(character,
        '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2a6df}')
}

/// Projects one whole Review onto its compact triage row.
fn compact_candidate_review(
    review: &CandidateReviewView,
    provisional_space_ids: &BTreeSet<SpaceId>,
) -> CompactCandidateReview {
    let top_assessment = review
        .analysis
        .assessments
        .iter()
        .max_by_key(|assessment| assessment.confidence.basis_points)
        .map(|assessment| CompactCandidateAssessment {
            relation: assessment.relation,
            target_context_id: assessment.target.map(|target| target.context_id),
            confidence_basis_points: assessment.confidence.basis_points,
        });
    let primary = review
        .space_recommendations
        .iter()
        .find(|recommendation| {
            matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing {
                    role: RecommendedSpaceRole::Primary,
                    ..
                }
            )
        })
        .or_else(|| {
            review.space_recommendations.iter().find(|recommendation| {
                matches!(
                    recommendation,
                    CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
                )
            })
        })
        .or_else(|| review.space_recommendations.first());
    let primary_space_recommendation = primary.map(|recommendation| match recommendation {
        CandidateSpaceRecommendation::Existing {
            recommendation_id,
            space_id,
            role,
            ..
        } => CompactSpaceRecommendation::Existing {
            recommendation_id: *recommendation_id,
            space_id: *space_id,
            role: *role,
            provisional: provisional_space_ids.contains(space_id),
        },
        CandidateSpaceRecommendation::ProposedNewSpaceIntent {
            recommendation_id,
            proposed_new_space_intent,
            ..
        } => CompactSpaceRecommendation::ProposedNewSpaceIntent {
            recommendation_id: *recommendation_id,
            title: proposed_new_space_intent.title.clone(),
            provisional: true,
        },
    });
    CompactCandidateReview {
        candidate_id: review.candidate_id,
        kind: review.content.kind,
        statement: review.content.statement.clone(),
        top_assessment,
        primary_space_recommendation,
        ready_for_review: review.ready_for_review,
        language_hint: language_hint(&review.content.statement),
        applicability_inherited: APPLICABILITY_INHERITED,
        untrusted_data: review.untrusted_data,
    }
}

/// Non-sensitive current status of bounded Candidate Build recovery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateRecoverySummary {
    pub attempted: usize,
    pub recovered: usize,
    pub failed_attempts: usize,
    pub pending: usize,
    pub incomplete: usize,
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

/// Strict explicit human confirmation of several owned Pending Candidates.
///
/// It shares one Space organization and one Review version across the batch; per-Candidate field
/// `edits` and proposed new Space recommendations stay single-Candidate operations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfirmBatchInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub expected_task_id: String,
    pub expected_intent_revision_id: String,
    pub candidate_ids: Vec<String>,
    pub expected_review_version: u64,
    pub primary: CandidateConfirmPrimaryInput,
    pub related_space_ids: Vec<String>,
}

/// Batch discard request; the Review version and reason apply to every listed Candidate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDiscardBatchInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub expected_task_id: String,
    pub expected_intent_revision_id: String,
    pub candidate_ids: Vec<String>,
    pub expected_review_version: u64,
    pub reason: String,
}

/// Either public shape accepted by `candidate_confirm`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CandidateConfirmRequest {
    Single(Box<CandidateConfirmInput>),
    Batch(Box<CandidateConfirmBatchInput>),
}

/// Either public shape accepted by `candidate_discard`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CandidateDiscardRequest {
    Single(Box<CandidateDiscardInput>),
    Batch(Box<CandidateDiscardBatchInput>),
}

/// Result of one atomically validated Candidate Confirmation batch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateConfirmBatchResponse {
    pub status: CandidateConfirmResponseStatus,
    pub confirmations: Vec<CandidateConfirmResponse>,
}

/// Result of one atomic Candidate Review discard batch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateDiscardBatchResponse {
    pub status: CandidateDiscardResponseStatus,
    pub reviews: Vec<CandidateReviewView>,
}

/// Either public shape returned by `candidate_confirm`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CandidateConfirmOutcome {
    Single(Box<CandidateConfirmResponse>),
    Batch(CandidateConfirmBatchResponse),
}

/// Either public shape returned by `candidate_discard`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CandidateDiscardOutcome {
    Single(Box<CandidateDiscardResponse>),
    Batch(CandidateDiscardBatchResponse),
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
    pub graph_rebuild_pending: bool,
}

/// Public Claim-scoped submission state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateBuildItemResponseStatus {
    Queued,
    Prepared,
    NeedsEvidence,
    Created,
    AlreadyExists,
    Failed,
}

impl From<CandidateBuildItemStatus> for CandidateBuildItemResponseStatus {
    fn from(value: CandidateBuildItemStatus) -> Self {
        match value {
            CandidateBuildItemStatus::Queued => Self::Queued,
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

/// Whether one Working Intent call created a Revision, matched current canonical semantics, or
/// forked a parallel `TaskSession` for a concurrent Agent sharing one `external_session_id`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentRevisionStatus {
    Created,
    AlreadyCurrent,
    Forked,
}

impl From<IntentRevisionWriteStatus> for IntentRevisionStatus {
    fn from(value: IntentRevisionWriteStatus) -> Self {
        match value {
            IntentRevisionWriteStatus::Created => Self::Created,
            IntentRevisionWriteStatus::AlreadyCurrent => Self::AlreadyCurrent,
            IntentRevisionWriteStatus::Forked => Self::Forked,
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
///
/// The Rust entry points always build the explainable [`ContextPackDetailLevel::Full`] shape.
/// The MCP tool surface defaults to `compact` and returns [`CompactTaskContextResponse`] instead,
/// which is this response projected onto the fields an Agent needs to inherit a fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContextResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub detail_level: ContextPackDetailLevel,
    pub candidate_spaces: Vec<TaskSpaceAssociation>,
    /// Compact projection of the surviving Space associations; empty under `full`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_candidate_spaces: Vec<CompactSpaceAssociation>,
    pub items: Vec<TaskContextItem>,
    /// Compact projection of the same budgeted selection; empty under `full`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_items: Vec<CompactTaskContextItem>,
    pub retrieval_paths: Vec<TaskContextRetrievalPaths>,
    pub graph_diagnostics: Vec<TaskGraphDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_token_explanation: Option<AutomaticQueryTokenExplanation>,
    pub task_fingerprint: String,
    pub tree: String,
    pub generation: u64,
    pub artifact_generation: Option<String>,
    pub graph_context_tree_oid: Option<String>,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub omitted: Vec<ContextPackOmitted>,
}

impl TaskContextResponse {
    /// Projects the budgeted selection onto the compact injection payload. Ranking, fusion, and
    /// per-item Retrieval Path details are dropped; each item keeps its `relations` and the
    /// deduplicated `retrieval_channels` names, while compact `candidate_spaces` carry only
    /// `space_id`, `title`, `score`, `provisional`, and up to two human-readable reasons.
    #[must_use]
    pub fn compact(&self) -> CompactTaskContextResponse {
        CompactTaskContextResponse {
            task_session_id: self.task_session_id,
            task_id: self.task_id,
            intent_revision_id: self.intent_revision_id,
            detail_level: ContextPackDetailLevel::Compact,
            candidate_spaces: self.compact_candidate_spaces.clone(),
            items: self.compact_items.clone(),
            graph_diagnostics: self.graph_diagnostics.clone(),
            query_token_explanation: self.query_token_explanation.clone(),
            task_fingerprint: self.task_fingerprint.clone(),
            tree: self.tree.clone(),
            generation: self.generation,
            token_budget: self.token_budget,
            estimated_tokens: self.estimated_tokens,
            omitted: self.omitted.clone(),
        }
    }
}

/// Compact Task Context payload. It keeps identity, budget accounting, and the inheritable facts
/// while dropping every explanation channel that only a debugging reader can act on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactTaskContextResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub detail_level: ContextPackDetailLevel,
    pub candidate_spaces: Vec<CompactSpaceAssociation>,
    pub items: Vec<CompactTaskContextItem>,
    pub graph_diagnostics: Vec<TaskGraphDiagnostic>,
    /// Automatic query-token selection, projected onto its totals plus a few named drops. It is
    /// the one explanation a compact Pack keeps: a Pack that came back with nothing still says
    /// how much of the question this Tree could answer at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_token_explanation: Option<AutomaticQueryTokenExplanation>,
    pub task_fingerprint: String,
    pub tree: String,
    pub generation: u64,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub omitted: Vec<ContextPackOmitted>,
}

/// Compact form of [`TaskIntentUpdateResponse`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactTaskIntentUpdateResponse {
    #[serde(flatten)]
    pub context: CompactTaskContextResponse,
    pub revision_status: IntentRevisionStatus,
    pub active_signals: Vec<TaskSignalRecord>,
}

/// Result of one request-local Focus resolution and its immediate Task Context retrieval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactFocusQueryResponse {
    pub resolved_focus: ResolvedFocus,
    pub context: TaskContextResponse,
}

/// Compact form of [`ArtifactFocusQueryResponse`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactArtifactFocusQueryResponse {
    pub resolved_focus: ResolvedFocus,
    pub context: CompactTaskContextResponse,
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

/// Explicit Space creation input. The Intent carries exactly the fields `sctx space create`
/// requires and is validated by the same [`IntentSnapshot::validate`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceCreateInput {
    pub intent: IntentSnapshot,
}

/// One Space a human or Agent named on purpose; it is never provisional.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpaceCreateResponse {
    pub space_id: SpaceId,
    pub intent_revision_id: RevisionId,
    pub provisional: bool,
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

/// Prefix of the only structured `recheck_when` entries the server evaluates.
pub const RECHECK_BRANCH_ADVANCED_PREFIX: &str = "branch_advanced:";

/// Prefix of the structured `file_changed_since:<commit>:<repository-relative path>` entry.
pub const RECHECK_FILE_CHANGED_SINCE_PREFIX: &str = "file_changed_since:";

/// One Context's structured `recheck_when` evaluation on this machine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextRecheckResult {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    /// Why this Context is considered stale; absent means every structured entry still holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    /// Structured entries no local checkout could answer. They are not a failure: an unreachable
    /// checkout, a missing commit, an absent `git`, or a timeout all land here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unevaluated: Vec<String>,
}

/// Outcome of one `recheck_when` evaluation pass over every accepted Context.
///
/// The result is written to the local `context_item.stale_reason` projection column only. It is
/// never an Event: staleness is this machine's reading of its own checkouts, not a shared fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextRecheckResponse {
    pub evaluated_contexts: usize,
    pub stale_contexts: usize,
    pub unevaluated_entries: usize,
    pub results: Vec<ContextRecheckResult>,
    pub tree: String,
    pub generation: u64,
}

/// Structured `recheck_when` entry the server can answer deterministically from a local checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
enum RecheckCondition {
    /// `branch_advanced:<branch>@<commit>` — the named branch no longer points at `<commit>`.
    BranchAdvanced { branch: String, commit: String },
    /// `file_changed_since:<commit>:<path>` — `<path>` changed between `<commit>` and `HEAD`.
    FileChangedSince { commit: String, path: String },
}

impl RecheckCondition {
    /// Parses the structured subset. Every other `recheck_when` entry stays free text and is
    /// returned untouched to the reader.
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if let Some(rest) = entry.strip_prefix(RECHECK_BRANCH_ADVANCED_PREFIX) {
            let (branch, commit) = rest.rsplit_once('@')?;
            let (branch, commit) = (branch.trim(), commit.trim());
            return (!branch.is_empty() && is_commit_ish(commit)).then(|| Self::BranchAdvanced {
                branch: branch.to_owned(),
                commit: commit.to_owned(),
            });
        }
        let rest = entry.strip_prefix(RECHECK_FILE_CHANGED_SINCE_PREFIX)?;
        let (commit, path) = rest.split_once(':')?;
        let (commit, path) = (commit.trim(), path.trim());
        (is_commit_ish(commit) && !path.is_empty() && !path.starts_with('-')).then(|| {
            Self::FileChangedSince {
                commit: commit.to_owned(),
                path: path.to_owned(),
            }
        })
    }
}

/// Accepts only abbreviated-or-full hexadecimal object names, so no entry can smuggle a Git flag.
fn is_commit_ish(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    /// Installation root. Candidate Build resolves the checkout of the `ExternalSession` that
    /// authored a Checkpoint, which is not always the session draining the outbox.
    root: PathBuf,
    /// Append-only Knowledge Store, opened on first write.
    ///
    /// `GitStore::open_existing` verifies the fixed worktree with two `git` child processes,
    /// which is the single largest fixed cost of opening this Runtime. Read-only tools never
    /// append an event, so they never pay it; every writing tool still opens and verifies the
    /// Store before it writes anything.
    store: OnceCell<GitStore>,
    index: ProjectionIndex,
    repositories: RepositoryRegistry,
    engineering_graph: Option<EngineeringProjectionStore>,
    tasks: TaskRuntime,
    catalog: RepositoryCatalogSnapshot,
    /// Explicit `[context_ttl]` policy. Contexts past their configured lifetime are `historical`:
    /// still searchable and explainable, never automatically injected.
    context_ttl: ContextTtlSettings,
    /// Activation scope that authorized this exact call, when the caller is public MCP dispatch.
    /// Internal entry points carry `None` and fall back to the scope recorded for the Episode's
    /// own `ExternalSession`.
    session_scope: Option<AuthorizedSessionScope>,
}

#[derive(Clone)]
struct ClaimBuildMaterial {
    checkpoint_id: AgentCheckpointId,
    claim_id: CheckpointClaimId,
    draft: Option<ContextRevisionDraft>,
    confidence: CandidateConfidence,
    unknowns: Vec<CheckpointUnknown>,
    error_code: Option<&'static str>,
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

/// State a caller already opened for this exact call, handed to [`Runtime::open_with_catalog`]
/// instead of being opened a second time.
#[derive(Default)]
struct RuntimeOpenParts {
    /// Activation scope that authorized this exact call, when the caller is public MCP dispatch.
    session_scope: Option<AuthorizedSessionScope>,
    /// `[context_ttl]` already read from the same `config.toml` read that froze the Catalog.
    context_ttl: Option<ContextTtlSettings>,
    /// Task Runtime already opened by the identity preflight of this call.
    tasks: Option<TaskRuntime>,
}

impl Runtime {
    fn open(root: &Path) -> Result<Self> {
        let _store = GitStore::open_existing(root)?;
        let catalog = UserConfigStore::open_existing(root)?.repository_catalog_wait()?;
        Self::open_with_catalog(root, catalog, RuntimeOpenParts::default())
    }

    /// Opens business state against the exact Catalog snapshot that authorized
    /// this call. Public MCP dispatch must never re-read a newer Catalog here.
    fn open_with_catalog(
        root: &Path,
        catalog: RepositoryCatalogSnapshot,
        parts: RuntimeOpenParts,
    ) -> Result<Self> {
        // Same two paths `GitStore::open_existing` would hand `ProjectionIndex::for_store`,
        // without the Git worktree verification that only a writing tool needs.
        let config = UserConfigStore::open_existing(root)?;
        let index = ProjectionIndex::new(config.repository(), config.root().join("state"));
        let repositories = RepositoryRegistry::initialize(root)?;
        sync_repository_catalog_snapshot(&repositories, &catalog)?;
        let engineering_graph = EngineeringProjectionStore::initialize(root).ok();
        let tasks = match parts.tasks {
            Some(tasks) => tasks,
            None => TaskRuntime::initialize(root)?,
        };
        let context_ttl = match parts.context_ttl {
            Some(context_ttl) => context_ttl,
            None => context_ttl_settings(&config.context_ttl_policy()?),
        };
        Ok(Self {
            root: root.to_path_buf(),
            store: OnceCell::new(),
            index,
            repositories,
            engineering_graph,
            tasks,
            catalog,
            context_ttl,
            session_scope: parts.session_scope,
        })
    }

    /// Opens and verifies the append-only Knowledge Store, once per Runtime.
    fn store(&self) -> Result<&GitStore> {
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        let store = GitStore::open_existing(&self.root)?
            .with_candidate_submission_index(Arc::new(self.index.clone()))
            .with_candidate_confirmation_index(Arc::new(self.index.clone()));
        let _ = self.store.set(store);
        Ok(self
            .store
            .get()
            .expect("the Knowledge Store was just initialized"))
    }

    /// One reduced Domain Snapshot, shared with every other reader of the same indexed Tree.
    ///
    /// The index reuses the reduction it already performed for this exact projection identity, so
    /// a tool call that reads the Snapshot once and then analyzes several Candidates against it
    /// pays for one reduction, not one per read.
    fn snapshot(&self) -> Result<Arc<DomainSnapshot>> {
        self.index.shared_domain_snapshot()
    }

    fn task_context_readonly(&self, input: &TaskContextReadInput) -> Result<TaskContextResponse> {
        self.task_context_readonly_with_detail(input, ContextPackDetailLevel::Full)
    }

    fn task_context_readonly_with_detail(
        &self,
        input: &TaskContextReadInput,
        detail_level: ContextPackDetailLevel,
    ) -> Result<TaskContextResponse> {
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
            &self.tasks,
            ContextInjectionSource::TaskContext,
            &snapshot,
            None,
            input.token_budget,
            input.max_spaces,
            detail_level,
            self.context_ttl,
        )
    }

    fn task_artifact_focus(
        &self,
        input: &ArtifactFocusQuery,
    ) -> Result<ArtifactFocusQueryResponse> {
        self.task_artifact_focus_with_detail(input, ContextPackDetailLevel::Full)
    }

    fn task_artifact_focus_with_detail(
        &self,
        input: &ArtifactFocusQuery,
        detail_level: ContextPackDetailLevel,
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
            &self.tasks,
            ContextInjectionSource::ArtifactFocus,
            &active,
            Some(resolved_focus.clone()),
            input.token_budget,
            input.max_spaces,
            detail_level,
            self.context_ttl,
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
        self.task_intent_update_with_detail(input, ContextPackDetailLevel::Full)
    }

    fn task_intent_update_with_detail(
        &self,
        input: &TaskIntentUpdateInput,
        detail_level: ContextPackDetailLevel,
    ) -> Result<TaskIntentUpdateResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let active = self.tasks.read_snapshot_by_locator(&locator)?;
        input.intent.validate()?;
        let (snapshot, revision_status) = match input.task_boundary {
            TaskBoundary::Continue => {
                if active.is_none() {
                    return Err(invalid(
                        "task_boundary=continue requires an existing ActiveTask",
                    ));
                }
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
                let outcome =
                    self.tasks
                        .continue_working_intent(&locator, parent, input.intent.clone())?;
                (outcome.snapshot, outcome.status.into())
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
            &self.tasks,
            ContextInjectionSource::IntentUpdate,
            &snapshot,
            None,
            default_token_budget(),
            default_max_spaces(),
            detail_level,
            self.context_ttl,
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

    fn task_checkpoint(&self, input: &TaskCheckpointInput) -> Result<TaskCheckpointResponse> {
        durable_checkpoint(&self.tasks, input)
    }
}

/// The whole durable Checkpoint ACK, over Runtime state alone.
///
/// ADR-0003 makes the ACK a receipt plus a Candidate Build outbox entry: it appends no Git event,
/// reads no retrieval index, consults no checkout, and touches no Repository Registry. It is
/// therefore given the Runtime database and nothing else, so public dispatch can answer a
/// Checkpoint without opening the stores only Candidate Build and retrieval need.
#[allow(clippy::too_many_lines)]
fn durable_checkpoint(
    tasks: &TaskRuntime,
    input: &TaskCheckpointInput,
) -> Result<TaskCheckpointResponse> {
    {
        let input_json = serde_json::to_string(input).map_err(|error| {
            invalid(format!(
                "serialize task_checkpoint privacy boundary: {error}"
            ))
        })?;
        validate_task_checkpoint_input(input, input_json.len())?;
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
        if input.claims.is_empty() && input.unknowns.is_empty() {
            if tasks.read_snapshot_by_locator(&locator)?.is_none() {
                return Err(invalid(
                    "ExternalSession has no ActiveTask for Agent Checkpoint",
                ));
            }
            return Ok(TaskCheckpointResponse::NoOp(TaskCheckpointNoOpResponse {
                status: TaskCheckpointNoOpStatus::NoOp,
            }));
        }
        let submission = AgentCheckpointSubmission {
            locator,
            claims: input
                .claims
                .iter()
                .map(|claim| DirectCheckpointClaimDraft {
                    context_kind: claim.context_kind,
                    statement: claim.statement.clone(),
                    rationale: claim.rationale.clone(),
                    conditions: claim.conditions.clone(),
                    evidence: claim
                        .evidence
                        .iter()
                        .map(|evidence| DirectEvidenceDraft {
                            evidence_type: evidence.evidence_type,
                            summary: evidence.summary.clone(),
                            limitations: evidence.limitations.clone(),
                        })
                        .collect(),
                })
                .collect(),
            unknowns: input
                .unknowns
                .iter()
                .map(|unknown| CheckpointUnknown {
                    statement: unknown.statement.clone(),
                    blocking: unknown.blocking,
                    recheck_when: Vec::new(),
                })
                .collect(),
        };
        // ADR-0003: the durable ACK is receipt plus outbox. Reference derivation, duplicate
        // collapsing and injection-usage comparison are all Candidate Build work, so nothing here
        // reads a checkout, spawns a process, or opens the retrieval index.
        let outcome = tasks.submit_agent_checkpoint(&submission)?;
        Ok(TaskCheckpointResponse::Accepted(
            TaskCheckpointAcceptedResponse {
                status: TaskCheckpointAcceptedStatus::Accepted,
                operation_id: outcome.operation_id,
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
                replayed: outcome.replayed,
                diagnostics: outcome
                    .inline_observation_ids
                    .into_iter()
                    .map(
                        |observation_id| TaskCheckpointDiagnostic::InlineValidationRecorded {
                            observation_id,
                        },
                    )
                    .collect(),
                candidate_build: CandidateBuildReceipt {
                    build_id: outcome.build.build_id,
                    episode_id: outcome.build.source_episode.episode_id,
                    status: outcome.build.status.into(),
                },
            },
        ))
    }
}

impl Runtime {
    /// Places one Episode's Claim path spellings inside the Repositories that Session was
    /// scoped to.
    ///
    /// Returns whether this call wrote the derivation. At most one `git ls-files` runs per
    /// checkout per Episode for the life of the installation: the runtime records the first
    /// answer, and a Build rerun after recovery reports it without consulting the checkout
    /// again. A Session started at the common parent of several checkouts carries several
    /// Repositories, so a spelling is placed by which checkout actually tracks it: exactly
    /// one resolving checkout wins, none or several leaves an unresolved retrieval hint.
    fn derive_episode_claim_references(&self, episode: &WorkEpisodeView) -> Result<bool> {
        let episode_id = episode.episode.episode_id;
        let Some(candidates) = self.tasks.pending_claim_reference_candidates(episode_id)? else {
            return Ok(false);
        };
        let resolvers = self
            .episode_checkouts(episode)?
            .into_iter()
            .map(|(repository_id, checkout)| {
                CheckoutReferenceResolver::from_checkout(repository_id, &checkout, &candidates)
            })
            .collect::<Vec<_>>();
        if resolvers.is_empty() {
            self.tasks
                .derive_episode_claim_references(episode_id, &reference_derivation::unresolvable)?;
            return Ok(true);
        }
        self.tasks
            .derive_episode_claim_references(episode_id, &|candidate| {
                let mut resolved = resolvers
                    .iter()
                    .filter_map(|resolver| resolver.resolve(candidate));
                let first = resolved.next()?;
                resolved.next().is_none().then_some(first)
            })?;
        Ok(true)
    }

    /// Resolves the checkouts of the `ExternalSession` that authored this Episode's Checkpoints.
    ///
    /// A Build can be drained by an Agent Hook or by the CLI, neither of which carries the
    /// authoring session's activation scope, so the scope is looked up by the Episode's own
    /// locator. A Disabled or absent lease yields no checkout and derives nothing. The order is
    /// the lease's own sorted Repository order, so the derivation is deterministic across runs.
    fn episode_checkouts(&self, episode: &WorkEpisodeView) -> Result<Vec<(RepositoryId, PathBuf)>> {
        let Some(task) = self.tasks.read_snapshot(episode.episode.task_session_id)? else {
            return Ok(Vec::new());
        };
        let locator = task.external_session_locator;
        let repository_ids = match &self.session_scope {
            Some(scope) if scope.external_session_locator == locator => {
                scope.decision.repository_ids().to_vec()
            }
            _ => match read_reconciled_session_scope(&self.root, &locator, &self.catalog) {
                Ok(AuthorizedSessionScopeRead::Current(scope)) => {
                    scope.decision.repository_ids().to_vec()
                }
                _ => return Ok(Vec::new()),
            },
        };
        Ok(repository_ids
            .into_iter()
            .filter_map(|repository_id| {
                self.catalog
                    .repositories
                    .iter()
                    .find(|entry| entry.repository_id == repository_id)
                    .and_then(|entry| entry.checkout_paths.first().cloned())
                    .map(|checkout| (repository_id, checkout))
            })
            .collect())
    }

    /// Records the injection outcome for every Claim this Episode carries.
    fn record_episode_context_usage(&self, episode: &WorkEpisodeView) -> Result<()> {
        let claims = episode
            .checkpoints
            .iter()
            .flat_map(|checkpoint| checkpoint.claims.iter().cloned())
            .collect::<Vec<_>>();
        if claims.is_empty() {
            return Ok(());
        }
        self.record_checkpoint_context_usage(episode.episode.task_id, &claims)
    }

    /// Compares what this Task was given with what it just claimed.
    ///
    /// Every Context injected into the Task is matched against the Checkpoint Claims by
    /// normalized statement token Jaccard. A Claim that restates an injected Context marks it
    /// `reused`; an injected Context no Claim restates is `ignored`. The model fills in nothing:
    /// both outcomes are derived from text it wrote for its own purpose. This runs during
    /// Candidate Build rather than in the ACK because it reads the retrieval index; writing the
    /// same rows again is a no-op, so a Build rerun and a Checkpoint replay both stay idempotent.
    fn record_checkpoint_context_usage(
        &self,
        task_id: TaskId,
        claims: &[CheckpointClaim],
    ) -> Result<()> {
        let injections = self.tasks.read_task_injections(task_id)?;
        if injections.is_empty() {
            return Ok(());
        }
        let claim_tokens = claims
            .iter()
            .map(|claim| statement_tokens(&claim.statement))
            .collect::<Vec<_>>();
        let injected = injections
            .iter()
            .map(|injection| (injection.context_id, injection.revision_id))
            .collect::<Vec<_>>();
        let statements = SearchEngine::new(self.index.clone()).context_statements(&injected)?;
        let mut records = Vec::new();
        for injection in &injections {
            let Some(statement) = statements.get(&injection.context_id) else {
                continue;
            };
            let injected_tokens = statement_tokens(statement);
            let reused = claim_tokens.iter().any(|claim| {
                jaccard_basis_points(&injected_tokens, claim)
                    >= USAGE_REUSED_SIMILARITY_BASIS_POINTS
            });
            records.push(ContextUsageRecord {
                context_id: injection.context_id,
                task_id,
                outcome: if reused {
                    ContextUsageOutcome::Reused
                } else {
                    ContextUsageOutcome::Ignored
                },
            });
        }
        self.tasks.record_context_usage(&records)?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    /// Collapses Claims that only restate a Candidate this `ExternalSession` already proposed.
    ///
    /// The decision compares normalized statement tokens with Jaccard similarity and never runs on
    /// a Claim whose Candidate already reached Git, so it cannot retract a submitted proposal or
    /// change a Candidate that a human is already reviewing.
    fn deduplicated_claims(
        &self,
        episode: &WorkEpisodeView,
        materials: &[ClaimBuildMaterial],
        snapshot: &DomainSnapshot,
    ) -> Result<Vec<CandidateBuildDuplicatePreparation>> {
        let submitted = self
            .tasks
            .read_candidate_build(episode.episode.episode_id)?
            .map(|build| {
                build
                    .items
                    .iter()
                    .filter(|item| item.status.is_finalized())
                    .map(|item| item.claim_id)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let comparable = materials
            .iter()
            .filter(|material| material.draft.is_some() && !submitted.contains(&material.claim_id))
            .count();
        if comparable == 0 {
            return Ok(Vec::new());
        }
        // Candidates built from this same Episode are excluded so the decision does not depend on
        // how far this Build already progressed.
        let corpus = self
            .tasks
            .list_session_candidate_reviews(
                episode.episode.task_session_id,
                sctx_task_runtime::MAX_SESSION_CANDIDATE_REVIEW_SCAN,
            )?
            .into_iter()
            .filter(|record| record.source_episode.episode_id != episode.episode.episode_id)
            .filter_map(|record| {
                snapshot
                    .projection
                    .candidates
                    .get(&record.candidate_id)
                    .map(|projection| {
                        (
                            record.candidate_id,
                            statement_tokens(&projection.candidate.content.statement),
                        )
                    })
            })
            .filter(|(_, tokens)| !tokens.is_empty())
            .collect::<Vec<_>>();
        if corpus.is_empty() {
            return Ok(Vec::new());
        }
        let mut duplicates = Vec::new();
        for material in materials {
            if submitted.contains(&material.claim_id) {
                continue;
            }
            let Some(draft) = material.draft.as_ref() else {
                continue;
            };
            let tokens = statement_tokens(&draft.statement);
            if tokens.is_empty() {
                continue;
            }
            let best = corpus
                .iter()
                .map(|(candidate_id, existing)| {
                    (jaccard_basis_points(&tokens, existing), *candidate_id)
                })
                .filter(|(similarity, _)| {
                    *similarity >= CANDIDATE_DUPLICATE_SIMILARITY_BASIS_POINTS
                })
                .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)));
            if let Some((similarity_basis_points, duplicate_of_candidate_id)) = best {
                duplicates.push(CandidateBuildDuplicatePreparation {
                    checkpoint_id: material.checkpoint_id,
                    claim_id: material.claim_id,
                    duplicate_of_candidate_id,
                    similarity_basis_points,
                });
            }
        }
        Ok(duplicates)
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
        // Everything the ACK deliberately skipped happens here, once per Episode: place the path
        // spellings the Claims carried, then compare what this Task was injected with against what
        // it claimed. Both are best-effort derivations over text the Agent wrote for itself.
        let episode = if self.derive_episode_claim_references(&episode)? {
            self.tasks
                .read_work_episode(episode_id)?
                .ok_or_else(|| invariant("Candidate Builder source Work Episode disappeared"))?
        } else {
            episode
        };
        let _ = self.record_episode_context_usage(&episode);
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
        let (materials, duplicates) = if claim_count == 0 {
            (Vec::new(), Vec::new())
        } else {
            let snapshot = self.snapshot()?;
            let signals = self
                .tasks
                .read_signal_history(episode.episode.task_session_id)?;
            let materials = episode
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
                .collect::<Vec<_>>();
            let duplicates = self.deduplicated_claims(&episode, &materials, &snapshot)?;
            (materials, duplicates)
        };
        let deduplicated = duplicates
            .iter()
            .map(|duplicate| duplicate.claim_id)
            .collect::<BTreeSet<_>>();
        let preparations = materials
            .iter()
            .filter(|material| !deduplicated.contains(&material.claim_id))
            .map(|material| material.preparation(&episode.episode.ownership()))
            .collect::<Vec<_>>();
        let mut build = self.tasks.prepare_candidate_build_with_duplicates(
            episode_id,
            &preparations,
            &duplicates,
        )?;
        let materials = materials
            .iter()
            .map(|material| (material.claim_id, material))
            .collect::<BTreeMap<_, _>>();
        // Analysis is deferred to one pass after every Candidate of this Episode is committed:
        // it reads the Knowledge Store but writes only local analysis rows, so running it once
        // over one Domain Snapshot is equivalent to running it per Candidate, minus N-1 full
        // projection reads. Sibling Candidates of the same build are never analysis targets
        // (targets come from Spaces, which an unconfirmed Candidate has not joined), so the
        // deferral cannot change what any Candidate is compared against.
        let mut pending_analysis = Vec::new();
        let mut submissions = Vec::new();
        for item in &build.items {
            if item.status.is_finalized() {
                if let Some(candidate_id) = item.candidate_id
                    && self
                        .tasks
                        .read_candidate_analysis(candidate_id)?
                        .is_none_or(|view| {
                            view.candidate.analysis.status != CandidateAnalysisStatus::Complete
                        })
                {
                    pending_analysis.push(candidate_id);
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
            submissions.push(CandidateSubmissionRequest {
                submission_id: item.submission_id,
                source_episode: episode.episode.ownership(),
                content: draft.clone(),
            });
        }
        // One lock cycle, one index synchronization pair, one journal and one Git commit carry
        // every Candidate this Episode still owes. Each Candidate keeps its own SubmissionId
        // idempotency and its own Writer batch id, so replaying the build returns exactly the
        // same Candidate identities and writes nothing.
        for chunk in submissions.chunks(MAX_CANDIDATE_SUBMISSION_BATCH) {
            for (submission_id, result) in self.submit_candidate_chunk(chunk)? {
                match result {
                    Ok(record) => {
                        let submission_status = match record.status {
                            CandidateSubmissionStatus::Created => CandidateBuildItemStatus::Created,
                            CandidateSubmissionStatus::AlreadyExists => {
                                CandidateBuildItemStatus::AlreadyExists
                            }
                        };
                        build = self.tasks.record_candidate_build_item_result(
                            build.build_id,
                            submission_id,
                            submission_status,
                            Some(record.candidate_id),
                            Some(record.event_id),
                            None,
                        )?;
                        pending_analysis.push(record.candidate_id);
                    }
                    Err(error_code) => {
                        build = self.tasks.record_candidate_build_item_result(
                            build.build_id,
                            submission_id,
                            CandidateBuildItemStatus::Failed,
                            None,
                            None,
                            Some(error_code),
                        )?;
                    }
                }
            }
        }
        if !pending_analysis.is_empty() {
            let snapshot = self.snapshot()?;
            for candidate_id in pending_analysis {
                self.analyze_candidate_in_snapshot(
                    candidate_id,
                    default_candidate_analysis_token_budget(),
                    default_candidate_analysis_top_k(),
                    &snapshot,
                )?;
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

    /// Submits one Candidate slice atomically, degrading to per-Candidate writes on rejection.
    ///
    /// The batch entry point rejects the whole slice when one member is at fault, so a rejected
    /// batch is retried one Candidate at a time. That keeps the pre-batch Builder behaviour where
    /// exactly the offending Candidate is recorded as `Failed` and its siblings still land.
    #[allow(clippy::type_complexity)]
    fn submit_candidate_chunk(
        &self,
        requests: &[CandidateSubmissionRequest],
    ) -> Result<
        Vec<(
            SubmissionId,
            std::result::Result<CandidateSubmissionOutcomeRecord, &'static str>,
        )>,
    > {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        if let Ok(write) = self.store()?.submit_candidates(requests) {
            return Ok(write
                .entries
                .into_iter()
                .map(|entry| {
                    (
                        entry.submission_id,
                        Ok(CandidateSubmissionOutcomeRecord {
                            candidate_id: entry.record.candidate_id,
                            event_id: entry.record.event_id,
                            status: entry.status,
                        }),
                    )
                })
                .collect());
        }
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            let submission_id = request.submission_id;
            let result = match self.store()?.submit_candidate(request.clone()) {
                Ok(outcome) => Ok(CandidateSubmissionOutcomeRecord {
                    candidate_id: outcome.record.candidate_id,
                    event_id: outcome.record.event_id,
                    status: outcome.status,
                }),
                Err(error) => Err(candidate_builder_error_code(&error)),
            };
            results.push((submission_id, result));
        }
        Ok(results)
    }

    fn recover_candidate_build_episode(&self, episode_id: WorkEpisodeId) -> Result<bool> {
        match self.build_closed_episode(episode_id) {
            Ok(build) => {
                self.tasks
                    .record_candidate_build_recovery_attempt(episode_id, None)?;
                Ok(build.status == CandidateBuildResponseStatus::Complete)
            }
            Err(error) => {
                self.tasks.record_candidate_build_recovery_attempt(
                    episode_id,
                    Some(candidate_builder_error_code(&error)),
                )?;
                Ok(false)
            }
        }
    }

    fn recover_candidate_builds(
        &self,
        locator: &ExternalSessionLocator,
    ) -> Result<CandidateRecoverySummary> {
        let episodes = self
            .tasks
            .list_recoverable_candidate_build_episodes(locator, MAX_RECOVERABLE_BUILDS_PER_READ)?;
        let mut summary = CandidateRecoverySummary {
            attempted: episodes.len(),
            ..CandidateRecoverySummary::default()
        };
        for episode_id in episodes {
            if self.recover_candidate_build_episode(episode_id)? {
                summary.recovered += 1;
            } else {
                summary.failed_attempts += 1;
            }
        }
        let current = self.tasks.candidate_build_recovery_status(locator)?;
        summary.pending = current.pending;
        summary.incomplete = current.incomplete;
        Ok(summary)
    }

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
        self.analyze_candidate_in_snapshot(candidate_id, input.token_budget, input.top_k, &snapshot)
    }

    /// Analyzes one Candidate against an already read Domain Snapshot.
    ///
    /// Analysis only reads the Knowledge Store and writes local analysis rows, so several
    /// Candidates committed by the same Git commit share one snapshot read.
    #[allow(clippy::too_many_lines)]
    fn analyze_candidate_in_snapshot(
        &self,
        candidate_id: CandidateId,
        token_budget: usize,
        top_k: usize,
        snapshot: &DomainSnapshot,
    ) -> Result<CandidateAnalyzeResponse> {
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
        let proposed_space_group_key = ProposedSpaceGroupKey::from_task(task.task_id);
        let proposed_space_group_space_id = self
            .tasks
            .read_proposed_space_group(proposed_space_group_key)?
            .filter(|mapping| {
                mapping.status == ProposedSpaceGroupMappingStatus::Committed
                    && mapping.candidate_id != persisted.candidate_id
            })
            .map(|mapping| mapping.space_id);
        let signal_history = self
            .tasks
            .read_signal_history(episode.episode.task_session_id)?;
        let material = build_claim_material(
            &episode,
            final_checkpoint,
            checkpoint,
            claim,
            &signal_history,
            snapshot,
        );
        let engine = self.engineering_graph.as_ref().map_or_else(
            || SearchEngine::new(self.index.clone()),
            |graph| SearchEngine::with_engineering_graph(self.index.clone(), graph.clone()),
        );
        let derived = engine.analyze_candidate(&CandidateAnalysisRequest {
            candidate: persisted.clone(),
            source_task_id: task.task_id,
            source_intent_revision_id: source_intent_id,
            source_working_intent: source_intent,
            source_task_signals: task.task_signals.clone(),
            explicit_related_contexts: claim.related_contexts.clone(),
            // Server-derived References are the only Artifact coordinates most Claims carry, so
            // the analyzer's shared-Artifact path sees them alongside any Agent-authored ref.
            artifact_refs: candidate_artifact_refs(claim),
            proposed_space_group_space_id,
            token_budget,
            top_k,
        });
        let mut checkpoint_ids = vec![item.checkpoint_id];
        if !checkpoint_ids.contains(&build.final_checkpoint_id) {
            checkpoint_ids.push(build.final_checkpoint_id);
        }
        let observation_ids = claim
            .evidence_refs
            .iter()
            .filter_map(|evidence| match evidence {
                CheckpointEvidenceRef::Observation { observation_id } => Some(*observation_id),
                CheckpointEvidenceRef::TaskSignal { .. }
                | CheckpointEvidenceRef::ContextEvidence { .. } => None,
            })
            .collect::<Vec<_>>();
        let provenance = CandidateBuilderProvenance {
            build_id: build.build_id,
            source_episode: episode.episode.ownership(),
            checkpoint_ids,
            observation_ids,
            engineering_references: claim.engineering_references.clone(),
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
        self.candidate_list_with_detail(input, ContextPackDetailLevel::Full)
    }

    fn candidate_list_with_detail(
        &self,
        input: &CandidateListInput,
        detail_level: ContextPackDetailLevel,
    ) -> Result<CandidateListResponse> {
        if input.token_budget < MIN_CANDIDATE_REVIEW_TOKEN_BUDGET
            || input.token_budget > MAX_CANDIDATE_REVIEW_TOKEN_BUDGET
        {
            return Err(invalid(
                "Candidate Review token budget is outside the safe bound",
            ));
        }
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let recovery = self.recover_candidate_builds(&locator)?;
        let page = self.tasks.list_candidate_reviews(
            &locator,
            input.status,
            input.limit,
            input.cursor.as_deref(),
        )?;
        let snapshot = self.snapshot()?;
        let provisional_space_ids = provisional_space_ids(&snapshot);
        let space_advisories = provisional_space_advisories(&snapshot);
        let mut reviews = Vec::new();
        let mut compact_reviews = Vec::new();
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
            let compact = compact_candidate_review(&summary.0, &provisional_space_ids);
            let tokens = match detail_level {
                ContextPackDetailLevel::Full => estimate_candidate_review_tokens(&summary)?,
                ContextPackDetailLevel::Compact => estimate_candidate_review_tokens(&compact)?,
            };
            if estimated_tokens.saturating_add(tokens) > input.token_budget {
                omitted.push(CandidateReviewOmitted {
                    candidate_id: record.candidate_id,
                    reason: CandidateReviewOmittedReason::TokenBudget,
                    estimated_tokens: tokens,
                });
            } else {
                estimated_tokens = estimated_tokens.saturating_add(tokens);
                reviews.push(summary);
                compact_reviews.push(compact);
            }
        }
        if detail_level == ContextPackDetailLevel::Full {
            compact_reviews.clear();
        }
        Ok(CandidateListResponse {
            reviews,
            detail_level,
            compact_reviews,
            space_advisories,
            provisional_space_ids,
            omitted,
            next_cursor: page.next_cursor,
            estimated_tokens,
            token_budget: input.token_budget,
            recovery,
        })
    }

    fn candidate_get(&self, input: &CandidateGetInput) -> Result<CandidateReviewView> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let candidate_id = parse_id_value(&input.candidate_id, "candidate_id")?;
        let mut record = self.tasks.read_candidate_review(&locator, candidate_id)?;
        let target_episode = if let Some(record) = &record {
            self.tasks
                .read_candidate_analysis(candidate_id)?
                .is_none_or(|view| {
                    view.candidate.analysis.status != CandidateAnalysisStatus::Complete
                })
                .then_some(record.source_episode.episode_id)
        } else {
            let snapshot = self.snapshot()?;
            snapshot
                .projection
                .candidates
                .get(&candidate_id)
                .map(|candidate| candidate.candidate.source_episode.episode_id)
        };
        if let Some(episode_id) = target_episode
            && self
                .tasks
                .owns_candidate_recovery_episode(&locator, episode_id)?
        {
            let _recovered = self.recover_candidate_build_episode(episode_id)?;
            record = self.tasks.read_candidate_review(&locator, candidate_id)?;
        }
        let record = record.ok_or_else(|| {
            invalid(
                "Candidate Review does not exist or recovery remains pending for the ActiveTask",
            )
        })?;
        let snapshot = self.snapshot()?;
        let view = self.candidate_review_view(&record, &snapshot)?;
        if view.analysis.status != CandidateAnalysisStatus::Complete {
            return Err(invalid(
                "Candidate Review recovery remains pending for the ActiveTask",
            ));
        }
        Ok(view)
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

    /// Discards several owned Pending Candidates in one atomic Runtime transaction.
    fn candidate_discard_batch(
        &self,
        input: &CandidateDiscardBatchInput,
    ) -> Result<CandidateDiscardBatchResponse> {
        let scan = PrivacyScanner::default().scan(&input.reason)?;
        if !scan.is_clean() {
            return Err(Error::new(
                ErrorKind::PrivacyRejected,
                "Candidate discard reason failed the privacy boundary",
            ));
        }
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let expected_task_id = parse_id_value(&input.expected_task_id, "expected_task_id")?;
        let expected_intent_revision_id = parse_id_value(
            &input.expected_intent_revision_id,
            "expected_intent_revision_id",
        )?;
        let requests = parse_batch_candidate_ids(&input.candidate_ids)?
            .into_iter()
            .map(|candidate_id| CandidateReviewDiscard {
                locator: locator.clone(),
                expected_task_id,
                expected_intent_revision_id,
                candidate_id,
                expected_review_version: input.expected_review_version,
                reason: input.reason.clone(),
            })
            .collect::<Vec<_>>();
        let outcomes = self.tasks.discard_candidate_reviews(&requests)?;
        let snapshot = self.snapshot()?;
        let reviews = outcomes
            .iter()
            .map(|outcome| self.candidate_review_view(&outcome.record, &snapshot))
            .collect::<Result<Vec<_>>>()?;
        Ok(CandidateDiscardBatchResponse {
            status: if outcomes
                .iter()
                .all(|outcome| outcome.status == CandidateReviewDiscardStatus::AlreadyDiscarded)
            {
                CandidateDiscardResponseStatus::AlreadyDiscarded
            } else {
                CandidateDiscardResponseStatus::Discarded
            },
            reviews,
        })
    }

    fn candidate_confirm(&self, input: &CandidateConfirmInput) -> Result<CandidateConfirmResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let expected_task_id = parse_id_value(&input.expected_task_id, "expected_task_id")?;
        let expected_intent_revision_id = parse_id_value(
            &input.expected_intent_revision_id,
            "expected_intent_revision_id",
        )?;
        let candidate_id = parse_id_value(&input.candidate_id, "candidate_id")?;
        let snapshot = self.snapshot()?;
        let prepared = self.prepare_candidate_confirmation(
            &snapshot,
            &locator,
            candidate_id,
            input.expected_review_version,
            &input.primary,
            &input.related_space_ids,
            &input.edits,
        )?;
        self.commit_candidate_confirmation(
            &locator,
            expected_task_id,
            expected_intent_revision_id,
            prepared,
        )
    }

    /// Confirms several owned Pending Candidates under one Space organization.
    ///
    /// Every Candidate is fully validated and planned before the first Git write, so a rejected
    /// batch writes nothing and names the exact Candidate that failed. Field edits and a proposed
    /// new Space are Candidate-scoped and stay single-Candidate operations.
    fn candidate_confirm_batch(
        &self,
        input: &CandidateConfirmBatchInput,
    ) -> Result<CandidateConfirmBatchResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let expected_task_id = parse_id_value(&input.expected_task_id, "expected_task_id")?;
        let expected_intent_revision_id = parse_id_value(
            &input.expected_intent_revision_id,
            "expected_intent_revision_id",
        )?;
        let candidate_ids = parse_batch_candidate_ids(&input.candidate_ids)?;
        if matches!(input.primary, CandidateConfirmPrimaryInput::Proposed(_)) {
            return Err(invalid(
                "batch candidate_confirm requires existing_space_id; a proposed new Space recommendation identifies one Candidate only",
            ));
        }
        let snapshot = self.snapshot()?;
        let mut prepared = Vec::with_capacity(candidate_ids.len());
        for (position, candidate_id) in candidate_ids.iter().enumerate() {
            prepared.push(
                self.prepare_candidate_confirmation(
                    &snapshot,
                    &locator,
                    *candidate_id,
                    input.expected_review_version,
                    &input.primary,
                    &input.related_space_ids,
                    &OptionalCandidateEdits::default(),
                )
                .map_err(|error| batch_preparation_error(position, *candidate_id, &error))?,
            );
        }
        let mut reserved = Vec::with_capacity(prepared.len());
        for (position, prepared) in prepared.into_iter().enumerate() {
            let candidate_id = prepared.candidate_id;
            reserved.push(
                self.reserve_candidate_confirmation(
                    &locator,
                    expected_task_id,
                    expected_intent_revision_id,
                    prepared,
                )
                .map_err(|error| batch_preparation_error(position, candidate_id, &error))?,
            );
        }
        // One Confirmation lock, one Writer batch, one Git commit for the whole slice: a rejected
        // member leaves no Candidate written, and every accepted member shares one commit.
        let plans = reserved
            .iter()
            .map(|reserved| reserved.plan.clone())
            .collect::<Vec<_>>();
        let write = self.store()?.confirm_candidates(&plans)?;
        let finalized = self.tasks.finalize_candidate_confirmations(
            &write
                .entries
                .iter()
                .map(|entry| CandidateConfirmationFinalize {
                    candidate_id: entry.candidate_id,
                    operation_hash: entry.record.operation_hash.clone(),
                    confirmation_id: entry.record.confirmation_id,
                    result_context_id: entry.record.result_context_id,
                })
                .collect::<Vec<_>>(),
        )?;
        let written = write
            .entries
            .into_iter()
            .zip(&finalized)
            .map(|(entry, finalized)| WrittenCandidateConfirmation {
                append: entry.append,
                already_confirmed: finalized.already_confirmed
                    || entry.status == CandidateConfirmationWriteStatus::AlreadyExists,
            })
            .collect::<Vec<_>>();
        let confirmations =
            self.finish_candidate_confirmations(expected_task_id, reserved, written)?;
        Ok(CandidateConfirmBatchResponse {
            status: if confirmations.iter().all(|confirmation| {
                confirmation.status == CandidateConfirmResponseStatus::AlreadyConfirmed
            }) {
                CandidateConfirmResponseStatus::AlreadyConfirmed
            } else {
                CandidateConfirmResponseStatus::Confirmed
            },
            confirmations,
        })
    }

    /// Validates one Candidate Confirmation and reserves nothing.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn prepare_candidate_confirmation(
        &self,
        snapshot: &DomainSnapshot,
        locator: &ExternalSessionLocator,
        candidate_id: sctx_domain::CandidateId,
        expected_review_version: u64,
        primary: &CandidateConfirmPrimaryInput,
        related_space_ids: &[String],
        edits: &OptionalCandidateEdits,
    ) -> Result<PreparedCandidateConfirmation> {
        let review_record = self
            .tasks
            .read_candidate_review(locator, candidate_id)?
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
        let review = self.candidate_review_view(&review_record, snapshot)?;
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
        let (primary_reference, resolved_primary, primary_space_id, proposed_space_group_key) =
            match primary {
                CandidateConfirmPrimaryInput::Existing(existing) => {
                    let space_id =
                        parse_id_value(&existing.existing_space_id, "existing_space_id")?;
                    if !snapshot.projection.spaces.contains_key(&space_id) {
                        return Err(invalid(
                            "Candidate Confirmation Primary Space does not exist",
                        ));
                    }
                    (
                        CandidateConfirmationPrimaryReference::ExistingSpace { space_id },
                        CandidatePrimarySelection::Existing { space_id },
                        Some(space_id),
                        None,
                    )
                }
                CandidateConfirmPrimaryInput::Proposed(proposed) => {
                    let recommendation_id = parse_id_value::<SpaceRecommendationId>(
                        &proposed.new_space_recommendation_id,
                        "new_space_recommendation_id",
                    )?;
                    let find_proposed = |recommendations: &[CandidateSpaceRecommendation]| {
                        recommendations
                            .iter()
                            .find_map(|recommendation| match recommendation {
                                CandidateSpaceRecommendation::ProposedNewSpaceIntent {
                                    recommendation_id: actual,
                                    proposed_space_group_key,
                                    proposed_new_space_intent,
                                    ..
                                } if *actual == recommendation_id => Some((
                                    proposed_new_space_intent.clone(),
                                    proposed_space_group_key.as_ref().copied()?,
                                )),
                                _ => None,
                            })
                    };
                    // Once a sibling Candidate of this Task has opened the proposed Space,
                    // `candidate_review_view` replaces the proposal with the resolved Space, so the
                    // recommendation a caller was handed earlier is no longer in the current view.
                    // It is still the analysis the server itself produced, so fall back to it and
                    // land in the Space the group already opened rather than rejecting a caller
                    // who cannot repair the identity on its own.
                    let analyzed = self
                        .tasks
                        .read_candidate_analysis(candidate_id)?
                        .map(|view| view.candidate.space_recommendations)
                        .unwrap_or_default();
                    let (intent, proposed_space_group_key) = find_proposed(
                        &review.space_recommendations,
                    )
                    .or_else(|| find_proposed(&analyzed))
                    .ok_or_else(|| {
                        invalid(
                            "new_space_recommendation_id is not a proposed recommendation this \
                             Candidate analysis produced: use candidate_get to refresh \
                             recommendations, or confirm into an existing Space with \
                             primary.existing_space_id",
                        )
                    })?;
                    let committed = self
                        .tasks
                        .read_proposed_space_group(proposed_space_group_key)?
                        .filter(|mapping| {
                            mapping.status == ProposedSpaceGroupMappingStatus::Committed
                                && mapping.candidate_id != candidate_id
                                && snapshot.projection.spaces.contains_key(&mapping.space_id)
                        })
                        .map(|mapping| mapping.space_id);
                    committed.map_or(
                        (
                            CandidateConfirmationPrimaryReference::ProposedRecommendation {
                                recommendation_id,
                            },
                            CandidatePrimarySelection::ProposedNew { intent },
                            None,
                            Some(proposed_space_group_key),
                        ),
                        |space_id| {
                            (
                                CandidateConfirmationPrimaryReference::ExistingSpace { space_id },
                                CandidatePrimarySelection::Existing { space_id },
                                Some(space_id),
                                None,
                            )
                        },
                    )
                }
            };
        let related_space_ids = related_space_ids
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
        // Derived, overridable `problem_view`: Candidate Build has no Task Intent to read, so the
        // question the source Task was answering is attached here instead. An explicit
        // `edits.problem_view` always wins, `clear` included.
        let mut edits = edits.clone();
        if edits.problem_view.is_none()
            && persisted.content.problem_view.is_none()
            && let Some(value) = self.derived_problem_view(review_record.source_episode)?
        {
            edits.problem_view = Some(ProblemViewEdit::Set { value });
        }
        let edits = &edits;
        let final_draft = edits.apply(&persisted.content)?;
        validate_context_relation_targets(snapshot, &final_draft.relations)?;
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
            review_parent_version: expected_review_version,
            analysis_generation: review.analysis_generation.unwrap_or_default(),
            primary: primary_reference,
            related_space_ids,
            edits: edits.clone(),
        };
        let final_content_hash = context_revision_content_hash(&final_draft);
        let conflict_openings = contradiction_conflict_openings(
            snapshot,
            &final_draft,
            primary_space_id,
            &final_content_hash,
        );
        let proposed_plan = CandidateConfirmationPlan::reserve(
            persisted,
            operation,
            resolved_primary,
            review.engineering_references.clone(),
            conflict_openings,
        )?;
        Ok(PreparedCandidateConfirmation {
            candidate_id,
            proposed_plan,
            proposed_space_group_key,
            assessments: review.analysis.assessments,
        })
    }

    /// Reserves, writes and finalizes one already validated Candidate Confirmation.
    fn commit_candidate_confirmation(
        &self,
        locator: &ExternalSessionLocator,
        expected_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
        prepared: PreparedCandidateConfirmation,
    ) -> Result<CandidateConfirmResponse> {
        let reserved = self.reserve_candidate_confirmation(
            locator,
            expected_task_id,
            expected_intent_revision_id,
            prepared,
        )?;
        let write = self.store()?.confirm_candidate(&reserved.plan)?;
        let finalized = self.tasks.finalize_candidate_confirmation(
            reserved.candidate_id,
            &reserved.plan.operation_hash,
            write.record.confirmation_id,
            write.record.result_context_id,
        )?;
        let written = vec![WrittenCandidateConfirmation {
            append: write.append,
            already_confirmed: finalized.already_confirmed
                || write.status == CandidateConfirmationWriteStatus::AlreadyExists,
        }];
        self.finish_candidate_confirmations(expected_task_id, vec![reserved], written)?
            .pop()
            .ok_or_else(|| invariant("Candidate Confirmation produced no response"))
    }

    /// Reserves one server-owned plan without writing any Git fact.
    fn reserve_candidate_confirmation(
        &self,
        locator: &ExternalSessionLocator,
        expected_task_id: TaskId,
        expected_intent_revision_id: TaskIntentRevisionId,
        prepared: PreparedCandidateConfirmation,
    ) -> Result<ReservedCandidateConfirmation> {
        let reservation = self.tasks.reserve_candidate_confirmation(
            locator,
            expected_task_id,
            expected_intent_revision_id,
            &prepared.proposed_plan,
            prepared.proposed_space_group_key,
        )?;
        Ok(ReservedCandidateConfirmation {
            candidate_id: prepared.candidate_id,
            plan: reservation.operation.plan,
            assessments: prepared.assessments,
        })
    }

    /// Records usage, refreshes derived state once, and answers for every written Confirmation.
    fn finish_candidate_confirmations(
        &self,
        expected_task_id: TaskId,
        reserved: Vec<ReservedCandidateConfirmation>,
        written: Vec<WrittenCandidateConfirmation>,
    ) -> Result<Vec<CandidateConfirmResponse>> {
        if reserved.len() != written.len() {
            return Err(invariant(
                "Candidate Confirmation results do not match their reserved plans",
            ));
        }
        // Late usage signal: a Context this Task was given and then explicitly contradicted is
        // `refuted`, which no later Checkpoint may downgrade. Best effort, like every other
        // usage record.
        for reserved in &reserved {
            let _ = self.record_confirmation_context_usage(
                expected_task_id,
                &reserved.plan.result_revision,
            );
        }
        let graph_rebuild_pending = if reserved
            .iter()
            .all(|reserved| reserved.plan.engineering_references.is_empty())
        {
            false
        } else {
            self.association_rebuild(&AssociationRebuildInput {
                diagnose_only: false,
            })
            .is_err()
        };
        let snapshot = self.snapshot()?;
        Ok(reserved
            .into_iter()
            .zip(written)
            .map(|(reserved, written)| {
                let plan = reserved.plan;
                let status = if written.already_confirmed {
                    CandidateConfirmResponseStatus::AlreadyConfirmed
                } else {
                    CandidateConfirmResponseStatus::Confirmed
                };
                CandidateConfirmResponse {
                    status,
                    created: status == CandidateConfirmResponseStatus::Confirmed,
                    candidate_id: reserved.candidate_id,
                    confirmation_id: plan.confirmation.confirmation_id,
                    context_id: plan.result_context_id,
                    revision_id: plan.result_revision.revision_id,
                    primary_space_id: plan.confirmation.primary_space_id,
                    related_space_ids: plan.confirmation.related_space_ids,
                    batch_id: written.append.batch_id,
                    commit_oid: written.append.commit_oid,
                    event_ids: written.append.event_ids,
                    indexed_tree_oid: snapshot.metadata.indexed_tree_oid.clone(),
                    projection_generation: snapshot.metadata.projection_generation,
                    assessment_acknowledgments: reserved.assessments,
                    graph_rebuild_pending,
                }
            })
            .collect())
    }

    /// Records the Contexts this Confirmation explicitly contradicted as `refuted`.
    ///
    /// Only Contexts that were actually injected into the confirming Task are recorded: the table
    /// answers "what happened to what we injected", and a contradiction of a Context this Task
    /// never received is a graph fact the Relation and its conflict already carry.
    fn record_confirmation_context_usage(
        &self,
        task_id: TaskId,
        revision: &sctx_domain::ContextRevision,
    ) -> Result<()> {
        let contradicted = revision
            .relations
            .iter()
            .filter(|relation| relation.kind == ContextRelationKind::Contradicts)
            .map(|relation| relation.target_context_id)
            .collect::<BTreeSet<_>>();
        if contradicted.is_empty() {
            return Ok(());
        }
        let injected = self
            .tasks
            .read_task_injections(task_id)?
            .into_iter()
            .map(|injection| injection.context_id)
            .collect::<BTreeSet<_>>();
        let records = contradicted
            .intersection(&injected)
            .map(|context_id| ContextUsageRecord {
                context_id: *context_id,
                task_id,
                outcome: ContextUsageOutcome::Refuted,
            })
            .collect::<Vec<_>>();
        self.tasks.record_context_usage(&records)?;
        Ok(())
    }

    /// The question the Candidate's source Task was working on, condensed for `problem_view`.
    ///
    /// It reads the Working Intent revision the source Episode closed under: the goal, what the
    /// Task declared in scope, and the questions it left open. `None` whenever the Episode or its
    /// Intent revision is no longer resolvable, which only leaves the field absent.
    fn derived_problem_view(&self, source_episode: WorkEpisodeRef) -> Result<Option<String>> {
        let Some(episode) = self.tasks.read_work_episode(source_episode.episode_id)? else {
            return Ok(None);
        };
        let intent_revision_id = episode.episode.intent_revisions.last();
        let Some(task) = self.tasks.read_snapshot(episode.episode.task_session_id)? else {
            return Ok(None);
        };
        Ok(task
            .intent_revisions
            .iter()
            .find(|revision| revision.revision_id == intent_revision_id)
            .and_then(|revision| compose_problem_view(&revision.working_intent)))
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
        if analysis_view.as_ref().is_some_and(|view| {
            view.candidate.builder_provenance.engineering_references != claim.engineering_references
        }) {
            return Err(invariant(
                "Candidate Review Engineering Reference provenance changed",
            ));
        }
        let (analysis, mut recommendations, confidence, unknowns, candidate_status, generation) =
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
        let proposed_space_group_key =
            recommendations
                .iter()
                .find_map(|recommendation| match recommendation {
                    CandidateSpaceRecommendation::ProposedNewSpaceIntent {
                        proposed_space_group_key,
                        ..
                    } => *proposed_space_group_key,
                    CandidateSpaceRecommendation::Existing { .. } => None,
                });
        if let Some(proposed_space_group_key) = proposed_space_group_key {
            if let Some(mapping) = self
                .tasks
                .read_proposed_space_group(proposed_space_group_key)?
                .filter(|mapping| {
                    mapping.status == ProposedSpaceGroupMappingStatus::Committed
                        && mapping.candidate_id != record.candidate_id
                })
            {
                if !snapshot.projection.spaces.contains_key(&mapping.space_id) {
                    return Err(invariant(
                        "committed proposed Space group points to a missing Space",
                    ));
                }
                recommendations.retain(|recommendation| match recommendation {
                    CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. } => false,
                    CandidateSpaceRecommendation::Existing { space_id, .. } => {
                        *space_id != mapping.space_id
                    }
                });
                recommendations.insert(
                    0,
                    CandidateSpaceRecommendation::existing_with_paths(
                        mapping.space_id,
                        sctx_domain::RecommendedSpaceRole::Primary,
                        "The first confirmed Candidate for this Task Intent revision created this Space",
                        CandidateConfidence {
                            basis_points: 10_000,
                            rationale: "Exact proposed Space group mapping".to_owned(),
                        },
                        vec![CandidateSpaceRecommendationPath::ProposedSpaceGroupResolved {
                            proposed_space_group_key,
                        }],
                    ),
                );
            }
        }
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
            engineering_references: claim.engineering_references.clone(),
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

    /// Creates one explicitly named Space through the exact `sctx space create` path.
    ///
    /// The Intent is validated by [`IntentSnapshot::validate`] and appended as one
    /// `SpaceCreated` Event, so an Agent no longer needs an escalated shell to seed a Space.
    fn space_create(&self, input: &SpaceCreateInput) -> Result<SpaceCreateResponse> {
        let event = Event::space_created(input.intent.clone(), None)?;
        let (space_id, intent_revision_id, provisional) = match event.payload() {
            EventPayload::SpaceCreated {
                space_id,
                intent_revision,
            } => (
                *space_id,
                intent_revision.revision_id,
                intent_revision.provisional,
            ),
            _ => unreachable!("space_created builds exactly one SpaceCreated payload"),
        };
        let event_id = event.event_id();
        let append = self.store()?.append_event(AppendRequest::event(event))?;
        let metadata = self.index.synchronize()?.metadata;
        Ok(SpaceCreateResponse {
            space_id,
            intent_revision_id,
            provisional,
            event_id,
            batch_id: append.batch_id.to_string(),
            commit_oid: append.commit_oid,
            tree: metadata.indexed_tree_oid,
            generation: metadata.projection_generation,
        })
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
        let append = self.store()?.append_event(AppendRequest::event(event))?;
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

    /// Evaluates every accepted Context's structured `recheck_when` entries against the local
    /// checkouts and records the outcome in the local `context_item.stale_reason` column.
    ///
    /// Nothing here becomes an Event. A Context whose entries all still hold has its previous
    /// stale marking cleared; a Context with no structured entry is left exactly as it was, since
    /// this pass has nothing to say about it. Every Git read is bounded and read-only, and any
    /// failure — no checkout, no `git`, unknown commit, timeout — is reported as "unevaluated"
    /// rather than as staleness or an error.
    fn context_recheck(&self) -> Result<ContextRecheckResponse> {
        let snapshot = self.snapshot()?;
        let checkouts = self.recheck_checkouts()?;
        let mut results = Vec::new();
        let mut rows = Vec::new();
        let mut unevaluated_entries = 0;
        for space in snapshot.projection.spaces.values() {
            for (context_id, context) in &space.contexts {
                let ContextGovernanceStatus::Accepted { revision_id, .. } = context.governance
                else {
                    continue;
                };
                let Some(revision) = context.revisions.get(&revision_id) else {
                    continue;
                };
                let conditions = revision
                    .revision
                    .recheck_when
                    .iter()
                    .filter_map(|entry| {
                        RecheckCondition::parse(entry).map(|condition| (entry.clone(), condition))
                    })
                    .collect::<Vec<_>>();
                if conditions.is_empty() {
                    continue;
                }
                let mut stale_reason = None;
                let mut unevaluated = Vec::new();
                for (entry, condition) in conditions {
                    match evaluate_recheck_condition(&condition, &checkouts) {
                        RecheckAnswer::Stale(reason) => {
                            stale_reason.get_or_insert(reason);
                        }
                        RecheckAnswer::Current => {}
                        RecheckAnswer::Unevaluated => unevaluated.push(entry),
                    }
                }
                unevaluated_entries += unevaluated.len();
                rows.push((context_id.to_string(), stale_reason.clone()));
                results.push(ContextRecheckResult {
                    context_id: *context_id,
                    revision_id,
                    stale_reason,
                    unevaluated,
                });
            }
        }
        self.index.record_stale_reasons(&rows)?;
        let metadata = self.index.synchronize()?.metadata;
        Ok(ContextRecheckResponse {
            evaluated_contexts: results.len(),
            stale_contexts: results
                .iter()
                .filter(|result| result.stale_reason.is_some())
                .count(),
            unevaluated_entries,
            results,
            tree: metadata.indexed_tree_oid,
            generation: metadata.projection_generation,
        })
    }

    /// Every available local checkout in the Catalog, in Repository order.
    ///
    /// A `recheck_when` entry names a commit and a Repository-relative path but not a Repository,
    /// so evaluation asks each configured checkout in turn and uses the first that knows the
    /// commit. Deterministic because the Catalog order is.
    fn recheck_checkouts(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .repositories
            .list()?
            .into_iter()
            .flat_map(|repository| repository.locators)
            .filter(|locator| {
                locator.availability == RepositoryAvailability::Available
                    && locator.checkout_path.is_dir()
            })
            .map(|locator| locator.checkout_path)
            .collect())
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
            tree: snapshot.metadata.indexed_tree_oid.clone(),
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

/// What one local checkout could say about a structured `recheck_when` entry.
#[derive(Clone, Debug, Eq, PartialEq)]
enum RecheckAnswer {
    /// The condition fired; the payload explains it in one sentence.
    Stale(String),
    /// The condition was answered and still holds.
    Current,
    /// No configured checkout could answer it: unknown commit, missing `git`, or a timeout.
    Unevaluated,
}

/// Answers one structured condition against the first checkout that knows its commit.
fn evaluate_recheck_condition(
    condition: &RecheckCondition,
    checkouts: &[PathBuf],
) -> RecheckAnswer {
    for checkout in checkouts {
        let answer = match condition {
            RecheckCondition::BranchAdvanced { branch, commit } => {
                let Some(head) = bounded_git(checkout, &["rev-parse", "--verify", branch]) else {
                    continue;
                };
                let head = head.trim();
                if head.is_empty() {
                    continue;
                }
                if head.starts_with(commit.as_str()) {
                    RecheckAnswer::Current
                } else {
                    RecheckAnswer::Stale(format!(
                        "branch_advanced: {branch} moved from {commit} to {head}"
                    ))
                }
            }
            RecheckCondition::FileChangedSince { commit, path } => {
                if bounded_git(
                    checkout,
                    &["cat-file", "-e", &format!("{commit}^{{commit}}")],
                )
                .is_none()
                {
                    continue;
                }
                let Some(changed) = bounded_git(
                    checkout,
                    &[
                        "log",
                        "--format=%H",
                        "-1",
                        &format!("{commit}..HEAD"),
                        "--",
                        path,
                    ],
                ) else {
                    continue;
                };
                let changed = changed.trim().to_owned();
                if changed.is_empty() {
                    RecheckAnswer::Current
                } else {
                    RecheckAnswer::Stale(format!(
                        "file_changed_since: {path} changed after {commit} in {changed}"
                    ))
                }
            }
        };
        return answer;
    }
    RecheckAnswer::Unevaluated
}

/// Runs one read-only `git` query under the same bounded timeout Reference derivation uses.
///
/// Returns `None` for a missing `git`, a non-zero exit, non-UTF-8 output, or a timeout, so a
/// broken or slow checkout can never fail or hang the caller.
fn bounded_git(checkout: &Path, arguments: &[&str]) -> Option<String> {
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut buffer)
            .ok()
            .map(|_| buffer)
    });
    let deadline = std::time::Instant::now() + reference_derivation::GIT_QUERY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let output = reader.join().ok().flatten()?;
    status?.success().then_some(())?;
    String::from_utf8(output).ok()
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

/// Artifact coordinates one Claim can be compared on: its own refs plus the coordinates the
/// server derived from its text. Deduplicated and ordered so analysis stays reproducible.
fn serialize_candidate_reviews(value: &impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("serialize Candidate Review response: {error}"),
        )
    })
}

/// Adds the two response-only Candidate Review aids that no Git fact carries: whether the Claim's
/// applicability was inherited from the Task Intent, and the `problem_view` that confirmation
/// would write. Both exist so a reviewer can correct them before confirming.
/// Marks each recommended Space in one serialized Full Review row with whether that Space is
/// still the server's provisional proposal.
///
/// This is response-side only: the stored recommendation carries no such field, and a proposed
/// new Space is provisional by construction because confirming it is what creates the Space.
fn insert_space_recommendation_provisional(row: &mut Value, provisional: &BTreeSet<SpaceId>) {
    let provisional = provisional
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let Some(recommendations) = row
        .get_mut("space_recommendations")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for recommendation in recommendations {
        let Some(object) = recommendation.as_object_mut() else {
            continue;
        };
        let is_provisional = match object.get("kind").and_then(Value::as_str) {
            Some("proposed_new_space_intent") => true,
            Some("existing") => object
                .get("space_id")
                .and_then(Value::as_str)
                .is_some_and(|space_id| provisional.contains(space_id)),
            _ => continue,
        };
        object.insert("provisional".to_owned(), Value::Bool(is_provisional));
    }
}

fn insert_review_aids(
    runtime: &Runtime,
    row: &mut Value,
    view: &CandidateReviewView,
) -> Result<()> {
    let derived = match view.content.problem_view.clone() {
        Some(problem_view) => Some(problem_view),
        None => runtime.derived_problem_view(view.source_episode)?,
    };
    let Some(object) = row.as_object_mut() else {
        return Ok(());
    };
    object.insert(
        "applicability_inherited".to_owned(),
        Value::Bool(APPLICABILITY_INHERITED),
    );
    if let Some(problem_view) = derived {
        object.insert(
            "derived_problem_view".to_owned(),
            Value::String(problem_view),
        );
    }
    Ok(())
}

/// Adds the non-blocking Chinese-default advisory to one serialized full Review row.
fn insert_language_hint(row: &mut Value, statement: &str) {
    let Some(hint) = language_hint(statement) else {
        return;
    };
    if let Some(object) = row.as_object_mut() {
        object.insert("language_hint".to_owned(), Value::String(hint));
    }
}

/// Upper bound on a derived `problem_view`. It is a retrieval and orientation aid, not a second
/// copy of the Intent, so it is truncated rather than allowed to grow with the Task.
const MAX_DERIVED_PROBLEM_VIEW_CHARS: usize = 400;

/// Renders one Working Intent as the problem a Context answers: the goal first, then the declared
/// scope, then what stayed open. Returns `None` when the Intent says nothing usable.
fn compose_problem_view(intent: &WorkingIntentSnapshot) -> Option<String> {
    let mut parts = Vec::new();
    let goal = intent.goal.trim();
    if !goal.is_empty() {
        parts.push(goal.to_owned());
    }
    for (label, values) in [
        ("In scope", &intent.in_scope),
        ("Open questions", &intent.open_questions),
    ] {
        let joined = values
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if !joined.is_empty() {
            parts.push(format!("{label}: {joined}"));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let composed = parts.join(" | ");
    if composed.chars().count() <= MAX_DERIVED_PROBLEM_VIEW_CHARS {
        return Some(composed);
    }
    let mut truncated = composed
        .chars()
        .take(MAX_DERIVED_PROBLEM_VIEW_CHARS - 1)
        .collect::<String>();
    truncated.push('…');
    Some(truncated)
}

fn candidate_artifact_refs(claim: &CheckpointClaim) -> Vec<ArtifactRef> {
    let mut refs = claim.artifact_refs.clone();
    for reference in &claim.engineering_references {
        let derived = ArtifactRef {
            repository_id: reference.repository_id.clone(),
            locator: reference.locator.clone(),
        };
        if !refs.contains(&derived) {
            refs.push(derived);
        }
    }
    refs
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
        unknowns.push(CheckpointUnknown {
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
        unknowns.push(CheckpointUnknown {
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
    // Path and identifier spellings the Checkpoint's own Evidence text carried but the checkout
    // could not place. They are retrieval text only, never graph facts, and are recomputed from
    // the persisted Claim so a rebuild reproduces the same searchable Candidate.
    let evidence_summaries = evidence
        .iter()
        .filter_map(|snapshot| {
            snapshot
                .content
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let hints = reference_derivation::claim_hints(
        &claim.statement,
        &claim.rationale,
        &evidence_summaries
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        &claim.engineering_references,
    );
    let draft = error_code.is_none().then(|| ContextRevisionDraft {
        problem_view: None,
        hints,
        kind,
        topic_key: claim.topic_key_hint.clone(),
        statement: claim.statement.clone(),
        rationale: claim.rationale.clone(),
        applicability: claim.applicability.clone(),
        assumptions: claim.assumptions.clone(),
        recheck_when: claim.recheck_when.clone(),
        relations: claim.relations.clone(),
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
    reference: &CheckpointEvidenceRef,
    claim: &CheckpointClaim,
    episode: &WorkEpisodeView,
    signals: &[TaskSignalRecord],
    snapshot: &DomainSnapshot,
    visited_observations: &mut HashSet<WorkObservationId>,
    observation_ids: &mut Vec<WorkObservationId>,
    evidence: &mut Vec<EvidenceSnapshotDraft>,
) -> std::result::Result<(), &'static str> {
    match reference {
        CheckpointEvidenceRef::Observation { observation_id } => {
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
        CheckpointEvidenceRef::TaskSignal { signal_id } => {
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
        CheckpointEvidenceRef::ContextEvidence {
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
                NormalizedWorkObservation::ContextUse { .. } => EvidenceType::SourceSnapshot,
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

fn normalized_observation_limitations(_observation: &WorkObservation) -> Vec<String> {
    vec!["The snapshot excludes raw transcript and tool output".to_owned()]
}

fn deduplicate_candidate_evidence(evidence: &mut Vec<EvidenceSnapshotDraft>) {
    let mut seen = HashSet::with_capacity(evidence.len());
    evidence.retain(|snapshot| {
        serde_json::to_string(snapshot).map_or(true, |encoded| seen.insert(encoded))
    });
}

/// Identity of one committed Candidate submission, as the Builder records it.
struct CandidateSubmissionOutcomeRecord {
    candidate_id: CandidateId,
    event_id: EventId,
    status: CandidateSubmissionStatus,
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
        duplicates: view
            .duplicates
            .into_iter()
            .map(|duplicate| CandidateBuildDuplicateSummary {
                checkpoint_id: duplicate.checkpoint_id,
                claim_id: duplicate.claim_id,
                duplicate_of_candidate_id: duplicate.duplicate_of_candidate_id,
                similarity_basis_points: duplicate.similarity_basis_points,
            })
            .collect(),
    })
}

/// Server-owned plan for one validated Candidate Confirmation that has not been written yet.
struct PreparedCandidateConfirmation {
    candidate_id: sctx_domain::CandidateId,
    proposed_plan: CandidateConfirmationPlan,
    proposed_space_group_key: Option<ProposedSpaceGroupKey>,
    assessments: Vec<CandidateRelationAssessment>,
}

/// One reserved server-owned plan waiting for its shared Git batch.
struct ReservedCandidateConfirmation {
    candidate_id: sctx_domain::CandidateId,
    plan: CandidateConfirmationPlan,
    assessments: Vec<CandidateRelationAssessment>,
}

/// Git and Runtime result of writing one reserved plan.
struct WrittenCandidateConfirmation {
    append: AppendBatchOutcome,
    already_confirmed: bool,
}

/// Maximum Candidates one batch Review decision may carry.
const MAX_CANDIDATE_BATCH_ITEMS: usize = 32;

/// Chooses the single or batch `candidate_discard` shape from the exact declared field.
fn decode_candidate_discard_request(
    arguments: Value,
) -> std::result::Result<CandidateDiscardRequest, ToolFailure> {
    if candidate_batch_requested(&arguments)? {
        decode_arguments(arguments).map(|input| CandidateDiscardRequest::Batch(Box::new(input)))
    } else {
        decode_arguments(arguments).map(|input| CandidateDiscardRequest::Single(Box::new(input)))
    }
}

/// Chooses the single or batch `candidate_confirm` shape from the exact declared field.
fn decode_candidate_confirm_request(
    arguments: Value,
) -> std::result::Result<CandidateConfirmRequest, ToolFailure> {
    if candidate_batch_requested(&arguments)? {
        if arguments
            .as_object()
            .is_some_and(|object| object.contains_key("edits"))
        {
            return Err(invalid(
                "batch candidate_confirm does not accept edits; confirm that Candidate with candidate_id instead",
            )
            .into());
        }
        validate_candidate_primary(&arguments)?;
        decode_arguments(arguments).map(|input| CandidateConfirmRequest::Batch(Box::new(input)))
    } else {
        validate_candidate_primary(&arguments)?;
        decode_arguments(arguments).map(|input| CandidateConfirmRequest::Single(Box::new(input)))
    }
}

/// Resolves the exclusive `candidate_id` / `candidate_ids` selection the flat host view cannot state.
///
/// The host declaration lists both fields as optional properties, because a top-level union
/// declaration degrades into an untyped map on at least one host; the exclusivity is therefore
/// authoritative server validation.
fn candidate_batch_requested(arguments: &Value) -> std::result::Result<bool, ToolFailure> {
    let object = arguments.as_object();
    let single = object.is_some_and(|object| object.contains_key("candidate_id"));
    let batch = object.is_some_and(|object| object.contains_key("candidate_ids"));
    match (single, batch) {
        (true, true) => {
            Err(invalid("send exactly one of candidate_id or candidate_ids, not both").into())
        }
        (false, false) => Err(invalid("send exactly one of candidate_id or candidate_ids").into()),
        _ => Ok(batch),
    }
}

/// Resolves the exclusive Primary Space selection the flat host view cannot state.
fn validate_candidate_primary(arguments: &Value) -> std::result::Result<(), ToolFailure> {
    let Some(primary) = arguments.get("primary").and_then(Value::as_object) else {
        return Ok(());
    };
    let existing = primary.contains_key("existing_space_id");
    let proposed = primary.contains_key("new_space_recommendation_id");
    match (existing, proposed) {
        (true, true) => Err(invalid(
            "primary must send exactly one of existing_space_id or new_space_recommendation_id, not both",
        )
        .into()),
        (false, false) => Err(invalid(
            "primary must send exactly one of existing_space_id or new_space_recommendation_id",
        )
        .into()),
        _ => Ok(()),
    }
}

fn parse_batch_candidate_ids(values: &[String]) -> Result<Vec<sctx_domain::CandidateId>> {
    if values.is_empty() {
        return Err(invalid("candidate_ids must not be empty"));
    }
    if values.len() > MAX_CANDIDATE_BATCH_ITEMS {
        return Err(invalid(format!(
            "candidate_ids must not exceed {MAX_CANDIDATE_BATCH_ITEMS} Candidates"
        )));
    }
    let candidate_ids = values
        .iter()
        .map(|value| parse_id_value::<sctx_domain::CandidateId>(value, "candidate_ids"))
        .collect::<Result<Vec<_>>>()?;
    if candidate_ids.iter().collect::<BTreeSet<_>>().len() != candidate_ids.len() {
        return Err(invalid("candidate_ids must be unique"));
    }
    Ok(candidate_ids)
}

/// Reports the exact batch member that failed before anything was written.
fn batch_preparation_error(
    position: usize,
    candidate_id: sctx_domain::CandidateId,
    error: &Error,
) -> Error {
    Error::new(
        error.kind(),
        format!(
            "{} (batch item {position}, candidate {candidate_id}); no Candidate in this batch was written",
            error.message()
        ),
    )
}

/// Normalized statement tokens used only for Builder deduplication.
fn statement_tokens(statement: &str) -> BTreeSet<String> {
    sctx_index::normalize_search_text(statement)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Deterministic token Jaccard in basis points.
fn jaccard_basis_points(left: &BTreeSet<String>, right: &BTreeSet<String>) -> u16 {
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let intersection = left.intersection(right).count();
    let union = left.len() + right.len() - intersection;
    if union == 0 {
        return 0;
    }
    u16::try_from(intersection.saturating_mul(10_000) / union).unwrap_or(10_000)
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

/// Reads a Context Pack for an already-authoritative `ActiveTask` without mutation, budgeting the
/// item/association selection itself against the requested `detail_level` instead of only
/// re-shaping a Full-selected result. Local operator tooling (for example the CLI `--compact`
/// flag) uses this so a Compact request actually admits more items under one token budget.
///
/// # Errors
///
/// Returns an input error when no Working Intent update established the Task.
pub fn task_context_readonly_with_detail_at_root(
    root: impl AsRef<Path>,
    input: &TaskContextReadInput,
    detail_level: ContextPackDetailLevel,
) -> Result<TaskContextResponse> {
    Runtime::open(root.as_ref())?.task_context_readonly_with_detail(input, detail_level)
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

/// Finalizes one Agent-authored Checkpoint for the caller's current `ActiveTask` and Intent.
///
/// # Errors
///
/// Returns typed Session ownership, Evidence, privacy, Builder, or storage errors.
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

/// Lists bounded whole Candidate Review summaries for one exact `ActiveTask`, budgeting the
/// per-review token estimate against the requested `detail_level` instead of only re-shaping a
/// Full-selected page. Local operator tooling (for example the CLI `--compact` flag) uses this so
/// a Compact request actually admits more triage rows under one token budget.
///
/// # Errors
///
/// Returns typed Session, cursor, budget, Runtime, index, or Review assembly errors.
pub fn candidate_list_with_detail_at_root(
    root: impl AsRef<Path>,
    input: &CandidateListInput,
    detail_level: ContextPackDetailLevel,
) -> Result<CandidateListResponse> {
    Runtime::open(root.as_ref())?.candidate_list_with_detail(input, detail_level)
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

/// Atomically discards several owned Pending Candidate Reviews.
///
/// # Errors
///
/// Returns typed validation, CAS, conflict or storage errors naming the failing `CandidateId`.
pub fn candidate_discard_batch_at_root(
    root: impl AsRef<Path>,
    input: &CandidateDiscardBatchInput,
) -> Result<CandidateDiscardBatchResponse> {
    Runtime::open(root.as_ref())?.candidate_discard_batch(input)
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

/// Confirms several owned Pending Candidates under one Space organization.
///
/// # Errors
///
/// Returns typed validation, CAS, conflict, privacy or storage errors naming the failing
/// `CandidateId`; a rejected plan is reported before any Candidate is written.
pub fn candidate_confirm_batch_at_root(
    root: impl AsRef<Path>,
    input: &CandidateConfirmBatchInput,
) -> Result<CandidateConfirmBatchResponse> {
    Runtime::open(root.as_ref())?.candidate_confirm_batch(input)
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

/// Creates one explicitly named Space from a complete Intent snapshot.
///
/// # Errors
///
/// Returns typed Intent validation, privacy, Writer, or projection errors.
pub fn space_create_at_root(
    root: impl AsRef<Path>,
    input: &SpaceCreateInput,
) -> Result<SpaceCreateResponse> {
    Runtime::open(root.as_ref())?.space_create(input)
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
    let runtime = Runtime::open(root.as_ref())?;
    let response = runtime.association_rebuild(input)?;
    if !input.diagnose_only {
        // A rebuild is the moment the local checkouts were just read anyway, so the structured
        // `recheck_when` evaluation rides along. It is advisory local state: a failure here must
        // not turn a successful Graph rebuild into an error.
        let _ = runtime.context_recheck();
    }
    Ok(response)
}

/// Evaluates every accepted Context's structured `recheck_when` entries against local checkouts.
///
/// The outcome is recorded in this machine's `context_item.stale_reason` projection column and
/// never written to Git. It is cleared by any projection rebuild, so re-run it after new Events.
///
/// # Errors
///
/// Returns typed storage errors. An unanswerable condition is reported, not raised.
pub fn context_recheck_at_root(root: impl AsRef<Path>) -> Result<ContextRecheckResponse> {
    Runtime::open(root.as_ref())?.context_recheck()
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

/// Translates the explicit local `[context_ttl]` policy into query-time retrieval settings.
const fn context_ttl_settings(policy: &sctx_local_state::ContextTtlPolicy) -> ContextTtlSettings {
    ContextTtlSettings {
        validation_seconds: policy.validation_seconds,
        progress_seconds: policy.progress_seconds,
        now_unix_seconds: None,
    }
}

#[allow(clippy::too_many_arguments)]
/// Reads the installation-local Context usage prior out of Task Runtime state.
///
/// Every failure degrades to "no prior": the ranking signal is advisory local state, and a busy
/// or unreadable Runtime database must never turn a retrieval into an error or a different Pack
/// than the one an installation without recorded usage would return.
#[derive(Clone, Debug)]
struct RuntimeUsagePrior {
    tasks: TaskRuntime,
}

impl UsagePriorSource for RuntimeUsagePrior {
    fn usage_counts(&self, context_ids: &[ContextId]) -> BTreeMap<ContextId, ContextUsageCounts> {
        self.tasks
            .context_usage_totals(context_ids)
            .unwrap_or_default()
            .into_iter()
            .map(|(context_id, totals)| {
                (
                    context_id,
                    ContextUsageCounts {
                        reused: totals.reused,
                        ignored: totals.ignored,
                        refuted: totals.refuted,
                    },
                )
            })
            .collect()
    }
}

/// Records which Contexts one retrieval entry point just handed to the Agent.
///
/// The record is disposable local state and strictly best effort: it is written after the
/// response is already built, and any storage failure is dropped so the Agent still receives the
/// Pack it was going to receive.
fn record_injected_contexts(
    tasks: &TaskRuntime,
    response: &TaskContextResponse,
    source: ContextInjectionSource,
) {
    let injected = response
        .items
        .iter()
        .map(|item| InjectedContext {
            context_id: item.context.context_id,
            revision_id: item.context.revision_id,
        })
        .chain(response.compact_items.iter().map(|item| InjectedContext {
            context_id: item.context_id,
            revision_id: item.revision_id,
        }))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if injected.is_empty() {
        return;
    }
    let _ = tasks.record_task_injections(
        response.task_id,
        response.intent_revision_id,
        source,
        &injected,
    );
}

#[allow(clippy::too_many_arguments)]
fn build_task_context_response(
    index: &ProjectionIndex,
    engineering_graph: Option<&EngineeringProjectionStore>,
    tasks: &TaskRuntime,
    injection_source: ContextInjectionSource,
    snapshot: &TaskSessionSnapshot,
    resolved_focus: Option<ResolvedFocus>,
    token_budget: usize,
    max_spaces: usize,
    detail_level: ContextPackDetailLevel,
    context_ttl: ContextTtlSettings,
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
    let engine = if let Some(engineering_graph) = engineering_graph {
        SearchEngine::with_engineering_graph(index.clone(), engineering_graph.clone())
    } else {
        SearchEngine::new(index.clone())
    };
    let pack = engine
        .with_context_ttl(context_ttl)
        .with_usage_prior(Arc::new(RuntimeUsagePrior {
            tasks: tasks.clone(),
        }))
        .task_context_pack_with_detail(&request, detail_level)?;
    let retrieval_paths = pack
        .items
        .iter()
        .map(|item| TaskContextRetrievalPaths {
            association_space_id: item.association_space_id,
            context_id: item.context.context_id,
            paths: item.retrieval_paths.clone(),
        })
        .collect::<Vec<_>>();
    let response = TaskContextResponse {
        task_session_id: snapshot.task_session_id,
        task_id: snapshot.task_id,
        intent_revision_id: current.revision_id,
        detail_level: pack.detail_level,
        candidate_spaces: pack.associations,
        compact_candidate_spaces: pack.compact_associations,
        items: pack.items,
        compact_items: pack.compact_items,
        retrieval_paths,
        graph_diagnostics: pack.graph_diagnostics,
        query_token_explanation: pack.query_token_explanation,
        task_fingerprint: pack.task_fingerprint,
        tree: pack.indexed_tree_oid,
        generation: pack.projection_generation,
        artifact_generation: pack.artifact_generation,
        graph_context_tree_oid: pack.graph_context_tree_oid,
        token_budget: pack.token_budget,
        estimated_tokens: pack.estimated_tokens,
        omitted: pack.omitted,
    };
    record_injected_contexts(tasks, &response, injection_source);
    Ok(response)
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
    scope: AuthorizedSessionScope,
    /// `[context_ttl]` read from the same `config.toml` read that froze `catalog`.
    context_ttl: ContextTtlSettings,
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
            return Err(ToolFailure::locator_invalid());
        }
        let authorization = authorize_public_call(&self.root, &locator)?;
        if let Some(hook) = &self.authorization_linearization_hook {
            hook();
        }
        let preflight_tasks = if requires_runtime_identity_preflight(&call.name) {
            let runtime_database = self.root.join("state/runtime.sqlite");
            if !runtime_database.is_file() {
                return Err(identity_target_unavailable(&call.name));
            }
            let tasks = TaskRuntime::initialize(&self.root)
                .map_err(|error| runtime_open_failure(&call.name, error))?;
            authorize_runtime_identity_target(&call.name, &call.arguments, &tasks)?;
            Some(tasks)
        } else {
            None
        };
        // A durable Checkpoint ACK is Runtime state only (ADR-0003), so it answers from the
        // database the identity preflight already opened. Opening the Git store, the retrieval
        // index and the Repository Registry here would be latency the receipt never spends.
        if call.name == "task_checkpoint" {
            let tasks = preflight_tasks.ok_or_else(|| identity_target_unavailable(&call.name))?;
            let input: TaskCheckpointInput = decode_arguments(call.arguments.clone())?;
            let response =
                durable_checkpoint(&tasks, &input).map_err(ToolFailure::task_context_failed)?;
            return serde_json::to_value(response).map_err(serialization_failure);
        }
        // Every remaining tool reuses exactly what authorization and the identity preflight
        // already opened: the frozen Catalog, its `[context_ttl]` from the same read, and the
        // Task Runtime the preflight resolved this call's identity target against.
        let runtime = Runtime::open_with_catalog(
            &self.root,
            authorization.catalog.clone(),
            RuntimeOpenParts {
                session_scope: Some(authorization.scope.clone()),
                context_ttl: Some(authorization.context_ttl),
                tasks: preflight_tasks,
            },
        )
        .map_err(|error| runtime_open_failure(&call.name, error))?;
        self.runtime = Some(runtime);
        let arguments = call.arguments.clone();
        match call.name.as_str() {
            "task_intent_update" => self.task_intent_update(arguments),
            "task_artifact_focus" => self.task_artifact_focus(arguments),
            "task_signal_supersede" => self.task_signal_supersede(arguments),
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
            "space_create" => self.space_create(arguments),
            _ => unreachable!("public tool was validated before dispatch"),
        }
    }

    fn context_search(&self, arguments: Value) -> ToolResult {
        let input: SearchInput = decode_arguments(arguments)?;
        let request = input.into_request()?;
        let match_mode = request.match_mode;
        let response = SearchEngine::new(self.runtime().index.clone())
            .with_context_ttl(self.runtime().context_ttl)
            .search(&request)?;
        let conflicts = collect_conflicts(&response.results);
        let coverage_basis_points = response
            .results
            .iter()
            .map(|result| {
                json!({
                    "context_id": result.context_id,
                    "revision_id": result.revision_id,
                    "coverage_basis_points": result.match_reason.coverage_basis_points,
                })
            })
            .collect::<Vec<_>>();
        let mut data = serde_json::to_value(response).map_err(serialization_failure)?;
        insert_fields(
            &mut data,
            [
                (
                    "conflicts",
                    serde_json::to_value(conflicts).map_err(serialization_failure)?,
                ),
                (
                    "match_mode",
                    serde_json::to_value(match_mode).map_err(serialization_failure)?,
                ),
                ("coverage_basis_points", Value::Array(coverage_basis_points)),
                (
                    "match_reason",
                    json!("structured_filters_and_full_text_rank"),
                ),
            ],
        )?;
        Ok(data)
    }

    fn task_context(&self, arguments: Value) -> ToolResult {
        let (arguments, detail_level) = split_detail_level(arguments)?;
        let input: TaskContextReadInput = decode_arguments(arguments)?;
        input.validate()?;
        let response = self
            .runtime()
            .task_context_readonly_with_detail(&input, detail_level)
            .map_err(ToolFailure::task_context_failed)?;
        match detail_level {
            ContextPackDetailLevel::Compact => {
                serde_json::to_value(response.compact()).map_err(serialization_failure)
            }
            ContextPackDetailLevel::Full => {
                serde_json::to_value(response).map_err(serialization_failure)
            }
        }
    }

    fn task_intent_update(&self, arguments: Value) -> ToolResult {
        let (arguments, detail_level) = split_detail_level(arguments)?;
        let input: TaskIntentUpdateInput = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_intent_update_with_detail(&input, detail_level)
            .map_err(ToolFailure::intent_update_failed)?;
        match detail_level {
            ContextPackDetailLevel::Compact => {
                serde_json::to_value(CompactTaskIntentUpdateResponse {
                    context: response.context.compact(),
                    revision_status: response.revision_status,
                    active_signals: response.active_signals,
                })
                .map_err(serialization_failure)
            }
            ContextPackDetailLevel::Full => {
                serde_json::to_value(response).map_err(serialization_failure)
            }
        }
    }

    fn task_artifact_focus(&self, arguments: Value) -> ToolResult {
        let (arguments, detail_level) = split_detail_level(arguments)?;
        let input: ArtifactFocusQuery = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_artifact_focus_with_detail(&input, detail_level)
            .map_err(ToolFailure::task_context_failed)?;
        match detail_level {
            ContextPackDetailLevel::Compact => {
                serde_json::to_value(CompactArtifactFocusQueryResponse {
                    resolved_focus: response.resolved_focus,
                    context: response.context.compact(),
                })
                .map_err(serialization_failure)
            }
            ContextPackDetailLevel::Full => {
                serde_json::to_value(response).map_err(serialization_failure)
            }
        }
    }

    fn task_signal_supersede(&self, arguments: Value) -> ToolResult {
        let input: TaskSignalSupersedeInput = decode_arguments(arguments)?;
        let response = self
            .runtime()
            .task_signal_supersede(&input)
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

    fn space_create(&self, arguments: Value) -> ToolResult {
        let input = decode_arguments::<McpSpaceCreateInput>(arguments)?.into_inner();
        let response = self.runtime().space_create(&input)?;
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
                    // A conflicted Space has no winning head, so it never reports provisional.
                    "provisional": space.intent.heads.len() == 1
                        && space
                            .intent
                            .heads
                            .first()
                            .and_then(|revision_id| space.intent.revisions.get(revision_id))
                            .is_some_and(|revision| revision.provisional),
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
        let (arguments, detail_level) = split_detail_level(arguments)?;
        let input: CandidateListInput = decode_arguments(arguments)?;
        let runtime = self.runtime();
        runtime
            .candidate_list_with_detail(&input, detail_level)
            .and_then(|response| {
                if detail_level == ContextPackDetailLevel::Compact {
                    return serialize_candidate_reviews(&response.compact());
                }
                let mut value = serialize_candidate_reviews(&response)?;
                if let Some(rows) = value.get_mut("reviews").and_then(Value::as_array_mut) {
                    for (row, summary) in rows.iter_mut().zip(response.reviews.iter()) {
                        insert_review_aids(runtime, row, &summary.0)?;
                        insert_language_hint(row, &summary.0.content.statement);
                        insert_space_recommendation_provisional(
                            row,
                            &response.provisional_space_ids,
                        );
                    }
                }
                Ok(value)
            })
            .map_err(ToolFailure::candidate_review_failed)
    }

    fn candidate_get(&self, arguments: Value) -> ToolResult {
        let input: CandidateGetInput = decode_arguments(arguments)?;
        let runtime = self.runtime();
        runtime
            .candidate_get(&input)
            .and_then(|response| {
                let mut value = serialize_candidate_reviews(&response)?;
                insert_review_aids(runtime, &mut value, &response)?;
                insert_space_recommendation_provisional(
                    &mut value,
                    &provisional_space_ids(runtime.snapshot()?.as_ref()),
                );
                Ok(value)
            })
            .map_err(ToolFailure::candidate_review_failed)
    }

    fn candidate_discard(&self, arguments: Value) -> ToolResult {
        let runtime = self.runtime();
        match decode_candidate_discard_request(arguments)? {
            CandidateDiscardRequest::Single(input) => runtime
                .candidate_discard(&input)
                .map(|response| CandidateDiscardOutcome::Single(Box::new(response))),
            CandidateDiscardRequest::Batch(input) => runtime
                .candidate_discard_batch(&input)
                .map(CandidateDiscardOutcome::Batch),
        }
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
        let runtime = self.runtime();
        match decode_candidate_confirm_request(arguments)? {
            CandidateConfirmRequest::Single(input) => runtime
                .candidate_confirm(&input)
                .map(|response| CandidateConfirmOutcome::Single(Box::new(response))),
            CandidateConfirmRequest::Batch(input) => runtime
                .candidate_confirm_batch(&input)
                .map(CandidateConfirmOutcome::Batch),
        }
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
    /// Error family reported as `kind` when the domain `ErrorKind` is not the useful
    /// grouping. Only authorization uses it today: its four distinct causes must stay
    /// distinguishable by `code` while remaining one family a client can branch on.
    family: Option<&'static str>,
    error: Error,
}

impl ToolFailure {
    /// One failure whose `kind` is derived from its domain `ErrorKind`.
    fn coded(code: &'static str, error: Error) -> Self {
        Self {
            code,
            family: None,
            error,
        }
    }

    fn maintenance_failed(error: Error) -> Self {
        let busy = error.kind() == ErrorKind::MaintenanceBusy;
        drop(error);
        Self::coded(
            if busy {
                "maintenance_busy"
            } else {
                "maintenance_unavailable"
            },
            Error::new(
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
        )
    }

    /// The one error family every authorization refusal belongs to.
    ///
    /// It stays a single `kind` so a client can branch on "this call was not authorized"
    /// exactly as before, while `code` names which of the four independent causes it was:
    /// a locator the call itself got wrong, a Session no Hook ever leased, a directory
    /// activation never covered, or a local failure that decided nothing at all. Merging
    /// them made every one of them read as "you sent a bad id", which is the wrong repair
    /// for three of the four.
    const SESSION_NOT_AUTHORIZED: &'static str = "session_not_authorized";

    fn authorization_failure(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            family: Some(Self::SESSION_NOT_AUTHORIZED),
            error: Error::new(ErrorKind::External, message),
        }
    }

    /// The call did not carry a usable Agent Session locator at all.
    fn locator_invalid() -> Self {
        Self::authorization_failure(
            "locator_invalid",
            "Shared Context MCP call carries no usable Agent Session locator: agent_kind and external_session_id are both required, non-empty, and must match this MCP client; external_session_id is the host session id shown in the <shared-context-active> marker (Codex: also $CODEX_SESSION_ID; Cursor: the conversation id)",
        )
    }

    /// No Hook ever authorized this Agent Session.
    fn lease_missing() -> Self {
        Self::authorization_failure(
            "lease_missing",
            "Shared Context has no authorization lease for this Agent Session: copy external_session_id verbatim from the <shared-context-active> marker (Codex: also $CODEX_SESSION_ID; Cursor: the conversation id) — never retype, derive, or invent one — and if no marker ever appeared run `sctx doctor --hooks`",
        )
    }

    /// The Session is leased, but its directory is not under an activated Repository.
    fn activation_disabled() -> Self {
        Self::authorization_failure(
            "activation_disabled",
            "Shared Context is not activated for this Agent Session: the directory it started in is not covered by any registered Repository. Register it with `sctx repository add` and start a new Agent Session",
        )
    }

    /// Authorization could not be decided locally, so nothing was decided.
    fn authorization_internal(error: &Error) -> Self {
        Self::authorization_failure(
            "authorization_internal",
            format!(
                "Shared Context could not evaluate this Agent Session's authorization: {}",
                error.message()
            ),
        )
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
        Self::coded(code, error)
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
        Self::coded(code, error)
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
        Self::coded(code, error)
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
        Self::coded(code, error)
    }

    fn task_target_failed(error: Error) -> Self {
        if error.kind() == ErrorKind::InvalidInput {
            Self::coded(
                "task_target_unavailable",
                Error::new(ErrorKind::External, "Task target is unavailable"),
            )
        } else {
            Self::task_context_failed(error)
        }
    }

    fn candidate_target_failed(error: Error) -> Self {
        if error.kind() == ErrorKind::InvalidInput {
            Self::coded(
                "candidate_review_unavailable",
                Error::new(
                    ErrorKind::External,
                    "Candidate Review target is unavailable",
                ),
            )
        } else {
            Self::candidate_review_failed(error)
        }
    }
}

impl From<Error> for ToolFailure {
    fn from(error: Error) -> Self {
        Self::coded(error_code(error.kind()), error)
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
struct McpSpaceCreateInput {
    #[serde(rename = "agent_kind")]
    _agent_kind: String,
    #[serde(rename = "external_session_id")]
    _external_session_id: String,
    intent: McpSpaceIntentInput,
}

/// Flat Intent object; `out_of_scope` and `domain_terms` are the only optional lists, exactly as
/// on `sctx space create`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSpaceIntentInput {
    title: String,
    problem: String,
    desired_outcome: String,
    in_scope: Vec<String>,
    acceptance_conditions: Vec<String>,
    #[serde(default)]
    out_of_scope: Vec<String>,
    #[serde(default)]
    domain_terms: Vec<String>,
}

impl McpSpaceCreateInput {
    fn into_inner(self) -> SpaceCreateInput {
        SpaceCreateInput {
            intent: IntentSnapshot {
                title: self.intent.title,
                problem: self.intent.problem,
                desired_outcome: self.intent.desired_outcome,
                in_scope: self.intent.in_scope,
                out_of_scope: self.intent.out_of_scope,
                acceptance_conditions: self.intent.acceptance_conditions,
                domain_terms: self.intent.domain_terms,
            },
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
    /// Defaults to `ranked`; `exact` keeps the strict all-tokens lookup.
    #[serde(default)]
    match_mode: SearchMatchMode,
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
            match_mode: self.match_mode,
        })
    }
}

type ToolResultSearch = std::result::Result<SearchRequest, ToolFailure>;

fn is_public_tool(name: &str) -> bool {
    matches!(
        name,
        "task_intent_update"
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
            | "space_create"
    )
}

fn runtime_open_failure(name: &str, error: Error) -> ToolFailure {
    match name {
        "task_intent_update" => ToolFailure::intent_update_failed(error),
        "task_artifact_focus" | "task_signal_supersede" | "task_checkpoint" | "task_context" => {
            ToolFailure::task_context_failed(error)
        }
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

/// Splits the optional `detail_level` selector out of one tool call. Every strict input struct
/// keeps `deny_unknown_fields`, so the selector is removed before the typed decode and defaults to
/// [`ContextPackDetailLevel::Compact`] when absent.
fn split_detail_level(
    mut arguments: Value,
) -> std::result::Result<(Value, ContextPackDetailLevel), ToolFailure> {
    let selector = arguments
        .as_object_mut()
        .and_then(|object| object.remove("detail_level"));
    let detail_level = match selector {
        None => ContextPackDetailLevel::default(),
        Some(value) => serde_json::from_value(value).map_err(|error| {
            ToolFailure::from(invalid(format!(
                "detail_level must be \"compact\" or \"full\": {error}"
            )))
        })?,
    };
    Ok((arguments, detail_level))
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
    macro_rules! decode_detail_leveled {
        ($input:ty) => {{
            let (arguments, _) = split_detail_level(arguments.clone())?;
            let _: $input = decode_arguments(arguments)?;
        }};
    }
    match name {
        "task_intent_update" => decode_detail_leveled!(TaskIntentUpdateInput),
        "task_artifact_focus" => {
            validate_locator_composition(arguments.get("locator"), false)?;
            decode_detail_leveled!(ArtifactFocusQuery);
        }
        "task_signal_supersede" => decode!(TaskSignalSupersedeInput),
        "task_checkpoint" => decode!(TaskCheckpointInput),
        "task_context" => decode_detail_leveled!(TaskContextReadInput),
        "repository_scan" => decode!(McpRepositoryScanInput),
        "engineering_reference_record" => {
            validate_locator_composition(arguments.get("locator"), true)?;
            decode!(McpEngineeringReferenceRecordInput);
        }
        "association_explain" => decode!(McpAssociationExplainInput),
        "association_rebuild" => decode!(McpAssociationRebuildInput),
        "context_search" => decode!(SearchInput),
        "context_get" => decode!(GetInput),
        "candidate_list" => decode_detail_leveled!(CandidateListInput),
        "candidate_get" => decode!(CandidateGetInput),
        "candidate_discard" => {
            let _ = decode_candidate_discard_request(arguments.clone())?;
        }
        "candidate_confirm" => {
            let _ = decode_candidate_confirm_request(arguments.clone())?;
        }
        "space_list" => decode!(SessionInput),
        "space_create" => decode!(McpSpaceCreateInput),
        _ => unreachable!("public tool name was checked"),
    }
    Ok(())
}

/// Exact coordinate fields each Artifact locator kind carries beyond `locator_kind`.
const LOCATOR_KIND_COORDINATES: [(&str, &[&str]); 6] = [
    ("file", &[]),
    ("module", &[]),
    ("api", &["protocol", "operation", "normalized_route"]),
    ("schema", &["namespace", "version", "qualified_name"]),
    (
        "symbol",
        &[
            "language",
            "module",
            "enclosing_type",
            "symbol_name",
            "signature",
        ],
    ),
    ("test", &["qualified_test_name"]),
];

/// Validates the kind-specific Artifact locator composition the flat host view cannot state.
///
/// The declared view lists every coordinate field as optional, because a host union declaration
/// degrades into an untyped map; the exact per-kind field set therefore stays authoritative server
/// validation and names the exact missing or foreign field.
fn validate_locator_composition(
    locator: Option<&Value>,
    with_path: bool,
) -> std::result::Result<(), ToolFailure> {
    let Some(object) = locator.and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(kind) = object.get("locator_kind").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some((_, coordinates)) = LOCATOR_KIND_COORDINATES
        .iter()
        .find(|(candidate, _)| *candidate == kind)
    else {
        return Ok(());
    };
    for field in *coordinates {
        if !object.contains_key(*field) {
            return Err(invalid(format!("locator_kind {kind} requires locator.{field}")).into());
        }
    }
    for field in object.keys() {
        if field == "locator_kind" || (with_path && field == "path") {
            continue;
        }
        if !coordinates.contains(&field.as_str()) {
            return Err(invalid(format!(
                "locator_kind {kind} does not accept locator.{field}"
            ))
            .into());
        }
    }
    Ok(())
}

fn locator_from_arguments(
    arguments: &Value,
) -> std::result::Result<ExternalSessionLocator, ToolFailure> {
    let object = arguments
        .as_object()
        .ok_or_else(ToolFailure::locator_invalid)?;
    let agent_kind = object
        .get("agent_kind")
        .and_then(Value::as_str)
        .ok_or_else(ToolFailure::locator_invalid)?;
    let external_session_id = object
        .get("external_session_id")
        .and_then(Value::as_str)
        .ok_or_else(ToolFailure::locator_invalid)?;
    ExternalSessionLocator::new(agent_kind, external_session_id)
        .map_err(|_| ToolFailure::locator_invalid())
}

fn authorize_public_call(
    root: &Path,
    locator: &ExternalSessionLocator,
) -> std::result::Result<AuthorizedCallSnapshot, ToolFailure> {
    // The successful nonblocking lease classification against this exact Catalog is the call's
    // authorization linearization point. Later expiry, SessionEnd, or Catalog replacement affects
    // the next call; this call carries the frozen Catalog and allowed Repository identities.
    // The three outcomes stay apart: a decided refusal names its own cause, while a
    // configuration, lock, or IO failure decided nothing and says so instead of blaming
    // the id the Agent sent.
    let authorization = || -> Result<std::result::Result<AuthorizedCallSnapshot, ToolFailure>> {
        let (catalog, context_ttl) =
            UserConfigStore::open_existing(root)?.repository_catalog_with_context_ttl()?;
        let context_ttl = context_ttl_settings(&context_ttl);
        Ok(
            match read_reconciled_session_scope(root, locator, &catalog)? {
                AuthorizedSessionScopeRead::Current(scope) if scope.decision.is_enabled() => {
                    Ok(AuthorizedCallSnapshot {
                        catalog,
                        scope,
                        context_ttl,
                    })
                }
                AuthorizedSessionScopeRead::Current(_) => Err(ToolFailure::activation_disabled()),
                AuthorizedSessionScopeRead::Missing => Err(ToolFailure::lease_missing()),
            },
        )
    };
    authorization().map_err(|error| ToolFailure::authorization_internal(&error))?
}

/// The single lease read every MCP path uses.
///
/// A lease is permanently bound to its Agent Session and never expires, so the
/// only thing that can change under a running Session is the Catalog. This one
/// non-blocking read therefore re-derives the decision from the lease's recorded
/// canonical `startup_cwd` against the Catalog frozen for this call, which is
/// pure: no filesystem stat, no Git, no Repository scan. A Repository registered
/// or removed mid-Session takes effect on the very next call.
fn read_reconciled_session_scope(
    root: &Path,
    locator: &ExternalSessionLocator,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<AuthorizedSessionScopeRead> {
    AuthorizedSessionScopeStore::initialize(root)?.try_read_reconciled(locator, catalog)
}

fn authorize_runtime_identity_target(
    name: &str,
    arguments: &Value,
    tasks: &TaskRuntime,
) -> std::result::Result<(), ToolFailure> {
    match name {
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
            let active = tasks
                .read_snapshot_by_locator(&locator)
                .map_err(ToolFailure::task_context_failed)?;
            if active.is_none() {
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
        "candidate_discard" => match decode_candidate_discard_request(arguments.clone())? {
            CandidateDiscardRequest::Single(input) => require_owned_task_and_candidate(
                tasks,
                &input.agent_kind,
                &input.external_session_id,
                &input.expected_task_id,
                &input.candidate_id,
            )?,
            CandidateDiscardRequest::Batch(input) => {
                for candidate_id in &input.candidate_ids {
                    require_owned_task_and_candidate(
                        tasks,
                        &input.agent_kind,
                        &input.external_session_id,
                        &input.expected_task_id,
                        candidate_id,
                    )?;
                }
            }
        },
        "candidate_confirm" => match decode_candidate_confirm_request(arguments.clone())? {
            CandidateConfirmRequest::Single(input) => require_owned_task_and_candidate(
                tasks,
                &input.agent_kind,
                &input.external_session_id,
                &input.expected_task_id,
                &input.candidate_id,
            )?,
            CandidateConfirmRequest::Batch(input) => {
                for candidate_id in &input.candidate_ids {
                    require_owned_task_and_candidate(
                        tasks,
                        &input.agent_kind,
                        &input.external_session_id,
                        &input.expected_task_id,
                        candidate_id,
                    )?;
                }
            }
        },
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
        "task_signal_supersede"
            | "task_checkpoint"
            | "candidate_get"
            | "candidate_discard"
            | "candidate_confirm"
    )
}

fn identity_target_unavailable(name: &str) -> ToolFailure {
    if matches!(name, "task_signal_supersede" | "task_checkpoint") {
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
            "CAS-record a lightweight Working Intent snapshot, optionally start a new explicit Task, and return its TaskContextPack. detail_level defaults to compact, which returns only the inheritable Context fields; pass full for retrieval paths, match reasons, and the automatic query token explanation.",
            task_intent_update_schema()
        ),
        tool_schema(
            "task_artifact_focus",
            "Declare one current File/Module/Symbol/API/Schema/Test focus under ActiveTask CAS and immediately retrieve exact historical Graph context. Repository identity and relative path are resolved by the configured local Catalog. detail_level defaults to compact.",
            task_artifact_focus_schema()
        ),
        tool_schema(
            "task_signal_supersede",
            "Supersede stable active Signal IDs under exact Task and Intent CAS guards.",
            task_signal_supersede_schema()
        ),
        tool_schema(
            "task_checkpoint",
            "Finalize one content-addressed Agent Checkpoint for the current ActiveTask and Intent. Submit focused Claims with self-contained Evidence summaries; the server durably queues untrusted Candidate drafts for bounded recovery by Candidate review reads. Empty Claims and Unknowns are a successful no-op.",
            task_checkpoint_schema()
        ),
        tool_schema(
            "task_context",
            "Read the Context Pack for an existing authoritative ActiveTask without changing runtime state. detail_level defaults to compact; omitted Contexts are named by context_id and title so they can be read explicitly.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1},
                    "token_budget": {"type": "integer", "minimum": MIN_TASK_CONTEXT_TOKEN_BUDGET, "default": 2000},
                    "max_spaces": {"type": "integer", "minimum": 1, "maximum": MAX_TASK_MAX_SPACES, "default": DEFAULT_TASK_MAX_SPACES},
                    "detail_level": detail_level_schema()
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
            "Search Context revisions with stable filters, pagination, conflicts, and match reasons. match_mode defaults to ranked (any token, ordered by BM25 and query token coverage); pass exact for the strict all-tokens lookup.",
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
            "List untrusted automatic Candidate Reviews for the exact ActiveTask; Pending is the default lifecycle filter. detail_level defaults to compact, which returns one triage row per Candidate; pass full for the whole untrusted drafts, or read one with candidate_get.",
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
        ),
        tool_schema(
            "space_create",
            "Create one explicitly named ContextSpace from a complete Space Intent. Use this once when a new requirement has no owning Space yet; the Space is never provisional because a human or Agent named it on purpose. Same validation as the operator CLI `sctx space create`.",
            space_create_schema()
        )
    ]})
}

fn space_create_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "intent"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "intent": {
                "type": "object",
                "additionalProperties": false,
                "description": "Complete Space Intent; out_of_scope and domain_terms are the only optional lists.",
                "required": [
                    "title", "problem", "desired_outcome", "in_scope", "acceptance_conditions"
                ],
                "properties": {
                    "title": {"type": "string", "minLength": 1},
                    "problem": {"type": "string", "minLength": 1},
                    "desired_outcome": {"type": "string", "minLength": 1},
                    "in_scope": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"type": "string", "minLength": 1}
                    },
                    "acceptance_conditions": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"type": "string", "minLength": 1}
                    },
                    "out_of_scope": {"type": "array", "items": {"type": "string", "minLength": 1}},
                    "domain_terms": {"type": "array", "items": {"type": "string", "minLength": 1}}
                }
            }
        }
    })
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
            "max_spaces": {"type": "integer", "minimum": 1, "maximum": MAX_TASK_MAX_SPACES, "default": DEFAULT_TASK_MAX_SPACES},
            "detail_level": detail_level_schema()
        }
    })
}

fn task_artifact_focus_coordinates_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["locator_kind"],
        "description": "Kind-specific Artifact coordinates. Send locator_kind plus exactly the fields that kind requires and no others: file and module require nothing else; api requires protocol, operation and normalized_route; schema requires namespace, version and qualified_name; symbol requires language, module, enclosing_type (string or null), symbol_name and signature; test requires qualified_test_name. The server names the missing or foreign coordinate field.",
        "properties": {
            "locator_kind": {
                "type": "string",
                "enum": ["file", "module", "api", "schema", "symbol", "test"]
            },
            "protocol": {"type": "string", "minLength": 1},
            "operation": {"type": "string", "minLength": 1},
            "normalized_route": {"type": "string", "minLength": 1},
            "namespace": {"type": "string", "minLength": 1},
            "version": {"type": "string", "minLength": 1},
            "qualified_name": {"type": "string", "minLength": 1},
            "language": {"type": "string", "minLength": 1},
            "module": {"type": "string", "minLength": 1},
            "enclosing_type": {"type": ["string", "null"], "minLength": 1},
            "symbol_name": {"type": "string", "minLength": 1},
            "signature": {"type": "string", "minLength": 1},
            "qualified_test_name": {"type": "string", "minLength": 1}
        }
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
            "artifact_kind": enum_schema([
                ArtifactKind::Module, ArtifactKind::File, ArtifactKind::Symbol,
                ArtifactKind::Api, ArtifactKind::Schema, ArtifactKind::Test
            ]),
            "relation": enum_schema([
                ReferenceRelation::Implements, ReferenceRelation::Defines,
                ReferenceRelation::Consumes, ReferenceRelation::Validates,
                ReferenceRelation::Constrains, ReferenceRelation::DependsOn
            ]),
            "locator": artifact_locator_input_schema(),
            "supports": {"type": "string", "minLength": 1},
            "limitations": {"type": "array", "minItems": 1, "items": {"type": "string", "minLength": 1}}
        }
    })
}

fn artifact_locator_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["locator_kind", "path"],
        "description": "Kind-specific Artifact coordinates plus the repository-relative path. Send locator_kind and path plus exactly the fields that kind requires and no others: file and module require nothing else; api requires protocol, operation and normalized_route; schema requires namespace, version and qualified_name; symbol requires language, module, enclosing_type (string or null), symbol_name and signature; test requires qualified_test_name. The server names the missing or foreign coordinate field.",
        "properties": {
            "locator_kind": {
                "type": "string",
                "enum": ["file", "module", "api", "schema", "symbol", "test"]
            },
            "path": {"type": "string", "minLength": 1},
            "protocol": {"type": "string", "minLength": 1},
            "operation": {"type": "string", "minLength": 1},
            "normalized_route": {"type": "string", "minLength": 1},
            "namespace": {"type": "string", "minLength": 1},
            "version": {"type": "string", "minLength": 1},
            "qualified_name": {"type": "string", "minLength": 1},
            "language": {"type": "string", "minLength": 1},
            "module": {"type": "string", "minLength": 1},
            "enclosing_type": {"type": ["string", "null"], "minLength": 1},
            "symbol_name": {"type": "string", "minLength": 1},
            "signature": {"type": "string", "minLength": 1},
            "qualified_test_name": {"type": "string", "minLength": 1}
        }
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
            "task_boundary": enum_schema([TaskBoundary::Continue, TaskBoundary::New]),
            "expected_revision_id": {
                "type": ["string", "null"],
                "pattern": "^tir_[0-9a-fA-F-]+$",
                "description": "The last returned intent_revision_id, or null only for the first new Task in a new external Session."
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
            },
            "detail_level": detail_level_schema()
        }
    })
}

/// Optional Task Context payload shape shared by every retrieval tool.
fn detail_level_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["compact", "full"],
        "default": "compact",
        "description": "compact returns only the inheritable fact fields; full returns the explainable payload with retrieval paths and match reasons."
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

fn task_checkpoint_schema() -> Value {
    let string_list = || {
        json!({
            "type": "array", "maxItems": MAX_TASK_CHECKPOINT_LIST_ITEMS,
            "items": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            }
        })
    };
    let evidence = json!({
        "type": "object", "additionalProperties": false,
        "required": ["evidence_type", "summary", "limitations"],
        "properties": {
            "evidence_type": enum_schema([
                EvidenceType::SourceSnapshot, EvidenceType::ExperimentRecord,
                EvidenceType::ArtifactSnapshot
            ]),
            "summary": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            },
            "limitations": string_list()
        }
    });
    let claim = json!({
        "type": "object", "additionalProperties": false,
        "required": ["context_kind", "statement", "rationale", "conditions", "evidence"],
        "properties": {
            "context_kind": kind_schema(),
            "statement": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            },
            "rationale": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            },
            "conditions": string_list(),
            "evidence": {
                "type": "array", "minItems": 1,
                "maxItems": MAX_TASK_CHECKPOINT_EVIDENCE_PER_CLAIM, "items": evidence
            },
        }
    });
    let unknown = json!({
        "type": "object", "additionalProperties": false,
        "required": ["statement", "blocking"],
        "properties": {
            "statement": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            },
            "blocking": {"type": "boolean"}
        }
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "claims", "unknowns"],
        "properties": {
            "agent_kind": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            },
            "external_session_id": {
                "type": "string", "minLength": 1, "maxLength": MAX_TASK_CHECKPOINT_TEXT_BYTES
            },
            "claims": {
                "type": "array", "maxItems": MAX_TASK_CHECKPOINT_CLAIMS, "items": claim
            },
            "unknowns": {
                "type": "array", "maxItems": MAX_TASK_CHECKPOINT_UNKNOWNS, "items": unknown
            }
        }
    })
}

#[allow(clippy::needless_pass_by_value)]
/// The one sentence appended to every public tool description.
///
/// `external_session_id` is the only argument no Model can derive: it is the host
/// Session id the Hook stated in the activation marker. Real sessions showed models
/// inventing it from a documentation example, and a tool description is the one text a
/// host always renders next to the call it is about — so the rule is stated there as
/// well as in the marker and the Skill gate.
const EXTERNAL_SESSION_ID_DESCRIPTION: &str = "external_session_id: copy verbatim from the `<shared-context-active>` marker (Codex: also $CODEX_SESSION_ID; Cursor: the conversation id); never invent or derive one.";

/// Declares one public tool, appending the `external_session_id` provenance sentence to
/// every tool that takes one. Appending it here instead of in each literal is what keeps
/// a newly added tool from silently shipping without it. The schema itself is untouched.
fn tool_schema(name: &str, description: &str, input_schema: Value) -> Value {
    let requires_external_session_id = input_schema["required"]
        .as_array()
        .is_some_and(|required| required.iter().any(|field| field == "external_session_id"));
    let description = if requires_external_session_id {
        format!("{description} {EXTERNAL_SESSION_ID_DESCRIPTION}")
    } else {
        description.to_owned()
    };
    let mut tool = Map::new();
    tool.insert("name".to_owned(), Value::String(name.to_owned()));
    tool.insert("description".to_owned(), Value::String(description));
    tool.insert("inputSchema".to_owned(), input_schema);
    Value::Object(tool)
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
            "cursor": {"type": "string"},
            "match_mode": {
                "type": "string",
                "enum": ["ranked", "exact"],
                "default": "ranked",
                "description": "ranked matches any query token and orders by BM25 combined with query token coverage; exact requires every token."
            }
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
            },
            "detail_level": detail_level_schema()
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
            "expected_intent_revision_id", "expected_review_version", "reason"
        ],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "expected_task_id": id_schema("tsk_"),
            "expected_intent_revision_id": id_schema("tir_"),
            "candidate_id": {
                "type": "string",
                "pattern": "^cnd_[0-9a-fA-F-]+$",
                "description": "Send exactly one of candidate_id or candidate_ids; sending both or neither is rejected."
            },
            "candidate_ids": {
                "type": "array", "uniqueItems": true,
                "minItems": 1, "maxItems": MAX_CANDIDATE_BATCH_ITEMS,
                "items": id_schema("cnd_"),
                "description": "Send exactly one of candidate_id or candidate_ids; the batch form discards every listed Candidate atomically under the same Review version and reason."
            },
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
            "expected_intent_revision_id", "expected_review_version",
            "primary", "related_space_ids"
        ],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "expected_task_id": id_schema("tsk_"),
            "expected_intent_revision_id": id_schema("tir_"),
            "candidate_id": {
                "type": "string",
                "pattern": "^cnd_[0-9a-fA-F-]+$",
                "description": "Send exactly one of candidate_id or candidate_ids; sending both or neither is rejected."
            },
            "candidate_ids": {
                "type": "array", "uniqueItems": true,
                "minItems": 1, "maxItems": MAX_CANDIDATE_BATCH_ITEMS,
                "items": id_schema("cnd_"),
                "description": "Send exactly one of candidate_id or candidate_ids; the batch form shares one Space organization and rejects edits, so per-Candidate edits stay single-Candidate calls."
            },
            "expected_review_version": {"type": "integer", "minimum": 1},
            "primary": {
                "type": "object",
                "additionalProperties": false,
                "description": "Send exactly one of existing_space_id or new_space_recommendation_id; sending both or neither is rejected, and a proposed new Space is single-Candidate only.",
                "properties": {
                    "existing_space_id": id_schema("spc_"),
                    "new_space_recommendation_id": id_schema("rec_")
                }
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
            "topic_key": nullable_field_edit_schema("topic key"),
            "problem_view": nullable_field_edit_schema("problem view"),
            "statement": {"type": "string", "minLength": 1},
            "rationale": {"type": "string", "minLength": 1},
            "hints": string_array_schema(),
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
            "relations": {"type": "array", "items": context_relation_schema()},
            "evidence": {
                "type": "array", "minItems": 1,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["kind", "supports", "content", "interpretation", "limitations"],
                    "properties": {
                        "kind": enum_schema([
                            EvidenceType::SourceSnapshot, EvidenceType::ExperimentRecord,
                            EvidenceType::ArtifactSnapshot
                        ]),
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
    enum_schema([
        ContextKind::Decision,
        ContextKind::Contract,
        ContextKind::Issue,
        ContextKind::Risk,
        ContextKind::Validation,
        ContextKind::Discovery,
        ContextKind::Progress,
    ])
}

fn context_relation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["target_context_id", "kind", "rationale", "supports"],
        "properties": {
            "target_context_id": id_schema("ctx_"),
            "kind": enum_schema([
                sctx_domain::ContextRelationKind::DependsOn,
                sctx_domain::ContextRelationKind::Constrains,
                sctx_domain::ContextRelationKind::Implements,
                sctx_domain::ContextRelationKind::ValidatedBy,
                sctx_domain::ContextRelationKind::Contradicts,
                sctx_domain::ContextRelationKind::Supersedes,
                sctx_domain::ContextRelationKind::RelatedTo,
            ]),
            "rationale": {"type": "string", "minLength": 1},
            "supports": {
                "type": "array", "minItems": 1,
                "items": {"type": "string", "minLength": 1}
            }
        }
    })
}

fn kind_array_schema() -> Value {
    json!({"type": "array", "items": kind_schema()})
}

/// Flat replacement declaration for one nullable Candidate field.
///
/// The host view lists both `action` values and the optional `value`; the exclusive
/// `set` requires `value` / `clear` forbids `value` composition stays authoritative Rust
/// validation, because a host union declaration degrades into an untyped map.
fn nullable_field_edit_schema(field: &str) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action"],
        "description": format!(
            "Replace the {field}: action set requires value; action clear removes it and must omit value."
        ),
        "properties": {
            "action": {"type": "string", "enum": ["set", "clear"]},
            "value": {"type": "string", "minLength": 1}
        }
    })
}

fn string_array_schema() -> Value {
    json!({"type": "array", "items": {"type": "string", "minLength": 1}})
}

fn enum_schema<T, const N: usize>(values: [T; N]) -> Value
where
    T: Serialize,
{
    let values = values.into_iter().collect::<Vec<_>>();
    json!({"type": "string", "enum": values})
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
            "kind": failure
                .family
                .unwrap_or_else(|| error_code(failure.error.kind())),
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

fn estimate_candidate_review_tokens(summary: &impl Serialize) -> Result<usize> {
    let bytes = serde_json::to_vec(summary)
        .map_err(|error| Error::new(ErrorKind::Io, format!("serialize Review summary: {error}")))?;
    Ok(bytes.len().div_ceil(4).max(1))
}

fn validate_task_checkpoint_input(
    input: &TaskCheckpointInput,
    serialized_bytes: usize,
) -> Result<()> {
    let mut violations = Vec::new();
    if serialized_bytes > MAX_TASK_CHECKPOINT_BYTES {
        violations.push(format!(
            "serialized payload exceeds {MAX_TASK_CHECKPOINT_BYTES} bytes"
        ));
    }
    validate_checkpoint_text(&input.agent_kind, "agent_kind", &mut violations);
    validate_checkpoint_text(
        &input.external_session_id,
        "external_session_id",
        &mut violations,
    );
    if input.claims.len() > MAX_TASK_CHECKPOINT_CLAIMS {
        violations.push(format!("claims exceeds {MAX_TASK_CHECKPOINT_CLAIMS} items"));
    }
    if input.unknowns.len() > MAX_TASK_CHECKPOINT_UNKNOWNS {
        violations.push(format!(
            "unknowns exceeds {MAX_TASK_CHECKPOINT_UNKNOWNS} items"
        ));
    }
    for (claim_index, claim) in input.claims.iter().enumerate() {
        validate_checkpoint_text(
            &claim.statement,
            &format!("claims[{claim_index}].statement"),
            &mut violations,
        );
        validate_checkpoint_text(
            &claim.rationale,
            &format!("claims[{claim_index}].rationale"),
            &mut violations,
        );
        validate_checkpoint_string_list(
            &claim.conditions,
            &format!("claims[{claim_index}].conditions"),
            &mut violations,
        );
        if claim.evidence.is_empty() {
            violations.push(format!("claims[{claim_index}].evidence must not be empty"));
        } else if claim.evidence.len() > MAX_TASK_CHECKPOINT_EVIDENCE_PER_CLAIM {
            violations.push(format!(
                "claims[{claim_index}].evidence exceeds {MAX_TASK_CHECKPOINT_EVIDENCE_PER_CLAIM} items"
            ));
        }
        for (evidence_index, evidence) in claim.evidence.iter().enumerate() {
            validate_checkpoint_text(
                &evidence.summary,
                &format!("claims[{claim_index}].evidence[{evidence_index}].summary"),
                &mut violations,
            );
            validate_checkpoint_string_list(
                &evidence.limitations,
                &format!("claims[{claim_index}].evidence[{evidence_index}].limitations"),
                &mut violations,
            );
        }
    }
    for (unknown_index, unknown) in input.unknowns.iter().enumerate() {
        validate_checkpoint_text(
            &unknown.statement,
            &format!("unknowns[{unknown_index}].statement"),
            &mut violations,
        );
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "task_checkpoint validation failed: {}",
            violations.join("; ")
        )))
    }
}

fn validate_checkpoint_string_list(values: &[String], field: &str, violations: &mut Vec<String>) {
    if values.len() > MAX_TASK_CHECKPOINT_LIST_ITEMS {
        violations.push(format!(
            "{field} exceeds {MAX_TASK_CHECKPOINT_LIST_ITEMS} items"
        ));
    }
    for (index, value) in values.iter().enumerate() {
        validate_checkpoint_text(value, &format!("{field}[{index}]"), violations);
    }
}

fn validate_checkpoint_text(value: &str, field: &str, violations: &mut Vec<String>) {
    if value.trim().is_empty() {
        violations.push(format!("{field} must not be empty"));
    } else if value.len() > MAX_TASK_CHECKPOINT_TEXT_BYTES {
        violations.push(format!(
            "{field} exceeds {MAX_TASK_CHECKPOINT_TEXT_BYTES} bytes"
        ));
    }
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

/// Turns every `contradicts` Relation on the confirming revision into a `SemanticConflictOpened`
/// reservation, so confirming a contradiction is the same batch as declaring it.
///
/// A target that is not currently accepted has no publication head to name as the other side, so
/// it is skipped instead of failing the Confirmation: the Relation itself is still recorded.
/// A pair already covered by an open conflict is skipped too, which is what makes a same-content
/// re-confirmation (a duplicate Candidate carrying the identical draft) idempotent rather than
/// conflict-spamming.
fn contradiction_conflict_openings(
    snapshot: &DomainSnapshot,
    final_draft: &ContextRevisionDraft,
    primary_space_id: Option<SpaceId>,
    final_content_hash: &str,
) -> Vec<SemanticConflictOpeningDraft> {
    let mut openings = Vec::new();
    for relation in final_draft
        .relations
        .iter()
        .filter(|relation| relation.kind == ContextRelationKind::Contradicts)
    {
        let Some(target) =
            projectable_conflict_participant(snapshot, final_draft, primary_space_id, relation)
        else {
            continue;
        };
        if conflict_already_open(snapshot, relation.target_context_id, final_content_hash) {
            continue;
        }
        openings.push(SemanticConflictOpeningDraft {
            target,
            reason: relation.rationale.clone(),
        });
    }
    openings
}

/// The other conflict side, but only when the reducer would admit the resulting conflict.
///
/// A `SemanticConflictOpened` Event is only projected when both sides are accepted publish heads
/// in one Space, both are `decision` or `contract`, both carry the same `topic_key`, and their
/// applicabilities overlap. Emitting one that fails those rules would write a permanently
/// diagnostic Event, so a contradiction that cannot be projected records only its Relation.
fn projectable_conflict_participant(
    snapshot: &DomainSnapshot,
    final_draft: &ContextRevisionDraft,
    primary_space_id: Option<SpaceId>,
    relation: &ContextRelation,
) -> Option<ConflictParticipant> {
    if !conflictable_kind(final_draft.kind) {
        return None;
    }
    let (space_id, context) = find_context(snapshot, None, relation.target_context_id).ok()?;
    if primary_space_id != Some(space_id) {
        return None;
    }
    let ContextGovernanceStatus::Accepted {
        publication_id,
        revision_id,
    } = context.governance
    else {
        return None;
    };
    let target = &context.revisions.get(&revision_id)?.revision;
    if !conflictable_kind(target.kind)
        || target.topic_key != final_draft.topic_key
        || !applicability_overlaps(&final_draft.applicability, &target.applicability)
    {
        return None;
    }
    Some(ConflictParticipant {
        context_id: relation.target_context_id,
        revision_id,
        publication_id,
    })
}

/// Only settled `decision` and `contract` statements can semantically conflict.
const fn conflictable_kind(kind: ContextKind) -> bool {
    matches!(kind, ContextKind::Decision | ContextKind::Contract)
}

/// Mirrors the reducer's V1 rule: an omitted dimension is unrestricted, otherwise an exact match.
fn applicability_overlaps(left: &Applicability, right: &Applicability) -> bool {
    let dimension = |left: &[String], right: &[String]| {
        left.is_empty() || right.is_empty() || left.iter().any(|value| right.contains(value))
    };
    dimension(&left.domains, &right.domains)
        && dimension(&left.platforms, &right.platforms)
        && dimension(&left.conditions, &right.conditions)
}

/// Whether an unresolved conflict already pairs `target_context_id` with this exact content.
fn conflict_already_open(
    snapshot: &DomainSnapshot,
    target_context_id: ContextId,
    final_content_hash: &str,
) -> bool {
    snapshot
        .projection
        .semantic_conflicts
        .values()
        .filter(|projection| matches!(projection.status, SemanticConflictStatus::Open { .. }))
        .any(|projection| {
            projection
                .conflict
                .participants
                .iter()
                .any(|participant| participant.context_id == target_context_id)
                && projection.conflict.participants.iter().any(|participant| {
                    accepted_content_hash(snapshot, participant.context_id).as_deref()
                        == Some(final_content_hash)
                })
        })
}

/// Content hash of one Context's currently accepted revision, excluding generated identities.
fn accepted_content_hash(snapshot: &DomainSnapshot, context_id: ContextId) -> Option<String> {
    let (_, context) = find_context(snapshot, None, context_id).ok()?;
    let ContextGovernanceStatus::Accepted { revision_id, .. } = context.governance else {
        return None;
    };
    context.revisions.get(&revision_id).map(|projection| {
        context_revision_content_hash(&context_revision_as_draft(&projection.revision))
    })
}

fn validate_context_relation_targets(
    snapshot: &DomainSnapshot,
    relations: &[ContextRelation],
) -> Result<()> {
    let mut identities = BTreeSet::new();
    for relation in relations {
        relation.validate()?;
        if !identities.insert((relation.target_context_id, relation.kind)) {
            return Err(invalid("Context Relation target/kind pairs must be unique"));
        }
        find_context(snapshot, None, relation.target_context_id)
            .map_err(|_| invalid("Context Relation target Context does not exist"))?;
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
    fn language_hint_only_flags_statements_without_any_chinese() {
        assert_eq!(
            language_hint("The reviewed branch preserves runtime service resolution"),
            Some(CHINESE_KNOWLEDGE_BASE_HINT.to_owned())
        );
        assert_eq!(
            language_hint("PoiEntranceAssem.kt:202 的 null 保护发生得过晚"),
            None,
            "a Chinese statement keeps its Latin identifiers and needs no advisory"
        );
        assert_eq!(language_hint("仅中文"), None);
    }

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

    #[test]
    fn full_review_rows_mark_every_recommended_space_as_provisional_or_not() {
        let provisional_space = SpaceId::new();
        let named_space = SpaceId::new();
        let mut row = json!({
            "space_recommendations": [
                {"kind": "existing", "space_id": provisional_space.to_string()},
                {"kind": "existing", "space_id": named_space.to_string()},
                {"kind": "proposed_new_space_intent", "recommendation_id": "srx"},
            ]
        });
        insert_space_recommendation_provisional(&mut row, &BTreeSet::from([provisional_space]));
        let rows = row["space_recommendations"].as_array().unwrap();
        assert_eq!(rows[0]["provisional"], Value::Bool(true));
        assert_eq!(rows[1]["provisional"], Value::Bool(false));
        // Confirming a proposed recommendation is what creates the Space, so it is always
        // provisional and needs no lookup.
        assert_eq!(rows[2]["provisional"], Value::Bool(true));
    }

    /// Builds one provisional Space by patching the serialized `space.created` event: only
    /// Candidate Confirmation writes the flag, and this test is about how the flag is read.
    fn provisional_space_event(title: &str) -> Event {
        let human = Event::space_created(advisory_intent(title), None).expect("valid Intent");
        let mut json = serde_json::to_value(&human).expect("event serializes");
        json["intent_revision"]["provisional"] = Value::Bool(true);
        let bytes = serde_json::to_vec(&json).expect("event serializes");
        sctx_event_schema::parse_event(&bytes)
            .expect("patched event stays a valid V1 event")
            .known()
            .expect("patched event stays a known V1 event")
            .clone()
    }

    fn advisory_intent(title: &str) -> sctx_domain::IntentSnapshot {
        sctx_domain::IntentSnapshot {
            title: title.to_owned(),
            problem: "provisional Spaces accumulate without a human boundary".to_owned(),
            desired_outcome: "a reviewer names or merges the Space".to_owned(),
            in_scope: vec!["provisional Space advisories".to_owned()],
            out_of_scope: vec!["automatic merging".to_owned()],
            acceptance_conditions: vec!["the advisory only nudges".to_owned()],
            domain_terms: vec!["space".to_owned()],
        }
    }

    fn advisory_context(statement: &str, relations: Vec<ContextRelation>) -> ContextRevisionDraft {
        ContextRevisionDraft {
            kind: ContextKind::Discovery,
            topic_key: None,
            problem_view: None,
            statement: statement.to_owned(),
            rationale: "recorded by the advisory fixture".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            hints: Vec::new(),
            relations,
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: statement.to_owned(),
                content: json!({"command": "fixture", "actual": "recorded"}),
                interpretation: "fixture observation".to_owned(),
                limitations: vec!["fixture".to_owned()],
            }],
        }
    }

    /// Appends one accepted Context and returns its `ContextId`.
    fn accept_context(
        store: &GitStore,
        space_id: SpaceId,
        statement: &str,
        relations: Vec<ContextRelation>,
    ) -> ContextId {
        let event =
            Event::context_revision_added(space_id, advisory_context(statement, relations), None)
                .expect("valid Context revision");
        let (context_id, revision_id) = match event.payload() {
            EventPayload::ContextRevisionAdded {
                context_id,
                revision,
                ..
            } => (*context_id, revision.revision_id),
            _ => unreachable!("context.revision_added"),
        };
        store
            .append_event(AppendRequest::event(event))
            .expect("append Context revision");
        let publication = Event::publication_changed(
            space_id,
            context_id,
            sctx_domain::PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: sctx_domain::PublicationAction::Publish,
                revision_id,
                review_event_ids: Vec::new(),
            },
            None,
        )
        .expect("valid publication");
        store
            .append_event(AppendRequest::event(publication))
            .expect("append publication");
        context_id
    }

    fn space_id_of(event: &Event) -> SpaceId {
        match event.payload() {
            EventPayload::SpaceCreated { space_id, .. } => *space_id,
            _ => unreachable!("space.created"),
        }
    }

    #[test]
    fn only_provisional_spaces_past_the_threshold_or_referenced_get_an_advisory() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let store =
            GitStore::bootstrap_local(temporary.path().join("installation")).expect("bootstrap");
        let index = ProjectionIndex::for_store(&store);

        let grown = provisional_space_event("Grown provisional Space");
        let grown_id = space_id_of(&grown);
        store
            .append_event(AppendRequest::event(grown))
            .expect("append provisional Space");
        let small = provisional_space_event("Small provisional Space");
        let small_id = space_id_of(&small);
        store
            .append_event(AppendRequest::event(small))
            .expect("append provisional Space");
        let human =
            Event::space_created(advisory_intent("Human named Space"), None).expect("valid Intent");
        let human_id = space_id_of(&human);
        store
            .append_event(AppendRequest::event(human))
            .expect("append human Space");

        for index_of in 0..PROVISIONAL_SPACE_MERGE_THRESHOLD {
            accept_context(
                &store,
                grown_id,
                &format!("Grown provisional fact {index_of}"),
                Vec::new(),
            );
        }
        let small_context = accept_context(
            &store,
            small_id,
            "The only fact in the small provisional Space",
            Vec::new(),
        );

        index.synchronize().expect("synchronize");
        let snapshot = index.domain_snapshot().expect("snapshot");
        // The small Space is provisional but has one accepted Context and no inbound relation.
        let advisories = provisional_space_advisories(&snapshot);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].space_id, grown_id);
        assert_eq!(advisories[0].title, "Grown provisional Space");
        assert_eq!(
            advisories[0].reason,
            format!(
                "Provisional Space has {PROVISIONAL_SPACE_MERGE_THRESHOLD} accepted Contexts; consider `sctx space intent revise` to name it or merge into a human-defined Space"
            )
        );
        assert_eq!(
            provisional_space_ids(&snapshot),
            BTreeSet::from([grown_id, small_id])
        );

        // A `related_to` edge from a human-defined Space is the second, independent trigger.
        accept_context(
            &store,
            human_id,
            "The human Space already reads the provisional knowledge",
            vec![ContextRelation {
                target_context_id: small_context,
                kind: ContextRelationKind::RelatedTo,
                rationale: "the named boundary already depends on this fact".to_owned(),
                supports: vec!["fixture relation".to_owned()],
            }],
        );
        index.synchronize().expect("synchronize");
        let snapshot = index.domain_snapshot().expect("snapshot");
        let advisories = provisional_space_advisories(&snapshot);
        assert_eq!(advisories.len(), 2);
        let small_advisory = advisories
            .iter()
            .find(|advisory| advisory.space_id == small_id)
            .expect("the referenced provisional Space is advised");
        assert_eq!(
            small_advisory.reason,
            "Provisional Space has 1 accepted Contexts and 1 related Context relations from other Spaces; consider `sctx space intent revise` to name it or merge into a human-defined Space"
        );
        // Nothing was merged or renamed: both Spaces still exist with their proposed titles.
        assert_eq!(snapshot.projection.spaces.len(), 3);
    }
}
