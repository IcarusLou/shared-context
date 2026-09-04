use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, Transaction, params};
use sctx_domain::{
    ArtifactLocator, ContextGovernanceStatus, ContextSpaceAssociationOrigin, PublicationAction,
    RevisionId, RevisionLifecycle, SemanticConflictStatus, hints,
};
use sctx_event_schema::{EvidenceType, ReviewVerdict};

use crate::{
    IMPLEMENTATION_VERSIONS, normalize_search_text,
    project::{BuildInput, ProjectionDiagnostic},
    search_tokens, sql_error,
};

pub(crate) const NEXT_PREFIX: &str = "_next_";

const TABLES: [&str; 29] = [
    "meta",
    "source_file",
    "context_candidate",
    "candidate_submission",
    "candidate_submission_conflict",
    "space_projection",
    "intent_revision",
    "intent_head",
    "context_item",
    "context_revision",
    "context_relation",
    "engineering_reference",
    "review",
    "publication",
    "publication_head",
    "context_space_association",
    "context_space_association_head",
    "context_space_association_conflict",
    "candidate_confirmation",
    "candidate_confirmation_conflict",
    "evidence",
    "scope",
    "semantic_conflict",
    "conflict_resolution",
    "conflict",
    "diagnostic",
    "context_fts",
    "space_fts",
    "token_alias",
];

pub(crate) fn is_complete(connection: &rusqlite::Connection) -> crate::Result<bool> {
    // Every read synchronizes first, so this runs on the hot path: one scan of `sqlite_schema`
    // answers it instead of one statement per projection table.
    let present = connection
        .query_row(
            "SELECT COUNT(DISTINCT name) FROM sqlite_schema
             WHERE type = 'table' AND name IN (SELECT value FROM json_each(?1))",
            [serde_json::Value::from(TABLES.to_vec()).to_string()],
            |row| row.get::<_, usize>(0),
        )
        .map_err(sql_error("inspect core projection schema"))?;
    Ok(present == TABLES.len())
}

pub(crate) fn replace_projection(
    transaction: &Transaction<'_>,
    input: &BuildInput,
    tree_oid: &str,
    generation: u64,
    operational_warnings_json: &str,
) -> crate::Result<()> {
    drop_tables(transaction, NEXT_PREFIX)?;
    create_tables(transaction, NEXT_PREFIX)?;
    populate(
        transaction,
        NEXT_PREFIX,
        input,
        tree_oid,
        generation,
        operational_warnings_json,
    )?;
    drop_tables(transaction, "")?;
    for table in TABLES {
        transaction
            .execute_batch(&format!(
                "ALTER TABLE {NEXT_PREFIX}{table} RENAME TO {table};"
            ))
            .map_err(sql_error("activate shadow projection table"))?;
    }
    create_indexes(transaction)?;
    Ok(())
}

pub(crate) fn read_generation(connection: &rusqlite::Connection) -> crate::Result<Option<u64>> {
    let value = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'projection_generation'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error("read projection generation"))?;
    Ok(value.and_then(|value| value.parse::<u64>().ok()))
}

/// Physical table holding the append-only Event introduction cache.
///
/// This table is deliberately *not* part of [`TABLES`]: it is a rebuildable memo of a Git fact,
/// not a projection aggregate, so the shadow-table swap of a full rebuild leaves it in place and
/// a new process starts warm instead of re-walking history.
const EVENT_COMMIT_TABLE: &str = "event_commit";

const EVENT_COMMIT_DDL: &str = "CREATE TABLE IF NOT EXISTS event_commit (
    event_path TEXT NOT NULL,
    commit_oid TEXT NOT NULL,
    commit_time INTEGER NOT NULL,
    PRIMARY KEY (event_path, commit_oid)
) WITHOUT ROWID;";

/// Reads the memoized Event introductions, treating an absent table as an empty memo.
pub(crate) fn read_event_commits(
    connection: &rusqlite::Connection,
) -> crate::Result<BTreeMap<String, Vec<crate::git_tree::EventAddition>>> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
            [EVENT_COMMIT_TABLE],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error("inspect Event introduction cache"))?;
    if !exists {
        return Ok(BTreeMap::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT event_path, commit_oid, commit_time FROM event_commit
             ORDER BY event_path, commit_time, commit_oid",
        )
        .map_err(sql_error("prepare Event introduction read"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(sql_error("read Event introductions"))?;
    let mut cached: BTreeMap<String, Vec<crate::git_tree::EventAddition>> = BTreeMap::new();
    for row in rows {
        let (event_path, commit_oid, commit_time) =
            row.map_err(sql_error("collect Event introductions"))?;
        cached
            .entry(event_path)
            .or_default()
            .push(crate::git_tree::EventAddition {
                commit_oid,
                commit_time,
            });
    }
    Ok(cached)
}

/// Replaces the memo with everything one history walk observed.
pub(crate) fn write_event_commits(
    connection: &mut rusqlite::Connection,
    additions: &BTreeMap<String, Vec<crate::git_tree::EventAddition>>,
) -> crate::Result<()> {
    let transaction = connection
        .transaction()
        .map_err(sql_error("begin Event introduction cache write"))?;
    transaction
        .execute_batch(EVENT_COMMIT_DDL)
        .map_err(sql_error("create Event introduction cache"))?;
    transaction
        .execute_batch("DELETE FROM event_commit;")
        .map_err(sql_error("clear Event introduction cache"))?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT OR REPLACE INTO event_commit(event_path, commit_oid, commit_time)
                 VALUES (?1, ?2, ?3)",
            )
            .map_err(sql_error("prepare Event introduction write"))?;
        for (event_path, entries) in additions {
            for entry in entries {
                statement
                    .execute(params![event_path, entry.commit_oid, entry.commit_time])
                    .map_err(sql_error("write Event introduction"))?;
            }
        }
    }
    transaction
        .commit()
        .map_err(sql_error("commit Event introduction cache write"))
}

pub(crate) fn read_meta_value(
    connection: &rusqlite::Connection,
    key: &str,
) -> crate::Result<Option<String>> {
    connection
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(sql_error("read projection metadata"))
}

