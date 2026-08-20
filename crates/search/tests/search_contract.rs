use sctx_domain::{
    Applicability, ConflictParticipant, ContextId, ContextKind, ContextRevisionDraft,
    EvidenceSnapshotDraft, EvidenceType, PublicationAction, PublicationDraft, RevisionId,
    SemanticConflictDraft, SpaceId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    ContextPackDetail, ContextPackMode, ContextPackRequest, ContextStatus, ScopeFilter,
    SearchEngine, SearchFilters, SearchRequest,
};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    safe_context_id: ContextId,
    safe_revision_id: RevisionId,
    conflict_context_ids: [ContextId; 2],
    excluded_context_ids: [ContextId; 3],
}

fn intent(title: &str) -> sctx_domain::IntentSnapshot {
    sctx_domain::IntentSnapshot {
        title: title.to_owned(),
        problem: "Search must preserve deterministic shared context".to_owned(),
        desired_outcome: "Stable ranked retrieval".to_owned(),
        in_scope: vec!["search".to_owned()],
        out_of_scope: vec!["transport".to_owned()],
        acceptance_conditions: vec!["results are deterministic".to_owned()],
        domain_terms: vec!["Context Pack".to_owned()],
    }
}

fn revision(statement: &str, rationale: &str, evidence: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Decision,
        topic_key: Some("search/retrieval-policy".to_owned()),
        statement: statement.to_owned(),
        rationale: rationale.to_owned(),
        applicability: Applicability {
            domains: vec!["search".to_owned()],
            platforms: vec!["macos".to_owned()],
            conditions: vec!["offline".to_owned()],
        },
        assumptions: vec!["the current Tree is readable".to_owned()],
        recheck_when: vec!["the tokenizer version changes".to_owned()],
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: evidence.to_owned(),
            content: serde_json::json!({
                "command": "cargo test -p sctx-search",
                "actual": "passed"
            }),
            interpretation: "the fixture is self-contained".to_owned(),
            limitations: vec!["synthetic fixture".to_owned()],
        }],
    }
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append fixture event");
}

