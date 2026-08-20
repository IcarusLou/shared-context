use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier, Mutex},
    thread,
};

use rusqlite::{Connection, types::ValueRef};
use sctx_event_schema::{
    Applicability, ConflictParticipant, ContextId, ContextKind, ContextRevisionDraft, Event,
    EventPayload, EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, PublicationAction,
    PublicationDraft, PublicationId, ReviewDraft, ReviewVerdict, RevisionId, SemanticConflictDraft,
    SpaceId, WorkEpisodeId,
};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::{
    DB_SCHEMA_VERSION, IncrementalFallback, IndexUpdateKind, ProjectionIndex, REDUCER_VERSION,
    RebuildReason, normalize_search_text,
};
use tempfile::TempDir;

struct Fixture {
    temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    committed_event_path: PathBuf,
}

fn intent(title: &str) -> IntentSnapshot {
    IntentSnapshot {
        title: title.to_owned(),
        problem: "projection must not become a second source of truth".to_owned(),
        desired_outcome: "deterministic rebuild from HEAD blobs".to_owned(),
        in_scope: vec!["SQLite projection".to_owned()],
        out_of_scope: vec!["tokenizer and ranking".to_owned()],
        acceptance_conditions: vec!["scratch rebuild is stable".to_owned()],
        domain_terms: vec!["generation".to_owned()],
    }
}

fn context(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Decision,
        topic_key: Some("projection/source-of-truth".to_owned()),
        statement: statement.to_owned(),
        rationale: "Git Tree identity is authoritative".to_owned(),
        applicability: Applicability {
            domains: vec!["index".to_owned()],
            platforms: vec!["macos".to_owned()],
            conditions: vec!["offline".to_owned()],
        },
        assumptions: vec!["HEAD resolves to a tree".to_owned()],
        recheck_when: vec!["event schema changes".to_owned()],
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the same Tree produces the same projection".to_owned(),
            content: serde_json::json!({
                "command": "cargo test -p sctx-index",
                "actual": "projection matched"
            }),
            interpretation: "working-tree state was not observed".to_owned(),
            limitations: vec!["ranking is outside this issue".to_owned()],
        }],
    }
}

fn candidate(statement: &str) -> Event {
    Event::context_candidate_created(WorkEpisodeId::new(), context(statement), None)
        .expect("valid Candidate event")
}

fn space_ids(event: &Event) -> (SpaceId, RevisionId) {
    match event.payload() {
        EventPayload::SpaceCreated {
            space_id,
            intent_revision,
        } => (*space_id, intent_revision.revision_id),
        _ => panic!("expected space.created"),
    }
}

fn context_ids(event: &Event) -> (ContextId, RevisionId) {
    match event.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => panic!("expected context.revision_added"),
    }
}

fn publication_id(event: &Event) -> PublicationId {
    match event.payload() {
        EventPayload::ContextPublicationChanged { publication, .. } => publication.publication_id,
        _ => panic!("expected context.publication_changed"),
    }
}

fn append(store: &GitStore, event: Event) -> PathBuf {
    let outcome = store
        .append_event(AppendRequest::event(event))
        .expect("append fixture event");
    store.repository().join(outcome.event_path)
}

fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::initialize(temporary.path().join("installation")).unwrap();

    append(
        &store,
        candidate("Unassigned Candidates project outside every Space"),
    );

    let space_event = Event::space_created(intent("SQLite deterministic rebuild"), None).unwrap();
    let (space_id, _) = space_ids(&space_event);
    let committed_event_path = append(&store, space_event);

    let first_context =
        Event::context_revision_added(space_id, context("SQLite is a derived projection"), None)
            .unwrap();
    let (first_context_id, first_revision_id) = context_ids(&first_context);
    append(&store, first_context);
    let review = Event::context_reviewed(
        space_id,
        first_context_id,
        ReviewDraft {
            revision_id: first_revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "fixture review".to_owned(),
        },
        None,
    )
    .unwrap();
    let review_event_id = review.event_id();
    append(&store, review);
    let first_publication = Event::publication_changed(
        space_id,
        first_context_id,
        PublicationDraft {
            previous_publication_ids: Vec::new(),
            action: PublicationAction::Publish,
            revision_id: first_revision_id,
            review_event_ids: vec![review_event_id],
        },
        None,
    )
    .unwrap();
    let first_publication_id = publication_id(&first_publication);
    append(&store, first_publication);

    let second_context = Event::context_revision_added(
        space_id,
        context("Only the current HEAD Tree may be read"),
        None,
    )
    .unwrap();
    let (second_context_id, second_revision_id) = context_ids(&second_context);
    append(&store, second_context);
    let second_publication = Event::publication_changed(
        space_id,
        second_context_id,
        PublicationDraft {
            previous_publication_ids: Vec::new(),
            action: PublicationAction::Publish,
            revision_id: second_revision_id,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let second_publication_id = publication_id(&second_publication);
    append(&store, second_publication);

    let conflict = Event::semantic_conflict_opened(
        space_id,
        SemanticConflictDraft {
            participants: vec![
                ConflictParticipant {
                    context_id: first_context_id,
                    revision_id: first_revision_id,
                    publication_id: first_publication_id,
                },
                ConflictParticipant {
                    context_id: second_context_id,
                    revision_id: second_revision_id,
                    publication_id: second_publication_id,
                },
            ],
            reason: "fixture confirms the overlapping decisions conflict".to_owned(),
            applicability: Applicability {
                domains: vec!["index".to_owned()],
                platforms: vec!["macos".to_owned()],
                conditions: vec!["offline".to_owned()],
            },
        },
        None,
    )
    .unwrap();
    append(&store, conflict);

    commit_diagnostic_fixtures(store.repository());
    let index = ProjectionIndex::for_store(&store);
    Fixture {
        temporary,
        store,
        index,
        committed_event_path,
    }
}

fn commit_diagnostic_fixtures(repository: &Path) {
    let unknown_path = repository.join("events/fe/unknown-schema.json");
    let malformed_path = repository.join("events/ff/malformed-v1.json");
    fs::create_dir_all(unknown_path.parent().unwrap()).unwrap();
    fs::create_dir_all(malformed_path.parent().unwrap()).unwrap();
    fs::write(
        &unknown_path,
        br#"{"schema_version":"999","event_id":"future_event","event_type":"future.event","payload":{}}"#,
    )
    .unwrap();
    fs::write(
        &malformed_path,
        br#"{"schema_version":"1","event_id":"not-a-v4-id","event_type":"space.created"}"#,
    )
    .unwrap();
    git(
        repository,
        [
            "add",
            "--",
            "events/fe/unknown-schema.json",
            "events/ff/malformed-v1.json",
        ],
    );
    git(
        repository,
        ["commit", "-m", "Add index diagnostic fixtures"],
    );
}

fn git<const N: usize>(repository: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn deletion_rebuilds_complete_projection_and_dirty_tree_is_never_read() {
    let fixture = fixture();
    let first = fixture.index.synchronize().unwrap();
    assert_eq!(first.reason, RebuildReason::MissingDatabase);
    assert_eq!(first.metadata.projection_generation, 1);
    assert!(fixture.index.quick_check().unwrap().healthy);
    let expected = projection_dump(fixture.index.database_path());

    let dirty_event = Event::space_created(intent("DIRTY SENTINEL MUST NOT APPEAR"), None).unwrap();
    let dirty_path = fixture.store.repository().join("events/00/dirty.json");
    fs::create_dir_all(dirty_path.parent().unwrap()).unwrap();
    fs::write(&dirty_path, serde_json::to_vec(&dirty_event).unwrap()).unwrap();
    let committed_bytes = fs::read(&fixture.committed_event_path).unwrap();
    fs::write(
        &fixture.committed_event_path,
        b"dirty overwrite outside HEAD",
    )
    .unwrap();

    let forced = fixture.index.rebuild().unwrap();
    assert_eq!(forced.reason, RebuildReason::Forced);
    assert_eq!(forced.metadata.projection_generation, 2);
    assert_eq!(projection_dump(fixture.index.database_path()), expected);
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    let dirty_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM space_projection WHERE title LIKE '%DIRTY SENTINEL%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dirty_count, 0);
    drop(connection);

    fs::write(&fixture.committed_event_path, committed_bytes).unwrap();
    fs::remove_file(dirty_path).unwrap();
    remove_database(fixture.index.database_path());
    let rebuilt = fixture.index.synchronize().unwrap();
    assert_eq!(rebuilt.reason, RebuildReason::MissingDatabase);
    assert_eq!(rebuilt.metadata.projection_generation, 1);
    assert_eq!(projection_dump(fixture.index.database_path()), expected);

    let expected_tree = git(fixture.store.repository(), ["rev-parse", "HEAD^{tree}"]);
    assert_eq!(rebuilt.metadata.indexed_tree_oid, expected_tree);
    assert_eq!(rebuilt.metadata.db_schema_version, DB_SCHEMA_VERSION);
    assert_eq!(rebuilt.metadata.reducer_version, REDUCER_VERSION);

    let connection = Connection::open(fixture.index.database_path()).unwrap();
    assert_eq!(count(&connection, "space_projection"), 1);
    assert_eq!(count(&connection, "context_candidate"), 1);
    assert_eq!(
        count_where(
            &connection,
            "context_candidate",
            "auto_injection_eligible = 0"
        ),
        1
    );
    assert_eq!(count(&connection, "context_item"), 2);
    assert_eq!(
        count_where(
            &connection,
            "context_item",
            "governance_status = 'accepted'"
        ),
        2
    );
    assert_eq!(count(&connection, "review"), 1);
    assert_eq!(count(&connection, "semantic_conflict"), 1);
    assert_eq!(
        count_where(&connection, "semantic_conflict", "status = 'open'"),
        1
    );
    assert_eq!(count(&connection, "evidence"), 2);
    assert_eq!(count(&connection, "context_fts"), 2);
    assert_eq!(count(&connection, "space_fts"), 1);
    assert_eq!(
        count_where(&connection, "diagnostic", "code = 'UNKNOWN_SCHEMA_VERSION'"),
        1
    );
    assert_eq!(
        count_where(&connection, "diagnostic", "code = 'EVENT_PARSE_ERROR'"),
        1
    );
    let foreign_key_violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_violations, 0);
    assert_eq!(count_fts_matches(&connection, "context_fts", "SQLite"), 2);
    assert_eq!(count_fts_matches(&connection, "space_fts", "SQLite"), 1);
    assert_core_tables(&connection);

    let pragmas = fixture.index.pragmas().unwrap();
    assert_eq!(pragmas.journal_mode, "wal");
    assert_eq!(pragmas.synchronous, 1);
    assert!(pragmas.foreign_keys);
    assert_eq!(pragmas.busy_timeout_ms, 3_000);
}

#[test]
fn healthy_shadow_rebuild_preserves_file_and_repairs_unknown_db_version() {
    let fixture = fixture();
    let first = fixture.index.synchronize().unwrap();
    let identity = file_identity(fixture.index.database_path());
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    connection
        .execute(
            "UPDATE meta SET value = 'unknown-v999' WHERE key = 'db_schema_version'",
            [],
        )
        .unwrap();
    drop(connection);

    let rebuilt = fixture.index.synchronize().unwrap();
    assert_eq!(rebuilt.reason, RebuildReason::ImplementationVersionChanged);
    assert_eq!(
        rebuilt.metadata.projection_generation,
        first.metadata.projection_generation + 1
    );
    assert_eq!(rebuilt.metadata.db_schema_version, DB_SCHEMA_VERSION);
    assert_eq!(file_identity(fixture.index.database_path()), identity);

    let connection = Connection::open(fixture.index.database_path()).unwrap();
    connection.execute("DROP TABLE diagnostic", []).unwrap();
    drop(connection);
    let repaired_schema = fixture.index.synchronize().unwrap();
    assert_eq!(
        repaired_schema.reason,
        RebuildReason::ImplementationVersionChanged
    );
    assert_eq!(
        repaired_schema.metadata.projection_generation,
        rebuilt.metadata.projection_generation + 1
    );
    assert_eq!(file_identity(fixture.index.database_path()), identity);

    let forced = fixture.index.rebuild().unwrap();
    assert_eq!(forced.reason, RebuildReason::Forced);
    assert_eq!(file_identity(fixture.index.database_path()), identity);
    assert!(forced.quarantined_database.is_none());
}

#[test]
fn corrupt_database_is_isolated_before_full_tree_rebuild() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let expected = projection_dump(fixture.index.database_path());
    fs::write(fixture.index.database_path(), b"not a SQLite database").unwrap();

    let rebuilt = fixture.index.synchronize().unwrap();
    assert_eq!(rebuilt.reason, RebuildReason::CorruptDatabase);
    let quarantined = rebuilt
        .quarantined_database
        .expect("corrupt database must be isolated");
    assert!(quarantined.exists());
    assert_eq!(fs::read(quarantined).unwrap(), b"not a SQLite database");
    assert!(fixture.index.quick_check().unwrap().healthy);
    assert_eq!(projection_dump(fixture.index.database_path()), expected);
}

