//! Structured FTS5 search, Task-to-Space association, and deterministic Context Packs.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    str::FromStr,
    sync::Arc,
};

use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};
use sctx_domain::{
    Applicability, ArtifactAssociationKind, ArtifactKey, ArtifactKind, ContextId, ContextKind,
    ContextRelationKind, EvidenceId, EvidenceType, ReferenceId, RepositoryId, ResolutionStatus,
    ResolvedFocus, RevisionId, SpaceAssociationId, SpaceId, TaskId, TaskSignal, TaskSignalKind,
    TaskSpaceAssociation, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    EngineeringProjection, EngineeringProjectionSnapshot, EngineeringProjectionStore,
    GraphContextSafety, GraphContextSnapshot, GraphContextStatus, MatchBasis,
    ResolvedReferenceProjection,
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
/// How many times a Pack rereads a Graph that moved underneath it before giving the channel up.
const GRAPH_GENERATION_ATTEMPTS: usize = 3;
/// Omission reason for a projection that exists and could not be read.
pub const GRAPH_READ_FAILED_REASON: &str = "graph_read_failed";
/// Omission reason for a projection that was rebuilt underneath every retrieval attempt.
pub const GRAPH_GENERATION_UNSTABLE_REASON: &str = "graph_generation_unstable";
/// How many unresolved-Reference diagnostics one Pack reports before the rest are dropped.
///
/// Three is a report; a list as long as the Repository is a second retrieval result competing
/// with the facts it was supposed to explain.
pub const MAX_UNRESOLVED_FOCUS_DIAGNOSTICS: usize = 3;

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

/// How explicit search matches a multi-token query.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMatchMode {
    /// Any query token may match. Results are ordered by BM25 combined with query token coverage
    /// and truncated below [`RANKED_MIN_COVERAGE_BASIS_POINTS`], so a caller who phrases a known
    /// fact differently still recalls it.
    #[default]
    Ranked,
    /// Every query token must be present in the same revision. This is the strict lookup used to
    /// confirm an exact identifier.
    Exact,
}

/// One stable-cursor search request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub filters: SearchFilters,
    pub page_size: usize,
    pub cursor: Option<String>,
    /// Defaults to [`SearchMatchMode::Ranked`].
    #[serde(default)]
    pub match_mode: SearchMatchMode,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            filters: SearchFilters::default(),
            page_size: DEFAULT_PAGE_SIZE,
            cursor: None,
            match_mode: SearchMatchMode::Ranked,
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
    ProblemView,
    HintText,
}

/// One hit a query token only reached after `token_alias` expansion.
///
/// It names the original query token, the alias that actually occurs in the indexed text, and the
/// alias group both belong to, so a reader can tell a literal match from an expanded one.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct AliasMatch {
    pub token: String,
    pub alias: String,
    pub group_key: String,
}

/// Explainable ranking inputs returned with every hit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchReason {
    pub matched_fields: Vec<MatchField>,
    pub matched_tokens: Vec<String>,
    pub bm25: f64,
    /// Share of the distinct query tokens this revision matched, in basis points. It is the
    /// second ranking key of [`SearchMatchMode::Ranked`] and the truncation input.
    pub coverage_basis_points: u16,
    pub evidence_completeness: u16,
    pub structured_filter_match: bool,
    /// Query tokens that only matched through a `token_alias` expansion. Empty when every hit was
    /// literal, so an unexpanded query serializes exactly as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_via_alias: Vec<AliasMatch>,
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

/// Locally derived Context state that no immutable Git fact carries.
///
/// Every field is a projection or evaluation this machine performed: `supersedes` Relations
/// declared by another accepted revision, the outcome of evaluating structured `recheck_when`
/// entries against the local checkouts, and the configured `[context_ttl]` policy. None of them
/// changes the accepted facts in Git; they only change how the Context is ranked and whether it
/// is eligible for automatic injection.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextDerivedState {
    /// Context whose accepted revision declares `supersedes` on this Context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<ContextId>,
    /// Why the last structured `recheck_when` evaluation considered this Context stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    /// Why the configured Context time-to-live considers this Context historical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_reason: Option<String>,
    /// Product of every ranking multiplier applied to this item after Space fusion, in basis
    /// points. Present only when something demoted it, and only in a Context Pack: an explicit
    /// `search` page has no fused Space score to demote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demotion_basis_points: Option<u16>,
}

impl ContextDerivedState {
    /// Whether nothing was derived, which is the common case.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.superseded_by.is_none()
            && self.stale_reason.is_none()
            && self.historical_reason.is_none()
            && self.demotion_basis_points.is_none()
    }

    /// Superseded and historical Contexts stay searchable and explainable but never inject.
    #[must_use]
    pub const fn blocks_automatic_injection(&self) -> bool {
        self.superseded_by.is_some() || self.historical_reason.is_some()
    }
}

/// Ranking multiplier for a Context that participates in an unresolved semantic conflict.
pub const CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS: u16 = 7_000;

/// Ranking multiplier for a Context whose structured `recheck_when` evaluation fired.
pub const STALE_SCORE_MULTIPLIER_BASIS_POINTS: u16 = 6_000;

/// Explicit Context time-to-live policy evaluated at query time.
///
/// The caller owns the policy because it comes from `config.toml`, and it owns the clock because
/// expiry must be reproducible in tests. `now_unix_seconds` defaults to the wall clock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextTtlSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_unix_seconds: Option<i64>,
}

impl ContextTtlSettings {
    #[must_use]
    const fn lifetime_seconds(&self, kind: ContextKind) -> Option<i64> {
        match kind {
            ContextKind::Validation => self.validation_seconds,
            ContextKind::Progress => self.progress_seconds,
            _ => None,
        }
    }

    fn now(&self) -> i64 {
        self.now_unix_seconds.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
                })
        })
    }

    /// Explains why one accepted Context has outlived its configured lifetime.
    fn historical_reason(&self, kind: ContextKind, accepted_at: Option<i64>) -> Option<String> {
        let lifetime = self.lifetime_seconds(kind)?;
        let accepted_at = accepted_at?;
        let age = self.now().checked_sub(accepted_at)?;
        (age > lifetime).then(|| {
            format!(
                "context_ttl: {} Context published {age}s ago exceeds the configured {lifetime}s lifetime",
                context_kind_name(kind)
            )
        })
    }
}

const fn context_kind_name(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::Decision => "decision",
        ContextKind::Contract => "contract",
        ContextKind::Issue => "issue",
        ContextKind::Risk => "risk",
        ContextKind::Validation => "validation",
        ContextKind::Discovery => "discovery",
        ContextKind::Progress => "progress",
    }
}

/// Search hit for one immutable revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub space_id: SpaceId,
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    /// Context-owned display title derived from the immutable revision statement. Context
    /// revisions carry no stored title, so the first [`CONTEXT_TITLE_MAX_CHARS`] characters of
    /// the statement identify the Context instead of borrowing its Space Intent title.
    pub title: String,
    /// Title of the owning `ContextSpace` Intent head. Reported separately so a Space label is
    /// never mistaken for the Context's own identity.
    pub space_title: String,
    pub kind: ContextKind,
    pub status: ContextStatus,
    pub statement: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub assumptions: Vec<String>,
    pub recheck_when: Vec<String>,
    pub evidence: Vec<EvidenceView>,
    pub conflicts: Vec<ConflictView>,
    /// Locally derived lifecycle state. Superseded, stale, and historical Contexts stay fully
    /// searchable; the derivation is reported instead of hiding the row.
    #[serde(default, skip_serializing_if = "ContextDerivedState::is_empty")]
    pub derived_state: ContextDerivedState,
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
    /// Automatic query-token selection, reported once here rather than repeated inside every
    /// Association. Each Association still names the selection it was matched under, in its
    /// projected form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_token_explanation: Option<AutomaticQueryTokenExplanation>,
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
    ResolvedFocusTextFallback,
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

/// Why one query token was not used for automatic retrieval.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticQueryTokenFilter {
    /// Too short to carry retrievable meaning, independent of corpus size.
    ShortToken,
    /// Dropped by the built-in stop-word table, which only applies while the corpus is too small
    /// for document frequency to discriminate.
    StopWordFallback,
    /// Dropped because the token appears in too large a share of the indexed corpus (low IDF).
    HighDocumentFrequency,
    /// Dropped because rarer tokens already filled the automatic token budget.
    TokenBudget,
}

/// One dropped automatic query token and the exact filter responsible for it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutomaticQueryTokenDrop {
    pub token: String,
    pub filter: AutomaticQueryTokenFilter,
    /// Observed document frequency, present only when the corpus was large enough to measure it.
    pub document_frequency: Option<usize>,
}

/// Typed automatic query-token selection explanation serialized into a [`TaskSpaceAssociation`]
/// reason. It makes IDF filtering and stop-word fallback distinguishable after the fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutomaticQueryTokenExplanation {
    pub document_count: usize,
    pub high_document_frequency_min_documents: usize,
    pub high_document_frequency_threshold_basis_points: usize,
    pub stop_word_fallback_active: bool,
    pub selected_tokens: Vec<String>,
    /// The selected tokens this corpus can actually answer: document frequency above zero, and
    /// not a generic word that answers nothing wherever it appears.
    ///
    /// They are the denominator of the automatic coverage gate, so a question is not held against
    /// the words this repository has simply never written down, only against the words it has.
    #[serde(default)]
    pub answerable_tokens: Vec<String>,
    pub dropped_tokens: Vec<AutomaticQueryTokenDrop>,
    /// Totals that survive the compact projection, which drops the token lists themselves.
    #[serde(default)]
    pub selected_token_count: usize,
    #[serde(default)]
    pub answerable_token_count: usize,
    #[serde(default)]
    pub dropped_token_count: usize,
}

/// Most dropped tokens named in the compact projection of an
/// [`AutomaticQueryTokenExplanation`]. Past a handful the list stops being readable and starts
/// competing with the facts for the same budget.
const COMPACT_QUERY_TOKEN_DROP_LIMIT: usize = 8;

impl AutomaticQueryTokenExplanation {
    /// Recomputes the three totals from the token lists. Every constructor and every merge ends
    /// here, so the counts never disagree with the lists they summarize.
    fn refresh_counts(&mut self) {
        self.selected_token_count = self.selected_tokens.len();
        self.answerable_token_count = self.answerable_tokens.len();
        self.dropped_token_count = self.dropped_tokens.len();
    }

    /// Projects the explanation onto the compact payload: the three totals plus at most
    /// [`COMPACT_QUERY_TOKEN_DROP_LIMIT`] named drops and their filters.
    ///
    /// A compact Pack that returned nothing is the one that most needs this, so the projection is
    /// small enough to be affordable at any budget rather than being dropped when room runs out.
    #[must_use]
    fn compact_projection(&self) -> Self {
        let mut compact = self.clone();
        compact.selected_tokens = Vec::new();
        compact.answerable_tokens = Vec::new();
        compact
            .dropped_tokens
            .truncate(COMPACT_QUERY_TOKEN_DROP_LIMIT);
        compact
    }
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

/// Current organization role through which one Context matched a Task-associated Space.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceAssociationRole {
    Primary,
    Related,
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
///
/// The last two name a Reference that does point at this Focus and did not resolve. They exist
/// because "not reachable" is true of an Artifact nobody ever wrote about and equally true of one
/// whose Reference is a rename away from resolving, and only the second is worth a human's time.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGraphDiagnosticKind {
    ArtifactNotReachableInGraph,
    /// A Reference names this Focus and its locator is absent from the Repository snapshot.
    ArtifactReferenceMissing,
    /// A Reference names this Focus and several Artifacts answer to its locator.
    ArtifactReferenceAmbiguous,
}

/// Budgeted Task-level Graph diagnostic that makes zero-result semantics explicit.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct TaskGraphDiagnostic {
    pub kind: TaskGraphDiagnosticKind,
    pub resolved_focus: ResolvedFocus,
    /// One compact sentence naming what did not resolve. Absent for a kind whose meaning is
    /// exactly its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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

/// Strict text-only explanation used only when no Engineering Graph snapshot is available.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedFocusTextFallbackExplanation {
    pub resolved_focus: ResolvedFocus,
    pub matched_components: Vec<String>,
    pub matched_fields: Vec<MatchField>,
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
    SpaceAssociation {
        association_id: SpaceAssociationId,
        role: SpaceAssociationRole,
        matched_space_id: SpaceId,
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
    ResolvedFocusTextFallback {
        explanation: ResolvedFocusTextFallbackExplanation,
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
    /// Payload shape this Pack was budgeted for. `items` and `compact_items` are never both
    /// populated; the budgeter charges exactly the representation named here.
    pub detail_level: ContextPackDetailLevel,
    pub associations: Vec<TaskSpaceAssociation>,
    /// Compact projection of the surviving associations. Empty under
    /// [`ContextPackDetailLevel::Full`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_associations: Vec<CompactSpaceAssociation>,
    pub items: Vec<TaskContextItem>,
    /// Compact projection of the same budgeted selection. Empty under
    /// [`ContextPackDetailLevel::Full`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_items: Vec<CompactTaskContextItem>,
    pub graph_diagnostics: Vec<TaskGraphDiagnostic>,
    /// Automatic query-token selection for this Task, reported once at the top level so token
    /// filtering stays visible even when no Space association survived. Present under
    /// [`ContextPackDetailLevel::Full`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_token_explanation: Option<AutomaticQueryTokenExplanation>,
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

/// One omission, including enough identity to fetch the Context or the Space explicitly.
///
/// Budget omissions name a Context; retrieval-gate omissions name a Space and carry the numbers
/// the gate decided on, so a Pack that returned nothing still says why.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextPackOmitted {
    pub context_id: Option<ContextId>,
    pub revision_id: Option<RevisionId>,
    /// Display title of the omitted Context or Space. Present whenever the omission names one of
    /// them, so an Agent can decide whether what is missing is worth an explicit read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Space this omission is about. Present on every omission that drops a whole Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<SpaceId>,
    pub reason: String,
    pub estimated_tokens: usize,
    pub count: usize,
    /// Coverage the automatic text gate measured, in basis points of the answerable query tokens.
    /// Present only on the gate's own reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_basis_points: Option<u16>,
    /// Query tokens this corpus can match at all, the coverage denominator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answerable_tokens: Option<usize>,
    /// Query tokens automatic retrieval selected, answerable or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_tokens: Option<usize>,
    /// Independent text channels that matched this Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_channel_count: Option<usize>,
    /// Free text naming what went wrong, for the omissions that report a degraded channel rather
    /// than a dropped Context. Carried only under [`ContextPackMode::Explicit`]: an automatic
    /// injection gets the reason, which is what it can act on, and not the storage error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Payload shape requested for one Task Context Pack.
///
/// `Compact` keeps only the fields an Agent needs to inherit a fact (identity, title, statement,
/// applicability conditions, Evidence summaries, Relations, and short reasons). `Full` keeps the
/// complete explainable payload, including per-item Retrieval Paths, match reasons, and the
/// authoritative safety source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPackDetailLevel {
    #[default]
    Compact,
    Full,
}

/// Maximum characters retained from one Evidence summary in a compact Context Pack.
pub const COMPACT_EVIDENCE_SUMMARY_MAX_CHARS: usize = 200;

/// Maximum one-sentence reasons attached to one compact Context Pack item.
pub const COMPACT_ITEM_REASON_LIMIT: usize = 3;

/// Maximum Engineering Reference locations attached to one compact Evidence entry.
pub const COMPACT_EVIDENCE_LOCATION_LIMIT: usize = 4;

/// Share of a compact token budget reserved for Context items before anything else is packed.
///
/// A real payload spent more than half its budget on Space explanations and omission notices and
/// returned two Contexts out of ten. The facts are the payload; the explanations are the margin.
pub const COMPACT_ITEM_BUDGET_BASIS_POINTS: usize = 7_000;

/// Compact items the budgeter refuses to drop, even when a single item exceeds its share.
pub const COMPACT_MIN_ITEMS: usize = 3;

/// Evidence summary length one oversized item is squeezed to so it still reaches the Agent.
pub const COMPACT_SQUEEZED_EVIDENCE_SUMMARY_MAX_CHARS: usize = 120;

/// Human-readable Space reasons kept in one compact association.
pub const COMPACT_ASSOCIATION_REASON_LIMIT: usize = 2;

/// Individually named omissions in a compact payload; the rest collapse into one counted entry.
pub const COMPACT_NAMED_OMISSION_LIMIT: usize = 5;

/// Title characters kept on one named compact omission.
pub const COMPACT_OMITTED_TITLE_MAX_CHARS: usize = 40;

/// Retrieval channel names kept on one compact item.
pub const COMPACT_RETRIEVAL_CHANNEL_LIMIT: usize = 6;

/// Bounded Evidence projection used by [`ContextPackDetailLevel::Compact`]. The Evidence identity
/// and full content stay reachable through `context_get`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactEvidenceView {
    pub kind: String,
    pub summary: String,
    /// `<repository_id>:<repository-relative path>` for every Engineering Reference recorded on the
    /// same immutable revision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<String>,
}

/// Compact Space association: which Space the facts came from, how confident the match was, and a
/// sentence or two of why.
///
/// Everything an Agent cannot act on is dropped. `relation_paths` in particular carried a whole
/// Relation rationale per hop and, on a real payload, cost more characters than the Contexts it
/// was explaining; the same Relations already reach the Agent on `CompactTaskContextItem`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactSpaceAssociation {
    pub space_id: SpaceId,
    /// Intent-head title of the Space, so the Agent can name the Space without a second call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub score: f64,
    /// True when the Space is still the server's provisional proposal rather than a curated one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provisional: bool,
    pub reasons: Vec<String>,
}

/// Space headline fields the compact payload needs but a [`TaskSpaceAssociation`] does not carry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SpaceHeader {
    title: Option<String>,
    provisional: bool,
}

/// One active outgoing Context Relation, reduced to kind and target identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactContextRelation {
    pub kind: ContextRelationKind,
    pub target_context_id: ContextId,
}

/// Compact Context Pack item. It carries the inheritable fact and drops every ranking, fusion, and
/// provenance channel; `ContextPackDetailLevel::Full` remains available for the explainable form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactTaskContextItem {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub space_id: SpaceId,
    pub kind: ContextKind,
    pub status: ContextStatus,
    pub title: String,
    pub statement: String,
    /// Only `applicability.conditions`; the inherited domain/platform dimensions stay in `full`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<String>,
    pub evidence: Vec<CompactEvidenceView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<CompactContextRelation>,
    /// Unresolved conflict sides are never dropped: hiding them would make a compact payload less
    /// safe than the full one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictView>,
    /// Locally derived lifecycle state, never dropped for the same reason conflicts are not.
    #[serde(default, skip_serializing_if = "ContextDerivedState::is_empty")]
    pub derived_state: ContextDerivedState,
    /// Deduplicated `retrieval_paths[].source` names that produced this item, at most
    /// [`COMPACT_RETRIEVAL_CHANNEL_LIMIT`] of them.
    ///
    /// The full explanation carries whole path payloads; the channel names alone are cheap and
    /// still answer the only question a compact reader asks of them: which route found this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retrieval_channels: Vec<String>,
    pub why: Vec<String>,
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
    /// Context-owned display title derived from the immutable revision statement.
    pub title: String,
    /// Title of the owning `ContextSpace` Intent head.
    pub space_title: String,
    pub kind: ContextKind,
    pub status: ContextStatus,
    pub statement: String,
    pub rationale: Option<String>,
    pub applicability: Applicability,
    pub evidence: Vec<EvidenceView>,
    pub conflicts: Vec<ConflictView>,
    #[serde(default, skip_serializing_if = "ContextDerivedState::is_empty")]
    pub derived_state: ContextDerivedState,
    pub auto_injection_eligible: bool,
    pub safety_source: ContextSafetySource,
    pub match_reason: MatchReason,
    /// How earlier Tasks used this Context after it was injected into them. Empty whenever no
    /// usage prior is attached, and never serialized in that case.
    #[serde(default, skip_serializing_if = "ContextUsageCounts::is_empty")]
    pub usage: ContextUsageCounts,
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

/// How earlier Tasks used one Context after it was injected into them.
///
/// The counts are installation-local derived state: they come from the disposable Task Runtime,
/// never from the Context Store, and they are absent whenever no usage source is attached.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextUsageCounts {
    /// Tasks whose Checkpoint restated this Context.
    pub reused: u32,
    /// Tasks that had it injected and checkpointed nothing resembling it.
    pub ignored: u32,
    /// Tasks that confirmed a Context contradicting it.
    pub refuted: u32,
}

impl ContextUsageCounts {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.reused == 0 && self.ignored == 0 && self.refuted == 0
    }
}

/// Optional prior that reports how earlier Tasks used the retrieved Contexts.
///
/// The knowledge index cannot answer this: injection outcomes live in installation-local Runtime
/// state that is never projected from Git. The engine therefore takes the prior as an injected
/// read-only boundary and behaves exactly as before when none is attached.
pub trait UsagePriorSource: fmt::Debug + Send + Sync {
    /// Returns the recorded counts of the requested Contexts. A Context with no recorded usage
    /// may be absent from the map. Implementations degrade to an empty map instead of failing:
    /// the prior is advisory and must never turn a retrieval into an error.
    fn usage_counts(&self, context_ids: &[ContextId]) -> BTreeMap<ContextId, ContextUsageCounts>;
}

/// Score multiplier for a Context at least one earlier Task's Checkpoint restated.
pub const USAGE_REUSED_BONUS_BASIS_POINTS: u16 = 11_500;

/// Score multiplier for a Context repeatedly injected and never restated.
pub const USAGE_IGNORED_PENALTY_BASIS_POINTS: u16 = 9_000;

/// Tasks that must have ignored a Context before the ignore penalty applies.
pub const USAGE_IGNORED_PENALTY_MINIMUM_TASKS: u32 = 3;

