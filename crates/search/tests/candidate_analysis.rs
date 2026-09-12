use std::{fs, process::Command, sync::Arc};

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactRef,
    CandidateAssessmentPath, CandidateAssessmentRelation, CandidateSpaceRecommendation,
    ContextCandidate, ContextId, ContextKind, ContextRelation, ContextRelationKind,
    ContextRevisionDraft, ContextRevisionRef, EngineeringArtifact, EngineeringReference,
    EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft,
    PublicationId, RecommendedSpaceRole, ReferenceId, ReferenceRelation, RepoRelativePath,
    RepositoryId, RepositoryIdentity, RevisionId, SpaceId, SubmissionId, TaskId,
    TaskIntentRevisionId, TaskSessionId, WorkEpisodeId, WorkEpisodeRef, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, ProjectedEngineeringReference, RepositoryScanOutcome,
    RepositorySnapshot, ScanCoverage, SnapshotArtifact, SnapshotSourcePolicy, SourceLanguage,
    build_graph_context_snapshots,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_search::{CandidateAnalysisRequest, SearchEngine};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    store: GitStore,
    index: ProjectionIndex,
    exact_space: SpaceId,
    exact_intent_initial: RevisionId,
    related_space: SpaceId,
    exact: ContextRevisionRef,
    support: ContextRevisionRef,
    revise: ContextRevisionRef,
    potential: ContextRevisionRef,
    fts: ContextRevisionRef,
    exact_draft: ContextRevisionDraft,
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append Candidate analysis fixture");
}

fn intent(title: &str, term: &str) -> IntentSnapshot {
    IntentSnapshot {
        title: title.to_owned(),
        problem: format!("{term} knowledge is fragmented"),
        desired_outcome: format!("govern {term} behavior"),
        in_scope: vec![term.to_owned()],
        out_of_scope: vec![format!("unrelated {term}")],
        acceptance_conditions: vec![format!("{term} is reviewed")],
        domain_terms: vec![term.to_owned()],
    }
}

fn add_space(store: &GitStore, title: &str, term: &str) -> (SpaceId, RevisionId) {
    let event = Event::space_created(intent(title, term), None).unwrap();
    let EventPayload::SpaceCreated {
        space_id,
        intent_revision,
    } = event.payload()
    else {
        unreachable!();
    };
    let ids = (*space_id, intent_revision.revision_id);
    append(store, event);
    ids
}

fn draft(
    topic: Option<&str>,
    statement: &str,
    rationale: &str,
    domain: &str,
) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Discovery,
        problem_view: None,
        hints: Vec::new(),
        topic_key: topic.map(ToOwned::to_owned),
        statement: statement.to_owned(),
        rationale: rationale.to_owned(),
        applicability: Applicability {
            domains: vec![domain.to_owned()],
            platforms: vec!["fe".to_owned()],
            conditions: vec!["production".to_owned()],
        },
        assumptions: Vec::new(),
        recheck_when: vec!["the contract changes".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: format!("{statement} was validated"),
            content: serde_json::json!({"actual": statement}),
            interpretation: "the immutable Context is self-contained".to_owned(),
            limitations: Vec::new(),
        }],
    }
}

fn add_context(
    store: &GitStore,
    space_id: SpaceId,
    draft: ContextRevisionDraft,
) -> ContextRevisionRef {
    let event = Event::context_revision_added(space_id, draft, None).unwrap();
    append_context(store, space_id, event)
}

fn add_context_with_id(
    store: &GitStore,
    space_id: SpaceId,
    content: ContextRevisionDraft,
    id: &str,
) -> ContextRevisionRef {
    let event = Event::context_revision_added(space_id, content, None).unwrap();
    let mut value = serde_json::to_value(event).unwrap();
    value["context_id"] = serde_json::json!(id);
    append_context(store, space_id, serde_json::from_value(value).unwrap())
}

fn append_context(store: &GitStore, space_id: SpaceId, event: Event) -> ContextRevisionRef {
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision,
        ..
    } = event.payload()
    else {
        unreachable!();
    };
    let target = ContextRevisionRef {
        context_id: *context_id,
        revision_id: revision.revision_id,
    };
    append(store, event);
    append(
        store,
        Event::publication_changed(
            space_id,
            target.context_id,
            PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: PublicationAction::Publish,
                revision_id: target.revision_id,
                review_event_ids: Vec::new(),
            },
            None,
        )
        .unwrap(),
    );
    target
}

fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("candidate analysis root");
    let base = GitStore::bootstrap_local(&root).unwrap();
    let (exact_space, exact_intent_initial) =
        add_space(&base, "Search Requirement", "searchcandidate");
    let (related_space, _) = add_space(&base, "Protocol Contract", "protocolcandidate");
    let exact_draft = draft(
        Some("candidate/exact"),
        "Canonical Candidate behavior",
        "Every field is equal",
        "exact-domain",
    );
    let exact = add_context(&base, exact_space, exact_draft.clone());
    let support = add_context(
        &base,
        exact_space,
        draft(
            Some("candidate/support"),
            "Shared support statement",
            "Existing rationale",
            "support-domain",
        ),
    );
    let revise = add_context(
        &base,
        related_space,
        draft(
            Some("candidate/revise"),
            "Old revision behavior",
            "Old rationale",
            "revise-domain",
        ),
    );
    let potential = add_context(
        &base,
        related_space,
        draft(
            Some("candidate/potential"),
            "Existing affirmative policy",
            "Existing potential rationale",
            "potential-domain",
        ),
    );
    let fts = add_context(
        &base,
        related_space,
        draft(
            Some("candidate/fts-target"),
            "RareWebsocketQuorum contract is retained",
            "Text retrieval only",
            "fts-target-domain",
        ),
    );
    let index = ProjectionIndex::for_store(&base);
    index.synchronize().unwrap();
    let store = base.with_candidate_submission_index(Arc::new(index.clone()));
    Fixture {
        _temporary: temporary,
        store,
        index,
        exact_space,
        exact_intent_initial,
        related_space,
        exact,
        support,
        revise,
        potential,
        fts,
        exact_draft,
    }
}

#[test]
fn bm25_relevance_order_survives_candidate_analysis() {
    let temporary = tempfile::tempdir().unwrap();
    let store = GitStore::bootstrap_local(temporary.path().join("bm25 candidate order")).unwrap();
    let (space, _) = add_space(&store, "Ranking", "ranking");
    let low = add_context_with_id(
        &store,
        space,
        draft(
            None,
            "Ordinary cache behavior",
            "quizzical appears in rationale",
            "low-domain",
        ),
        "ctx_00000000-0000-4000-8000-000000000001",
    );
    let high = add_context_with_id(
        &store,
        space,
        draft(
            None,
            "quizzical quizzical quizzical cache",
            "Stronger statement match",
            "high-domain",
        ),
        "ctx_ffffffff-ffff-4fff-bfff-ffffffffffff",
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let engine = SearchEngine::new(index);
    let search = engine
        .search(&sctx_search::SearchRequest {
            query: "quizzical".to_owned(),
            filters: sctx_search::SearchFilters::default(),
            page_size: 8,
            cursor: None,
            match_mode: sctx_search::SearchMatchMode::Ranked,
        })
        .unwrap();
    assert_eq!(search.results.len(), 2);
    assert_eq!(search.results[0].context_id, high.context_id);
    assert_eq!(search.results[1].context_id, low.context_id);
    assert!(
        search.results[0].match_reason.bm25 < search.results[1].match_reason.bm25,
        "SQLite BM25 is lower for the more relevant hit"
    );
    let candidate = candidate(draft(
        None,
        "quizzical novel claim",
        "Independent claim",
        "candidate-domain",
    ));
    let result = engine
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 8_000,
            top_k: 8,
        })
        .unwrap();
    assert_eq!(result.analysis.assessments.len(), 2);
    for assessment in &result.analysis.assessments {
        assert!(
            assessment
                .paths
                .iter()
                .all(|path| matches!(path, CandidateAssessmentPath::ContextFullText { .. })),
            "only BM25 may affect this fixture's rank: {:?}",
            assessment.paths
        );
    }
    assert_eq!(result.analysis.assessments[0].target, Some(high));
    assert_eq!(result.analysis.assessments[1].target, Some(low));
}

