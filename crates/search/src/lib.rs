//! Structured FTS5 search and deterministic, token-budgeted Context Packs.

use std::{collections::BTreeSet, str::FromStr};

use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use sctx_domain::{Applicability, ContextId, ContextKind, EvidenceId, RevisionId, SpaceId};
use sctx_index::{IndexMetadata, ProjectionIndex, search_tokens};
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
    /// Preferred Space is a ranking boost, while `filters.space_ids` is a hard restriction.
    pub preferred_space_id: Option<SpaceId>,
    pub page_size: usize,
    pub cursor: Option<String>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            filters: SearchFilters::default(),
            preferred_space_id: None,
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
    pub exact_space: bool,
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
    space_rank: i64,
    relevance_bits: u64,
    evidence_completeness: i64,
    context_id: String,
    revision_id: String,
    seen: usize,
}

#[derive(Debug)]
struct RankedRow {
    result: SearchResult,
    space_rank: i64,
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
    let space_rank = if request.preferred_space_id.is_some() {
        "CASE WHEN revision.space_id = ? THEN 0 ELSE 1 END"
    } else {
        "0"
    };
    let relevance = if match_expression.is_some() {
        "bm25(context_fts, 0.0, 0.0, 10.0, 8.0, 4.0, 2.0)"
    } else {
        "0.0"
    };
    let mut parameters = Vec::new();
    if let Some(space_id) = request.preferred_space_id {
        parameters.push(SqlValue::Text(space_id.to_string()));
    }
    parameters.extend(base_parameters);
    let simple_rank = match_expression.is_none() && request.preferred_space_id.is_none();
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
                SqlValue::Integer(cursor.space_rank),
                SqlValue::Integer(cursor.space_rank),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.space_rank),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Integer(cursor.space_rank),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Text(cursor.context_id.clone()),
                SqlValue::Integer(cursor.space_rank),
                SqlValue::Real(relevance),
                SqlValue::Integer(cursor.evidence_completeness),
                SqlValue::Text(cursor.context_id.clone()),
                SqlValue::Text(cursor.revision_id.clone()),
            ]);
            "WHERE space_rank > ?
          OR (space_rank = ? AND relevance > ?)
          OR (space_rank = ? AND relevance = ? AND evidence_completeness < ?)
          OR (space_rank = ? AND relevance = ? AND evidence_completeness = ? AND context_id > ?)
          OR (space_rank = ? AND relevance = ? AND evidence_completeness = ? AND context_id = ? AND revision_id > ?)"
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
        "space_rank ASC, relevance ASC, evidence_completeness DESC,
         context_id ASC, revision_id ASC"
    };
    let sql = format!(
        "WITH ranked AS (
           SELECT revision.space_id, revision.context_id, revision.revision_id,
                  COALESCE(space.title, ''), revision.kind, {status} AS result_status,
                  revision.statement, revision.rationale, revision.applicability_json,
                  revision.assumptions_json, revision.recheck_when_json,
                  item.auto_injection_eligible, {space_rank} AS space_rank,
                  {relevance} AS relevance, {evidence} AS evidence_completeness
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
        let row_space_rank = row.get(12).map_err(sql_error("read Space rank"))?;
        let row_relevance = row.get(13).map_err(sql_error("read BM25 rank"))?;
        let row_evidence = row
            .get(14)
            .map_err(sql_error("read Evidence completeness"))?;
        let evidence = load_evidence(connection, revision_id)?;
        let conflicts = load_conflicts(connection, context_id, revision_id)?;
        let match_reason = explain_match(
            &query_tokens,
            &title,
            &statement,
            &rationale,
            &evidence,
            row_space_rank == 0 && request.preferred_space_id.is_some(),
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
            space_rank: row_space_rank,
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
                    space_rank: row.space_rank,
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

#[allow(clippy::too_many_arguments)]
fn explain_match(
    query_tokens: &[String],
    title: &str,
    statement: &str,
    rationale: &str,
    evidence: &[EvidenceView],
    exact_space: bool,
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
        exact_space,
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
        }
    }

    fn percentile(samples: &[Duration], percentile: usize) -> Duration {
        let index = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
        samples[index]
    }
}