#[allow(clippy::too_many_lines)]
fn create_tables(transaction: &Transaction<'_>, prefix: &str) -> crate::Result<()> {
    let sql = format!(
        r"
CREATE TABLE {prefix}meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}source_file (
    path TEXT PRIMARY KEY,
    blob_oid TEXT NOT NULL,
    parse_status TEXT NOT NULL,
    event_id TEXT,
    diagnostic_code TEXT,
    diagnostic_message TEXT,
    content BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}context_candidate (
    candidate_id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    submission_id TEXT NOT NULL UNIQUE,
    source_episode_id TEXT NOT NULL,
    source_task_session_id TEXT NOT NULL,
    source_task_id TEXT NOT NULL,
    submission_content_hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    topic_key TEXT,
    problem_view TEXT,
    statement TEXT NOT NULL,
    rationale TEXT NOT NULL,
    applicability_json TEXT NOT NULL,
    assumptions_json TEXT NOT NULL,
    recheck_when_json TEXT NOT NULL,
    hints_json TEXT NOT NULL,
    hint_text TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    auto_injection_eligible INTEGER NOT NULL CHECK (auto_injection_eligible = 0),
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}candidate_submission (
    submission_id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL UNIQUE REFERENCES {prefix}context_candidate(candidate_id),
    event_id TEXT NOT NULL UNIQUE,
    source_episode_id TEXT NOT NULL,
    source_task_session_id TEXT NOT NULL,
    source_task_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    commit_oid TEXT NOT NULL,
    event_path TEXT NOT NULL UNIQUE
) WITHOUT ROWID;
CREATE TABLE {prefix}candidate_submission_conflict (
    submission_id TEXT PRIMARY KEY,
    event_ids_json TEXT NOT NULL,
    candidate_ids_json TEXT NOT NULL,
    content_hashes_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}space_projection (
    space_id TEXT PRIMARY KEY,
    current_intent_revision_id TEXT,
    title TEXT,
    problem TEXT,
    desired_outcome TEXT,
    intent_conflicted INTEGER NOT NULL CHECK (intent_conflicted IN (0, 1)),
    provisional INTEGER NOT NULL CHECK (provisional IN (0, 1)),
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}intent_revision (
    revision_id TEXT PRIMARY KEY,
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    parent_revision_ids_json TEXT NOT NULL,
    intent_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}intent_head (
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    revision_id TEXT NOT NULL REFERENCES {prefix}intent_revision(revision_id),
    PRIMARY KEY (space_id, revision_id)
) WITHOUT ROWID;
CREATE TABLE {prefix}context_item (
    context_id TEXT PRIMARY KEY,
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    governance_status TEXT NOT NULL,
    accepted_revision_id TEXT,
    accepted_publication_id TEXT,
    auto_injection_eligible INTEGER NOT NULL CHECK (auto_injection_eligible IN (0, 1)),
    accepted_at_unix_seconds INTEGER,
    superseded_by TEXT,
    stale_reason TEXT,
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}context_revision (
    revision_id TEXT PRIMARY KEY,
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    parent_revision_ids_json TEXT NOT NULL,
    kind TEXT NOT NULL,
    topic_key TEXT,
    problem_view TEXT,
    statement TEXT NOT NULL,
    rationale TEXT NOT NULL,
    applicability_json TEXT NOT NULL,
    assumptions_json TEXT NOT NULL,
    recheck_when_json TEXT NOT NULL,
    hints_json TEXT NOT NULL,
    hint_text TEXT NOT NULL,
    review_summary TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    evidence_completeness INTEGER NOT NULL CHECK (evidence_completeness BETWEEN 0 AND 1000),
    is_head INTEGER NOT NULL CHECK (is_head IN (0, 1))
) WITHOUT ROWID;
CREATE TABLE {prefix}context_relation (
    source_context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    source_revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    source_space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    target_context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    target_space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    kind TEXT NOT NULL,
    rationale TEXT NOT NULL,
    supports_json TEXT NOT NULL,
    PRIMARY KEY (source_revision_id, target_context_id, kind)
) WITHOUT ROWID;
CREATE TABLE {prefix}engineering_reference (
    reference_id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    repository_id TEXT NOT NULL,
    artifact_kind TEXT NOT NULL,
    relation TEXT NOT NULL,
    locator_json TEXT NOT NULL,
    supports TEXT NOT NULL,
    limitations_json TEXT NOT NULL,
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}review (
    event_id TEXT PRIMARY KEY,
    review_id TEXT NOT NULL UNIQUE,
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    verdict TEXT NOT NULL,
    reason TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}publication (
    publication_id TEXT PRIMARY KEY,
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    previous_publication_ids_json TEXT NOT NULL,
    action TEXT NOT NULL,
    revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    review_event_ids_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}publication_head (
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    publication_id TEXT NOT NULL REFERENCES {prefix}publication(publication_id),
    PRIMARY KEY (context_id, publication_id)
) WITHOUT ROWID;
CREATE TABLE {prefix}context_space_association (
    association_id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    primary_space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    related_space_ids_json TEXT NOT NULL,
    previous_association_ids_json TEXT NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('candidate_confirmation', 'correction')),
    origin_candidate_id TEXT REFERENCES {prefix}context_candidate(candidate_id),
    projection_json TEXT NOT NULL,
    CHECK (
        (origin = 'candidate_confirmation' AND origin_candidate_id IS NOT NULL) OR
        (origin = 'correction' AND origin_candidate_id IS NULL)
    )
) WITHOUT ROWID;
CREATE TABLE {prefix}context_space_association_head (
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    association_id TEXT NOT NULL REFERENCES {prefix}context_space_association(association_id),
    PRIMARY KEY (context_id, association_id)
) WITHOUT ROWID;
CREATE TABLE {prefix}context_space_association_conflict (
    context_id TEXT PRIMARY KEY REFERENCES {prefix}context_item(context_id),
    head_ids_json TEXT NOT NULL,
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}candidate_confirmation (
    confirmation_id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    candidate_id TEXT NOT NULL UNIQUE REFERENCES {prefix}context_candidate(candidate_id),
    submission_id TEXT NOT NULL,
    source_episode_id TEXT NOT NULL,
    source_task_session_id TEXT NOT NULL,
    source_task_id TEXT NOT NULL,
    result_context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    result_revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    primary_space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    related_space_ids_json TEXT NOT NULL,
    space_association_id TEXT NOT NULL REFERENCES {prefix}context_space_association(association_id),
    publication_id TEXT NOT NULL REFERENCES {prefix}publication(publication_id),
    created_space_id TEXT REFERENCES {prefix}space_projection(space_id),
    edits_json TEXT NOT NULL,
    final_content_hash TEXT NOT NULL,
    causal_refs_json TEXT NOT NULL,
    operation_hash TEXT NOT NULL,
    plan_hash TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    commit_oid TEXT NOT NULL,
    event_ids_json TEXT NOT NULL,
    event_paths_json TEXT NOT NULL,
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}candidate_confirmation_conflict (
    candidate_id TEXT PRIMARY KEY,
    confirmation_ids_json TEXT NOT NULL,
    event_ids_json TEXT NOT NULL,
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}evidence (
    evidence_id TEXT PRIMARY KEY,
    revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    kind TEXT NOT NULL,
    supports TEXT NOT NULL,
    content_json TEXT NOT NULL,
    interpretation TEXT NOT NULL,
    limitations_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}scope (
    revision_id TEXT NOT NULL REFERENCES {prefix}context_revision(revision_id),
    dimension TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (revision_id, dimension, value)
) WITHOUT ROWID;
CREATE TABLE {prefix}semantic_conflict (
    conflict_id TEXT PRIMARY KEY,
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    status TEXT NOT NULL,
    reason TEXT NOT NULL,
    applicability_json TEXT NOT NULL,
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}conflict_resolution (
    resolution_id TEXT PRIMARY KEY,
    conflict_id TEXT NOT NULL REFERENCES {prefix}semantic_conflict(conflict_id),
    previous_resolution_ids_json TEXT NOT NULL,
    related_publication_ids_json TEXT NOT NULL,
    results_json TEXT NOT NULL,
    rationale TEXT NOT NULL,
    is_head INTEGER NOT NULL CHECK (is_head IN (0, 1))
) WITHOUT ROWID;
CREATE TABLE {prefix}conflict (
    conflict_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    context_id TEXT REFERENCES {prefix}context_item(context_id),
    status TEXT NOT NULL,
    details_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}diagnostic (
    diagnostic_key TEXT PRIMARY KEY,
    source_path TEXT,
    code TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    event_ids_json TEXT NOT NULL,
    message TEXT NOT NULL
) WITHOUT ROWID;
CREATE VIRTUAL TABLE {prefix}context_fts USING fts5(
    context_id UNINDEXED,
    revision_id UNINDEXED,
    title,
    statement,
    rationale,
    evidence,
    problem_view,
    hint_text
);
CREATE VIRTUAL TABLE {prefix}space_fts USING fts5(
    space_id UNINDEXED,
    revision_id UNINDEXED,
    title,
    problem,
    desired_outcome,
    in_scope,
    out_of_scope,
    acceptance_conditions,
    domain_terms
);
CREATE TABLE {prefix}token_alias (
    token TEXT NOT NULL,
    alias TEXT NOT NULL,
    source TEXT NOT NULL,
    group_key TEXT NOT NULL,
    PRIMARY KEY (token, alias, source, group_key)
) WITHOUT ROWID;
"
    );
    transaction
        .execute_batch(&sql)
        .map_err(sql_error("create shadow projection schema"))
}

fn drop_tables(transaction: &Transaction<'_>, prefix: &str) -> crate::Result<()> {
    for table in TABLES.iter().rev() {
        transaction
            .execute_batch(&format!("DROP TABLE IF EXISTS {prefix}{table};"))
            .map_err(sql_error("drop projection table"))?;
    }
    Ok(())
}

