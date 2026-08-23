use sctx_domain::{
    Applicability, ConflictParticipant, ContextId, ContextKind, ContextRelation,
    ContextRelationKind, ContextRevisionDraft, EvidenceSnapshotDraft, EvidenceType,
    PublicationAction, PublicationDraft, RevisionId, SemanticConflictDraft, SpaceId, TaskId,
    TaskSignal, TaskSignalKind, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{
    ContextStatus, ScopeFilter, SearchEngine, SearchFilters, SearchRequest, SpaceIntentField,
};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    safe_context_id: ContextId,
    safe_revision_id: RevisionId,
    conflict_context_ids: [ContextId; 2],
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
        relations: Vec::new(),
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

fn intent_revision_id(event: &Event) -> RevisionId {
    match event.payload() {
        EventPayload::SpaceCreated {
            intent_revision, ..
        }
        | EventPayload::SpaceIntentRevisionAdded {
            intent_revision, ..
        } => intent_revision.revision_id,
        _ => panic!("expected Space Intent event"),
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

    let (_candidate, _) = add_context(
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
fn relation_kind_rationale_support_and_target_id_are_searchable_from_source_revision() {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::initialize(temporary.path().join("relation-search")).unwrap();
    let target_space_event = Event::space_created(intent("Relation Target"), None).unwrap();
    let target_space_id = space_id(&target_space_event);
    append(&store, target_space_event);
    let (target_context_id, _) = add_context(
        &store,
        target_space_id,
        revision("Target Contract", "target rationale", "target evidence"),
    );

    let source_space_event = Event::space_created(intent("Relation Source"), None).unwrap();
    let source_space_id = space_id(&source_space_event);
    append(&store, source_space_event);
    let mut source = revision("Source Decision", "source rationale", "source evidence");
    source.relations = vec![ContextRelation {
        target_context_id,
        kind: ContextRelationKind::ValidatedBy,
        rationale: "relationrationaleneedle links the validation Contract".to_owned(),
        supports: vec!["relationsupportneedle proves the stable edge".to_owned()],
    }];
    let (source_context_id, source_revision_id) = add_context(&store, source_space_id, source);
    let engine = SearchEngine::new(ProjectionIndex::for_store(&store));

    for query in [
        "validated_by".to_owned(),
        "relationrationaleneedle".to_owned(),
        "relationsupportneedle".to_owned(),
        target_context_id.to_string(),
    ] {
        let response = engine
            .search(&SearchRequest {
                query,
                page_size: 20,
                ..SearchRequest::default()
            })
            .unwrap();
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].context_id, source_context_id);
        assert_eq!(response.results[0].revision_id, source_revision_id);
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
fn conflicts_show_both_sides_and_disable_automatic_injection() {
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
}

struct IntentFixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    rich_space_id: SpaceId,
    shared_space_ids: [SpaceId; 2],
    conflicted_space_id: SpaceId,
    conflicted_head_ids: [RevisionId; 2],
}

fn rich_intent() -> sctx_domain::IntentSnapshot {
    sctx_domain::IntentSnapshot {
        title: "EnglishTitleNeedle".to_owned(),
        problem: "中文检索必须恢复历史意图".to_owned(),
        desired_outcome: "MultilingualOutcome".to_owned(),
        in_scope: vec!["GeneralTabVisibility".to_owned()],
        out_of_scope: vec!["LegacyRouterBoundary".to_owned()],
        acceptance_conditions: vec!["SearchV2Endpoint remains stable".to_owned()],
        domain_terms: vec!["RequirementIntent".to_owned()],
    }
}

fn minimal_intent(title: &str, domain_term: &str) -> sctx_domain::IntentSnapshot {
    sctx_domain::IntentSnapshot {
        title: title.to_owned(),
        problem: format!("{title} has a distinct problem"),
        desired_outcome: format!("{title} has a distinct outcome"),
        in_scope: vec![format!("{title}Scope")],
        out_of_scope: vec![format!("{title}Excluded")],
        acceptance_conditions: vec![format!("{title}Accepted")],
        domain_terms: vec![domain_term.to_owned()],
    }
}

fn intent_fixture() -> IntentFixture {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::initialize(temporary.path().join("intent-installation")).unwrap();

    let rich = Event::space_created(rich_intent(), None).unwrap();
    let rich_space_id = space_id(&rich);
    append(&store, rich);

    let shared_alpha = Event::space_created(
        minimal_intent("AlphaSpace", "sharedassociationneedle"),
        None,
    )
    .unwrap();
    let shared_alpha_id = space_id(&shared_alpha);
    append(&store, shared_alpha);
    let shared_bravo = Event::space_created(
        minimal_intent("BravoSpace", "sharedassociationneedle"),
        None,
    )
    .unwrap();
    let shared_bravo_id = space_id(&shared_bravo);
    append(&store, shared_bravo);

    let conflict_base =
        Event::space_created(minimal_intent("ConflictBase", "conflictbase"), None).unwrap();
    let conflicted_space_id = space_id(&conflict_base);
    let conflict_parent_id = intent_revision_id(&conflict_base);
    append(&store, conflict_base);
    let left = Event::intent_revision_added(
        conflicted_space_id,
        vec![conflict_parent_id],
        minimal_intent("forkmatchneedle leftonlyneedle", "leftbranch"),
        None,
    )
    .unwrap();
    let left_id = intent_revision_id(&left);
    append(&store, left);
    let right = Event::intent_revision_added(
        conflicted_space_id,
        vec![conflict_parent_id],
        minimal_intent("forkmatchneedle rightonlyneedle", "rightbranch"),
        None,
    )
    .unwrap();
    let right_id = intent_revision_id(&right);
    append(&store, right);

    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    IntentFixture {
        _temporary: temporary,
        index,
        rich_space_id,
        shared_space_ids: [shared_alpha_id, shared_bravo_id],
        conflicted_space_id,
        conflicted_head_ids: [left_id, right_id],
    }
}

fn task_query(text: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: text.to_owned(),
        current_direction: Some(text.to_owned()),
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

#[test]
fn task_intent_and_signals_match_chinese_english_code_and_api_tokens() {
    let fixture = intent_fixture();
    let engine = SearchEngine::new(fixture.index);
    for (task, signals, expected_field) in [
        (
            task_query("中文检索"),
            Vec::new(),
            SpaceIntentField::Problem,
        ),
        (
            task_query("multilingualoutcome"),
            Vec::new(),
            SpaceIntentField::DesiredOutcome,
        ),
        (
            task_query("unmatchedcodesignal"),
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "GeneralTabVisibility".to_owned(),
            }],
            SpaceIntentField::InScope,
        ),
        (
            task_query("unmatchedapisignal"),
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "SearchV2Endpoint".to_owned(),
            }],
            SpaceIntentField::AcceptanceConditions,
        ),
        (
            task_query("LegacyRouterBoundary"),
            Vec::new(),
            SpaceIntentField::OutOfScope,
        ),
    ] {
        let task_id = TaskId::new();
        let response = engine
            .space_intent_candidates(task_id, &task, &signals)
            .unwrap();
        assert_eq!(response.task_id, task_id);
        assert_eq!(response.candidates.len(), 1);
        assert_eq!(response.candidates[0].space_id, fixture.rich_space_id);
        assert!(
            response.candidates[0]
                .matched_fields
                .contains(&expected_field)
        );
        assert!(!response.candidates[0].matched_tokens.is_empty());
        assert!(response.candidates[0].bm25.is_finite());
    }
}

