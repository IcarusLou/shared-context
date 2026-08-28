use sctx_domain::{
    CandidateAnalysisStatus, CandidateReviewStatus, CandidateSpaceRecommendation, ContextKind,
    EvidenceType, IntentSnapshot, OptionalCandidateEdits, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_mcp::{
    CandidateConfirmInput, CandidateConfirmPrimaryInput, CandidateConfirmResponseStatus,
    CandidateGetInput, CandidateListInput, ExistingCandidatePrimaryInput, ExpectedRevisionId,
    NewCandidatePrimaryInput, TaskBoundary, TaskCheckpointClaimInput, TaskCheckpointEvidenceInput,
    TaskCheckpointInput, TaskContextReadInput, TaskIntentUpdateInput, candidate_confirm_at_root,
    candidate_get_at_root, candidate_list_at_root, task_checkpoint_at_root,
    task_context_readonly_at_root, task_intent_update_at_root,
};
use sctx_search::{ContextStatus, ScopeFilter, SearchEngine, SearchFilters, SearchRequest};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Oracle {
    version: u64,
    existing_session: String,
    new_session: String,
    working_intent: WorkingIntentSnapshot,
    existing_claim: Claim,
    new_claim: Claim,
    expected: Expected,
}

#[derive(Deserialize)]
struct Claim {
    statement: String,
    rationale: String,
    supports: String,
    limitations: Vec<String>,
}

#[derive(Deserialize)]
struct Expected {
    builder_items: usize,
    pending_reviews: usize,
    existing_confirmation_events: usize,
    new_confirmation_events: usize,
    accepted_query: String,
}

fn checkpoint_claim(claim: &Claim) -> TaskCheckpointClaimInput {
    TaskCheckpointClaimInput {
        context_kind: ContextKind::Contract,
        statement: claim.statement.clone(),
        rationale: claim.rationale.clone(),
        conditions: Vec::new(),
        evidence: vec![TaskCheckpointEvidenceInput {
            evidence_type: EvidenceType::ExperimentRecord,
            summary: claim.supports.clone(),
            limitations: claim.limitations.clone(),
        }],
    }
}

fn build_review(
    root: &std::path::Path,
    agent_kind: &str,
    session: &str,
    intent: &WorkingIntentSnapshot,
    claim: &Claim,
    expected_builder_items: usize,
) -> (
    sctx_mcp::TaskIntentUpdateResponse,
    sctx_domain::CandidateId,
    sctx_domain::WorkEpisodeId,
) {
    let task = task_intent_update_at_root(
        root,
        &TaskIntentUpdateInput {
            agent_kind: agent_kind.to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: intent.clone(),
        },
    )
    .unwrap();
    let closed = task_checkpoint_at_root(
        root,
        &TaskCheckpointInput {
            agent_kind: agent_kind.to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![checkpoint_claim(claim)],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("nonempty Checkpoint must be accepted");
    assert_eq!(
        closed.candidate_build.status,
        sctx_mcp::CandidateBuildResponseStatus::Pending
    );
    let recovered = candidate_list_at_root(
        root,
        &CandidateListInput {
            agent_kind: agent_kind.to_owned(),
            external_session_id: session.to_owned(),
            status: CandidateReviewStatus::Pending,
            limit: 10,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert_eq!(recovered.reviews.len(), expected_builder_items);
    (task, recovered.reviews[0].0.candidate_id, closed.episode_id)
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_milestone_four_builder_review_confirm_oracle() {
    let oracle: Oracle =
        serde_json::from_str(include_str!("../../../fixtures/m4/fixed-oracle.json")).unwrap();
    assert_eq!(oracle.version, 1);
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("m4 fixed oracle");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let space = Event::space_created(
        IntentSnapshot {
            title: "Shared result contract".to_owned(),
            problem: "Cross-client response semantics need one governed home".to_owned(),
            desired_outcome: "FE, iOS, and Android share the accepted contract".to_owned(),
            in_scope: vec!["search-v2-endpoint".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["all three clients agree".to_owned()],
            domain_terms: vec!["SearchResultRenderer".to_owned()],
        },
        None,
    )
    .unwrap();
    let EventPayload::SpaceCreated { space_id, .. } = space.payload() else {
        unreachable!()
    };
    let existing_space_id = *space_id;
    store.append_event(AppendRequest::event(space)).unwrap();

    let (existing_task, existing_candidate, existing_episode) = build_review(
        &root,
        "codex",
        &oracle.existing_session,
        &oracle.working_intent,
        &oracle.existing_claim,
        oracle.expected.builder_items,
    );
    let list = candidate_list_at_root(
        &root,
        &CandidateListInput {
            agent_kind: "codex".to_owned(),
            external_session_id: oracle.existing_session.clone(),
            status: CandidateReviewStatus::Pending,
            limit: 10,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert_eq!(list.reviews.len(), oracle.expected.pending_reviews);
    assert!(list.next_cursor.is_none());
    let review = candidate_get_at_root(
        &root,
        &CandidateGetInput {
            agent_kind: "codex".to_owned(),
            external_session_id: oracle.existing_session.clone(),
            candidate_id: existing_candidate.to_string(),
        },
    )
    .unwrap();
    assert_eq!(review.source_episode.episode_id, existing_episode);
    assert_eq!(review.source_episode.task_id, existing_task.context.task_id);
    assert_eq!(review.content.statement, oracle.existing_claim.statement);
    assert_eq!(review.content.rationale, oracle.existing_claim.rationale);
    assert_eq!(
        review.content.evidence[0].supports,
        oracle.existing_claim.statement
    );
    assert_eq!(
        review.content.evidence[0].content,
        json!({"summary": oracle.existing_claim.supports})
    );
    assert_eq!(review.analysis.status, CandidateAnalysisStatus::Complete);
    assert!(review.ready_for_review && review.untrusted_data);
    assert!(!review.analysis.assessments.is_empty());
    assert!(!review.space_recommendations.is_empty());
    let pack = task_context_readonly_at_root(
        &root,
        &TaskContextReadInput {
            agent_kind: "codex".to_owned(),
            external_session_id: oracle.existing_session.clone(),
            token_budget: 2_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    assert!(
        !serde_json::to_string(&pack)
            .unwrap()
            .contains(&existing_candidate.to_string())
    );

    let existing_input = CandidateConfirmInput {
        agent_kind: "codex".to_owned(),
        external_session_id: oracle.existing_session.clone(),
        expected_task_id: existing_task.context.task_id.to_string(),
        expected_intent_revision_id: existing_task.context.intent_revision_id.to_string(),
        candidate_id: existing_candidate.to_string(),
        expected_review_version: review.review_version,
        primary: CandidateConfirmPrimaryInput::Existing(ExistingCandidatePrimaryInput {
            existing_space_id: existing_space_id.to_string(),
        }),
        related_space_ids: Vec::new(),
        edits: OptionalCandidateEdits::default(),
    };
    let confirmed = candidate_confirm_at_root(&root, &existing_input).unwrap();
    assert_eq!(confirmed.status, CandidateConfirmResponseStatus::Confirmed);
    assert_eq!(
        confirmed.event_ids.len(),
        oracle.expected.existing_confirmation_events
    );
    let retry = candidate_confirm_at_root(&root, &existing_input).unwrap();
    assert_eq!(
        retry.status,
        CandidateConfirmResponseStatus::AlreadyConfirmed
    );
    assert_eq!(retry.confirmation_id, confirmed.confirmation_id);
    let search = SearchEngine::new(sctx_index::ProjectionIndex::for_store(&store))
        .search(&SearchRequest {
            query: oracle.expected.accepted_query,
            filters: SearchFilters {
                statuses: vec![ContextStatus::Accepted],
                scope: ScopeFilter::default(),
                ..SearchFilters::default()
            },
            page_size: 10,
            cursor: None,
        })
        .unwrap();
    assert!(
        search
            .results
            .iter()
            .any(|item| item.context_id == confirmed.context_id)
    );

    let mut novel_intent = oracle.working_intent.clone();
    novel_intent.goal = "Verify a novel offline reconciliation rule".to_owned();
    let (new_task, new_candidate, _) = build_review(
        &root,
        "cursor",
        &oracle.new_session,
        &novel_intent,
        &oracle.new_claim,
        oracle.expected.builder_items,
    );
    assert_ne!(new_task.context.task_id, existing_task.context.task_id);
    let new_review = candidate_get_at_root(
        &root,
        &CandidateGetInput {
            agent_kind: "cursor".to_owned(),
            external_session_id: oracle.new_session.clone(),
            candidate_id: new_candidate.to_string(),
        },
    )
    .unwrap();
    let recommendation_id = new_review
        .space_recommendations
        .iter()
        .find_map(|recommendation| match recommendation {
            CandidateSpaceRecommendation::ProposedNewSpaceIntent {
                recommendation_id, ..
            } => Some(*recommendation_id),
            CandidateSpaceRecommendation::Existing { .. } => None,
        })
        .unwrap();
    let new_confirmed = candidate_confirm_at_root(
        &root,
        &CandidateConfirmInput {
            agent_kind: "cursor".to_owned(),
            external_session_id: oracle.new_session,
            expected_task_id: new_task.context.task_id.to_string(),
            expected_intent_revision_id: new_task.context.intent_revision_id.to_string(),
            candidate_id: new_candidate.to_string(),
            expected_review_version: new_review.review_version,
            primary: CandidateConfirmPrimaryInput::Proposed(NewCandidatePrimaryInput {
                new_space_recommendation_id: recommendation_id.to_string(),
            }),
            related_space_ids: Vec::new(),
            edits: OptionalCandidateEdits::default(),
        },
    )
    .unwrap();
    assert_eq!(
        new_confirmed.status,
        CandidateConfirmResponseStatus::Confirmed
    );
    assert_eq!(
        new_confirmed.event_ids.len(),
        oracle.expected.new_confirmation_events
    );
    assert_ne!(new_confirmed.primary_space_id, existing_space_id);
}
