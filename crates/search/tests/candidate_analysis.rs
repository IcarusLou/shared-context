use std::{fs, process::Command, sync::Arc};

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactRef,
    CandidateAssessmentPath, CandidateAssessmentRelation, CandidateSpaceRecommendation,
    ContextCandidate, ContextKind, ContextRelation, ContextRelationKind, ContextRevisionDraft,
    ContextRevisionRef, EngineeringArtifact, EngineeringReference, EvidenceSnapshotDraft,
    EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft, RecommendedSpaceRole,
    ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId, RepositoryIdentity, RevisionId,
    SpaceId, SubmissionId, TaskId, TaskIntentRevisionId, TaskSessionId, WorkEpisodeId,
    WorkEpisodeRef, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, ProjectedEngineeringReference, RepositoryScanOutcome,
    RepositorySnapshot, SnapshotArtifact, SnapshotSourcePolicy, SourceLanguage,
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
    let contract = add_context(
        &store,
        contract_space,
        draft(
            Some("candidate/graph-contract"),
            "Server contract is reached across the Graph",
            "Cross-end contract rationale",
            "server-graph-domain",
        ),
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
    let source = add_context(&store, source_space, source_draft);
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