#[test]
fn conflicted_space_is_related_only_and_keeps_a_proposed_primary_option() {
    let fixture = fixture();
    for suffix in ["alpha", "beta"] {
        append(
            &fixture.store,
            Event::intent_revision_added(
                fixture.exact_space,
                vec![fixture.exact_intent_initial],
                intent(
                    &format!("Conflicted {suffix}"),
                    &format!("conflict-{suffix}"),
                ),
                None,
            )
            .unwrap(),
        );
    }
    fixture.index.synchronize().unwrap();
    let result = analyze(&fixture, fixture.exact_draft.clone(), Vec::new(), 4_000, 8);
    assert!(result.space_recommendations.iter().any(|recommendation| matches!(
        recommendation,
        CandidateSpaceRecommendation::Existing {
            space_id,
            role: RecommendedSpaceRole::Related,
            paths,
            ..
        } if *space_id == fixture.exact_space
            && paths.iter().any(|path| matches!(
                path,
                sctx_domain::CandidateSpaceRecommendationPath::IntentConflict { head_count: 2 }
            ))
    )));
    assert!(
        !result
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing {
                    space_id,
                    role: RecommendedSpaceRole::Primary,
                    ..
                } if *space_id == fixture.exact_space
            ))
    );
    assert!(
        result
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
            ))
    );
}

#[test]
fn empty_context_store_yields_zero_existing_spaces_and_does_not_write_git() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("empty candidate analysis root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let head = || {
        String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(store.repository())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    let before = head();
    let candidate = candidate(draft(
        None,
        "Empty store Candidate",
        "No Context exists",
        "empty-store-domain",
    ));
    let result = SearchEngine::new(index)
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 2_000,
            top_k: 4,
        })
        .unwrap();
    assert_eq!(
        result
            .space_recommendations
            .iter()
            .filter(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing { .. }
            ))
            .count(),
        0
    );
    assert!(
        result
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
            ))
    );
    assert_eq!(head(), before);
}

#[test]
fn canonically_duplicate_evidence_supports_do_not_invalidate_candidate_intent() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("duplicate evidence support root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let mut content = draft(
        Some("candidate/duplicate-support"),
        "Duplicate support Candidate",
        "Distinct Evidence may support the same acceptance condition",
        "duplicate-support-domain",
    );
    content.evidence[0].supports = "The implementation satisfies the contract".to_owned();
    content.evidence.push(EvidenceSnapshotDraft {
        kind: EvidenceType::SourceSnapshot,
        supports: "  the implementation   SATISFIES the contract  ".to_owned(),
        content: serde_json::json!({"source": "independent capture"}),
        interpretation: "a second observation supports the same conclusion".to_owned(),
        limitations: vec!["the observations remain independently reviewable".to_owned()],
    });
    let candidate = candidate(content);

    let result = SearchEngine::new(index)
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 4_000,
            top_k: 4,
        })
        .unwrap();

    assert_eq!(
        result.analysis.status,
        sctx_domain::CandidateAnalysisStatus::Complete
    );
}

#[test]
fn one_safe_exact_owner_yields_one_existing_primary_space() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("single candidate analysis root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let (space_id, _) = add_space(&store, "Single Requirement", "single-owner");
    let content = draft(
        Some("candidate/single"),
        "Single exact Candidate",
        "Only one Context owner exists",
        "single-owner-domain",
    );
    add_context(&store, space_id, content.clone());
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let candidate = candidate(content);
    let result = SearchEngine::new(index)
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 2_000,
            top_k: 4,
        })
        .unwrap();
    let existing = result
        .space_recommendations
        .iter()
        .filter(|recommendation| {
            matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(existing.len(), 1);
    assert!(matches!(
        existing[0],
        CandidateSpaceRecommendation::Existing {
            space_id: target,
            role: RecommendedSpaceRole::Primary,
            ..
        } if *target == space_id
    ));
}

fn source_episode() -> WorkEpisodeRef {
    WorkEpisodeRef {
        episode_id: WorkEpisodeId::new(),
        task_session_id: TaskSessionId::new(),
        task_id: TaskId::new(),
    }
}

fn candidate(content: ContextRevisionDraft) -> ContextCandidate {
    ContextCandidate::from_verified_submission(SubmissionId::new(), source_episode(), content)
        .unwrap()
}

fn candidate_for_task(content: ContextRevisionDraft, task_id: TaskId) -> ContextCandidate {
    ContextCandidate::from_verified_submission(
        SubmissionId::new(),
        WorkEpisodeRef {
            episode_id: WorkEpisodeId::new(),
            task_session_id: TaskSessionId::new(),
            task_id,
        },
        content,
    )
    .unwrap()
}

fn source_intent(_task_id: TaskId) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: "ZXQ review objective".to_owned(),
        current_direction: Some("Nebula routing objective".to_owned()),
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
#[allow(clippy::too_many_lines)]
fn proposed_space_group_is_stable_per_task_and_resolves_to_its_first_space() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("proposed Space group root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let task_id = TaskId::new();
    let intent_revision_id = TaskIntentRevisionId::new();
    let working_intent = WorkingIntentSnapshot {
        goal: "  System suggestion: Build grouped checkout compatibility knowledge for every supported client without duplicating candidate spaces after review  ".to_owned(),
        current_direction: Some("Keep one review boundary per Task".to_owned()),
        in_scope: vec!["grouped Candidate review".to_owned()],
        out_of_scope: vec!["semantic Candidate deduplication".to_owned()],
        domains: vec!["candidate".to_owned()],
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec!["all sibling Claims share one proposed Space".to_owned()],
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    };
    let analyze = |content: ContextRevisionDraft,
                   revision_id: TaskIntentRevisionId,
                   mapped_space_id: Option<SpaceId>| {
        SearchEngine::new(index.clone())
            .analyze_candidate(&CandidateAnalysisRequest {
                has_blocking_unknowns: false,
                source_task_id: task_id,
                source_intent_revision_id: revision_id,
                source_working_intent: working_intent.clone(),
                source_task_signals: Vec::new(),
                candidate: candidate_for_task(content, task_id),
                explicit_related_contexts: Vec::new(),
                artifact_refs: Vec::new(),
                proposed_space_group_space_id: mapped_space_id,
                token_budget: 8_000,
                top_k: 8,
            })
            .unwrap()
    };
    let first = analyze(
        draft(
            Some("candidate/group-a"),
            "First grouped Candidate",
            "First Claim rationale",
            "group-a",
        ),
        intent_revision_id,
        None,
    );
    let second = analyze(
        draft(
            Some("candidate/group-b"),
            "Second grouped Candidate",
            "Second Claim rationale",
            "group-b",
        ),
        intent_revision_id,
        None,
    );
    let proposed = |result: &sctx_search::CandidateAnalysisResult| {
        result
            .space_recommendations
            .iter()
            .find_map(|recommendation| match recommendation {
                CandidateSpaceRecommendation::ProposedNewSpaceIntent {
                    recommendation_id,
                    proposed_space_group_key,
                    proposed_new_space_intent,
                    ..
                } => Some((
                    *recommendation_id,
                    proposed_space_group_key.unwrap(),
                    proposed_new_space_intent.clone(),
                )),
                CandidateSpaceRecommendation::Existing { .. } => None,
            })
            .unwrap()
    };
    let first_proposed = proposed(&first);
    let second_proposed = proposed(&second);
    assert_eq!(first_proposed, second_proposed);
    // The goal is normalized (prefix stripped, whitespace collapsed) and elided at 40 `char`s,
    // so the title stays a scannable handle while `desired_outcome` keeps the whole goal.
    assert_eq!(
        first_proposed.2.title,
        "Build grouped checkout compatibility kno\u{2026}"
    );
    assert!(!first_proposed.2.title.contains("System suggestion"));
    assert!(first_proposed.2.title.chars().count() <= 41);
    assert_eq!(first_proposed.2.desired_outcome, working_intent.goal);

    // The group is bound to the Task alone: a later Intent revision — every governance turn
    // makes one — must keep proposing the same Space rather than a second one.
    let next_revision = analyze(
        draft(
            Some("candidate/group-c"),
            "Third grouped Candidate",
            "Third Claim rationale",
            "group-c",
        ),
        TaskIntentRevisionId::new(),
        None,
    );
    assert_eq!(proposed(&next_revision).1, first_proposed.1);

    let (mapped_space_id, _) = add_space(&store, "Mapped Task Intent", "mapped-task-intent");
    index.synchronize().unwrap();
    let mapped = analyze(
        draft(
            Some("candidate/group-d"),
            "Fourth grouped Candidate",
            "Fourth Claim rationale",
            "group-d",
        ),
        intent_revision_id,
        Some(mapped_space_id),
    );
    assert!(mapped.space_recommendations.iter().any(|recommendation| matches!(
        recommendation,
        CandidateSpaceRecommendation::Existing {
            space_id,
            role: RecommendedSpaceRole::Primary,
            paths,
            ..
        } if *space_id == mapped_space_id && paths.iter().any(|path| matches!(
            path,
            sctx_domain::CandidateSpaceRecommendationPath::ProposedSpaceGroupResolved { proposed_space_group_key }
                if *proposed_space_group_key == first_proposed.1
        ))
    )));
    assert!(
        !mapped
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
            ))
    );
}