#[test]
fn full_task_query_explains_all_current_intent_fields() {
    let fixture = intent_fixture();
    let engine = SearchEngine::new(fixture.index);
    let task = WorkingIntentSnapshot {
        goal: "englishtitleneedle".to_owned(),
        current_direction: Some("multilingualoutcome".to_owned()),
        in_scope: vec!["中文检索".to_owned()],
        out_of_scope: vec!["legacyrouterboundary".to_owned()],
        domains: vec!["requirementintent".to_owned()],
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec!["searchv2endpoint".to_owned()],
        artifact_hints: vec!["generaltabvisibility".to_owned()],
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    };
    let response = engine
        .space_intent_candidates(TaskId::new(), &task, &[])
        .unwrap();
    assert_eq!(response.candidates.len(), 1);
    assert_eq!(response.candidates[0].space_id, fixture.rich_space_id);
    assert_eq!(
        response.candidates[0].matched_fields,
        vec![
            SpaceIntentField::Title,
            SpaceIntentField::Problem,
            SpaceIntentField::DesiredOutcome,
            SpaceIntentField::InScope,
            SpaceIntentField::OutOfScope,
            SpaceIntentField::AcceptanceConditions,
            SpaceIntentField::DomainTerms,
        ]
    );
    assert_eq!(
        response.candidates[0]
            .field_matches
            .iter()
            .map(|field_match| field_match.field)
            .collect::<Vec<_>>(),
        response.candidates[0].matched_fields
    );
}

