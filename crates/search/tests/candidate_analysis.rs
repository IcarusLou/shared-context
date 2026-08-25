use std::{fs, process::Command, sync::Arc};

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactRef,
    CandidateAssessmentPath, CandidateAssessmentRelation, CandidateSpaceRecommendation,
    ContextCandidate, ContextKind, ContextRelation, ContextRelationKind, ContextRevisionDraft,
    ContextRevisionRef, EngineeringArtifact, EngineeringReference, EvidenceSnapshotDraft,
    EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft, RecommendedSpaceRole,
    ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId, RepositoryIdentity, RevisionId,
    SpaceId, SubmissionId, TaskId, TaskSessionId, WorkEpisodeId, WorkEpisodeRef,
    WorkingIntentSnapshot,
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
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
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
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: Vec::new(),
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
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: explicit,
            artifact_refs: Vec::new(),
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
    let candidate = candidate(content);
    let result = SearchEngine::with_engineering_graph(index, graph_store)
        .analyze_candidate(&CandidateAnalysisRequest {
            source_task_id: candidate.source_episode.task_id,
            source_working_intent: source_intent(candidate.source_episode.task_id),
            source_task_signals: Vec::new(),
            candidate,
            explicit_related_contexts: Vec::new(),
            artifact_refs: vec![ArtifactRef {
                repository_id: repository.repository_id.clone(),
                locator: symbol_locator(),
            }],
            token_budget: 8_000,
            top_k: 16,
        })
        .unwrap();
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
}