/// Query boundary that always delegates reads to one [`sctx_index::QuerySnapshot`] transaction.
#[derive(Clone, Debug)]
pub struct SearchEngine {
    index: ProjectionIndex,
    engineering_graph: Option<EngineeringProjectionStore>,
    context_ttl: ContextTtlSettings,
    /// Installation-local prior on how earlier Tasks used each Context. `None` keeps ranking
    /// identical to an installation that never recorded an injection.
    usage_prior: Option<Arc<dyn UsagePriorSource>>,
}

impl SearchEngine {
    #[must_use]
    pub const fn new(index: ProjectionIndex) -> Self {
        Self {
            index,
            engineering_graph: None,
            context_ttl: ContextTtlSettings {
                validation_seconds: None,
                progress_seconds: None,
                now_unix_seconds: None,
            },
            usage_prior: None,
        }
    }

    /// Attaches the installation-local usage prior read from Task Runtime state.
    ///
    /// It only reweights an already fused Context Pack item; it never admits a Context the
    /// safety and eligibility rules excluded, and it is applied after every demotion so a
    /// conflicting or stale Context cannot be promoted back by past reuse.
    #[must_use]
    pub fn with_usage_prior(mut self, usage_prior: Arc<dyn UsagePriorSource>) -> Self {
        self.usage_prior = Some(usage_prior);
        self
    }

    /// Applies the explicit `[context_ttl]` policy read from `config.toml`.
    ///
    /// An expired Context becomes `historical`: it stays searchable and explainable and reports
    /// why, but it is never injected automatically.
    #[must_use]
    pub const fn with_context_ttl(mut self, context_ttl: ContextTtlSettings) -> Self {
        self.context_ttl = context_ttl;
        self
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
            context_ttl: ContextTtlSettings {
                validation_seconds: None,
                progress_seconds: None,
                now_unix_seconds: None,
            },
            usage_prior: None,
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
        let mut graph_read = GraphSnapshotRead::Absent;
        for _attempt in 0..GRAPH_GENERATION_ATTEMPTS {
            graph_read = self.read_graph_snapshot();
            let graph_snapshot = graph_read.snapshot();
            let snapshot = self.index.query_snapshot(|connection| {
                let graph = graph_projection(graph_snapshot);
                infer_task_space_associations(
                    connection,
                    task_id,
                    &query_tokens,
                    &query_phrases,
                    &hint_queries,
                    &scope_targets,
                    resolved_focus,
                    graph,
                    graph_snapshot.and_then(|snapshot| snapshot.context_tree_oid.as_deref()),
                    resolved_focus.is_some() && graph.is_none(),
                    ContextPackMode::AutomaticInjection,
                )
            })?;
            if graph_snapshot.is_some() && !self.graph_snapshot_unchanged(graph_snapshot) {
                continue;
            }
            return Ok(TaskSpaceAssociationsResponse {
                indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
                projection_generation: snapshot.metadata.projection_generation,
                task_id,
                associations: snapshot.data.associations,
                query_token_explanation: Some(snapshot.data.query_token_explanation),
            });
        }
        // A Graph being rebuilt underneath a reader is a race this reader lost, not a broken
        // installation, and this response has no field in which to say so. Text recall is what it
        // would have fallen back to had the Graph simply been absent, so that is what it returns.
        drop(graph_read);
        let snapshot = self.index.query_snapshot(|connection| {
            infer_task_space_associations(
                connection,
                task_id,
                &query_tokens,
                &query_phrases,
                &hint_queries,
                &scope_targets,
                resolved_focus,
                None,
                None,
                resolved_focus.is_some(),
                ContextPackMode::AutomaticInjection,
            )
        })?;
        Ok(TaskSpaceAssociationsResponse {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            task_id,
            associations: snapshot.data.associations,
            query_token_explanation: Some(snapshot.data.query_token_explanation),
        })
    }

    /// Reads the immutable statements of the named Context revisions.
    ///
    /// This is the cheap read behind the injection/Claim comparison: it answers "what text did we
    /// actually hand this Task" from the projection index alone, without reducing the Event log
    /// into a full domain snapshot. Revisions the index no longer carries are absent.
    ///
    /// # Errors
    ///
    /// Returns storage errors propagated by index synchronization and snapshot reads.
    pub fn context_statements(
        &self,
        revisions: &[(ContextId, RevisionId)],
    ) -> Result<BTreeMap<ContextId, String>> {
        let requested = revisions.iter().copied().collect::<BTreeSet<_>>();
        if requested.is_empty() {
            return Ok(BTreeMap::new());
        }
        let snapshot = self.index.query_snapshot(|connection| {
            let mut statements = BTreeMap::new();
            let mut statement = connection
                .prepare(
                    "SELECT statement FROM context_revision
                     WHERE context_id = ?1 AND revision_id = ?2",
                )
                .map_err(sql_error("prepare Context statement read"))?;
            for (context_id, revision_id) in &requested {
                let text = statement
                    .query_row(
                        rusqlite::params![context_id.to_string(), revision_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(sql_error("read Context statement"))?;
                if let Some(text) = text {
                    statements.insert(*context_id, text);
                }
            }
            Ok(statements)
        })?;
        Ok(snapshot.data)
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
        self.task_context_pack_with_detail(request, ContextPackDetailLevel::Full)
    }

    /// Builds the same Task-first Context Pack in one explicit payload shape.
    ///
    /// [`ContextPackDetailLevel::Compact`] budgets and returns only the inheritable fact fields,
    /// so a small `token_budget` carries several Contexts instead of one explainable Context.
    ///
    /// # Errors
    ///
    /// Returns the same input and storage errors as [`Self::task_context_pack`].
    pub fn task_context_pack_with_detail(
        &self,
        request: &TaskContextRequest,
        detail_level: ContextPackDetailLevel,
    ) -> Result<TaskContextPack> {
        validate_task_context_request(request)?;
        let fingerprint = task_fingerprint(&request.working_intent, &request.task_signals)?;
        let query_tokens = association_query_tokens(&request.working_intent, &request.task_signals);
        let query_phrases =
            association_query_phrases(&request.working_intent, &request.task_signals);
        let hint_queries = working_intent_hint_queries(&request.working_intent);
        let scope_targets = ScopeTargets::from_intent(&request.working_intent);
        let plan = TaskContextPlan {
            request,
            detail_level,
            fingerprint: &fingerprint,
            query_tokens: &query_tokens,
            query_phrases: &query_phrases,
            hint_queries: &hint_queries,
            scope_targets: &scope_targets,
        };
        for _attempt in 0..GRAPH_GENERATION_ATTEMPTS {
            let graph_read = self.read_graph_snapshot();
            let graph_snapshot = graph_read.snapshot();
            // An unreadable projection is not the same fact as an absent one, and only one of the
            // two is worth a line of an Agent's budget. `Absent` stays silent; a read failure is
            // reported and the Pack is built the way it would have been built without a Graph.
            let degraded = graph_read
                .failure()
                .map(|detail| {
                    vec![degraded_graph_omission(
                        GRAPH_READ_FAILED_REASON,
                        detail,
                        request.mode,
                    )]
                })
                .unwrap_or_default();
            let pack = self.pack_task_context_attempt(&plan, graph_snapshot, &degraded)?;
            if graph_snapshot.is_some() && !self.graph_snapshot_unchanged(graph_snapshot) {
                continue;
            }
            return Ok(pack);
        }
        // The Graph kept moving underneath every attempt. That is a race this reader lost, not a
        // reason to fail a retrieval the caller can still be served from text: the Pack degrades
        // to the recall it would have had with no Graph at all, and says which channel it lost.
        self.pack_task_context_attempt(
            &plan,
            None,
            &[degraded_graph_omission(
                GRAPH_GENERATION_UNSTABLE_REASON,
                "the Engineering Graph generation changed under every retrieval attempt",
                request.mode,
            )],
        )
    }

    /// One complete Pack build against one fixed view of the Engineering Graph.
    fn pack_task_context_attempt(
        &self,
        plan: &TaskContextPlan<'_>,
        graph_snapshot: Option<&EngineeringProjectionSnapshot>,
        degraded: &[ContextPackOmitted],
    ) -> Result<TaskContextPack> {
        let request = plan.request;
        let detail_level = plan.detail_level;
        let snapshot = self.index.query_snapshot(|connection| {
            let graph = graph_projection(graph_snapshot);
            let mut inference = infer_task_space_associations(
                connection,
                request.task_id,
                plan.query_tokens,
                plan.query_phrases,
                plan.hint_queries,
                plan.scope_targets,
                request.resolved_focus.as_ref(),
                graph,
                graph_snapshot.and_then(|snapshot| snapshot.context_tree_oid.as_deref()),
                request.resolved_focus.is_some() && graph.is_none(),
                request.mode,
            )?;
            let mut space_omissions = space_top_k_omissions(
                connection,
                &inference.associations[request.max_spaces.min(inference.associations.len())..],
            )?;
            space_omissions.extend(degraded.iter().cloned());
            inference.associations.truncate(request.max_spaces);
            let candidates = load_task_context_candidates(
                connection,
                &inference,
                request.mode,
                request.candidate_limit,
                detail_level,
                &self.context_ttl,
                self.usage_prior.as_deref(),
            )?;
            let graph_diagnostics = artifact_focus_diagnostics(
                request.resolved_focus.as_ref(),
                inference.focus_reachable,
                &inference.unresolved_focus_diagnostics,
            );
            Ok(pack_task_context_candidates(
                candidates,
                request.token_budget,
                inference,
                graph_diagnostics,
                space_omissions,
                detail_level,
            ))
        })?;
        Ok(TaskContextPack {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            artifact_generation: graph_projection(graph_snapshot)
                .map(|graph| graph.artifact_generation.clone()),
            graph_context_tree_oid: graph_snapshot
                .and_then(|snapshot| snapshot.context_tree_oid.clone()),
            task_id: request.task_id,
            task_fingerprint: plan.fingerprint.to_owned(),
            token_budget: request.token_budget,
            estimated_tokens: snapshot.data.estimated_tokens,
            mode: request.mode,
            detail_level,
            associations: snapshot.data.associations,
            compact_associations: snapshot.data.compact_associations,
            items: snapshot.data.items,
            compact_items: snapshot.data.compact_items,
            graph_diagnostics: snapshot.data.graph_diagnostics,
            query_token_explanation: snapshot.data.query_token_explanation,
            omitted: snapshot.data.omitted,
        })
    }

    /// Reads the historical Engineering projection, keeping "there is none" and "it could not be
    /// read" apart.
    ///
    /// They used to be the same `None`, which made a corrupt or unreadable projection look exactly
    /// like an installation that had never built one -- and the second is the ordinary case, so
    /// the first went unreported forever.
    fn read_graph_snapshot(&self) -> GraphSnapshotRead {
        let Some(store) = self.engineering_graph.as_ref() else {
            return GraphSnapshotRead::Absent;
        };
        match store.read_snapshot() {
            Ok(Some(snapshot)) => GraphSnapshotRead::Available(snapshot),
            Ok(None) => GraphSnapshotRead::Absent,
            Err(error) => GraphSnapshotRead::Failed(error.to_string()),
        }
    }

    fn graph_snapshot_unchanged(&self, before: Option<&EngineeringProjectionSnapshot>) -> bool {
        let after = self.read_graph_snapshot();
        graph_snapshot_identity(before) == graph_snapshot_identity(after.snapshot())
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
            search_in_snapshot(connection, request, &tree_oid, false, &self.context_ttl)
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
                let page =
                    search_in_snapshot(connection, &request, &tree_oid, false, &self.context_ttl)?;
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

/// Everything one Pack build needs that does not change between Graph read attempts.
struct TaskContextPlan<'a> {
    request: &'a TaskContextRequest,
    detail_level: ContextPackDetailLevel,
    fingerprint: &'a str,
    query_tokens: &'a [String],
    query_phrases: &'a [String],
    hint_queries: &'a [WorkingIntentHintQuery],
    scope_targets: &'a ScopeTargets,
}

/// One attempt to read the historical Engineering projection.
enum GraphSnapshotRead {
    /// No projection store is configured, or it holds no projection yet. The ordinary case for an
    /// installation that has never scanned a Repository, and never reported.
    Absent,
    Available(EngineeringProjectionSnapshot),
    /// A projection exists and could not be read. Reported, never fatal.
    Failed(String),
}

impl GraphSnapshotRead {
    const fn snapshot(&self) -> Option<&EngineeringProjectionSnapshot> {
        match self {
            Self::Available(snapshot) => Some(snapshot),
            Self::Absent | Self::Failed(_) => None,
        }
    }

    fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(detail) => Some(detail),
            Self::Absent | Self::Available(_) => None,
        }
    }
}

/// Reports one Graph channel this Pack could not use.
///
/// The reason is machine-readable and reaches every caller; the free-text detail reaches only an
/// explicit read, because an automatic injection can act on "the Graph was unreadable" and has no
/// use for the storage error that said so.
fn degraded_graph_omission(
    reason: &str,
    detail: &str,
    mode: ContextPackMode,
) -> ContextPackOmitted {
    ContextPackOmitted {
        reason: reason.to_owned(),
        count: 1,
        detail: (mode == ContextPackMode::Explicit).then(|| detail.to_owned()),
        ..ContextPackOmitted::default()
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

const MAX_AUTOMATIC_QUERY_TOKENS: usize = 64;
const AUTOMATIC_TEXT_COVERAGE_THRESHOLD_BASIS_POINTS: u16 = 6_000;
/// Smallest share of the selected query tokens the corpus has to be able to answer at all before
/// the answerable coverage denominator is trusted on its own.
///
/// Dividing by the answerable tokens is what lets a real question through: the few words of it
/// this repository has written down are the whole of what it can be asked. The same division
/// hands a query the corpus barely recognizes a perfect score off one incidental word, which is
/// exactly the noise an automatic channel must never inject. Below this ratio the gate stops
/// believing coverage and falls back to the multi-channel evidence it shares with every other
/// path.
const AUTOMATIC_MIN_ANSWERABLE_RATIO_BASIS_POINTS: usize = 2_500;
/// Fewest answerable tokens the coverage gate is willing to divide by, relaxed when the query
/// selected fewer tokens than this in the first place (an identifier lookup is one token and is
/// meant to pass).
const AUTOMATIC_MIN_ANSWERABLE_QUERY_TOKENS: usize = 2;
/// How many plain query tokens one identifier-channel token is worth in the automatic coverage
/// gate.
///
/// A Context spells the identifiers it names verbatim whatever language its prose is in, so an
/// English question asked of a Chinese knowledge base can only ever land on that channel: two of
/// its six or seven tokens match, coverage reads 30% and
/// [`AUTOMATIC_TEXT_COVERAGE_THRESHOLD_BASIS_POINTS`] rejects an association that explicit search
/// ranks first. Weighting those tokens says what the plain count cannot: naming the artifact is
/// worth more than sharing prose.
const AUTOMATIC_IDENTIFIER_TOKEN_COVERAGE_WEIGHT: usize = 3;
/// Fewest identifier-channel tokens before the weighting applies. One shared word (`service`,
/// `manager`) is vocabulary that happens to occur inside some identifier; two words of the same
/// `identifier_split` group are the query naming that identifier.
const AUTOMATIC_MIN_IDENTIFIER_QUERY_TOKENS: usize = 2;
/// Smallest corpus that lets observed document frequency stand in for the built-in stop-word
/// table. Below it the table is the only available fallback; at or above it the corpus decides,
/// so real domain vocabulary such as `search` stays eligible.
const AUTOMATIC_HIGH_DF_MIN_DOCUMENTS: usize = 5;
/// Smallest corpus in which a high document frequency is evidence of a generic word rather than
/// of a small fixture. Below it document frequency only orders tokens and never drops one.
const AUTOMATIC_HIGH_DF_DROP_MIN_DOCUMENTS: usize = 20;
const AUTOMATIC_HIGH_DF_THRESHOLD_BASIS_POINTS: usize = 5_000;
/// Number of rarest tokens that are never dropped for being merely frequent. It keeps a
/// single-token or short intent intact, because dropping its only discriminating word retrieves
/// nothing at all.
const AUTOMATIC_MIN_RETAINED_QUERY_TOKENS: usize = 8;
/// Document frequency at which a token selects essentially the whole corpus and therefore carries
/// no retrieval signal at all. Such a token is dropped even inside the retained floor, because
/// keeping it would turn a bare generic intent into an unbounded automatic injection.
const AUTOMATIC_UNIVERSAL_DF_THRESHOLD_BASIS_POINTS: usize = 9_000;

/// Deterministic query-token selection for automatic injection plus its explanation.
struct AutomaticTokenSelection {
    tokens: Vec<String>,
    /// The subset of `tokens` whose document frequency is above zero.
    answerable: Vec<String>,
    explanation: AutomaticQueryTokenExplanation,
}

/// Denominator of every automatic text-coverage decision.
///
/// `selected` is what automatic retrieval queried with; `answerable` is the part of it this
/// corpus can match at all. Coverage divides by `answerable`, and
/// [`Self::answerable_ratio_sufficient`] keeps that smaller denominator from turning a query the
/// corpus barely recognizes into full coverage.
#[derive(Clone, Debug, Default)]
struct AutomaticCoverageBasis {
    selected: Vec<String>,
    answerable: Vec<String>,
}

impl AutomaticCoverageBasis {
    fn from_selection(selection: &AutomaticTokenSelection) -> Self {
        Self {
            selected: selection.tokens.clone(),
            answerable: selection.answerable.clone(),
        }
    }

    /// The same basis with the answerable set widened back to every selected token.
    ///
    /// Relation expansion is the one consumer that keeps the stricter denominator. A seed does
    /// not only rank itself: it pulls whole Spaces in over a
    /// [`CONTEXT_RELATION_CHANNEL_WEIGHT`] channel, and that weight was calibrated against the
    /// seed set the wider denominator produces. Widening the seeds is a separate decision from
    /// fixing how coverage is measured, and taking both at once lets a Space reached only by a
    /// hop out of the answering Space's Context outrank the Space that answered.
    fn selected_only(&self) -> Self {
        Self {
            selected: self.selected.clone(),
            answerable: self.selected.clone(),
        }
    }

    /// The coverage denominator. A matched token is by construction answerable, so intersecting a
    /// matched set with this list keeps the same numerator the selected list produced and only
    /// divides it by what the corpus could actually answer.
    fn tokens(&self) -> &[String] {
        &self.answerable
    }

    fn selected_count(&self) -> usize {
        self.selected.len()
    }

    fn answerable_count(&self) -> usize {
        self.answerable.len()
    }

    /// True when the answerable denominator is broad enough for coverage to mean "this Space
    /// answered the question" instead of "the corpus recognized one word of it".
    fn answerable_ratio_sufficient(&self) -> bool {
        if self.answerable.is_empty() {
            return false;
        }
        if self.selected.len() < AUTOMATIC_MIN_ANSWERABLE_QUERY_TOKENS {
            return true;
        }
        if self.answerable.len() < AUTOMATIC_MIN_ANSWERABLE_QUERY_TOKENS {
            return false;
        }
        self.answerable
            .len()
            .saturating_mul(BASIS_POINTS_SCALE)
            .checked_div(self.selected.len())
            .unwrap_or(0)
            >= AUTOMATIC_MIN_ANSWERABLE_RATIO_BASIS_POINTS
    }
}

/// What the automatic text gate decided about one Space, and the numbers it decided on. The
/// numbers are what a zero-result Pack reports as its omission.
#[derive(Clone, Copy, Debug)]
struct AutomaticTextGate {
    eligible: bool,
    coverage_basis_points: u16,
    text_channel_count: usize,
    /// True when coverage alone would have passed and only
    /// [`AutomaticCoverageBasis::answerable_ratio_sufficient`] held it back.
    blocked_by_answerable_ratio: bool,
}

impl AutomaticTextGate {
    const ELIGIBLE: Self = Self {
        eligible: true,
        coverage_basis_points: 0,
        text_channel_count: 0,
        blocked_by_answerable_ratio: false,
    };

    /// Names the omission reason a rejected gate reports.
    fn omission_reason(self) -> &'static str {
        if self.blocked_by_answerable_ratio {
            "low_answerable_ratio"
        } else {
            "automatic_text_ineligible"
        }
    }
}

/// Passes every query token straight through, unmeasured.
///
/// Explicit retrieval never pays for the per-token frequency probes, so it cannot know which
/// tokens this corpus can answer. Every selected token therefore stays answerable and the
/// coverage denominator is exactly the one explicit ranking already had.
fn explicit_token_selection(tokens: &[String]) -> AutomaticTokenSelection {
    let mut explanation = AutomaticQueryTokenExplanation {
        document_count: 0,
        high_document_frequency_min_documents: AUTOMATIC_HIGH_DF_DROP_MIN_DOCUMENTS,
        high_document_frequency_threshold_basis_points: AUTOMATIC_HIGH_DF_THRESHOLD_BASIS_POINTS,
        stop_word_fallback_active: false,
        selected_tokens: tokens.to_vec(),
        answerable_tokens: tokens.to_vec(),
        dropped_tokens: Vec::new(),
        selected_token_count: 0,
        answerable_token_count: 0,
        dropped_token_count: 0,
    };
    explanation.refresh_counts();
    AutomaticTokenSelection {
        tokens: tokens.to_vec(),
        answerable: tokens.to_vec(),
        explanation,
    }
}

/// Selects the query tokens used for automatic retrieval.
///
/// Document frequency always orders the tokens (rarest first, so the most discriminating survive
/// truncation), but it only *drops* a token in a corpus large enough for frequency to mean
/// "generic" instead of "this fixture is small", and never below
/// [`AUTOMATIC_MIN_RETAINED_QUERY_TOKENS`] surviving tokens.
fn automatic_eligible_query_tokens(
    connection: &Connection,
    tokens: &[String],
    mode: ContextPackMode,
) -> Result<AutomaticTokenSelection> {
    if mode == ContextPackMode::Explicit {
        return Ok(explicit_token_selection(tokens));
    }
    let document_count = automatic_text_document_count(connection)?;
    let stop_word_fallback_active = document_count < AUTOMATIC_HIGH_DF_MIN_DOCUMENTS;
    let mut dropped = Vec::new();

    let mut candidates = Vec::new();
    for token in tokens.iter().collect::<BTreeSet<_>>() {
        if automatic_short_token(token) {
            dropped.push(AutomaticQueryTokenDrop {
                token: token.clone(),
                filter: AutomaticQueryTokenFilter::ShortToken,
                document_frequency: None,
            });
        } else {
            candidates.push(token.clone());
        }
    }

    if stop_word_fallback_active {
        let (kept, stop_words): (Vec<_>, Vec<_>) = candidates
            .iter()
            .cloned()
            .partition(|token| !automatic_stop_word(token));
        // An intent written entirely in generic words still has to retrieve something, so the
        // fallback never empties the query.
        if !kept.is_empty() {
            dropped.extend(stop_words.into_iter().map(|token| AutomaticQueryTokenDrop {
                token,
                filter: AutomaticQueryTokenFilter::StopWordFallback,
                document_frequency: None,
            }));
            candidates = kept;
        }
    }

    // Rarest first: low document frequency is high IDF. Length and lexicographic order only break
    // exact frequency ties so the selection stays deterministic.
    let mut ranked = Vec::with_capacity(candidates.len());
    for token in candidates {
        let frequency = automatic_token_document_frequency(connection, &token)?;
        ranked.push((frequency, token));
    }
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.chars().count().cmp(&left.1.chars().count()))
            .then_with(|| left.1.cmp(&right.1))
    });

    if document_count >= AUTOMATIC_HIGH_DF_DROP_MIN_DOCUMENTS {
        ranked = drop_high_document_frequency_tokens(ranked, document_count, &mut dropped);
    }

    for (frequency, token) in ranked.iter().skip(MAX_AUTOMATIC_QUERY_TOKENS) {
        dropped.push(AutomaticQueryTokenDrop {
            token: token.clone(),
            filter: AutomaticQueryTokenFilter::TokenBudget,
            document_frequency: Some(*frequency),
        });
    }
    ranked.truncate(MAX_AUTOMATIC_QUERY_TOKENS);
    // The frequencies were already paid for above, so naming the answerable tokens costs no query.
    //
    // A generic word is excluded whatever its frequency. The denominator asks which words of the
    // question this repository could have answered, and `the`, `task` or `code` answer nothing:
    // leaving them in would let a query built entirely out of them read as fully covered the
    // moment one of them appears somewhere, which is the noise an automatic channel exists to
    // keep out. They stay in the query itself, where BM25 can still use them.
    let mut answerable = ranked
        .iter()
        .filter(|(frequency, token)| *frequency > 0 && !automatic_stop_word(token))
        .map(|(_, token)| token.clone())
        .collect::<Vec<_>>();
    answerable.sort();
    let mut eligible = ranked
        .into_iter()
        .map(|(_, token)| token)
        .collect::<Vec<_>>();
    eligible.sort();
    dropped.sort_by(|left, right| left.token.cmp(&right.token));
    let mut explanation = AutomaticQueryTokenExplanation {
        document_count,
        high_document_frequency_min_documents: AUTOMATIC_HIGH_DF_DROP_MIN_DOCUMENTS,
        high_document_frequency_threshold_basis_points: AUTOMATIC_HIGH_DF_THRESHOLD_BASIS_POINTS,
        stop_word_fallback_active,
        selected_tokens: eligible.clone(),
        answerable_tokens: answerable.clone(),
        dropped_tokens: dropped,
        selected_token_count: 0,
        answerable_token_count: 0,
        dropped_token_count: 0,
    };
    explanation.refresh_counts();
    Ok(AutomaticTokenSelection {
        explanation,
        tokens: eligible,
        answerable,
    })
}