fn create_indexes(transaction: &Transaction<'_>) -> crate::Result<()> {
    transaction
        .execute_batch(
            "CREATE INDEX context_revision_context_status_idx
                 ON context_revision(context_id, lifecycle, revision_id);
             CREATE INDEX context_revision_space_kind_status_idx
                 ON context_revision(space_id, kind, lifecycle, revision_id);
             CREATE INDEX context_revision_lifecycle_idx
                 ON context_revision(lifecycle, revision_id);
             CREATE INDEX context_revision_stable_rank_idx
                 ON context_revision(
                   lifecycle, evidence_completeness DESC, context_id, revision_id
                 );
             CREATE INDEX context_item_space_governance_idx
                 ON context_item(space_id, governance_status, auto_injection_eligible, context_id);
             CREATE INDEX evidence_revision_idx
                 ON evidence(revision_id, evidence_id);
             CREATE INDEX scope_lookup_idx
                 ON scope(dimension, value, revision_id);
             CREATE INDEX semantic_conflict_status_idx
                 ON semantic_conflict(status, conflict_id);
             CREATE INDEX conflict_context_status_idx
                 ON conflict(context_id, kind, status, conflict_key);
             CREATE INDEX publication_head_context_idx
                 ON publication_head(context_id, publication_id);
             CREATE INDEX context_space_association_context_idx
                 ON context_space_association(context_id, association_id);
             CREATE INDEX context_space_association_primary_idx
                 ON context_space_association(primary_space_id, context_id, association_id);
             CREATE INDEX candidate_confirmation_result_idx
                 ON candidate_confirmation(result_context_id, result_revision_id, confirmation_id);
             CREATE INDEX context_candidate_source_idx
                 ON context_candidate(source_episode_id, candidate_id);
             CREATE INDEX candidate_submission_candidate_idx
                 ON candidate_submission(candidate_id, submission_id);
             CREATE INDEX engineering_reference_context_idx
                 ON engineering_reference(context_id, revision_id, reference_id);
             CREATE INDEX engineering_reference_repository_idx
                 ON engineering_reference(repository_id, artifact_kind, reference_id);
             CREATE INDEX context_relation_target_idx
                 ON context_relation(target_context_id, kind, source_revision_id);
             CREATE INDEX context_relation_source_idx
                 ON context_relation(source_context_id, source_revision_id, kind);
             CREATE INDEX token_alias_token_idx
                 ON token_alias(token, source, alias);
             CREATE INDEX token_alias_group_idx
                 ON token_alias(group_key, token, alias);",
        )
        .map_err(sql_error("create projection query indexes"))
}