fn analyze(
    fixture: &Fixture,
    content: ContextRevisionDraft,
    explicit: Vec<ContextRevisionRef>,
    budget: usize,
    top_k: usize,
) -> sctx_search::CandidateAnalysisResult {
    let candidate = candidate(content);
    SearchEngine::new(fixture.index.clone())
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: explicit,
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: budget,
            top_k,
        })
        .unwrap()
}

fn relation_for(
    result: &sctx_search::CandidateAnalysisResult,
    target: ContextRevisionRef,
) -> CandidateAssessmentRelation {
    result
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(target))
        .unwrap_or_else(|| panic!("missing assessment for {target:?}"))
        .relation
}

/// One statement under one topic key is one fact, however the two drafts otherwise differ.
///
/// Two Tasks recording the same finding never produce byte-identical drafts — the Evidence, the
/// rationale wording and the problem each Task was working on all differ — so before Candidate
/// Build derived a topic key this pair could only ever reach `supports`.
#[test]
fn a_restated_statement_on_one_topic_key_is_an_exact_duplicate() {
    let fixture = fixture();
    let restated = analyze(
        &fixture,
        draft(
            Some("candidate/support"),
            "Shared support statement",
            "A second Task recorded the same finding in its own words",
            "support-other-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    assert_eq!(
        relation_for(&restated, fixture.support),
        CandidateAssessmentRelation::ExactDuplicate
    );
    let assessment = restated
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(fixture.support))
        .unwrap();
    assert!(
        assessment
            .paths
            .contains(&CandidateAssessmentPath::StatementEquality)
    );
    assert!(assessment.paths.iter().any(|path| matches!(
        path,
        CandidateAssessmentPath::TopicEquality { topic_key } if topic_key == "candidate/support"
    )));
    assert_eq!(
        restated.candidate_status,
        sctx_domain::AutomaticCandidateStatus::ExactDuplicateReview
    );
}