#[test]
fn append_uses_incremental_closure_and_matches_scratch_rebuild() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    let space_id: String = connection
        .query_row("SELECT space_id FROM space_projection", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(connection);
    let event = Event::context_revision_added(
        space_id.parse().unwrap(),
        context("incremental append reaches its Space closure"),
        None,
    )
    .unwrap();
    append(&fixture.store, event);

    let incremental = fixture.index.synchronize().unwrap();
    assert_eq!(incremental.reason, RebuildReason::TreeChanged);
    assert_eq!(incremental.update_kind, IndexUpdateKind::Incremental);
    assert!(!incremental.rebuilt);
    assert!(incremental.incremental_fallback.is_none());

    let scratch_state = fixture.temporary.path().join("scratch-state");
    let scratch = ProjectionIndex::new(fixture.store.repository(), scratch_state);
    let rebuilt = scratch.rebuild().unwrap();
    assert_eq!(rebuilt.update_kind, IndexUpdateKind::FullRebuild);
    assert_eq!(
        projection_dump(fixture.index.database_path()),
        projection_dump(scratch.database_path())
    );
}

#[test]
fn intent_append_refreshes_every_space_fts_field_and_matches_scratch_rebuild() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    let (space_id, parent_revision_id): (SpaceId, RevisionId) = connection
        .query_row("SELECT space_id, revision_id FROM intent_head", [], |row| {
            Ok((
                row.get::<_, String>(0)?.parse().unwrap(),
                row.get::<_, String>(1)?.parse().unwrap(),
            ))
        })
        .unwrap();
    drop(connection);
    let revised_intent = IntentSnapshot {
        title: "任务意图检索".to_owned(),
        problem: "orphanedknowledge must be recovered".to_owned(),
        desired_outcome: "multispaceassociation is deterministic".to_owned(),
        in_scope: vec!["SearchResultRenderer".to_owned()],
        out_of_scope: vec!["legacyrouter".to_owned()],
        acceptance_conditions: vec!["SearchV2Endpoint remains stable".to_owned()],
        domain_terms: vec!["RequirementIntent".to_owned()],
    };
    append(
        &fixture.store,
        Event::intent_revision_added(
            space_id,
            vec![parent_revision_id],
            revised_intent.clone(),
            None,
        )
        .unwrap(),
    );

    let incremental = fixture.index.synchronize().unwrap();
    assert_eq!(incremental.reason, RebuildReason::TreeChanged);
    assert_eq!(incremental.update_kind, IndexUpdateKind::Incremental);
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    assert_eq!(count(&connection, "space_fts"), 1);
    let indexed_fields: (String, String, String, String, String, String, String) = connection
        .query_row(
            "SELECT title, problem, desired_outcome, in_scope, out_of_scope,
                    acceptance_conditions, domain_terms FROM space_fts",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        indexed_fields,
        (
            normalize_search_text(&revised_intent.title),
            normalize_search_text(&revised_intent.problem),
            normalize_search_text(&revised_intent.desired_outcome),
            normalize_search_text(&revised_intent.in_scope.join(" ")),
            normalize_search_text(&revised_intent.out_of_scope.join(" ")),
            normalize_search_text(&revised_intent.acceptance_conditions.join(" ")),
            normalize_search_text(&revised_intent.domain_terms.join(" ")),
        )
    );
    for token in [
        "意图",
        "orphanedknowledge",
        "multispaceassociation",
        "searchresultrenderer",
        "legacyrouter",
        "searchv2endpoint",
        "requirementintent",
    ] {
        assert_eq!(
            count_fts_matches(&connection, "space_fts", token),
            1,
            "missing current Intent token {token}"
        );
    }
    assert_eq!(
        count_fts_matches(&connection, "space_fts", "SQLite"),
        0,
        "non-head Intent must leave the FTS"
    );
    drop(connection);

    let scratch = ProjectionIndex::new(
        fixture.store.repository(),
        fixture.temporary.path().join("intent-fts-scratch-state"),
    );
    scratch.rebuild().unwrap();
    assert_eq!(
        projection_dump(fixture.index.database_path()),
        projection_dump(scratch.database_path())
    );
}

