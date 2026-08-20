//! Structured FTS5 search, Task-to-Space association, and deterministic Context Packs.

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use sctx_domain::{
    Applicability, ContextId, ContextKind, EvidenceId, RevisionId, SpaceId, TaskId, TaskIntent,
    TaskSignal, TaskSignalKind, TaskSpaceAssociation,
};
use sctx_index::{IndexMetadata, ProjectionIndex, normalize_search_text, search_tokens};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub use sctx_domain::{Error, ErrorKind, Result};

const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 200;
const DEFAULT_CANDIDATE_LIMIT: usize = 100;

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

/// Explainable match against one current Intent head.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceIntentHeadMatch {
    pub revision_id: RevisionId,
    pub matched_fields: Vec<SpaceIntentField>,
    pub matched_tokens: Vec<String>,
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
    pub matched_fields: Vec<SpaceIntentField>,
    pub matched_tokens: Vec<String>,
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

/// Context Pack construction request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextPackRequest {
    pub search: SearchRequest,
    pub token_budget: usize,
    pub candidate_limit: usize,
    pub mode: ContextPackMode,
}

impl ContextPackRequest {
    #[must_use]
    pub fn automatic(search: SearchRequest, token_budget: usize) -> Self {
        Self {
            search,
            token_budget,
            candidate_limit: DEFAULT_CANDIDATE_LIMIT,
            mode: ContextPackMode::AutomaticInjection,
        }
    }
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
    pub match_reason: MatchReason,
    pub detail: ContextPackDetail,
}

/// Context Pack response. Items retain match reasons and both sides of every expanded conflict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextPack {
    pub indexed_tree_oid: String,
    pub projection_generation: u64,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub mode: ContextPackMode,
    pub items: Vec<ContextPackItem>,
    pub omitted: Vec<ContextPackOmitted>,
}

/// Query boundary that always delegates reads to one [`sctx_index::QuerySnapshot`] transaction.
#[derive(Clone, Debug)]
pub struct SearchEngine {
    index: ProjectionIndex,
}

impl SearchEngine {
    #[must_use]
    pub const fn new(index: ProjectionIndex) -> Self {
        Self { index }
    }

    /// Finds zero or more current Space Intent candidates from a Task Intent and its observed
    /// signals. Every current head is searched independently; conflicts are returned rather than
    /// resolved by ranking.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid Task Intent or signal collection, and storage errors
    /// propagated by index synchronization and snapshot reads.
    pub fn space_intent_candidates(
        &self,
        intent: &TaskIntent,
        signals: &[TaskSignal],
    ) -> Result<SpaceIntentCandidatesResponse> {
        intent.validate()?;
        TaskSignal::validate_collection(signals)?;
        let query_tokens = task_query_tokens(intent, signals);
        let snapshot = self.index.query_snapshot(|connection| {
            query_space_intent_candidates(connection, &query_tokens)
        })?;
        Ok(SpaceIntentCandidatesResponse {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            task_id: intent.task_id,
            candidates: snapshot.data,
        })
    }