#[allow(clippy::too_many_lines)]
fn populate(
    transaction: &Transaction<'_>,
    prefix: &str,
    input: &BuildInput,
    tree_oid: &str,
    generation: u64,
    operational_warnings_json: &str,
) -> crate::Result<()> {
    let insert_meta = format!("INSERT INTO {prefix}meta(key, value) VALUES (?1, ?2)");
    transaction
        .execute(&insert_meta, params!["indexed_tree_oid", tree_oid])
        .map_err(sql_error("write indexed Tree metadata"))?;
    transaction
        .execute(
            &insert_meta,
            params!["projection_generation", generation.to_string()],
        )
        .map_err(sql_error("write projection generation"))?;
    // Additive to `meta`'s existing generic `(key, value)` shape: see
    // `crate::OPERATIONAL_WARNINGS_META_KEY` for why this needs no schema version bump. The caller
    // resolved this string before the shadow rebuild began -- either freshly from this
    // generation's own comparison, or carried forward from what `meta` already held -- so it is
    // written verbatim here without this function needing to know which.
    transaction
        .execute(
            &insert_meta,
            params![
                crate::OPERATIONAL_WARNINGS_META_KEY,
                operational_warnings_json
            ],
        )
        .map_err(sql_error("write operational warning metadata"))?;
    for (key, value) in IMPLEMENTATION_VERSIONS {
        transaction
            .execute(&insert_meta, params![key, value])
            .map_err(sql_error("write implementation version metadata"))?;
    }

    let source_sql = format!(
        "INSERT INTO {prefix}source_file(path, blob_oid, parse_status, event_id, diagnostic_code, diagnostic_message, content) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
    );
    for source in &input.source_files {
        transaction
            .execute(
                &source_sql,
                params![
                    source.path,
                    source.blob_oid,
                    source.parse_status,
                    source.event_id,
                    source.diagnostic_code,
                    source.diagnostic_message,
                    source.content
                ],
            )
            .map_err(sql_error("write source-file projection"))?;
    }

    let mut alias_rows = BTreeSet::new();
    for (candidate_id, projection) in &input.projection.candidates {
        let content = &projection.candidate.content;
        let candidate_prose = prose_hints(
            &content.statement,
            &content.rationale,
            content.evidence.iter().map(|evidence| {
                (
                    evidence.supports.as_str(),
                    &evidence.content,
                    evidence.interpretation.as_str(),
                )
            }),
        );
        for term in &candidate_prose.alias_seeds {
            collect_alias_rows(term, "identifier_split", &mut alias_rows);
        }
        let submission = input
            .projection
            .candidate_submissions
            .get(&projection.candidate.submission_id)
            .ok_or_else(|| {
                crate::Error::new(
                    crate::ErrorKind::InvariantViolation,
                    "valid Candidate lacks submission projection",
                )
            })?;
        let metadata = input
            .candidate_events
            .get(&projection.event_id)
            .ok_or_else(|| {
                crate::Error::new(
                    crate::ErrorKind::InvariantViolation,
                    "valid Candidate lacks Event metadata",
                )
            })?;
        if metadata.submission_id != submission.submission_id
            || metadata.candidate_id != submission.candidate_id
        {
            return Err(crate::Error::new(
                crate::ErrorKind::InvariantViolation,
                "Candidate submission metadata identity mismatch",
            ));
        }
        let batch_id = metadata.batch_id.as_deref().ok_or_else(|| {
            crate::Error::new(
                crate::ErrorKind::InvariantViolation,
                "Candidate Event lacks Writer batch metadata",
            )
        })?;
        let commit_oid = metadata.commit_oid.as_deref().ok_or_else(|| {
            crate::Error::new(
                crate::ErrorKind::InvariantViolation,
                "Candidate Event lacks introducing commit metadata",
            )
        })?;
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}context_candidate(candidate_id, event_id, submission_id, source_episode_id, source_task_session_id, source_task_id, submission_content_hash, kind, topic_key, problem_view, statement, rationale, applicability_json, assumptions_json, recheck_when_json, hints_json, hint_text, evidence_json, auto_injection_eligible, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 0, ?19)"
                ),
                params![
                    candidate_id.to_string(),
                    projection.event_id.to_string(),
                    submission.submission_id.to_string(),
                    submission.source_episode.episode_id.to_string(),
                    submission.source_episode.task_session_id.to_string(),
                    submission.source_episode.task_id.to_string(),
                    submission.content_hash,
                    enum_text(content.kind),
                    content.topic_key,
                    content.problem_view,
                    content.statement,
                    content.rationale,
                    json(&content.applicability)?,
                    json(&content.assumptions)?,
                    json(&content.recheck_when)?,
                    json(&content.hints)?,
                    hint_text(
                        &[
                            &content.hints,
                            &topic_key_terms(content.topic_key.as_deref()),
                        ],
                        &candidate_prose.terms,
                    ),
                    json(&content.evidence)?,
                    json(projection)?
                ],
            )
            .map_err(sql_error("write Context Candidate projection"))?;
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}candidate_submission(submission_id, candidate_id, event_id, source_episode_id, source_task_session_id, source_task_id, content_hash, batch_id, commit_oid, event_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
                ),
                params![
                    submission.submission_id.to_string(),
                    submission.candidate_id.to_string(),
                    submission.event_id.to_string(),
                    submission.source_episode.episode_id.to_string(),
                    submission.source_episode.task_session_id.to_string(),
                    submission.source_episode.task_id.to_string(),
                    submission.content_hash,
                    batch_id,
                    commit_oid,
                    metadata.event_path,
                ],
            )
            .map_err(sql_error("write Candidate submission projection"))?;
    }
    for (submission_id, conflict) in &input.projection.candidate_submission_conflicts {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}candidate_submission_conflict(submission_id, event_ids_json, candidate_ids_json, content_hashes_json) VALUES (?1, ?2, ?3, ?4)"
                ),
                params![
                    submission_id.to_string(),
                    json(&conflict.event_ids)?,
                    json(&conflict.candidate_ids)?,
                    json(&conflict.content_hashes)?,
                ],
            )
            .map_err(sql_error("write Candidate submission conflict"))?;
    }

    let reference_hints = revision_reference_hints(input);
    for projection in input.projection.engineering_references.values() {
        for term in locator_identifier_sources(&projection.reference.locator) {
            collect_alias_rows(&term, "identifier_split", &mut alias_rows);
        }
    }

    for (space_id, space) in &input.projection.spaces {
        let current_intent = (space.intent.heads.len() == 1)
            .then(|| space.intent.heads.first().copied())
            .flatten();
        let intent =
            current_intent.and_then(|revision_id| space.intent.revisions.get(&revision_id));
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}space_projection(space_id, current_intent_revision_id, title, problem, desired_outcome, intent_conflicted, provisional, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                ),
                params![
                    space_id.to_string(),
                    current_intent.map(|id| id.to_string()),
                    intent.map(|revision| revision.intent.title.as_str()),
                    intent.map(|revision| revision.intent.problem.as_str()),
                    intent.map(|revision| revision.intent.desired_outcome.as_str()),
                    i64::from(space.intent.heads.len() > 1),
                    // Only a single resolved Intent head can say whether the Space is still the
                    // server's provisional proposal; a conflicted Space reports 0.
                    i64::from(intent.is_some_and(|revision| revision.provisional)),
                    json(space)?
                ],
            )
            .map_err(sql_error("write Space projection"))?;
        for (revision_id, revision) in &space.intent.revisions {
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}intent_revision(revision_id, space_id, parent_revision_ids_json, intent_json) VALUES (?1, ?2, ?3, ?4)"
                    ),
                    params![
                        revision_id.to_string(),
                        space_id.to_string(),
                        json(&revision.parent_revision_ids)?,
                        json(&revision.intent)?
                    ],
                )
                .map_err(sql_error("write Intent revision"))?;
        }
        for revision_id in &space.intent.heads {
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}intent_head(space_id, revision_id) VALUES (?1, ?2)"
                    ),
                    params![space_id.to_string(), revision_id.to_string()],
                )
                .map_err(sql_error("write Intent head"))?;
            let revision = space
                .intent
                .revisions
                .get(revision_id)
                .expect("the reducer only projects resolvable Intent heads");
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}space_fts(space_id, revision_id, title, problem, desired_outcome, in_scope, out_of_scope, acceptance_conditions, domain_terms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                    ),
                    params![
                        space_id.to_string(),
                        revision_id.to_string(),
                        normalize_search_text(&revision.intent.title),
                        normalize_search_text(&revision.intent.problem),
                        normalize_search_text(&revision.intent.desired_outcome),
                        normalize_search_text(&revision.intent.in_scope.join(" ")),
                        normalize_search_text(&revision.intent.out_of_scope.join(" ")),
                        normalize_search_text(&revision.intent.acceptance_conditions.join(" ")),
                        normalize_search_text(&revision.intent.domain_terms.join(" ")),
                    ],
                )
                .map_err(sql_error("write Space Intent FTS5 projection"))?;
            for term in &revision.intent.domain_terms {
                collect_alias_rows(term, "domain_term", &mut alias_rows);
            }
        }
        if space.intent.heads.len() > 1 {
            insert_conflict(
                transaction,
                prefix,
                &format!("intent:{space_id}"),
                "intent",
                &space_id.to_string(),
                None,
                "open",
                &json(&space.intent.heads)?,
            )?;
        }

        for (context_id, context) in &space.contexts {
            let (governance_status, accepted_publication, accepted_revision) =
                governance_columns(&context.governance);
            let accepted_publication_id = match &context.governance {
                ContextGovernanceStatus::Accepted { publication_id, .. } => Some(*publication_id),
                _ => None,
            };
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}context_item(context_id, space_id, governance_status, accepted_revision_id, accepted_publication_id, auto_injection_eligible, accepted_at_unix_seconds, superseded_by, stale_reason, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8)"
                    ),
                    params![
                        context_id.to_string(),
                        space_id.to_string(),
                        governance_status,
                        accepted_revision,
                        accepted_publication,
                        i64::from(context.auto_injection.eligible),
                        accepted_publication_id
                            .and_then(|id| input.publication_times.get(&id).copied()),
                        json(context)?
                    ],
                )
                .map_err(sql_error("write Context item"))?;

            for (revision_id, revision_projection) in &context.revisions {
                let revision = &revision_projection.revision;
                let prose = prose_hints(
                    &revision.statement,
                    &revision.rationale,
                    revision.evidence.iter().map(|evidence| {
                        (
                            evidence.supports.as_str(),
                            &evidence.content,
                            evidence.interpretation.as_str(),
                        )
                    }),
                );
                for term in &prose.alias_seeds {
                    collect_alias_rows(term, "identifier_split", &mut alias_rows);
                }
                for term in topic_key_alias_seeds(revision.topic_key.as_deref()) {
                    collect_alias_rows(&term, "identifier_split", &mut alias_rows);
                }
                let topic_terms = topic_key_terms(revision.topic_key.as_deref());
                let hint_text = hint_text(
                    &[
                        reference_hints
                            .get(revision_id)
                            .map_or(&[][..], Vec::as_slice),
                        &revision.hints,
                        &topic_terms,
                    ],
                    &prose.terms,
                );
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {prefix}context_revision(revision_id, context_id, space_id, parent_revision_ids_json, kind, topic_key, problem_view, statement, rationale, applicability_json, assumptions_json, recheck_when_json, hints_json, hint_text, review_summary, lifecycle, evidence_completeness, is_head) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
                        ),
                        params![
                            revision_id.to_string(),
                            context_id.to_string(),
                            space_id.to_string(),
                            json(&revision.parent_revision_ids)?,
                            enum_text(revision.kind),
                            revision.topic_key,
                            revision.problem_view,
                            revision.statement,
                            revision.rationale,
                            json(&revision.applicability)?,
                            json(&revision.assumptions)?,
                            json(&revision.recheck_when)?,
                            json(&revision.hints)?,
                            hint_text.as_str(),
                            enum_text(revision_projection.review_summary),
                            lifecycle_text(revision_projection.lifecycle),
                            evidence_completeness(&revision.evidence),
                            i64::from(revision_projection.is_head)
                        ],
                    )
                    .map_err(sql_error("write Context revision"))?;

                let mut evidence_search = Vec::new();
                for evidence in &revision.evidence {
                    let content = json(&evidence.content)?;
                    evidence_search.push(format!(
                        "{} {} {} {}",
                        evidence.supports,
                        content,
                        evidence.interpretation,
                        evidence.limitations.join(" ")
                    ));
                    transaction
                        .execute(
                            &format!(
                                "INSERT INTO {prefix}evidence(evidence_id, revision_id, context_id, space_id, kind, supports, content_json, interpretation, limitations_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                            ),
                            params![
                                evidence.evidence_id.to_string(),
                                revision_id.to_string(),
                                context_id.to_string(),
                                space_id.to_string(),
                                evidence_text(evidence.kind),
                                evidence.supports,
                                content,
                                evidence.interpretation,
                                json(&evidence.limitations)?
                            ],
                        )
                        .map_err(sql_error("write Evidence projection"))?;
                }
                evidence_search.extend(revision.relations.iter().map(|relation| {
                    format!(
                        "{} {} {} {}",
                        enum_text(relation.kind),
                        relation.target_context_id,
                        relation.rationale,
                        relation.supports.join(" ")
                    )
                }));
                for (dimension, values) in [
                    ("domain", &revision.applicability.domains),
                    ("platform", &revision.applicability.platforms),
                    ("condition", &revision.applicability.conditions),
                ] {
                    for value in values {
                        transaction
                            .execute(
                                &format!(
                                    "INSERT INTO {prefix}scope(revision_id, dimension, value) VALUES (?1, ?2, ?3)"
                                ),
                                params![revision_id.to_string(), dimension, value],
                            )
                            .map_err(sql_error("write Scope projection"))?;
                    }
                }
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {prefix}context_fts(context_id, revision_id, title, statement, rationale, evidence, problem_view, hint_text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                        ),
                        params![
                            context_id.to_string(),
                            revision_id.to_string(),
                            normalize_search_text(&context_display_title(&revision.statement)),
                            normalize_search_text(&revision.statement),
                            normalize_search_text(&revision.rationale),
                            normalize_search_text(&evidence_search.join(" ")),
                            normalize_search_text(revision.problem_view.as_deref().unwrap_or("")),
                            hint_text.as_str()
                        ],
                    )
                    .map_err(sql_error("write FTS5 projection"))?;
            }
            for (event_id, review) in &context.reviews {
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {prefix}review(event_id, review_id, context_id, space_id, revision_id, verdict, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                        ),
                        params![
                            event_id.to_string(),
                            review.review_id.to_string(),
                            context_id.to_string(),
                            space_id.to_string(),
                            review.revision_id.to_string(),
                            review_text(review.verdict),
                            review.reason
                        ],
                    )
                    .map_err(sql_error("write Review projection"))?;
            }
            for (publication_id, publication) in &context.publications {
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {prefix}publication(publication_id, context_id, space_id, previous_publication_ids_json, action, revision_id, review_event_ids_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                        ),
                        params![
                            publication_id.to_string(),
                            context_id.to_string(),
                            space_id.to_string(),
                            json(&publication.previous_publication_ids)?,
                            publication_text(publication.action),
                            publication.revision_id.to_string(),
                            json(&publication.review_event_ids)?
                        ],
                    )
                    .map_err(sql_error("write Publication projection"))?;
            }
            for publication_id in &context.publication_heads {
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {prefix}publication_head(context_id, publication_id) VALUES (?1, ?2)"
                        ),
                        params![context_id.to_string(), publication_id.to_string()],
                    )
                    .map_err(sql_error("write Publication head"))?;
            }
            if context.publication_heads.len() > 1 {
                insert_conflict(
                    transaction,
                    prefix,
                    &format!("publication:{context_id}"),
                    "publication",
                    &space_id.to_string(),
                    Some(&context_id.to_string()),
                    "open",
                    &json(&context.publication_heads)?,
                )?;
            }
        }
    }

    for (source_space_id, space) in &input.projection.spaces {
        for (source_context_id, context) in &space.contexts {
            for (source_revision_id, revision) in &context.revisions {
                for relation in &revision.revision.relations {
                    let target_space_id = input
                        .projection
                        .spaces
                        .iter()
                        .find(|(_, target_space)| {
                            target_space
                                .contexts
                                .contains_key(&relation.target_context_id)
                        })
                        .map(|(space_id, _)| *space_id)
                        .expect("Reducer only projects relations to valid Contexts");
                    transaction
                        .execute(
                            &format!(
                                "INSERT INTO {prefix}context_relation(source_context_id, source_revision_id, source_space_id, target_context_id, target_space_id, kind, rationale, supports_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                            ),
                            params![
                                source_context_id.to_string(),
                                source_revision_id.to_string(),
                                source_space_id.to_string(),
                                relation.target_context_id.to_string(),
                                target_space_id.to_string(),
                                enum_text(relation.kind),
                                relation.rationale,
                                json(&relation.supports)?
                            ],
                        )
                        .map_err(sql_error("write Context Relation projection"))?;
                }
            }
        }
    }

    for (association_id, projection) in &input.projection.context_space_associations {
        let (origin, origin_candidate_id) = match projection.association.origin {
            ContextSpaceAssociationOrigin::CandidateConfirmation { candidate_id } => {
                ("candidate_confirmation", Some(candidate_id.to_string()))
            }
            ContextSpaceAssociationOrigin::Correction => ("correction", None),
        };
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}context_space_association(association_id, event_id, context_id, primary_space_id, related_space_ids_json, previous_association_ids_json, origin, origin_candidate_id, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    association_id.to_string(),
                    projection.event_id.to_string(),
                    projection.association.context_id.to_string(),
                    projection.association.primary_space_id.to_string(),
                    json(&projection.association.related_space_ids)?,
                    json(&projection.association.previous_association_ids)?,
                    origin,
                    origin_candidate_id,
                    json(projection)?,
                ],
            )
            .map_err(sql_error("write Context Space Association projection"))?;
    }
    for (context_id, head_ids) in &input.projection.context_space_association_heads {
        for association_id in head_ids {
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}context_space_association_head(context_id, association_id) VALUES (?1, ?2)"
                    ),
                    params![context_id.to_string(), association_id.to_string()],
                )
                .map_err(sql_error("write Context Space Association head"))?;
        }
    }
    for (context_id, conflict) in &input.projection.context_space_association_conflicts {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}context_space_association_conflict(context_id, head_ids_json, projection_json) VALUES (?1, ?2, ?3)"
                ),
                params![
                    context_id.to_string(),
                    json(&conflict.head_ids)?,
                    json(conflict)?,
                ],
            )
            .map_err(sql_error("write Context Space Association conflict"))?;
    }
    for (confirmation_id, projection) in &input.projection.candidate_confirmations {
        let confirmation = &projection.confirmation;
        let metadata = input
            .confirmation_events
            .get(&projection.event_id)
            .ok_or_else(|| crate::invariant("valid Candidate Confirmation lacks Event metadata"))?;
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}candidate_confirmation(confirmation_id, event_id, candidate_id, submission_id, source_episode_id, source_task_session_id, source_task_id, result_context_id, result_revision_id, primary_space_id, related_space_ids_json, space_association_id, publication_id, created_space_id, edits_json, final_content_hash, causal_refs_json, operation_hash, plan_hash, batch_id, commit_oid, event_ids_json, event_paths_json, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)"
                ),
                params![
                    confirmation_id.to_string(),
                    projection.event_id.to_string(),
                    confirmation.candidate_id.to_string(),
                    confirmation.submission_id.to_string(),
                    confirmation.source_episode.episode_id.to_string(),
                    confirmation.source_episode.task_session_id.to_string(),
                    confirmation.source_episode.task_id.to_string(),
                    confirmation.result_context_id.to_string(),
                    confirmation.result_revision_id.to_string(),
                    confirmation.primary_space_id.to_string(),
                    json(&confirmation.related_space_ids)?,
                    confirmation.space_association_id.to_string(),
                    confirmation.publication_id.to_string(),
                    confirmation.created_space_id.map(|id| id.to_string()),
                    json(&confirmation.edits)?,
                    confirmation.final_content_hash,
                    json(&confirmation.causal_refs)?,
                    metadata.operation_hash.as_ref().ok_or_else(|| crate::invariant(
                        "Candidate Confirmation lacks operation hash"
                    ))?,
                    metadata.plan_hash.as_ref().ok_or_else(|| crate::invariant(
                        "Candidate Confirmation lacks plan hash"
                    ))?,
                    metadata.batch_id.as_ref().ok_or_else(|| crate::invariant(
                        "Candidate Confirmation lacks batch ID"
                    ))?,
                    metadata.commit_oid.as_ref().ok_or_else(|| crate::invariant(
                        "Candidate Confirmation lacks introducing commit"
                    ))?,
                    json(&metadata.batch_event_ids)?,
                    json(&metadata.batch_event_paths)?,
                    json(projection)?,
                ],
            )
            .map_err(sql_error("write Candidate Confirmation projection"))?;
    }
    for (candidate_id, conflict) in &input.projection.candidate_confirmation_conflicts {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}candidate_confirmation_conflict(candidate_id, confirmation_ids_json, event_ids_json, projection_json) VALUES (?1, ?2, ?3, ?4)"
                ),
                params![
                    candidate_id.to_string(),
                    json(&conflict.confirmation_ids)?,
                    json(&conflict.event_ids)?,
                    json(conflict)?,
                ],
            )
            .map_err(sql_error("write Candidate Confirmation conflict"))?;
    }

    for (reference_id, projection) in &input.projection.engineering_references {
        let reference = &projection.reference;
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}engineering_reference(reference_id, event_id, space_id, context_id, revision_id, repository_id, artifact_kind, relation, locator_json, supports, limitations_json, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
                ),
                params![
                    reference_id.to_string(),
                    projection.event_id.to_string(),
                    projection.space_id.to_string(),
                    projection.context_id.to_string(),
                    projection.revision_id.to_string(),
                    reference.repository_id.to_string(),
                    enum_text(reference.artifact_kind),
                    enum_text(reference.relation),
                    json(&reference.locator)?,
                    reference.supports,
                    json(&reference.limitations)?,
                    json(projection)?
                ],
            )
            .map_err(sql_error("write Engineering Reference projection"))?;
    }

    for candidate in &input.projection.semantic_conflict_candidates {
        let participants = json(&candidate.participants)?;
        let key = format!(
            "semantic_candidate:{}:{}:{}",
            candidate.space_id, candidate.topic_key, participants
        );
        insert_conflict(
            transaction,
            prefix,
            &key,
            "semantic_candidate",
            &candidate.space_id.to_string(),
            None,
            "candidate",
            &json(candidate)?,
        )?;
    }

    for (conflict_id, conflict) in &input.projection.semantic_conflicts {
        let status = semantic_status(&conflict.status);
        transaction
            .execute(
                &format!(
                    "INSERT INTO {prefix}semantic_conflict(conflict_id, space_id, status, reason, applicability_json, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                params![
                    conflict_id.to_string(),
                    conflict.space_id.to_string(),
                    status,
                    conflict.conflict.reason,
                    json(&conflict.conflict.applicability)?,
                    json(conflict)?
                ],
            )
            .map_err(sql_error("write Semantic Conflict projection"))?;
        for (resolution_id, resolution) in &conflict.resolutions {
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}conflict_resolution(resolution_id, conflict_id, previous_resolution_ids_json, related_publication_ids_json, results_json, rationale, is_head) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                    ),
                    params![
                        resolution_id.to_string(),
                        conflict_id.to_string(),
                        json(&resolution.previous_resolution_ids)?,
                        json(&resolution.related_publication_ids)?,
                        json(&resolution.results)?,
                        resolution.rationale,
                        i64::from(conflict.resolution_heads.contains(resolution_id))
                    ],
                )
                .map_err(sql_error("write Conflict Resolution projection"))?;
        }
        insert_conflict(
            transaction,
            prefix,
            &format!("semantic:{conflict_id}"),
            "semantic",
            &conflict.space_id.to_string(),
            None,
            status,
            &json(conflict)?,
        )?;
    }

    let insert_alias = format!(
        "INSERT OR IGNORE INTO {prefix}token_alias(token, alias, source, group_key) VALUES (?1, ?2, ?3, ?4)"
    );
    for (token, alias, source, group_key) in &alias_rows {
        transaction
            .execute(&insert_alias, params![token, alias, source, group_key])
            .map_err(sql_error("write token alias projection"))?;
    }

    for diagnostic in &input.diagnostics {
        insert_diagnostic(transaction, prefix, diagnostic)?;
    }
    refresh_superseded_by(transaction, prefix)?;
    Ok(())
}

