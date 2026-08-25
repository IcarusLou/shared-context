//! Structured FTS5 search, Task-to-Space association, and deterministic Context Packs.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    str::FromStr,
};

use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use sctx_domain::{
    Applicability, ArtifactAssociationKind, ArtifactKey, ArtifactKind, ContextId, ContextKind,
    ContextRelationKind, EvidenceId, EvidenceType, ReferenceId, RepositoryId, ResolutionStatus,
    ResolvedFocus, RevisionId, SpaceId, TaskId, TaskSignal, TaskSignalKind, TaskSpaceAssociation,
    WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    EngineeringProjection, EngineeringProjectionSnapshot, EngineeringProjectionStore,
    GraphContextSafety, GraphContextSnapshot, GraphContextStatus, MatchBasis,
};
use sctx_index::{IndexMetadata, ProjectionIndex, normalize_search_text, search_tokens};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

mod candidate;

pub use candidate::{
    CandidateAnalysisRequest, CandidateAnalysisResult, MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET,
    MAX_CANDIDATE_ANALYSIS_TOP_K, MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET,
};

pub use sctx_domain::{Error, ErrorKind, Result};

const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 200;
const DEFAULT_CANDIDATE_LIMIT: usize = 100;
pub const DEFAULT_TASK_MAX_SPACES: usize = 8;
pub const MAX_TASK_MAX_SPACES: usize = 32;
pub const MIN_TASK_CONTEXT_TOKEN_BUDGET: usize = 256;

/// Structured applicability filter. Each populated dimension is required; values within one
/// dimension are alternatives.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScopeFilter {
    pub domains: Vec<String>,
    pub platforms: Vec<String>,
    pub conditions: Vec<String>,
}

/// Search-visible lifecycle/governance state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextStatus {
    Candidate,
    Accepted,
    Deprecated,
    Superseded,
    GovernanceConflict,
}

impl ContextStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Accepted => "accepted",
            Self::Deprecated => "deprecated",
            Self::Superseded => "superseded",
            Self::GovernanceConflict => "governance_conflict",
        }
    }
}

/// Hard filters applied before ranking.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchFilters {
    pub space_ids: Vec<SpaceId>,
    pub scope: ScopeFilter,
    pub kinds: Vec<ContextKind>,
    pub statuses: Vec<ContextStatus>,
}

/// One stable-cursor search request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub filters: SearchFilters,
    pub page_size: usize,
    pub cursor: Option<String>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            filters: SearchFilters::default(),
            page_size: DEFAULT_PAGE_SIZE,
            cursor: None,
        }
    }
}

/// FTS field responsible for a match.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchField {
    Title,
    Statement,
    Rationale,
    Evidence,
}

/// Explainable ranking inputs returned with every hit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchReason {
    pub matched_fields: Vec<MatchField>,
    pub matched_tokens: Vec<String>,
    pub bm25: f64,
    pub evidence_completeness: u16,
    pub structured_filter_match: bool,
}

/// Self-contained Evidence attached to one search result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceView {
    pub evidence_id: EvidenceId,
    pub kind: String,
    pub supports: String,
    pub content: Value,
    pub interpretation: String,
    pub limitations: Vec<String>,
}

/// One side of a governance or confirmed semantic conflict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictSide {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub publication_id: String,
}

/// Conflict details are expanded instead of silently selecting one participant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictView {
    pub conflict_id: String,
    pub kind: String,
    pub status: String,
    pub reason: String,
    pub participants: Vec<ConflictSide>,
}

/// Search hit for one immutable revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub space_id: SpaceId,
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub title: String,
    pub kind: ContextKind,
    pub status: ContextStatus,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    pub evidence: Vec<EvidenceView>,
    pub conflicts: Vec<ConflictView>,
    pub auto_injection_eligible: bool,
    pub match_reason: MatchReason,
}

/// Why matching rows were not present in this response page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchOmitted {
    pub count: usize,
    pub reason: String,
}

/// Stable page plus the exact Tree/Generation read in its `QuerySnapshot` transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub results: Vec<SearchResult>,
    pub next_cursor: Option<String>,
    pub omitted: Vec<SearchOmitted>,
}

/// Searchable field in one current `ContextSpace` Intent head.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceIntentField {
    Title,
    Problem,
    DesiredOutcome,
    InScope,
    OutOfScope,
    AcceptanceConditions,
    DomainTerms,
}

/// Tokens matched in one exact Space Intent field. Field-level matches preserve polarity and
/// remain diagnostic even when a negative-only candidate is not promoted to an association.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpaceIntentFieldMatch {
    pub field: SpaceIntentField,
    pub matched_tokens: Vec<String>,
}

/// Explainable match against one current Intent head.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceIntentHeadMatch {
    pub revision_id: RevisionId,
    pub field_matches: Vec<SpaceIntentFieldMatch>,
    pub matched_fields: Vec<SpaceIntentField>,
    pub matched_tokens: Vec<String>,
    pub phrase_match: bool,
    pub bm25: f64,
}

/// One Task-derived Space candidate. Every current head identity is included, while
/// `matching_heads` contains every head that matched the Task query. This keeps a conflicted
/// Intent explicit even when only one side contains the matching terms.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceIntentCandidate {
    pub space_id: SpaceId,
    pub intent_conflicted: bool,
    pub head_revision_ids: Vec<RevisionId>,
    pub matching_heads: Vec<SpaceIntentHeadMatch>,
    pub field_matches: Vec<SpaceIntentFieldMatch>,
    pub matched_fields: Vec<SpaceIntentField>,
    pub matched_tokens: Vec<String>,
    pub phrase_match: bool,
    /// Best (lowest) FTS5 BM25 value among the matching current heads.
    pub bm25: f64,
}

/// Deterministically ranked Space Intent candidates from one exact projection snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceIntentCandidatesResponse {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub task_id: TaskId,
    pub candidates: Vec<SpaceIntentCandidate>,
}

/// Deterministically ranked multi-Space associations from one exact projection snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskSpaceAssociationsResponse {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub task_id: TaskId,
    pub associations: Vec<TaskSpaceAssociation>,
}

/// Task-first Context retrieval request. No Space identifier is required or accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskContextRequest {
    pub task_id: TaskId,
    pub working_intent: WorkingIntentSnapshot,
    pub task_signals: Vec<TaskSignal>,
    pub resolved_focus: Option<ResolvedFocus>,
    pub token_budget: usize,
    pub max_spaces: usize,
    pub candidate_limit: usize,
    pub mode: ContextPackMode,
}

impl TaskContextRequest {
    #[must_use]
    pub fn automatic(
        task_id: TaskId,
        working_intent: WorkingIntentSnapshot,
        task_signals: Vec<TaskSignal>,
        token_budget: usize,
    ) -> Self {
        Self {
            task_id,
            working_intent,
            task_signals,
            resolved_focus: None,
            token_budget,
            max_spaces: DEFAULT_TASK_MAX_SPACES,
            candidate_limit: DEFAULT_CANDIDATE_LIMIT,
            mode: ContextPackMode::AutomaticInjection,
        }
    }

    #[must_use]
    pub fn for_resolved_focus(
        task_id: TaskId,
        working_intent: WorkingIntentSnapshot,
        task_signals: Vec<TaskSignal>,
        resolved_focus: ResolvedFocus,
        token_budget: usize,
    ) -> Self {
        let mut request = Self::automatic(task_id, working_intent, task_signals, token_budget);
        request.resolved_focus = Some(resolved_focus);
        request
    }
}

/// Negative Intent field that conflicts with otherwise positive association evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentScopeConflictKind {
    ContextSpaceOutOfScope,
}

/// Deterministic policy applied when positive evidence also crosses a Space exclusion boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentScopeConflictPolicy {
    PenalizeAssociation,
}

/// Typed explanation embedded in one [`TaskSpaceAssociation`] reason. Search owns this structure
/// while the domain association retains its stable string-reason boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntentScopeConflictExplanation {
    pub kind: IntentScopeConflictKind,
    pub matched_tokens: Vec<String>,
    pub policy: IntentScopeConflictPolicy,
    pub score_multiplier_basis_points: u16,
}

/// Kind of unresolved Space Intent handoff exposed to the active Agent session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentConflictKind {
    ContextAndIntentAlternativesConflict,
}

/// Explicit statement that retrieval did not choose one Intent branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentConflictSelection {
    SystemHasNotSelectedWinner,
}

/// Evidence the session Agent must validate before relying on conflicted alternatives.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentConflictValidation {
    CurrentCode,
    Evidence,
    TaskApplicability,
}

/// Actor responsible for resolving applicability during the current task.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentConflictActor {
    SessionAgent,
}

/// Decision explicitly delegated to the active Agent session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentConflictDecision {
    DecideWhichContextIsMoreSuitable,
}

/// Typed warning handed to the session Agent without blocking otherwise safe Context retrieval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntentConflictHandoffExplanation {
    pub kind: IntentConflictKind,
    pub head_revision_ids: Vec<RevisionId>,
    pub selection: IntentConflictSelection,
    pub required_actor: IntentConflictActor,
    pub must_validate: Vec<IntentConflictValidation>,
    pub required_decision: IntentConflictDecision,
}

/// Deterministic candidate channel participating in Reciprocal Rank Fusion.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssociationChannel {
    ResolvedArtifactExact,
    ContextRelation,
    ArtifactHintSpaceIntentBm25,
    ArtifactHintAcceptedContextBm25,
    InterfaceHintSpaceIntentBm25,
    InterfaceHintAcceptedContextBm25,
    SpaceIntentBm25,
    AcceptedContextBm25,
    ExactScope,
}

/// Explainable features and rank contribution from one RRF channel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskAssociationChannelFeature {
    pub channel: TaskAssociationChannel,
    pub rank: usize,
    pub reciprocal_rank_micros: u32,
    pub bm25_micros: Option<i64>,
    pub query_token_coverage_basis_points: u16,
    pub idf_bm25_contribution_micros: u32,
    pub phrase_match: bool,
    pub field_weight_points: u16,
    pub exact_match_strength: u16,
}

/// Stable fusion algorithm identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssociationFusionAlgorithm {
    ReciprocalRankFusion,
}

/// Typed RRF explanation serialized into a [`TaskSpaceAssociation`] reason.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskAssociationFusionExplanation {
    pub algorithm: TaskAssociationFusionAlgorithm,
    pub rrf_k: u16,
    pub channels: Vec<TaskAssociationChannelFeature>,
    pub fused_score_basis_points: u16,
    pub minimum_score_basis_points: u16,
    pub final_score_basis_points: u16,
}

/// One exact active Artifact Focus to a historical Engineering Artifact association.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphArtifactRetrievalPath {
    pub resolved_focus: ResolvedFocus,
    pub repository_id: RepositoryId,
    pub artifact_key: ArtifactKey,
    pub artifact_kind: ArtifactKind,
    pub reference_id: ReferenceId,
    pub association_id: String,
    pub association_kind: ArtifactAssociationKind,
    pub match_basis: MatchBasis,
    pub resolution_status: ResolutionStatus,
    pub confidence_basis_points: u16,
    pub artifact_generation: String,
}

/// One stable, authoritative Context Relation hop from a retrieved Context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextRelationRetrievalPath {
    pub source_context_id: ContextId,
    pub source_revision_id: RevisionId,
    pub target_context_id: ContextId,
    pub target_revision_id: RevisionId,
    pub kind: ContextRelationKind,
    pub rationale: String,
    pub supports: Vec<String>,
    pub depth: u8,
}

/// Explicit-only diagnostic for an Artifact edge that was not safe to resolve.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphResolutionDiagnosticPath {
    pub resolved_focus: ResolvedFocus,
    pub repository_id: RepositoryId,
    pub reference_id: ReferenceId,
    pub resolution_status: ResolutionStatus,
    pub candidate_artifact_keys: Vec<ArtifactKey>,
    pub match_bases: Vec<MatchBasis>,
    pub artifact_generation: String,
}

/// Why this request's Resolved Focus produced no exact node in the selected Graph snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGraphDiagnosticKind {
    ArtifactNotReachableInGraph,
}

/// Budgeted Task-level Graph diagnostic that makes zero-result semantics explicit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskGraphDiagnostic {
    pub kind: TaskGraphDiagnosticKind,
    pub resolved_focus: ResolvedFocus,
}

/// Working Intent field that supplied one non-factual text-retrieval Hint.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingIntentHintField {
    ArtifactHints,
    InterfaceHints,
}

/// FTS projection matched by one Working Intent Hint without resolving an Artifact.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingIntentHintTarget {
    SpaceIntentFts,
    AcceptedContextFts,
}

/// Typed explanation for one positive, text-only Working Intent Hint retrieval path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkingIntentHintTextExplanation {
    pub source_field: WorkingIntentHintField,
    pub target: WorkingIntentHintTarget,
    pub matched_tokens: Vec<String>,
    pub phrase_match: bool,
    pub query_token_coverage_basis_points: u16,
    pub bm25_micros: i64,
    pub fusion_contribution_micros: u32,
}

/// Explainable route from the Task to one returned Context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum TaskRetrievalPath {
    EngineeringGraph {
        path: GraphArtifactRetrievalPath,
        relation_hops: Vec<ContextRelationRetrievalPath>,
    },
    ContextRelation {
        hops: Vec<ContextRelationRetrievalPath>,
    },
    GraphDiagnostic {
        diagnostic: GraphResolutionDiagnosticPath,
    },
    IntentFts {
        matched_fields: Vec<String>,
        matched_tokens: Vec<String>,
    },
    ContextFts {
        matched_fields: Vec<MatchField>,
        matched_tokens: Vec<String>,
    },
    WorkingIntentHintText {
        explanation: WorkingIntentHintTextExplanation,
    },
    ExactScope {
        dimension: String,
        value: String,
    },
}

/// One budgeted Context explicitly linked to its Task-to-Space association and retrieval paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContextItem {
    pub association_space_id: SpaceId,
    pub context: ContextPackItem,
    pub retrieval_paths: Vec<TaskRetrievalPath>,
}

/// Task-first Context Pack produced from the current Context projection and an optional,
/// independently built historical Engineering Graph snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContextPack {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub artifact_generation: Option<String>,
    pub graph_context_tree_oid: Option<String>,
    pub task_id: TaskId,
    pub task_fingerprint: String,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub mode: ContextPackMode,
    pub associations: Vec<TaskSpaceAssociation>,
    pub items: Vec<TaskContextItem>,
    pub graph_diagnostics: Vec<TaskGraphDiagnostic>,
    pub omitted: Vec<ContextPackOmitted>,
}

/// Several cursor pages materialized under one pinned `QuerySnapshot` transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchPagesResponse {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub pages: Vec<SearchPageResponse>,
    pub next_cursor: Option<String>,
}

/// Page body used by [`SearchPagesResponse`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchPageResponse {
    pub results: Vec<SearchResult>,
    pub omitted: Vec<SearchOmitted>,
}

/// Whether a Context Pack is an explicit lookup or privileged automatic injection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPackMode {
    Explicit,
    AutomaticInjection,
}

/// One budget omission, including enough identity to fetch the Context explicitly.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextPackOmitted {
    pub context_id: Option<ContextId>,
    pub revision_id: Option<RevisionId>,
    pub reason: String,
    pub estimated_tokens: usize,
    pub count: usize,
}

/// Detail tier selected by the budgeter. Summary items retain identity, status, statement, match
/// reason, and complete conflict sides while deferring Evidence and rationale expansion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPackDetail {
    Full,
    Summary,
}

/// Budgeted representation of a search result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextPackItem {
    pub space_id: SpaceId,
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub title: String,
    pub kind: ContextKind,
    pub status: ContextStatus,
    pub statement: String,
    pub rationale: Option<String>,
    pub applicability: Applicability,
    pub evidence: Vec<EvidenceView>,
    pub conflicts: Vec<ConflictView>,
    pub auto_injection_eligible: bool,
    pub safety_source: ContextSafetySource,
    pub match_reason: MatchReason,
    pub detail: ContextPackDetail,
}

/// Authoritative origin of the automatic-injection decision carried by one Context item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ContextSafetySource {
    CurrentProjection,
    EngineeringGraphSnapshot {
        context_tree_oid: Option<String>,
        artifact_generation: String,
        context_id: ContextId,
        revision_id: RevisionId,
        safety: GraphContextSafety,
    },
}

/// Query boundary that always delegates reads to one [`sctx_index::QuerySnapshot`] transaction.
#[derive(Clone, Debug)]
pub struct SearchEngine {
    index: ProjectionIndex,
    engineering_graph: Option<EngineeringProjectionStore>,
}

impl SearchEngine {
    #[must_use]
    pub const fn new(index: ProjectionIndex) -> Self {
        Self {
            index,
            engineering_graph: None,
        }
    }

    /// Adds the optional local Engineering Graph projection used for exact
    /// Artifact retrieval. The Graph is advisory and Task retrieval degrades
    /// to Context-only channels when it is missing, unpinned, or behind HEAD.
    #[must_use]
    pub const fn with_engineering_graph(
        index: ProjectionIndex,
        engineering_graph: EngineeringProjectionStore,
    ) -> Self {
        Self {
            index,
            engineering_graph: Some(engineering_graph),
        }
    }

    /// Finds zero or more current Space Intent candidates from a Working Intent and its observed
    /// signals. Every current head is searched independently; conflicts are returned rather than
    /// resolved by ranking.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid Working Intent or signal collection, and storage errors
    /// propagated by index synchronization and snapshot reads.
    pub fn space_intent_candidates(
        &self,
        task_id: TaskId,
        intent: &WorkingIntentSnapshot,
        signals: &[TaskSignal],
    ) -> Result<SpaceIntentCandidatesResponse> {
        intent.validate()?;
        TaskSignal::validate_collection(signals)?;
        let query_tokens = task_query_tokens(intent, signals);
        let query_phrases = task_query_phrases(intent, signals, true);
        let snapshot = self.index.query_snapshot(|connection| {
            query_space_intent_candidates(connection, &query_tokens, &query_phrases)
        })?;
        Ok(SpaceIntentCandidatesResponse {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            task_id,
            candidates: snapshot.data,
        })
    }