    /// Infers zero or more explainable Space associations for a Task. The M2 inference boundary
    /// fuses current Space Intent text, automatic-injection-safe Context text and scope, and exact
    /// textual artifact hints. It deliberately leaves `relation_paths` empty because code graph
    /// resolution belongs to the later Engineering Graph stage.
    ///
    /// Workspace signals are location observations only and are excluded from both matching and
    /// scoring.
    ///
    /// # Errors
    ///
    /// Returns an input error for an invalid Task Intent or signal collection, and storage errors
    /// propagated by index synchronization and snapshot reads.
    pub fn task_space_associations(
        &self,
        intent: &TaskIntent,
        signals: &[TaskSignal],
    ) -> Result<TaskSpaceAssociationsResponse> {
        intent.validate()?;
        TaskSignal::validate_collection(signals)?;
        let query_tokens = association_query_tokens(intent, signals);
        let artifact_hints = artifact_hints(signals);
        let scope_targets = ScopeTargets::from_intent(intent);
        let snapshot = self.index.query_snapshot(|connection| {
            infer_task_space_associations(
                connection,
                intent.task_id,
                &query_tokens,
                &artifact_hints,
                &scope_targets,
            )
        })?;
        Ok(TaskSpaceAssociationsResponse {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            task_id: intent.task_id,
            associations: snapshot.data,
        })
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

    /// Constructs a deterministic Context Pack without leaving the `QuerySnapshot` transaction.
    /// Automatic mode hard-filters to Accepted, evidence-backed, conflict-free rows.
    ///
    /// # Errors
    ///
    /// Returns an input error for a zero budget/limit or invalid search request, and storage errors
    /// propagated by index synchronization and snapshot reads.
    pub fn context_pack(&self, request: &ContextPackRequest) -> Result<ContextPack> {
        validate_search_request(&request.search)?;
        if request.token_budget == 0 {
            return Err(invalid(
                "context pack token_budget must be greater than zero",
            ));
        }
        if request.candidate_limit == 0 || request.candidate_limit > MAX_PAGE_SIZE {
            return Err(invalid(format!(
                "context pack candidate_limit must be between 1 and {MAX_PAGE_SIZE}"
            )));
        }
        let mut search = request.search.clone();
        search.page_size = request.candidate_limit;
        search.cursor = None;
        if request.mode == ContextPackMode::AutomaticInjection {
            search.filters.statuses = vec![ContextStatus::Accepted];
        }
        let automatic = request.mode == ContextPackMode::AutomaticInjection;
        let snapshot = self.index.query_snapshot(|connection| {
            let tree_oid = meta(connection, "indexed_tree_oid")?;
            let page = search_in_snapshot(connection, &search, &tree_oid, automatic)?;
            Ok(pack_page(page, request.token_budget))
        })?;
        Ok(ContextPack {
            indexed_tree_oid: snapshot.metadata.indexed_tree_oid,
            projection_generation: snapshot.metadata.projection_generation,
            token_budget: request.token_budget,
            estimated_tokens: snapshot.data.estimated_tokens,
            mode: request.mode,
            items: snapshot.data.items,
            omitted: snapshot.data.omitted,
        })
    }
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

fn task_query_tokens(intent: &TaskIntent, signals: &[TaskSignal]) -> Vec<String> {
    let list_text = [
        &intent.in_scope,
        &intent.out_of_scope,
        &intent.domains,
        &intent.platforms,
        &intent.constraints,
        &intent.acceptance_conditions,
        &intent.artifacts,
        &intent.interfaces,
        &intent.unknowns,
    ]
    .into_iter()
    .flat_map(|values| values.iter().map(String::as_str));
    let signal_text = signals
        .iter()
        .filter(|signal| signal.kind != TaskSignalKind::Workspace)
        .map(|signal| signal.content.as_str());
    [intent.goal.as_str(), intent.desired_change.as_str()]
        .into_iter()
        .chain(list_text)
        .chain(signal_text)
        .flat_map(search_tokens)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn association_query_tokens(intent: &TaskIntent, signals: &[TaskSignal]) -> Vec<String> {
    let non_artifact_signals = signals
        .iter()
        .filter(|signal| {
            matches!(
                signal.kind,
                TaskSignalKind::Prompt | TaskSignalKind::Repository | TaskSignalKind::Diff
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    task_query_tokens(intent, &non_artifact_signals)
}

fn query_space_intent_candidates(
    connection: &Connection,
    query_tokens: &[String],
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
        let mut bm25 = f64::INFINITY;
        for head in &matching_heads {
            matched_fields.extend(head.matched_fields.iter().copied());
            matched_tokens.extend(head.matched_tokens.iter().cloned());
            bm25 = bm25.min(head.bm25);
        }
        candidates.push(SpaceIntentCandidate {
            space_id,
            intent_conflicted,
            head_revision_ids,
            matching_heads,
            matched_fields: matched_fields.into_iter().collect(),
            matched_tokens: matched_tokens.into_iter().collect(),
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
    let (matched_fields, matched_tokens) = explain_intent_match(query_tokens, fields);
    Ok(RawIntentHeadMatch {
        space_id: parse_id(&row.space_id)?,
        intent_conflicted: row.intent_conflicted,
        head_match: SpaceIntentHeadMatch {
            revision_id: parse_id(&row.revision_id)?,
            matched_fields,
            matched_tokens,
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
) -> (Vec<SpaceIntentField>, Vec<String>) {
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

#[derive(Clone, Debug)]
struct ArtifactHint {
    label: String,
    tokens: Vec<String>,
}

#[derive(Debug, Default)]
struct ScopeTargets {
    domains: BTreeSet<String>,
    platforms: BTreeSet<String>,
    conditions: BTreeSet<String>,
}

impl ScopeTargets {
    fn from_intent(intent: &TaskIntent) -> Self {
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

#[derive(Debug, Default)]
struct AcceptedContextEvidence {
    textual_match: bool,
    matched_artifacts: BTreeSet<String>,
    matched_scopes: BTreeSet<String>,
}

const SAFE_ACCEPTED_CONTEXT_PREDICATE: &str = "item.governance_status = 'accepted'
     AND item.accepted_revision_id = revision.revision_id
     AND item.auto_injection_eligible = 1
     AND revision.lifecycle = 'accepted'
     AND revision.evidence_completeness >= 750
     AND EXISTS (
         SELECT 1 FROM evidence AS required_evidence
         WHERE required_evidence.revision_id = revision.revision_id
           AND trim(required_evidence.supports) <> ''
           AND trim(required_evidence.interpretation) <> ''
           AND required_evidence.content_json <> '{}'
     )";

#[derive(Debug, Default)]
struct AssociationEvidence {
    intent_fields: BTreeSet<String>,
    intent_matched: bool,
    intent_conflicted: bool,
    matched_artifacts: BTreeSet<String>,
    matched_contexts: BTreeSet<ContextId>,
    textual_contexts: BTreeSet<ContextId>,
    matched_scopes: BTreeSet<String>,
}

fn artifact_hints(signals: &[TaskSignal]) -> Vec<ArtifactHint> {
    signals
        .iter()
        .filter_map(|signal| {
            let kind = artifact_kind(signal.kind)?;
            let tokens = search_tokens(&signal.content);
            (!tokens.is_empty()).then(|| ArtifactHint {
                label: format!("{kind}:{}", signal.content),
                tokens,
            })
        })
        .collect()
}

const fn artifact_kind(kind: TaskSignalKind) -> Option<&'static str> {
    match kind {
        TaskSignalKind::File => Some("file"),
        TaskSignalKind::Symbol => Some("symbol"),
        TaskSignalKind::Api => Some("api"),
        TaskSignalKind::Schema => Some("schema"),
        TaskSignalKind::Test => Some("test"),
        TaskSignalKind::Prompt
        | TaskSignalKind::Workspace
        | TaskSignalKind::Repository
        | TaskSignalKind::Diff => None,
    }
}

fn normalized_values(values: &[String]) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| normalize_search_text(value))
        .collect()
}

fn artifact_match_expression(hints: &[ArtifactHint]) -> Option<String> {
    let alternatives = hints
        .iter()
        .filter(|hint| !hint.tokens.is_empty())
        .map(|hint| {
            let required = hint
                .tokens
                .iter()
                .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            format!("({required})")
        })
        .collect::<Vec<_>>();
    (!alternatives.is_empty()).then(|| alternatives.join(" OR "))
}

fn infer_task_space_associations(
    connection: &Connection,
    task_id: TaskId,
    query_tokens: &[String],
    artifact_hints: &[ArtifactHint],
    scope_targets: &ScopeTargets,
) -> Result<Vec<TaskSpaceAssociation>> {
    let intent_candidates = query_space_intent_candidates(connection, query_tokens)?;
    let intent_artifacts = query_exact_intent_artifacts(connection, artifact_hints)?;
    let contexts =
        query_accepted_context_evidence(connection, query_tokens, artifact_hints, scope_targets)?;
    let mut evidence = BTreeMap::<SpaceId, AssociationEvidence>::new();
    apply_intent_evidence(&mut evidence, intent_candidates);
    for (space_id, (intent_conflicted, artifacts)) in intent_artifacts {
        let aggregate = evidence.entry(space_id).or_default();
        aggregate.intent_conflicted |= intent_conflicted;
        aggregate.matched_artifacts.extend(artifacts);
    }
    for ((space_id, context_id), context) in contexts {
        let aggregate = evidence.entry(space_id).or_default();
        aggregate.matched_contexts.insert(context_id);
        if context.textual_match {
            aggregate.textual_contexts.insert(context_id);
        }
        aggregate
            .matched_artifacts
            .extend(context.matched_artifacts);
        aggregate.matched_scopes.extend(context.matched_scopes);
    }
    let mut associations = evidence
        .into_iter()
        .filter_map(|(space_id, evidence)| association(task_id, space_id, evidence))
        .collect::<Vec<_>>();
    associations.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.space_id.cmp(&right.space_id))
    });
    TaskSpaceAssociation::validate_collection(task_id, &associations)?;
    Ok(associations)
}

fn apply_intent_evidence(
    evidence: &mut BTreeMap<SpaceId, AssociationEvidence>,
    candidates: Vec<SpaceIntentCandidate>,
) {
    for candidate in candidates {
        let aggregate = evidence.entry(candidate.space_id).or_default();
        aggregate.intent_matched = true;
        aggregate.intent_conflicted = candidate.intent_conflicted;
        aggregate.intent_fields.extend(
            candidate
                .matched_fields
                .into_iter()
                .map(|field| intent_field_name(field).to_owned()),
        );
    }
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

fn query_exact_intent_artifacts(
    connection: &Connection,
    artifact_hints: &[ArtifactHint],
) -> Result<BTreeMap<SpaceId, (bool, BTreeSet<String>)>> {
    let Some(match_expression) = artifact_match_expression(artifact_hints) else {
        return Ok(BTreeMap::new());
    };
    let mut statement = connection
        .prepare(
            "SELECT space_fts.space_id, space.intent_conflicted,
                    space_fts.title, space_fts.problem,
                    space_fts.desired_outcome, space_fts.in_scope, space_fts.out_of_scope,
                    space_fts.acceptance_conditions, space_fts.domain_terms
             FROM space_fts
             JOIN intent_head
              ON intent_head.space_id = space_fts.space_id
              AND intent_head.revision_id = space_fts.revision_id
             JOIN space_projection AS space USING(space_id)
             WHERE space_fts MATCH ?1
             ORDER BY space_fts.space_id, space_fts.revision_id",
        )
        .map_err(sql_error("prepare exact Space Intent artifact matching"))?;
    let rows = statement
        .query_map([match_expression], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                [
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ],
            ))
        })
        .map_err(sql_error("read exact Space Intent artifact candidates"))?;
    let mut matches = BTreeMap::<SpaceId, (bool, BTreeSet<String>)>::new();
    for row in rows {
        let (space_id, intent_conflicted, fields) =
            row.map_err(sql_error("collect Space Intent artifact row"))?;
        let labels = exact_artifact_labels(&fields, artifact_hints);
        if !labels.is_empty() {
            let entry = matches
                .entry(parse_id(&space_id)?)
                .or_insert_with(|| (intent_conflicted, BTreeSet::new()));
            entry.0 |= intent_conflicted;
            entry.1.extend(labels);
        }
    }
    Ok(matches)
}

fn query_accepted_context_evidence(
    connection: &Connection,
    query_tokens: &[String],
    artifact_hints: &[ArtifactHint],
    scope_targets: &ScopeTargets,
) -> Result<BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>> {
    let mut evidence = BTreeMap::new();
    query_accepted_context_text(connection, query_tokens, &mut evidence)?;
    query_accepted_context_artifacts(connection, artifact_hints, &mut evidence)?;
    query_accepted_context_scope(connection, scope_targets, &mut evidence)?;
    Ok(evidence)
}

fn query_accepted_context_artifacts(
    connection: &Connection,
    artifact_hints: &[ArtifactHint],
    evidence: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let Some(match_expression) = artifact_match_expression(artifact_hints) else {
        return Ok(());
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT revision.space_id, revision.context_id, context_fts.title,
                    context_fts.statement, context_fts.rationale, context_fts.evidence
             FROM context_fts
             JOIN context_revision AS revision USING(revision_id)
             JOIN context_item AS item USING(context_id)
             WHERE context_fts MATCH ?1
               AND {SAFE_ACCEPTED_CONTEXT_PREDICATE}
             ORDER BY revision.space_id, revision.context_id"
        ))
        .map_err(sql_error(
            "prepare safe accepted Context artifact association",
        ))?;
    let rows = statement
        .query_map([match_expression], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                [
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ],
            ))
        })
        .map_err(sql_error("read safe accepted Context artifact association"))?;
    for row in rows {
        let (space_id, context_id, fields) =
            row.map_err(sql_error("collect accepted Context artifact association"))?;
        let labels = exact_artifact_labels(&fields, artifact_hints);
        if !labels.is_empty() {
            evidence
                .entry((parse_id(&space_id)?, parse_id(&context_id)?))
                .or_default()
                .matched_artifacts
                .extend(labels);
        }
    }
    Ok(())
}

fn query_accepted_context_text(
    connection: &Connection,
    query_tokens: &[String],
    evidence: &mut BTreeMap<(SpaceId, ContextId), AcceptedContextEvidence>,
) -> Result<()> {
    let Some(match_expression) = fts_or_match_expression(query_tokens) else {
        return Ok(());
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT revision.space_id, revision.context_id
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
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error("read safe accepted Context association text"))?;
    for row in rows {
        let (space_id, context_id) =
            row.map_err(sql_error("collect accepted Context association text"))?;
        let entry = evidence
            .entry((parse_id(&space_id)?, parse_id(&context_id)?))
            .or_default();
        entry.textual_match = true;
    }
    Ok(())
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
                .insert(format!("{dimension}={value}"));
        }
    }
    Ok(())
}