/// Recomputes the local `superseded_by` derivation for every projected Context.
///
/// `Supersedes` is an ordinary immutable Relation on the superseding revision; nothing in Git
/// marks the target. Only an *accepted* revision's Relation counts, and the derivation is a pure
/// function of the projected Relations, so it is recomputed rather than incrementally patched.
/// Deterministic tie-break: the lowest superseding Context ID wins when several claim one target.
fn refresh_superseded_by(transaction: &Transaction<'_>, prefix: &str) -> crate::Result<()> {
    transaction
        .execute_batch(&format!(
            "UPDATE {prefix}context_item SET superseded_by = (
                 SELECT MIN(relation.source_context_id)
                 FROM {prefix}context_relation AS relation
                 JOIN {prefix}context_item AS source
                   ON source.context_id = relation.source_context_id
                 WHERE relation.kind = 'supersedes'
                   AND relation.target_context_id = {prefix}context_item.context_id
                   AND relation.source_context_id <> {prefix}context_item.context_id
                   AND source.governance_status = 'accepted'
                   AND source.accepted_revision_id = relation.source_revision_id
             );"
        ))
        .map_err(sql_error("derive superseded Context state"))
}

pub(crate) fn cached_blobs(
    connection: &rusqlite::Connection,
) -> crate::Result<Vec<crate::git_tree::TreeBlob>> {
    let mut statement = connection
        .prepare("SELECT path, blob_oid, content FROM source_file ORDER BY path")
        .map_err(sql_error("prepare cached source read"))?;
    statement
        .query_map([], |row| {
            Ok(crate::git_tree::TreeBlob {
                path: row.get(0)?,
                oid: row.get(1)?,
                bytes: row.get(2)?,
            })
        })
        .map_err(sql_error("read cached source rows"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error("collect cached source rows"))
}

/// Replaces only proven-affected Space aggregates while atomically advancing source metadata and
/// diagnostics. A complete shadow projection is used as the deterministic source of replacement
/// rows; readers observe either the old or new generation, never an intermediate mixture.
#[allow(clippy::too_many_lines)]
pub(crate) fn replace_projection_incremental(
    transaction: &Transaction<'_>,
    input: &BuildInput,
    tree_oid: &str,
    generation: u64,
    affected_spaces: &std::collections::BTreeSet<String>,
    operational_warnings_json: &str,
) -> crate::Result<()> {
    drop_tables(transaction, NEXT_PREFIX)?;
    create_tables(transaction, NEXT_PREFIX)?;
    populate(
        transaction,
        NEXT_PREFIX,
        input,
        tree_oid,
        generation,
        operational_warnings_json,
    )?;
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _affected_space(space_id TEXT PRIMARY KEY) WITHOUT ROWID;
             DELETE FROM _affected_space;",
        )
        .map_err(sql_error("prepare affected Space closure"))?;
    for space_id in affected_spaces {
        transaction
            .execute(
                "INSERT INTO _affected_space(space_id) VALUES (?1)",
                [space_id],
            )
            .map_err(sql_error("record affected Space"))?;
    }

    transaction
        .execute_batch(
            "DELETE FROM source_file;
             INSERT INTO source_file SELECT * FROM _next_source_file;
             DELETE FROM candidate_confirmation_conflict;
             DELETE FROM candidate_confirmation;
             DELETE FROM context_space_association_conflict;
             DELETE FROM context_space_association_head;
             DELETE FROM context_space_association;
             DELETE FROM candidate_submission;
             DELETE FROM context_candidate;
             INSERT INTO context_candidate SELECT * FROM _next_context_candidate;
             INSERT INTO candidate_submission SELECT * FROM _next_candidate_submission;
             DELETE FROM candidate_submission_conflict;
             INSERT INTO candidate_submission_conflict SELECT * FROM _next_candidate_submission_conflict;
             DELETE FROM diagnostic;
             INSERT INTO diagnostic SELECT * FROM _next_diagnostic;
             DELETE FROM token_alias;
             INSERT INTO token_alias SELECT * FROM _next_token_alias;

             DELETE FROM space_fts WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM context_fts WHERE revision_id IN (
                 SELECT revision_id FROM context_revision
                 WHERE space_id IN (SELECT space_id FROM _affected_space)
             );
             DELETE FROM conflict WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM context_relation
                 WHERE source_space_id IN (SELECT space_id FROM _affected_space)
                    OR target_space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM engineering_reference
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM conflict_resolution WHERE conflict_id IN (
                 SELECT conflict_id FROM semantic_conflict
                 WHERE space_id IN (SELECT space_id FROM _affected_space)
             );
             DELETE FROM semantic_conflict WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM scope WHERE revision_id IN (
                 SELECT revision_id FROM context_revision
                 WHERE space_id IN (SELECT space_id FROM _affected_space)
             );
             DELETE FROM evidence WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM publication_head WHERE context_id IN (
                 SELECT context_id FROM context_item
                 WHERE space_id IN (SELECT space_id FROM _affected_space)
             );
             DELETE FROM publication WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM review WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM context_revision WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM context_item WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM intent_head WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM intent_revision WHERE space_id IN (SELECT space_id FROM _affected_space);
             DELETE FROM space_projection WHERE space_id IN (SELECT space_id FROM _affected_space);

             INSERT INTO space_projection SELECT * FROM _next_space_projection
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO intent_revision SELECT * FROM _next_intent_revision
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO intent_head SELECT * FROM _next_intent_head
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO space_fts SELECT * FROM _next_space_fts
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO context_item SELECT * FROM _next_context_item
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO context_revision SELECT * FROM _next_context_revision
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO context_relation SELECT * FROM _next_context_relation
                 WHERE source_space_id IN (SELECT space_id FROM _affected_space)
                    OR target_space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO engineering_reference SELECT * FROM _next_engineering_reference
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO review SELECT * FROM _next_review
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO publication SELECT * FROM _next_publication
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO publication_head SELECT next.* FROM _next_publication_head AS next
                 JOIN _next_context_item AS item USING(context_id)
                 WHERE item.space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO evidence SELECT * FROM _next_evidence
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO scope SELECT next.* FROM _next_scope AS next
                 JOIN _next_context_revision AS revision USING(revision_id)
                 WHERE revision.space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO context_fts SELECT next.* FROM _next_context_fts AS next
                 JOIN _next_context_revision AS revision USING(revision_id)
                 WHERE revision.space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO semantic_conflict SELECT * FROM _next_semantic_conflict
                 WHERE space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO conflict_resolution SELECT next.* FROM _next_conflict_resolution AS next
                 JOIN _next_semantic_conflict AS conflict USING(conflict_id)
                 WHERE conflict.space_id IN (SELECT space_id FROM _affected_space);
             INSERT INTO conflict SELECT * FROM _next_conflict
                 WHERE space_id IN (SELECT space_id FROM _affected_space);

             INSERT INTO context_space_association
                 SELECT * FROM _next_context_space_association;
             INSERT INTO context_space_association_head
                 SELECT * FROM _next_context_space_association_head;
             INSERT INTO context_space_association_conflict
                 SELECT * FROM _next_context_space_association_conflict;
             INSERT INTO candidate_confirmation SELECT * FROM _next_candidate_confirmation;
             INSERT INTO candidate_confirmation_conflict
                 SELECT * FROM _next_candidate_confirmation_conflict;

             DELETE FROM meta;
             INSERT INTO meta SELECT * FROM _next_meta;",
        )
        .map_err(sql_error("replace affected projection closure"))?;
    // `Supersedes` can cross Space boundaries, so the derivation is recomputed over the whole
    // live projection instead of only the replaced Space closure.
    refresh_superseded_by(transaction, "")?;
    drop_tables(transaction, NEXT_PREFIX)?;
    transaction
        .execute_batch("DROP TABLE _affected_space;")
        .map_err(sql_error("drop affected Space closure"))?;
    Ok(())
}