    /// Infers zero or more explainable Space associations for a Task. The inference boundary
    /// gives current, uniquely resolved Engineering Artifact associations and stable active
    /// Context Relations higher RRF weight than Intent/Context text and exact scope.
    ///
    /// Workspace signals never add a Space prior. Repository identity signals may expose an
    /// explicit unavailable-resolution diagnostic, but never add ranking or injection evidence.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid Working Intent or signal collection, and storage errors
    /// propagated by index synchronization and snapshot reads.
    pub fn task_space_associations(
        &self,
        task_id: TaskId,
        intent: &WorkingIntentSnapshot,
        signals: &[TaskSignal],
    ) -> Result<TaskSpaceAssociationsResponse> {
        self.task_space_associations_with_resolved_focus(task_id, intent, signals, None)
    }

    /// Infers associations with at most one request-local Resolved Focus as an
    /// Engineering Graph seed.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid request-local Resolved Focus.
    pub fn task_space_associations_with_resolved_focus(
        &self,
        task_id: TaskId,
        intent: &WorkingIntentSnapshot,
        signals: &[TaskSignal],
        resolved_focus: Option<&ResolvedFocus>,
    ) -> Result<TaskSpaceAssociationsResponse> {
        intent.validate()?;
        TaskSignal::validate_collection(signals)?;
        if let Some(focus) = resolved_focus {
            focus.validate()?;
        }
        let query_tokens = association_query_tokens(intent, signals);
        let query_phrases = association_query_phrases(intent, signals);
        let hint_queries = working_intent_hint_queries(intent);
        let scope_targets = ScopeTargets::from_intent(intent);
        for _attempt in 0..3 {
            let graph_snapshot = self.read_graph_snapshot();
            let snapshot = self.index.query_snapshot(|connection| {
                let graph = graph_projection(graph_snapshot.as_ref());
                infer_task_space_associations(
                    connection,
                    task_id,
                    &query_tokens,
                    &query_phrases,
                    &hint_queries,
                    &scope_targets,
                    resolved_focus,
                    graph,
                    graph_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.context_tree_oid.as_deref()),
                    ContextPackMode::AutomaticInjection,
                )
            })?;
            let used_graph = graph_projection(graph_snapshot.as_ref());
            if used_graph.is_some() && !self.graph_snapshot_unchanged(graph_snapshot.as_ref()) {
                continue;
            }
            return Ok(TaskSpaceAssociationsResponse {
                indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
                projection_generation: snapshot.metadata.projection_generation,
                task_id,
                associations: snapshot.data.associations,
            });
        }
        Err(invariant(
            "Engineering Graph generation changed during Task association retrieval",
        ))
    }

    /// Builds an explainable Task-first Context Pack from one Context `QuerySnapshot` and, when
    /// available, one generation-stable historical Engineering projection. Its Context Tree is
    /// provenance only; build-time immutable revisions and safety remain authoritative until an
    /// explicit Graph rebuild.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid Task, signals, budget, or candidate limit, and
    /// storage errors propagated by index synchronization and snapshot reads.
    pub fn task_context_pack(&self, request: &TaskContextRequest) -> Result<TaskContextPack> {
        validate_task_context_request(request)?;
        let fingerprint = task_fingerprint(&request.working_intent, &request.task_signals)?;
        let query_tokens = association_query_tokens(&request.working_intent, &request.task_signals);
        let query_phrases =
            association_query_phrases(&request.working_intent, &request.task_signals);
        let hint_queries = working_intent_hint_queries(&request.working_intent);
        let scope_targets = ScopeTargets::from_intent(&request.working_intent);
        for _attempt in 0..3 {
            let graph_snapshot = self.read_graph_snapshot();
            let snapshot = self.index.query_snapshot(|connection| {
                let graph = graph_projection(graph_snapshot.as_ref());
                let mut inference = infer_task_space_associations(
                    connection,
                    request.task_id,
                    &query_tokens,
                    &query_phrases,
                    &hint_queries,
                    &scope_targets,
                    request.resolved_focus.as_ref(),
                    graph,
                    graph_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.context_tree_oid.as_deref()),
                    request.mode,
                )?;
                let omitted_spaces = inference
                    .associations
                    .len()
                    .saturating_sub(request.max_spaces);
                let omitted_space_tokens = inference.associations
                    [request.max_spaces.min(inference.associations.len())..]
                    .iter()
                    .map(serialized_tokens)
                    .sum();
                inference.associations.truncate(request.max_spaces);
                let candidates = load_task_context_candidates(
                    connection,
                    &inference,
                    request.mode,
                    request.candidate_limit,
                )?;
                let graph_diagnostics = artifact_focus_diagnostics(
                    request.resolved_focus.as_ref(),
                    inference.focus_reachable,
                );
                Ok(pack_task_context_candidates(
                    candidates,
                    request.token_budget,
                    inference,
                    graph_diagnostics,
                    omitted_spaces,
                    omitted_space_tokens,
                ))
            })?;
            let used_graph = graph_projection(graph_snapshot.as_ref());
            if used_graph.is_some() && !self.graph_snapshot_unchanged(graph_snapshot.as_ref()) {
                continue;
            }
            return Ok(TaskContextPack {
                indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
                projection_generation: snapshot.metadata.projection_generation,
                artifact_generation: used_graph.map(|graph| graph.artifact_generation.clone()),
                graph_context_tree_oid: graph_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.context_tree_oid.clone()),
                task_id: request.task_id,
                task_fingerprint: fingerprint,
                token_budget: request.token_budget,
                estimated_tokens: snapshot.data.estimated_tokens,
                mode: request.mode,
                associations: snapshot.data.associations,
                items: snapshot.data.items,
                graph_diagnostics: snapshot.data.graph_diagnostics,
                omitted: snapshot.data.omitted,
            });
        }
        Err(invariant(
            "Engineering Graph generation changed during Task Context retrieval",
        ))
    }

    fn read_graph_snapshot(&self) -> Option<EngineeringProjectionSnapshot> {
        self.engineering_graph
            .as_ref()
            .and_then(|store| store.read_snapshot().ok().flatten())
    }

    fn graph_snapshot_unchanged(&self, before: Option<&EngineeringProjectionSnapshot>) -> bool {
        let after = self.read_graph_snapshot();
        graph_snapshot_identity(before) == graph_snapshot_identity(after.as_ref())
    }

    /// Searches and expands Evidence/conflicts within one read transaction.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid cursor/page size, or a storage error when the index
    /// cannot be synchronized or read.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        validate_search_request(request)?;
        let snapshot = self.index.query_snapshot(|connection| {
            let tree_oid = meta(connection, "indexed_tree_oid")?;
            search_in_snapshot(connection, request, &tree_oid, false)
        })?;
        Ok(response_from_page(snapshot.metadata, snapshot.data))
    }

    /// Reads up to `maximum_pages` cursor pages in the same pinned transaction.
    ///
    /// # Errors
    ///
    /// Returns an input error for a zero page count or invalid request, and storage errors from
    /// synchronization/query execution.
    pub fn search_pages(
        &self,
        request: &SearchRequest,
        maximum_pages: usize,
    ) -> Result<SearchPagesResponse> {
        validate_search_request(request)?;
        if maximum_pages == 0 {
            return Err(invalid("maximum_pages must be greater than zero"));
        }
        let snapshot = self.index.query_snapshot(|connection| {
            let tree_oid = meta(connection, "indexed_tree_oid")?;
            let mut request = request.clone();
            let mut pages = Vec::new();
            let mut next_cursor = None;
            for _ in 0..maximum_pages {
                let page = search_in_snapshot(connection, &request, &tree_oid, false)?;
                next_cursor.clone_from(&page.next_cursor);
                pages.push(SearchPageResponse {
                    results: page.results,
                    omitted: page.omitted,
                });
                let Some(cursor) = page.next_cursor else {
                    break;
                };
                request.cursor = Some(cursor);
            }
            Ok((pages, next_cursor))
        })?;
        Ok(SearchPagesResponse {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            pages: snapshot.data.0,
            next_cursor: snapshot.data.1,
        })
    }
}

fn graph_projection(
    snapshot: Option<&EngineeringProjectionSnapshot>,
) -> Option<&EngineeringProjection> {
    snapshot.map(|snapshot| &snapshot.projection)
}

fn graph_snapshot_identity(
    snapshot: Option<&EngineeringProjectionSnapshot>,
) -> Option<(&str, Option<&str>)> {
    snapshot.map(|snapshot| {
        (
            snapshot.projection.artifact_generation.as_str(),
            snapshot.context_tree_oid.as_deref(),
        )
    })
}

#[derive(Debug)]
struct RawIntentHeadMatch {
    space_id: SpaceId,
    intent_conflicted: bool,
    head_match: SpaceIntentHeadMatch,
}

struct StoredIntentFtsMatch {
    space_id: String,
    revision_id: String,
    intent_conflicted: bool,
    bm25: f64,
    fields: [String; 7],
}