fn exact_artifact_labels<const N: usize>(
    fields: &[String; N],
    artifact_hints: &[ArtifactHint],
) -> BTreeSet<String> {
    artifact_hints
        .iter()
        .filter(|hint| {
            fields
                .iter()
                .any(|field| contains_token_sequence(field, &hint.tokens))
        })
        .map(|hint| hint.label.clone())
        .collect()
}

fn contains_token_sequence(text: &str, wanted: &[String]) -> bool {
    if wanted.is_empty() {
        return false;
    }
    let available = search_tokens(text);
    available
        .windows(wanted.len())
        .any(|window| window == wanted)
}

fn association(
    task_id: TaskId,
    space_id: SpaceId,
    evidence: AssociationEvidence,
) -> Option<TaskSpaceAssociation> {
    if !evidence.intent_matched
        && evidence.matched_artifacts.is_empty()
        && evidence.matched_contexts.is_empty()
        && evidence.matched_scopes.is_empty()
    {
        return None;
    }
    let score = association_score(&evidence);
    let reasons = association_reasons(&evidence);
    Some(TaskSpaceAssociation {
        task_id,
        space_id,
        score,
        matched_intent_fields: evidence.intent_fields.into_iter().collect(),
        matched_artifacts: evidence.matched_artifacts.into_iter().collect(),
        matched_contexts: evidence.matched_contexts.into_iter().collect(),
        relation_paths: Vec::new(),
        reasons,
    })
}