/// Removes the tokens whose document frequency makes them useless discriminators. `ranked` is
/// ordered rarest first, so the retained floor is simply its prefix: a frequent token survives
/// while the query is short, unless it is present in nearly every document and would therefore
/// select the whole corpus.
fn drop_high_document_frequency_tokens(
    ranked: Vec<(usize, String)>,
    document_count: usize,
    dropped: &mut Vec<AutomaticQueryTokenDrop>,
) -> Vec<(usize, String)> {
    let minimum_frequency = document_count.saturating_mul(AUTOMATIC_HIGH_DF_THRESHOLD_BASIS_POINTS);
    let universal_frequency =
        document_count.saturating_mul(AUTOMATIC_UNIVERSAL_DF_THRESHOLD_BASIS_POINTS);
    let mut retained = Vec::with_capacity(ranked.len());
    for (position, (frequency, token)) in ranked.into_iter().enumerate() {
        let scaled = frequency.saturating_mul(BASIS_POINTS_SCALE);
        let frequent =
            position >= AUTOMATIC_MIN_RETAINED_QUERY_TOKENS && scaled >= minimum_frequency;
        if frequent || scaled >= universal_frequency {
            dropped.push(AutomaticQueryTokenDrop {
                token,
                filter: AutomaticQueryTokenFilter::HighDocumentFrequency,
                document_frequency: Some(frequency),
            });
        } else {
            retained.push((frequency, token));
        }
    }
    retained
}

/// Structural noise that never carries retrievable meaning, independent of corpus size.
fn automatic_short_token(token: &str) -> bool {
    if token.is_ascii() {
        token.len() < 3
    } else {
        token.chars().count() < 2
    }
}

/// Built-in stop-word fallback. It only applies while the corpus is too small for document
/// frequency to discriminate, and never empties a query. It deliberately excludes words such as
/// `search` that are genuine domain vocabulary in this repository; only observed frequency in a
/// large enough corpus may drop those.
fn automatic_stop_word(token: &str) -> bool {
    matches!(
        token,
        "代码"
            | "分支"
            | "文件"
            | "功能"
            | "问题"
            | "修改"
            | "当前"
            | "实现"
            | "方法"
            | "逻辑"
            | "相关"
            | "需要"
            | "是否"
            | "进行"
            | "android"
            | "app"
            | "branch"
            | "change"
            | "ios"
            | "review"
            | "tiktok"
    ) || automatic_generic_token(token)
}

fn automatic_generic_token(token: &str) -> bool {
    automatic_short_token(token)
        || matches!(
            token,
            "a" | "an"
                | "and"
                | "are"
                | "as"
                | "at"
                | "be"
                | "by"
                | "code"
                | "context"
                | "data"
                | "file"
                | "for"
                | "from"
                | "in"
                | "is"
                | "it"
                | "of"
                | "on"
                | "or"
                | "system"
                | "task"
                | "test"
                | "that"
                | "the"
                | "this"
                | "to"
                | "update"
                | "with"
                | "work"
        )
}

fn automatic_text_document_count(connection: &Connection) -> Result<usize> {
    let sql = format!(
        "SELECT
            (SELECT COUNT(*) FROM space_fts
             JOIN intent_head ON intent_head.space_id = space_fts.space_id
               AND intent_head.revision_id = space_fts.revision_id)
            +
            (SELECT COUNT(*) FROM context_fts
             JOIN context_revision AS revision USING(revision_id)
             JOIN context_item AS item USING(context_id)
             WHERE {SAFE_ACCEPTED_CONTEXT_PREDICATE})"
    );
    let count = connection
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map_err(sql_error("count automatic text documents"))?;
    usize::try_from(count).map_err(|_| invariant("automatic text document count is negative"))
}

fn automatic_token_document_frequency(connection: &Connection, token: &str) -> Result<usize> {
    let Some(expression) = fts_or_match_expression(&[token.to_owned()]) else {
        return Ok(0);
    };
    let intent_count = connection
        .query_row(
            "SELECT COUNT(*) FROM space_fts
             JOIN intent_head ON intent_head.space_id = space_fts.space_id
               AND intent_head.revision_id = space_fts.revision_id
             WHERE space_fts MATCH ?1",
            [&expression],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error("count automatic Space Intent token frequency"))?;
    let context_sql = format!(
        "SELECT COUNT(*) FROM context_fts
         JOIN context_revision AS revision USING(revision_id)
         JOIN context_item AS item USING(context_id)
         WHERE context_fts MATCH ?1 AND {SAFE_ACCEPTED_CONTEXT_PREDICATE}"
    );
    let context_count = connection
        .query_row(&context_sql, [&expression], |row| row.get::<_, i64>(0))
        .map_err(sql_error("count automatic Context token frequency"))?;
    usize::try_from(intent_count.saturating_add(context_count))
        .map_err(|_| invariant("automatic token document frequency is negative"))
}

fn eligible_query_phrases(phrases: &[String], tokens: &[String]) -> Vec<String> {
    let allowed = tokens.iter().collect::<BTreeSet<_>>();
    phrases
        .iter()
        .filter(|phrase| {
            search_tokens(phrase)
                .iter()
                .collect::<BTreeSet<_>>()
                .intersection(&allowed)
                .count()
                >= 2
        })
        .cloned()
        .collect()
}

#[derive(Clone, Debug)]
struct WorkingIntentHintQuery {
    source_field: WorkingIntentHintField,
    tokens: Vec<String>,
    /// The subset of `tokens` this corpus can match at all, and so the coverage denominator of
    /// this hint channel. Equal to `tokens` until automatic selection has measured them.
    answerable_tokens: Vec<String>,
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
            answerable_tokens: tokens.clone(),
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
    // Automatic retrieval is always the ranked, recall-oriented mode, so it always expands.
    let alias = AliasExpansion::load(connection, query_tokens)?;
    let Some(match_expression) = fts_or_match_expression(&alias.expanded_tokens(query_tokens))
    else {
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
                &alias,
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
    alias: &AliasExpansion,
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
    let field_matches = explain_intent_match(query_tokens, alias, fields);
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
    alias: &AliasExpansion,
    fields: [(SpaceIntentField, &str); N],
) -> Vec<SpaceIntentFieldMatch> {
    let wanted = query_tokens.iter().cloned().collect::<BTreeSet<_>>();
    fields
        .into_iter()
        .filter_map(|(field, text)| {
            let available = search_tokens(text).into_iter().collect::<BTreeSet<_>>();
            // An alias hit is reported as its origin query token, so Intent field coverage keeps
            // the original denominator.
            let (matched, _aliases) = alias.resolve(&wanted, &available);
            (!matched.is_empty()).then_some(SpaceIntentFieldMatch {
                field,
                matched_tokens: matched.into_iter().collect(),
            })
        })
        .collect()
}

#[derive(Debug, Default)]
struct ScopeTargets {
    domains: BTreeSet<String>,
    /// Alias spellings of [`ScopeTargets::domains`]; empty until `with_domain_aliases` runs.
    domain_aliases: BTreeSet<String>,
    platforms: BTreeSet<String>,
    /// Normalized tokens of the Task goal and in-scope items, not whole constraint strings.
    conditions: BTreeSet<String>,
}

impl ScopeTargets {
    fn from_intent(intent: &WorkingIntentSnapshot) -> Self {
        // `condition` compares tokens, not whole strings: a Context condition is a phrase written
        // by whoever recorded the fact, while the Task states the same situation as its goal and
        // in-scope items. Comparing the Task's constraints as whole strings almost never matched.
        let mut conditions = search_tokens(&intent.goal)
            .into_iter()
            .collect::<BTreeSet<_>>();
        for entry in &intent.in_scope {
            conditions.extend(search_tokens(entry));
        }
        Self {
            domains: normalized_values(&intent.domains),
            domain_aliases: BTreeSet::new(),
            platforms: normalized_values(&intent.platforms),
            conditions,
        }
    }

    /// Adds the `token_alias` expansions of the Task's domains, so a Context recorded under one
    /// spelling of a domain still matches a Task that names another spelling of the same group.
    fn with_domain_aliases(&self, connection: &Connection) -> Result<Self> {
        let domain_tokens = self
            .domains
            .iter()
            .flat_map(|domain| search_tokens(domain))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let alias = AliasExpansion::load(connection, &domain_tokens)?;
        let mut domain_aliases = BTreeSet::new();
        if !alias.is_empty() {
            for domain in &self.domains {
                let tokens = search_tokens(domain);
                for expanded in expanded_value_spellings(&alias, &tokens) {
                    if !self.domains.contains(&expanded) {
                        domain_aliases.insert(expanded);
                    }
                }
            }
        }
        Ok(Self {
            domains: self.domains.clone(),
            domain_aliases,
            platforms: self.platforms.clone(),
            conditions: self.conditions.clone(),
        })
    }

    fn matches(&self, dimension: &str, value: &str) -> bool {
        match dimension {
            "domain" => {
                let normalized = normalize_search_text(value);
                self.domains.contains(&normalized) || self.domain_aliases.contains(&normalized)
            }
            "platform" => self.platforms.contains(&normalize_search_text(value)),
            // Every token of the Context condition must be stated by the Task, so a condition
            // matches only when the Task already describes that situation.
            "condition" => {
                let tokens = search_tokens(value);
                !tokens.is_empty() && tokens.iter().all(|token| self.conditions.contains(token))
            }
            _ => false,
        }
    }
}

/// Every one-token substitution of `tokens` through `alias`, joined back into a normalized value.
///
/// Only one token is substituted at a time: the point is to accept another spelling of the same
/// domain, not to enumerate the cross product of every alias group in the value.
fn expanded_value_spellings(alias: &AliasExpansion, tokens: &[String]) -> BTreeSet<String> {
    let mut spellings = BTreeSet::new();
    for (index, token) in tokens.iter().enumerate() {
        for replacement in alias.token_group(token) {
            if replacement == *token {
                continue;
            }
            let mut candidate = tokens.to_vec();
            candidate[index] = replacement;
            spellings.insert(candidate.join(" "));
        }
    }
    spellings
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
    /// Subset of `matched_tokens` that names part of a code identifier this Tree indexes; see
    /// [`AliasExpansion::identifier_tokens`].
    identifier_matched_tokens: BTreeSet<String>,
    alias_matches: BTreeSet<AliasMatch>,
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EffectiveSpaceRole {
    matched_space_id: SpaceId,
    role: SpaceAssociationRole,
}

#[derive(Clone, Debug)]
struct EffectiveContextSpaces {
    association_id: SpaceAssociationId,
    roles: Vec<EffectiveSpaceRole>,
}

#[derive(Clone, Debug)]
struct MatchedContextSpace {
    association_space_id: SpaceId,
    association_path: Option<TaskRetrievalPath>,
}

const SAFE_ACCEPTED_CONTEXT_PREDICATE: &str = "item.governance_status = 'accepted'
     AND item.accepted_revision_id = revision.revision_id
     AND item.auto_injection_eligible = 1
     AND item.superseded_by IS NULL
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
    /// Subset of `context_tokens` that names part of a code identifier this Tree indexes.
    context_identifier_tokens: BTreeSet<String>,
    context_fields: BTreeSet<MatchField>,
    context_bm25: Option<f64>,
    context_phrase_match: bool,
    context_field_weight_points: u16,
    hint_text: BTreeMap<WorkingIntentHintTextChannel, HintTextEvidence>,
    matched_scopes: BTreeSet<ScopeEvidence>,
    graph_exact_contexts: BTreeSet<ContextId>,
    relation_contexts: BTreeSet<ContextId>,
    focus_text_fallback_contexts: BTreeSet<ContextId>,
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
    effective_spaces: BTreeMap<ContextId, EffectiveContextSpaces>,
    graph_context_tree_oid: Option<String>,
    graph_artifact_generation: Option<String>,
    focus_reachable: bool,
    /// Compact reports for the References that name this Focus and did not resolve. Automatic
    /// injection has no other way to learn the difference between an Artifact nobody wrote about
    /// and one whose Reference stopped resolving.
    unresolved_focus_diagnostics: Vec<TaskGraphDiagnostic>,
    /// Denominator every automatic coverage decision downstream of the inference divides by.
    coverage_basis: AutomaticCoverageBasis,
    query_token_explanation: AutomaticQueryTokenExplanation,
    /// Spaces the automatic text gate dropped, already collapsed to a reportable size.
    omitted: Vec<ContextPackOmitted>,
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
    focus_text_fallback_enabled: bool,
    mode: ContextPackMode,
) -> Result<TaskAssociationInference> {
    let selection = automatic_eligible_query_tokens(connection, query_tokens, mode)?;
    let coverage_basis = AutomaticCoverageBasis::from_selection(&selection);
    let mut token_explanation = selection.explanation;
    let query_tokens = selection.tokens;
    let query_phrases = eligible_query_phrases(query_phrases, &query_tokens);
    let hint_queries = hint_queries
        .iter()
        .map(|query| {
            let selection = automatic_eligible_query_tokens(connection, &query.tokens, mode)?;
            merge_token_explanations(&mut token_explanation, selection.explanation);
            Ok(WorkingIntentHintQuery {
                source_field: query.source_field,
                phrases: eligible_query_phrases(&query.phrases, &selection.tokens),
                answerable_tokens: selection.answerable,
                tokens: selection.tokens,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let intent_candidates =
        query_space_intent_candidates(connection, &query_tokens, &query_phrases)?;
    let effective_spaces = query_effective_context_spaces(connection)?;
    let mut contexts =
        query_accepted_context_evidence(connection, &query_tokens, &query_phrases, scope_targets)?;
    let mut evidence = BTreeMap::<SpaceId, AssociationEvidence>::new();
    apply_intent_evidence(&mut evidence, intent_candidates);
    apply_working_intent_hint_evidence(connection, &hint_queries, &mut evidence, &mut contexts)?;
    retain_only_negative_intent_tokens(&mut evidence);
    if focus_text_fallback_enabled {
        if let Some(resolved_focus) = resolved_focus {
            query_resolved_focus_text_fallback(connection, resolved_focus, &mut contexts)?;
        }
    }
    let mut graph_contexts = BTreeMap::new();
    let mut focus_reachable = false;
    let mut unresolved_focus_diagnostics = Vec::new();
    if let Some(graph) = engineering_graph {
        focus_reachable = query_graph_context_evidence(
            graph,
            resolved_focus,
            mode,
            &mut graph_contexts,
            &mut unresolved_focus_diagnostics,
        );
        expand_graph_context_relation_evidence(graph, mode, &mut graph_contexts)?;
    }
    expand_current_context_relation_evidence(connection, mode, &coverage_basis, &mut contexts)?;
    for ((space_id, context_id), context) in &contexts {
        aggregate_context_across_effective_spaces(
            &mut evidence,
            &effective_spaces,
            *space_id,
            *context_id,
            context,
        );
    }
    for ((space_id, context_id, _revision_id), graph) in &graph_contexts {
        aggregate_context_across_effective_spaces(
            &mut evidence,
            &effective_spaces,
            *space_id,
            *context_id,
            &graph.evidence,
        );
    }
    hydrate_intent_conflict_state(connection, &mut evidence)?;
    assign_channel_features(&mut evidence, coverage_basis.tokens());
    let mut gate_omitted = Vec::new();
    let mut associations = Vec::new();
    for (space_id, space_evidence) in &evidence {
        if let Some(built) = association(
            task_id,
            *space_id,
            space_evidence,
            &coverage_basis,
            mode,
            &token_explanation,
            &mut gate_omitted,
        ) {
            associations.push(built);
        }
    }
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
        effective_spaces,
        graph_context_tree_oid: graph_context_tree_oid.map(ToOwned::to_owned),
        graph_artifact_generation: engineering_graph.map(|graph| graph.artifact_generation.clone()),
        focus_reachable,
        unresolved_focus_diagnostics,
        coverage_basis,
        query_token_explanation: token_explanation,
        omitted: collapse_gate_omissions(gate_omitted),
    })
}

/// Names the first [`AUTOMATIC_GATE_OMISSION_LIMIT`] Spaces the automatic text gate dropped and
/// collapses the rest into one counted notice.
///
/// A Pack that returned nothing owes the Agent the reason; it does not owe it one entry per Space
/// in the repository, which on a large Tree would cost more budget than the facts it replaced.
fn collapse_gate_omissions(mut omitted: Vec<ContextPackOmitted>) -> Vec<ContextPackOmitted> {
    if omitted.len() <= AUTOMATIC_GATE_OMISSION_LIMIT {
        return omitted;
    }
    // Highest coverage first: the Spaces that came closest to passing are the ones worth naming.
    omitted.sort_by(|left, right| {
        right
            .coverage_basis_points
            .cmp(&left.coverage_basis_points)
            .then_with(|| left.space_id.cmp(&right.space_id))
    });
    let collapsed = omitted.split_off(AUTOMATIC_GATE_OMISSION_LIMIT);
    omitted.push(ContextPackOmitted {
        reason: "automatic_text_ineligible".to_owned(),
        count: collapsed.len(),
        ..ContextPackOmitted::default()
    });
    omitted
}

fn query_effective_context_spaces(
    connection: &Connection,
) -> Result<BTreeMap<ContextId, EffectiveContextSpaces>> {
    let mut statement = connection
        .prepare(
            "SELECT association.association_id, association.context_id,
                    association.primary_space_id, association.related_space_ids_json
             FROM context_space_association_head AS head
             JOIN context_space_association AS association USING(association_id)
             WHERE head.context_id IN (
                 SELECT context_id FROM context_space_association_head
                 GROUP BY context_id HAVING COUNT(*) = 1
             )
             ORDER BY association.context_id, association.association_id",
        )
        .map_err(sql_error("prepare effective Context Space associations"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error("read effective Context Space associations"))?;
    let mut effective = BTreeMap::new();
    for row in rows {
        let (association_id, context_id, primary_space_id, related) =
            row.map_err(sql_error("collect effective Context Space association"))?;
        let association_id = parse_id(&association_id)?;
        let context_id = parse_id(&context_id)?;
        let primary_space_id = parse_id(&primary_space_id)?;
        let related_space_ids = from_json::<Vec<SpaceId>>(&related)?;
        let mut roles = Vec::with_capacity(related_space_ids.len() + 1);
        roles.push(EffectiveSpaceRole {
            matched_space_id: primary_space_id,
            role: SpaceAssociationRole::Primary,
        });
        roles.extend(
            related_space_ids
                .into_iter()
                .map(|matched_space_id| EffectiveSpaceRole {
                    matched_space_id,
                    role: SpaceAssociationRole::Related,
                }),
        );
        effective.insert(
            context_id,
            EffectiveContextSpaces {
                association_id,
                roles,
            },
        );
    }
    Ok(effective)
}

fn aggregate_context_across_effective_spaces(
    evidence: &mut BTreeMap<SpaceId, AssociationEvidence>,
    effective_spaces: &BTreeMap<ContextId, EffectiveContextSpaces>,
    fallback_space_id: SpaceId,
    context_id: ContextId,
    context: &AcceptedContextEvidence,
) {
    if let Some(effective) = effective_spaces.get(&context_id) {
        for role in &effective.roles {
            aggregate_context_evidence(
                evidence.entry(role.matched_space_id).or_default(),
                context_id,
                context,
            );
        }
    } else {
        aggregate_context_evidence(
            evidence.entry(fallback_space_id).or_default(),
            context_id,
            context,
        );
    }
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
        aggregate
            .context_identifier_tokens
            .extend(context.identifier_matched_tokens.iter().cloned());
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
    if context
        .graph_paths
        .iter()
        .any(|path| matches!(path, TaskRetrievalPath::ResolvedFocusTextFallback { .. }))
    {
        aggregate.focus_text_fallback_contexts.insert(context_id);
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

/// Keeps only the query tokens that are exclusively negative for one Space. A token that also
/// matched any positive Intent field of the same Space (`title`, `problem`, `desired_outcome`,
/// `in_scope`, `acceptance_conditions`, `domain_terms`) is shared vocabulary, not an exclusion,
/// so it must not raise a scope conflict or reduce the association score.
fn retain_only_negative_intent_tokens(evidence: &mut BTreeMap<SpaceId, AssociationEvidence>) {
    for aggregate in evidence.values_mut() {
        if aggregate.excluded_intent_tokens.is_empty() {
            continue;
        }
        let mut positive = aggregate.intent_tokens.clone();
        for (channel, hint) in &aggregate.hint_text {
            if channel.target == WorkingIntentHintTarget::SpaceIntentFts {
                positive.extend(hint.matched_tokens.iter().cloned());
            }
        }
        aggregate
            .excluded_intent_tokens
            .retain(|token| !positive.contains(token));
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
                        query_tokens: query.answerable_tokens.iter().cloned().collect(),
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
                query_tokens: query.answerable_tokens.iter().cloned().collect(),
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

/// Mirrors [`CONTEXT_FTS_BM25_WEIGHTS`] for the fusion feature that ranks by which fields matched.
const fn context_field_weight(field: MatchField) -> u16 {
    match field {
        MatchField::Title | MatchField::Statement | MatchField::ProblemView => 8,
        MatchField::Rationale => 4,
        MatchField::Evidence | MatchField::HintText => 2,
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
    unresolved: &mut Vec<TaskGraphDiagnostic>,
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
            // A Reference that resolved to nothing has no Artifact key to match the Focus
            // against, so it is matched against the locator the Reference itself names. That is
            // how a broken association stops being indistinguishable from an Artifact nobody ever
            // documented -- and it stays a diagnostic: no Context, no evidence, no association, so
            // an unreachable Artifact still retrieves exactly nothing through the Graph.
            if mode == ContextPackMode::AutomaticInjection
                && resolved.resolution.repository_id == resolved_focus.repository_id
                && resolved.locator.as_ref() == Some(&resolved_focus.locator)
                && let Some(diagnostic) = unresolved_focus_diagnostic(resolved_focus, resolved)
            {
                unresolved.push(diagnostic);
            }
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
        } else if mode == ContextPackMode::AutomaticInjection {
            // Automatic injection never crosses an unresolved Reference, but it must not stay
            // silent about one either: the Reference is the evidence that somebody documented
            // this exact Artifact and the association has since come apart.
            if let Some(diagnostic) = unresolved_focus_diagnostic(resolved_focus, resolved) {
                unresolved.push(diagnostic);
            }
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
    unresolved.sort();
    unresolved.dedup();
    unresolved.truncate(MAX_UNRESOLVED_FOCUS_DIAGNOSTICS);
    reachable
}

/// One compact sentence for a Reference that names this Focus and did not resolve.
///
/// `Unavailable` is deliberately absent: it says the checkout was not there to look in, which is
/// a fact about this machine rather than about the association, and the generic "not reachable"
/// diagnostic already covers it without implying the Reference itself is broken.
fn unresolved_focus_diagnostic(
    resolved_focus: &ResolvedFocus,
    resolved: &ResolvedReferenceProjection,
) -> Option<TaskGraphDiagnostic> {
    let locator = resolved_focus.locator.canonical_key();
    let (kind, detail) = match resolved.resolution.status {
        ResolutionStatus::Missing => (
            TaskGraphDiagnosticKind::ArtifactReferenceMissing,
            format!(
                "{locator} is referenced by accepted knowledge and is absent from the scanned Repository snapshot"
            ),
        ),
        ResolutionStatus::Ambiguous => (
            TaskGraphDiagnosticKind::ArtifactReferenceAmbiguous,
            format!(
                "{locator} is referenced by accepted knowledge and several Artifacts answer to it"
            ),
        ),
        ResolutionStatus::Resolved | ResolutionStatus::Unavailable => return None,
    };
    Some(TaskGraphDiagnostic {
        kind,
        resolved_focus: resolved_focus.clone(),
        detail: Some(detail),
    })
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
    coverage_basis: &AutomaticCoverageBasis,
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
    let seed_basis = coverage_basis.selected_only();
    let seeds = contexts
        .iter()
        .filter(|(_, evidence)| context_is_positive_seed(evidence, mode, &seed_basis))
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

fn context_is_positive_seed(
    evidence: &AcceptedContextEvidence,
    mode: ContextPackMode,
    coverage_basis: &AutomaticCoverageBasis,
) -> bool {
    if evidence.graph_paths.iter().any(|path| {
        matches!(
            path,
            TaskRetrievalPath::EngineeringGraph { relation_hops, .. }
                if relation_hops.is_empty()
        )
    }) || !evidence.matched_artifacts.is_empty()
    {
        return true;
    }
    if mode == ContextPackMode::Explicit {
        return evidence.textual_match
            || !evidence.hint_text.is_empty()
            || !evidence.matched_scopes.is_empty();
    }
    automatic_direct_context_text_eligible(evidence, coverage_basis)
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
    // Automatic retrieval is always the ranked, recall-oriented mode, so it always expands.
    let alias = &AliasExpansion::load(connection, query_tokens)?;
    let Some(match_expression) = fts_or_match_expression(&alias.expanded_tokens(query_tokens))
    else {
        return Ok(());
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT revision.space_id, revision.context_id,
                    bm25(context_fts, {CONTEXT_FTS_BM25_WEIGHTS}),
                    context_fts.title, context_fts.statement,
                    context_fts.rationale, context_fts.evidence,
                    context_fts.problem_view, context_fts.hint_text
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
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ],
            ))
        })
        .map_err(sql_error("read safe accepted Context association text"))?;
    for row in rows {
        let (space_id, context_id, bm25, fields) =
            row.map_err(sql_error("collect accepted Context association text"))?;
        let (matched_fields, matched_tokens, alias_matches) =
            explain_context_text_match(query_tokens, alias, &fields);
        let entry = evidence
            .entry((parse_id(&space_id)?, parse_id(&context_id)?))
            .or_default();
        entry.textual_match = true;
        entry.matched_fields.extend(matched_fields);
        entry.identifier_matched_tokens.extend(
            matched_tokens
                .iter()
                .filter(|token| alias.identifier_tokens().contains(*token))
                .cloned(),
        );
        entry.matched_tokens.extend(matched_tokens);
        entry.alias_matches.extend(alias_matches);
        entry.phrase_match |= contains_any_phrase(&fields, query_phrases);
        entry.bm25 = Some(entry.bm25.map_or(bm25, |current| current.min(bm25)));
    }
    Ok(())
}

fn query_resolved_focus_text_fallback(
    connection: &Connection,
    resolved_focus: &ResolvedFocus,
    evidence: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let components = resolved_focus_text_components(resolved_focus);
    let tokens = components
        .iter()
        .flat_map(|component| search_tokens(component))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let Some(match_expression) = fts_or_match_expression(&tokens) else {
        return Ok(());
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT revision.space_id, revision.context_id,
                    bm25(context_fts, {CONTEXT_FTS_BM25_WEIGHTS}),
                    context_fts.title, context_fts.statement,
                    context_fts.rationale, context_fts.evidence,
                    context_fts.problem_view, context_fts.hint_text
             FROM context_fts
             JOIN context_revision AS revision USING(revision_id)
             JOIN context_item AS item USING(context_id)
             WHERE context_fts MATCH ?1
               AND {SAFE_ACCEPTED_CONTEXT_PREDICATE}
             ORDER BY revision.space_id, revision.context_id"
        ))
        .map_err(sql_error("prepare Resolved Focus text fallback"))?;
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
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ],
            ))
        })
        .map_err(sql_error("query Resolved Focus text fallback"))?;
    let field_names = CONTEXT_FTS_TEXT_FIELDS;
    for row in rows {
        let (space_id, context_id, bm25, fields) =
            row.map_err(sql_error("collect Resolved Focus text fallback"))?;
        let mut matched_fields = BTreeSet::new();
        let all_components_match = components.iter().all(|component| {
            let component_fields = fields
                .iter()
                .enumerate()
                .filter_map(|(index, field)| {
                    strict_text_component_match(field, component).then_some(field_names[index])
                })
                .collect::<Vec<_>>();
            matched_fields.extend(component_fields.iter().copied());
            !component_fields.is_empty()
        });
        if !all_components_match {
            continue;
        }
        let entry = evidence
            .entry((parse_id(&space_id)?, parse_id(&context_id)?))
            .or_default();
        entry.matched_fields.extend(matched_fields.iter().copied());
        entry.matched_tokens.extend(tokens.iter().cloned());
        entry.bm25 = Some(entry.bm25.map_or(bm25, |current| current.min(bm25)));
        entry
            .graph_paths
            .push(TaskRetrievalPath::ResolvedFocusTextFallback {
                explanation: ResolvedFocusTextFallbackExplanation {
                    resolved_focus: resolved_focus.clone(),
                    matched_components: components.clone(),
                    matched_fields: matched_fields.into_iter().collect(),
                },
            });
    }
    Ok(())
}