#[test]
fn unassigned_candidate_incrementally_projects_and_matches_scratch_rebuild() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    append(
        &fixture.store,
        candidate("A second Candidate still has no Space route"),
    );

    let incremental = fixture.index.synchronize().unwrap();
    assert_eq!(incremental.reason, RebuildReason::TreeChanged);
    assert_eq!(incremental.update_kind, IndexUpdateKind::Incremental);
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    assert_eq!(count(&connection, "context_candidate"), 2);
    assert_eq!(count(&connection, "space_projection"), 1);
    assert_eq!(count(&connection, "context_item"), 2);
    assert_eq!(count(&connection, "context_fts"), 2);
    drop(connection);

    let scratch = ProjectionIndex::new(
        fixture.store.repository(),
        fixture.temporary.path().join("candidate-scratch-state"),
    );
    scratch.rebuild().unwrap();
    assert_eq!(
        projection_dump(fixture.index.database_path()),
        projection_dump(scratch.database_path())
    );
}

#[test]
fn reverse_reference_closure_recovers_dangling_nodes_and_handles_duplicate_append() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let space_id: SpaceId = Connection::open(fixture.index.database_path())
        .unwrap()
        .query_row("SELECT space_id FROM space_projection", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .parse()
        .unwrap();
    let target = Event::context_revision_added(
        space_id,
        context("late target for an old dangling review"),
        None,
    )
    .unwrap();
    let (context_id, revision_id) = context_ids(&target);
    let review = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "arrived before target".to_owned(),
        },
        None,
    )
    .unwrap();
    append(&fixture.store, review);
    let dangling = fixture.index.synchronize().unwrap();
    assert_eq!(dangling.update_kind, IndexUpdateKind::Incremental);
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    assert_eq!(count(&connection, "review"), 1); // the original fixture review only
    assert!(
        count_where(
            &connection,
            "diagnostic",
            "code = 'invalid_review_reference'"
        ) >= 1
    );
    drop(connection);

    let target_path = append(&fixture.store, target);
    let recovered = fixture.index.synchronize().unwrap();
    assert_eq!(recovered.update_kind, IndexUpdateKind::Incremental);
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    assert_eq!(count(&connection, "review"), 2);
    assert_eq!(
        count_where(
            &connection,
            "diagnostic",
            "code = 'invalid_review_reference'"
        ),
        0
    );
    drop(connection);

    let duplicate = fixture.store.repository().join("events/ab/duplicate.json");
    fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    fs::copy(target_path, &duplicate).unwrap();
    git(
        fixture.store.repository(),
        ["add", "--", "events/ab/duplicate.json"],
    );
    git(
        fixture.store.repository(),
        ["commit", "-m", "Append duplicate event fixture"],
    );
    let duplicated = fixture.index.synchronize().unwrap();
    assert_eq!(duplicated.update_kind, IndexUpdateKind::Incremental);
    assert!(
        count_where(
            &Connection::open(fixture.index.database_path()).unwrap(),
            "diagnostic",
            "code = 'duplicate_event_id'"
        ) >= 1
    );

    let scratch = ProjectionIndex::new(
        fixture.store.repository(),
        fixture.temporary.path().join("reverse-closure-scratch"),
    );
    scratch.rebuild().unwrap();
    assert_eq!(
        projection_dump(fixture.index.database_path()),
        projection_dump(scratch.database_path())
    );
}