fn task_query_tokens(intent: &WorkingIntentSnapshot, signals: &[TaskSignal]) -> Vec<String> {
    let list_text = [
        &intent.in_scope,
        &intent.out_of_scope,
        &intent.domains,
        &intent.platforms,
        &intent.constraints,
        &intent.acceptance_conditions,
        &intent.artifact_hints,
        &intent.interface_hints,
        &intent.open_questions,
    ]
    .into_iter()
    .flat_map(|values| values.iter().map(String::as_str));
    let signal_text = signals
        .iter()
        .filter(|signal| matches!(signal.kind, TaskSignalKind::Prompt | TaskSignalKind::Diff))
        .map(|signal| signal.content.as_str());
    std::iter::once(intent.goal.as_str())
        .chain(intent.current_direction.as_deref())
        .chain(list_text)
        .chain(signal_text)
        .flat_map(search_tokens)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn association_query_tokens(intent: &WorkingIntentSnapshot, signals: &[TaskSignal]) -> Vec<String> {
    let list_text = [
        &intent.in_scope,
        &intent.domains,
        &intent.platforms,
        &intent.constraints,
        &intent.acceptance_conditions,
        &intent.open_questions,
    ]
    .into_iter()
    .flat_map(|values| values.iter().map(String::as_str));
    let signal_text = signals
        .iter()
        .filter(|signal| matches!(signal.kind, TaskSignalKind::Prompt | TaskSignalKind::Diff))
        .map(|signal| signal.content.as_str());
    std::iter::once(intent.goal.as_str())
        .chain(intent.current_direction.as_deref())
        .chain(list_text)
        .chain(signal_text)
        .flat_map(search_tokens)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn association_query_phrases(
    intent: &WorkingIntentSnapshot,
    signals: &[TaskSignal],
) -> Vec<String> {
    let mut texts = vec![intent.goal.as_str()];
    texts.extend(intent.current_direction.as_deref());
    for values in [
        &intent.in_scope,
        &intent.domains,
        &intent.platforms,
        &intent.constraints,
        &intent.acceptance_conditions,
        &intent.open_questions,
    ] {
        texts.extend(values.iter().map(String::as_str));
    }
    texts.extend(
        signals
            .iter()
            .filter(|signal| matches!(signal.kind, TaskSignalKind::Prompt | TaskSignalKind::Diff))
            .map(|signal| signal.content.as_str()),
    );
    normalized_phrases(texts)
}

#[derive(Clone, Debug)]
struct WorkingIntentHintQuery {
    source_field: WorkingIntentHintField,
    tokens: Vec<String>,
    phrases: Vec<String>,
}

fn working_intent_hint_queries(intent: &WorkingIntentSnapshot) -> Vec<WorkingIntentHintQuery> {
    [
        (
            WorkingIntentHintField::ArtifactHints,
            &intent.artifact_hints,
        ),
        (
            WorkingIntentHintField::InterfaceHints,
            &intent.interface_hints,
        ),
    ]
    .into_iter()
    .filter_map(|(source_field, values)| {
        let tokens = values
            .iter()
            .flat_map(|value| search_tokens(value))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        (!tokens.is_empty()).then(|| WorkingIntentHintQuery {
            source_field,
            tokens,
            phrases: normalized_phrases(values.iter().map(String::as_str)),
        })
    })
    .collect()
}

fn normalized_phrases<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    texts
        .into_iter()
        .filter(|text| search_tokens(text).len() > 1)
        .map(normalize_search_text)
        .filter(|phrase| !phrase.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn task_query_phrases(
    intent: &WorkingIntentSnapshot,
    signals: &[TaskSignal],
    include_out_of_scope: bool,
) -> Vec<String> {
    let mut texts = vec![intent.goal.as_str()];
    texts.extend(intent.current_direction.as_deref());
    for values in [
        &intent.in_scope,
        &intent.domains,
        &intent.platforms,
        &intent.constraints,
        &intent.acceptance_conditions,
        &intent.artifact_hints,
        &intent.interface_hints,
        &intent.open_questions,
    ] {
        texts.extend(values.iter().map(String::as_str));
    }
    if include_out_of_scope {
        texts.extend(intent.out_of_scope.iter().map(String::as_str));
    }
    texts.extend(
        signals
            .iter()
            .filter(|signal| matches!(signal.kind, TaskSignalKind::Prompt | TaskSignalKind::Diff))
            .map(|signal| signal.content.as_str()),
    );
    normalized_phrases(texts)
}

fn query_space_intent_candidates(
    connection: &Connection,
    query_tokens: &[String],
    query_phrases: &[String],
) -> Result<Vec<SpaceIntentCandidate>> {
    let Some(match_expression) = fts_or_match_expression(query_tokens) else {
        return Ok(Vec::new());
    };
    let mut statement = connection
        .prepare(
            "SELECT space_fts.space_id, space_fts.revision_id,
                    space.intent_conflicted,
                    bm25(space_fts, 0.0, 0.0, 10.0, 8.0, 8.0, 4.0, 1.0, 6.0, 5.0),
                    space_fts.title, space_fts.problem, space_fts.desired_outcome,
                    space_fts.in_scope, space_fts.out_of_scope,
                    space_fts.acceptance_conditions, space_fts.domain_terms
             FROM space_fts
             JOIN intent_head
               ON intent_head.space_id = space_fts.space_id
              AND intent_head.revision_id = space_fts.revision_id
             JOIN space_projection AS space USING(space_id)
             WHERE space_fts MATCH ?1
             ORDER BY 4 ASC, space_fts.space_id ASC, space_fts.revision_id ASC",
        )
        .map_err(sql_error("prepare Space Intent candidate query"))?;
    let raw = statement
        .query_map([match_expression], read_intent_fts_match)
        .map_err(sql_error("execute Space Intent candidate query"))?
        .map(|row| {
            parse_intent_fts_match(
                &row.map_err(sql_error("collect Space Intent candidate row"))?,
                query_tokens,
                query_phrases,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    drop(statement);

    let mut grouped = BTreeMap::<SpaceId, (bool, Vec<SpaceIntentHeadMatch>)>::new();
    for row in raw {
        let entry = grouped
            .entry(row.space_id)
            .or_insert_with(|| (row.intent_conflicted, Vec::new()));
        entry.1.push(row.head_match);
    }

    let mut candidates = Vec::with_capacity(grouped.len());
    for (space_id, (intent_conflicted, mut matching_heads)) in grouped {
        matching_heads.sort_by_key(|head| head.revision_id);
        let head_revision_ids = load_intent_head_ids(connection, space_id)?;
        let mut matched_fields = BTreeSet::new();
        let mut matched_tokens = BTreeSet::new();
        let mut field_tokens = BTreeMap::<SpaceIntentField, BTreeSet<String>>::new();
        let mut phrase_match = false;
        let mut bm25 = f64::INFINITY;
        for head in &matching_heads {
            matched_fields.extend(head.matched_fields.iter().copied());
            matched_tokens.extend(head.matched_tokens.iter().cloned());
            for field_match in &head.field_matches {
                field_tokens
                    .entry(field_match.field)
                    .or_default()
                    .extend(field_match.matched_tokens.iter().cloned());
            }
            phrase_match |= head.phrase_match;
            bm25 = bm25.min(head.bm25);
        }
        candidates.push(SpaceIntentCandidate {
            space_id,
            intent_conflicted,
            head_revision_ids,
            matching_heads,
            field_matches: field_tokens
                .into_iter()
                .map(|(field, matched_tokens)| SpaceIntentFieldMatch {
                    field,
                    matched_tokens: matched_tokens.into_iter().collect(),
                })
                .collect(),
            matched_fields: matched_fields.into_iter().collect(),
            matched_tokens: matched_tokens.into_iter().collect(),
            phrase_match,
            bm25,
        });
    }
    candidates.sort_by(|left, right| {
        left.bm25
            .total_cmp(&right.bm25)
            .then_with(|| left.space_id.cmp(&right.space_id))
    });
    Ok(candidates)
}

fn read_intent_fts_match(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredIntentFtsMatch> {
    Ok(StoredIntentFtsMatch {
        space_id: row.get(0)?,
        revision_id: row.get(1)?,
        intent_conflicted: row.get::<_, i64>(2)? != 0,
        bm25: row.get(3)?,
        fields: [
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
        ],
    })
}

fn parse_intent_fts_match(
    row: &StoredIntentFtsMatch,
    query_tokens: &[String],
    query_phrases: &[String],
) -> Result<RawIntentHeadMatch> {
    let field_names = [
        SpaceIntentField::Title,
        SpaceIntentField::Problem,
        SpaceIntentField::DesiredOutcome,
        SpaceIntentField::InScope,
        SpaceIntentField::OutOfScope,
        SpaceIntentField::AcceptanceConditions,
        SpaceIntentField::DomainTerms,
    ];
    let fields: [(SpaceIntentField, &str); 7] =
        std::array::from_fn(|index| (field_names[index], row.fields[index].as_str()));
    let field_matches = explain_intent_match(query_tokens, fields);
    let positive_fields = [
        row.fields[0].as_str(),
        row.fields[1].as_str(),
        row.fields[2].as_str(),
        row.fields[3].as_str(),
        row.fields[5].as_str(),
        row.fields[6].as_str(),
    ];
    let matched_fields = field_matches.iter().map(|value| value.field).collect();
    let matched_tokens = field_matches
        .iter()
        .flat_map(|value| value.matched_tokens.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(RawIntentHeadMatch {
        space_id: parse_id(&row.space_id)?,
        intent_conflicted: row.intent_conflicted,
        head_match: SpaceIntentHeadMatch {
            revision_id: parse_id(&row.revision_id)?,
            field_matches,
            matched_fields,
            matched_tokens,
            phrase_match: contains_any_phrase(&positive_fields, query_phrases),
            bm25: row.bm25,
        },
    })
}

fn load_intent_head_ids(connection: &Connection, space_id: SpaceId) -> Result<Vec<RevisionId>> {
    let mut statement = connection
        .prepare("SELECT revision_id FROM intent_head WHERE space_id = ?1 ORDER BY revision_id ASC")
        .map_err(sql_error("prepare Space Intent head expansion"))?;
    statement
        .query_map([space_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(sql_error("read Space Intent heads"))?
        .map(|row| {
            let revision_id = row.map_err(sql_error("collect Space Intent head"))?;
            parse_id(&revision_id)
        })
        .collect()
}

fn explain_intent_match<const N: usize>(
    query_tokens: &[String],
    fields: [(SpaceIntentField, &str); N],
) -> Vec<SpaceIntentFieldMatch> {
    let wanted = query_tokens.iter().cloned().collect::<BTreeSet<_>>();
    fields
        .into_iter()
        .filter_map(|(field, text)| {
            let available = search_tokens(text).into_iter().collect::<BTreeSet<_>>();
            let matched_tokens = wanted.intersection(&available).cloned().collect::<Vec<_>>();
            (!matched_tokens.is_empty()).then_some(SpaceIntentFieldMatch {
                field,
                matched_tokens,
            })
        })
        .collect()
}

#[derive(Debug, Default)]
struct ScopeTargets {
    domains: BTreeSet<String>,
    platforms: BTreeSet<String>,
    conditions: BTreeSet<String>,
}

impl ScopeTargets {
    fn from_intent(intent: &WorkingIntentSnapshot) -> Self {
        Self {
            domains: normalized_values(&intent.domains),
            platforms: normalized_values(&intent.platforms),
            conditions: normalized_values(&intent.constraints),
        }
    }

    fn matches(&self, dimension: &str, value: &str) -> bool {
        let value = normalize_search_text(value);
        match dimension {
            "domain" => self.domains.contains(&value),
            "platform" => self.platforms.contains(&value),
            "condition" => self.conditions.contains(&value),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ScopeEvidence {
    dimension: String,
    value: String,
}

#[derive(Clone, Debug, Default)]
struct AcceptedContextEvidence {
    textual_match: bool,
    matched_fields: BTreeSet<MatchField>,
    matched_tokens: BTreeSet<String>,
    bm25: Option<f64>,
    phrase_match: bool,
    matched_artifacts: BTreeSet<String>,
    matched_scopes: BTreeSet<ScopeEvidence>,
    hint_text: BTreeMap<WorkingIntentHintTextChannel, HintTextEvidence>,
    graph_paths: Vec<TaskRetrievalPath>,
    relation_depth: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WorkingIntentHintTextChannel {
    source_field: WorkingIntentHintField,
    target: WorkingIntentHintTarget,
}

#[derive(Clone, Debug, Default)]
struct HintTextEvidence {
    query_tokens: BTreeSet<String>,
    matched_tokens: BTreeSet<String>,
    bm25: Option<f64>,
    phrase_match: bool,
    field_weight_points: u16,
}

impl HintTextEvidence {
    fn merge(&mut self, other: &Self) {
        self.query_tokens.extend(other.query_tokens.iter().cloned());
        self.matched_tokens
            .extend(other.matched_tokens.iter().cloned());
        if let Some(bm25) = other.bm25 {
            self.bm25 = Some(self.bm25.map_or(bm25, |current| current.min(bm25)));
        }
        self.phrase_match |= other.phrase_match;
        self.field_weight_points = self
            .field_weight_points
            .saturating_add(other.field_weight_points);
    }

    fn coverage_basis_points(&self) -> u16 {
        if self.query_tokens.is_empty() {
            return 0;
        }
        u16::try_from(
            self.matched_tokens
                .intersection(&self.query_tokens)
                .count()
                .saturating_mul(BASIS_POINTS_SCALE)
                .checked_div(self.query_tokens.len())
                .unwrap_or(0)
                .min(BASIS_POINTS_SCALE),
        )
        .expect("coverage basis points fit u16")
    }
}

#[derive(Clone, Debug)]
struct GraphContextEvidence {
    snapshot: GraphContextSnapshot,
    evidence: AcceptedContextEvidence,
}

type GraphContextKey = (SpaceId, ContextId, RevisionId);

const SAFE_ACCEPTED_CONTEXT_PREDICATE: &str = "item.governance_status = 'accepted'
     AND item.accepted_revision_id = revision.revision_id
     AND item.auto_injection_eligible = 1
     AND revision.lifecycle = 'accepted'
     AND revision.evidence_completeness = 1000
     AND EXISTS (
         SELECT 1 FROM evidence AS required_evidence
         WHERE required_evidence.revision_id = revision.revision_id
           AND trim(required_evidence.supports) <> ''
           AND trim(required_evidence.interpretation) <> ''
           AND required_evidence.content_json <> '{}'
     )";

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct AssociationEvidence {
    intent_fields: BTreeSet<String>,
    intent_tokens: BTreeSet<String>,
    intent_matched: bool,
    intent_conflicted: bool,
    intent_head_revision_ids: BTreeSet<RevisionId>,
    intent_bm25: Option<f64>,
    intent_phrase_match: bool,
    intent_field_weight_points: u16,
    excluded_intent_tokens: BTreeSet<String>,
    matched_artifacts: BTreeSet<String>,
    matched_contexts: BTreeSet<ContextId>,
    textual_contexts: BTreeSet<ContextId>,
    context_tokens: BTreeSet<String>,
    context_fields: BTreeSet<MatchField>,
    context_bm25: Option<f64>,
    context_phrase_match: bool,
    context_field_weight_points: u16,
    hint_text: BTreeMap<WorkingIntentHintTextChannel, HintTextEvidence>,
    matched_scopes: BTreeSet<ScopeEvidence>,
    graph_exact_contexts: BTreeSet<ContextId>,
    relation_contexts: BTreeSet<ContextId>,
    relation_paths: BTreeSet<Vec<String>>,
    channel_features: Vec<TaskAssociationChannelFeature>,
    fused_score_basis_points: u16,
}

#[derive(Debug)]
struct TaskAssociationInference {
    associations: Vec<TaskSpaceAssociation>,
    evidence: BTreeMap<SpaceId, AssociationEvidence>,
    contexts: BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
    graph_contexts: BTreeMap<GraphContextKey, GraphContextEvidence>,
    graph_context_tree_oid: Option<String>,
    graph_artifact_generation: Option<String>,
    focus_reachable: bool,
}

fn normalized_values(values: &[String]) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| normalize_search_text(value))
        .collect()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn infer_task_space_associations(
    connection: &Connection,
    task_id: TaskId,
    query_tokens: &[String],
    query_phrases: &[String],
    hint_queries: &[WorkingIntentHintQuery],
    scope_targets: &ScopeTargets,
    resolved_focus: Option<&ResolvedFocus>,
    engineering_graph: Option<&EngineeringProjection>,
    graph_context_tree_oid: Option<&str>,
    mode: ContextPackMode,
) -> Result<TaskAssociationInference> {
    let intent_candidates = query_space_intent_candidates(connection, query_tokens, query_phrases)?;
    let mut contexts =
        query_accepted_context_evidence(connection, query_tokens, query_phrases, scope_targets)?;
    let mut evidence = BTreeMap::<SpaceId, AssociationEvidence>::new();
    apply_intent_evidence(&mut evidence, intent_candidates);
    apply_working_intent_hint_evidence(connection, hint_queries, &mut evidence, &mut contexts)?;
    let mut graph_contexts = BTreeMap::new();
    let mut focus_reachable = false;
    if let Some(graph) = engineering_graph {
        focus_reachable =
            query_graph_context_evidence(graph, resolved_focus, mode, &mut graph_contexts);
        expand_graph_context_relation_evidence(graph, mode, &mut graph_contexts)?;
    }
    expand_current_context_relation_evidence(connection, mode, &mut contexts)?;
    for ((space_id, context_id), context) in &contexts {
        aggregate_context_evidence(evidence.entry(*space_id).or_default(), *context_id, context);
    }
    for ((space_id, context_id, _revision_id), graph) in &graph_contexts {
        aggregate_context_evidence(
            evidence.entry(*space_id).or_default(),
            *context_id,
            &graph.evidence,
        );
    }
    hydrate_intent_conflict_state(connection, &mut evidence)?;
    assign_channel_features(&mut evidence, query_tokens);
    let mut associations = evidence
        .iter()
        .filter_map(|(space_id, evidence)| association(task_id, *space_id, evidence))
        .collect::<Vec<_>>();
    associations.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.space_id.cmp(&right.space_id))
    });
    TaskSpaceAssociation::validate_collection(task_id, &associations)?;
    Ok(TaskAssociationInference {
        associations,
        evidence,
        contexts,
        graph_contexts,
        graph_context_tree_oid: graph_context_tree_oid.map(ToOwned::to_owned),
        graph_artifact_generation: engineering_graph.map(|graph| graph.artifact_generation.clone()),
        focus_reachable,
    })
}

fn aggregate_context_evidence(
    aggregate: &mut AssociationEvidence,
    context_id: ContextId,
    context: &AcceptedContextEvidence,
) {
    aggregate.matched_contexts.insert(context_id);
    if context.textual_match {
        aggregate.textual_contexts.insert(context_id);
        aggregate
            .context_tokens
            .extend(context.matched_tokens.iter().cloned());
        if let Some(bm25) = context.bm25 {
            aggregate.context_bm25 = Some(
                aggregate
                    .context_bm25
                    .map_or(bm25, |current| current.min(bm25)),
            );
        }
        aggregate.context_phrase_match |= context.phrase_match;
        for field in &context.matched_fields {
            if aggregate.context_fields.insert(*field) {
                aggregate.context_field_weight_points = aggregate
                    .context_field_weight_points
                    .saturating_add(context_field_weight(*field));
            }
        }
    }
    for (channel, hint) in &context.hint_text {
        aggregate.hint_text.entry(*channel).or_default().merge(hint);
    }
    aggregate
        .matched_artifacts
        .extend(context.matched_artifacts.iter().cloned());
    aggregate
        .matched_scopes
        .extend(context.matched_scopes.iter().cloned());
    if context
        .graph_paths
        .iter()
        .any(|path| matches!(path, TaskRetrievalPath::EngineeringGraph { relation_hops, .. } if relation_hops.is_empty()))
    {
        aggregate.graph_exact_contexts.insert(context_id);
    }
    if context.relation_depth.is_some() {
        aggregate.relation_contexts.insert(context_id);
    }
    aggregate.relation_paths.extend(
        context
            .graph_paths
            .iter()
            .filter(|path| {
                matches!(
                    path,
                    TaskRetrievalPath::EngineeringGraph { .. }
                        | TaskRetrievalPath::ContextRelation { .. }
                )
            })
            .map(|path| {
                vec![
                    serde_json::to_string(path)
                        .expect("typed Graph RetrievalPath is always serializable"),
                ]
            }),
    );
}

fn hydrate_intent_conflict_state(
    connection: &Connection,
    evidence: &mut BTreeMap<SpaceId, AssociationEvidence>,
) -> Result<()> {
    let mut statement = connection
        .prepare(
            "SELECT space_id FROM space_projection
             WHERE intent_conflicted = 1
             ORDER BY space_id",
        )
        .map_err(sql_error("prepare conflicted Space hydration"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error("read conflicted Spaces"))?;
    for row in rows {
        let space_id = parse_id(&row.map_err(sql_error("collect conflicted Space"))?)?;
        if let Some(aggregate) = evidence.get_mut(&space_id) {
            aggregate.intent_conflicted = true;
            aggregate
                .intent_head_revision_ids
                .extend(load_intent_head_ids(connection, space_id)?);
        }
    }
    Ok(())
}

fn apply_intent_evidence(
    evidence: &mut BTreeMap<SpaceId, AssociationEvidence>,
    candidates: Vec<SpaceIntentCandidate>,
) {
    for candidate in candidates {
        let aggregate = evidence.entry(candidate.space_id).or_default();
        aggregate.intent_conflicted = candidate.intent_conflicted;
        aggregate
            .intent_head_revision_ids
            .extend(candidate.head_revision_ids);
        aggregate.intent_bm25 = Some(
            aggregate
                .intent_bm25
                .map_or(candidate.bm25, |current| current.min(candidate.bm25)),
        );
        aggregate.intent_phrase_match |= candidate.phrase_match;
        for field_match in candidate.field_matches {
            if field_match.field == SpaceIntentField::OutOfScope {
                aggregate
                    .excluded_intent_tokens
                    .extend(field_match.matched_tokens);
            } else {
                aggregate.intent_matched = true;
                if aggregate
                    .intent_fields
                    .insert(intent_field_name(field_match.field).to_owned())
                {
                    aggregate.intent_field_weight_points = aggregate
                        .intent_field_weight_points
                        .saturating_add(intent_field_weight(field_match.field));
                }
                aggregate.intent_tokens.extend(field_match.matched_tokens);
            }
        }
    }
}

fn apply_working_intent_hint_evidence(
    connection: &Connection,
    queries: &[WorkingIntentHintQuery],
    associations: &mut BTreeMap<SpaceId, AssociationEvidence>,
    contexts: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    for query in queries {
        let space_channel = WorkingIntentHintTextChannel {
            source_field: query.source_field,
            target: WorkingIntentHintTarget::SpaceIntentFts,
        };
        for candidate in query_space_intent_candidates(connection, &query.tokens, &query.phrases)? {
            let aggregate = associations.entry(candidate.space_id).or_default();
            aggregate.intent_conflicted |= candidate.intent_conflicted;
            aggregate
                .intent_head_revision_ids
                .extend(candidate.head_revision_ids);
            let mut matched_tokens = BTreeSet::new();
            let mut matched_fields = BTreeSet::new();
            for field_match in candidate.field_matches {
                if field_match.field == SpaceIntentField::OutOfScope {
                    aggregate
                        .excluded_intent_tokens
                        .extend(field_match.matched_tokens);
                } else {
                    matched_fields.insert(field_match.field);
                    matched_tokens.extend(field_match.matched_tokens);
                }
            }
            if !matched_tokens.is_empty() {
                aggregate
                    .hint_text
                    .entry(space_channel)
                    .or_default()
                    .merge(&HintTextEvidence {
                        query_tokens: query.tokens.iter().cloned().collect(),
                        matched_tokens,
                        bm25: Some(candidate.bm25),
                        phrase_match: candidate.phrase_match,
                        field_weight_points: matched_fields
                            .into_iter()
                            .map(intent_field_weight)
                            .fold(0_u16, u16::saturating_add),
                    });
            }
        }

        let mut context_matches = BTreeMap::new();
        query_accepted_context_text(
            connection,
            &query.tokens,
            &query.phrases,
            &mut context_matches,
        )?;
        let context_channel = WorkingIntentHintTextChannel {
            source_field: query.source_field,
            target: WorkingIntentHintTarget::AcceptedContextFts,
        };
        for (key, matched) in context_matches {
            let hint = HintTextEvidence {
                query_tokens: query.tokens.iter().cloned().collect(),
                matched_tokens: matched.matched_tokens,
                bm25: matched.bm25,
                phrase_match: matched.phrase_match,
                field_weight_points: matched
                    .matched_fields
                    .into_iter()
                    .map(context_field_weight)
                    .fold(0_u16, u16::saturating_add),
            };
            contexts
                .entry(key)
                .or_default()
                .hint_text
                .entry(context_channel)
                .or_default()
                .merge(&hint);
        }
    }
    Ok(())
}

const fn intent_field_name(field: SpaceIntentField) -> &'static str {
    match field {
        SpaceIntentField::Title => "title",
        SpaceIntentField::Problem => "problem",
        SpaceIntentField::DesiredOutcome => "desired_outcome",
        SpaceIntentField::InScope => "in_scope",
        SpaceIntentField::OutOfScope => "out_of_scope",
        SpaceIntentField::AcceptanceConditions => "acceptance_conditions",
        SpaceIntentField::DomainTerms => "domain_terms",
    }
}

const fn intent_field_weight(field: SpaceIntentField) -> u16 {
    match field {
        SpaceIntentField::Title => 10,
        SpaceIntentField::Problem | SpaceIntentField::DesiredOutcome => 8,
        SpaceIntentField::InScope => 4,
        SpaceIntentField::OutOfScope => 0,
        SpaceIntentField::AcceptanceConditions => 6,
        SpaceIntentField::DomainTerms => 5,
    }
}

const fn context_field_weight(field: MatchField) -> u16 {
    match field {
        MatchField::Title => 10,
        MatchField::Statement => 8,
        MatchField::Rationale => 4,
        MatchField::Evidence => 2,
    }
}

fn query_accepted_context_evidence(
    connection: &Connection,
    query_tokens: &[String],
    query_phrases: &[String],
    scope_targets: &ScopeTargets,
) -> Result<BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>> {
    let mut evidence = BTreeMap::new();
    query_accepted_context_text(connection, query_tokens, query_phrases, &mut evidence)?;
    query_accepted_context_scope(connection, scope_targets, &mut evidence)?;
    Ok(evidence)
}

#[allow(clippy::too_many_lines)]
fn query_graph_context_evidence(
    graph: &EngineeringProjection,
    resolved_focus: Option<&ResolvedFocus>,
    mode: ContextPackMode,
    contexts: &mut BTreeMap<GraphContextKey, GraphContextEvidence>,
) -> bool {
    let Some(resolved_focus) = resolved_focus else {
        return false;
    };
    let mut reachable = false;
    for resolved in &graph.references {
        let candidates = resolved
            .resolution
            .resolved_artifact
            .iter()
            .chain(resolved.resolution.candidates.iter())
            .collect::<Vec<_>>();
        let Some(artifact) = candidates
            .iter()
            .find(|artifact| focus_matches_artifact(resolved_focus, artifact))
            .copied()
        else {
            continue;
        };
        let Some(snapshot) = graph.contexts.iter().find(|snapshot| {
            snapshot.context_id == resolved.context_id
                && snapshot.revision.revision_id == resolved.revision_id
        }) else {
            continue;
        };
        if mode == ContextPackMode::AutomaticInjection
            && !snapshot.safety.automatic_injection_eligible
        {
            continue;
        }
        let key = (
            snapshot.space_id,
            snapshot.context_id,
            snapshot.revision.revision_id,
        );
        if resolved.resolution.status == ResolutionStatus::Resolved {
            let Some(association) = &resolved.association else {
                continue;
            };
            let Some(match_evidence) = resolved
                .evidence
                .iter()
                .find(|evidence| evidence.artifact_key.as_ref() == Some(&association.artifact_key))
            else {
                continue;
            };
            reachable = true;
            let path = GraphArtifactRetrievalPath {
                resolved_focus: resolved_focus.clone(),
                repository_id: artifact.repository_id(),
                artifact_key: artifact.clone(),
                artifact_kind: artifact.kind(),
                reference_id: resolved.reference_id,
                association_id: context_artifact_association_id(
                    resolved.reference_id,
                    resolved.context_id,
                    resolved.revision_id,
                    artifact,
                ),
                association_kind: association.kind,
                match_basis: match_evidence.basis,
                resolution_status: resolved.resolution.status,
                confidence_basis_points: confidence_basis_points(association.confidence),
                artifact_generation: graph.artifact_generation.clone(),
            };
            let context = contexts.entry(key).or_insert_with(|| GraphContextEvidence {
                snapshot: snapshot.clone(),
                evidence: AcceptedContextEvidence::default(),
            });
            context.evidence.matched_artifacts.insert(
                serde_json::to_string(resolved_focus)
                    .expect("ResolvedFocus is always JSON serializable"),
            );
            context
                .evidence
                .graph_paths
                .push(TaskRetrievalPath::EngineeringGraph {
                    path,
                    relation_hops: Vec::new(),
                });
        } else if mode == ContextPackMode::Explicit {
            reachable = true;
            let mut bases = resolved
                .evidence
                .iter()
                .map(|evidence| evidence.basis)
                .collect::<Vec<_>>();
            bases.sort();
            bases.dedup();
            contexts
                .entry(key)
                .or_insert_with(|| GraphContextEvidence {
                    snapshot: snapshot.clone(),
                    evidence: AcceptedContextEvidence::default(),
                })
                .evidence
                .graph_paths
                .push(TaskRetrievalPath::GraphDiagnostic {
                    diagnostic: GraphResolutionDiagnosticPath {
                        resolved_focus: resolved_focus.clone(),
                        repository_id: resolved.resolution.repository_id.clone(),
                        reference_id: resolved.reference_id,
                        resolution_status: resolved.resolution.status,
                        candidate_artifact_keys: candidates
                            .iter()
                            .map(|artifact| (*artifact).clone())
                            .collect(),
                        match_bases: bases,
                        artifact_generation: graph.artifact_generation.clone(),
                    },
                });
        }
    }
    for context in contexts.values_mut() {
        sort_dedup_paths(&mut context.evidence.graph_paths);
    }
    reachable
}

fn current_context_space(
    connection: &Connection,
    context_id: ContextId,
    revision_id: RevisionId,
    mode: ContextPackMode,
) -> Result<Option<SpaceId>> {
    let predicate = if mode == ContextPackMode::AutomaticInjection {
        SAFE_ACCEPTED_CONTEXT_PREDICATE
    } else {
        "revision.is_head = 1"
    };
    connection
        .query_row(
            &format!(
                "SELECT revision.space_id
                 FROM context_revision AS revision
                 JOIN context_item AS item USING(context_id)
                 WHERE revision.context_id = ?1 AND revision.revision_id = ?2
                   AND {predicate}"
            ),
            [context_id.to_string(), revision_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("read current fallback Context revision"))?
        .map(|space_id| parse_id(&space_id))
        .transpose()
}

fn focus_matches_artifact(focus: &ResolvedFocus, artifact: &ArtifactKey) -> bool {
    focus.repository_id == artifact.repository_id() && &focus.locator == artifact.locator()
}

fn context_artifact_association_id(
    reference_id: ReferenceId,
    context_id: ContextId,
    revision_id: RevisionId,
    artifact: &ArtifactKey,
) -> String {
    let mut hasher = Sha256::new();
    for value in [
        reference_id.to_string(),
        context_id.to_string(),
        revision_id.to_string(),
        artifact.digest().to_owned(),
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("caa_{:x}", hasher.finalize())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn confidence_basis_points(confidence: f64) -> u16 {
    (confidence * 10_000.0).round().clamp(0.0, 10_000.0) as u16
}

#[derive(Clone, Debug)]
struct IndexedContextRelation {
    source_context_id: ContextId,
    source_revision_id: RevisionId,
    target_context_id: ContextId,
    target_revision_id: RevisionId,
    target_space_id: SpaceId,
    kind: ContextRelationKind,
    rationale: String,
    supports: Vec<String>,
}

const DEFAULT_CONTEXT_RELATION_DEPTH: u8 = 2;

#[allow(clippy::too_many_lines)]
fn expand_graph_context_relation_evidence(
    graph: &EngineeringProjection,
    mode: ContextPackMode,
    contexts: &mut BTreeMap<GraphContextKey, GraphContextEvidence>,
) -> Result<()> {
    if contexts.is_empty() {
        return Ok(());
    }
    let snapshots = graph
        .contexts
        .iter()
        .map(|snapshot| {
            (
                (
                    snapshot.space_id,
                    snapshot.context_id,
                    snapshot.revision.revision_id,
                ),
                snapshot,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let seeds = contexts
        .iter()
        .flat_map(|(key, context)| {
            context.evidence.graph_paths.iter().filter_map(move |path| {
                let TaskRetrievalPath::EngineeringGraph {
                    path,
                    relation_hops,
                } = path
                else {
                    return None;
                };
                relation_hops.is_empty().then_some((*key, path.clone()))
            })
        })
        .collect::<Vec<_>>();
    for (seed_key, graph_path) in seeds {
        let mut queue = VecDeque::from([(seed_key, Vec::new(), BTreeSet::from([seed_key.1]))]);
        while let Some((current_key, hops, visited)) = queue.pop_front() {
            if hops.len() >= usize::from(DEFAULT_CONTEXT_RELATION_DEPTH) {
                continue;
            }
            let Some(current) = snapshots.get(&current_key) else {
                return Err(invariant(
                    "Graph relation traversal lost a frozen source Context snapshot",
                ));
            };
            for relation in &current.relations {
                if visited.contains(&relation.target_context_id) {
                    continue;
                }
                let target_key = (
                    relation.target_space_id,
                    relation.target_context_id,
                    relation.target_revision_id,
                );
                let Some(target_snapshot) = snapshots.get(&target_key) else {
                    return Err(invariant(
                        "Graph relation traversal lost a frozen target Context snapshot",
                    ));
                };
                if mode == ContextPackMode::AutomaticInjection
                    && !target_snapshot.safety.automatic_injection_eligible
                {
                    continue;
                }
                let depth =
                    u8::try_from(hops.len() + 1).expect("Context Relation depth is bounded by two");
                let mut next_hops = hops.clone();
                next_hops.push(ContextRelationRetrievalPath {
                    source_context_id: current.context_id,
                    source_revision_id: current.revision.revision_id,
                    target_context_id: target_snapshot.context_id,
                    target_revision_id: target_snapshot.revision.revision_id,
                    kind: relation.kind,
                    rationale: relation.rationale.clone(),
                    supports: relation.supports.clone(),
                    depth,
                });
                let target = contexts
                    .entry(target_key)
                    .or_insert_with(|| GraphContextEvidence {
                        snapshot: (*target_snapshot).clone(),
                        evidence: AcceptedContextEvidence::default(),
                    });
                target.evidence.relation_depth = Some(
                    target
                        .evidence
                        .relation_depth
                        .map_or(depth, |current| current.min(depth)),
                );
                target
                    .evidence
                    .graph_paths
                    .push(TaskRetrievalPath::EngineeringGraph {
                        path: graph_path.clone(),
                        relation_hops: next_hops.clone(),
                    });
                let mut next_visited = visited.clone();
                next_visited.insert(target_snapshot.context_id);
                queue.push_back((target_key, next_hops, next_visited));
            }
        }
    }
    for context in contexts.values_mut() {
        sort_dedup_paths(&mut context.evidence.graph_paths);
    }
    Ok(())
}

fn expand_current_context_relation_evidence(
    connection: &Connection,
    mode: ContextPackMode,
    contexts: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let edges = load_active_context_relations(connection, mode)?;
    if edges.is_empty() || contexts.is_empty() {
        return Ok(());
    }
    let mut outgoing = BTreeMap::<ContextId, Vec<IndexedContextRelation>>::new();
    for edge in edges {
        outgoing
            .entry(edge.source_context_id)
            .or_default()
            .push(edge);
    }
    let seeds = contexts
        .iter()
        .filter(|(_, evidence)| context_is_positive_seed(evidence))
        .map(|((space_id, context_id), _evidence)| (*space_id, *context_id))
        .collect::<Vec<_>>();
    for (_space_id, seed_context_id) in seeds {
        let mut queue = VecDeque::from([(
            seed_context_id,
            Vec::new(),
            BTreeSet::from([seed_context_id]),
        )]);
        while let Some((current, hops, route_visited)) = queue.pop_front() {
            if hops.len() >= usize::from(DEFAULT_CONTEXT_RELATION_DEPTH) {
                continue;
            }
            for edge in outgoing.get(&current).into_iter().flatten() {
                if route_visited.contains(&edge.target_context_id) {
                    continue;
                }
                let depth =
                    u8::try_from(hops.len() + 1).expect("Context Relation depth is bounded by two");
                let mut next_hops = hops.clone();
                next_hops.push(ContextRelationRetrievalPath {
                    source_context_id: edge.source_context_id,
                    source_revision_id: edge.source_revision_id,
                    target_context_id: edge.target_context_id,
                    target_revision_id: edge.target_revision_id,
                    kind: edge.kind,
                    rationale: edge.rationale.clone(),
                    supports: edge.supports.clone(),
                    depth,
                });
                let target = contexts
                    .entry((edge.target_space_id, edge.target_context_id))
                    .or_default();
                target.relation_depth = Some(
                    target
                        .relation_depth
                        .map_or(depth, |current| current.min(depth)),
                );
                target.graph_paths.push(TaskRetrievalPath::ContextRelation {
                    hops: next_hops.clone(),
                });
                let mut next_visited = route_visited.clone();
                next_visited.insert(edge.target_context_id);
                queue.push_back((edge.target_context_id, next_hops, next_visited));
            }
        }
    }
    for context in contexts.values_mut() {
        sort_dedup_paths(&mut context.graph_paths);
    }
    Ok(())
}

fn context_is_positive_seed(evidence: &AcceptedContextEvidence) -> bool {
    evidence.textual_match
        || !evidence.hint_text.is_empty()
        || !evidence.matched_artifacts.is_empty()
        || !evidence.matched_scopes.is_empty()
        || evidence.graph_paths.iter().any(|path| {
            matches!(
                path,
                TaskRetrievalPath::EngineeringGraph { relation_hops, .. }
                    if relation_hops.is_empty()
            )
        })
}

fn load_active_context_relations(
    connection: &Connection,
    mode: ContextPackMode,
) -> Result<Vec<IndexedContextRelation>> {
    let mut statement = connection
        .prepare(
            "SELECT relation.source_context_id, relation.source_revision_id,
                    relation.target_context_id, target_item.accepted_revision_id,
                    relation.target_space_id, relation.kind, relation.rationale,
                    relation.supports_json
             FROM context_relation AS relation
             JOIN context_item AS source_item
               ON source_item.context_id = relation.source_context_id
             JOIN context_item AS target_item
               ON target_item.context_id = relation.target_context_id
             WHERE source_item.accepted_revision_id = relation.source_revision_id
               AND target_item.accepted_revision_id IS NOT NULL
             ORDER BY relation.source_context_id, relation.target_context_id,
                      relation.kind, relation.source_revision_id",
        )
        .map_err(sql_error("prepare active Context Relation retrieval"))?;
    let rows = statement
        .query_map([], |row| {
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
        })
        .map_err(sql_error("read active Context Relations"))?;
    let mut edges = Vec::new();
    for row in rows {
        let (
            source,
            source_revision,
            target,
            target_revision,
            target_space,
            kind,
            rationale,
            supports,
        ) = row.map_err(sql_error("collect active Context Relation"))?;
        let source_context_id = parse_id(&source)?;
        let source_revision_id = parse_id(&source_revision)?;
        let target_context_id = parse_id(&target)?;
        let target_revision_id = parse_id(&target_revision)?;
        if current_context_space(connection, source_context_id, source_revision_id, mode)?.is_none()
            || current_context_space(connection, target_context_id, target_revision_id, mode)?
                .is_none()
        {
            continue;
        }
        edges.push(IndexedContextRelation {
            source_context_id,
            source_revision_id,
            target_context_id,
            target_revision_id,
            target_space_id: parse_id(&target_space)?,
            kind: parse_context_relation_kind(&kind)?,
            rationale,
            supports: from_json(&supports)?,
        });
    }
    Ok(edges)
}

fn parse_context_relation_kind(value: &str) -> Result<ContextRelationKind> {
    match value {
        "depends_on" => Ok(ContextRelationKind::DependsOn),
        "constrains" => Ok(ContextRelationKind::Constrains),
        "implements" => Ok(ContextRelationKind::Implements),
        "validated_by" => Ok(ContextRelationKind::ValidatedBy),
        "contradicts" => Ok(ContextRelationKind::Contradicts),
        "related_to" => Ok(ContextRelationKind::RelatedTo),
        _ => Err(invariant(format!(
            "unknown indexed Context Relation kind {value}"
        ))),
    }
}

fn sort_dedup_paths(paths: &mut Vec<TaskRetrievalPath>) {
    paths.sort_by_cached_key(|path| serde_json::to_string(path).unwrap_or_default());
    paths.dedup();
}

fn query_accepted_context_text(
    connection: &Connection,
    query_tokens: &[String],
    query_phrases: &[String],
    evidence: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let Some(match_expression) = fts_or_match_expression(query_tokens) else {
        return Ok(());
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT revision.space_id, revision.context_id,
                    bm25(context_fts, 0.0, 0.0, 10.0, 8.0, 4.0, 2.0),
                    context_fts.title, context_fts.statement,
                    context_fts.rationale, context_fts.evidence
             FROM context_fts
             JOIN context_revision AS revision USING(revision_id)
             JOIN context_item AS item USING(context_id)
             WHERE context_fts MATCH ?1
               AND {SAFE_ACCEPTED_CONTEXT_PREDICATE}
             ORDER BY revision.space_id, revision.context_id"
        ))
        .map_err(sql_error("prepare safe accepted Context association text"))?;
    let rows = statement
        .query_map([match_expression], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                [
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ],
            ))
        })
        .map_err(sql_error("read safe accepted Context association text"))?;
    for row in rows {
        let (space_id, context_id, bm25, fields) =
            row.map_err(sql_error("collect accepted Context association text"))?;
        let (matched_fields, matched_tokens) = explain_context_text_match(query_tokens, &fields);
        let entry = evidence
            .entry((parse_id(&space_id)?, parse_id(&context_id)?))
            .or_default();
        entry.textual_match = true;
        entry.matched_fields.extend(matched_fields);
        entry.matched_tokens.extend(matched_tokens);
        entry.phrase_match |= contains_any_phrase(&fields, query_phrases);
        entry.bm25 = Some(entry.bm25.map_or(bm25, |current| current.min(bm25)));
    }
    Ok(())
}

fn explain_context_text_match(
    query_tokens: &[String],
    fields: &[String; 4],
) -> (Vec<MatchField>, Vec<String>) {
    let names = [
        MatchField::Title,
        MatchField::Statement,
        MatchField::Rationale,
        MatchField::Evidence,
    ];
    let named_fields: [(MatchField, &str); 4] =
        std::array::from_fn(|index| (names[index], fields[index].as_str()));
    explain_token_fields(query_tokens, named_fields)
}

fn explain_token_fields<T, const N: usize>(
    query_tokens: &[String],
    fields: [(T, &str); N],
) -> (Vec<T>, Vec<String>)
where
    T: Copy,
{
    let wanted = query_tokens.iter().cloned().collect::<BTreeSet<_>>();
    let mut matched_fields = Vec::new();
    let mut matched_tokens = BTreeSet::new();
    for (field, text) in fields {
        let available = search_tokens(text).into_iter().collect::<BTreeSet<_>>();
        let intersection = wanted.intersection(&available).cloned().collect::<Vec<_>>();
        if !intersection.is_empty() {
            matched_fields.push(field);
            matched_tokens.extend(intersection);
        }
    }
    (matched_fields, matched_tokens.into_iter().collect())
}

fn query_accepted_context_scope(
    connection: &Connection,
    targets: &ScopeTargets,
    evidence: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT revision.space_id, revision.context_id, scope.dimension, scope.value
             FROM scope
             JOIN context_revision AS revision USING(revision_id)
             JOIN context_item AS item USING(context_id)
             WHERE {SAFE_ACCEPTED_CONTEXT_PREDICATE}
             ORDER BY revision.space_id, revision.context_id, scope.dimension, scope.value"
        ))
        .map_err(sql_error("prepare safe accepted Context scope association"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error("read safe accepted Context scope association"))?;
    for row in rows {
        let (space_id, context_id, dimension, value) =
            row.map_err(sql_error("collect accepted Context scope association"))?;
        if targets.matches(&dimension, &value) {
            evidence
                .entry((parse_id(&space_id)?, parse_id(&context_id)?))
                .or_default()
                .matched_scopes
                .insert(ScopeEvidence { dimension, value });
        }
    }
    Ok(())
}

fn contains_any_phrase<T, const N: usize>(fields: &[T; N], phrases: &[String]) -> bool
where
    T: AsRef<str>,
{
    fields.iter().any(|field| {
        let field = field.as_ref();
        phrases.iter().any(|phrase| field.contains(phrase))
    })
}

const RRF_K: usize = 60;
const RRF_SCALE: usize = 1_000_000;
const M2_FUSION_CHANNEL_WEIGHT: usize = 1;
const GRAPH_ARTIFACT_CHANNEL_WEIGHT: usize = 13;
const CONTEXT_RELATION_CHANNEL_WEIGHT: usize = 10;
const HINT_TEXT_CHANNEL_WEIGHT: usize = 3;
const FUSION_CHANNEL_WEIGHT: usize = GRAPH_ARTIFACT_CHANNEL_WEIGHT
    + CONTEXT_RELATION_CHANNEL_WEIGHT
    + (4 * HINT_TEXT_CHANNEL_WEIGHT)
    + 3;
const MINIMUM_ASSOCIATION_SCORE_BASIS_POINTS: u16 = 100;
const TASK_CONTEXT_ENVELOPE_TOKEN_RESERVE: usize = 128;

#[allow(clippy::too_many_lines)]
fn assign_channel_features(
    evidence: &mut BTreeMap<SpaceId, AssociationEvidence>,
    query_tokens: &[String],
) {
    assign_graph_channel_features(evidence);
    assign_hint_channel_features(evidence);
    let mut intent = evidence
        .iter()
        .filter_map(|(space_id, value)| {
            value.intent_matched.then_some((
                *space_id,
                value.intent_bm25.unwrap_or(0.0),
                token_coverage_basis_points(&value.intent_tokens, query_tokens),
                value.intent_phrase_match,
                value.intent_field_weight_points,
            ))
        })
        .collect::<Vec<_>>();
    intent.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| right.4.cmp(&left.4))
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut previous_intent = None;
    let mut intent_rank = 0;
    for (offset, (space_id, bm25, coverage, phrase_match, field_weight_points)) in
        intent.into_iter().enumerate()
    {
        let key = (bm25.to_bits(), coverage, phrase_match, field_weight_points);
        if previous_intent.as_ref() != Some(&key) {
            intent_rank = offset + 1;
            previous_intent = Some(key);
        }
        evidence
            .get_mut(&space_id)
            .expect("ranked Intent Space exists")
            .channel_features
            .push(text_channel_feature(
                TaskAssociationChannel::SpaceIntentBm25,
                intent_rank,
                bm25,
                coverage,
                phrase_match,
                field_weight_points,
            ));
    }

    let mut contexts = evidence
        .iter()
        .filter_map(|(space_id, value)| {
            value.context_bm25.map(|bm25| {
                (
                    *space_id,
                    bm25,
                    token_coverage_basis_points(&value.context_tokens, query_tokens),
                    value.context_phrase_match,
                    value.context_field_weight_points,
                )
            })
        })
        .collect::<Vec<_>>();
    contexts.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| right.3.cmp(&left.3))
            .then_with(|| right.4.cmp(&left.4))
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut previous_context = None;
    let mut context_rank = 0;
    for (offset, (space_id, bm25, coverage, phrase_match, field_weight_points)) in
        contexts.into_iter().enumerate()
    {
        let key = (bm25.to_bits(), coverage, phrase_match, field_weight_points);
        if previous_context.as_ref() != Some(&key) {
            context_rank = offset + 1;
            previous_context = Some(key);
        }
        evidence
            .get_mut(&space_id)
            .expect("ranked Context Space exists")
            .channel_features
            .push(text_channel_feature(
                TaskAssociationChannel::AcceptedContextBm25,
                context_rank,
                bm25,
                coverage,
                phrase_match,
                field_weight_points,
            ));
    }

    let mut scopes = evidence
        .iter()
        .filter_map(|(space_id, value)| {
            (!value.matched_scopes.is_empty()).then_some((*space_id, value.matched_scopes.len()))
        })
        .collect::<Vec<_>>();
    scopes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let mut previous_scope = None;
    let mut scope_rank = 0;
    for (offset, (space_id, strength)) in scopes.into_iter().enumerate() {
        if previous_scope != Some(strength) {
            scope_rank = offset + 1;
            previous_scope = Some(strength);
        }
        evidence
            .get_mut(&space_id)
            .expect("ranked Scope Space exists")
            .channel_features
            .push(exact_channel_feature(
                TaskAssociationChannel::ExactScope,
                scope_rank,
                strength,
            ));
    }

    let maximum_rrf = FUSION_CHANNEL_WEIGHT * reciprocal_rank_micros(1) as usize;
    for value in evidence.values_mut() {
        value
            .channel_features
            .sort_by_key(|feature| feature.channel);
        let rrf = value
            .channel_features
            .iter()
            .map(|feature| feature.reciprocal_rank_micros as usize)
            .sum::<usize>();
        value.fused_score_basis_points = u16::try_from(
            rrf.saturating_mul(BASIS_POINTS_SCALE)
                .checked_div(maximum_rrf)
                .unwrap_or(0)
                .min(BASIS_POINTS_SCALE),
        )
        .expect("basis points fit u16");
    }
}

fn assign_hint_channel_features(evidence: &mut BTreeMap<SpaceId, AssociationEvidence>) {
    for key in [
        WorkingIntentHintTextChannel {
            source_field: WorkingIntentHintField::ArtifactHints,
            target: WorkingIntentHintTarget::SpaceIntentFts,
        },
        WorkingIntentHintTextChannel {
            source_field: WorkingIntentHintField::ArtifactHints,
            target: WorkingIntentHintTarget::AcceptedContextFts,
        },
        WorkingIntentHintTextChannel {
            source_field: WorkingIntentHintField::InterfaceHints,
            target: WorkingIntentHintTarget::SpaceIntentFts,
        },
        WorkingIntentHintTextChannel {
            source_field: WorkingIntentHintField::InterfaceHints,
            target: WorkingIntentHintTarget::AcceptedContextFts,
        },
    ] {
        let mut matches = evidence
            .iter()
            .filter_map(|(space_id, value)| {
                value.hint_text.get(&key).and_then(|hint| {
                    hint.bm25.map(|bm25| {
                        (
                            *space_id,
                            bm25,
                            hint.coverage_basis_points(),
                            hint.phrase_match,
                            hint.field_weight_points,
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| right.2.cmp(&left.2))
                .then_with(|| right.3.cmp(&left.3))
                .then_with(|| right.4.cmp(&left.4))
                .then_with(|| left.0.cmp(&right.0))
        });
        let mut previous = None;
        let mut rank = 0;
        for (offset, (space_id, bm25, coverage, phrase_match, field_weight_points)) in
            matches.into_iter().enumerate()
        {
            let rank_key = (bm25.to_bits(), coverage, phrase_match, field_weight_points);
            if previous.as_ref() != Some(&rank_key) {
                rank = offset + 1;
                previous = Some(rank_key);
            }
            evidence
                .get_mut(&space_id)
                .expect("ranked Working Intent Hint Space exists")
                .channel_features
                .push(weighted_text_channel_feature(
                    hint_association_channel(key),
                    rank,
                    bm25,
                    coverage,
                    phrase_match,
                    field_weight_points,
                    HINT_TEXT_CHANNEL_WEIGHT,
                ));
        }
    }
}

const fn hint_association_channel(key: WorkingIntentHintTextChannel) -> TaskAssociationChannel {
    match (key.source_field, key.target) {
        (WorkingIntentHintField::ArtifactHints, WorkingIntentHintTarget::SpaceIntentFts) => {
            TaskAssociationChannel::ArtifactHintSpaceIntentBm25
        }
        (WorkingIntentHintField::ArtifactHints, WorkingIntentHintTarget::AcceptedContextFts) => {
            TaskAssociationChannel::ArtifactHintAcceptedContextBm25
        }
        (WorkingIntentHintField::InterfaceHints, WorkingIntentHintTarget::SpaceIntentFts) => {
            TaskAssociationChannel::InterfaceHintSpaceIntentBm25
        }
        (WorkingIntentHintField::InterfaceHints, WorkingIntentHintTarget::AcceptedContextFts) => {
            TaskAssociationChannel::InterfaceHintAcceptedContextBm25
        }
    }
}

fn assign_graph_channel_features(evidence: &mut BTreeMap<SpaceId, AssociationEvidence>) {
    let mut artifacts = evidence
        .iter()
        .filter_map(|(space_id, value)| {
            (!value.graph_exact_contexts.is_empty())
                .then_some((*space_id, value.graph_exact_contexts.len()))
        })
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let mut previous = None;
    let mut rank = 0;
    for (offset, (space_id, strength)) in artifacts.into_iter().enumerate() {
        if previous != Some(strength) {
            rank = offset + 1;
            previous = Some(strength);
        }
        evidence
            .get_mut(&space_id)
            .expect("ranked Graph Artifact Space exists")
            .channel_features
            .push(weighted_exact_channel_feature(
                TaskAssociationChannel::ResolvedArtifactExact,
                rank,
                strength,
                GRAPH_ARTIFACT_CHANNEL_WEIGHT,
            ));
    }

    let mut relations = evidence
        .iter()
        .filter_map(|(space_id, value)| {
            (!value.relation_contexts.is_empty())
                .then_some((*space_id, value.relation_contexts.len()))
        })
        .collect::<Vec<_>>();
    relations.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    previous = None;
    rank = 0;
    for (offset, (space_id, strength)) in relations.into_iter().enumerate() {
        if previous != Some(strength) {
            rank = offset + 1;
            previous = Some(strength);
        }
        evidence
            .get_mut(&space_id)
            .expect("ranked Context Relation Space exists")
            .channel_features
            .push(weighted_exact_channel_feature(
                TaskAssociationChannel::ContextRelation,
                rank,
                strength,
                CONTEXT_RELATION_CHANNEL_WEIGHT,
            ));
    }
}

fn text_channel_feature(
    channel: TaskAssociationChannel,
    rank: usize,
    bm25: f64,
    coverage: u16,
    phrase_match: bool,
    field_weight_points: u16,
) -> TaskAssociationChannelFeature {
    weighted_text_channel_feature(
        channel,
        rank,
        bm25,
        coverage,
        phrase_match,
        field_weight_points,
        M2_FUSION_CHANNEL_WEIGHT,
    )
}

#[allow(clippy::too_many_arguments)]
fn weighted_text_channel_feature(
    channel: TaskAssociationChannel,
    rank: usize,
    bm25: f64,
    coverage: u16,
    phrase_match: bool,
    field_weight_points: u16,
    weight: usize,
) -> TaskAssociationChannelFeature {
    TaskAssociationChannelFeature {
        channel,
        rank,
        reciprocal_rank_micros: reciprocal_rank_micros(rank)
            .saturating_mul(u32::try_from(weight).unwrap_or(u32::MAX)),
        bm25_micros: Some(scale_bm25(bm25)),
        query_token_coverage_basis_points: coverage,
        idf_bm25_contribution_micros: scale_idf_bm25_contribution(bm25),
        phrase_match,
        field_weight_points,
        exact_match_strength: 0,
    }
}

fn exact_channel_feature(
    channel: TaskAssociationChannel,
    rank: usize,
    strength: usize,
) -> TaskAssociationChannelFeature {
    weighted_exact_channel_feature(channel, rank, strength, M2_FUSION_CHANNEL_WEIGHT)
}

fn weighted_exact_channel_feature(
    channel: TaskAssociationChannel,
    rank: usize,
    strength: usize,
    weight: usize,
) -> TaskAssociationChannelFeature {
    TaskAssociationChannelFeature {
        channel,
        rank,
        reciprocal_rank_micros: reciprocal_rank_micros(rank)
            .saturating_mul(u32::try_from(weight).unwrap_or(u32::MAX)),
        bm25_micros: None,
        query_token_coverage_basis_points: 0,
        idf_bm25_contribution_micros: 0,
        phrase_match: false,
        field_weight_points: 0,
        exact_match_strength: u16::try_from(strength).unwrap_or(u16::MAX),
    }
}

fn reciprocal_rank_micros(rank: usize) -> u32 {
    u32::try_from(RRF_SCALE / RRF_K.saturating_add(rank)).unwrap_or(u32::MAX)
}

fn token_coverage_basis_points(matched: &BTreeSet<String>, query_tokens: &[String]) -> u16 {
    if query_tokens.is_empty() {
        return 0;
    }
    let query = query_tokens.iter().collect::<BTreeSet<_>>();
    let matched_count = matched.iter().filter(|token| query.contains(token)).count();
    u16::try_from(
        matched_count
            .saturating_mul(BASIS_POINTS_SCALE)
            .checked_div(query.len())
            .unwrap_or(0)
            .min(BASIS_POINTS_SCALE),
    )
    .expect("coverage basis points fit u16")
}

#[allow(clippy::cast_possible_truncation)]
fn scale_bm25(value: f64) -> i64 {
    (value * 1_000_000.0).round() as i64
}

fn scale_idf_bm25_contribution(value: f64) -> u32 {
    u32::try_from(scale_bm25(value).saturating_neg()).unwrap_or(u32::MAX)
}

fn association(
    task_id: TaskId,
    space_id: SpaceId,
    evidence: &AssociationEvidence,
) -> Option<TaskSpaceAssociation> {
    if !evidence.intent_matched
        && evidence.hint_text.is_empty()
        && evidence.matched_artifacts.is_empty()
        && evidence.matched_contexts.is_empty()
        && evidence.matched_scopes.is_empty()
    {
        return None;
    }
    if evidence.fused_score_basis_points < MINIMUM_ASSOCIATION_SCORE_BASIS_POINTS {
        return None;
    }
    let score = association_score(evidence);
    let reasons = association_reasons(evidence);
    Some(TaskSpaceAssociation {
        task_id,
        space_id,
        score,
        matched_intent_fields: evidence.intent_fields.iter().cloned().collect(),
        matched_artifacts: evidence.matched_artifacts.iter().cloned().collect(),
        matched_contexts: evidence.matched_contexts.iter().copied().collect(),
        relation_paths: evidence.relation_paths.iter().cloned().collect(),
        reasons,
    })
}

const BASIS_POINTS_SCALE: usize = 10_000;
const SCOPE_CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS: u16 = 5_000;

fn association_score(evidence: &AssociationEvidence) -> f64 {
    f64::from(final_score_basis_points(evidence)) / 10_000.0
}

fn final_score_basis_points(evidence: &AssociationEvidence) -> u16 {
    let mut points = usize::from(evidence.fused_score_basis_points);
    if has_intent_scope_conflict(evidence) {
        points = points.saturating_mul(usize::from(SCOPE_CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS))
            / BASIS_POINTS_SCALE;
    }
    u16::try_from(points).expect("association score basis points fit u16")
}

fn has_intent_scope_conflict(evidence: &AssociationEvidence) -> bool {
    !evidence.excluded_intent_tokens.is_empty()
}

fn intent_scope_conflict(evidence: &AssociationEvidence) -> Option<IntentScopeConflictExplanation> {
    has_intent_scope_conflict(evidence).then(|| IntentScopeConflictExplanation {
        kind: IntentScopeConflictKind::ContextSpaceOutOfScope,
        matched_tokens: evidence.excluded_intent_tokens.iter().cloned().collect(),
        policy: IntentScopeConflictPolicy::PenalizeAssociation,
        score_multiplier_basis_points: SCOPE_CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS,
    })
}

fn intent_conflict_handoff(
    evidence: &AssociationEvidence,
) -> Option<IntentConflictHandoffExplanation> {
    evidence
        .intent_conflicted
        .then(|| IntentConflictHandoffExplanation {
            kind: IntentConflictKind::ContextAndIntentAlternativesConflict,
            head_revision_ids: evidence.intent_head_revision_ids.iter().copied().collect(),
            selection: IntentConflictSelection::SystemHasNotSelectedWinner,
            required_actor: IntentConflictActor::SessionAgent,
            must_validate: vec![
                IntentConflictValidation::CurrentCode,
                IntentConflictValidation::Evidence,
                IntentConflictValidation::TaskApplicability,
            ],
            required_decision: IntentConflictDecision::DecideWhichContextIsMoreSuitable,
        })
}

fn association_reasons(evidence: &AssociationEvidence) -> Vec<String> {
    let mut reasons = vec![
        serde_json::to_string(&TaskAssociationFusionExplanation {
            algorithm: TaskAssociationFusionAlgorithm::ReciprocalRankFusion,
            rrf_k: u16::try_from(RRF_K).expect("RRF K fits u16"),
            channels: evidence.channel_features.clone(),
            fused_score_basis_points: evidence.fused_score_basis_points,
            minimum_score_basis_points: MINIMUM_ASSOCIATION_SCORE_BASIS_POINTS,
            final_score_basis_points: final_score_basis_points(evidence),
        })
        .expect("Task Association fusion explanation is always serializable"),
    ];
    if evidence.intent_matched {
        reasons.push(format!(
            "Task text matched Space Intent fields: {}",
            evidence
                .intent_fields
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(conflict) = intent_conflict_handoff(evidence) {
        reasons.push(
            serde_json::to_string(&conflict)
                .expect("Intent Conflict handoff explanation is always serializable"),
        );
    }
    if let Some(conflict) = intent_scope_conflict(evidence) {
        reasons.push(
            serde_json::to_string(&conflict)
                .expect("Intent Scope Conflict explanation is always serializable"),
        );
    }
    if !evidence.textual_contexts.is_empty() {
        reasons.push(format!(
            "Task text matched {} accepted, injection-safe Context(s)",
            evidence.textual_contexts.len()
        ));
    }
    if !evidence.hint_text.is_empty() {
        reasons.push(format!(
            "Working Intent Hint text matched {} positive FTS channel(s)",
            evidence.hint_text.len()
        ));
    }
    if !evidence.matched_scopes.is_empty() {
        reasons.push(format!(
            "Task applicability matched accepted Context scope: {}",
            evidence
                .matched_scopes
                .iter()
                .map(|scope| format!("{}={}", scope.dimension, scope.value))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !evidence.graph_exact_contexts.is_empty() {
        reasons.push(format!(
            "Resolved current-generation Engineering Artifact associations matched {} Context(s)",
            evidence.graph_exact_contexts.len()
        ));
    }
    if !evidence.relation_contexts.is_empty() {
        reasons.push(format!(
            "Bounded Context Relations reached {} Context(s) within depth {}",
            evidence.relation_contexts.len(),
            DEFAULT_CONTEXT_RELATION_DEPTH
        ));
    }
    reasons
}

#[derive(Debug)]
struct TaskContextCandidate {
    association_rank: usize,
    direct_path_count: usize,
    item: TaskContextItem,
}

#[derive(Debug)]
struct LoadedTaskContexts {
    candidates: Vec<TaskContextCandidate>,
    omitted: Vec<ContextPackOmitted>,
}

#[derive(Debug)]
struct PackedTaskContexts {
    estimated_tokens: usize,
    associations: Vec<TaskSpaceAssociation>,
    items: Vec<TaskContextItem>,
    graph_diagnostics: Vec<TaskGraphDiagnostic>,
    omitted: Vec<ContextPackOmitted>,
}

fn artifact_focus_diagnostics(
    resolved_focus: Option<&ResolvedFocus>,
    reachable: bool,
) -> Vec<TaskGraphDiagnostic> {
    resolved_focus
        .filter(|_| !reachable)
        .map(|focus| TaskGraphDiagnostic {
            kind: TaskGraphDiagnosticKind::ArtifactNotReachableInGraph,
            resolved_focus: focus.clone(),
        })
        .into_iter()
        .collect()
}

fn validate_task_context_request(request: &TaskContextRequest) -> Result<()> {
    request.working_intent.validate()?;
    TaskSignal::validate_collection(&request.task_signals)?;
    if let Some(focus) = &request.resolved_focus {
        focus.validate()?;
    }
    if request.token_budget < MIN_TASK_CONTEXT_TOKEN_BUDGET {
        return Err(invalid(format!(
            "task context token_budget must be at least {MIN_TASK_CONTEXT_TOKEN_BUDGET}"
        )));
    }
    if request.max_spaces == 0 || request.max_spaces > MAX_TASK_MAX_SPACES {
        return Err(invalid(format!(
            "task context max_spaces must be between 1 and {MAX_TASK_MAX_SPACES}"
        )));
    }
    if request.candidate_limit == 0 || request.candidate_limit > MAX_PAGE_SIZE {
        return Err(invalid(format!(
            "task context candidate_limit must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn task_fingerprint(intent: &WorkingIntentSnapshot, signals: &[TaskSignal]) -> Result<String> {
    let mut intent = intent.clone();
    for values in [
        &mut intent.in_scope,
        &mut intent.out_of_scope,
        &mut intent.domains,
        &mut intent.platforms,
        &mut intent.constraints,
        &mut intent.acceptance_conditions,
        &mut intent.artifact_hints,
        &mut intent.interface_hints,
        &mut intent.open_questions,
    ] {
        values.sort();
    }
    let mut signals = signals
        .iter()
        .filter(|signal| signal.kind != TaskSignalKind::Workspace)
        .cloned()
        .collect::<Vec<_>>();
    signals.sort_by(|left, right| {
        signal_kind_name(left.kind)
            .cmp(signal_kind_name(right.kind))
            .then_with(|| left.content.cmp(&right.content))
    });
    let bytes = serde_json::to_vec(&(intent, signals))
        .map_err(|error| invalid(format!("serialize Task fingerprint: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

const fn signal_kind_name(kind: TaskSignalKind) -> &'static str {
    match kind {
        TaskSignalKind::Prompt => "prompt",
        TaskSignalKind::Workspace => "workspace",
        TaskSignalKind::Diff => "diff",
        TaskSignalKind::TestOutcome => "test_outcome",
    }
}

#[allow(clippy::too_many_lines)]
fn load_task_context_candidates(
    connection: &Connection,
    inference: &TaskAssociationInference,
    mode: ContextPackMode,
    candidate_limit: usize,
) -> Result<LoadedTaskContexts> {
    if inference.associations.is_empty() {
        return Ok(LoadedTaskContexts {
            candidates: Vec::new(),
            omitted: Vec::new(),
        });
    }
    let association_rank = inference
        .associations
        .iter()
        .enumerate()
        .map(|(rank, association)| (association.space_id, rank))
        .collect::<BTreeMap<_, _>>();
    let placeholders = std::iter::repeat_n("?", association_rank.len())
        .collect::<Vec<_>>()
        .join(", ");
    let revision_clause = if mode == ContextPackMode::AutomaticInjection {
        SAFE_ACCEPTED_CONTEXT_PREDICATE.to_owned()
    } else {
        "revision.is_head = 1".to_owned()
    };
    let sql = format!(
        "SELECT revision.space_id, revision.context_id, revision.revision_id,
                COALESCE(space.title, ''), revision.kind, {status} AS result_status,
                revision.statement, revision.rationale, revision.applicability_json,
                item.auto_injection_eligible, revision.evidence_completeness
         FROM context_revision AS revision
         JOIN context_item AS item USING(context_id)
         JOIN space_projection AS space USING(space_id)
         WHERE revision.space_id IN ({placeholders})
           AND {revision_clause}
         ORDER BY revision.space_id, revision.context_id, revision.revision_id",
        status = status_expression(),
    );
    let parameters = association_rank
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut statement = connection
        .prepare(&sql)
        .map_err(sql_error("prepare Task Context candidate retrieval"))?;
    let mut rows = statement
        .query(params_from_iter(parameters.iter()))
        .map_err(sql_error("execute Task Context candidate retrieval"))?;
    let mut candidates = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(sql_error("read Task Context candidate row"))?
    {
        if let Some(candidate) =
            task_context_candidate_from_row(connection, row, inference, &association_rank, mode)?
        {
            candidates.push(candidate);
        }
    }
    for graph in inference.graph_contexts.values() {
        if let Some(candidate) = graph_context_candidate(inference, &association_rank, graph, mode)?
        {
            candidates.push(candidate);
        }
    }
    let mut revision_aware =
        BTreeMap::<(SpaceId, ContextId, RevisionId), TaskContextCandidate>::new();
    for mut candidate in candidates {
        let key = (
            candidate.item.context.space_id,
            candidate.item.context.context_id,
            candidate.item.context.revision_id,
        );
        if let Some(existing) = revision_aware.remove(&key) {
            let graph_is_candidate = matches!(
                candidate.item.context.safety_source,
                ContextSafetySource::EngineeringGraphSnapshot { .. }
            );
            let (mut preferred, other) = if graph_is_candidate {
                (candidate, existing)
            } else {
                (existing, candidate)
            };
            preferred
                .item
                .retrieval_paths
                .extend(other.item.retrieval_paths);
            sort_dedup_paths(&mut preferred.item.retrieval_paths);
            preferred.direct_path_count = preferred
                .item
                .retrieval_paths
                .iter()
                .filter(|path| !matches!(path, TaskRetrievalPath::IntentFts { .. }))
                .count();
            candidate = preferred;
        }
        revision_aware.insert(key, candidate);
    }
    let mut candidates = revision_aware.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.association_rank
            .cmp(&right.association_rank)
            .then_with(|| right.direct_path_count.cmp(&left.direct_path_count))
            .then_with(|| {
                left.item
                    .context
                    .context_id
                    .cmp(&right.item.context.context_id)
            })
            .then_with(|| {
                left.item
                    .context
                    .revision_id
                    .cmp(&right.item.context.revision_id)
            })
    });
    let omitted_count = candidates.len().saturating_sub(candidate_limit);
    let omitted_tokens = candidates[candidate_limit.min(candidates.len())..]
        .iter()
        .map(|candidate| serialized_tokens(&candidate.item))
        .sum();
    candidates.truncate(candidate_limit);
    let omitted = (omitted_count > 0)
        .then(|| ContextPackOmitted {
            context_id: None,
            revision_id: None,
            reason: "item_candidate_limit".to_owned(),
            estimated_tokens: omitted_tokens,
            count: omitted_count,
        })
        .into_iter()
        .collect();
    Ok(LoadedTaskContexts {
        candidates,
        omitted,
    })
}

fn graph_context_candidate(
    inference: &TaskAssociationInference,
    association_rank: &BTreeMap<SpaceId, usize>,
    graph: &GraphContextEvidence,
    mode: ContextPackMode,
) -> Result<Option<TaskContextCandidate>> {
    let snapshot = &graph.snapshot;
    let Some(rank) = association_rank.get(&snapshot.space_id) else {
        return Ok(None);
    };
    if mode == ContextPackMode::AutomaticInjection && !snapshot.safety.automatic_injection_eligible
    {
        return Ok(None);
    }
    let Some(artifact_generation) = inference.graph_artifact_generation.clone() else {
        return Err(invariant(
            "Graph Context candidate is missing its Artifact Generation",
        ));
    };
    let paths = task_retrieval_paths(
        inference
            .evidence
            .get(&snapshot.space_id)
            .ok_or_else(|| invariant("Graph Context candidate has no Space association"))?,
        Some(&graph.evidence),
    );
    if paths.is_empty() {
        return Ok(None);
    }
    let evidence = snapshot
        .revision
        .evidence
        .iter()
        .map(|evidence| EvidenceView {
            evidence_id: evidence.evidence_id,
            kind: evidence_type_name(evidence.kind).to_owned(),
            supports: evidence.supports.clone(),
            content: evidence.content.clone(),
            interpretation: evidence.interpretation.clone(),
            limitations: evidence.limitations.clone(),
        })
        .collect::<Vec<_>>();
    let direct_path_count = paths
        .iter()
        .filter(|path| !matches!(path, TaskRetrievalPath::IntentFts { .. }))
        .count();
    Ok(Some(TaskContextCandidate {
        association_rank: *rank,
        direct_path_count,
        item: TaskContextItem {
            association_space_id: snapshot.space_id,
            context: ContextPackItem {
                space_id: snapshot.space_id,
                context_id: snapshot.context_id,
                revision_id: snapshot.revision.revision_id,
                title: snapshot.space_title.clone(),
                kind: snapshot.revision.kind,
                status: graph_context_status(snapshot.status),
                statement: snapshot.revision.statement.clone(),
                rationale: Some(snapshot.revision.rationale.clone()),
                applicability: snapshot.revision.applicability.clone(),
                evidence,
                conflicts: Vec::new(),
                auto_injection_eligible: snapshot.safety.automatic_injection_eligible,
                safety_source: ContextSafetySource::EngineeringGraphSnapshot {
                    context_tree_oid: inference.graph_context_tree_oid.clone(),
                    artifact_generation,
                    context_id: snapshot.context_id,
                    revision_id: snapshot.revision.revision_id,
                    safety: snapshot.safety.clone(),
                },
                match_reason: context_match_reason(
                    Some(&graph.evidence),
                    i64::from(snapshot.evidence_completeness),
                ),
                detail: ContextPackDetail::Full,
            },
            retrieval_paths: paths,
        },
    }))
}

const fn graph_context_status(status: GraphContextStatus) -> ContextStatus {
    match status {
        GraphContextStatus::Candidate => ContextStatus::Candidate,
        GraphContextStatus::Accepted => ContextStatus::Accepted,
        GraphContextStatus::Deprecated => ContextStatus::Deprecated,
        GraphContextStatus::Superseded => ContextStatus::Superseded,
        GraphContextStatus::GovernanceConflict => ContextStatus::GovernanceConflict,
    }
}

const fn evidence_type_name(kind: EvidenceType) -> &'static str {
    match kind {
        EvidenceType::SourceSnapshot => "source_snapshot",
        EvidenceType::ExperimentRecord => "experiment_record",
        EvidenceType::ArtifactSnapshot => "artifact_snapshot",
    }
}

fn task_context_candidate_from_row(
    connection: &Connection,
    row: &rusqlite::Row<'_>,
    inference: &TaskAssociationInference,
    association_rank: &BTreeMap<SpaceId, usize>,
    mode: ContextPackMode,
) -> Result<Option<TaskContextCandidate>> {
    let space_id: SpaceId = parse_id(
        &row.get::<_, String>(0)
            .map_err(sql_error("read Space ID"))?,
    )?;
    let context_id: ContextId = parse_id(
        &row.get::<_, String>(1)
            .map_err(sql_error("read Context ID"))?,
    )?;
    let revision_id: RevisionId = parse_id(
        &row.get::<_, String>(2)
            .map_err(sql_error("read Revision ID"))?,
    )?;
    let Some(space_evidence) = inference.evidence.get(&space_id) else {
        return Ok(None);
    };
    let context_evidence = inference.contexts.get(&(space_id, context_id));
    let inherited = space_evidence.intent_matched
        || space_evidence
            .hint_text
            .keys()
            .any(|channel| channel.target == WorkingIntentHintTarget::SpaceIntentFts);
    if context_evidence.is_none() && !inherited {
        return Ok(None);
    }
    let paths = task_retrieval_paths(space_evidence, context_evidence);
    if paths.is_empty() {
        return Ok(None);
    }
    let title = row.get(3).map_err(sql_error("read Task Context title"))?;
    let kind = parse_kind(
        &row.get::<_, String>(4)
            .map_err(sql_error("read Task Context kind"))?,
    )?;
    let status = parse_status(
        &row.get::<_, String>(5)
            .map_err(sql_error("read Task Context status"))?,
    )?;
    let statement = row
        .get(6)
        .map_err(sql_error("read Task Context statement"))?;
    let rationale = row
        .get(7)
        .map_err(sql_error("read Task Context rationale"))?;
    let applicability = from_json(
        &row.get::<_, String>(8)
            .map_err(sql_error("read Task Context applicability"))?,
    )?;
    let auto_injection_eligible = row
        .get::<_, i64>(9)
        .map_err(sql_error("read Task Context injection eligibility"))?
        != 0;
    let evidence_completeness = row
        .get::<_, i64>(10)
        .map_err(sql_error("read Task Context Evidence completeness"))?;
    let evidence = load_evidence(connection, revision_id)?;
    let conflicts = load_conflicts(connection, context_id, revision_id)?;
    if mode == ContextPackMode::AutomaticInjection
        && (status != ContextStatus::Accepted
            || !auto_injection_eligible
            || evidence.is_empty()
            || !conflicts.is_empty())
    {
        return Ok(None);
    }
    let match_reason = context_match_reason(context_evidence, evidence_completeness);
    let direct_path_count = paths
        .iter()
        .filter(|path| !matches!(path, TaskRetrievalPath::IntentFts { .. }))
        .count();
    Ok(Some(TaskContextCandidate {
        association_rank: *association_rank
            .get(&space_id)
            .expect("candidate Space comes from association set"),
        direct_path_count,
        item: TaskContextItem {
            association_space_id: space_id,
            context: ContextPackItem {
                space_id,
                context_id,
                revision_id,
                title,
                kind,
                status,
                statement,
                rationale: Some(rationale),
                applicability,
                evidence,
                conflicts,
                auto_injection_eligible,
                safety_source: ContextSafetySource::CurrentProjection,
                match_reason,
                detail: ContextPackDetail::Full,
            },
            retrieval_paths: paths,
        },
    }))
}

fn task_retrieval_paths(
    space: &AssociationEvidence,
    context: Option<&AcceptedContextEvidence>,
) -> Vec<TaskRetrievalPath> {
    let mut paths = Vec::new();
    if space.intent_matched {
        paths.push(TaskRetrievalPath::IntentFts {
            matched_fields: space.intent_fields.iter().cloned().collect(),
            matched_tokens: space.intent_tokens.iter().cloned().collect(),
        });
    }
    if !space.excluded_intent_tokens.is_empty() {
        paths.push(TaskRetrievalPath::IntentFts {
            matched_fields: vec![intent_field_name(SpaceIntentField::OutOfScope).to_owned()],
            matched_tokens: space.excluded_intent_tokens.iter().cloned().collect(),
        });
    }
    paths.extend(
        space
            .hint_text
            .iter()
            .filter(|(channel, _)| channel.target == WorkingIntentHintTarget::SpaceIntentFts)
            .map(|(channel, hint)| working_intent_hint_path(space, *channel, hint)),
    );
    if let Some(context) = context {
        paths.extend(context.graph_paths.iter().cloned());
        if context.textual_match {
            paths.push(TaskRetrievalPath::ContextFts {
                matched_fields: context.matched_fields.iter().copied().collect(),
                matched_tokens: context.matched_tokens.iter().cloned().collect(),
            });
        }
        paths.extend(
            context
                .hint_text
                .iter()
                .map(|(channel, hint)| working_intent_hint_path(space, *channel, hint)),
        );
        paths.extend(
            context
                .matched_scopes
                .iter()
                .map(|scope| TaskRetrievalPath::ExactScope {
                    dimension: scope.dimension.clone(),
                    value: scope.value.clone(),
                }),
        );
    }
    sort_dedup_paths(&mut paths);
    paths
}

fn working_intent_hint_path(
    space: &AssociationEvidence,
    channel: WorkingIntentHintTextChannel,
    hint: &HintTextEvidence,
) -> TaskRetrievalPath {
    let fusion_contribution_micros = space
        .channel_features
        .iter()
        .find(|feature| feature.channel == hint_association_channel(channel))
        .map_or(0, |feature| feature.reciprocal_rank_micros);
    TaskRetrievalPath::WorkingIntentHintText {
        explanation: WorkingIntentHintTextExplanation {
            source_field: channel.source_field,
            target: channel.target,
            matched_tokens: hint.matched_tokens.iter().cloned().collect(),
            phrase_match: hint.phrase_match,
            query_token_coverage_basis_points: hint.coverage_basis_points(),
            bm25_micros: hint.bm25.map_or(0, scale_bm25),
            fusion_contribution_micros,
        },
    }
}

fn context_match_reason(
    context: Option<&AcceptedContextEvidence>,
    evidence_completeness: i64,
) -> MatchReason {
    MatchReason {
        matched_fields: context
            .into_iter()
            .flat_map(|value| value.matched_fields.iter().copied())
            .collect(),
        matched_tokens: context
            .into_iter()
            .flat_map(|value| value.matched_tokens.iter().cloned())
            .collect(),
        bm25: context.and_then(|value| value.bm25).unwrap_or(0.0),
        evidence_completeness: u16::try_from(evidence_completeness).unwrap_or(u16::MAX),
        structured_filter_match: true,
    }
}

fn pack_task_context_candidates(
    loaded: LoadedTaskContexts,
    token_budget: usize,
    inference: TaskAssociationInference,
    mut graph_diagnostics: Vec<TaskGraphDiagnostic>,
    omitted_space_count: usize,
    omitted_space_tokens: usize,
) -> PackedTaskContexts {
    let mut omitted = loaded.omitted;
    if omitted_space_count > 0 {
        omitted.push(ContextPackOmitted {
            context_id: None,
            revision_id: None,
            reason: "space_top_k".to_owned(),
            estimated_tokens: omitted_space_tokens,
            count: omitted_space_count,
        });
    }
    let mut associations = inference.associations;
    let mut items = loaded
        .candidates
        .into_iter()
        .map(|candidate| candidate.item)
        .collect::<Vec<_>>();
    let mut detail_omitted = OmissionAggregate::default();
    let mut item_omitted = OmissionAggregate::default();
    let mut space_omitted = OmissionAggregate::default();
    let mut diagnostic_omitted = OmissionAggregate::default();

    loop {
        let current_omitted = task_budget_omissions(
            &omitted,
            detail_omitted,
            item_omitted,
            space_omitted,
            diagnostic_omitted,
        );
        let estimated_tokens = charged_task_context_tokens(
            &associations,
            &items,
            &graph_diagnostics,
            &current_omitted,
        );
        if estimated_tokens <= token_budget {
            return PackedTaskContexts {
                estimated_tokens,
                associations,
                items,
                graph_diagnostics,
                omitted: current_omitted,
            };
        }

        if let Some(space_id) = associations.last().map(|association| association.space_id) {
            if let Some(position) = items.iter().rposition(|item| {
                item.association_space_id == space_id
                    && item.context.detail == ContextPackDetail::Full
            }) {
                let item = &mut items[position];
                let before = serialized_tokens(item);
                item.context.rationale = None;
                item.context.evidence.clear();
                item.context.detail = ContextPackDetail::Summary;
                let after = serialized_tokens(item);
                detail_omitted.add(before.saturating_sub(after));
                continue;
            }
            if let Some(position) = items
                .iter()
                .rposition(|item| item.association_space_id == space_id)
            {
                let item = items.remove(position);
                item_omitted.add(serialized_tokens(&item));
                continue;
            }
            let association = associations.pop().expect("last Association exists");
            space_omitted.add(serialized_tokens(&association));
            continue;
        }

        if let Some(diagnostic) = graph_diagnostics.pop() {
            diagnostic_omitted.add(serialized_tokens(&diagnostic));
            continue;
        }

        let count = current_omitted.iter().map(|item| item.count).sum();
        let compact = vec![ContextPackOmitted {
            context_id: None,
            revision_id: None,
            reason: "omitted".to_owned(),
            estimated_tokens: current_omitted
                .iter()
                .map(|item| item.estimated_tokens)
                .sum(),
            count,
        }];
        return PackedTaskContexts {
            estimated_tokens: charged_task_context_tokens(&[], &[], &[], &compact),
            associations: Vec::new(),
            items: Vec::new(),
            graph_diagnostics: Vec::new(),
            omitted: compact,
        };
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct OmissionAggregate {
    count: usize,
    estimated_tokens: usize,
}

impl OmissionAggregate {
    fn add(&mut self, estimated_tokens: usize) {
        self.count += 1;
        self.estimated_tokens = self.estimated_tokens.saturating_add(estimated_tokens);
    }
}

fn task_budget_omissions(
    base: &[ContextPackOmitted],
    detail: OmissionAggregate,
    item: OmissionAggregate,
    space: OmissionAggregate,
    diagnostic: OmissionAggregate,
) -> Vec<ContextPackOmitted> {
    let mut omitted = base.to_vec();
    for (reason, aggregate) in [
        ("detail_token_budget", detail),
        ("item_token_budget", item),
        ("space_token_budget", space),
        ("diagnostic_token_budget", diagnostic),
    ] {
        if aggregate.count > 0 {
            omitted.push(ContextPackOmitted {
                context_id: None,
                revision_id: None,
                reason: reason.to_owned(),
                estimated_tokens: aggregate.estimated_tokens,
                count: aggregate.count,
            });
        }
    }
    omitted
}

fn charged_task_context_tokens(
    associations: &[TaskSpaceAssociation],
    items: &[TaskContextItem],
    graph_diagnostics: &[TaskGraphDiagnostic],
    omitted: &[ContextPackOmitted],
) -> usize {
    TASK_CONTEXT_ENVELOPE_TOKEN_RESERVE.saturating_add(serialized_tokens(&(
        associations,
        items,
        graph_diagnostics,
        omitted,
    )))
}

/// Recomputes the charged Association, item/path, omission, and deterministic envelope reserve.
#[must_use]
pub fn estimate_task_context_payload_tokens(pack: &TaskContextPack) -> usize {
    charged_task_context_tokens(
        &pack.associations,
        &pack.items,
        &pack.graph_diagnostics,
        &pack.omitted,
    )
}

#[derive(Debug)]
struct SearchPage {
    results: Vec<SearchResult>,
    next_cursor: Option<String>,
    omitted: Vec<SearchOmitted>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CursorPayload {
    version: u8,
    tree_oid: String,
    query_fingerprint: String,
    relevance_bits: u64,
    evidence_completeness: i64,
    context_id: String,
    revision_id: String,
    seen: usize,
}

#[derive(Debug)]
struct RankedRow {
    result: SearchResult,
    relevance: f64,
    evidence_completeness: i64,
}

#[allow(clippy::too_many_lines)]
fn search_in_snapshot(
    connection: &Connection,
    request: &SearchRequest,
    tree_oid: &str,
    eligible_only: bool,
) -> Result<SearchPage> {
    let query_tokens = search_tokens(&request.query);
    let match_expression = fts_match_expression(&query_tokens);
    let fingerprint = query_fingerprint(request, eligible_only)?;
    let cursor = request.cursor.as_deref().map(decode_cursor).transpose()?;
    if let Some(cursor) = &cursor {
        if cursor.version != 1
            || cursor.tree_oid != tree_oid
            || cursor.query_fingerprint != fingerprint
        {
            return Err(invalid(
                "search cursor does not belong to this Tree and query",
            ));
        }
    }

    let (where_sql, base_parameters) =
        search_where(request, match_expression.as_deref(), eligible_only);
    let from_sql = if match_expression.is_some() {
        "context_fts
         JOIN context_revision AS revision USING(revision_id)
         JOIN context_item AS item USING(context_id)
         JOIN space_projection AS space USING(space_id)"
    } else {
        "context_revision AS revision
         JOIN context_item AS item USING(context_id)
         JOIN space_projection AS space USING(space_id)"
    };
    let total_sql = format!("SELECT COUNT(*) FROM {from_sql} WHERE {where_sql}");
    let total = connection
        .query_row(
            &total_sql,
            params_from_iter(base_parameters.iter()),
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error("count search matches"))?;

    let status = status_expression();
    let evidence = evidence_expression();
    let relevance = if match_expression.is_some() {
        "bm25(context_fts, 0.0, 0.0, 10.0, 8.0, 4.0, 2.0)"
    } else {
        "0.0"
    };
    let mut parameters = base_parameters;
    let simple_rank = match_expression.is_none();
    let cursor_sql = if let Some(cursor) = &cursor {
        if simple_rank {
            parameters.extend([
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Text(cursor.context_id.clone()),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Text(cursor.context_id.clone()),
                SqlValue::Text(cursor.revision_id.clone()),
            ]);
            "WHERE evidence_completeness < ?
              OR (evidence_completeness = ? AND context_id > ?)
              OR (evidence_completeness = ? AND context_id = ? AND revision_id > ?)"
        } else {
            let relevance = f64::from_bits(cursor.relevance_bits);
            parameters.extend([
                SqlValue::Real(relevance),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Text(cursor.context_id.clone()),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Text(cursor.context_id.clone()),
                SqlValue::Text(cursor.revision_id.clone()),
            ]);
            "WHERE relevance > ?
          OR (relevance = ? AND evidence_completeness < ?)
          OR (relevance = ? AND evidence_completeness = ? AND context_id > ?)
          OR (relevance = ? AND evidence_completeness = ? AND context_id = ? AND revision_id > ?)"
        }
    } else {
        ""
    };
    parameters.push(SqlValue::Integer(
        i64::try_from(request.page_size + 1).map_err(|_| invalid("page size overflow"))?,
    ));
    let order_sql = if simple_rank {
        "evidence_completeness DESC, context_id ASC, revision_id ASC"
    } else {
        "relevance ASC, evidence_completeness DESC, context_id ASC, revision_id ASC"
    };
    let sql = format!(
        "WITH ranked AS (
           SELECT revision.space_id, revision.context_id, revision.revision_id,
                  COALESCE(space.title, ''), revision.kind, {status} AS result_status,
                  revision.statement, revision.rationale, revision.applicability_json,
                  revision.assumptions_json, revision.recheck_when_json,
                  item.auto_injection_eligible, {relevance} AS relevance,
                  {evidence} AS evidence_completeness
           FROM {from_sql}
           WHERE {where_sql}
         )
         SELECT * FROM ranked {cursor_sql}
         ORDER BY {order_sql}
         LIMIT ?"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(sql_error("prepare stable search"))?;
    let mut rows = statement
        .query(params_from_iter(parameters.iter()))
        .map_err(sql_error("execute stable search"))?;
    let mut ranked = Vec::new();
    while let Some(row) = rows.next().map_err(sql_error("read search row"))? {
        let space_id_text = row
            .get::<_, String>(0)
            .map_err(sql_error("read Space ID"))?;
        let context_id_text = row
            .get::<_, String>(1)
            .map_err(sql_error("read Context ID"))?;
        let revision_id_text = row
            .get::<_, String>(2)
            .map_err(sql_error("read Revision ID"))?;
        let space_id = parse_id(&space_id_text)?;
        let context_id = parse_id(&context_id_text)?;
        let revision_id = parse_id(&revision_id_text)?;
        let title: String = row.get(3).map_err(sql_error("read search title"))?;
        let kind_text: String = row.get(4).map_err(sql_error("read Context kind"))?;
        let status_text: String = row.get(5).map_err(sql_error("read Context status"))?;
        let statement: String = row.get(6).map_err(sql_error("read statement"))?;
        let rationale: String = row.get(7).map_err(sql_error("read rationale"))?;
        let applicability_json = row
            .get::<_, String>(8)
            .map_err(sql_error("read applicability"))?;
        let assumptions_json = row
            .get::<_, String>(9)
            .map_err(sql_error("read assumptions"))?;
        let recheck_when_json = row
            .get::<_, String>(10)
            .map_err(sql_error("read recheck_when"))?;
        let applicability = from_json(&applicability_json)?;
        let assumptions = from_json(&assumptions_json)?;
        let recheck_when = from_json(&recheck_when_json)?;
        let auto_injection_eligible = row
            .get::<_, i64>(11)
            .map_err(sql_error("read injection eligibility"))?
            != 0;
        let row_relevance = row.get(12).map_err(sql_error("read BM25 rank"))?;
        let row_evidence = row
            .get(13)
            .map_err(sql_error("read Evidence completeness"))?;
        let evidence = load_evidence(connection, revision_id)?;
        let conflicts = load_conflicts(connection, context_id, revision_id)?;
        let match_reason = explain_match(
            &query_tokens,
            &title,
            &statement,
            &rationale,
            &evidence,
            row_relevance,
            row_evidence,
        );
        ranked.push(RankedRow {
            result: SearchResult {
                space_id,
                context_id,
                revision_id,
                title,
                kind: parse_kind(&kind_text)?,
                status: parse_status(&status_text)?,
                statement,
                rationale,
                applicability,
                assumptions,
                recheck_when,
                evidence,
                conflicts,
                auto_injection_eligible,
                match_reason,
            },
            relevance: row_relevance,
            evidence_completeness: row_evidence,
        });
    }
    let has_more = ranked.len() > request.page_size;
    ranked.truncate(request.page_size);
    let next_cursor = if has_more {
        ranked
            .last()
            .map(|row| {
                encode_cursor(&CursorPayload {
                    version: 1,
                    tree_oid: tree_oid.to_owned(),
                    query_fingerprint: fingerprint,
                    relevance_bits: row.relevance.to_bits(),
                    evidence_completeness: row.evidence_completeness,
                    context_id: row.result.context_id.to_string(),
                    revision_id: row.result.revision_id.to_string(),
                    seen: cursor.as_ref().map_or(0, |value| value.seen) + ranked.len(),
                })
            })
            .transpose()?
    } else {
        None
    };
    let returned_before = cursor.as_ref().map_or(0, |value| value.seen);
    let remaining = usize::try_from(total)
        .unwrap_or(usize::MAX)
        .saturating_sub(returned_before + ranked.len());
    let omitted = (remaining > 0)
        .then(|| SearchOmitted {
            count: remaining,
            reason: "page_limit".to_owned(),
        })
        .into_iter()
        .collect();
    Ok(SearchPage {
        results: ranked.into_iter().map(|row| row.result).collect(),
        next_cursor,
        omitted,
    })
}

fn search_where(
    request: &SearchRequest,
    match_expression: Option<&str>,
    eligible_only: bool,
) -> (String, Vec<SqlValue>) {
    let mut clauses = Vec::new();
    let mut parameters = Vec::new();
    if let Some(expression) = match_expression {
        clauses.push("context_fts MATCH ?".to_owned());
        parameters.push(SqlValue::Text(expression.to_owned()));
    }
    add_in_filter(
        &mut clauses,
        &mut parameters,
        "revision.space_id",
        request.filters.space_ids.iter().map(ToString::to_string),
    );
    add_in_filter(
        &mut clauses,
        &mut parameters,
        "revision.kind",
        request
            .filters
            .kinds
            .iter()
            .map(|kind| kind_text(*kind).to_owned()),
    );
    add_status_filter(&mut clauses, &mut parameters, &request.filters.statuses);
    for (dimension, values) in [
        ("domain", &request.filters.scope.domains),
        ("platform", &request.filters.scope.platforms),
        ("condition", &request.filters.scope.conditions),
    ] {
        if values.is_empty() {
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM scope AS selected_scope
             WHERE selected_scope.revision_id = revision.revision_id
               AND selected_scope.dimension = ?
               AND selected_scope.value IN ({placeholders}))"
        ));
        parameters.push(SqlValue::Text(dimension.to_owned()));
        parameters.extend(values.iter().cloned().map(SqlValue::Text));
    }
    if eligible_only {
        clauses.push("item.governance_status = 'accepted'".to_owned());
        clauses.push("item.auto_injection_eligible = 1".to_owned());
        clauses.push("revision.lifecycle = 'accepted'".to_owned());
        clauses.push("revision.evidence_completeness >= 750".to_owned());
        clauses.push(
            "EXISTS (SELECT 1 FROM evidence AS required_evidence
                     WHERE required_evidence.revision_id = revision.revision_id
                       AND trim(required_evidence.supports) <> ''
                       AND trim(required_evidence.interpretation) <> ''
                       AND required_evidence.content_json <> '{}')"
                .to_owned(),
        );
    }
    if clauses.is_empty() {
        clauses.push("1 = 1".to_owned());
    }
    (clauses.join(" AND "), parameters)
}

fn add_in_filter(
    clauses: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    expression: &str,
    values: impl Iterator<Item = String>,
) {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    let placeholders = std::iter::repeat_n("?", values.len())
        .collect::<Vec<_>>()
        .join(", ");
    clauses.push(format!("{expression} IN ({placeholders})"));
    parameters.extend(values.into_iter().map(SqlValue::Text));
}

fn add_status_filter(
    clauses: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    statuses: &[ContextStatus],
) {
    if statuses.is_empty() {
        return;
    }
    let mut alternatives = Vec::new();
    for status in statuses {
        if *status == ContextStatus::GovernanceConflict {
            alternatives.push("item.governance_status = 'governance_conflict'".to_owned());
        } else {
            alternatives.push(
                "(item.governance_status <> 'governance_conflict' AND revision.lifecycle = ?)"
                    .to_owned(),
            );
            parameters.push(SqlValue::Text(status.as_str().to_owned()));
        }
    }
    clauses.push(format!("({})", alternatives.join(" OR ")));
}

fn status_expression() -> &'static str {
    "CASE WHEN item.governance_status = 'governance_conflict'
          THEN 'governance_conflict' ELSE revision.lifecycle END"
}

fn evidence_expression() -> &'static str {
    "revision.evidence_completeness"
}

fn load_evidence(connection: &Connection, revision_id: RevisionId) -> Result<Vec<EvidenceView>> {
    let mut statement = connection
        .prepare(
            "SELECT evidence_id, kind, supports, content_json, interpretation, limitations_json
             FROM evidence WHERE revision_id = ? ORDER BY evidence_id",
        )
        .map_err(sql_error("prepare Evidence expansion"))?;
    statement
        .query_map([revision_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(sql_error("read Evidence expansion"))?
        .map(|row| {
            let (evidence_id, kind, supports, content, interpretation, limitations) =
                row.map_err(sql_error("collect Evidence expansion"))?;
            Ok(EvidenceView {
                evidence_id: parse_id(&evidence_id)?,
                kind,
                supports,
                content: from_json(&content)?,
                interpretation,
                limitations: from_json(&limitations)?,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct StoredConflictProjection {
    conflict: StoredConflict,
}

#[derive(Deserialize)]
struct StoredConflict {
    conflict_id: String,
    participants: Vec<StoredParticipant>,
    reason: String,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct StoredParticipant {
    context_id: String,
    revision_id: String,
    publication_id: String,
}

fn load_conflicts(
    connection: &Connection,
    context_id: ContextId,
    revision_id: RevisionId,
) -> Result<Vec<ConflictView>> {
    let mut conflicts = Vec::new();
    let mut semantic = connection
        .prepare(
            "SELECT projection_json FROM semantic_conflict
             WHERE status = 'open' ORDER BY conflict_id",
        )
        .map_err(sql_error("prepare semantic Conflict expansion"))?;
    let projections = semantic
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error("read semantic Conflict expansion"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("collect semantic Conflict expansion"))?;
    for projection in projections {
        let stored: StoredConflictProjection = from_json(&projection)?;
        if !stored.conflict.participants.iter().any(|participant| {
            participant.context_id == context_id.to_string()
                && participant.revision_id == revision_id.to_string()
        }) {
            continue;
        }
        let participants = stored
            .conflict
            .participants
            .into_iter()
            .map(|participant| {
                Ok(ConflictSide {
                    context_id: parse_id(&participant.context_id)?,
                    revision_id: parse_id(&participant.revision_id)?,
                    publication_id: participant.publication_id,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conflicts.push(ConflictView {
            conflict_id: stored.conflict.conflict_id,
            kind: "semantic".to_owned(),
            status: "open".to_owned(),
            reason: stored.conflict.reason,
            participants,
        });
    }

    let mut governance = connection
        .prepare(
            "SELECT conflict_key FROM conflict
             WHERE kind = 'publication' AND status = 'open' AND context_id = ?
             ORDER BY conflict_key",
        )
        .map_err(sql_error("prepare governance Conflict expansion"))?;
    let keys = governance
        .query_map([context_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(sql_error("read governance Conflict expansion"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("collect governance Conflict expansion"))?;
    for key in keys {
        let mut participants_statement = connection
            .prepare(
                "SELECT publication.revision_id, publication.publication_id
                 FROM publication_head
                 JOIN publication USING(publication_id)
                 WHERE publication_head.context_id = ?
                 ORDER BY publication.publication_id",
            )
            .map_err(sql_error("prepare governance Conflict sides"))?;
        let sides = participants_statement
            .query_map([context_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_error("read governance Conflict sides"))?
            .map(|side| {
                let (participant_revision, publication_id) =
                    side.map_err(sql_error("collect governance Conflict sides"))?;
                Ok(ConflictSide {
                    context_id,
                    revision_id: parse_id(&participant_revision)?,
                    publication_id,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conflicts.push(ConflictView {
            conflict_id: key,
            kind: "governance".to_owned(),
            status: "open".to_owned(),
            reason: "multiple publication heads".to_owned(),
            participants: sides,
        });
    }
    Ok(conflicts)
}

fn explain_match(
    query_tokens: &[String],
    title: &str,
    statement: &str,
    rationale: &str,
    evidence: &[EvidenceView],
    bm25: f64,
    evidence_completeness: i64,
) -> MatchReason {
    let wanted = query_tokens.iter().cloned().collect::<BTreeSet<_>>();
    let evidence_text = evidence
        .iter()
        .map(|item| {
            format!(
                "{} {} {} {}",
                item.supports,
                item.content,
                item.interpretation,
                item.limitations.join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let fields = [
        (MatchField::Title, title),
        (MatchField::Statement, statement),
        (MatchField::Rationale, rationale),
        (MatchField::Evidence, evidence_text.as_str()),
    ];
    let mut matched_fields = Vec::new();
    let mut matched_tokens = BTreeSet::new();
    for (field, text) in fields {
        let field_tokens = search_tokens(text).into_iter().collect::<BTreeSet<_>>();
        let intersection = wanted
            .intersection(&field_tokens)
            .cloned()
            .collect::<Vec<_>>();
        if !intersection.is_empty() {
            matched_fields.push(field);
            matched_tokens.extend(intersection);
        }
    }
    MatchReason {
        matched_fields,
        matched_tokens: matched_tokens.into_iter().collect(),
        bm25,
        evidence_completeness: u16::try_from(evidence_completeness).unwrap_or(u16::MAX),
        structured_filter_match: true,
    }
}

fn serialized_tokens(value: &impl Serialize) -> usize {
    estimate_tokens(&serde_json::to_string(value).unwrap_or_default())
}

fn estimate_tokens(text: &str) -> usize {
    let mut tokens: usize = 0;
    let mut non_han_bytes: usize = 0;
    for character in text.chars() {
        if is_han(character) {
            tokens += 1;
            if non_han_bytes > 0 {
                tokens += non_han_bytes.div_ceil(4);
                non_han_bytes = 0;
            }
        } else {
            non_han_bytes += character.len_utf8();
        }
    }
    tokens + non_han_bytes.div_ceil(4)
}

fn is_han(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x323AF
    )
}

fn fts_match_expression(tokens: &[String]) -> Option<String> {
    (!tokens.is_empty()).then(|| {
        tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ")
    })
}

fn fts_or_match_expression(tokens: &[String]) -> Option<String> {
    (!tokens.is_empty()).then(|| {
        tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

fn query_fingerprint(request: &SearchRequest, eligible_only: bool) -> Result<String> {
    let mut request = request.clone();
    request.cursor = None;
    request.page_size = 0;
    let bytes = serde_json::to_vec(&(request, eligible_only))
        .map_err(|error| invalid(format!("serialize search fingerprint: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn encode_cursor(cursor: &CursorPayload) -> Result<String> {
    let bytes = serde_json::to_vec(cursor)
        .map_err(|error| invalid(format!("serialize search cursor: {error}")))?;
    Ok(hex_encode(&bytes))
}

fn decode_cursor(value: &str) -> Result<CursorPayload> {
    let bytes = hex_decode(value)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid search cursor: {error}")))
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        return Err(invalid("invalid search cursor encoding"));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(invalid("invalid search cursor encoding")),
    }
}

fn response_from_page(metadata: IndexMetadata, page: SearchPage) -> SearchResponse {
    SearchResponse {
        indexed_tree_oid: metadata.indexed_tree_oid,
        projection_generation: metadata.projection_generation,
        results: page.results,
        next_cursor: page.next_cursor,
        omitted: page.omitted,
    }
}

fn meta(connection: &Connection, key: &str) -> Result<String> {
    connection
        .query_row("SELECT value FROM meta WHERE key = ?", [key], |row| {
            row.get(0)
        })
        .map_err(sql_error("read QuerySnapshot metadata"))
}

fn validate_search_request(request: &SearchRequest) -> Result<()> {
    if request.page_size == 0 || request.page_size > MAX_PAGE_SIZE {
        return Err(invalid(format!(
            "search page_size must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn parse_kind(value: &str) -> Result<ContextKind> {
    match value {
        "decision" => Ok(ContextKind::Decision),
        "contract" => Ok(ContextKind::Contract),
        "issue" => Ok(ContextKind::Issue),
        "risk" => Ok(ContextKind::Risk),
        "validation" => Ok(ContextKind::Validation),
        "discovery" => Ok(ContextKind::Discovery),
        "progress" => Ok(ContextKind::Progress),
        _ => Err(invariant(format!("unknown indexed Context kind {value}"))),
    }
}

fn kind_text(value: ContextKind) -> &'static str {
    match value {
        ContextKind::Decision => "decision",
        ContextKind::Contract => "contract",
        ContextKind::Issue => "issue",
        ContextKind::Risk => "risk",
        ContextKind::Validation => "validation",
        ContextKind::Discovery => "discovery",
        ContextKind::Progress => "progress",
    }
}

fn parse_status(value: &str) -> Result<ContextStatus> {
    match value {
        "candidate" => Ok(ContextStatus::Candidate),
        "accepted" => Ok(ContextStatus::Accepted),
        "deprecated" => Ok(ContextStatus::Deprecated),
        "superseded" => Ok(ContextStatus::Superseded),
        "governance_conflict" => Ok(ContextStatus::GovernanceConflict),
        _ => Err(invariant(format!("unknown indexed Context status {value}"))),
    }
}

fn parse_id<T>(value: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| invariant(format!("invalid indexed ID {value}: {error}")))
}

fn from_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|error| invariant(format!("invalid indexed JSON: {error}")))
}

fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rusqlite::params;

    use sctx_index::normalize_search_text;

    use super::{
        ContextStatus, ScopeFilter, SearchFilters, SearchRequest, estimate_tokens, hex_decode,
        hex_encode, search_in_snapshot,
    };

    #[test]
    fn cursor_hex_round_trips() {
        let bytes = b"stable cursor";
        assert_eq!(hex_decode(&hex_encode(bytes)).unwrap(), bytes);
    }

    #[test]
    fn token_estimate_charges_han_individually() {
        assert_eq!(estimate_tokens("中文abcd"), 3);
    }

    /// Manual fixed-corpus baseline; run with:
    /// `cargo test --release -p sctx-search warm_search_benchmark_baseline -- --ignored --nocapture`
    #[test]
    #[ignore = "100k-row manual benchmark baseline"]
    #[allow(clippy::too_many_lines)]
    fn warm_search_benchmark_baseline() {
        const ROWS: usize = 100_000;
        const ITERATIONS: usize = 30;
        const P95_LIMIT: Duration = Duration::from_millis(100);
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
                 CREATE TABLE space_projection(space_id TEXT PRIMARY KEY, title TEXT) WITHOUT ROWID;
                 CREATE TABLE context_item(
                   context_id TEXT PRIMARY KEY, space_id TEXT NOT NULL,
                   governance_status TEXT NOT NULL, auto_injection_eligible INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 CREATE TABLE context_revision(
                   revision_id TEXT PRIMARY KEY, context_id TEXT NOT NULL, space_id TEXT NOT NULL,
                   kind TEXT NOT NULL, statement TEXT NOT NULL, rationale TEXT NOT NULL,
                   applicability_json TEXT NOT NULL, assumptions_json TEXT NOT NULL,
                   recheck_when_json TEXT NOT NULL, lifecycle TEXT NOT NULL,
                   evidence_completeness INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 CREATE INDEX context_revision_space_kind_status_idx
                   ON context_revision(space_id, kind, lifecycle, revision_id);
                 CREATE INDEX context_revision_lifecycle_idx
                   ON context_revision(lifecycle, revision_id);
                 CREATE INDEX context_revision_stable_rank_idx
                   ON context_revision(
                     lifecycle, evidence_completeness DESC, context_id, revision_id
                   );
                 CREATE TABLE evidence(
                   evidence_id TEXT PRIMARY KEY, revision_id TEXT NOT NULL, kind TEXT NOT NULL,
                   supports TEXT NOT NULL, content_json TEXT NOT NULL,
                   interpretation TEXT NOT NULL, limitations_json TEXT NOT NULL
                 ) WITHOUT ROWID;
                 CREATE INDEX evidence_revision_idx ON evidence(revision_id, evidence_id);
                 CREATE TABLE scope(
                   revision_id TEXT NOT NULL, dimension TEXT NOT NULL, value TEXT NOT NULL,
                   PRIMARY KEY(revision_id, dimension, value)
                 ) WITHOUT ROWID;
                 CREATE TABLE semantic_conflict(
                   conflict_id TEXT PRIMARY KEY, status TEXT NOT NULL, projection_json TEXT NOT NULL
                 ) WITHOUT ROWID;
                 CREATE TABLE conflict(
                   conflict_key TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL,
                   context_id TEXT
                 ) WITHOUT ROWID;
                 CREATE TABLE publication_head(context_id TEXT, publication_id TEXT);
                 CREATE TABLE publication(publication_id TEXT, revision_id TEXT);
                 CREATE VIRTUAL TABLE context_fts USING fts5(
                   context_id UNINDEXED, revision_id UNINDEXED,
                   title, statement, rationale, evidence
                 );",
            )
            .unwrap();
        let space_id = "spc_00000000-0000-4000-8000-000000000001";
        connection
            .execute(
                "INSERT INTO space_projection VALUES (?, ?)",
                params![space_id, "Benchmark Search Context"],
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        {
            let mut item = transaction
                .prepare("INSERT INTO context_item VALUES (?, ?, 'accepted', 1)")
                .unwrap();
            let mut revision = transaction
                .prepare(
                    "INSERT INTO context_revision VALUES (
                       ?, ?, ?, 'decision', ?, ?, ?, '[]', '[]', 'accepted', 1000
                     )",
                )
                .unwrap();
            let mut evidence = transaction
                .prepare(
                    "INSERT INTO evidence VALUES (
                       ?, ?, 'experiment_record', ?, '{\"actual\":\"passed\"}', ?, '[\"fixture\"]'
                     )",
                )
                .unwrap();
            let mut scope = transaction
                .prepare("INSERT INTO scope VALUES (?, 'domain', 'search')")
                .unwrap();
            let mut fts = transaction
                .prepare("INSERT INTO context_fts VALUES (?, ?, ?, ?, ?, ?)")
                .unwrap();
            for index in 0..ROWS {
                let context_id = format!("ctx_00000000-0000-4000-8000-{index:012x}");
                let revision_id = format!("rev_10000000-0000-4000-8000-{index:012x}");
                let evidence_id = format!("evd_20000000-0000-4000-8000-{index:012x}");
                let matched = index % 10 == 0;
                let statement = if matched {
                    "needle searchResultParser 中文检索"
                } else {
                    "ordinary deterministic context"
                };
                let rationale = "fixed benchmark corpus";
                let evidence_text = "self-contained benchmark evidence";
                item.execute(params![context_id, space_id]).unwrap();
                revision
                    .execute(params![
                        revision_id,
                        context_id,
                        space_id,
                        statement,
                        rationale,
                        "{\"domains\":[\"search\"],\"platforms\":[],\"conditions\":[]}"
                    ])
                    .unwrap();
                evidence
                    .execute(params![
                        evidence_id,
                        revision_id,
                        evidence_text,
                        "complete fixture"
                    ])
                    .unwrap();
                scope.execute(params![revision_id]).unwrap();
                fts.execute(params![
                    context_id,
                    revision_id,
                    normalize_search_text("Benchmark Search Context"),
                    normalize_search_text(statement),
                    normalize_search_text(rationale),
                    normalize_search_text(evidence_text)
                ])
                .unwrap();
            }
        }
        transaction.commit().unwrap();

        let queries = ["needle", "search_result_parser", "中文检索", ""];
        for query in queries {
            let request = SearchRequest {
                query: query.to_owned(),
                filters: SearchFilters {
                    scope: ScopeFilter {
                        domains: vec!["search".to_owned()],
                        ..ScopeFilter::default()
                    },
                    statuses: vec![ContextStatus::Accepted],
                    ..SearchFilters::default()
                },
                page_size: 20,
                ..SearchRequest::default()
            };
            search_in_snapshot(&connection, &request, "benchmark-tree", false).unwrap();
            let mut samples = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let started = Instant::now();
                search_in_snapshot(&connection, &request, "benchmark-tree", false).unwrap();
                samples.push(started.elapsed());
            }
            samples.sort_unstable();
            let p50 = percentile(&samples, 50);
            let p95 = percentile(&samples, 95);
            println!("rows={ROWS} query={query:?} warm_p50={p50:?} warm_p95={p95:?}");
            assert!(
                p95 < P95_LIMIT,
                "100k-row warm Search P95 for {query:?} was {p95:?}, limit {P95_LIMIT:?}"
            );
        }
    }

    fn percentile(samples: &[Duration], percentile: usize) -> Duration {
        let index = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
        samples[index]
    }
}