#[test]
fn exact_support_revise_potential_fts_and_novel_remain_distinct_assessments() {
    let fixture = fixture();
    let exact = analyze(&fixture, fixture.exact_draft.clone(), Vec::new(), 8_000, 16);
    assert_eq!(
        relation_for(&exact, fixture.exact),
        CandidateAssessmentRelation::ExactDuplicate
    );
    assert_eq!(
        exact.candidate_status,
        sctx_domain::AutomaticCandidateStatus::ExactDuplicateReview
    );

    let support = analyze(
        &fixture,
        draft(
            Some("candidate/support-new"),
            "Shared support statement",
            "New Evidence changes the rationale",
            "support-other-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    assert_eq!(
        relation_for(&support, fixture.support),
        CandidateAssessmentRelation::Supports
    );

    let revise = analyze(
        &fixture,
        draft(
            Some("candidate/revise"),
            "New revision behavior",
            "Explicit relation permits revision review",
            "unrelated-revise-scope",
        ),
        vec![fixture.revise],
        8_000,
        16,
    );
    assert_eq!(
        relation_for(&revise, fixture.revise),
        CandidateAssessmentRelation::Revises
    );

    let potential = analyze(
        &fixture,
        draft(
            Some("candidate/potential"),
            "Candidate negative policy",
            "Different statement requires review",
            "potential-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    assert_eq!(
        relation_for(&potential, fixture.potential),
        CandidateAssessmentRelation::PotentialContradiction
    );
    assert_eq!(
        potential.candidate_status,
        sctx_domain::AutomaticCandidateStatus::PotentialContradictionReview
    );

    let fts = analyze(
        &fixture,
        draft(
            Some("candidate/no-topic-match"),
            "RareWebsocketQuorum is observed by a client",
            "Only full text can connect these words",
            "fts-candidate-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    assert_eq!(
        relation_for(&fts, fixture.fts),
        CandidateAssessmentRelation::UnresolvedRelated
    );

    let novel = analyze(
        &fixture,
        draft(
            None,
            "ZXQ entirely unseen observation",
            "Nebula-only rationale",
            "novel-isolated-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    assert_eq!(novel.analysis.assessments.len(), 1);
    assert_eq!(
        novel.analysis.assessments[0].relation,
        CandidateAssessmentRelation::Novel
    );
    assert_eq!(
        novel.candidate_status,
        sctx_domain::AutomaticCandidateStatus::NeedsSpaceReview
    );
}

const NEAR_TARGET_STATEMENT: &str =
    "Retry budget is enforced by the gateway before the request reaches upstream";
const NEAR_CANDIDATE_STATEMENT: &str =
    "Retry budget is enforced by the gateway before the request reaches the upstream service";

/// Builds one accepted Context whose Evidence text does not embed the statement, so a reworded
/// Candidate can be compared field by field.
fn near_duplicate_base() -> ContextRevisionDraft {
    let mut base = draft(
        Some("candidate/near-duplicate"),
        NEAR_TARGET_STATEMENT,
        "Existing rationale for the enforced retry budget",
        "near-duplicate-domain",
    );
    "The retry budget enforcement was validated".clone_into(&mut base.evidence[0].supports);
    base.evidence[0].content = serde_json::json!({"actual": "retry budget enforcement"});
    base
}

#[test]
fn a_reworded_statement_supports_an_accepted_context_instead_of_contradicting_it() {
    let fixture = fixture();
    let base = near_duplicate_base();
    let target = add_context(&fixture.store, fixture.exact_space, base.clone());
    fixture.index.synchronize().unwrap();

    let mut reworded = base.clone();
    NEAR_CANDIDATE_STATEMENT.clone_into(&mut reworded.statement);
    "New Evidence changed why the retry budget is enforced".clone_into(&mut reworded.rationale);
    let supports = analyze(&fixture, reworded, Vec::new(), 8_000, 16);
    assert_eq!(
        relation_for(&supports, target),
        CandidateAssessmentRelation::Supports
    );
    let assessment = supports
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(target))
        .unwrap();
    assert!(
        assessment
            .paths
            .iter()
            .any(|path| matches!(path, CandidateAssessmentPath::StatementEquality)),
        "a near-duplicate statement enters the statement channel: {:?}",
        assessment.paths
    );
    assert!(
        assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("statement similarity")),
        "reasons name the triggering path: {:?}",
        assessment.reasons
    );

    let mut identical = base;
    NEAR_CANDIDATE_STATEMENT.clone_into(&mut identical.statement);
    let duplicate = analyze(&fixture, identical, Vec::new(), 8_000, 16);
    assert_eq!(
        relation_for(&duplicate, target),
        CandidateAssessmentRelation::ExactDuplicate
    );
}

#[test]
fn a_negated_near_duplicate_statement_is_a_contradiction_not_support() {
    let fixture = fixture();
    let mut base = near_duplicate_base();
    "Retry budget is reached by the gateway before upstream".clone_into(&mut base.statement);
    let target = add_context(&fixture.store, fixture.exact_space, base.clone());
    fixture.index.synchronize().unwrap();

    let mut negated = base;
    "Retry budget is not reached by the gateway before upstream".clone_into(&mut negated.statement);
    let result = analyze(&fixture, negated, Vec::new(), 8_000, 16);
    let assessment = result
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(target))
        .expect("the near-duplicate statement is still retrieved");
    assert_eq!(
        assessment.relation,
        CandidateAssessmentRelation::PotentialContradiction
    );
    assert!(
        !assessment.paths.iter().any(|path| matches!(
            path,
            CandidateAssessmentPath::StatementEquality
                | CandidateAssessmentPath::CanonicalDraftEquality
        )),
        "a negated statement never claims an equality path: {:?}",
        assessment.paths
    );
    assert!(
        assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("negation markers differ")),
        "reasons name the negation guard: {:?}",
        assessment.reasons
    );
}

/// A same-topic pair whose statement differs only by scattered synonym substitution — a
/// paraphrase, not a disagreement. Word overlap alone (unigram Jaccard) sits below the
/// `statement` channel's strong threshold, so before this bound existed every such pair fell
/// through to `topic && statement_differs` and was sent to `potential_contradiction`: a real
/// false positive a reviewer had to dismiss (the R2-B pattern — two Claims about the same topic
/// that complement rather than oppose each other). Bigram overlap tells them apart because a
/// paraphrase still reproduces most of its neighbor-token pairs.
#[test]
fn a_same_topic_paraphrase_is_unresolved_related_not_a_contradiction() {
    let fixture = fixture();
    let target_statement = "The bottom bar fallback registers the default comment input box \
        whenever the candidate list returned by the priority resolver is empty at render time";
    let target = add_context(
        &fixture.store,
        fixture.exact_space,
        draft(
            Some("candidate/paraphrase"),
            target_statement,
            "Existing rationale for the fallback registration",
            "paraphrase-domain",
        ),
    );
    fixture.index.synchronize().unwrap();

    let paraphrase = draft(
        Some("candidate/paraphrase"),
        "The bottom bar fallback installs the default comment input box whenever the candidate \
            list produced by the priority resolver is empty during render",
        "A second Task described the same fallback from its own angle",
        "paraphrase-domain",
    );
    let result = analyze(&fixture, paraphrase, Vec::new(), 8_000, 16);
    assert_eq!(
        relation_for(&result, target),
        CandidateAssessmentRelation::UnresolvedRelated,
        "a reworded restatement of the same fallback must not read as a contradiction hypothesis"
    );
    let assessment = result
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(target))
        .unwrap();
    assert!(
        assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("paraphrase")),
        "reasons should name the paraphrase read: {:?}",
        assessment.reasons
    );
}

/// A same-topic pair that gives an opposite judgment about the same retry policy: one says the
/// backoff delay doubles after each failure, the other says it stays fixed regardless of
/// failures. The wording is independently written (low bigram overlap, no shared negation
/// marker), which is exactly the shape the bigram guard must still forward to a human — tightening
/// the paraphrase path must never silence a real conflict.
#[test]
fn a_same_topic_opposite_judgment_still_reaches_contradiction_review() {
    let fixture = fixture();
    let target_statement = "The retry policy on the payment gateway doubles the backoff delay after each failed attempt";
    let target = add_context(
        &fixture.store,
        fixture.exact_space,
        draft(
            Some("candidate/retry-policy"),
            target_statement,
            "Existing rationale for the exponential backoff",
            "retry-policy-domain",
        ),
    );
    fixture.index.synchronize().unwrap();

    let opposite = draft(
        Some("candidate/retry-policy"),
        "The retry policy on the payment gateway always waits a fixed two second gap between \
            every attempt regardless of the failure count",
        "A second Task inspected the same retry policy and reached a different reading",
        "retry-policy-domain",
    );
    let result = analyze(&fixture, opposite, Vec::new(), 8_000, 16);
    assert_eq!(
        relation_for(&result, target),
        CandidateAssessmentRelation::PotentialContradiction,
        "an independently worded, opposing account of the same retry policy must still reach \
            human review"
    );
    assert_eq!(
        result.candidate_status,
        sctx_domain::AutomaticCandidateStatus::PotentialContradictionReview
    );
}