fn resolved_focus_text_components(resolved_focus: &ResolvedFocus) -> Vec<String> {
    vec![
        resolved_focus.repository_id.to_string(),
        resolved_focus.locator.canonical_key(),
    ]
}

fn strict_text_component_match(field: &str, component: &str) -> bool {
    let field = format!(" {} ", field.trim());
    let component = format!(" {} ", normalize_search_text(component));
    component != "  " && field.contains(&component)
}

/// Column order of [`CONTEXT_FTS_TEXT_FIELDS`]' owning query, mapped onto explainable names.
const CONTEXT_FTS_TEXT_FIELDS: [MatchField; 6] = [
    MatchField::Title,
    MatchField::Statement,
    MatchField::Rationale,
    MatchField::Evidence,
    MatchField::ProblemView,
    MatchField::HintText,
];

fn explain_context_text_match(
    query_tokens: &[String],
    alias: &AliasExpansion,
    fields: &[String; 6],
) -> (Vec<MatchField>, Vec<String>, Vec<AliasMatch>) {
    let named_fields: [(MatchField, &str); 6] =
        std::array::from_fn(|index| (CONTEXT_FTS_TEXT_FIELDS[index], fields[index].as_str()));
    explain_token_fields(query_tokens, alias, named_fields)
}

fn explain_token_fields<T, const N: usize>(
    query_tokens: &[String],
    alias: &AliasExpansion,
    fields: [(T, &str); N],
) -> (Vec<T>, Vec<String>, Vec<AliasMatch>)
where
    T: Copy,
{
    let wanted = query_tokens.iter().cloned().collect::<BTreeSet<_>>();
    let mut matched_fields = Vec::new();
    let mut matched_tokens = BTreeSet::new();
    let mut alias_matches = BTreeSet::new();
    for (field, text) in fields {
        let available = search_tokens(text).into_iter().collect::<BTreeSet<_>>();
        let (matched, aliases) = alias.resolve(&wanted, &available);
        if !matched.is_empty() {
            matched_fields.push(field);
            matched_tokens.extend(matched);
            alias_matches.extend(aliases);
        }
    }
    (
        matched_fields,
        matched_tokens.into_iter().collect(),
        alias_matches.into_iter().collect(),
    )
}

