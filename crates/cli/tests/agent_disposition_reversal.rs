//! Reversing automatic acceptances in batch, and counting what was decided by whom.
//!
//! ADR-0005 makes automatic confirmation acceptable only because it is reversible: the human
//! reversal rate is the single input for widening or narrowing the permission surface, so the
//! reversal has to be one command over a selector, not a hunt through the Space tree. These tests
//! drive the real binary and pin that the selector reaches exactly the Contexts it names, that the
//! reversal is an ordinary append, and that a human acceptance is never caught by it.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use sctx_domain::{
    AutomaticCandidateStatus, CandidateAssessmentPath, CandidateAssessmentRelation,
    CandidateConfidence, CandidateId, CandidateRelationAssessment, ContextGovernanceStatus,
    ContextKind, DecisionSource, EvidenceType, IntentSnapshot, SpaceId, WorkingIntentSnapshot,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_mcp::{
    CandidateConfirmInput, CandidateConfirmPrimaryInput, CandidateListInput,
    ExistingCandidatePrimaryInput, ExpectedRevisionId, TaskBoundary, TaskCheckpointClaimInput,
    TaskCheckpointEvidenceInput, TaskCheckpointInput, TaskIntentUpdateInput,
    candidate_confirm_at_root, candidate_list_at_root, task_checkpoint_at_root,
    task_intent_update_at_root,
};
use sctx_task_runtime::TaskRuntime;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    space_id: SpaceId,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let root = home.join(".shared-context");
        let store = GitStore::bootstrap_local(&root).unwrap();
        let created = Event::space_created(
            IntentSnapshot {
                title: "Automatic disposition reversal".to_owned(),
                problem: "automatic acceptances must be reversible in batch".to_owned(),
                desired_outcome: "one selector reverses one session's automatic work".to_owned(),
                in_scope: vec!["candidate confirmation".to_owned()],
                out_of_scope: Vec::new(),
                acceptance_conditions: vec!["the reversal is an ordinary append".to_owned()],
                domain_terms: Vec::new(),
            },
            None,
        )
        .unwrap();
        let EventPayload::SpaceCreated { space_id, .. } = created.payload() else {
            unreachable!()
        };
        let space_id = *space_id;
        store.append_event(AppendRequest::event(created)).unwrap();
        ProjectionIndex::for_store(&store).synchronize().unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            space_id,
        }
    }

    fn run(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("sctx should start");
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn intent(goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: Some(format!("Deliver {goal}")),
        in_scope: vec![goal.to_owned()],
        out_of_scope: Vec::new(),
        domains: vec!["cli".to_owned()],
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec![format!("{goal} is recorded")],
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

/// Builds one Candidate and drops it inside the automatic permission surface.
fn permitted_candidate(
    harness: &Harness,
    session: &str,
    statement: &str,
) -> (sctx_mcp::TaskIntentUpdateResponse, CandidateId) {
    let task = task_intent_update_at_root(
        &harness.root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: intent(statement),
        },
    )
    .unwrap();
    task_checkpoint_at_root(
        &harness.root,
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            claims: vec![TaskCheckpointClaimInput {
                context_kind: ContextKind::Decision,
                statement: statement.to_owned(),
                rationale: "Automatic disposition reversal fixture".to_owned(),
                conditions: Vec::new(),
                evidence: vec![TaskCheckpointEvidenceInput {
                    evidence_type: EvidenceType::ExperimentRecord,
                    summary: format!("The reversal fixture produced {statement}"),
                    limitations: vec!["local fixture".to_owned()],
                }],
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap()
    .into_accepted()
    .expect("a nonempty Checkpoint is accepted");

    let list = candidate_list_at_root(
        &harness.root,
        &CandidateListInput {
            scope: sctx_domain::CandidateReviewScope::Task,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: sctx_domain::CandidateReviewStatus::Pending,
            limit: 100,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert_eq!(list.reviews.len(), 1);
    let candidate_id = list.reviews[0].0.candidate_id;

    // The gate reads the analyzer's own verdict; writing the verdict the analyzer would have
    // reached keeps this a test of the reversal path, not of retrieval.
    let tasks = TaskRuntime::initialize(&harness.root).unwrap();
    let mut view = tasks
        .read_candidate_analysis(candidate_id)
        .unwrap()
        .unwrap();
    view.candidate.analysis.assessments = vec![CandidateRelationAssessment {
        relation: CandidateAssessmentRelation::Novel,
        target: None,
        confidence: CandidateConfidence {
            basis_points: 9_000,
            rationale: "fixed review surface".to_owned(),
        },
        paths: vec![CandidateAssessmentPath::NoSufficientCandidate],
        reasons: vec!["fixed review surface".to_owned()],
    }];
    view.candidate.status = AutomaticCandidateStatus::ReadyForReview;
    tasks.replace_candidate_analysis(&view.candidate).unwrap();
    (task, candidate_id)
}

fn confirm(
    harness: &Harness,
    session: &str,
    statement: &str,
    decision_source: DecisionSource,
) -> sctx_domain::ContextId {
    let (task, candidate_id) = permitted_candidate(harness, session, statement);
    candidate_confirm_at_root(
        &harness.root,
        &CandidateConfirmInput {
            decision_source,
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            candidate_id: candidate_id.to_string(),
            expected_review_version: 1,
            primary: CandidateConfirmPrimaryInput::Existing(ExistingCandidatePrimaryInput {
                existing_space_id: harness.space_id.to_string(),
            }),
            related_space_ids: Vec::new(),
            edits: sctx_domain::OptionalCandidateEdits::default(),
        },
    )
    .unwrap()
    .context_id
}

fn governance(root: &Path, context_id: sctx_domain::ContextId) -> ContextGovernanceStatus {
    let store = GitStore::open_existing(root).unwrap();
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    index
        .domain_snapshot()
        .unwrap()
        .projection
        .spaces
        .values()
        .find_map(|space| space.contexts.get(&context_id))
        .expect("the confirmed Context is projected")
        .governance
        .clone()
}

/// The selector reverses every automatic acceptance and leaves the human one accepted.
#[test]
fn withdraw_by_decision_source_reverses_only_the_automatic_acceptances() {
    let harness = Harness::new();
    let automatic_one = confirm(
        &harness,
        "reversal-agent-one",
        "First automatic acceptance",
        DecisionSource::AgentPolicy,
    );
    let automatic_two = confirm(
        &harness,
        "reversal-agent-two",
        "Second automatic acceptance",
        DecisionSource::AgentPolicy,
    );
    let human = confirm(
        &harness,
        "reversal-human",
        "A person accepted this one",
        DecisionSource::Human,
    );

    let stats = harness.run(&["candidate", "stats"]);
    assert_eq!(stats["data"]["agent_policy"]["confirmed"], 2);
    assert_eq!(stats["data"]["human"]["confirmed"], 1);
    assert_eq!(stats["data"]["auto_confirm_not_permitted"], 0);

    // A dry run names the whole selection and writes nothing.
    let planned = harness.run(&[
        "context",
        "withdraw",
        "--decision-source",
        "agent_policy",
        "--dry-run",
    ]);
    assert_eq!(planned["data"]["dry_run"], true);
    assert_eq!(planned["data"]["selected"], 2);
    let planned_ids = planned["data"]["planned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["context_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(planned_ids.contains(&automatic_one.to_string()));
    assert!(planned_ids.contains(&automatic_two.to_string()));
    assert!(!planned_ids.contains(&human.to_string()));
    assert!(matches!(
        governance(&harness.root, automatic_one),
        ContextGovernanceStatus::Accepted { .. }
    ));

    // The real run withdraws each one through the ordinary Publication path.
    let withdrawn = harness.run(&["context", "withdraw", "--decision-source", "agent_policy"]);
    assert_eq!(withdrawn["data"]["dry_run"], false);
    let entries = withdrawn["data"]["withdrawn"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert!(entry["event_id"].as_str().unwrap().starts_with("evt_"));
        assert!(!entry["commit_oid"].as_str().unwrap().is_empty());
    }

    for reversed in [automatic_one, automatic_two] {
        assert!(
            !matches!(
                governance(&harness.root, reversed),
                ContextGovernanceStatus::Accepted { .. }
            ),
            "an automatically accepted Context is no longer accepted after the reversal"
        );
    }
    assert!(
        matches!(
            governance(&harness.root, human),
            ContextGovernanceStatus::Accepted { .. }
        ),
        "a human acceptance is never caught by the automatic selector"
    );

    // Re-running finds nothing left to withdraw and stays a no-op.
    let again = harness.run(&["context", "withdraw", "--decision-source", "agent_policy"]);
    assert_eq!(again["data"]["selected"], 2);
    assert!(again["data"]["withdrawn"].as_array().unwrap().is_empty());
    assert_eq!(again["data"]["skipped"].as_array().unwrap().len(), 2);
}

/// The session selector narrows the reversal to one originating session.
#[test]
fn withdraw_by_external_session_reverses_one_session_only() {
    let harness = Harness::new();
    let first = confirm(
        &harness,
        "session-scoped-one",
        "Accepted by the session under reversal",
        DecisionSource::AgentPolicy,
    );
    let second = confirm(
        &harness,
        "session-scoped-two",
        "Accepted by a session left alone",
        DecisionSource::AgentPolicy,
    );

    let tasks = TaskRuntime::initialize(&harness.root).unwrap();
    let target = tasks
        .list_confirmed_dispositions(Some(DecisionSource::AgentPolicy), None)
        .unwrap()
        .into_iter()
        .find(|disposition| disposition.result_context_id == first)
        .expect("the first acceptance is recorded")
        .external_session_id;

    let planned = harness.run(&[
        "context",
        "withdraw",
        "--decision-source",
        "agent_policy",
        "--external-session",
        &target.to_string(),
        "--dry-run",
    ]);
    assert_eq!(planned["data"]["selected"], 1);
    assert_eq!(
        planned["data"]["planned"][0]["context_id"],
        first.to_string()
    );

    harness.run(&[
        "context",
        "withdraw",
        "--decision-source",
        "agent_policy",
        "--external-session",
        &target.to_string(),
    ]);
    assert!(!matches!(
        governance(&harness.root, first),
        ContextGovernanceStatus::Accepted { .. }
    ));
    assert!(
        matches!(
            governance(&harness.root, second),
            ContextGovernanceStatus::Accepted { .. }
        ),
        "another session's acceptance is outside the selector"
    );
}