/// The shape five of this repository's own nine accepted Contexts were recorded in: one
/// conclusion, written again from scratch by a later Task in slightly different words, under no
/// topic key at all. Statement equality on a topic key cannot see it — the topic key is optional
/// and neither Task typed one — so every rewrite used to arrive as merely related and got
/// confirmed as a brand new fact.
#[test]
fn one_conclusion_rewritten_without_a_topic_key_is_a_duplicate_of_the_accepted_context() {
    const ACCEPTED: &str =
        "Session activation binds the lease to the agent session rather than the parent directory";
    const FIRST_REWRITE: &str =
        "Session activation binds the lease to the agent session instead of the parent directory";
    const SECOND_REWRITE: &str = "Session activation binds the session lease to the agent session \
                                  in place of the parent directory";

    let fixture = fixture();
    let accepted = draft(
        None,
        ACCEPTED,
        "The lease outlived the directory it was keyed on",
        "activation-domain",
    );
    let target = add_context(&fixture.store, fixture.exact_space, accepted.clone());
    fixture.index.synchronize().unwrap();

    for restatement in [FIRST_REWRITE, SECOND_REWRITE] {
        let mut rewrite = accepted.clone();
        restatement.clone_into(&mut rewrite.statement);
        "A later Task reached the same conclusion on its own Evidence"
            .clone_into(&mut rewrite.rationale);
        let result = analyze(&fixture, rewrite, Vec::new(), 8_000, 16);
        let assessment = result
            .analysis
            .assessments
            .iter()
            .find(|assessment| assessment.target == Some(target))
            .unwrap_or_else(|| panic!("no assessment against the accepted Context: {restatement}"));
        assert_eq!(
            assessment.relation,
            CandidateAssessmentRelation::ExactDuplicate,
            "{restatement}"
        );
        let similarity = assessment
            .paths
            .iter()
            .find_map(|path| match path {
                CandidateAssessmentPath::NearDuplicateStatement {
                    similarity_basis_points,
                } => Some(*similarity_basis_points),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no near-duplicate path: {:?}", assessment.paths));
        assert!(
            (5_000..8_000).contains(&similarity),
            "the rewrite sits in the measured near-duplicate band, not on an equality path: \
             {similarity}"
        );
        assert_eq!(
            result.candidate_status,
            sctx_domain::AutomaticCandidateStatus::ExactDuplicateReview,
            "{restatement}"
        );
    }

    // The other half of the measurement: pairs about different conclusions sat at or below 1_100
    // basis points, so widening the duplicate band to 5_000 does not sweep them in.
    let unrelated = analyze(
        &fixture,
        draft(
            None,
            "Repository scan skips vendored directories before it plans any file read",
            "Scanning vendored trees cost more than it returned",
            "activation-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    assert!(
        unrelated
            .analysis
            .assessments
            .iter()
            .filter(|assessment| assessment.target == Some(target))
            .all(|assessment| assessment.relation != CandidateAssessmentRelation::ExactDuplicate),
        "an unrelated conclusion never restates the accepted Context: {:?}",
        unrelated.analysis.assessments
    );
}

#[test]
fn applicability_domain_overlap_alone_no_longer_contradicts() {
    let fixture = fixture();
    let scope_only = analyze(
        &fixture,
        draft(
            Some("candidate/scope-overlap-only"),
            "Candidate negative policy",
            "Only the inherited Intent domain is shared",
            "potential-domain",
        ),
        Vec::new(),
        8_000,
        16,
    );
    let assessment = scope_only
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(fixture.potential))
        .expect("the scope channel still retrieves the target");
    assert!(
        assessment
            .paths
            .iter()
            .any(|path| matches!(path, CandidateAssessmentPath::ScopeOverlap { .. })),
        "scope overlap is still explained: {:?}",
        assessment.paths
    );
    assert_eq!(
        assessment.relation,
        CandidateAssessmentRelation::UnresolvedRelated
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn space_recommendations_are_nonbinding_stable_budgeted_and_rebuildable() {
    let fixture = fixture();
    let first = analyze(&fixture, fixture.exact_draft.clone(), Vec::new(), 2_000, 4);
    assert!(first.analysis.estimated_tokens <= first.analysis.token_budget);
    assert!(
        first
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing {
                    space_id,
                    role: RecommendedSpaceRole::Primary,
                    ..
                } if *space_id == fixture.exact_space
            ))
    );
    assert!(
        !first
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
            ))
    );
    let repeated = analyze(&fixture, fixture.exact_draft.clone(), Vec::new(), 2_000, 4);
    assert_eq!(first.analysis, repeated.analysis);
    assert_eq!(
        serde_json::to_value(&first.space_recommendations).unwrap(),
        serde_json::to_value(&repeated.space_recommendations).unwrap()
    );

    fs::remove_file(fixture.index.database_path()).unwrap();
    fixture.index.synchronize().unwrap();
    let rebuilt = analyze(&fixture, fixture.exact_draft.clone(), Vec::new(), 2_000, 4);
    assert_eq!(
        first.analysis.context_tree_oid,
        rebuilt.analysis.context_tree_oid
    );
    assert_eq!(
        first.analysis.context_generation,
        rebuilt.analysis.context_generation
    );
    assert_eq!(
        first
            .analysis
            .assessments
            .iter()
            .map(|assessment| (assessment.relation, assessment.target))
            .collect::<Vec<_>>(),
        rebuilt
            .analysis
            .assessments
            .iter()
            .map(|assessment| (assessment.relation, assessment.target))
            .collect::<Vec<_>>()
    );
    assert!(rebuilt.analysis.estimated_tokens <= 2_000);

    let multiple = analyze(
        &fixture,
        draft(
            None,
            "Multiple explicit owners require review",
            "Two immutable Contexts were cited",
            "multi-owner-domain",
        ),
        vec![fixture.exact, fixture.revise],
        4_000,
        8,
    );
    assert_eq!(
        multiple
            .space_recommendations
            .iter()
            .filter(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing { .. }
            ))
            .count(),
        2
    );

    let novel = analyze(
        &fixture,
        draft(
            None,
            "Novel Space Required ZXQ",
            "No existing owner",
            "brand-new-domain",
        ),
        Vec::new(),
        2_000,
        4,
    );
    assert!(
        novel
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
            ))
    );
    assert!(
        novel
            .space_recommendations
            .iter()
            .all(|recommendation| !matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing {
                    role: RecommendedSpaceRole::Primary,
                    ..
                }
            ))
    );
    let _ = fixture.related_space;
    let _ = fixture.store.repository();
}

