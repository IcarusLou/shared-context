use rusqlite::{OptionalExtension, Transaction, params};
use sctx_domain::{
    ContextGovernanceStatus, PublicationAction, RevisionLifecycle, SemanticConflictStatus,
};
use sctx_event_schema::{EvidenceType, ReviewVerdict};

use crate::{
    IMPLEMENTATION_VERSIONS,
    project::{BuildInput, ProjectionDiagnostic},
    sql_error,
};

pub(crate) const NEXT_PREFIX: &str = "_next_";

const TABLES: [&str; 17] = [
    "meta",
    "source_file",
    "space_projection",
    "intent_revision",
    "intent_head",
    "context_item",
    "context_revision",
    "review",
    "publication",
    "publication_head",
    "evidence",
    "scope",
    "semantic_conflict",
    "conflict_resolution",
    "conflict",
    "diagnostic",
    "context_fts",
];

pub(crate) fn is_complete(connection: &rusqlite::Connection) -> crate::Result<bool> {
    for table in TABLES {
        let exists = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(sql_error("inspect core projection schema"))?;
        if !exists {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn replace_projection(
    transaction: &Transaction<'_>,
    input: &BuildInput,
    tree_oid: &str,
    generation: u64,
) -> crate::Result<()> {
    drop_tables(transaction, NEXT_PREFIX)?;
    create_tables(transaction, NEXT_PREFIX)?;
    populate(transaction, NEXT_PREFIX, input, tree_oid, generation)?;
    drop_tables(transaction, "")?;
    for table in TABLES {
        transaction
            .execute_batch(&format!(
                "ALTER TABLE {NEXT_PREFIX}{table} RENAME TO {table};"
            ))
            .map_err(sql_error("activate shadow projection table"))?;
    }
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
    diagnostic_message TEXT
) WITHOUT ROWID;
CREATE TABLE {prefix}space_projection (
    space_id TEXT PRIMARY KEY,
    current_intent_revision_id TEXT,
    title TEXT,
    problem TEXT,
    desired_outcome TEXT,
    intent_conflicted INTEGER NOT NULL CHECK (intent_conflicted IN (0, 1)),
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
    projection_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE {prefix}context_revision (
    revision_id TEXT PRIMARY KEY,
    context_id TEXT NOT NULL REFERENCES {prefix}context_item(context_id),
    space_id TEXT NOT NULL REFERENCES {prefix}space_projection(space_id),
    parent_revision_ids_json TEXT NOT NULL,
    kind TEXT NOT NULL,
    topic_key TEXT,
    statement TEXT NOT NULL,
    rationale TEXT NOT NULL,
    applicability_json TEXT NOT NULL,
    assumptions_json TEXT NOT NULL,
    recheck_when_json TEXT NOT NULL,
    review_summary TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    is_head INTEGER NOT NULL CHECK (is_head IN (0, 1))
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
    evidence
);
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

#[allow(clippy::too_many_lines)]
fn populate(
    transaction: &Transaction<'_>,
    prefix: &str,
    input: &BuildInput,
    tree_oid: &str,
    generation: u64,
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
    for (key, value) in IMPLEMENTATION_VERSIONS {
        transaction
            .execute(&insert_meta, params![key, value])
            .map_err(sql_error("write implementation version metadata"))?;
    }

    let source_sql = format!(
        "INSERT INTO {prefix}source_file(path, blob_oid, parse_status, event_id, diagnostic_code, diagnostic_message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
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
                    source.diagnostic_message
                ],
            )
            .map_err(sql_error("write source-file projection"))?;
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
                    "INSERT INTO {prefix}space_projection(space_id, current_intent_revision_id, title, problem, desired_outcome, intent_conflicted, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
                params![
                    space_id.to_string(),
                    current_intent.map(|id| id.to_string()),
                    intent.map(|revision| revision.intent.title.as_str()),
                    intent.map(|revision| revision.intent.problem.as_str()),
                    intent.map(|revision| revision.intent.desired_outcome.as_str()),
                    i64::from(space.intent.heads.len() > 1),
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

        let fts_title = if let Some(intent) = intent {
            intent.intent.title.clone()
        } else {
            space
                .intent
                .heads
                .iter()
                .filter_map(|id| space.intent.revisions.get(id))
                .map(|revision| revision.intent.title.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        for (context_id, context) in &space.contexts {
            let (governance_status, accepted_publication, accepted_revision) =
                governance_columns(&context.governance);
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {prefix}context_item(context_id, space_id, governance_status, accepted_revision_id, accepted_publication_id, auto_injection_eligible, projection_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                    ),
                    params![
                        context_id.to_string(),
                        space_id.to_string(),
                        governance_status,
                        accepted_revision,
                        accepted_publication,
                        i64::from(context.auto_injection.eligible),
                        json(context)?
                    ],
                )
                .map_err(sql_error("write Context item"))?;

            for (revision_id, revision_projection) in &context.revisions {
                let revision = &revision_projection.revision;
                transaction
                    .execute(
                        &format!(
                            "INSERT INTO {prefix}context_revision(revision_id, context_id, space_id, parent_revision_ids_json, kind, topic_key, statement, rationale, applicability_json, assumptions_json, recheck_when_json, review_summary, lifecycle, is_head) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
                        ),
                        params![
                            revision_id.to_string(),
                            context_id.to_string(),
                            space_id.to_string(),
                            json(&revision.parent_revision_ids)?,
                            enum_text(revision.kind),
                            revision.topic_key,
                            revision.statement,
                            revision.rationale,
                            json(&revision.applicability)?,
                            json(&revision.assumptions)?,
                            json(&revision.recheck_when)?,
                            enum_text(revision_projection.review_summary),
                            lifecycle_text(revision_projection.lifecycle),
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
                            "INSERT INTO {prefix}context_fts(context_id, revision_id, title, statement, rationale, evidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                        ),
                        params![
                            context_id.to_string(),
                            revision_id.to_string(),
                            fts_title,
                            revision.statement,
                            revision.rationale,
                            evidence_search.join(" ")
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

    for diagnostic in &input.diagnostics {
        insert_diagnostic(transaction, prefix, diagnostic)?;
    }
    Ok(())
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