/// Maximum characters of a Context statement used as its own display title.
const CONTEXT_TITLE_MAX_CHARS: usize = 60;

/// Mirrors the retrieval-side Context title derivation so FTS `title` indexes the Context itself.
fn context_display_title(statement: &str) -> String {
    let trimmed = statement.trim();
    if trimmed.chars().count() <= CONTEXT_TITLE_MAX_CHARS {
        return trimmed.to_owned();
    }
    let mut title = trimmed
        .chars()
        .take(CONTEXT_TITLE_MAX_CHARS)
        .collect::<String>();
    title.push('\u{2026}');
    title
}

/// Path, basename, and extension-free basename forms of one repository-relative path.
fn path_hint_terms(path: &str) -> Vec<String> {
    let mut terms = vec![path.to_owned()];
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename != path && !basename.is_empty() {
        terms.push(basename.to_owned());
    }
    if let Some((stem, _)) = basename.rsplit_once('.')
        && !stem.is_empty()
    {
        terms.push(stem.to_owned());
    }
    terms
}

/// Identifier-shaped coordinates of one Artifact locator, used for alias groups.
fn locator_identifier_sources(locator: &ArtifactLocator) -> Vec<String> {
    let path = locator.path().as_str();
    let basename = path.rsplit('/').next().unwrap_or(path);
    let stem = basename
        .rsplit_once('.')
        .map_or(basename, |(stem, _)| stem)
        .to_owned();
    let mut sources = Vec::new();
    if !stem.is_empty() {
        sources.push(stem);
    }
    match locator {
        ArtifactLocator::File { .. } | ArtifactLocator::Module { .. } => {}
        ArtifactLocator::Api {
            operation,
            normalized_route,
            ..
        } => {
            sources.push(operation.clone());
            sources.push(normalized_route.clone());
        }
        ArtifactLocator::Schema {
            namespace,
            qualified_name,
            ..
        } => {
            sources.push(namespace.clone());
            sources.push(qualified_name.clone());
        }
        ArtifactLocator::Symbol {
            module,
            enclosing_type,
            symbol_name,
            ..
        } => {
            sources.push(module.clone());
            if let Some(enclosing_type) = enclosing_type {
                sources.push(enclosing_type.clone());
            }
            sources.push(symbol_name.clone());
        }
        ArtifactLocator::Test {
            qualified_test_name,
            ..
        } => sources.push(qualified_test_name.clone()),
    }
    sources
}