fn symbol_locator() -> ArtifactLocator {
    ArtifactLocator::Symbol {
        path: RepoRelativePath::new("src/search.ts").unwrap(),
        language: "typescript".to_owned(),
        module: "fe::search".to_owned(),
        enclosing_type: None,
        symbol_name: "SearchEntry".to_owned(),
        signature: "SearchEntry()".to_owned(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn exact_artifact_graph_reaches_cross_end_space_with_frozen_generation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("candidate graph root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let (source_space, _) = add_space(&store, "Frontend Source", "frontend-graph");
    let (contract_space, _) = add_space(&store, "Server Contract", "server-graph");
    let contract = add_context_with_id(
        &store,
        contract_space,
        draft(
            Some("candidate/graph-contract"),
            "Server contract is reached across the Graph",
            "Cross-end contract rationale",
            "server-graph-domain",
        ),
        "ctx_00000000-0000-4000-8000-000000000001",
    );
    let mut source_draft = draft(
        Some("candidate/graph-source"),
        "Old frontend Graph behavior",
        "Frontend Graph rationale",
        "frontend-graph-domain",
    );
    source_draft.relations = vec![ContextRelation {
        target_context_id: contract.context_id,
        kind: ContextRelationKind::Implements,
        rationale: "The frontend implements the server contract".to_owned(),
        supports: vec!["Cross-end integration is verified".to_owned()],
    }];
    let source = add_context_with_id(
        &store,
        source_space,
        source_draft,
        "ctx_ffffffff-ffff-4fff-bfff-ffffffffffff",
    );
    let index = ProjectionIndex::for_store(&store);
    let metadata = index.synchronize().unwrap().metadata;
    let repository = RepositoryIdentity {
        repository_id: RepositoryId::new(),
        canonical_name: "candidate-graph".to_owned(),
    };
    let artifact_key =
        ArtifactKey::derive(repository.repository_id.clone(), symbol_locator()).unwrap();
    let artifact = SnapshotArtifact {
        artifact: EngineeringArtifact {
            repository: repository.clone(),
            artifact_key,
            display_name: "SearchEntry".to_owned(),
        },
        snapshot_generation: "candidate-graph-generation".to_owned(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        observations: vec![ArtifactObservation {
            path: "src/search.ts".to_owned(),
            line: Some(1),
            language: SourceLanguage::TypeScriptJavaScript,
            source_state: ArtifactSourceState::TrackedHead,
        }],
    };
    let reference = ProjectedEngineeringReference {
        context_id: source.context_id,
        revision_id: source.revision_id,
        reference: EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: repository.repository_id.clone(),
            artifact_kind: ArtifactKind::Symbol,
            relation: ReferenceRelation::Implements,
            locator: symbol_locator(),
            supports: "The FE Symbol implements the source Context".to_owned(),
            limitations: vec!["Synthetic Graph fixture".to_owned()],
        },
    };
    let contexts = build_graph_context_snapshots(
        &index.domain_snapshot().unwrap().projection,
        std::slice::from_ref(&reference),
    )
    .unwrap();
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&reference),
            &[RepositoryScanOutcome::Available(RepositorySnapshot {
                repository_id: repository.repository_id.clone(),
                source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
                policy_version: "planned-paths-plus-safe-tracked-modifications-v2",
                head_tree_oid: "candidate-graph-tree".to_owned(),
                generation: "candidate-graph-generation".to_owned(),
                planned_paths: vec![RepoRelativePath::new("src/search.ts").unwrap()],
                coverage: ScanCoverage::Complete,
                artifacts: vec![artifact],
                scanned_files: 1,
                scanned_bytes: 100,
                skipped_files: Vec::new(),
            })],
            &contexts,
        )
        .unwrap();
    let graph_store = EngineeringProjectionStore::initialize(&root).unwrap();
    graph_store
        .rebuild_for_context_tree(&projection, Some(&metadata.indexed_tree_oid))
        .unwrap();
    let content = draft(
        Some("candidate/graph-source"),
        "New frontend Graph behavior",
        "Exact Artifact allows revision review",
        "frontend-graph-candidate",
    );
    let engine = SearchEngine::with_engineering_graph(index, graph_store);
    let analyze_with_graph = |content: ContextRevisionDraft| {
        let candidate = candidate(content);
        engine
            .analyze_candidate(&CandidateAnalysisRequest {
                has_blocking_unknowns: false,
                source_task_id: candidate.source_episode.task_id,
                source_intent_revision_id: TaskIntentRevisionId::new(),
                source_working_intent: source_intent(candidate.source_episode.task_id),
                source_task_signals: Vec::new(),
                candidate,
                explicit_related_contexts: Vec::new(),
                artifact_refs: vec![ArtifactRef {
                    repository_id: repository.repository_id.clone(),
                    locator: symbol_locator(),
                }],
                proposed_space_group_space_id: None,
                token_budget: 8_000,
                top_k: 16,
            })
            .unwrap()
    };
    // Only graph retrieval applies here. The root must precede its hop even though
    // the hop's ID sorts first.
    let graph_only = analyze_with_graph(draft(
        None,
        "Quizzical zephyrs dance",
        "Independent vocabulary",
        "unrelated-domain",
    ));
    assert_eq!(
        graph_only
            .analysis
            .assessments
            .iter()
            .map(|assessment| assessment.target)
            .collect::<Vec<_>>(),
        vec![Some(source), Some(contract)],
    );
    let result = analyze_with_graph(content);
    assert_eq!(
        relation_for(&result, source),
        CandidateAssessmentRelation::Revises
    );
    let cross = result
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(contract))
        .unwrap();
    assert!(cross.paths.iter().any(|path| matches!(
        path,
        CandidateAssessmentPath::ContextRelationHop { depth: 1, .. }
    )));
    assert_eq!(
        result.analysis.artifact_generation.as_deref(),
        Some(projection.artifact_generation.as_str())
    );
    assert!(
        result
            .space_recommendations
            .iter()
            .any(|recommendation| matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing { space_id, .. }
                    if *space_id == contract_space
            ))
    );
    assert_eq!(
        relation_for(&result, contract),
        CandidateAssessmentRelation::UnresolvedRelated,
        "a shared Artifact with unrelated wording stays unresolved"
    );

    // A shared exact Artifact plus a statement in the middle similarity band is the one new
    // contradiction path: close enough to be about the same fact, different enough to need review.
    let near = analyze_with_graph(draft(
        Some("candidate/graph-near"),
        "Server contract is reached across the client Graph boundary",
        "The reviewed branch disagrees about the same contract",
        "server-graph-candidate",
    ));
    let assessment = near
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(contract))
        .unwrap();
    assert_eq!(
        assessment.relation,
        CandidateAssessmentRelation::PotentialContradiction
    );
    assert!(
        assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("shared exact Artifact")),
        "reasons name the shared Artifact path: {:?}",
        assessment.reasons
    );
}

/// Three accepted Chinese Contexts, each restated by an English Claim that shares nothing but the
/// repository identifiers. This is the real-session shape that used to produce
/// `unresolved_related` for every duplicate: statement Jaccard sat around 2000 basis points, the
/// Contexts predate server-side Reference derivation so no Artifact is shared, and the Claim and
/// its target are written in different natural languages.
struct IdentifierFixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    anchor: ContextRevisionRef,
    dummy: ContextRevisionRef,
    bottom_bar: ContextRevisionRef,
}

fn identifier_draft(
    kind: ContextKind,
    topic: &str,
    statement: &str,
    rationale: &str,
    domain: &str,
) -> ContextRevisionDraft {
    let mut draft = draft(Some(topic), statement, rationale, domain);
    draft.kind = kind;
    draft
}

fn identifier_fixture() -> IdentifierFixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("identifier analysis root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let (space_id, _) = add_space(&store, "直播入口评审", "liveentryreview");
    let anchor = add_context(
        &store,
        space_id,
        identifier_draft(
            ContextKind::Issue,
            "identifier/anchor",
            "当 ISearchLiveEntryService 无真实实现或 addParamsForLiveAnchor 返回空时，\
             SearchProductAnchorAssem 会在商品锚点点击回调中提前返回，跳过配置分发与进入直播间导航。",
            "基线仍会导航，因此该路径不功能等价。",
            "anchor-domain",
        ),
    );
    let dummy = add_context(
        &store,
        space_id,
        identifier_draft(
            ContextKind::Discovery,
            "identifier/dummy",
            "无真实实现时，SearchDummyVerticalDomainService 与旧动态代理在基本类型与空返回值上大多等价；\
             SearchMusicDetailAssem 的日志方法由空值变为空映射。",
            "差异只影响埋点统计，不改变控制流。",
            "dummy-domain",
        ),
    );
    let bottom_bar = add_context(
        &store,
        space_id,
        identifier_draft(
            ContextKind::Validation,
            "identifier/bottombar",
            "包含真实垂类实现时，SearchPoiEntranceAssem 与 SearchBottomBarProtocolManager 的注册顺序保持不变，调试包构建成功。",
            "完整配置下的注册顺序与基线一致。",
            "bottombar-domain",
        ),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    IdentifierFixture {
        _temporary: temporary,
        index,
        anchor,
        dummy,
        bottom_bar,
    }
}

fn analyze_identifier_claim(
    fixture: &IdentifierFixture,
    content: ContextRevisionDraft,
) -> sctx_search::CandidateAnalysisResult {
    let candidate = candidate(content);
    SearchEngine::new(fixture.index.clone())
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 4_000,
            top_k: 8,
        })
        .unwrap()
}