fn space_id(event: &Event) -> SpaceId {
    match event.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
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

fn publication_id(event: &Event) -> sctx_domain::PublicationId {
    match event.payload() {
        EventPayload::ContextPublicationChanged { publication, .. } => publication.publication_id,
        _ => panic!("expected context.publication_changed"),
    }
}

fn add_context(
    store: &GitStore,
    space_id: SpaceId,
    draft: ContextRevisionDraft,
) -> (ContextId, RevisionId) {
    let event = Event::context_revision_added(space_id, draft, None).unwrap();
    let ids = context_ids(&event);
    append(store, event);
    ids
}

fn publish(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
    previous_publication_ids: Vec<sctx_domain::PublicationId>,
    action: PublicationAction,
) -> sctx_domain::PublicationId {
    let event = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids,
            action,
            revision_id,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let publication_id = publication_id(&event);
    append(store, event);
    publication_id
}

#[allow(clippy::too_many_lines)]
fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::initialize(temporary.path().join("installation")).unwrap();
    let created = Event::space_created(intent("Search Context"), None).unwrap();
    let space_id = space_id(&created);
    append(&store, created);

    let (safe_context_id, safe_revision_id) = add_context(
        &store,
        space_id,
        revision(
            "GeneralTabVisibility uses search_result_parser for 中文检索",
            "Unicode normalization keeps Straße and STRASSE equivalent",
            "the parser returned the expected result",
        ),
    );
    publish(
        &store,
        space_id,
        safe_context_id,
        safe_revision_id,
        Vec::new(),
        PublicationAction::Publish,
    );

    let (conflict_a, revision_a) = add_context(
        &store,
        space_id,
        revision(
            "semantic policy requires cache enabled",
            "first accepted side",
            "cache experiment A",
        ),
    );
    let publication_a = publish(
        &store,
        space_id,
        conflict_a,
        revision_a,
        Vec::new(),
        PublicationAction::Publish,
    );
    let (conflict_b, revision_b) = add_context(
        &store,
        space_id,
        revision(
            "semantic policy requires cache disabled",
            "second accepted side",
            "cache experiment B",
        ),
    );
    let publication_b = publish(
        &store,
        space_id,
        conflict_b,
        revision_b,
        Vec::new(),
        PublicationAction::Publish,
    );
    append(
        &store,
        Event::semantic_conflict_opened(
            space_id,
            SemanticConflictDraft {
                participants: vec![
                    ConflictParticipant {
                        context_id: conflict_a,
                        revision_id: revision_a,
                        publication_id: publication_a,
                    },
                    ConflictParticipant {
                        context_id: conflict_b,
                        revision_id: revision_b,
                        publication_id: publication_b,
                    },
                ],
                reason: "the accepted cache policies contradict each other".to_owned(),
                applicability: Applicability {
                    domains: vec!["search".to_owned()],
                    platforms: vec!["macos".to_owned()],
                    conditions: vec!["offline".to_owned()],
                },
            },
            None,
        )
        .unwrap(),
    );

    let (candidate, _) = add_context(
        &store,
        space_id,
        revision(
            "injection policy candidate",
            "not published",
            "candidate evidence",
        ),
    );
    let (deprecated, deprecated_revision) = add_context(
        &store,
        space_id,
        revision(
            "injection policy deprecated",
            "withdrawn policy",
            "deprecated evidence",
        ),
    );
    let deprecated_publication = publish(
        &store,
        space_id,
        deprecated,
        deprecated_revision,
        Vec::new(),
        PublicationAction::Publish,
    );
    publish(
        &store,
        space_id,
        deprecated,
        deprecated_revision,
        vec![deprecated_publication],
        PublicationAction::Withdraw,
    );
    let (governance, governance_revision) = add_context(
        &store,
        space_id,
        revision(
            "injection policy governance",
            "concurrent publication heads",
            "governance evidence",
        ),
    );
    publish(
        &store,
        space_id,
        governance,
        governance_revision,
        Vec::new(),
        PublicationAction::Publish,
    );
    publish(
        &store,
        space_id,
        governance,
        governance_revision,
        Vec::new(),
        PublicationAction::Publish,
    );

    let (statement_rank, statement_revision) = add_context(
        &store,
        space_id,
        revision(
            "weightedneedle appears in the statement",
            "field weight fixture",
            "ordinary evidence",
        ),
    );
    publish(
        &store,
        space_id,
        statement_rank,
        statement_revision,
        Vec::new(),
        PublicationAction::Publish,
    );
    let (evidence_rank, evidence_revision) = add_context(
        &store,
        space_id,
        revision(
            "field weight comparison",
            "the token is absent from rationale",
            "weightedneedle appears only in evidence",
        ),
    );
    publish(
        &store,
        space_id,
        evidence_rank,
        evidence_revision,
        Vec::new(),
        PublicationAction::Publish,
    );

    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    Fixture {
        _temporary: temporary,
        index,
        safe_context_id,
        safe_revision_id,
        conflict_context_ids: [conflict_a, conflict_b],
        excluded_context_ids: [candidate, deprecated, governance],
    }
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        page_size: 50,
        ..SearchRequest::default()
    }
}

#[test]
fn chinese_english_unicode_and_code_identifiers_hit_normalized_fts() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);
    for query in [
        "中文检索",
        "general_tab_visibility",
        "searchResultParser",
        "STRASSE",
    ] {
        let response = engine.search(&request(query)).unwrap();
        assert_eq!(response.results.len(), 1, "query {query}");
        assert_eq!(response.results[0].context_id, fixture.safe_context_id);
        assert_eq!(response.results[0].revision_id, fixture.safe_revision_id);
        assert!(!response.results[0].match_reason.matched_fields.is_empty());
        assert!(!response.indexed_tree_oid.is_empty());
        assert!(response.projection_generation > 0);
    }
}

#[test]
fn structured_filters_and_bm25_field_weights_are_applied_before_stable_ids() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);
    let mut weighted = request("weightedneedle");
    weighted.filters = SearchFilters {
        scope: ScopeFilter {
            domains: vec!["search".to_owned()],
            platforms: vec!["macos".to_owned()],
            conditions: vec!["offline".to_owned()],
        },
        kinds: vec![ContextKind::Decision],
        statuses: vec![ContextStatus::Accepted],
        ..SearchFilters::default()
    };
    let response = engine.search(&weighted).unwrap();
    assert_eq!(response.results.len(), 2);
    assert!(response.results[0].statement.contains("weightedneedle"));
    assert!(
        response.results[0].match_reason.bm25 < response.results[1].match_reason.bm25,
        "statement weight must outrank evidence weight"
    );
}