#[test]
fn manual_modify_delete_and_rename_force_full_equivalent_rebuilds() {
    for operation in ["modify", "delete", "rename"] {
        let fixture = fixture();
        fixture.index.synchronize().unwrap();
        let relative = fixture
            .committed_event_path
            .strip_prefix(fixture.store.repository())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        match operation {
            "modify" => {
                let replacement = Event::space_created(intent("manual replacement"), None).unwrap();
                fs::write(
                    &fixture.committed_event_path,
                    serde_json::to_vec(&replacement).unwrap(),
                )
                .unwrap();
                git(fixture.store.repository(), ["add", "--", &relative]);
            }
            "delete" => {
                git(fixture.store.repository(), ["rm", "--", &relative]);
            }
            "rename" => {
                let destination = "events/aa/manually-renamed.json";
                fs::create_dir_all(fixture.store.repository().join("events/aa")).unwrap();
                git(
                    fixture.store.repository(),
                    ["mv", "--", &relative, destination],
                );
            }
            _ => unreachable!(),
        }
        git(
            fixture.store.repository(),
            ["commit", "-m", "Bypass append protocol for index test"],
        );

        let synchronized = fixture.index.synchronize().unwrap();
        assert_eq!(synchronized.update_kind, IndexUpdateKind::FullRebuild);
        assert_eq!(
            synchronized.incremental_fallback,
            Some(IncrementalFallback::AppendProtocolBypassed)
        );
        assert_eq!(synchronized.operational_warnings.len(), 1);
        assert_eq!(
            synchronized.operational_warnings[0].code,
            "APPEND_PROTOCOL_BYPASSED"
        );

        let scratch_state = fixture
            .temporary
            .path()
            .join(format!("scratch-{operation}"));
        let scratch = ProjectionIndex::new(fixture.store.repository(), scratch_state);
        scratch.rebuild().unwrap();
        assert_eq!(
            projection_dump(fixture.index.database_path()),
            projection_dump(scratch.database_path()),
            "{operation} projection diverged from scratch"
        );
    }
}