#[test]
fn candidate_query_returns_zero_one_or_many_with_stable_id_ties() {
    let fixture = intent_fixture();
    let index = fixture.index.clone();
    let engine = SearchEngine::new(fixture.index);

    let none = engine
        .space_intent_candidates(TaskId::new(), &task_query("totallyabsenttoken"), &[])
        .unwrap();
    assert!(none.candidates.is_empty());

    let one = engine
        .space_intent_candidates(TaskId::new(), &task_query("EnglishTitleNeedle"), &[])
        .unwrap();
    assert_eq!(one.candidates.len(), 1);
    assert_eq!(one.candidates[0].space_id, fixture.rich_space_id);

    let task = task_query("sharedassociationneedle");
    let many = engine
        .space_intent_candidates(TaskId::new(), &task, &[])
        .unwrap();
    let mut expected = fixture.shared_space_ids.to_vec();
    expected.sort();
    let actual = many
        .candidates
        .iter()
        .map(|candidate| candidate.space_id)
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "equal BM25 values must use Space ID ties");
    assert_eq!(
        many.candidates[0].bm25.to_bits(),
        many.candidates[1].bm25.to_bits()
    );

    index.rebuild().unwrap();
    let rebuilt = SearchEngine::new(index)
        .space_intent_candidates(TaskId::new(), &task, &[])
        .unwrap();
    assert_eq!(rebuilt.candidates, many.candidates);
}

#[test]
fn conflicted_intent_returns_every_head_without_silently_selecting_one() {
    let fixture = intent_fixture();
    let engine = SearchEngine::new(fixture.index);
    let both = engine
        .space_intent_candidates(TaskId::new(), &task_query("forkmatchneedle"), &[])
        .unwrap();
    assert_eq!(both.candidates.len(), 1);
    let candidate = &both.candidates[0];
    assert_eq!(candidate.space_id, fixture.conflicted_space_id);
    assert!(candidate.intent_conflicted);
    let mut expected_heads = fixture.conflicted_head_ids.to_vec();
    expected_heads.sort();
    assert_eq!(candidate.head_revision_ids, expected_heads);
    assert_eq!(candidate.matching_heads.len(), 2);
    assert!(candidate.matching_heads.iter().all(|head| {
        !head.field_matches.is_empty()
            && head
                .field_matches
                .iter()
                .all(|field_match| !field_match.matched_tokens.is_empty())
    }));
    assert_eq!(
        candidate
            .matching_heads
            .iter()
            .map(|head| head.revision_id)
            .collect::<Vec<_>>(),
        expected_heads
    );

    let one_side = engine
        .space_intent_candidates(TaskId::new(), &task_query("leftonlyneedle"), &[])
        .unwrap();
    assert_eq!(one_side.candidates.len(), 1);
    assert!(one_side.candidates[0].intent_conflicted);
    assert_eq!(one_side.candidates[0].head_revision_ids, expected_heads);
    assert_eq!(one_side.candidates[0].matching_heads.len(), 1);
}