/// Every searchable hint term contributed by the Engineering References of one revision.
fn revision_reference_hints(input: &BuildInput) -> BTreeMap<RevisionId, Vec<String>> {
    let mut hints: BTreeMap<RevisionId, Vec<String>> = BTreeMap::new();
    for projection in input.projection.engineering_references.values() {
        let entry = hints.entry(projection.revision_id).or_default();
        entry.extend(path_hint_terms(
            projection.reference.locator.path().as_str(),
        ));
        entry.extend(locator_identifier_sources(&projection.reference.locator));
    }
    for terms in hints.values_mut() {
        terms.sort();
        terms.dedup();
    }
    hints
}

/// Normalized searchable text built from every hint source of one revision or Candidate.
///
/// The sources are the derived Engineering Reference locators, the identifiers and path spellings
/// read out of the free prose, and the unresolved hints the Claim carried. Merging them here is
/// what gives a Context accepted before server-side derivation existed a populated `hint_text`
/// after one `index rebuild`.
fn hint_text(sources: &[&[String]], prose_terms: &[String]) -> String {
    let terms = sources
        .iter()
        .flat_map(|source| source.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut text = normalize_search_text(&terms.into_iter().collect::<Vec<_>>().join(" "));
    // Authored hints and derived locators are indexed in full, split parts included: a reviewer
    // wrote them down precisely because they are the retrieval handle. Prose identifiers are not
    // authored hints — they are a by-product of how the Claim happens to be worded — so only the
    // whole identifier is indexed. Indexing `compile`, `debug` and `kotlin` because a summary said
    // `compileDebugKotlin` would let one incidental spelling outrank the Context that a natural
    // language question is actually about, which is the ranking regression this guards.
    let present = text
        .split_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for compound in prose_terms
        .iter()
        .filter_map(|term| compound_hint_token(term))
        .collect::<BTreeSet<_>>()
    {
        if present.contains(&compound) {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&compound);
    }
    text
}

/// Retrieval terms one `topic_key` contributes to `hint_text`.
///
/// A server-derived key is `{kind}:{repository}:{path}` or `{kind}:text:{stem}`, so only the last
/// segment names anything a question is asked about: the leading kind and the Repository identity
/// are coordinates, and indexing them would make every `decision` share a token. An Agent-authored
/// key carries no segments at all and is read whole. The spelling is then read exactly the way
/// Claim prose is read, so `src/main/PoiEntranceAssem.kt` contributes its basename and stem.
fn topic_key_terms(topic_key: Option<&str>) -> Vec<String> {
    topic_key_spelling(topic_key)
        .map(|spelling| hints::derived_hint_terms([spelling]))
        .unwrap_or_default()
}

/// The `token_alias` seeds of one `topic_key`: its spelling, split like any other identifier.
fn topic_key_alias_seeds(topic_key: Option<&str>) -> Vec<String> {
    topic_key_spelling(topic_key)
        .map(|spelling| vec![spelling.to_owned()])
        .unwrap_or_default()
}

/// The one segment of a `topic_key` that names a repository coordinate.
fn topic_key_spelling(topic_key: Option<&str>) -> Option<&str> {
    topic_key
        .map(str::trim)
        .filter(|topic_key| !topic_key.is_empty())
        .and_then(|topic_key| topic_key.rsplit(':').next())
        .filter(|spelling| !spelling.is_empty())
}

/// The whole-identifier form of one prose term, or `None` when the term is a path spelling.
///
/// Path spellings reach retrieval through the Evidence text that carried them; folding a whole
/// directory path into one token would only add an unsearchable string.
fn compound_hint_token(term: &str) -> Option<String> {
    if term.contains('/') || term.contains('.') {
        return None;
    }
    let folded = search_tokens(term).concat();
    (!folded.is_empty()).then_some(folded)
}

/// What one revision's or Candidate's free prose contributes to retrieval.
struct ProseHints {
    /// Every identifier and path spelling, for `hint_text`.
    terms: Vec<String>,
    /// Only the file-backed spellings, for `token_alias` groups.
    alias_seeds: Vec<String>,
}

/// Identifier and path terms read out of the free prose of one revision or Candidate draft.
///
/// Evidence content is read as its string leaves only: an object key like `source_kind` is schema,
/// not a repository coordinate, and must never become a retrieval hint.
///
/// Alias groups are seeded from the path spellings alone. A whole-word alias group is a strong
/// claim — it says any part of the name may stand in for the whole — and an incidental CamelCase
/// word in a sentence does not earn it: turning `compileDebugKotlin` into a `debug` alias hub made
/// the question "did the debug app build?" retrieve whichever Context merely mentioned a Gradle
/// task. A file name is a coordinate a reviewer can open, so it does earn it.
fn prose_hints<'a>(
    statement: &str,
    rationale: &str,
    evidence: impl IntoIterator<Item = (&'a str, &'a serde_json::Value, &'a str)>,
) -> ProseHints {
    let mut texts = vec![statement.to_owned(), rationale.to_owned()];
    for (supports, content, interpretation) in evidence {
        texts.push(supports.to_owned());
        texts.push(interpretation.to_owned());
        hints::json_string_leaves(content, &mut texts);
    }
    ProseHints {
        terms: hints::derived_hint_terms(texts.iter().map(String::as_str)),
        alias_seeds: hints::derived_path_stems(texts.iter().map(String::as_str)),
    }
}

/// Emits every ordered alias pair of one identifier or domain term as a single alias group.
///
/// An ASCII run is a code identifier, so its members are the whole spelling plus every word it
/// splits into. A run that carries non-ASCII text has no whole-word spelling to add: the
/// tokenizer indexes contiguous Han as overlapping bigrams, and concatenating those back
/// (`评论` + `论输` + …) would invent a string no document contains. Its members are therefore
/// exactly the tokens the run indexes as, which makes one written-down term — a Space Intent
/// domain term, a `topic_key` spelling — a group whose bigrams stand in for one another.
fn collect_alias_rows(
    source: &str,
    origin: &str,
    rows: &mut BTreeSet<(String, String, String, String)>,
) {
    for run in source.split(|character: char| !character.is_alphanumeric()) {
        if run.is_empty() {
            continue;
        }
        let parts = search_tokens(run);
        if parts.len() < 2 {
            continue;
        }
        let group_key = parts.join("-");
        let mut members = if run.is_ascii() {
            let mut members = vec![parts.concat()];
            members.extend(parts);
            members
        } else {
            parts
        };
        members.sort();
        members.dedup();
        for token in &members {
            for alias in &members {
                if token == alias {
                    continue;
                }
                rows.insert((
                    token.clone(),
                    alias.clone(),
                    origin.to_owned(),
                    group_key.clone(),
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_conflict(
    transaction: &Transaction<'_>,
    prefix: &str,
    key: &str,
    kind: &str,
    space_id: &str,
    context_id: Option<&str>,
    status: &str,
    details_json: &str,
) -> crate::Result<()> {
    transaction
        .execute(
            &format!(
                "INSERT INTO {prefix}conflict(conflict_key, kind, space_id, context_id, status, details_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            params![key, kind, space_id, context_id, status, details_json],
        )
        .map_err(sql_error("write Conflict overview"))?;
    Ok(())
}

fn insert_diagnostic(
    transaction: &Transaction<'_>,
    prefix: &str,
    diagnostic: &ProjectionDiagnostic,
) -> crate::Result<()> {
    transaction
        .execute(
            &format!(
                "INSERT INTO {prefix}diagnostic(diagnostic_key, source_path, code, entity_id, event_ids_json, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            params![
                diagnostic.key,
                diagnostic.source_path,
                diagnostic.code,
                diagnostic.entity_id,
                diagnostic.event_ids_json,
                diagnostic.message
            ],
        )
        .map_err(sql_error("write Diagnostic projection"))?;
    Ok(())
}

fn governance_columns(
    governance: &ContextGovernanceStatus,
) -> (&'static str, Option<String>, Option<String>) {
    match governance {
        ContextGovernanceStatus::Unpublished => ("unpublished", None, None),
        ContextGovernanceStatus::Accepted {
            publication_id,
            revision_id,
        } => (
            "accepted",
            Some(publication_id.to_string()),
            Some(revision_id.to_string()),
        ),
        ContextGovernanceStatus::Deprecated {
            publication_id,
            revision_id,
        } => (
            "deprecated",
            Some(publication_id.to_string()),
            Some(revision_id.to_string()),
        ),
        ContextGovernanceStatus::GovernanceConflict { .. } => ("governance_conflict", None, None),
    }
}

fn lifecycle_text(value: RevisionLifecycle) -> &'static str {
    match value {
        RevisionLifecycle::Candidate => "candidate",
        RevisionLifecycle::Accepted => "accepted",
        RevisionLifecycle::Deprecated => "deprecated",
        RevisionLifecycle::Superseded => "superseded",
    }
}

fn publication_text(value: PublicationAction) -> &'static str {
    match value {
        PublicationAction::Publish => "publish",
        PublicationAction::Withdraw => "withdraw",
    }
}

fn review_text(value: ReviewVerdict) -> &'static str {
    match value {
        ReviewVerdict::Approve => "approve",
        ReviewVerdict::Reject => "reject",
    }
}

fn evidence_text(value: EvidenceType) -> &'static str {
    match value {
        EvidenceType::SourceSnapshot => "source_snapshot",
        EvidenceType::ExperimentRecord => "experiment_record",
        EvidenceType::ArtifactSnapshot => "artifact_snapshot",
    }
}

fn evidence_completeness(evidence: &[sctx_domain::EvidenceSnapshot]) -> i64 {
    if evidence.is_empty() {
        return 0;
    }
    let total = evidence
        .iter()
        .map(|item| {
            i64::from(!item.supports.trim().is_empty()) * 250
                + i64::from(
                    item.content
                        .as_object()
                        .is_some_and(|value| !value.is_empty()),
                ) * 250
                + i64::from(!item.interpretation.trim().is_empty()) * 250
                + i64::from(!item.limitations.is_empty()) * 250
        })
        .sum::<i64>();
    total / i64::try_from(evidence.len()).expect("Evidence count fits i64")
}

fn semantic_status(value: &SemanticConflictStatus) -> &'static str {
    match value {
        SemanticConflictStatus::Open { .. } => "open",
        SemanticConflictStatus::Resolved { .. } => "resolved",
    }
}

fn enum_text(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("serializing domain enum cannot fail")
        .as_str()
        .expect("domain enum serializes as text")
        .to_owned()
}

fn json(value: &impl serde::Serialize) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|error| {
        crate::Error::new(
            crate::ErrorKind::InvalidInput,
            format!("serialize projection JSON: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::collect_alias_rows;

    fn group(source: &str, origin: &str) -> Vec<(String, String, String, String)> {
        let mut rows = BTreeSet::new();
        collect_alias_rows(source, origin, &mut rows);
        rows.into_iter().collect()
    }

    #[test]
    fn an_ascii_identifier_group_keeps_its_whole_spelling() {
        let rows = group("PoiEntranceAssem", "identifier_split");
        let members = rows
            .iter()
            .map(|(token, _, _, _)| token.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            members,
            BTreeSet::from(["assem", "entrance", "poi", "poientranceassem"])
        );
        assert!(
            rows.iter()
                .all(|(_, _, _, key)| key == "poi-entrance-assem")
        );
    }

    #[test]
    fn a_han_term_groups_the_bigrams_it_indexes_as() {
        let rows = group("评论输入栏", "domain_term");
        let members = rows
            .iter()
            .map(|(token, _, _, _)| token.as_str())
            .collect::<BTreeSet<_>>();
        // Contiguous Han indexes as overlapping bigrams, so those are exactly the members. No
        // concatenated whole-term spelling is invented, because no document holds one.
        assert_eq!(members, BTreeSet::from(["评论", "论输", "输入", "入栏"]));
        assert!(
            rows.iter()
                .all(|(_, _, source, key)| source == "domain_term" && key == "评论-论输-输入-入栏")
        );
        assert_eq!(rows.len(), 12);
    }

    #[test]
    fn a_han_run_shorter_than_one_bigram_pair_seeds_no_group() {
        assert!(group("栏", "domain_term").is_empty());
        assert!(group("输栏", "domain_term").is_empty());
    }
}