#[test]
fn paginated_query_is_pinned_to_one_tree_and_generation() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let connection = Connection::open(fixture.index.database_path()).unwrap();
    let space_id: SpaceId = connection
        .query_row("SELECT space_id FROM space_projection", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .parse()
        .unwrap();
    drop(connection);
    for number in 0..4 {
        append(
            &fixture.store,
            Event::context_revision_added(
                space_id,
                context(&format!("snapshot context {number}")),
                None,
            )
            .unwrap(),
        );
    }
    let before = fixture.index.synchronize().unwrap().metadata;
    let expected = context_ids_from_database(fixture.index.database_path());
    let appended_during_query = Event::context_revision_added(
        space_id,
        context("must appear only after this snapshot"),
        None,
    )
    .unwrap();
    let store = fixture.store.clone();
    let index = fixture.index.clone();

    let snapshot = fixture
        .index
        .query_snapshot(move |connection| {
            let mut combined = query_context_page(connection, 0, 2);
            let worker = thread::spawn(move || {
                append(&store, appended_during_query);
                index.synchronize().unwrap();
            });
            worker.join().unwrap();
            combined.extend(query_context_page(connection, 2, 100));
            Ok(combined)
        })
        .unwrap();

    assert_eq!(snapshot.metadata, before);
    assert_eq!(snapshot.data, expected);
    let after = fixture.index.synchronize().unwrap().metadata;
    assert!(after.projection_generation > snapshot.metadata.projection_generation);
    assert_ne!(after.indexed_tree_oid, snapshot.metadata.indexed_tree_oid);
    assert_eq!(
        context_ids_from_database(fixture.index.database_path()).len(),
        expected.len() + 1
    );
}