fn assessment_for(
    result: &sctx_search::CandidateAnalysisResult,
    target: ContextRevisionRef,
) -> &sctx_domain::CandidateRelationAssessment {
    result
        .analysis
        .assessments
        .iter()
        .find(|assessment| assessment.target == Some(target))
        .unwrap_or_else(|| panic!("missing assessment for {target:?}"))
}

fn shared_identifiers(assessment: &sctx_domain::CandidateRelationAssessment) -> Vec<String> {
    assessment
        .paths
        .iter()
        .find_map(|path| match path {
            CandidateAssessmentPath::SharedIdentifier { identifiers } => Some(identifiers.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn three_shared_identifiers_on_one_kind_make_a_cross_language_restatement_supporting() {
    let fixture = identifier_fixture();
    let result = analyze_identifier_claim(
        &fixture,
        identifier_draft(
            ContextKind::Issue,
            "claim/anchor",
            "SearchProductAnchorAssem returns early inside the product anchor click callback \
             whenever ISearchLiveEntryService resolves to no implementation, so \
             addParamsForLiveAnchor never dispatches the live room navigation.",
            "The baseline still navigates, so this path is not functionally equivalent.",
            "claim-anchor-domain",
        ),
    );
    let assessment = assessment_for(&result, fixture.anchor);
    let shared = shared_identifiers(assessment);
    assert_eq!(
        shared,
        vec![
            "addparamsforliveanchor".to_owned(),
            "isearchliveentryservice".to_owned(),
            "searchproductanchorassem".to_owned(),
        ]
    );
    assert_eq!(assessment.relation, CandidateAssessmentRelation::Supports);
    assert!(
        assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("Shared repository identifiers:")
                && reason.contains("searchproductanchorassem")),
        "{:?}",
        assessment.reasons
    );
}

#[test]
fn two_shared_identifiers_on_one_kind_are_sent_to_contradiction_review_not_dropped() {
    let fixture = identifier_fixture();
    let result = analyze_identifier_claim(
        &fixture,
        identifier_draft(
            ContextKind::Discovery,
            "claim/dummy",
            "SearchDummyVerticalDomainService keeps the previous dynamic proxy semantics for \
             primitive and void returns, while SearchMusicDetailAssem now yields an empty map \
             from its logging method.",
            "Only analytics observe the difference.",
            "claim-dummy-domain",
        ),
    );
    let assessment = assessment_for(&result, fixture.dummy);
    assert_eq!(
        shared_identifiers(assessment),
        vec![
            "searchdummyverticaldomainservice".to_owned(),
            "searchmusicdetailassem".to_owned(),
        ]
    );
    assert_eq!(
        assessment.relation,
        CandidateAssessmentRelation::PotentialContradiction,
        "two shared identifiers below the restatement threshold need human review: {:?}",
        assessment.reasons
    );
}

#[test]
fn shared_identifiers_across_kinds_stay_unresolved_but_name_the_shared_code() {
    let fixture = identifier_fixture();
    let result = analyze_identifier_claim(
        &fixture,
        identifier_draft(
            ContextKind::Issue,
            "claim/bottombar",
            "SearchPoiEntranceAssem registers itself with SearchBottomBarProtocolManager before \
             its own container has been populated.",
            "The empty container then wins the slot.",
            "claim-bottombar-domain",
        ),
    );
    let assessment = assessment_for(&result, fixture.bottom_bar);
    assert_eq!(
        shared_identifiers(assessment),
        vec![
            "searchbottombarprotocolmanager".to_owned(),
            "searchpoientranceassem".to_owned(),
        ]
    );
    assert_eq!(
        assessment.relation,
        CandidateAssessmentRelation::UnresolvedRelated
    );
    assert!(
        assessment.reasons.iter().any(|reason| reason
            .contains("Shared repository identifiers on a different Context kind:")
            && reason.contains("searchpoientranceassem")),
        "{:?}",
        assessment.reasons
    );
}

/// Two accepted Contexts, the second retiring the first with a `Supersedes` Relation.
///
/// Both spell out the same three repository identifiers, so both would qualify for the strong
/// identifier channel on wording alone. Only the successor may have it.
struct SupersessionFixture {
    _temporary: TempDir,
    index: ProjectionIndex,
    retired: ContextRevisionRef,
    successor: ContextRevisionRef,
}

fn supersession_fixture() -> SupersessionFixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("supersession analysis root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let (space_id, _) = add_space(&store, "直播标签链路", "livetagchain");
    let retired = add_context(
        &store,
        space_id,
        identifier_draft(
            ContextKind::Issue,
            "supersession/retired",
            "搜索直播标签由 SearchLiveStruct 的 ProgrammedLiveShowTag 字段决定，\
             LiveTagResolver 仅读取该字段。",
            "旧链路把展示判断放在结构体字段上。",
            "livetag-domain",
        ),
    );
    let mut successor_content = identifier_draft(
        ContextKind::Issue,
        "supersession/successor",
        "搜索直播标签现由 LiveTagResolver 统一解析，SearchLiveStruct 的 \
         ProgrammedLiveShowTag 字段只作为输入。",
        "解析集中到一处之后字段本身不再决定展示。",
        "livetag-domain",
    );
    successor_content.relations = vec![ContextRelation {
        target_context_id: retired.context_id,
        kind: ContextRelationKind::Supersedes,
        rationale: "解析集中到 LiveTagResolver 之后，字段口径的结论不再成立。".to_owned(),
        supports: vec!["上线后展示与字段值脱钩。".to_owned()],
    }];
    let successor = add_context(&store, space_id, successor_content);
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    SupersessionFixture {
        _temporary: temporary,
        index,
        retired,
        successor,
    }
}

/// The safety verdict Candidate review draws must mean what the Pack's means.
///
/// `superseded_by` is derived in the index, never in Git, so the reduced domain projection cannot
/// carry it. Candidate analysis used to read only the projection and reached the index-derived
/// value through one late correction on the BM25 channel — which a retired Context only receives
/// if it happens to land on the single-token FTS page. The strong identifier channel draws its
/// line before that, so a retired Context could be promoted as a factual peer of the Claim while
/// the very same installation refused to inject it.
#[test]
fn a_superseded_target_loses_the_identifier_channel_and_names_the_context_that_replaced_it() {
    let fixture = supersession_fixture();
    let claim = identifier_draft(
        ContextKind::Issue,
        "claim/livetag",
        "The live tag in search is resolved by LiveTagResolver instead of the \
         ProgrammedLiveShowTag field on SearchLiveStruct.",
        "Resolution moved out of the struct field.",
        "livetag-domain",
    );
    let candidate = candidate(claim);
    let result = SearchEngine::new(fixture.index.clone())
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 8_000,
            top_k: 8,
        })
        .unwrap();

    // The channel itself still works: the successor holds the identifiers and the strong path.
    let successor = assessment_for(&result, fixture.successor);
    assert_eq!(
        shared_identifiers(successor),
        vec![
            "livetagresolver".to_owned(),
            "programmedliveshowtag".to_owned(),
            "searchlivestruct".to_owned(),
        ]
    );
    assert_eq!(successor.relation, CandidateAssessmentRelation::Supports);
    assert!(
        !successor
            .paths
            .iter()
            .any(|path| matches!(path, CandidateAssessmentPath::SafetyDiagnostic { .. })),
        "the surviving Context is safe: {:?}",
        successor.paths
    );

    // The retired Context is still retrieved and still explained, and holds no strong path.
    let retired = assessment_for(&result, fixture.retired);
    assert!(
        shared_identifiers(retired).is_empty(),
        "a retired Context never enters the strong identifier channel: {:?}",
        retired.paths
    );
    assert_eq!(
        retired.relation,
        CandidateAssessmentRelation::UnresolvedRelated
    );
    let reason = retired
        .paths
        .iter()
        .find_map(|path| match path {
            CandidateAssessmentPath::SafetyDiagnostic { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("a retired target carries a safety diagnostic");
    assert!(
        reason.contains("superseded by")
            && reason.contains(&fixture.successor.context_id.to_string()),
        "the diagnostic names the Context that replaced it: {reason}"
    );
    assert_ne!(
        result.candidate_status,
        sctx_domain::AutomaticCandidateStatus::ExactDuplicateReview,
        "nothing may be a duplicate of a conclusion the knowledge base retired"
    );
}

fn publish(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
    previous_publication_ids: Vec<PublicationId>,
) -> PublicationId {
    let event = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids,
            action: PublicationAction::Publish,
            revision_id,
            review_event_ids: Vec::new(),
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextPublicationChanged { publication, .. } = event.payload() else {
        unreachable!()
    };
    let publication_id = publication.publication_id;
    append(store, event);
    publication_id
}

fn revise(
    store: &GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    parents: Vec<RevisionId>,
    content: ContextRevisionDraft,
) -> RevisionId {
    let event = Event::context_revised(space_id, context_id, parents, content, None).unwrap();
    let EventPayload::ContextRevisionAdded { revision, .. } = event.payload() else {
        unreachable!()
    };
    let revision_id = revision.revision_id;
    append(store, event);
    revision_id
}

/// One Context is one retrieval target, whatever shape its revision DAG has taken.
///
/// Every node of the DAG used to become its own target, so a Context competed with its own
/// history for the `top_k` slots and a reviewer saw the same conclusion twice — observed on the
/// real installation as `ctx_68b87734` entering as both its accepted revision and a superseded
/// one. Governance, not the DAG head, names the revision the knowledge base holds: an unpublished
/// newer draft is a head too, and letting it displace the accepted revision would hide the
/// accepted fact from duplicate review entirely.
#[test]
fn one_context_contributes_one_target_whatever_shape_its_revision_dag_has() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("revision dag analysis root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let (space_id, _) = add_space(&store, "Revision DAG", "revisiondag");
    let event = Event::context_revision_added(
        space_id,
        draft(
            None,
            "The first accepted wording of one conclusion",
            "First rationale",
            "dag-domain",
        ),
        None,
    )
    .unwrap();
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision,
        ..
    } = event.payload()
    else {
        unreachable!()
    };
    let context_id = *context_id;
    let first_revision = revision.revision_id;
    append(&store, event);
    let first_publication = publish(&store, space_id, context_id, first_revision, Vec::new());
    let accepted = revise(
        &store,
        space_id,
        context_id,
        vec![first_revision],
        draft(
            None,
            "The accepted wording of one conclusion",
            "Accepted rationale",
            "dag-domain",
        ),
    );
    publish(
        &store,
        space_id,
        context_id,
        accepted,
        vec![first_publication],
    );
    // An unpublished draft revision: a DAG head that governance has not accepted.
    revise(
        &store,
        space_id,
        context_id,
        vec![accepted],
        draft(
            None,
            "An unpublished later wording of one conclusion",
            "Draft rationale",
            "dag-domain",
        ),
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();

    let candidate = candidate(draft(
        None,
        "An independent claim about the same domain",
        "Independent rationale",
        "dag-domain",
    ));
    let result = SearchEngine::new(index)
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 8_000,
            top_k: 8,
        })
        .unwrap();
    let targets = result
        .analysis
        .assessments
        .iter()
        .filter_map(|assessment| assessment.target)
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![ContextRevisionRef {
            context_id,
            revision_id: accepted,
        }],
        "only the revision governance accepted is assessed"
    );
}

/// The five set channels have no order of their own, so they must not be given a random one.
///
/// `canonical`, `statement`, `topic`, `scope` and `identifier` are built by walking the target
/// map, which is keyed by two `Uuid::new_v4` values. Handing that walk to the ranking function
/// spent each channel's whole RRF spread on sixteen random bytes — measured on the real
/// installation, `scope`'s random swing exceeded the full spread of `bm25`, the one channel whose
/// order means something. Here the weaker overlap deliberately carries the lower Context ID.
#[test]
fn a_set_channels_rank_follows_its_measurement_and_never_the_context_id() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("scope rank analysis root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let (space_id, _) = add_space(&store, "Scope Ranking", "scoperanking");
    let mut narrow_content = draft(
        None,
        "scope overlap lane alpha only",
        "One shared applicability domain",
        "alpha",
    );
    narrow_content.applicability.domains = vec!["alpha".to_owned()];
    let narrow = add_context_with_id(
        &store,
        space_id,
        narrow_content,
        "ctx_00000000-0000-4000-8000-000000000001",
    );
    let mut wide_content = draft(
        None,
        "scope overlap lanes alpha beta gamma",
        "Three shared applicability domains",
        "alpha",
    );
    wide_content.applicability.domains =
        vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
    let wide = add_context_with_id(
        &store,
        space_id,
        wide_content,
        "ctx_ffffffff-ffff-4fff-bfff-ffffffffffff",
    );
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();

    let mut claim = draft(
        None,
        "budget ceiling ordering probe",
        "Nothing lexical connects this claim to either target",
        "alpha",
    );
    claim.applicability.domains = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
    let candidate = candidate(claim);
    let result = SearchEngine::new(index)
        .analyze_candidate(&CandidateAnalysisRequest {
            has_blocking_unknowns: false,
            source_task_id: candidate.source_episode.task_id,
            source_intent_revision_id: TaskIntentRevisionId::new(),
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
            proposed_space_group_space_id: None,
            token_budget: 8_000,
            top_k: 8,
        })
        .unwrap();
    assert_eq!(result.analysis.assessments.len(), 2);
    for assessment in &result.analysis.assessments {
        assert!(
            assessment
                .paths
                .iter()
                .all(|path| matches!(path, CandidateAssessmentPath::ScopeOverlap { .. })),
            "only the scope channel may affect this fixture's rank: {:?}",
            assessment.paths
        );
    }
    assert_eq!(
        result.analysis.assessments[0].target,
        Some(wide),
        "three overlapping domains outrank one, although this target holds the highest Context ID"
    );
    assert_eq!(result.analysis.assessments[1].target, Some(narrow));
}