fn query_accepted_context_scope(
    connection: &Connection,
    targets: &ScopeTargets,
    evidence: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let targets = targets.with_domain_aliases(connection)?;
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
const FOCUS_TEXT_FALLBACK_CHANNEL_WEIGHT: usize = 6;
const HINT_TEXT_CHANNEL_WEIGHT: usize = 3;
const FUSION_CHANNEL_WEIGHT: usize = GRAPH_ARTIFACT_CHANNEL_WEIGHT
    + CONTEXT_RELATION_CHANNEL_WEIGHT
    + FOCUS_TEXT_FALLBACK_CHANNEL_WEIGHT
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

    let mut fallbacks = evidence
        .iter()
        .filter_map(|(space_id, value)| {
            (!value.focus_text_fallback_contexts.is_empty())
                .then_some((*space_id, value.focus_text_fallback_contexts.len()))
        })
        .collect::<Vec<_>>();
    fallbacks.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    previous = None;
    rank = 0;
    for (offset, (space_id, strength)) in fallbacks.into_iter().enumerate() {
        if previous != Some(strength) {
            rank = offset + 1;
            previous = Some(strength);
        }
        evidence
            .get_mut(&space_id)
            .expect("ranked Focus fallback Space exists")
            .channel_features
            .push(weighted_exact_channel_feature(
                TaskAssociationChannel::ResolvedFocusTextFallback,
                rank,
                strength,
                FOCUS_TEXT_FALLBACK_CHANNEL_WEIGHT,
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

/// Query token coverage in which every matched identifier-channel token counts
/// [`AUTOMATIC_IDENTIFIER_TOKEN_COVERAGE_WEIGHT`] times over, capped at full coverage.
///
/// Falls back to the plain coverage until the query names at least
/// [`AUTOMATIC_MIN_IDENTIFIER_QUERY_TOKENS`] identifier tokens, so a single incidental word never
/// buys a Context past the automatic gate.
fn identifier_weighted_coverage_basis_points(
    matched: &BTreeSet<String>,
    identifier_matched: &BTreeSet<String>,
    query_tokens: &[String],
) -> u16 {
    let plain = token_coverage_basis_points(matched, query_tokens);
    if query_tokens.is_empty() {
        return plain;
    }
    let query = query_tokens.iter().collect::<BTreeSet<_>>();
    let matched_count = matched.iter().filter(|token| query.contains(token)).count();
    let identifier_count = identifier_matched
        .iter()
        .filter(|token| query.contains(token) && matched.contains(*token))
        .count();
    if identifier_count < AUTOMATIC_MIN_IDENTIFIER_QUERY_TOKENS {
        return plain;
    }
    let weighted = matched_count
        + identifier_count.saturating_mul(AUTOMATIC_IDENTIFIER_TOKEN_COVERAGE_WEIGHT - 1);
    let basis_points = weighted
        .saturating_mul(BASIS_POINTS_SCALE)
        .checked_div(query.len())
        .unwrap_or(0)
        .min(BASIS_POINTS_SCALE);
    u16::try_from(basis_points)
        .expect("coverage basis points fit u16")
        .max(plain)
}

#[allow(clippy::cast_possible_truncation)]
fn scale_bm25(value: f64) -> i64 {
    (value * 1_000_000.0).round() as i64
}

fn scale_idf_bm25_contribution(value: f64) -> u32 {
    u32::try_from(scale_bm25(value).saturating_neg()).unwrap_or(u32::MAX)
}

fn merge_token_explanations(
    target: &mut AutomaticQueryTokenExplanation,
    source: AutomaticQueryTokenExplanation,
) {
    target.document_count = target.document_count.max(source.document_count);
    target.stop_word_fallback_active |= source.stop_word_fallback_active;
    let mut selected = target
        .selected_tokens
        .iter()
        .cloned()
        .chain(source.selected_tokens)
        .collect::<BTreeSet<_>>();
    let answerable = target
        .answerable_tokens
        .iter()
        .cloned()
        .chain(source.answerable_tokens)
        .collect::<BTreeSet<_>>();
    target.answerable_tokens = answerable.into_iter().collect();
    target.dropped_tokens.extend(source.dropped_tokens);
    target.dropped_tokens.sort_by(|left, right| {
        left.token
            .cmp(&right.token)
            .then(left.filter.cmp(&right.filter))
    });
    target.dropped_tokens.dedup();
    // A token selected by any channel is never reported as dropped.
    target
        .dropped_tokens
        .retain(|drop| !selected.contains(&drop.token));
    target.selected_tokens = std::mem::take(&mut selected).into_iter().collect();
    target.refresh_counts();
}

fn association(
    task_id: TaskId,
    space_id: SpaceId,
    evidence: &AssociationEvidence,
    coverage_basis: &AutomaticCoverageBasis,
    mode: ContextPackMode,
    token_explanation: &AutomaticQueryTokenExplanation,
    omitted: &mut Vec<ContextPackOmitted>,
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
    if mode == ContextPackMode::AutomaticInjection {
        let gate = automatic_space_text_gate(evidence, coverage_basis);
        if !gate.eligible {
            // The Space that failed the text gate is the whole reason a Pack can come back empty,
            // so the decision is written down with the numbers it was made on.
            omitted.push(ContextPackOmitted {
                space_id: Some(space_id),
                reason: gate.omission_reason().to_owned(),
                count: 1,
                coverage_basis_points: Some(gate.coverage_basis_points),
                answerable_tokens: Some(coverage_basis.answerable_count()),
                selected_tokens: Some(coverage_basis.selected_count()),
                text_channel_count: Some(gate.text_channel_count),
                ..ContextPackOmitted::default()
            });
            return None;
        }
    }
    let score = association_score(evidence);
    let mut reasons = association_reasons(evidence);
    if !token_explanation.dropped_tokens.is_empty() {
        // The projection, not the whole selection: the Pack carries one full copy at the top
        // level, and repeating every token inside every Association spends the injection budget
        // on saying the same thing once per Space.
        reasons.push(
            serde_json::to_string(&token_explanation.compact_projection())
                .expect("automatic query token explanation is always serializable"),
        );
    }
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

/// Decides whether one Space's text evidence is strong enough for automatic injection, and
/// records the numbers behind the decision.
fn automatic_space_text_gate(
    evidence: &AssociationEvidence,
    coverage_basis: &AutomaticCoverageBasis,
) -> AutomaticTextGate {
    if !evidence.graph_exact_contexts.is_empty()
        || !evidence.relation_contexts.is_empty()
        || !evidence.focus_text_fallback_contexts.is_empty()
    {
        return AutomaticTextGate::ELIGIBLE;
    }
    let hint_phrase = evidence.hint_text.values().any(|hint| hint.phrase_match);
    if evidence.intent_phrase_match || evidence.context_phrase_match || hint_phrase {
        return AutomaticTextGate::ELIGIBLE;
    }
    let query_tokens = coverage_basis.tokens();
    let coverage = token_coverage_basis_points(&evidence.intent_tokens, query_tokens)
        .max(identifier_weighted_coverage_basis_points(
            &evidence.context_tokens,
            &evidence.context_identifier_tokens,
            query_tokens,
        ))
        .max(
            evidence
                .hint_text
                .values()
                .map(HintTextEvidence::coverage_basis_points)
                .max()
                .unwrap_or(0),
        );
    let text_channel_count = usize::from(evidence.intent_matched)
        + usize::from(evidence.context_bm25.is_some())
        + evidence.hint_text.len();
    let covered = coverage >= AUTOMATIC_TEXT_COVERAGE_THRESHOLD_BASIS_POINTS;
    let ratio_sufficient = coverage_basis.answerable_ratio_sufficient();
    let eligible = (covered && ratio_sufficient)
        || text_channel_count >= 2
        || (evidence.context_bm25.is_some() && !evidence.matched_scopes.is_empty());
    AutomaticTextGate {
        eligible,
        coverage_basis_points: coverage,
        text_channel_count,
        blocked_by_answerable_ratio: !eligible && covered && !ratio_sufficient,
    }
}

/// True when coverage measured against the answerable denominator may be believed on its own.
fn automatic_coverage_passes(coverage: u16, coverage_basis: &AutomaticCoverageBasis) -> bool {
    coverage >= AUTOMATIC_TEXT_COVERAGE_THRESHOLD_BASIS_POINTS
        && coverage_basis.answerable_ratio_sufficient()
}

/// Most Spaces named one by one in a single omission reason before the rest are collapsed into
/// one counted notice.
const AUTOMATIC_GATE_OMISSION_LIMIT: usize = 8;

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

/// Reweights fused item scores by how earlier Tasks used each Context.
///
/// The prior runs after every demotion, so it reorders equally ranked Contexts without ever
/// undoing a conflict, stale, or historical demotion decision. Without a source, nothing is read
/// and no score changes.
fn apply_usage_prior(
    candidates: &mut [TaskContextCandidate],
    usage_prior: Option<&dyn UsagePriorSource>,
) {
    let Some(source) = usage_prior else {
        return;
    };
    if candidates.is_empty() {
        return;
    }
    let context_ids = candidates
        .iter()
        .map(|candidate| candidate.item.context.context_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let counts = source.usage_counts(&context_ids);
    for candidate in candidates {
        let Some(usage) = counts.get(&candidate.item.context.context_id).copied() else {
            continue;
        };
        if usage.is_empty() {
            continue;
        }
        candidate.item.context.usage = usage;
        candidate.injection_score_basis_points = usage_prior_score(
            candidate.injection_score_basis_points,
            usage_multiplier_basis_points(usage),
        );
    }
}

/// Multiplier applied to one fused item score, in basis points.
///
/// A refuted Context is not demoted here: an open semantic conflict is already the stronger and
/// explainable signal, and demoting twice for the same fact would double-count it.
const fn usage_multiplier_basis_points(usage: ContextUsageCounts) -> u16 {
    if usage.reused >= 1 {
        return USAGE_REUSED_BONUS_BASIS_POINTS;
    }
    if usage.ignored >= USAGE_IGNORED_PENALTY_MINIMUM_TASKS {
        return USAGE_IGNORED_PENALTY_BASIS_POINTS;
    }
    BASIS_POINTS
}

const BASIS_POINTS: u16 = 10_000;

fn usage_prior_score(injection_score_basis_points: u16, multiplier_basis_points: u16) -> u16 {
    if multiplier_basis_points == BASIS_POINTS {
        return injection_score_basis_points;
    }
    u16::try_from(
        usize::from(injection_score_basis_points)
            .saturating_mul(usize::from(multiplier_basis_points))
            / BASIS_POINTS_SCALE,
    )
    .unwrap_or(u16::MAX)
}

/// Demotes one already fused item score for state the Space association cannot see.
///
/// The Space's fused RRF score is the base: items in one Space start equal, so a demoted item
/// sorts behind its undemoted siblings without inventing a second ranking signal. Demotions
/// compose multiplicatively and are reported on the item that carries them.
fn demoted_item_score(
    fused_score_basis_points: u16,
    conflicts: &[ConflictView],
    derived_state: &ContextDerivedState,
) -> u16 {
    let multiplier = usize::from(item_demotion_basis_points(conflicts, derived_state));
    u16::try_from(
        usize::from(fused_score_basis_points).saturating_mul(multiplier) / BASIS_POINTS_SCALE,
    )
    .unwrap_or(u16::MAX)
}

/// Product of every per-item ranking multiplier, in basis points; `BASIS_POINTS_SCALE` is none.
fn item_demotion_basis_points(
    conflicts: &[ConflictView],
    derived_state: &ContextDerivedState,
) -> u16 {
    let mut points = BASIS_POINTS_SCALE;
    if has_unresolved_semantic_conflict(conflicts) {
        points = points * usize::from(CONFLICT_SCORE_MULTIPLIER_BASIS_POINTS) / BASIS_POINTS_SCALE;
    }
    if derived_state.stale_reason.is_some() {
        points = points * usize::from(STALE_SCORE_MULTIPLIER_BASIS_POINTS) / BASIS_POINTS_SCALE;
    }
    u16::try_from(points).unwrap_or(u16::MAX)
}

/// Whether any expanded conflict side is an open *semantic* conflict.
///
/// Governance conflicts are a different fact: they mean the Context has several publication heads,
/// which stays a hard automatic-injection blocker rather than a ranking demotion.
fn has_unresolved_semantic_conflict(conflicts: &[ConflictView]) -> bool {
    conflicts
        .iter()
        .any(|conflict| conflict.kind == "semantic" && conflict.status == "open")
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
    if evidence.context_identifier_tokens.len() >= AUTOMATIC_MIN_IDENTIFIER_QUERY_TOKENS {
        reasons.push(format!(
            "Identifier coverage: Task text named code identifier token(s) {} that the Context \
             spells verbatim, each counted {AUTOMATIC_IDENTIFIER_TOKEN_COVERAGE_WEIGHT} times \
             over in the automatic text-coverage gate",
            evidence
                .context_identifier_tokens
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
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
    /// The matched Space's fused RRF score after every per-item demotion (unresolved semantic
    /// conflict, structured `recheck_when` staleness). Items inside one Space share a base score,
    /// so this only reorders demoted items behind their undemoted siblings.
    injection_score_basis_points: u16,
    direct_path_count: usize,
    item: TaskContextItem,
    /// Compact projection of `item`, materialized only for
    /// [`ContextPackDetailLevel::Compact`] so the budgeter charges the emitted representation.
    compact: Option<CompactTaskContextItem>,
}

#[derive(Debug)]
struct LoadedTaskContexts {
    candidates: Vec<TaskContextCandidate>,
    omitted: Vec<ContextPackOmitted>,
    /// Title and provisional flag of every associated Space, for the compact payload.
    space_headers: BTreeMap<SpaceId, SpaceHeader>,
}

#[derive(Debug)]
struct PackedTaskContexts {
    estimated_tokens: usize,
    associations: Vec<TaskSpaceAssociation>,
    compact_associations: Vec<CompactSpaceAssociation>,
    items: Vec<TaskContextItem>,
    compact_items: Vec<CompactTaskContextItem>,
    graph_diagnostics: Vec<TaskGraphDiagnostic>,
    query_token_explanation: Option<AutomaticQueryTokenExplanation>,
    omitted: Vec<ContextPackOmitted>,
}

/// The Pack's zero-result explanation for its Resolved Focus.
///
/// A named unresolved Reference replaces the generic "not reachable in Graph" rather than joining
/// it: both answer the same question, one of them says which Reference and why, and the generic
/// line is exactly the part an Agent cannot act on.
fn artifact_focus_diagnostics(
    resolved_focus: Option<&ResolvedFocus>,
    reachable: bool,
    unresolved: &[TaskGraphDiagnostic],
) -> Vec<TaskGraphDiagnostic> {
    let Some(focus) = resolved_focus.filter(|_| !reachable) else {
        return Vec::new();
    };
    if unresolved.is_empty() {
        return vec![TaskGraphDiagnostic {
            kind: TaskGraphDiagnosticKind::ArtifactNotReachableInGraph,
            resolved_focus: focus.clone(),
            detail: None,
        }];
    }
    unresolved.to_vec()
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
    detail_level: ContextPackDetailLevel,
    context_ttl: &ContextTtlSettings,
    usage_prior: Option<&dyn UsagePriorSource>,
) -> Result<LoadedTaskContexts> {
    if inference.associations.is_empty() {
        return Ok(LoadedTaskContexts {
            candidates: Vec::new(),
            omitted: Vec::new(),
            space_headers: BTreeMap::new(),
        });
    }
    // Dense rank: Spaces that fused to exactly the same score share one rank, so item ordering
    // falls through to the Context identity tie-break below instead of following the arbitrary
    // Space ID that happened to sort first.
    let mut association_rank = BTreeMap::new();
    let mut previous_score: Option<f64> = None;
    let mut current_rank = 0_usize;
    for (offset, association) in inference.associations.iter().enumerate() {
        if previous_score.is_none_or(|score| !score.total_cmp(&association.score).is_eq()) {
            current_rank = offset;
            previous_score = Some(association.score);
        }
        association_rank.insert(association.space_id, current_rank);
    }
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
                item.auto_injection_eligible, revision.evidence_completeness,
                item.superseded_by, item.stale_reason, item.accepted_at_unix_seconds
         FROM context_revision AS revision
         JOIN context_item AS item USING(context_id)
         JOIN space_projection AS space USING(space_id)
         WHERE (
               revision.space_id IN ({placeholders})
               OR revision.context_id IN (
                   SELECT association.context_id
                   FROM context_space_association_head AS head
                   JOIN context_space_association AS association USING(association_id)
                   WHERE head.context_id IN (
                       SELECT context_id FROM context_space_association_head
                       GROUP BY context_id HAVING COUNT(*) = 1
                   )
                     AND (
                         association.primary_space_id IN ({placeholders})
                         OR EXISTS (
                             SELECT 1 FROM json_each(association.related_space_ids_json) AS related
                             WHERE related.value IN ({placeholders})
                         )
                     )
               )
           )
           AND {revision_clause}
         ORDER BY revision.space_id, revision.context_id, revision.revision_id",
        status = status_expression(),
    );
    let association_parameters = association_rank
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let space_headers = load_space_headers(connection, &association_parameters)?;
    let parameters = association_parameters
        .iter()
        .chain(&association_parameters)
        .chain(&association_parameters)
        .cloned()
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
        if let Some(candidate) = task_context_candidate_from_row(
            connection,
            row,
            inference,
            &association_rank,
            mode,
            context_ttl,
        )? {
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
    apply_usage_prior(&mut candidates, usage_prior);
    candidates.sort_by(|left, right| {
        reached_only_by_a_hop(left)
            .cmp(&reached_only_by_a_hop(right))
            .then_with(|| {
                coverage_weighted_item_score_basis_points(right)
                    .cmp(&coverage_weighted_item_score_basis_points(left))
            })
            .then_with(|| left.association_rank.cmp(&right.association_rank))
            .then_with(|| right.direct_path_count.cmp(&left.direct_path_count))
            // Whatever the Space ranking cannot separate is separated by how much of the query the
            // Context itself answered. Falling straight through to the Context ID would order
            // equally ranked Contexts by an identity that carries no meaning.
            .then_with(|| {
                right
                    .item
                    .context
                    .match_reason
                    .coverage_basis_points
                    .cmp(&left.item.context.match_reason.coverage_basis_points)
            })
            .then_with(|| {
                left.item
                    .context
                    .match_reason
                    .bm25
                    .total_cmp(&right.item.context.match_reason.bm25)
            })
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
    let dropped = candidates.split_off(candidate_limit.min(candidates.len()));
    let omitted = if dropped.is_empty() {
        Vec::new()
    } else if detail_level == ContextPackDetailLevel::Compact {
        dropped
            .iter()
            .map(|candidate| ContextPackOmitted {
                context_id: Some(candidate.item.context.context_id),
                revision_id: Some(candidate.item.context.revision_id),
                title: Some(candidate.item.context.title.clone()),
                space_id: Some(candidate.item.association_space_id),
                reason: "item_candidate_limit".to_owned(),
                estimated_tokens: serialized_tokens(&candidate.item),
                count: 1,
                ..ContextPackOmitted::default()
            })
            .collect()
    } else {
        vec![ContextPackOmitted {
            reason: "item_candidate_limit".to_owned(),
            estimated_tokens: dropped
                .iter()
                .map(|candidate| serialized_tokens(&candidate.item))
                .sum(),
            count: dropped.len(),
            ..ContextPackOmitted::default()
        }]
    };
    if detail_level == ContextPackDetailLevel::Compact {
        let relations = load_compact_relations(connection, &candidates)?;
        let locations = load_compact_locations(connection, &candidates)?;
        for candidate in &mut candidates {
            candidate.compact = Some(compact_task_context_item(
                &candidate.item,
                relations
                    .get(&candidate.item.context.revision_id)
                    .cloned()
                    .unwrap_or_default(),
                locations
                    .get(&candidate.item.context.revision_id)
                    .map_or(&[] as &[String], Vec::as_slice),
            ));
        }
    }
    Ok(LoadedTaskContexts {
        candidates,
        omitted,
        space_headers,
    })
}

/// Names the Spaces `max_spaces` truncated, so an Agent can ask for one of them explicitly
/// instead of only learning that a number of them existed.
///
/// The first [`AUTOMATIC_GATE_OMISSION_LIMIT`] are named with their identity and title; the rest
/// collapse into one counted notice, because a long list of names competes with the facts for the
/// same budget.
fn space_top_k_omissions(
    connection: &Connection,
    dropped: &[TaskSpaceAssociation],
) -> Result<Vec<ContextPackOmitted>> {
    if dropped.is_empty() {
        return Ok(Vec::new());
    }
    let named = dropped.len().min(AUTOMATIC_GATE_OMISSION_LIMIT);
    let space_ids = dropped[..named]
        .iter()
        .map(|association| association.space_id.to_string())
        .collect::<Vec<_>>();
    let headers = load_space_headers(connection, &space_ids)?;
    let mut omitted = dropped[..named]
        .iter()
        .map(|association| ContextPackOmitted {
            title: headers
                .get(&association.space_id)
                .and_then(|header| header.title.clone()),
            space_id: Some(association.space_id),
            reason: "space_top_k".to_owned(),
            estimated_tokens: serialized_tokens(association),
            count: 1,
            ..ContextPackOmitted::default()
        })
        .collect::<Vec<_>>();
    if named < dropped.len() {
        omitted.push(ContextPackOmitted {
            reason: "space_top_k".to_owned(),
            estimated_tokens: dropped[named..].iter().map(serialized_tokens).sum(),
            count: dropped.len() - named,
            ..ContextPackOmitted::default()
        });
    }
    Ok(omitted)
}

/// Reads the Intent-head title and provisional flag of every associated Space.
fn load_space_headers(
    connection: &Connection,
    space_ids: &[String],
) -> Result<BTreeMap<SpaceId, SpaceHeader>> {
    let mut headers = BTreeMap::new();
    if space_ids.is_empty() {
        return Ok(headers);
    }
    let placeholders = std::iter::repeat_n("?", space_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let mut statement = connection
        .prepare(&format!(
            "SELECT space_id, title, provisional FROM space_projection
             WHERE space_id IN ({placeholders})"
        ))
        .map_err(sql_error("prepare Space header retrieval"))?;
    let mut rows = statement
        .query(rusqlite::params_from_iter(space_ids.iter()))
        .map_err(sql_error("execute Space header retrieval"))?;
    while let Some(row) = rows.next().map_err(sql_error("read Space header row"))? {
        let space_id = parse_id::<SpaceId>(
            &row.get::<_, String>(0)
                .map_err(sql_error("read projected Space identity"))?,
        )?;
        let title = row
            .get::<_, Option<String>>(1)
            .map_err(sql_error("read projected Space title"))?
            .filter(|title| !title.is_empty());
        let provisional = row
            .get::<_, bool>(2)
            .map_err(sql_error("read projected Space provisional flag"))?;
        headers.insert(space_id, SpaceHeader { title, provisional });
    }
    Ok(headers)
}

/// Reads the active outgoing Context Relations of every loaded candidate revision.
fn load_compact_relations(
    connection: &Connection,
    candidates: &[TaskContextCandidate],
) -> Result<BTreeMap<RevisionId, Vec<CompactContextRelation>>> {
    let mut relations = BTreeMap::<RevisionId, Vec<CompactContextRelation>>::new();
    if candidates.is_empty() {
        return Ok(relations);
    }
    let mut statement = connection
        .prepare(
            "SELECT target_context_id, kind FROM context_relation
             WHERE source_revision_id = ?1 ORDER BY target_context_id, kind",
        )
        .map_err(sql_error("prepare compact Context Relation retrieval"))?;
    for revision_id in candidates
        .iter()
        .map(|candidate| candidate.item.context.revision_id)
        .collect::<BTreeSet<_>>()
    {
        let mut rows = statement
            .query([revision_id.to_string()])
            .map_err(sql_error("execute compact Context Relation retrieval"))?;
        let mut edges = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(sql_error("read compact Context Relation row"))?
        {
            let target_context_id: ContextId = parse_id(
                &row.get::<_, String>(0)
                    .map_err(sql_error("read Context Relation target"))?,
            )?;
            let kind = parse_context_relation_kind(
                &row.get::<_, String>(1)
                    .map_err(sql_error("read Context Relation kind"))?,
            )?;
            edges.push(CompactContextRelation {
                kind,
                target_context_id,
            });
        }
        if !edges.is_empty() {
            relations.insert(revision_id, edges);
        }
    }
    Ok(relations)
}

/// Repository-qualified Artifact paths recorded as Engineering References on each candidate
/// revision. They are provenance the Agent can open directly, so a compact Evidence entry keeps
/// them even though it drops the Evidence body.
fn load_compact_locations(
    connection: &Connection,
    candidates: &[TaskContextCandidate],
) -> Result<BTreeMap<RevisionId, Vec<String>>> {
    let mut locations = BTreeMap::<RevisionId, Vec<String>>::new();
    if candidates.is_empty() {
        return Ok(locations);
    }
    let mut statement = connection
        .prepare(
            "SELECT repository_id, locator_json FROM engineering_reference
             WHERE revision_id = ?1 ORDER BY reference_id",
        )
        .map_err(sql_error("prepare compact Engineering Reference retrieval"))?;
    for revision_id in candidates
        .iter()
        .map(|candidate| candidate.item.context.revision_id)
        .collect::<BTreeSet<_>>()
    {
        let mut rows = statement
            .query([revision_id.to_string()])
            .map_err(sql_error("execute compact Engineering Reference retrieval"))?;
        let mut paths = BTreeSet::new();
        while let Some(row) = rows
            .next()
            .map_err(sql_error("read compact Engineering Reference row"))?
        {
            let repository_id: String = row
                .get(0)
                .map_err(sql_error("read Engineering Reference Repository"))?;
            let locator: LocatorPath = from_json(
                &row.get::<_, String>(1)
                    .map_err(sql_error("read Engineering Reference locator"))?,
            )?;
            paths.insert(format!("{repository_id}:{}", locator.path));
        }
        if !paths.is_empty() {
            locations.insert(
                revision_id,
                paths
                    .into_iter()
                    .take(COMPACT_EVIDENCE_LOCATION_LIMIT)
                    .collect(),
            );
        }
    }
    Ok(locations)
}

/// Path shared by every `ArtifactLocator` variant. Kind-specific coordinates are irrelevant to a
/// compact location hint.
#[derive(Debug, Deserialize)]
struct LocatorPath {
    path: String,
}

fn compact_task_context_item(
    item: &TaskContextItem,
    relations: Vec<CompactContextRelation>,
    locations: &[String],
) -> CompactTaskContextItem {
    CompactTaskContextItem {
        context_id: item.context.context_id,
        revision_id: item.context.revision_id,
        space_id: item.context.space_id,
        kind: item.context.kind,
        status: item.context.status,
        title: item.context.title.clone(),
        statement: item.context.statement.clone(),
        conditions: item.context.applicability.conditions.clone(),
        evidence: item
            .context
            .evidence
            .iter()
            .map(|evidence| CompactEvidenceView {
                kind: evidence.kind.clone(),
                summary: truncate_chars(
                    &evidence_summary(evidence),
                    COMPACT_EVIDENCE_SUMMARY_MAX_CHARS,
                ),
                locations: locations.to_vec(),
            })
            .collect(),
        relations,
        conflicts: item.context.conflicts.clone(),
        derived_state: item.context.derived_state.clone(),
        retrieval_channels: retrieval_channels(&item.retrieval_paths),
        why: compact_item_reasons(item),
    }
}

/// Deduplicated `source` discriminants of one item's Retrieval Paths, in first-seen order.
fn retrieval_channels(paths: &[TaskRetrievalPath]) -> Vec<String> {
    let mut channels = Vec::new();
    for path in paths {
        let channel = retrieval_channel_name(path);
        if !channels.iter().any(|seen| seen == channel) {
            channels.push(channel.to_owned());
        }
        if channels.len() == COMPACT_RETRIEVAL_CHANNEL_LIMIT {
            break;
        }
    }
    channels
}

/// The `source` tag one Retrieval Path serializes under, kept in step with its `serde` rename.
const fn retrieval_channel_name(path: &TaskRetrievalPath) -> &'static str {
    match path {
        TaskRetrievalPath::EngineeringGraph { .. } => "engineering_graph",
        TaskRetrievalPath::ContextRelation { .. } => "context_relation",
        TaskRetrievalPath::SpaceAssociation { .. } => "space_association",
        TaskRetrievalPath::GraphDiagnostic { .. } => "graph_diagnostic",
        TaskRetrievalPath::IntentFts { .. } => "intent_fts",
        TaskRetrievalPath::ContextFts { .. } => "context_fts",
        TaskRetrievalPath::WorkingIntentHintText { .. } => "working_intent_hint_text",
        TaskRetrievalPath::ResolvedFocusTextFallback { .. } => "resolved_focus_text_fallback",
        TaskRetrievalPath::ExactScope { .. } => "exact_scope",
    }
}

/// Self-contained Evidence summary. Checkpoint Evidence stores it as `content.summary`; other
/// Evidence falls back to its interpretation and then to its supported statement.
fn evidence_summary(evidence: &EvidenceView) -> String {
    if let Some(summary) = evidence
        .content
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
    {
        return summary.to_owned();
    }
    if !evidence.interpretation.trim().is_empty() {
        return evidence.interpretation.clone();
    }
    evidence.supports.clone()
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    if value.chars().count() <= maximum {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(maximum).collect::<String>();
    truncated.push('\u{2026}');
    truncated
}

/// At most [`COMPACT_ITEM_REASON_LIMIT`] one-sentence reasons, ordered from the strongest
/// retrieval path to the weakest, so a compact item stays explainable without its path payload.
fn compact_item_reasons(item: &TaskContextItem) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut relation_hops = 0;
    let mut graph = false;
    let mut focus_fallback = false;
    let mut text = false;
    let mut scopes = Vec::new();
    for path in &item.retrieval_paths {
        match path {
            TaskRetrievalPath::EngineeringGraph { .. } => graph = true,
            TaskRetrievalPath::ContextRelation { hops } => {
                relation_hops = relation_hops.max(hops.len());
            }
            TaskRetrievalPath::ResolvedFocusTextFallback { .. } => focus_fallback = true,
            TaskRetrievalPath::IntentFts { .. }
            | TaskRetrievalPath::ContextFts { .. }
            | TaskRetrievalPath::WorkingIntentHintText { .. } => text = true,
            TaskRetrievalPath::ExactScope { dimension, value } => {
                scopes.push(format!("{dimension}={value}"));
            }
            TaskRetrievalPath::SpaceAssociation { .. }
            | TaskRetrievalPath::GraphDiagnostic { .. } => {}
        }
    }
    if graph {
        reasons.push(
            "Resolved a current-generation Engineering Artifact association for this Task focus."
                .to_owned(),
        );
    }
    if relation_hops > 0 {
        reasons.push(format!(
            "Reached through {relation_hops} stable Context Relation hop(s)."
        ));
    }
    if focus_fallback {
        reasons.push("Matched the resolved Artifact Focus by text only.".to_owned());
    }
    if text && !item.context.match_reason.matched_tokens.is_empty() {
        reasons.push(format!(
            "Matched Task text on: {} (coverage-weighted rank: this Context answered {}% of the \
             query itself).",
            item.context
                .match_reason
                .matched_tokens
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            item.context.match_reason.coverage_basis_points / 100
        ));
    } else if text {
        reasons.push("Matched the Task Intent text of its ContextSpace.".to_owned());
    }
    if !scopes.is_empty() {
        reasons.push(format!(
            "Applicability matched the Task scope: {}.",
            scopes.join(", ")
        ));
    }
    for sentence in derived_state_reasons(&item.context) {
        reasons.push(sentence);
    }
    if let Some(sentence) = usage_prior_reason(item.context.usage) {
        reasons.push(sentence);
    }
    reasons.truncate(COMPACT_ITEM_REASON_LIMIT);
    reasons
}

/// One sentence for the usage prior, and only when the prior actually moved the score.
///
/// Reporting an ignore that changed nothing would read as a warning the ranking never applied.
fn usage_prior_reason(usage: ContextUsageCounts) -> Option<String> {
    if usage.reused >= 1 {
        return Some(format!("Reused in {} prior task(s).", usage.reused));
    }
    (usage.ignored >= USAGE_IGNORED_PENALTY_MINIMUM_TASKS)
        .then(|| format!("Ignored in {} prior task(s).", usage.ignored))
}

/// One-sentence reasons for state the retrieval paths cannot express.
///
/// They are appended last but survive truncation in practice because they replace the generic
/// conflict sentence with the identity an Agent needs to read the other side.
fn derived_state_reasons(context: &ContextPackItem) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut opposing = context
        .conflicts
        .iter()
        .filter(|conflict| conflict.kind == "semantic" && conflict.status == "open")
        .flat_map(|conflict| conflict.participants.iter())
        .map(|side| side.context_id)
        .filter(|participant| *participant != context.context_id)
        .collect::<Vec<_>>();
    opposing.sort_unstable();
    opposing.dedup();
    if !opposing.is_empty() {
        reasons.push(format!(
            "Unresolved semantic conflict with {}; read both sides before relying on it.",
            opposing
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if !context.conflicts.is_empty() {
        reasons.push(
            "Carries an unresolved conflict; read both sides before relying on it.".to_owned(),
        );
    }
    if let Some(reason) = &context.derived_state.stale_reason {
        reasons.push(format!("Possibly stale: {reason}."));
    }
    if let Some(context_id) = context.derived_state.superseded_by {
        reasons.push(format!(
            "Superseded by {context_id}; excluded from automatic injection."
        ));
    }
    if let Some(reason) = &context.derived_state.historical_reason {
        reasons.push(format!(
            "Historical: {reason}; excluded from automatic injection."
        ));
    }
    reasons
}

fn matched_context_space(
    inference: &TaskAssociationInference,
    association_rank: &BTreeMap<SpaceId, usize>,
    context_id: ContextId,
    fallback_space_id: SpaceId,
) -> Option<MatchedContextSpace> {
    if let Some(effective) = inference.effective_spaces.get(&context_id) {
        let role = effective
            .roles
            .iter()
            .filter_map(|role| {
                association_rank
                    .get(&role.matched_space_id)
                    .map(|rank| (*rank, *role))
            })
            .min_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then_with(|| left.1.matched_space_id.cmp(&right.1.matched_space_id))
            })?
            .1;
        return Some(MatchedContextSpace {
            association_space_id: role.matched_space_id,
            association_path: Some(TaskRetrievalPath::SpaceAssociation {
                association_id: effective.association_id,
                role: role.role,
                matched_space_id: role.matched_space_id,
            }),
        });
    }
    association_rank
        .contains_key(&fallback_space_id)
        .then_some(MatchedContextSpace {
            association_space_id: fallback_space_id,
            association_path: None,
        })
}

fn graph_context_candidate(
    inference: &TaskAssociationInference,
    association_rank: &BTreeMap<SpaceId, usize>,
    graph: &GraphContextEvidence,
    mode: ContextPackMode,
) -> Result<Option<TaskContextCandidate>> {
    let snapshot = &graph.snapshot;
    let Some(matched_space) = matched_context_space(
        inference,
        association_rank,
        snapshot.context_id,
        snapshot.space_id,
    ) else {
        return Ok(None);
    };
    let rank = association_rank
        .get(&matched_space.association_space_id)
        .expect("matched Graph Context Space has an association rank");
    if mode == ContextPackMode::AutomaticInjection && !snapshot.safety.automatic_injection_eligible
    {
        return Ok(None);
    }
    let Some(artifact_generation) = inference.graph_artifact_generation.clone() else {
        return Err(invariant(
            "Graph Context candidate is missing its Artifact Generation",
        ));
    };
    let mut paths = task_retrieval_paths(
        inference
            .evidence
            .get(&matched_space.association_space_id)
            .ok_or_else(|| invariant("Graph Context candidate has no Space association"))?,
        Some(&graph.evidence),
    );
    paths.extend(matched_space.association_path);
    sort_dedup_paths(&mut paths);
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
        injection_score_basis_points: inference
            .evidence
            .get(&matched_space.association_space_id)
            .map_or(0, final_score_basis_points),
        direct_path_count,
        item: TaskContextItem {
            association_space_id: matched_space.association_space_id,
            context: ContextPackItem {
                space_id: snapshot.space_id,
                context_id: snapshot.context_id,
                revision_id: snapshot.revision.revision_id,
                title: context_display_title(&snapshot.revision.statement),
                space_title: snapshot.space_title.clone(),
                kind: snapshot.revision.kind,
                status: graph_context_status(snapshot.status),
                statement: snapshot.revision.statement.clone(),
                rationale: Some(snapshot.revision.rationale.clone()),
                applicability: snapshot.revision.applicability.clone(),
                evidence,
                conflicts: Vec::new(),
                derived_state: ContextDerivedState::default(),
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
                    inference.coverage_basis.tokens(),
                ),
                usage: ContextUsageCounts::default(),
                detail: ContextPackDetail::Full,
            },
            retrieval_paths: paths,
        },
        compact: None,
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

#[allow(clippy::too_many_lines)]
fn task_context_candidate_from_row(
    connection: &Connection,
    row: &rusqlite::Row<'_>,
    inference: &TaskAssociationInference,
    association_rank: &BTreeMap<SpaceId, usize>,
    mode: ContextPackMode,
    context_ttl: &ContextTtlSettings,
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
    let Some(matched_space) =
        matched_context_space(inference, association_rank, context_id, space_id)
    else {
        return Ok(None);
    };
    let Some(space_evidence) = inference.evidence.get(&matched_space.association_space_id) else {
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
    if mode == ContextPackMode::AutomaticInjection
        && !automatic_context_text_eligible(
            space_evidence,
            context_evidence,
            &inference.coverage_basis,
        )
    {
        return Ok(None);
    }
    let mut paths = task_retrieval_paths(space_evidence, context_evidence);
    paths.extend(matched_space.association_path.clone());
    sort_dedup_paths(&mut paths);
    if paths.is_empty() {
        return Ok(None);
    }
    let space_title = row
        .get(3)
        .map_err(sql_error("read Task Context Space title"))?;
    let kind = parse_kind(
        &row.get::<_, String>(4)
            .map_err(sql_error("read Task Context kind"))?,
    )?;
    let status = parse_status(
        &row.get::<_, String>(5)
            .map_err(sql_error("read Task Context status"))?,
    )?;
    let statement: String = row
        .get(6)
        .map_err(sql_error("read Task Context statement"))?;
    let title = context_display_title(&statement);
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
    let superseded_by = row
        .get::<_, Option<String>>(11)
        .map_err(sql_error("read Task Context superseding Context"))?
        .map(|value| parse_id(&value))
        .transpose()?;
    let stale_reason = row
        .get::<_, Option<String>>(12)
        .map_err(sql_error("read Task Context stale reason"))?;
    let accepted_at = row
        .get::<_, Option<i64>>(13)
        .map_err(sql_error("read Task Context publication time"))?;
    let mut derived_state = ContextDerivedState {
        superseded_by,
        stale_reason,
        historical_reason: context_ttl.historical_reason(kind, accepted_at),
        demotion_basis_points: None,
    };
    let evidence = load_evidence(connection, revision_id)?;
    let conflicts = load_conflicts(connection, context_id, revision_id)?;
    let demotion = item_demotion_basis_points(&conflicts, &derived_state);
    derived_state.demotion_basis_points =
        (usize::from(demotion) < BASIS_POINTS_SCALE).then_some(demotion);
    let auto_injection_eligible =
        auto_injection_eligible && !derived_state.blocks_automatic_injection();
    if mode == ContextPackMode::AutomaticInjection
        && (status != ContextStatus::Accepted
            || !auto_injection_eligible
            || evidence.is_empty()
            || !conflicts.is_empty())
    {
        return Ok(None);
    }
    let match_reason = context_match_reason(
        context_evidence,
        evidence_completeness,
        inference.coverage_basis.tokens(),
    );
    let direct_path_count = paths
        .iter()
        .filter(|path| !matches!(path, TaskRetrievalPath::IntentFts { .. }))
        .count();
    Ok(Some(TaskContextCandidate {
        association_rank: *association_rank
            .get(&matched_space.association_space_id)
            .expect("candidate Space comes from association set"),
        injection_score_basis_points: demoted_item_score(
            final_score_basis_points(space_evidence),
            &conflicts,
            &derived_state,
        ),
        direct_path_count,
        item: TaskContextItem {
            association_space_id: matched_space.association_space_id,
            context: ContextPackItem {
                space_id,
                context_id,
                revision_id,
                title,
                space_title,
                kind,
                status,
                statement,
                rationale: Some(rationale),
                applicability,
                evidence,
                conflicts,
                derived_state,
                auto_injection_eligible,
                safety_source: ContextSafetySource::CurrentProjection,
                match_reason,
                usage: ContextUsageCounts::default(),
                detail: ContextPackDetail::Full,
            },
            retrieval_paths: paths,
        },
        compact: None,
    }))
}

fn automatic_context_text_eligible(
    space: &AssociationEvidence,
    context: Option<&AcceptedContextEvidence>,
    coverage_basis: &AutomaticCoverageBasis,
) -> bool {
    let Some(context) = context else {
        return automatic_inherited_space_text_eligible(space, coverage_basis);
    };
    if !context.matched_artifacts.is_empty()
        || context.graph_paths.iter().any(|path| {
            matches!(
                path,
                TaskRetrievalPath::EngineeringGraph { .. }
                    | TaskRetrievalPath::ContextRelation { .. }
                    | TaskRetrievalPath::ResolvedFocusTextFallback { .. }
            )
        })
    {
        return true;
    }
    if automatic_direct_context_text_eligible(context, coverage_basis) {
        return true;
    }
    let direct_text_channels = usize::from(context.textual_match) + context.hint_text.len();
    let all_text_channels = direct_text_channels
        + usize::from(space.intent_matched)
        + space
            .hint_text
            .keys()
            .filter(|channel| channel.target == WorkingIntentHintTarget::SpaceIntentFts)
            .count();
    direct_text_channels > 0 && all_text_channels >= 2
}

fn automatic_inherited_space_text_eligible(
    space: &AssociationEvidence,
    coverage_basis: &AutomaticCoverageBasis,
) -> bool {
    let space_hints = space
        .hint_text
        .iter()
        .filter(|(channel, _)| channel.target == WorkingIntentHintTarget::SpaceIntentFts)
        .map(|(_, hint)| hint)
        .collect::<Vec<_>>();
    if space.intent_phrase_match || space_hints.iter().any(|hint| hint.phrase_match) {
        return true;
    }
    let coverage = token_coverage_basis_points(&space.intent_tokens, coverage_basis.tokens()).max(
        space_hints
            .iter()
            .map(|hint| hint.coverage_basis_points())
            .max()
            .unwrap_or(0),
    );
    if automatic_coverage_passes(coverage, coverage_basis) {
        return true;
    }
    usize::from(space.intent_matched) + space_hints.len() >= 2
}

fn automatic_direct_context_text_eligible(
    context: &AcceptedContextEvidence,
    coverage_basis: &AutomaticCoverageBasis,
) -> bool {
    if context.phrase_match || context.hint_text.values().any(|hint| hint.phrase_match) {
        return true;
    }
    let coverage = identifier_weighted_coverage_basis_points(
        &context.matched_tokens,
        &context.identifier_matched_tokens,
        coverage_basis.tokens(),
    )
    .max(
        context
            .hint_text
            .values()
            .map(HintTextEvidence::coverage_basis_points)
            .max()
            .unwrap_or(0),
    );
    if automatic_coverage_passes(coverage, coverage_basis) {
        return true;
    }
    let direct_text_channels = usize::from(context.textual_match) + context.hint_text.len();
    direct_text_channels >= 2 || (direct_text_channels > 0 && !context.matched_scopes.is_empty())
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
    query_tokens: &[String],
) -> MatchReason {
    let matched_tokens = context
        .into_iter()
        .flat_map(|value| value.matched_tokens.iter().cloned())
        .collect::<BTreeSet<_>>();
    MatchReason {
        matched_fields: context
            .into_iter()
            .flat_map(|value| value.matched_fields.iter().copied())
            .collect(),
        coverage_basis_points: token_coverage_basis_points(&matched_tokens, query_tokens),
        matched_tokens: matched_tokens.into_iter().collect(),
        bm25: context.and_then(|value| value.bm25).unwrap_or(0.0),
        evidence_completeness: u16::try_from(evidence_completeness).unwrap_or(u16::MAX),
        structured_filter_match: true,
        matched_via_alias: context
            .into_iter()
            .flat_map(|value| value.alias_matches.iter().cloned())
            .collect(),
    }
}

/// True when a Context never mentions any of the query text and was reached only by following a
/// relation out of another Context in its Space.
///
/// Such a Context is background for the answer, not the answer: the contradicted side of a
/// contradiction, or a sibling in the same Space. It is still returned and still explains its own
/// route, but it never leads the Pack ahead of a Context that answered the query directly.
/// Coverage at which a text-matched Context counts as having answered the query in full for
/// ranking.
///
/// It saturated at 3000 while coverage divided by every selected query token, because Han bigrams
/// and English stop words the corpus never wrote down made full coverage unreachable and the
/// multiplier had to separate answers somewhere inside the reachable range. Coverage now divides
/// by the tokens this corpus can answer, where matching all of them is both reachable and exactly
/// what "answered the query in full" means, so the saturation is that same full scale: a Context
/// answering four of four answerable tokens has to outrank one answering one of them, and an
/// early ceiling made those two nearly indistinguishable.
const ITEM_TEXT_COVERAGE_SATURATION_BASIS_POINTS: u16 = 10_000;
/// Multiplier a text-matched Context keeps when it covered none of the query beyond the token that
/// found it. It is a demotion, never an exclusion: such a Context is still an answer, just not the
/// one that answered most of the question.
const ITEM_TEXT_COVERAGE_MULTIPLIER_FLOOR_BASIS_POINTS: u16 = 5_000;

/// The matched Space's fused score, scaled by how much of the query this Context's own text
/// answered.
///
/// The fused score is a property of the *Space*: every Context a Space contributes carries the
/// same one, so ordering by it alone lets the Space that won on Intent text put forward whichever
/// of its Contexts happens to share one phrase with the query, ahead of a Context in a
/// marginally lower-ranked Space whose own statement answered most of it. Coverage-weighting the
/// score is what stops a short near-duplicate from outranking the longer Context that actually
/// covers the question.
///
/// Contexts that were not reached by their own text at all — Engineering Graph hits, Relation hops,
/// scope matches — keep the Space score unscaled: their coverage is zero because text played no
/// part in finding them, not because they answered little.
fn coverage_weighted_item_score_basis_points(candidate: &TaskContextCandidate) -> u16 {
    let multiplier = item_text_coverage_multiplier_basis_points(candidate);
    u16::try_from(
        usize::from(candidate.injection_score_basis_points).saturating_mul(usize::from(multiplier))
            / BASIS_POINTS_SCALE,
    )
    .unwrap_or(u16::MAX)
}

/// Ranking multiplier in basis points; `BASIS_POINTS_SCALE` is none.
fn item_text_coverage_multiplier_basis_points(candidate: &TaskContextCandidate) -> u16 {
    let matched_by_own_text = candidate.item.retrieval_paths.iter().any(|path| {
        matches!(
            path,
            TaskRetrievalPath::ContextFts { .. } | TaskRetrievalPath::WorkingIntentHintText { .. }
        )
    });
    if !matched_by_own_text {
        return u16::try_from(BASIS_POINTS_SCALE).unwrap_or(u16::MAX);
    }
    let coverage = candidate
        .item
        .context
        .match_reason
        .coverage_basis_points
        .min(ITEM_TEXT_COVERAGE_SATURATION_BASIS_POINTS);
    let span = BASIS_POINTS_SCALE - usize::from(ITEM_TEXT_COVERAGE_MULTIPLIER_FLOOR_BASIS_POINTS);
    let earned = span.saturating_mul(usize::from(coverage))
        / usize::from(ITEM_TEXT_COVERAGE_SATURATION_BASIS_POINTS);
    ITEM_TEXT_COVERAGE_MULTIPLIER_FLOOR_BASIS_POINTS
        .saturating_add(u16::try_from(earned).unwrap_or(u16::MAX))
}

fn reached_only_by_a_hop(candidate: &TaskContextCandidate) -> bool {
    candidate.item.context.match_reason.coverage_basis_points == 0
        && candidate.item.retrieval_paths.iter().all(|path| {
            matches!(
                path,
                TaskRetrievalPath::ContextRelation { .. }
                    | TaskRetrievalPath::SpaceAssociation { .. }
            )
        })
}

fn pack_task_context_candidates(
    loaded: LoadedTaskContexts,
    token_budget: usize,
    inference: TaskAssociationInference,
    graph_diagnostics: Vec<TaskGraphDiagnostic>,
    space_omissions: Vec<ContextPackOmitted>,
    detail_level: ContextPackDetailLevel,
) -> PackedTaskContexts {
    let mut omitted = loaded.omitted;
    omitted.extend(space_omissions);
    // The retrieval gate's own omissions are carried apart from the budget omissions: they are
    // everything a Pack that returned nothing has to say, and nothing a Pack that returned a
    // Context should give a Context up for.
    let gate_omitted = inference.omitted.clone();
    // Both detail levels report the token selection. A compact Pack gets the projection rather
    // than nothing, because the reader most in need of it is the one whose Pack came back empty.
    let query_token_explanation = Some(match detail_level {
        ContextPackDetailLevel::Full => inference.query_token_explanation.clone(),
        ContextPackDetailLevel::Compact => inference.query_token_explanation.compact_projection(),
    });
    match detail_level {
        ContextPackDetailLevel::Full => pack_full_task_context(
            loaded.candidates,
            inference.associations,
            graph_diagnostics,
            &omitted,
            &gate_omitted,
            token_budget,
            query_token_explanation,
        ),
        ContextPackDetailLevel::Compact => {
            let associations = inference
                .associations
                .into_iter()
                .map(|association| {
                    let header = loaded
                        .space_headers
                        .get(&association.space_id)
                        .cloned()
                        .unwrap_or_default();
                    compact_association(association, &header)
                })
                .collect();
            pack_compact_task_context(
                loaded.candidates,
                associations,
                graph_diagnostics,
                &omitted,
                &gate_omitted,
                token_budget,
                query_token_explanation,
            )
        }
    }
}

/// Drops the Task identity, the machine-readable fusion payload, the Artifact/Context match lists
/// already carried by the returned items, and the Relation path explanations.
fn compact_association(
    association: TaskSpaceAssociation,
    header: &SpaceHeader,
) -> CompactSpaceAssociation {
    CompactSpaceAssociation {
        space_id: association.space_id,
        title: header.title.clone(),
        score: association.score,
        provisional: header.provisional,
        reasons: association
            .reasons
            .into_iter()
            .filter(|reason| !reason.starts_with('{'))
            .take(COMPACT_ASSOCIATION_REASON_LIMIT)
            .collect(),
    }
}

/// Packs a compact payload facts-first.
///
/// Items are placed in rank order until they have used [`COMPACT_ITEM_BUDGET_BASIS_POINTS`] of the
/// budget; only then do Space associations, Graph diagnostics and omission notices compete for
/// what is left. [`COMPACT_MIN_ITEMS`] items are never dropped for want of room: an oversized item
/// has its Evidence summaries squeezed to [`COMPACT_SQUEEZED_EVIDENCE_SUMMARY_MAX_CHARS`] and is
/// packed anyway, because an Agent that receives one truncated fact is better off than one that
/// receives a longer list of what it did not get.
#[allow(clippy::too_many_lines)]
fn pack_compact_task_context(
    candidates: Vec<TaskContextCandidate>,
    mut associations: Vec<CompactSpaceAssociation>,
    mut graph_diagnostics: Vec<TaskGraphDiagnostic>,
    base_omitted: &[ContextPackOmitted],
    gate_omitted: &[ContextPackOmitted],
    token_budget: usize,
    query_token_explanation: Option<AutomaticQueryTokenExplanation>,
) -> PackedTaskContexts {
    let ranked = candidates
        .into_iter()
        .enumerate()
        .map(|(rank, candidate)| {
            (
                rank,
                candidate
                    .compact
                    .expect("compact Task Context candidates carry their compact projection"),
            )
        })
        .collect::<Vec<_>>();
    let item_budget = token_budget.saturating_mul(COMPACT_ITEM_BUDGET_BASIS_POINTS) / 10_000;
    let mut items: Vec<RankedItem> = Vec::new();
    let mut dropped: Vec<RankedItem> = Vec::new();
    for (rank, item) in ranked {
        if dropped.is_empty() && fits(&items, &item, item_budget) {
            items.push((rank, item));
            continue;
        }
        if items.len() < COMPACT_MIN_ITEMS {
            let squeezed = squeeze_compact_item(item.clone());
            if fits(&items, &squeezed, token_budget) {
                items.push((rank, squeezed));
                continue;
            }
        }
        dropped.push((rank, item));
    }

    let mut space_omitted = OmissionAggregate::default();
    let mut diagnostic_omitted = OmissionAggregate::default();
    loop {
        let carried = ranked_values(&items);
        let omissions = |carry_gate: bool| {
            compact_budget_omissions(
                &with_gate_omissions(base_omitted, gate_omitted, carry_gate),
                &ranked_values(&dropped),
                space_omitted,
                diagnostic_omitted,
            )
        };
        let charge = |carry_gate: bool, explanation: Option<&AutomaticQueryTokenExplanation>| {
            charged_task_context_tokens(
                &associations,
                &carried,
                &graph_diagnostics,
                &omissions(carry_gate),
                explanation,
            )
        };
        let budgeted = budgeted_diagnostics(
            &charge,
            query_token_explanation.as_ref(),
            carried.is_empty(),
            token_budget,
        );
        if let Some(budgeted) = budgeted {
            let effective_base =
                with_gate_omissions(base_omitted, gate_omitted, budgeted.carry_gate_omissions);
            // The item share is a floor, not a ceiling: whatever the explanations left unspent
            // goes back to the highest ranked Context the first pass could not afford.
            if let Some((probe_items, probe_dropped)) = backfill(
                &items,
                &dropped,
                &associations,
                &graph_diagnostics,
                &effective_base,
                space_omitted,
                diagnostic_omitted,
                token_budget,
                budgeted.explanation.as_ref(),
            ) {
                items = probe_items;
                dropped = probe_dropped;
                continue;
            }
            return PackedTaskContexts {
                estimated_tokens: budgeted.estimated_tokens,
                associations: Vec::new(),
                compact_associations: associations,
                items: Vec::new(),
                compact_items: carried,
                graph_diagnostics,
                query_token_explanation: budgeted.explanation,
                omitted: omissions(budgeted.carry_gate_omissions),
            };
        }
        // Explanations yield to facts: the Space list and the Graph diagnostics go first, and only
        // then is an item given up.
        if let Some(association) = associations.pop() {
            space_omitted.add(serialized_tokens(&association));
            continue;
        }
        if let Some(diagnostic) = graph_diagnostics.pop() {
            diagnostic_omitted.add(serialized_tokens(&diagnostic));
            continue;
        }
        if let Some(item) = items.pop() {
            dropped.push(item);
            dropped.sort_by_key(|(rank, _)| *rank);
            continue;
        }
        let current_omitted = compact_budget_omissions(
            &with_gate_omissions(base_omitted, gate_omitted, true),
            &ranked_values(&dropped),
            space_omitted,
            diagnostic_omitted,
        );
        return exhausted_task_context(&current_omitted, query_token_explanation, token_budget);
    }
}

/// Strips the packing ranks off a rank-ordered item list.
fn ranked_values(items: &[RankedItem]) -> Vec<CompactTaskContextItem> {
    items.iter().map(|(_, item)| item.clone()).collect()
}

/// Tries to re-admit the highest ranked dropped item into the unspent remainder of the budget.
/// One item paired with the packing rank that keeps the emitted order stable.
type RankedItem = (usize, CompactTaskContextItem);

#[allow(clippy::too_many_arguments)]
fn backfill(
    items: &[RankedItem],
    dropped: &[RankedItem],
    associations: &[CompactSpaceAssociation],
    graph_diagnostics: &[TaskGraphDiagnostic],
    base_omitted: &[ContextPackOmitted],
    space_omitted: OmissionAggregate,
    diagnostic_omitted: OmissionAggregate,
    token_budget: usize,
    query_token_explanation: Option<&AutomaticQueryTokenExplanation>,
) -> Option<(Vec<RankedItem>, Vec<RankedItem>)> {
    let position = dropped
        .iter()
        .enumerate()
        .min_by_key(|(_, (rank, _))| *rank)
        .map(|(position, _)| position)?;
    let mut probe_items = items.to_vec();
    probe_items.push(dropped[position].clone());
    probe_items.sort_by_key(|(rank, _)| *rank);
    let mut probe_dropped = dropped.to_vec();
    probe_dropped.remove(position);
    let probe_omitted = compact_budget_omissions(
        base_omitted,
        &ranked_values(&probe_dropped),
        space_omitted,
        diagnostic_omitted,
    );
    let probe_tokens = charged_task_context_tokens(
        associations,
        &ranked_values(&probe_items),
        graph_diagnostics,
        &probe_omitted,
        query_token_explanation,
    );
    (probe_tokens <= token_budget).then_some((probe_items, probe_dropped))
}

/// True when `item` still fits beside the already packed items inside `budget`.
fn fits(items: &[RankedItem], item: &CompactTaskContextItem, budget: usize) -> bool {
    let mut probe = ranked_values(items);
    probe.push(item.clone());
    charged_task_context_tokens::<CompactSpaceAssociation, _>(&[], &probe, &[], &[], None) <= budget
}

/// Squeezes one oversized item to its shortest still-useful form.
fn squeeze_compact_item(mut item: CompactTaskContextItem) -> CompactTaskContextItem {
    for evidence in &mut item.evidence {
        evidence.summary = elide(
            &evidence.summary,
            COMPACT_SQUEEZED_EVIDENCE_SUMMARY_MAX_CHARS,
        );
    }
    item
}

/// Truncates one text to `max_chars` characters, marking the elision.
fn elide(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut elided = value.chars().take(max_chars).collect::<String>();
    elided.push('\u{2026}');
    elided
}

#[allow(clippy::too_many_lines)]
fn pack_full_task_context(
    candidates: Vec<TaskContextCandidate>,
    mut associations: Vec<TaskSpaceAssociation>,
    mut graph_diagnostics: Vec<TaskGraphDiagnostic>,
    omitted: &[ContextPackOmitted],
    gate_omitted: &[ContextPackOmitted],
    token_budget: usize,
    query_token_explanation: Option<AutomaticQueryTokenExplanation>,
) -> PackedTaskContexts {
    let mut items = candidates
        .into_iter()
        .map(|candidate| candidate.item)
        .collect::<Vec<_>>();
    let mut detail_omitted = OmissionAggregate::default();
    let mut item_omitted = OmissionAggregate::default();
    let mut space_omitted = OmissionAggregate::default();
    let mut diagnostic_omitted = OmissionAggregate::default();

    loop {
        let omissions = |carry_gate: bool| {
            task_budget_omissions(
                &with_gate_omissions(omitted, gate_omitted, carry_gate),
                detail_omitted,
                item_omitted,
                space_omitted,
                diagnostic_omitted,
            )
        };
        let charge = |carry_gate: bool, explanation: Option<&AutomaticQueryTokenExplanation>| {
            charged_task_context_tokens::<TaskSpaceAssociation, _>(
                &associations,
                &items,
                &graph_diagnostics,
                &omissions(carry_gate),
                explanation,
            )
        };
        if let Some(budgeted) = budgeted_diagnostics(
            &charge,
            query_token_explanation.as_ref(),
            items.is_empty(),
            token_budget,
        ) {
            return PackedTaskContexts {
                estimated_tokens: budgeted.estimated_tokens,
                associations,
                compact_associations: Vec::new(),
                items,
                compact_items: Vec::new(),
                graph_diagnostics,
                query_token_explanation: budgeted.explanation,
                omitted: omissions(budgeted.carry_gate_omissions),
            };
        }
        let current_omitted = omissions(true);

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

        return exhausted_task_context(&current_omitted, query_token_explanation, token_budget);
    }
}

/// Appends the retrieval-gate omissions to the budget omissions when the budget can afford them.
fn with_gate_omissions(
    omitted: &[ContextPackOmitted],
    gate_omitted: &[ContextPackOmitted],
    carry_gate: bool,
) -> Vec<ContextPackOmitted> {
    let mut all = omitted.to_vec();
    if carry_gate {
        all.extend_from_slice(gate_omitted);
    }
    all
}

/// One affordable way to explain the Pack: what it costs, and what survives at that cost.
struct BudgetedDiagnostics {
    estimated_tokens: usize,
    explanation: Option<AutomaticQueryTokenExplanation>,
    carry_gate_omissions: bool,
}

/// Fits the retrieval explanations into what is left of the budget, or reports that the payload
/// has to give a fact up first.
///
/// They degrade in the order they stop being worth their tokens: the full token selection, then
/// its projection, then the list of Spaces the text gate dropped, then nothing. The last two
/// steps are taken only once the Pack carries an item that can speak for itself -- a Pack that
/// came back empty keeps every explanation and gives up a Space or a diagnostic instead, because
/// being told nothing without being told why is the failure this whole reason chain exists to
/// prevent. And a Pack that did retrieve something has already answered the question the
/// explanations exist to answer, so no Context is ever given up to make room for one.
fn budgeted_diagnostics(
    charge: &impl Fn(bool, Option<&AutomaticQueryTokenExplanation>) -> usize,
    explanation: Option<&AutomaticQueryTokenExplanation>,
    pack_is_empty: bool,
    token_budget: usize,
) -> Option<BudgetedDiagnostics> {
    let projected = explanation.map(AutomaticQueryTokenExplanation::compact_projection);
    let mut tiers = vec![(true, explanation.cloned()), (true, projected.clone())];
    if !pack_is_empty {
        tiers.push((false, projected));
        tiers.push((false, None));
    }
    tiers.into_iter().find_map(|(carry_gate, explanation)| {
        let estimated_tokens = charge(carry_gate, explanation.as_ref());
        (estimated_tokens <= token_budget).then_some(BudgetedDiagnostics {
            estimated_tokens,
            explanation,
            carry_gate_omissions: carry_gate,
        })
    })
}

/// Last resort when even one aggregated omission list exceeds the budget: keep nothing but one
/// collapsed omission so the caller still learns that the Pack was dropped.
///
/// The token-selection explanation survives everything the ordinary packing loop gives up, but
/// here there is nothing left to give up instead, and a Pack that overruns the budget it was
/// handed is worse than one that cannot say why it is empty. So it degrades to its compact
/// projection and, only if even that does not fit, to nothing.
fn exhausted_task_context(
    current_omitted: &[ContextPackOmitted],
    query_token_explanation: Option<AutomaticQueryTokenExplanation>,
    token_budget: usize,
) -> PackedTaskContexts {
    let collapsed = vec![ContextPackOmitted {
        reason: "omitted".to_owned(),
        estimated_tokens: current_omitted
            .iter()
            .map(|omitted| omitted.estimated_tokens)
            .sum(),
        count: current_omitted.iter().map(|omitted| omitted.count).sum(),
        ..ContextPackOmitted::default()
    }];
    let charge = |explanation: Option<&AutomaticQueryTokenExplanation>| {
        charged_task_context_tokens::<TaskSpaceAssociation, TaskContextItem>(
            &[],
            &[],
            &[],
            &collapsed,
            explanation,
        )
    };
    let mut query_token_explanation =
        query_token_explanation.map(|explanation| explanation.compact_projection());
    let mut estimated_tokens = charge(query_token_explanation.as_ref());
    if estimated_tokens > token_budget {
        query_token_explanation = None;
        estimated_tokens = charge(None);
    }
    PackedTaskContexts {
        estimated_tokens,
        associations: Vec::new(),
        compact_associations: Vec::new(),
        items: Vec::new(),
        compact_items: Vec::new(),
        graph_diagnostics: Vec::new(),
        query_token_explanation,
        omitted: collapsed,
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
                reason: reason.to_owned(),
                estimated_tokens: aggregate.estimated_tokens,
                count: aggregate.count,
                ..ContextPackOmitted::default()
            });
        }
    }
    omitted
}

/// Names the first [`COMPACT_NAMED_OMISSION_LIMIT`] dropped Contexts and collapses the rest.
///
/// A named omission is an invitation to fetch one Context by ID; past a handful of them the list
/// stops being actionable and starts competing with the facts for the same budget, which is the
/// failure this packer exists to prevent. Each name is a Context ID and an elided title, nothing
/// more: the revision identity and the full title are one `context_get` away.
fn compact_budget_omissions(
    base: &[ContextPackOmitted],
    dropped_items: &[CompactTaskContextItem],
    space: OmissionAggregate,
    diagnostic: OmissionAggregate,
) -> Vec<ContextPackOmitted> {
    let mut omitted = base.to_vec();
    for item in dropped_items.iter().take(COMPACT_NAMED_OMISSION_LIMIT) {
        omitted.push(ContextPackOmitted {
            context_id: Some(item.context_id),
            title: Some(elide(&item.title, COMPACT_OMITTED_TITLE_MAX_CHARS)),
            space_id: Some(item.space_id),
            reason: "item_token_budget".to_owned(),
            estimated_tokens: serialized_tokens(item),
            count: 1,
            ..ContextPackOmitted::default()
        });
    }
    let collapsed = dropped_items
        .iter()
        .skip(COMPACT_NAMED_OMISSION_LIMIT)
        .collect::<Vec<_>>();
    if !collapsed.is_empty() {
        omitted.push(ContextPackOmitted {
            reason: "item_token_budget".to_owned(),
            estimated_tokens: collapsed.iter().copied().map(serialized_tokens).sum(),
            count: collapsed.len(),
            ..ContextPackOmitted::default()
        });
    }
    for (reason, aggregate) in [
        ("space_token_budget", space),
        ("diagnostic_token_budget", diagnostic),
    ] {
        if aggregate.count > 0 {
            omitted.push(ContextPackOmitted {
                reason: reason.to_owned(),
                estimated_tokens: aggregate.estimated_tokens,
                count: aggregate.count,
                ..ContextPackOmitted::default()
            });
        }
    }
    omitted
}

fn charged_task_context_tokens<A: Serialize, T: Serialize>(
    associations: &[A],
    items: &[T],
    graph_diagnostics: &[TaskGraphDiagnostic],
    omitted: &[ContextPackOmitted],
    query_token_explanation: Option<&AutomaticQueryTokenExplanation>,
) -> usize {
    TASK_CONTEXT_ENVELOPE_TOKEN_RESERVE.saturating_add(serialized_tokens(&(
        associations,
        items,
        graph_diagnostics,
        omitted,
        query_token_explanation,
    )))
}

/// Recomputes the charged Association, item/path, omission, and deterministic envelope reserve.
#[must_use]
pub fn estimate_task_context_payload_tokens(pack: &TaskContextPack) -> usize {
    match pack.detail_level {
        ContextPackDetailLevel::Full => charged_task_context_tokens(
            &pack.associations,
            &pack.items,
            &pack.graph_diagnostics,
            &pack.omitted,
            pack.query_token_explanation.as_ref(),
        ),
        ContextPackDetailLevel::Compact => charged_task_context_tokens(
            &pack.compact_associations,
            &pack.compact_items,
            &pack.graph_diagnostics,
            &pack.omitted,
            pack.query_token_explanation.as_ref(),
        ),
    }
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

/// `bm25()` column weights for `context_fts`, in its exact column order:
/// `context_id, revision_id, title, statement, rationale, evidence, problem_view, hint_text`.
///
/// `problem_view` ranks with `statement` because it is the question the Context answers, and
/// `hint_text` ranks with `evidence` because it is unresolved locating text rather than a fact.
/// `title` sits at the same weight as `statement` rather than above it: the title is derived from
/// the first characters of the statement, so a higher weight would score that prefix twice.
const CONTEXT_FTS_BM25_WEIGHTS: &str = "0.0, 0.0, 8.0, 8.0, 4.0, 2.0, 8.0, 2.0";

/// Minimum share of distinct query tokens a revision must match to stay in a ranked page.
/// Below it the row is a single incidental term overlap rather than a plausible answer.
pub const RANKED_MIN_COVERAGE_BASIS_POINTS: u16 = 2_500;

/// Deterministic per-revision query token coverage computed inside the same read transaction.
/// It is a CTE rather than post-processing so ranking, truncation, totals and the stable cursor
/// all observe exactly the same value.
///
/// The denominator is the query's *answerable* tokens: those the Tree indexes at all. A token no
/// stored revision contains cannot be covered by any answer, so counting it would measure the
/// question's spelling rather than the answer's fit. This matters most for Han text, where the
/// bigram tokenizer turns a thirteen-character question into thirteen tokens of which only three
/// name anything the corpus knows; charging the other ten against every candidate held the whole
/// query under [`RANKED_MIN_COVERAGE_BASIS_POINTS`] and returned nothing at all. Dropping them
/// rescales every candidate's coverage by the same factor, so the ranked order is untouched and
/// only the truncation floor moves.
struct QueryTokenCoverage {
    /// One OR group per original query token: the token plus its bounded alias expansion. An
    /// alias hit therefore stays a hit of the query token the caller actually typed.
    groups: Vec<Vec<String>>,
}

impl QueryTokenCoverage {
    fn plan(tokens: &[String], alias: &AliasExpansion, matching: bool) -> Self {
        Self {
            groups: if matching {
                tokens
                    .iter()
                    .map(|token| alias.token_group(token))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    fn is_active(&self) -> bool {
        !self.groups.is_empty()
    }

    fn with_clause(&self) -> String {
        if !self.is_active() {
            return String::new();
        }
        let unions = self
            .groups
            .iter()
            .enumerate()
            .map(|(index, _)| {
                format!(
                    "SELECT revision_id, {index} AS token_index FROM context_fts                      WHERE context_fts MATCH ?"
                )
            })
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        format!(
            "WITH query_token_hit AS ({unions}),
             query_token_coverage AS (
               SELECT revision_id, COUNT(DISTINCT token_index) AS matched_query_tokens
               FROM query_token_hit
               GROUP BY revision_id
             ),
             answerable_query_token AS (
               SELECT MAX(1, COUNT(DISTINCT token_index)) AS answerable_query_tokens
               FROM query_token_hit
             ) "
        )
    }

    fn parameters(&self) -> Vec<SqlValue> {
        self.groups
            .iter()
            .map(|group| {
                SqlValue::Text(
                    fts_or_match_expression(group)
                        .expect("a coverage group always contains its own query token"),
                )
            })
            .collect()
    }

    fn join_sql(&self) -> &'static str {
        if self.is_active() {
            "LEFT JOIN query_token_coverage
             ON query_token_coverage.revision_id = context_fts.revision_id"
        } else {
            ""
        }
    }

    fn matched_expression(&self) -> String {
        if self.is_active() {
            "COALESCE(query_token_coverage.matched_query_tokens, 0)".to_owned()
        } else {
            "0".to_owned()
        }
    }

    /// Number of query tokens the Tree can answer at all, never zero so it is always a legal
    /// divisor. A query whose every token is unknown matches no row, so the value is unused.
    fn denominator_expression() -> &'static str {
        "(SELECT answerable_query_tokens FROM answerable_query_token)"
    }

    fn basis_points_expression(&self) -> String {
        if !self.is_active() {
            return "0".to_owned();
        }
        format!(
            "({} * {BASIS_POINTS_SCALE} / {})",
            self.matched_expression(),
            Self::denominator_expression()
        )
    }

    fn minimum_clause(&self) -> Option<String> {
        self.is_active().then(|| {
            format!(
                "{} * {BASIS_POINTS_SCALE} >= {} * {RANKED_MIN_COVERAGE_BASIS_POINTS}",
                self.matched_expression(),
                Self::denominator_expression()
            )
        })
    }
}

#[allow(clippy::too_many_lines)]
fn search_in_snapshot(
    connection: &Connection,
    request: &SearchRequest,
    tree_oid: &str,
    eligible_only: bool,
    context_ttl: &ContextTtlSettings,
) -> Result<SearchPage> {
    let query_tokens = search_tokens(&request.query);
    let coverage_tokens = query_tokens
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let ranked_mode = request.match_mode == SearchMatchMode::Ranked;
    // `exact` is the literal-spelling mode, so it never expands through `token_alias`.
    let alias = if ranked_mode {
        AliasExpansion::load(connection, &coverage_tokens)?
    } else {
        AliasExpansion::default()
    };
    let match_expression = if ranked_mode {
        fts_or_match_expression(&alias.expanded_tokens(&coverage_tokens))
    } else {
        fts_match_expression(&coverage_tokens)
    };
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

    let (mut where_sql, where_parameters) =
        search_where(request, match_expression.as_deref(), eligible_only);
    let coverage = QueryTokenCoverage::plan(&coverage_tokens, &alias, match_expression.is_some());
    let from_sql = if match_expression.is_some() {
        format!(
            "context_fts
         JOIN context_revision AS revision USING(revision_id)
         JOIN context_item AS item USING(context_id)
         JOIN space_projection AS space USING(space_id)
         {}",
            coverage.join_sql()
        )
    } else {
        "context_revision AS revision
         JOIN context_item AS item USING(context_id)
         JOIN space_projection AS space USING(space_id)"
            .to_owned()
    };
    let mut base_parameters = coverage.parameters();
    base_parameters.extend(where_parameters);
    let unranked_total = if ranked_mode && coverage.is_active() {
        let total_sql = format!(
            "{}SELECT COUNT(*) FROM {from_sql} WHERE {where_sql}",
            coverage.with_clause()
        );
        Some(
            connection
                .query_row(
                    &total_sql,
                    params_from_iter(base_parameters.iter()),
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sql_error("count ranked search matches before truncation"))?,
        )
    } else {
        None
    };
    if ranked_mode {
        if let Some(clause) = coverage.minimum_clause() {
            where_sql = format!("{where_sql} AND {clause}");
        }
    }
    let total_sql = format!(
        "{}SELECT COUNT(*) FROM {from_sql} WHERE {where_sql}",
        coverage.with_clause()
    );
    let total = connection
        .query_row(
            &total_sql,
            params_from_iter(base_parameters.iter()),
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error("count search matches"))?;

    let status = status_expression();
    let evidence = evidence_expression();
    let coverage_sql = coverage.basis_points_expression();
    let relevance = if match_expression.is_some() {
        if ranked_mode {
            // BM25 is negative and ascending, so scaling it by the matched share of the query
            // makes broader coverage strictly better while keeping BM25 the tie-breaker.
            format!(
                "bm25(context_fts, {CONTEXT_FTS_BM25_WEIGHTS}) * ({} / 10000.0)",
                coverage.basis_points_expression()
            )
        } else {
            format!("bm25(context_fts, {CONTEXT_FTS_BM25_WEIGHTS})")
        }
    } else {
        "0.0".to_owned()
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
        "{with_clause}{ranked_keyword} ranked AS (
           SELECT revision.space_id, revision.context_id, revision.revision_id,
                  COALESCE(space.title, ''), revision.kind, {status} AS result_status,
                  revision.statement, revision.rationale, revision.applicability_json,
                  revision.assumptions_json, revision.recheck_when_json,
                  item.auto_injection_eligible, {relevance} AS relevance,
                  {evidence} AS evidence_completeness, {coverage_sql} AS coverage_basis_points,
                  COALESCE(revision.problem_view, '') AS problem_view, revision.hint_text,
                  item.superseded_by, item.stale_reason, item.accepted_at_unix_seconds
           FROM {from_sql}
           WHERE {where_sql}
         )
         SELECT * FROM ranked {cursor_sql}
         ORDER BY {order_sql}
         LIMIT ?",
        with_clause = coverage.with_clause(),
        ranked_keyword = if coverage.is_active() { "," } else { "WITH" },
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
        let space_title: String = row.get(3).map_err(sql_error("read search Space title"))?;
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
        let row_coverage = row
            .get::<_, i64>(14)
            .map_err(sql_error("read query token coverage"))?;
        let problem_view: String = row.get(15).map_err(sql_error("read problem view"))?;
        let hint_text: String = row.get(16).map_err(sql_error("read hint text"))?;
        let superseded_by = row
            .get::<_, Option<String>>(17)
            .map_err(sql_error("read superseding Context"))?
            .map(|value| parse_id(&value))
            .transpose()?;
        let stale_reason = row
            .get::<_, Option<String>>(18)
            .map_err(sql_error("read stale reason"))?;
        let accepted_at = row
            .get::<_, Option<i64>>(19)
            .map_err(sql_error("read publication time"))?;
        let kind = parse_kind(&kind_text)?;
        let derived_state = ContextDerivedState {
            superseded_by,
            stale_reason,
            historical_reason: context_ttl.historical_reason(kind, accepted_at),
            demotion_basis_points: None,
        };
        let auto_injection_eligible =
            auto_injection_eligible && !derived_state.blocks_automatic_injection();
        let evidence = load_evidence(connection, revision_id)?;
        let conflicts = load_conflicts(connection, context_id, revision_id)?;
        let match_reason = explain_match(&ExplainMatchInput {
            query_tokens: &coverage_tokens,
            alias: &alias,
            title: &space_title,
            statement: &statement,
            rationale: &rationale,
            problem_view: &problem_view,
            hint_text: &hint_text,
            evidence: &evidence,
            bm25: row_relevance,
            evidence_completeness: row_evidence,
            coverage_basis_points: u16::try_from(row_coverage).unwrap_or(u16::MAX),
        });
        ranked.push(RankedRow {
            result: SearchResult {
                space_id,
                context_id,
                revision_id,
                title: context_display_title(&statement),
                space_title,
                kind,
                status: parse_status(&status_text)?,
                statement,
                rationale,
                applicability,
                assumptions,
                recheck_when,
                evidence,
                conflicts,
                derived_state,
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
    let mut omitted = (remaining > 0)
        .then(|| SearchOmitted {
            count: remaining,
            reason: "page_limit".to_owned(),
        })
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(unranked_total) = unranked_total {
        let truncated = usize::try_from(unranked_total.saturating_sub(total)).unwrap_or(0);
        if truncated > 0 {
            omitted.push(SearchOmitted {
                count: truncated,
                reason: "low_query_token_coverage".to_owned(),
            });
        }
    }
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

/// Maximum character length of the Context-owned display title derived from a statement.
const CONTEXT_TITLE_MAX_CHARS: usize = 60;

/// Derives the Context-owned display title from an immutable revision statement. Context
/// revisions have no stored title field, so the leading characters of the statement are the
/// only Context-owned identity available; truncation is by `char` and marks elision.
fn context_display_title(statement: &str) -> String {
    let trimmed = statement.trim();
    if trimmed.chars().count() <= CONTEXT_TITLE_MAX_CHARS {
        return trimmed.to_owned();
    }
    let mut title = trimmed
        .chars()
        .take(CONTEXT_TITLE_MAX_CHARS)
        .collect::<String>();
    title.push('…');
    title
}

/// Every input one ranked row needs to explain itself, in `context_fts` column order.
struct ExplainMatchInput<'a> {
    query_tokens: &'a [String],
    alias: &'a AliasExpansion,
    title: &'a str,
    statement: &'a str,
    rationale: &'a str,
    problem_view: &'a str,
    hint_text: &'a str,
    evidence: &'a [EvidenceView],
    bm25: f64,
    evidence_completeness: i64,
    coverage_basis_points: u16,
}

fn explain_match(input: &ExplainMatchInput<'_>) -> MatchReason {
    let evidence_text = input
        .evidence
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
        (MatchField::Title, input.title),
        (MatchField::Statement, input.statement),
        (MatchField::Rationale, input.rationale),
        (MatchField::Evidence, evidence_text.as_str()),
        (MatchField::ProblemView, input.problem_view),
        (MatchField::HintText, input.hint_text),
    ];
    let (matched_fields, matched_tokens, matched_via_alias) =
        explain_token_fields(input.query_tokens, input.alias, fields);
    MatchReason {
        matched_fields,
        matched_tokens,
        bm25: input.bm25,
        coverage_basis_points: input.coverage_basis_points,
        evidence_completeness: u16::try_from(input.evidence_completeness).unwrap_or(u16::MAX),
        structured_filter_match: true,
        matched_via_alias,
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

/// Upper bound on how many aliases one query token may pull in. Expansion widens recall, so it
/// stays bounded and deterministic rather than following the whole alias group transitively.
pub const MAX_ALIASES_PER_QUERY_TOKEN: usize = 8;

/// Upper bound on the aliases one whole query may add. Automatic retrieval submits every eligible
/// Intent token at once, and each extra `OR` term widens the FTS scan on a latency-bounded path.
pub const MAX_ALIAS_EXPANSIONS_PER_QUERY: usize = 16;

/// `token_alias.source` values, ranked: an identifier split is a spelling of the same artifact,
/// a domain term is only a vocabulary neighbour, so the identifier split is kept first.
const ALIAS_SOURCE_RANK: [&str; 2] = [IDENTIFIER_SPLIT_ALIAS_SOURCE, "domain_term"];

/// `token_alias.source` of a group produced by splitting a code identifier into its words.
const IDENTIFIER_SPLIT_ALIAS_SOURCE: &str = "identifier_split";

/// Least number of an alias group's members a query must already name before that group may
/// expand one of them. A single shared word such as `page` names no identifier in particular, so
/// expanding it would pull in every identifier that happens to contain it.
const MIN_ALIAS_GROUP_MEMBERS_IN_QUERY: usize = 2;

/// Bounded, deterministic `token_alias` expansion of one query's tokens.
///
/// Expansion never changes the coverage denominator: an alias hit is reported as a hit of the
/// original query token it was expanded from, so a query is not made to look better covered
/// merely because the corpus spells one of its tokens several ways.
#[derive(Clone, Debug, Default)]
struct AliasExpansion {
    aliases: BTreeMap<String, Vec<(String, String)>>,
    /// Query tokens that spell part of a code identifier the corpus names: every token of an
    /// `identifier_split` group the query already names
    /// [`MIN_ALIAS_GROUP_MEMBERS_IN_QUERY`] members of. Such a token is language-neutral
    /// evidence — a Context spells `ProductAnchorAssem` the same way whatever language its prose
    /// is written in — so [`identifier_weighted_coverage_basis_points`] weights it above prose.
    identifier_tokens: BTreeSet<String>,
}

impl AliasExpansion {
    /// Reads at most [`MAX_ALIASES_PER_QUERY_TOKEN`] aliases per token inside the caller's
    /// snapshot transaction. An absent or empty `token_alias` table yields no expansion.
    ///
    /// One statement covers the whole query: this runs on the automatic retrieval hot path, so a
    /// per-token round trip would be paid once per channel on every Intent update.
    fn load(connection: &Connection, tokens: &[String]) -> Result<Self> {
        if tokens.is_empty() {
            return Ok(Self::default());
        }
        let original = tokens.iter().cloned().collect::<BTreeSet<_>>();
        let placeholders = std::iter::repeat_n("?", original.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = connection
            .prepare(&format!(
                "SELECT token, alias, source, group_key FROM token_alias
                 WHERE token IN ({placeholders})
                 ORDER BY token ASC, group_key ASC, source ASC, alias ASC"
            ))
            .map_err(sql_error("prepare query token alias expansion"))?;
        let rows = statement
            .query_map(params_from_iter(original.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(sql_error("read query token alias expansion"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error("collect query token alias expansion"))?;
        let mut grouped = BTreeMap::<(String, String), Vec<(String, String)>>::new();
        for (token, alias, source, group_key) in rows {
            if alias == token {
                continue;
            }
            grouped
                .entry((token, group_key))
                .or_default()
                .push((alias, source));
        }
        let mut ranked = Vec::new();
        let mut identifier_tokens = BTreeSet::new();
        for ((token, group_key), members) in grouped {
            let named = 1 + members
                .iter()
                .filter(|(alias, _)| original.contains(alias))
                .count();
            if named < MIN_ALIAS_GROUP_MEMBERS_IN_QUERY {
                continue;
            }
            if members
                .iter()
                .any(|(_, source)| source == IDENTIFIER_SPLIT_ALIAS_SOURCE)
            {
                identifier_tokens.insert(token.clone());
            }
            for (alias, source) in members {
                if original.contains(&alias) {
                    continue;
                }
                ranked.push((
                    std::cmp::Reverse(named),
                    alias_source_rank(&source),
                    token.clone(),
                    alias,
                    group_key.clone(),
                ));
            }
        }
        // The more of an alias group the query already names, the more likely the group is the
        // identifier the author meant, so those expansions are kept first when the budget binds.
        ranked.sort();
        let mut aliases = BTreeMap::<String, Vec<(String, String)>>::new();
        let mut total = 0_usize;
        let mut seen = BTreeSet::new();
        for (_named, _source, token, alias, group_key) in ranked {
            if total == MAX_ALIAS_EXPANSIONS_PER_QUERY
                || !seen.insert((token.clone(), alias.clone()))
            {
                continue;
            }
            let selected = aliases.entry(token).or_default();
            if selected.len() == MAX_ALIASES_PER_QUERY_TOKEN {
                continue;
            }
            selected.push((alias, group_key));
            total += 1;
        }
        aliases.retain(|_token, selected| !selected.is_empty());
        Ok(Self {
            aliases,
            identifier_tokens,
        })
    }

    fn is_empty(&self) -> bool {
        self.aliases.is_empty()
    }

    /// The query tokens this Tree knows as parts of a code identifier.
    fn identifier_tokens(&self) -> &BTreeSet<String> {
        &self.identifier_tokens
    }

    /// One OR group per original query token: the token itself plus its aliases. Coverage counts
    /// the group, so an alias hit is exactly one covered original token.
    fn token_group(&self, token: &str) -> Vec<String> {
        let mut group = vec![token.to_owned()];
        if let Some(aliases) = self.aliases.get(token) {
            group.extend(aliases.iter().map(|(alias, _)| alias.clone()));
        }
        group
    }

    /// Every spelling the FTS `MATCH` expression must accept.
    fn expanded_tokens(&self, tokens: &[String]) -> Vec<String> {
        tokens
            .iter()
            .flat_map(|token| self.token_group(token))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Resolves the tokens present in one indexed text back onto the query tokens they answer.
    ///
    /// Returns the covered original tokens and the alias hits that explain the expanded ones.
    fn resolve(
        &self,
        wanted: &BTreeSet<String>,
        available: &BTreeSet<String>,
    ) -> (BTreeSet<String>, BTreeSet<AliasMatch>) {
        let mut matched = wanted
            .intersection(available)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut alias_matches = BTreeSet::new();
        if self.aliases.is_empty() {
            return (matched, alias_matches);
        }
        for (token, aliases) in &self.aliases {
            if !wanted.contains(token) {
                continue;
            }
            for (alias, group_key) in aliases {
                if available.contains(alias) {
                    matched.insert(token.clone());
                    alias_matches.insert(AliasMatch {
                        token: token.clone(),
                        alias: alias.clone(),
                        group_key: group_key.clone(),
                    });
                }
            }
        }
        (matched, alias_matches)
    }
}

fn alias_source_rank(source: &str) -> usize {
    ALIAS_SOURCE_RANK
        .iter()
        .position(|known| *known == source)
        .unwrap_or(ALIAS_SOURCE_RANK.len())
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
    /// The compact channel names are hand-written; this pins them to what `serde` actually emits.
    #[test]
    fn compact_retrieval_channel_names_match_the_serialized_source_tag() {
        for path in [
            super::TaskRetrievalPath::ExactScope {
                dimension: "domain".to_owned(),
                value: "search".to_owned(),
            },
            super::TaskRetrievalPath::ContextRelation { hops: Vec::new() },
            super::TaskRetrievalPath::ContextFts {
                matched_fields: Vec::new(),
                matched_tokens: Vec::new(),
            },
            super::TaskRetrievalPath::IntentFts {
                matched_fields: Vec::new(),
                matched_tokens: Vec::new(),
            },
        ] {
            let encoded = serde_json::to_value(&path).unwrap();
            assert_eq!(
                encoded["source"].as_str(),
                Some(super::retrieval_channel_name(&path))
            );
        }
    }

    use std::time::{Duration, Instant};

    use rusqlite::params;

    use sctx_index::normalize_search_text;

    use super::{
        ContextStatus, ContextTtlSettings, ContextUsageCounts, ScopeFilter, SearchFilters,
        SearchRequest, USAGE_IGNORED_PENALTY_BASIS_POINTS, USAGE_REUSED_BONUS_BASIS_POINTS,
        estimate_tokens, hex_decode, hex_encode, search_in_snapshot, usage_multiplier_basis_points,
        usage_prior_reason, usage_prior_score,
    };

    #[test]
    fn usage_prior_promotes_reuse_and_only_penalizes_repeated_ignores() {
        let reused = ContextUsageCounts {
            reused: 2,
            ignored: 9,
            refuted: 0,
        };
        assert_eq!(
            usage_multiplier_basis_points(reused),
            USAGE_REUSED_BONUS_BASIS_POINTS
        );
        let ignored_once = ContextUsageCounts {
            reused: 0,
            ignored: 2,
            refuted: 0,
        };
        assert_eq!(usage_multiplier_basis_points(ignored_once), 10_000);
        let ignored_often = ContextUsageCounts {
            reused: 0,
            ignored: 3,
            refuted: 0,
        };
        assert_eq!(
            usage_multiplier_basis_points(ignored_often),
            USAGE_IGNORED_PENALTY_BASIS_POINTS
        );
        // A refuted Context is demoted by its open semantic conflict, never twice here.
        let refuted = ContextUsageCounts {
            reused: 0,
            ignored: 0,
            refuted: 4,
        };
        assert_eq!(usage_multiplier_basis_points(refuted), 10_000);
        assert_eq!(usage_prior_score(4_000, 10_000), 4_000);
        assert_eq!(
            usage_prior_score(4_000, USAGE_REUSED_BONUS_BASIS_POINTS),
            4_600
        );
        assert_eq!(
            usage_prior_score(4_000, USAGE_IGNORED_PENALTY_BASIS_POINTS),
            3_600
        );
        assert_eq!(
            usage_prior_reason(reused).as_deref(),
            Some("Reused in 2 prior task(s).")
        );
        assert_eq!(usage_prior_reason(ignored_once), None);
        assert_eq!(
            usage_prior_reason(ignored_often).as_deref(),
            Some("Ignored in 3 prior task(s).")
        );
        assert_eq!(usage_prior_reason(ContextUsageCounts::default()), None);
    }

    fn tokens(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn basis(selected: &[&str], answerable: &[&str]) -> super::AutomaticCoverageBasis {
        super::AutomaticCoverageBasis {
            selected: tokens(selected),
            answerable: tokens(answerable),
        }
    }

    #[test]
    fn coverage_divides_by_the_tokens_the_corpus_can_answer() {
        let basis = basis(
            &["bottom", "comment", "default", "input", "missing", "why"],
            &["bottom", "comment", "input"],
        );
        let matched = ["bottom", "comment", "input"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect::<std::collections::BTreeSet<_>>();
        // Three of three answerable tokens is full coverage; against all six selected tokens the
        // same match read as 5000 and the automatic gate rejected it.
        assert_eq!(
            super::token_coverage_basis_points(&matched, basis.tokens()),
            10_000
        );
        assert_eq!(
            super::token_coverage_basis_points(&matched, &basis.selected),
            5_000
        );
    }

    #[test]
    fn coverage_is_zero_when_the_corpus_answers_none_of_the_query() {
        let basis = basis(&["kubernetes", "ingress", "timeout"], &[]);
        let matched = std::collections::BTreeSet::new();
        assert_eq!(
            super::token_coverage_basis_points(&matched, basis.tokens()),
            0
        );
        assert!(!basis.answerable_ratio_sufficient());
    }

    #[test]
    fn the_answerable_guard_separates_a_thin_query_from_a_narrow_one() {
        // A question the corpus recognizes a quarter of is asked in good faith.
        assert!(
            basis(&["a", "b", "c", "d", "e", "f", "g", "h"], &["a", "b"])
                .answerable_ratio_sufficient()
        );
        // One word in nine is a query this Tree does not speak, whatever that word matches.
        assert!(
            !basis(&["a", "b", "c", "d", "e", "f", "g", "h", "i"], &["a"])
                .answerable_ratio_sufficient()
        );
        // Two answerable tokens are the floor, and a query that never had two is exempt from it.
        assert!(!basis(&["a", "b"], &["a"]).answerable_ratio_sufficient());
        assert!(basis(&["a"], &["a"]).answerable_ratio_sufficient());
        assert!(!basis(&["a"], &[]).answerable_ratio_sufficient());
    }

    #[test]
    fn the_compact_projection_keeps_the_totals_and_drops_the_lists() {
        let mut explanation = super::AutomaticQueryTokenExplanation {
            document_count: 40,
            high_document_frequency_min_documents: 20,
            high_document_frequency_threshold_basis_points: 5_000,
            stop_word_fallback_active: false,
            selected_tokens: tokens(&["alpha", "bravo", "charlie"]),
            answerable_tokens: tokens(&["alpha"]),
            dropped_tokens: (0..12)
                .map(|index| super::AutomaticQueryTokenDrop {
                    token: format!("drop{index:02}"),
                    filter: super::AutomaticQueryTokenFilter::HighDocumentFrequency,
                    document_frequency: Some(index),
                })
                .collect(),
            selected_token_count: 0,
            answerable_token_count: 0,
            dropped_token_count: 0,
        };
        explanation.refresh_counts();
        let compact = explanation.compact_projection();
        assert!(compact.selected_tokens.is_empty());
        assert!(compact.answerable_tokens.is_empty());
        assert_eq!(compact.selected_token_count, 3);
        assert_eq!(compact.answerable_token_count, 1);
        assert_eq!(compact.dropped_token_count, 12);
        assert_eq!(
            compact.dropped_tokens.len(),
            super::COMPACT_QUERY_TOKEN_DROP_LIMIT
        );
        assert_eq!(compact.dropped_tokens[0].token, "drop00");
    }

    #[test]
    fn gate_omissions_name_the_closest_spaces_and_collapse_the_rest() {
        let omitted = (0..12)
            .map(|index| super::ContextPackOmitted {
                space_id: Some(sctx_domain::SpaceId::new()),
                reason: "automatic_text_ineligible".to_owned(),
                count: 1,
                coverage_basis_points: Some(index * 100),
                ..super::ContextPackOmitted::default()
            })
            .collect::<Vec<_>>();
        let collapsed = super::collapse_gate_omissions(omitted);
        assert_eq!(collapsed.len(), super::AUTOMATIC_GATE_OMISSION_LIMIT + 1);
        // Highest coverage first: the Spaces that came closest to passing are the named ones.
        assert_eq!(collapsed[0].coverage_basis_points, Some(1_100));
        assert!(collapsed[0].space_id.is_some());
        let last = collapsed.last().expect("the collapsed notice exists");
        assert_eq!(last.count, 4);
        assert!(last.space_id.is_none());
        // A list short enough to name is left exactly as it is.
        let short = vec![super::ContextPackOmitted {
            reason: "low_answerable_ratio".to_owned(),
            count: 1,
            ..super::ContextPackOmitted::default()
        }];
        assert_eq!(super::collapse_gate_omissions(short.clone()), short);
    }

    #[test]
    fn a_rejected_gate_names_which_rule_rejected_it() {
        let ratio = super::AutomaticTextGate {
            eligible: false,
            coverage_basis_points: 10_000,
            text_channel_count: 1,
            blocked_by_answerable_ratio: true,
        };
        assert_eq!(ratio.omission_reason(), "low_answerable_ratio");
        assert_eq!(
            super::AutomaticTextGate {
                blocked_by_answerable_ratio: false,
                ..ratio
            }
            .omission_reason(),
            "automatic_text_ineligible"
        );
    }

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
                   governance_status TEXT NOT NULL, auto_injection_eligible INTEGER NOT NULL,
                   accepted_at_unix_seconds INTEGER, superseded_by TEXT, stale_reason TEXT
                 ) WITHOUT ROWID;
                 CREATE TABLE context_revision(
                   revision_id TEXT PRIMARY KEY, context_id TEXT NOT NULL, space_id TEXT NOT NULL,
                   kind TEXT NOT NULL, statement TEXT NOT NULL, rationale TEXT NOT NULL,
                   applicability_json TEXT NOT NULL, assumptions_json TEXT NOT NULL,
                   recheck_when_json TEXT NOT NULL, lifecycle TEXT NOT NULL,
                   evidence_completeness INTEGER NOT NULL,
                   problem_view TEXT, hint_text TEXT NOT NULL DEFAULT ''
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
                   title, statement, rationale, evidence, problem_view, hint_text
                 );
                 CREATE TABLE token_alias(
                   token TEXT NOT NULL, alias TEXT NOT NULL, source TEXT NOT NULL,
                   group_key TEXT NOT NULL, PRIMARY KEY(token, alias, source, group_key)
                 ) WITHOUT ROWID;",
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
                       ?, ?, ?, 'decision', ?, ?, ?, '[]', '[]', 'accepted', 1000, NULL, ''
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
                .prepare("INSERT INTO context_fts VALUES (?, ?, ?, ?, ?, ?, '', '')")
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
            search_in_snapshot(
                &connection,
                &request,
                "benchmark-tree",
                false,
                &ContextTtlSettings::default(),
            )
            .unwrap();
            let mut samples = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let started = Instant::now();
                search_in_snapshot(
                    &connection,
                    &request,
                    "benchmark-tree",
                    false,
                    &ContextTtlSettings::default(),
                )
                .unwrap();
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