#[test]
fn long_lived_query_connection_reopens_after_corrupt_file_replacement() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let mut reader = fixture.index.query_connection();
    let first = reader
        .snapshot(|connection| Ok(count(connection, "context_item")))
        .unwrap();
    let space_id: SpaceId = Connection::open(fixture.index.database_path())
        .unwrap()
        .query_row("SELECT space_id FROM space_projection", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .parse()
        .unwrap();
    append(
        &fixture.store,
        Event::context_revision_added(
            space_id,
            context("visible only in replacement database"),
            None,
        )
        .unwrap(),
    );
    fs::write(fixture.index.database_path(), b"intentionally corrupt").unwrap();

    let second = reader
        .snapshot(|connection| Ok(count(connection, "context_item")))
        .unwrap();
    assert_eq!(second.data, first.data + 1);
    assert_ne!(
        second.metadata.indexed_tree_oid,
        first.metadata.indexed_tree_oid
    );
    assert!(fixture.index.quick_check().unwrap().healthy);
    let isolated = fs::read_dir(fixture.index.database_path().parent().unwrap())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.sqlite.corrupt-")
        });
    assert!(isolated);
}

#[test]
fn concurrent_query_index_rebuild_and_append_converge_without_generation_regression() {
    let fixture = fixture();
    fixture.index.synchronize().unwrap();
    let space_id: SpaceId = Connection::open(fixture.index.database_path())
        .unwrap()
        .query_row("SELECT space_id FROM space_projection", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .parse()
        .unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let observed_generations = Arc::new(Mutex::new(Vec::new()));

    let append_thread = {
        let barrier = Arc::clone(&barrier);
        let store = fixture.store.clone();
        thread::spawn(move || {
            barrier.wait();
            for number in 0..12 {
                append(
                    &store,
                    Event::context_revision_added(
                        space_id,
                        context(&format!("concurrent append {number}")),
                        None,
                    )
                    .unwrap(),
                );
            }
        })
    };
    let index_thread = {
        let barrier = Arc::clone(&barrier);
        let index = fixture.index.clone();
        thread::spawn(move || {
            barrier.wait();
            for _ in 0..20 {
                index.synchronize().unwrap();
            }
        })
    };
    let rebuild_thread = {
        let barrier = Arc::clone(&barrier);
        let index = fixture.index.clone();
        thread::spawn(move || {
            barrier.wait();
            for _ in 0..6 {
                index.rebuild().unwrap();
            }
        })
    };
    let query_thread = {
        let barrier = Arc::clone(&barrier);
        let index = fixture.index.clone();
        let generations = Arc::clone(&observed_generations);
        thread::spawn(move || {
            barrier.wait();
            let mut reader = index.query_connection();
            for _ in 0..30 {
                let snapshot = reader
                    .snapshot(|connection| Ok(count(connection, "context_item")))
                    .unwrap();
                generations
                    .lock()
                    .unwrap()
                    .push(snapshot.metadata.projection_generation);
            }
        })
    };
    for worker in [append_thread, index_thread, rebuild_thread, query_thread] {
        worker.join().unwrap();
    }

    let final_outcome = fixture.index.synchronize().unwrap();
    let head_tree = git(fixture.store.repository(), ["rev-parse", "HEAD^{tree}"]);
    assert_eq!(final_outcome.metadata.indexed_tree_oid, head_tree);
    let generations = observed_generations.lock().unwrap();
    assert!(
        generations.windows(2).all(|pair| pair[0] <= pair[1]),
        "query generations regressed: {generations:?}"
    );

    let scratch = ProjectionIndex::new(
        fixture.store.repository(),
        fixture.temporary.path().join("concurrent-scratch"),
    );
    scratch.rebuild().unwrap();
    assert_eq!(
        projection_dump(fixture.index.database_path()),
        projection_dump(scratch.database_path())
    );
}

fn query_context_page(connection: &Connection, offset: i64, limit: i64) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT context_id FROM context_item ORDER BY context_id LIMIT ?1 OFFSET ?2")
        .unwrap();
    statement
        .query_map([limit, offset], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn context_ids_from_database(database: &Path) -> Vec<String> {
    query_context_page(&Connection::open(database).unwrap(), 0, 10_000)
}

fn assert_core_tables(connection: &Connection) {
    let expected = [
        "meta",
        "source_file",
        "context_candidate",
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
        "space_fts",
    ];
    for table in expected {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = ?1 AND type IN ('table', 'view'))",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "missing core table {table}");
    }
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn count_where(connection: &Connection, table: &str, condition: &str) -> i64 {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {condition}"),
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn count_fts_matches(connection: &Connection, table: &str, query: &str) -> i64 {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {table} MATCH ?1"),
            [query],
            |row| row.get(0),
        )
        .unwrap()
}

fn projection_dump(database: &Path) -> Vec<String> {
    let connection = Connection::open(database).unwrap();
    [
        "SELECT key, value FROM meta WHERE key <> 'projection_generation' ORDER BY key",
        "SELECT * FROM source_file ORDER BY path",
        "SELECT * FROM context_candidate ORDER BY candidate_id",
        "SELECT * FROM space_projection ORDER BY space_id",
        "SELECT * FROM intent_revision ORDER BY revision_id",
        "SELECT * FROM intent_head ORDER BY space_id, revision_id",
        "SELECT * FROM context_item ORDER BY context_id",
        "SELECT * FROM context_revision ORDER BY revision_id",
        "SELECT * FROM review ORDER BY event_id",
        "SELECT * FROM publication ORDER BY publication_id",
        "SELECT * FROM publication_head ORDER BY context_id, publication_id",
        "SELECT * FROM evidence ORDER BY evidence_id",
        "SELECT * FROM scope ORDER BY revision_id, dimension, value",
        "SELECT * FROM semantic_conflict ORDER BY conflict_id",
        "SELECT * FROM conflict_resolution ORDER BY resolution_id",
        "SELECT * FROM conflict ORDER BY conflict_key",
        "SELECT * FROM diagnostic ORDER BY diagnostic_key",
        "SELECT context_id, revision_id, title, statement, rationale, evidence FROM context_fts ORDER BY context_id, revision_id",
        "SELECT space_id, revision_id, title, problem, desired_outcome, in_scope, out_of_scope, acceptance_conditions, domain_terms FROM space_fts ORDER BY space_id, revision_id",
    ]
    .into_iter()
    .map(|query| dump_query(&connection, query))
    .collect()
}

fn dump_query(connection: &Connection, query: &str) -> String {
    let mut statement = connection.prepare(query).unwrap();
    let columns = statement.column_count();
    let mut rows = statement.query([]).unwrap();
    let mut output = String::new();
    while let Some(row) = rows.next().unwrap() {
        for column in 0..columns {
            match row.get_ref(column).unwrap() {
                ValueRef::Null => output.push_str("N;"),
                ValueRef::Integer(value) => write!(output, "I{value};").unwrap(),
                ValueRef::Real(value) => write!(output, "R{value};").unwrap(),
                ValueRef::Text(value) => {
                    write!(output, "T{}:", value.len()).unwrap();
                    output.push_str(std::str::from_utf8(value).unwrap());
                    output.push(';');
                }
                ValueRef::Blob(value) => write!(output, "B{};", value.len()).unwrap(),
            }
        }
        output.push('\n');
    }
    output
}

fn remove_database(database: &Path) {
    for path in [
        database.to_path_buf(),
        PathBuf::from(format!("{}-wal", database.display())),
        PathBuf::from(format!("{}-shm", database.display())),
    ] {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
}

#[cfg(unix)]
fn file_identity(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().ino()
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}