#[test]
fn cursor_pages_and_rebuild_ranking_are_stable() {
    let fixture = fixture();
    let index = fixture.index.clone();
    let engine = SearchEngine::new(fixture.index);
    let mut page_request = request("policy");
    page_request.page_size = 1;
    let pinned_pages = engine.search_pages(&page_request, 10).unwrap();
    assert_eq!(pinned_pages.pages.len(), 5);
    assert!(pinned_pages.next_cursor.is_none());
    assert!(pinned_pages.projection_generation > 0);
    let mut first_order = Vec::new();
    loop {
        let page = engine.search(&page_request).unwrap();
        assert!(page.results.len() <= 1);
        first_order.extend(
            page.results
                .iter()
                .map(|result| (result.context_id, result.revision_id)),
        );
        let Some(cursor) = page.next_cursor else {
            break;
        };
        page_request.cursor = Some(cursor);
    }
    assert_eq!(first_order.len(), 5);
    index.rebuild().unwrap();
    let rebuilt = SearchEngine::new(index).search(&request("policy")).unwrap();
    let rebuilt_order = rebuilt
        .results
        .iter()
        .map(|result| (result.context_id, result.revision_id))
        .collect::<Vec<_>>();
    assert_eq!(first_order, rebuilt_order);
}

#[test]
fn conflicts_show_both_sides_and_automatic_pack_excludes_all_unsafe_states() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);
    let explicit = engine.search(&request("semantic policy")).unwrap();
    assert_eq!(explicit.results.len(), 2);
    for result in &explicit.results {
        assert_eq!(result.conflicts.len(), 1);
        let sides = result.conflicts[0]
            .participants
            .iter()
            .map(|side| side.context_id)
            .collect::<Vec<_>>();
        assert!(
            fixture
                .conflict_context_ids
                .iter()
                .all(|id| sides.contains(id))
        );
        assert!(!result.auto_injection_eligible);
    }

    let pack = engine
        .context_pack(&ContextPackRequest {
            search: request(""),
            token_budget: 100_000,
            candidate_limit: 100,
            mode: ContextPackMode::AutomaticInjection,
        })
        .unwrap();
    assert!(pack.items.iter().all(|item| {
        item.status == ContextStatus::Accepted
            && item.auto_injection_eligible
            && item.conflicts.is_empty()
    }));
    assert!(
        fixture
            .excluded_context_ids
            .iter()
            .all(|excluded| { pack.items.iter().all(|item| item.context_id != *excluded) })
    );
    assert!(
        fixture
            .conflict_context_ids
            .iter()
            .all(|excluded| { pack.items.iter().all(|item| item.context_id != *excluded) })
    );
}

#[test]
fn context_pack_respects_budget_and_reports_omissions() {
    let fixture = fixture();
    let engine = SearchEngine::new(fixture.index);
    let pack = engine
        .context_pack(&ContextPackRequest {
            search: request("policy"),
            token_budget: 32,
            candidate_limit: 50,
            mode: ContextPackMode::Explicit,
        })
        .unwrap();
    assert!(pack.estimated_tokens <= pack.token_budget);
    assert!(pack.items.is_empty());
    assert_eq!(
        pack.omitted
            .iter()
            .filter(|item| item.reason == "token_budget")
            .count(),
        5
    );
    assert!(!pack.indexed_tree_oid.is_empty());
    assert!(pack.projection_generation > 0);

    let full = engine
        .context_pack(&ContextPackRequest {
            search: request("general_tab_visibility"),
            token_budget: 100_000,
            candidate_limit: 50,
            mode: ContextPackMode::Explicit,
        })
        .unwrap();
    assert_eq!(full.items.len(), 1);
    assert_eq!(full.items[0].detail, ContextPackDetail::Full);
    let summary = engine
        .context_pack(&ContextPackRequest {
            search: request("general_tab_visibility"),
            token_budget: full.estimated_tokens - 1,
            candidate_limit: 50,
            mode: ContextPackMode::Explicit,
        })
        .unwrap();
    assert_eq!(summary.items.len(), 1);
    assert_eq!(summary.items[0].detail, ContextPackDetail::Summary);
    assert!(summary.items[0].evidence.is_empty());
    assert!(
        summary
            .omitted
            .iter()
            .any(|item| item.reason == "detail_token_budget")
    );
}