fn association_score(evidence: &AssociationEvidence) -> f64 {
    let intent =
        usize::from(evidence.intent_matched) * (300 + (evidence.intent_fields.len() * 20).min(140));
    let contexts = usize::from(!evidence.matched_contexts.is_empty())
        * (220 + (evidence.matched_contexts.len() * 20).min(100));
    let textual = usize::from(!evidence.textual_contexts.is_empty()) * 80;
    let artifacts = (evidence.matched_artifacts.len() * 50).min(150);
    let scopes = (evidence.matched_scopes.len() * 50).min(150);
    let points = (intent + contexts + textual + artifacts + scopes).min(1000);
    f64::from(u16::try_from(points).expect("association score points fit u16")) / 1000.0
}

fn association_reasons(evidence: &AssociationEvidence) -> Vec<String> {
    let mut reasons = Vec::new();
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
    if evidence.intent_conflicted {
        reasons.push("Space Intent is conflicted; no Intent head was selected".to_owned());
    }
    if !evidence.textual_contexts.is_empty() {
        reasons.push(format!(
            "Task text matched {} accepted, injection-safe Context(s)",
            evidence.textual_contexts.len()
        ));
    }
    if !evidence.matched_artifacts.is_empty() {
        reasons.push(format!(
            "Exact textual engineering hints matched: {}",
            evidence
                .matched_artifacts
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !evidence.matched_scopes.is_empty() {
        reasons.push(format!(
            "Task applicability matched accepted Context scope: {}",
            evidence
                .matched_scopes
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    reasons
}

#[derive(Debug)]
struct SearchPage {
    results: Vec<SearchResult>,
    next_cursor: Option<String>,
    omitted: Vec<SearchOmitted>,
}

#[derive(Debug)]
struct PackedPage {
    estimated_tokens: usize,
    items: Vec<ContextPackItem>,
    omitted: Vec<ContextPackOmitted>,
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

fn pack_page(page: SearchPage, token_budget: usize) -> PackedPage {
    let mut estimated_tokens: usize = 0;
    let mut items = Vec::new();
    let mut omitted = Vec::new();
    for item in page.results {
        let context_id = item.context_id;
        let revision_id = item.revision_id;
        let full = pack_item(item, ContextPackDetail::Full);
        let full_tokens = serialized_tokens(&full);
        if estimated_tokens.saturating_add(full_tokens) <= token_budget {
            estimated_tokens += full_tokens;
            items.push(full);
            continue;
        }
        let summary = ContextPackItem {
            rationale: None,
            evidence: Vec::new(),
            detail: ContextPackDetail::Summary,
            ..full
        };
        let summary_tokens = serialized_tokens(&summary);
        if estimated_tokens.saturating_add(summary_tokens) <= token_budget {
            estimated_tokens += summary_tokens;
            items.push(summary);
            omitted.push(ContextPackOmitted {
                context_id: Some(context_id),
                revision_id: Some(revision_id),
                reason: "detail_token_budget".to_owned(),
                estimated_tokens: full_tokens.saturating_sub(summary_tokens),
                count: 1,
            });
        } else {
            omitted.push(ContextPackOmitted {
                context_id: Some(context_id),
                revision_id: Some(revision_id),
                reason: "token_budget".to_owned(),
                estimated_tokens: summary_tokens,
                count: 1,
            });
        }
    }
    omitted.extend(page.omitted.into_iter().map(|item| ContextPackOmitted {
        context_id: None,
        revision_id: None,
        reason: item.reason,
        estimated_tokens: 0,
        count: item.count,
    }));
    PackedPage {
        estimated_tokens,
        items,
        omitted,
    }
}

fn pack_item(item: SearchResult, detail: ContextPackDetail) -> ContextPackItem {
    ContextPackItem {
        space_id: item.space_id,
        context_id: item.context_id,
        revision_id: item.revision_id,
        title: item.title,
        kind: item.kind,
        status: item.status,
        statement: item.statement,
        rationale: Some(item.rationale),
        applicability: item.applicability,
        evidence: item.evidence,
        conflicts: item.conflicts,
        auto_injection_eligible: item.auto_injection_eligible,
        match_reason: item.match_reason,
        detail,
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
