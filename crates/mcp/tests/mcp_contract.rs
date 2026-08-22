use std::{
    fs,
    io::{BufReader, Cursor},
    path::Path,
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use rusqlite::Connection;
use sctx_domain::{
    Applicability, ArtifactAction, ArtifactLocator, ArtifactRef, CandidateId,
    CandidateReviewDiagnostic, CandidateReviewStatus, CaptureId, CaptureUnknown, ContextId,
    ContextKind, ContextRevisionDraft, ContextRevisionRef, ContextUseDisposition, EventId,
    EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, NormalizedBreadcrumbKind,
    NormalizedWorkObservation, PublicationAction, PublicationDraft, RepoRelativePath, RepositoryId,
    RevisionId, SpaceId, SubmissionId, TaskId, TaskIntentDraft, TaskIntentRevisionId, TaskSignal,
    TaskSignalKind, WorkEpisodeId, WorkSourceRef, candidate_submission_content_hash,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, CandidateSubmissionRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_local_state::UserConfigStore;
use sctx_mcp::{
    CandidateBuildItemResponseStatus, CandidateBuildResponseStatus, CandidateDiscardInput,
    CandidateDiscardResponseStatus, CandidateGetInput, CandidateListInput, ClientKind,
    DisconnectReason, ExpectedRevisionId, IntentMaturity, McpServer, TaskBoundary,
    TaskCheckpointBoundary, TaskCheckpointClaimInput, TaskCheckpointEvidenceInput,
    TaskCheckpointInput, TaskContextReadInput, TaskIntentUpdateInput, TaskSignalSupersedeInput,
    TransportErrorKind, build_closed_episode_at_root, candidate_discard_at_root,
    candidate_get_at_root, candidate_list_at_root, task_checkpoint_at_root,
    task_context_readonly_at_root, task_intent_update_at_root, task_signal_supersede_at_root,
};
use sctx_task_runtime::{
    AgentCheckpointWrite, CandidateBuildItemPreparation, CandidateBuildItemStatus,
    CaptureIngestion, CheckpointBoundary, CheckpointClaimDraft, TaskRuntime,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum FixtureFraming {
    Newline,
    ContentLength,
}

struct Fixture {
    _temporary: TempDir,
    root: std::path::PathBuf,
    store: GitStore,
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("shared context root");
        let store = GitStore::initialize(&root).unwrap();
        let created = Event::space_created(intent(), None).unwrap();
        let space_id = match created.payload() {
            EventPayload::SpaceCreated { space_id, .. } => *space_id,
            _ => unreachable!(),
        };
        append(&store, created);
        let revision_added =
            Event::context_revision_added(space_id, draft("stdio MCP contract"), None).unwrap();
        let (context_id, revision_id) = context_identity(&revision_added);
        append(&store, revision_added);
        append(
            &store,
            Event::publication_changed(
                space_id,
                context_id,
                PublicationDraft {
                    previous_publication_ids: Vec::new(),
                    action: PublicationAction::Publish,
                    revision_id,
                    review_event_ids: Vec::new(),
                },
                None,
            )
            .unwrap(),
        );
        Self {
            _temporary: temporary,
            root,
            store,
            space_id,
            context_id,
            revision_id,
        }
    }

    fn server(&self, client: ClientKind) -> McpServer {
        McpServer::new(&self.root, client).unwrap()
    }
}

fn intent() -> IntentSnapshot {
    IntentSnapshot {
        title: "MCP Contract".to_owned(),
        problem: "agents need deterministic context".to_owned(),
        desired_outcome: "both clients share one protocol".to_owned(),
        in_scope: vec!["MCP".to_owned()],
        out_of_scope: vec!["adapter installation".to_owned()],
        acceptance_conditions: vec!["fixtures pass".to_owned()],
        domain_terms: vec!["QuerySnapshot".to_owned()],
    }
}

fn draft(statement: &str) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: ContextKind::Decision,
        topic_key: Some("mcp/transport".to_owned()),
        statement: statement.to_owned(),
        rationale: "Cursor and Codex need one stable boundary".to_owned(),
        applicability: Applicability {
            domains: vec!["mcp".to_owned()],
            platforms: vec!["macos".to_owned()],
            conditions: vec!["stdio".to_owned()],
        },
        assumptions: vec!["the committed Git tree is readable".to_owned()],
        recheck_when: vec!["the MCP protocol changes".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the client fixture completed".to_owned(),
            content: json!({"request": "tools/call", "actual": "success"}),
            interpretation: "the protocol contract is executable".to_owned(),
            limitations: vec!["local fixture".to_owned()],
        }],
    }
}

struct CandidateOwner {
    agent_kind: String,
    external_session_id: String,
    task_id: TaskId,
    intent_revision_id: TaskIntentRevisionId,
    source_episode_id: WorkEpisodeId,
}

fn closed_candidate_owner(fixture: &Fixture, agent_kind: &str, session: &str) -> CandidateOwner {
    let task = task_intent_update_at_root(
        &fixture.root,
        &TaskIntentUpdateInput {
            agent_kind: agent_kind.to_owned(),
            external_session_id: session.to_owned(),
            ..update_input(
                session,
                TaskBoundary::New,
                None,
                IntentMaturity::Provisional,
                "create a verified Candidate",
            )
        },
    )
    .unwrap();
    let closed = task_checkpoint_at_root(
        &fixture.root,
        &TaskCheckpointInput {
            agent_kind: agent_kind.to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            expected_episode_version: 0,
            boundary: TaskCheckpointBoundary::Close,
            claims: Vec::new(),
            unknowns: vec![CaptureUnknown {
                statement: "Candidate confirmation remains outside this operation".to_owned(),
                blocking: false,
                recheck_when: Vec::new(),
            }],
        },
    )
    .unwrap();
    CandidateOwner {
        agent_kind: agent_kind.to_owned(),
        external_session_id: session.to_owned(),
        task_id: task.context.task_id,
        intent_revision_id: task.context.intent_revision_id,
        source_episode_id: closed.episode_id,
    }
}

fn candidate_arguments(
    submission_id: SubmissionId,
    owner: &CandidateOwner,
    statement: &str,
) -> Value {
    json!({
        "submission_id": submission_id,
        "agent_kind": owner.agent_kind,
        "external_session_id": owner.external_session_id,
        "expected_task_id": owner.task_id,
        "expected_intent_revision_id": owner.intent_revision_id,
        "source_episode_id": owner.source_episode_id,
        "kind": "decision",
        "topic_key": "mcp/candidate",
        "statement": statement,
        "rationale": "candidate IDs and paths remain server-owned",
        "applicability": {"domains": ["mcp"], "platforms": ["macos"], "conditions": ["stdio"]},
        "assumptions": [],
        "recheck_when": ["the Writer contract changes"],
        "evidence": [{
            "kind": "experiment_record",
            "supports": "the Candidate fixture called the Writer",
            "content": {"fixture": "candidate_create", "actual": "candidate"},
            "interpretation": "the candidate was appended",
            "limitations": []
        }]
    })
}

fn directly_close_builder_episode(
    fixture: &Fixture,
    session: &str,
    statement: &str,
) -> (
    sctx_task_runtime::AgentCheckpointOutcome,
    ContextRevisionDraft,
) {
    let tasks = TaskRuntime::initialize(&fixture.root).unwrap();
    let locator = sctx_domain::ExternalSessionLocator::new("codex", session).unwrap();
    let task_id = TaskId::new();
    let intent = update_input(
        session,
        TaskBoundary::New,
        None,
        IntentMaturity::Provisional,
        "exercise Candidate Build crash recovery",
    )
    .intent
    .bind(task_id);
    let task = tasks
        .open_or_create(locator.clone(), intent, Vec::new())
        .unwrap()
        .snapshot;
    let evidence = EvidenceSnapshotDraft {
        kind: EvidenceType::ExperimentRecord,
        supports: statement.to_owned(),
        content: json!({"test": session, "actual": "passed"}),
        interpretation: "The deterministic Builder fixture passed".to_owned(),
        limitations: Vec::new(),
    };
    let rationale = "The persisted Checkpoint owns this exact Claim".to_owned();
    let closed = tasks
        .write_agent_checkpoint(&AgentCheckpointWrite {
            locator,
            expected_task_id: task.task_id,
            expected_intent_revision_id: task.current_intent_revision().unwrap().revision_id,
            expected_episode_version: 0,
            boundary: CheckpointBoundary::Close,
            claims: vec![CheckpointClaimDraft {
                context_kind_hint: Some(ContextKind::Validation),
                topic_key_hint: None,
                statement: statement.to_owned(),
                rationale: rationale.clone(),
                applicability: Applicability::default(),
                assumptions: Vec::new(),
                recheck_when: Vec::new(),
                evidence_refs: Vec::new(),
                inline_validations: vec![evidence.clone()],
                artifact_refs: Vec::new(),
                related_contexts: Vec::new(),
            }],
            unknowns: Vec::new(),
        })
        .unwrap();
    let content = ContextRevisionDraft {
        kind: ContextKind::Validation,
        topic_key: None,
        statement: statement.to_owned(),
        rationale,
        applicability: Applicability::default(),
        assumptions: Vec::new(),
        recheck_when: Vec::new(),
        relations: Vec::new(),
        evidence: vec![evidence],
    };
    (closed, content)
}

fn task_arguments(agent_kind: &str, external_session_id: &str) -> Value {
    json!({
        "agent_kind": agent_kind,
        "external_session_id": external_session_id,
        "token_budget": 2000
    })
}

fn update_input(
    external_session_id: &str,
    task_boundary: TaskBoundary,
    expected_revision_id: Option<String>,
    maturity: IntentMaturity,
    goal: &str,
) -> TaskIntentUpdateInput {
    TaskIntentUpdateInput {
        agent_kind: "codex".to_owned(),
        external_session_id: external_session_id.to_owned(),
        task_boundary,
        expected_revision_id: expected_revision_id
            .map_or(ExpectedRevisionId::Null(()), ExpectedRevisionId::Revision),
        maturity,
        intent: TaskIntentDraft {
            goal: goal.to_owned(),
            desired_change: format!("Deliver verified {goal}"),
            in_scope: vec!["MCP".to_owned()],
            out_of_scope: vec![],
            domains: vec!["mcp".to_owned()],
            platforms: vec![],
            constraints: vec![],
            acceptance_conditions: vec!["The Task Context Pack is returned".to_owned()],
            artifacts: vec![],
            interfaces: vec![],
            unknowns: vec![],
        },
        evidence_refs: vec![],
    }
}

fn append(store: &GitStore, event: Event) {
    store
        .append_event(AppendRequest::event(event))
        .expect("append fixture event");
}

fn context_identity(event: &Event) -> (ContextId, RevisionId) {
    match event.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[allow(clippy::needless_pass_by_value)]
fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    request(
        id,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
}

fn run_session(server: &mut McpServer, framing: FixtureFraming, requests: &[Value]) -> Vec<Value> {
    let input = encode_frames(requests, framing);
    let mut reader = BufReader::new(Cursor::new(input));
    let mut output = Vec::new();
    let outcome = server.serve(&mut reader, &mut output).unwrap();
    assert_eq!(outcome.disconnect, DisconnectReason::CleanEof);
    assert_eq!(outcome.requests_handled, requests.len() as u64);
    decode_frames(&output, framing)
}

fn encode_frames(values: &[Value], framing: FixtureFraming) -> Vec<u8> {
    let mut output = Vec::new();
    for value in values {
        let body = serde_json::to_vec(value).unwrap();
        match framing {
            FixtureFraming::Newline => {
                output.extend_from_slice(&body);
                output.push(b'\n');
            }
            FixtureFraming::ContentLength => {
                output.extend_from_slice(
                    format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
                );
                output.extend_from_slice(&body);
            }
        }
    }
    output
}

fn decode_frames(bytes: &[u8], framing: FixtureFraming) -> Vec<Value> {
    match framing {
        FixtureFraming::Newline => bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect(),
        FixtureFraming::ContentLength => {
            let mut values = Vec::new();
            let mut remaining = bytes;
            while !remaining.is_empty() {
                let header_end = remaining
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap();
                let header = std::str::from_utf8(&remaining[..header_end]).unwrap();
                let length = header
                    .strip_prefix("Content-Length: ")
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                let body_start = header_end + 4;
                let body_end = body_start + length;
                values.push(serde_json::from_slice(&remaining[body_start..body_end]).unwrap());
                remaining = &remaining[body_end..];
            }
            values
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn checkpoint_arguments(
    agent_kind: &str,
    session: &str,
    task_id: &str,
    revision_id: &str,
    version: u64,
    boundary: &str,
    claims: Value,
    unknowns: Value,
) -> Value {
    json!({
        "agent_kind": agent_kind,
        "external_session_id": session,
        "expected_task_id": task_id,
        "expected_intent_revision_id": revision_id,
        "expected_episode_version": version,
        "boundary": boundary,
        "claims": claims,
        "unknowns": unknowns
    })
}

fn typed_checkpoint_input(
    session: &str,
    task_id: TaskId,
    revision_id: sctx_domain::TaskIntentRevisionId,
    version: u64,
    evidence: Vec<TaskCheckpointEvidenceInput>,
) -> TaskCheckpointInput {
    TaskCheckpointInput {
        agent_kind: "codex".to_owned(),
        external_session_id: session.to_owned(),
        expected_task_id: task_id.to_string(),
        expected_intent_revision_id: revision_id.to_string(),
        expected_episode_version: version,
        boundary: TaskCheckpointBoundary::Continue,
        claims: vec![TaskCheckpointClaimInput {
            context_kind_hint: None,
            topic_key_hint: None,
            statement: "Checkpoint validation is strict".to_owned(),
            rationale: "Invalid references and private content must fail before storage".to_owned(),
            applicability: Applicability {
                domains: vec!["mcp".to_owned()],
                platforms: Vec::new(),
                conditions: vec!["checkpoint".to_owned()],
            },
            assumptions: Vec::new(),
            recheck_when: vec!["the schema changes".to_owned()],
            evidence,
            artifact_refs: Vec::new(),
            related_contexts: Vec::new(),
        }],
        unknowns: Vec::new(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn checkpoint_rejects_dangling_private_stale_conflicting_and_forged_input_without_residue() {
    let fixture = Fixture::new();
    let session = "checkpoint-validation";
    let created = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            session,
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "validate checkpoint boundaries",
        ),
    )
    .unwrap();
    let task_id = created.context.task_id;
    let revision_id = created.context.intent_revision_id;
    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    let projection = ProjectionIndex::for_store(&fixture.store)
        .domain_snapshot()
        .unwrap();
    let context_revision = &projection.projection.spaces[&fixture.space_id].contexts
        [&fixture.context_id]
        .revisions[&fixture.revision_id]
        .revision;
    let evidence_id = context_revision.evidence[0].evidence_id;

    let repository = fixture.root.join("checkpoint repository");
    fs::create_dir_all(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    let repository = fs::canonicalize(repository).unwrap();
    let repository_id = UserConfigStore::initialize(&fixture.root)
        .unwrap()
        .add_repository(None, std::slice::from_ref(&repository))
        .unwrap()
        .repository
        .repository_id;

    let dangling = typed_checkpoint_input(
        session,
        task_id,
        revision_id,
        0,
        vec![TaskCheckpointEvidenceInput::ContextEvidence {
            context_id: ContextId::new().to_string(),
            revision_id: RevisionId::new().to_string(),
            evidence_id: sctx_domain::EvidenceId::new().to_string(),
        }],
    );
    assert_eq!(
        task_checkpoint_at_root(&fixture.root, &dangling)
            .unwrap_err()
            .kind(),
        sctx_domain::ErrorKind::InvalidInput
    );
    assert!(
        runtime
            .list_work_episodes(created.context.task_session_id, 10)
            .unwrap()
            .is_empty()
    );

    let mut private = typed_checkpoint_input(
        session,
        task_id,
        revision_id,
        0,
        vec![TaskCheckpointEvidenceInput::InlineValidation {
            evidence: EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "private validation".to_owned(),
                content: json!({"owner": "person@example.com"}),
                interpretation: "private content must not persist".to_owned(),
                limitations: Vec::new(),
            },
        }],
    );
    assert_eq!(
        task_checkpoint_at_root(&fixture.root, &private)
            .unwrap_err()
            .kind(),
        sctx_domain::ErrorKind::PrivacyRejected
    );
    private.claims[0].evidence = vec![
        TaskCheckpointEvidenceInput::InlineValidation {
            evidence: EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "public validation".to_owned(),
                content: json!({"actual": "passed"}),
                interpretation: "the safe result is self-contained".to_owned(),
                limitations: Vec::new(),
            },
        },
        TaskCheckpointEvidenceInput::ContextEvidence {
            context_id: fixture.context_id.to_string(),
            revision_id: fixture.revision_id.to_string(),
            evidence_id: evidence_id.to_string(),
        },
    ];
    private.claims[0].artifact_refs = vec![ArtifactRef {
        repository_id,
        locator: ArtifactLocator::File {
            path: RepoRelativePath::new("src/checkpoint.rs").unwrap(),
        },
    }];
    private.claims[0].related_contexts = vec![ContextRevisionRef {
        context_id: fixture.context_id,
        revision_id: fixture.revision_id,
    }];

    let mut unknown_repository = private.clone();
    unknown_repository.claims[0].artifact_refs[0].repository_id = RepositoryId::new();
    assert_eq!(
        task_checkpoint_at_root(&fixture.root, &unknown_repository)
            .unwrap_err()
            .kind(),
        sctx_domain::ErrorKind::InvalidInput
    );
    let mut artifact_only = private.clone();
    artifact_only.claims[0].evidence.clear();
    assert_eq!(
        task_checkpoint_at_root(&fixture.root, &artifact_only)
            .unwrap_err()
            .kind(),
        sctx_domain::ErrorKind::InvalidInput,
        "an Artifact association must not satisfy Claim Evidence"
    );
    let persisted = task_checkpoint_at_root(&fixture.root, &private).unwrap();
    assert!(persisted.created);

    let mut conflict = private.clone();
    conflict.claims[0].statement = "different content at the same parent".to_owned();
    assert_eq!(
        task_checkpoint_at_root(&fixture.root, &conflict)
            .unwrap_err()
            .kind(),
        sctx_domain::ErrorKind::Conflict
    );
    let stale = typed_checkpoint_input(session, task_id, revision_id, 9, Vec::new());
    assert_eq!(
        task_checkpoint_at_root(&fixture.root, &stale)
            .unwrap_err()
            .kind(),
        sctx_domain::ErrorKind::StaleState
    );

    let mut server = fixture.server(ClientKind::Codex);
    let mut forged = serde_json::to_value(&private).unwrap();
    forged["space_id"] = json!(SpaceId::new());
    let mut private_rpc = private.clone();
    private_rpc.claims[0].statement = "contact person@example.com".to_owned();
    let responses = run_session(
        &mut server,
        FixtureFraming::Newline,
        &[
            request(10, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(11, "task_checkpoint", forged),
            tool_call(
                12,
                "task_checkpoint",
                serde_json::to_value(conflict).unwrap(),
            ),
            tool_call(13, "task_checkpoint", serde_json::to_value(stale).unwrap()),
            tool_call(
                14,
                "task_checkpoint",
                serde_json::to_value(private_rpc).unwrap(),
            ),
        ],
    );
    assert_eq!(responses[1]["result"]["isError"], true);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["error"]["code"],
        "invalid_input"
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["error"]["code"],
        "checkpoint_conflict"
    );
    assert_eq!(
        responses[3]["result"]["structuredContent"]["error"]["code"],
        "checkpoint_stale"
    );
    assert_eq!(
        responses[4]["result"]["structuredContent"]["error"]["code"],
        "privacy_rejected"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn codex_and_cursor_checkpoint_inline_evidence_builds_only_after_close() {
    for (client, framing, agent_kind) in [
        (ClientKind::Cursor, FixtureFraming::Newline, "cursor"),
        (ClientKind::Codex, FixtureFraming::ContentLength, "codex"),
    ] {
        let fixture = Fixture::new();
        let before_events = event_count(fixture.store.repository());
        let session = format!("checkpoint-{agent_kind}");
        let mut server = fixture.server(client);
        let started = run_session(
            &mut server,
            framing,
            &[
                request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
                tool_call(
                    2,
                    "task_intent_update",
                    serde_json::to_value(TaskIntentUpdateInput {
                        agent_kind: agent_kind.to_owned(),
                        external_session_id: session.clone(),
                        ..update_input(
                            &session,
                            TaskBoundary::New,
                            None,
                            IntentMaturity::Provisional,
                            "record a verified checkpoint",
                        )
                    })
                    .unwrap(),
                ),
            ],
        );
        let task = &started[1]["result"]["structuredContent"];
        let task_id = task["task_id"].as_str().unwrap();
        let revision_id = task["intent_revision_id"].as_str().unwrap();
        let claims = json!([{
            "statement": "The public Checkpoint transaction completed",
            "rationale": "A real MCP client submitted self-contained validation",
            "applicability": {"domains": ["mcp"], "platforms": [], "conditions": [agent_kind]},
            "assumptions": [],
            "recheck_when": ["the Checkpoint contract changes"],
            "evidence": [{
                "kind": "inline_validation",
                "evidence": {
                    "kind": "experiment_record",
                    "supports": "the Checkpoint write completed",
                    "content": {"client": agent_kind, "actual": "persisted"},
                    "interpretation": "the no-Hook workflow is executable",
                    "limitations": []
                }
            }],
            "artifact_refs": [],
            "related_contexts": []
        }]);
        let continued_arguments = checkpoint_arguments(
            agent_kind,
            &session,
            task_id,
            revision_id,
            0,
            "continue",
            claims,
            json!([]),
        );
        let continued = run_session(
            &mut server,
            framing,
            &[tool_call(3, "task_checkpoint", continued_arguments.clone())],
        );
        let continued = &continued[0]["result"]["structuredContent"];
        assert_eq!(continued["created"], true);
        assert_eq!(continued["episode_version"], 1);
        assert_eq!(continued["episode_status"], json!({"status": "open"}));
        assert!(continued.get("candidate_build").is_none());
        assert!(
            continued["checkpoint_id"]
                .as_str()
                .unwrap()
                .starts_with("ckp_")
        );
        assert!(
            continued["claim_ids"][0]
                .as_str()
                .unwrap()
                .starts_with("clm_")
        );
        assert_eq!(
            continued["diagnostics"][0]["kind"],
            "inline_validation_recorded"
        );

        let retried = run_session(
            &mut server,
            framing,
            &[tool_call(4, "task_checkpoint", continued_arguments)],
        );
        let retried = &retried[0]["result"]["structuredContent"];
        assert_eq!(retried["created"], false);
        assert_eq!(retried["checkpoint_id"], continued["checkpoint_id"]);

        let closed = run_session(
            &mut server,
            framing,
            &[tool_call(
                5,
                "task_checkpoint",
                checkpoint_arguments(
                    agent_kind,
                    &session,
                    task_id,
                    revision_id,
                    1,
                    "close",
                    json!([]),
                    json!([{
                        "statement": "No further conclusion is asserted",
                        "blocking": false,
                        "recheck_when": []
                    }]),
                ),
            )],
        );
        let closed = &closed[0]["result"]["structuredContent"];
        assert_eq!(closed["created"], true);
        assert_eq!(closed["episode_version"], 2);
        assert_eq!(
            closed["episode_status"]["final_checkpoint_id"],
            closed["checkpoint_id"]
        );
        let episode_id = closed["episode_id"].as_str().unwrap().parse().unwrap();
        let persisted = TaskRuntime::initialize(&fixture.root)
            .unwrap()
            .read_work_episode(episode_id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.checkpoints.len(), 2);
        assert_eq!(persisted.episode.observations.len(), 1);
        assert_eq!(closed["candidate_build"]["status"], "complete");
        assert_eq!(
            closed["candidate_build"]["items"].as_array().unwrap().len(),
            1
        );
        assert_eq!(closed["candidate_build"]["items"][0]["status"], "created");
        assert!(
            closed["candidate_build"]["items"][0]["candidate_id"]
                .as_str()
                .unwrap()
                .starts_with("cnd_")
        );
        assert_eq!(event_count(fixture.store.repository()), before_events + 1);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_builder_converts_six_typed_sources_without_raw_capture_or_search_injection() {
    let fixture = Fixture::new();
    let before_events = event_count(fixture.store.repository());
    let session = "candidate-builder-six-sources";
    let task = task_intent_update_at_root(
        &fixture.root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            ..update_input(
                session,
                TaskBoundary::New,
                None,
                IntentMaturity::Provisional,
                "build six evidenced Candidate drafts",
            )
        },
    )
    .unwrap();
    let tasks = TaskRuntime::initialize(&fixture.root).unwrap();
    let active = tasks
        .read_snapshot(task.context.task_session_id)
        .unwrap()
        .unwrap();
    let merged = tasks
        .merge_signals(
            active.task_session_id,
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "SearchResult fallback field changed".to_owned(),
            }],
        )
        .unwrap();
    let opened = tasks
        .open_work_episode(
            &sctx_domain::ExternalSessionLocator::new("codex", session).unwrap(),
            active.task_id,
            active.current_intent_revision().unwrap().revision_id,
        )
        .unwrap()
        .episode;
    let signal = opened
        .episode
        .signal_refs
        .iter()
        .find(|signal| signal.signal_id == merged.inserted_signal_ids[0])
        .copied()
        .unwrap();
    let artifact = ArtifactRef {
        repository_id: RepositoryId::new(),
        locator: ArtifactLocator::File {
            path: RepoRelativePath::new("src/search.ts").unwrap(),
        },
    };
    let artifact_observation = tasks
        .append_work_observation(
            opened.episode.episode_id,
            0,
            opened.episode.intent_revisions.last(),
            vec![WorkSourceRef::TaskSignal(signal)],
            NormalizedWorkObservation::Artifact {
                artifact: artifact.clone(),
                action: ArtifactAction::Inspected,
                summary: "The FE reads the server fallback field".to_owned(),
            },
        )
        .unwrap();
    let artifact_observation_id = artifact_observation.observation_id;
    let context_use = tasks
        .append_work_observation(
            opened.episode.episode_id,
            1,
            opened.episode.intent_revisions.last(),
            vec![WorkSourceRef::TaskSignal(signal)],
            NormalizedWorkObservation::ContextUse {
                context: ContextRevisionRef {
                    context_id: fixture.context_id,
                    revision_id: fixture.revision_id,
                },
                disposition: ContextUseDisposition::Applied,
                reason: "The existing transport decision constrains the implementation".to_owned(),
            },
        )
        .unwrap();
    let context_use_observation_id = context_use.observation_id;
    let capture_id = CaptureId::new();
    let capture = tasks
        .ingest_capture(&CaptureIngestion {
            capture_id,
            episode_id: opened.episode.episode_id,
            expected_episode_version: 2,
            task_session_id: active.task_session_id,
            task_id: active.task_id,
            intent_revision_id: opened.episode.intent_revisions.last(),
            additional_sources: Vec::new(),
            observation: NormalizedWorkObservation::Breadcrumb {
                category: NormalizedBreadcrumbKind::Decision,
                summary: "Normalized Capture confirms the compatibility boundary".to_owned(),
            },
            diagnostics: Vec::new(),
        })
        .unwrap();
    let source_snapshot = ProjectionIndex::for_store(&fixture.store)
        .domain_snapshot()
        .unwrap();
    let source_evidence = source_snapshot.projection.spaces[&fixture.space_id].contexts
        [&fixture.context_id]
        .revisions[&fixture.revision_id]
        .revision
        .evidence[0]
        .clone();
    let inline = EvidenceSnapshotDraft {
        kind: EvidenceType::ExperimentRecord,
        supports: "The focused compatibility test passed".to_owned(),
        content: json!({"test": "compatibility", "actual": "passed"}),
        interpretation: "The behavior is directly validated".to_owned(),
        limitations: vec!["local fixture".to_owned()],
    };
    let claims = vec![
        TaskCheckpointClaimInput {
            context_kind_hint: Some(ContextKind::Decision),
            topic_key_hint: None,
            statement: "Keep fallback ownership on the server".to_owned(),
            rationale: "Every client consumes one response contract".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![TaskCheckpointEvidenceInput::InlineValidation { evidence: inline }],
            artifact_refs: Vec::new(),
            related_contexts: Vec::new(),
        },
        TaskCheckpointClaimInput {
            context_kind_hint: None,
            topic_key_hint: None,
            statement: "The FE consumes the fallback Artifact".to_owned(),
            rationale: "The normalized Artifact observation identifies the consumer".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![TaskCheckpointEvidenceInput::Observation {
                observation_id: artifact_observation_id.to_string(),
            }],
            artifact_refs: Vec::new(),
            related_contexts: Vec::new(),
        },
        TaskCheckpointClaimInput {
            context_kind_hint: Some(ContextKind::Contract),
            topic_key_hint: Some("mcp/context-use".to_owned()),
            statement: "The existing Context constrains this implementation".to_owned(),
            rationale: "The Agent explicitly applied that immutable revision".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![TaskCheckpointEvidenceInput::Observation {
                observation_id: context_use_observation_id.to_string(),
            }],
            artifact_refs: Vec::new(),
            related_contexts: vec![ContextRevisionRef {
                context_id: fixture.context_id,
                revision_id: fixture.revision_id,
            }],
        },
        TaskCheckpointClaimInput {
            context_kind_hint: Some(ContextKind::Validation),
            topic_key_hint: None,
            statement: "The Task Diff records the fallback change".to_owned(),
            rationale: "The exact Task Signal is part of this Episode".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![TaskCheckpointEvidenceInput::TaskSignal {
                signal_id: signal.signal_id.to_string(),
            }],
            artifact_refs: Vec::new(),
            related_contexts: Vec::new(),
        },
        TaskCheckpointClaimInput {
            context_kind_hint: Some(ContextKind::Validation),
            topic_key_hint: None,
            statement: "The immutable Context Evidence remains applicable".to_owned(),
            rationale: "The exact indexed Evidence snapshot is reused".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![TaskCheckpointEvidenceInput::ContextEvidence {
                context_id: fixture.context_id.to_string(),
                revision_id: fixture.revision_id.to_string(),
                evidence_id: source_evidence.evidence_id.to_string(),
            }],
            artifact_refs: Vec::new(),
            related_contexts: Vec::new(),
        },
        TaskCheckpointClaimInput {
            context_kind_hint: Some(ContextKind::Progress),
            topic_key_hint: None,
            statement: "The normalized Capture records the compatibility boundary".to_owned(),
            rationale: "Only extracted engineering meaning is retained".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            evidence: vec![TaskCheckpointEvidenceInput::Observation {
                observation_id: capture.observation_id.to_string(),
            }],
            artifact_refs: Vec::new(),
            related_contexts: Vec::new(),
        },
    ];
    let closed = task_checkpoint_at_root(
        &fixture.root,
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: active.task_id.to_string(),
            expected_intent_revision_id: opened.episode.intent_revisions.last().to_string(),
            expected_episode_version: 3,
            boundary: TaskCheckpointBoundary::Close,
            claims,
            unknowns: Vec::new(),
        },
    )
    .unwrap();
    let build = closed.candidate_build.unwrap();
    assert_eq!(build.status, CandidateBuildResponseStatus::Complete);
    assert_eq!(build.items.len(), 6);
    assert!(
        build.items.iter().all(|item| {
            item.status == CandidateBuildItemResponseStatus::Created
                && item.candidate_id.is_some()
                && item.event_id.is_some()
                && item.candidate_status == sctx_domain::AutomaticCandidateStatus::NeedsSpaceReview
                && item.analysis.status == sctx_domain::CandidateAnalysisStatus::Complete
                && !item.analysis.assessments.is_empty()
                && item.space_recommendations.iter().any(|recommendation| {
                    matches!(
                        recommendation,
                        sctx_domain::CandidateSpaceRecommendation::ProposedNewSpaceIntent { .. }
                    )
                })
        }),
        "unexpected analyzed Build items: {:#?}",
        build.items
    );
    assert!(build.items[0].confidence.basis_points >= 4_000);
    assert!(
        build.items[0]
            .unknowns
            .iter()
            .any(|unknown| unknown.statement.contains("topic key") && !unknown.blocking)
    );
    assert!(build.items[1].confidence.basis_points >= 4_000);
    assert!(
        build.items[1]
            .unknowns
            .iter()
            .any(|unknown| unknown.statement.contains("Discovery fallback") && !unknown.blocking)
    );
    assert_eq!(event_count(fixture.store.repository()), before_events + 6);

    let candidates = ProjectionIndex::for_store(&fixture.store)
        .domain_snapshot()
        .unwrap()
        .projection
        .candidates;
    let candidate_views = build
        .items
        .iter()
        .map(|item| {
            &candidates[&item
                .candidate_id
                .expect("created build item must name Candidate")]
                .candidate
        })
        .collect::<Vec<_>>();
    assert_eq!(candidate_views[0].content.kind, ContextKind::Decision);
    assert_eq!(candidate_views[0].content.topic_key, None);
    assert_eq!(candidate_views[1].content.kind, ContextKind::Discovery);
    assert_eq!(candidate_views[2].content.kind, ContextKind::Contract);
    assert_eq!(
        candidate_views[2].content.topic_key.as_deref(),
        Some("mcp/context-use")
    );
    assert_eq!(
        candidate_views[4].content.evidence[0].kind,
        source_evidence.kind
    );
    assert_eq!(
        candidate_views[4].content.evidence[0].supports,
        source_evidence.supports
    );
    assert_eq!(
        candidate_views[4].content.evidence[0].content,
        source_evidence.content
    );
    let capture_candidate = serde_json::to_string(&candidate_views[5]).unwrap();
    assert!(!capture_candidate.contains(&capture_id.to_string()));
    assert!(!capture_candidate.contains("raw_payload"));

    let pack = task_context_readonly_at_root(
        &fixture.root,
        &TaskContextReadInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            token_budget: 2_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    let pack_json = serde_json::to_string(&pack).unwrap();
    for item in &build.items {
        assert!(!pack_json.contains(&item.candidate_id.unwrap().to_string()));
    }

    let full_list = candidate_list_at_root(
        &fixture.root,
        &CandidateListInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: CandidateReviewStatus::Pending,
            limit: 10,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert_eq!(full_list.reviews.len(), 6);
    assert!(full_list.omitted.is_empty());
    assert!(full_list.reviews.iter().all(|summary| {
        summary.0.untrusted_data
            && summary.0.ready_for_review
            && summary.0.analysis.status == sctx_domain::CandidateAnalysisStatus::Complete
            && !summary.0.content.evidence.is_empty()
    }));
    let first_page = candidate_list_at_root(
        &fixture.root,
        &CandidateListInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: CandidateReviewStatus::Pending,
            limit: 2,
            cursor: None,
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert_eq!(first_page.reviews.len(), 2);
    let next = first_page.next_cursor.clone().unwrap();
    let second_page = candidate_list_at_root(
        &fixture.root,
        &CandidateListInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: CandidateReviewStatus::Pending,
            limit: 2,
            cursor: Some(next),
            token_budget: 32_768,
        },
    )
    .unwrap();
    assert!(first_page.reviews.iter().all(|left| {
        second_page
            .reviews
            .iter()
            .all(|right| left.0.candidate_id != right.0.candidate_id)
    }));
    let budgeted = candidate_list_at_root(
        &fixture.root,
        &CandidateListInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            status: CandidateReviewStatus::Pending,
            limit: 6,
            cursor: None,
            token_budget: 512,
        },
    )
    .unwrap();
    assert!(budgeted.estimated_tokens <= 512);
    assert!(!budgeted.omitted.is_empty());

    let review_candidate_id = build.items[2].candidate_id.unwrap();
    let full_review = candidate_get_at_root(
        &fixture.root,
        &CandidateGetInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            candidate_id: review_candidate_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(full_review.content, candidate_views[2].content);
    assert_eq!(full_review.checkpoint_id, build.items[2].checkpoint_id);
    assert_eq!(full_review.claim_id, build.items[2].claim_id);
    assert!(!full_review.analysis.assessments.is_empty());
    assert!(!full_review.space_recommendations.is_empty());

    let other = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "candidate-review-other-session",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "isolate Candidate Reviews",
        ),
    )
    .unwrap();
    assert!(
        candidate_list_at_root(
            &fixture.root,
            &CandidateListInput {
                agent_kind: "codex".to_owned(),
                external_session_id: "candidate-review-other-session".to_owned(),
                status: CandidateReviewStatus::Pending,
                limit: 10,
                cursor: None,
                token_budget: 4_096,
            },
        )
        .unwrap()
        .reviews
        .is_empty()
    );
    assert!(
        candidate_get_at_root(
            &fixture.root,
            &CandidateGetInput {
                agent_kind: "codex".to_owned(),
                external_session_id: "candidate-review-other-session".to_owned(),
                candidate_id: review_candidate_id.to_string(),
            },
        )
        .is_err()
    );
    assert_ne!(other.context.task_id, active.task_id);

    let manual_submission = SubmissionId::new();
    let manual_owner = CandidateOwner {
        agent_kind: "codex".to_owned(),
        external_session_id: session.to_owned(),
        task_id: active.task_id,
        intent_revision_id: active.current_intent_revision().unwrap().revision_id,
        source_episode_id: opened.episode.episode_id,
    };
    let manual = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(90, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                91,
                "candidate_create",
                candidate_arguments(
                    manual_submission,
                    &manual_owner,
                    "manual Git-only Candidate must stay undiscoverable",
                ),
            ),
        ],
    );
    assert_eq!(manual[1]["result"]["isError"], false);
    assert_eq!(
        candidate_list_at_root(
            &fixture.root,
            &CandidateListInput {
                agent_kind: "codex".to_owned(),
                external_session_id: session.to_owned(),
                status: CandidateReviewStatus::Pending,
                limit: 10,
                cursor: None,
                token_budget: 32_768,
            },
        )
        .unwrap()
        .reviews
        .len(),
        6,
        "Git-only manual Candidate must not enter Runtime Review discovery"
    );

    let failed_candidate_id = build.items[0].candidate_id.unwrap();
    let mut failed = tasks
        .read_candidate_analysis(failed_candidate_id)
        .unwrap()
        .unwrap()
        .candidate;
    failed.analysis = sctx_domain::CandidateAnalysis {
        status: sctx_domain::CandidateAnalysisStatus::Failed,
        error_code: Some("analysis_dependency_unavailable".to_owned()),
        ..sctx_domain::CandidateAnalysis::default()
    };
    failed.space_recommendations.clear();
    failed.confidence = sctx_domain::CandidateConfidence {
        basis_points: 0,
        rationale: "Analysis failed and remains retryable".to_owned(),
    };
    failed.status = sctx_domain::AutomaticCandidateStatus::Draft;
    tasks.replace_candidate_analysis(&failed).unwrap();
    let missing_candidate_id = build.items[1].candidate_id.unwrap();
    Connection::open(tasks.database_path())
        .unwrap()
        .execute(
            "DELETE FROM candidate_analysis WHERE candidate_id = ?1",
            [missing_candidate_id.to_string()],
        )
        .unwrap();
    let failed_review = candidate_get_at_root(
        &fixture.root,
        &CandidateGetInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            candidate_id: failed_candidate_id.to_string(),
        },
    )
    .unwrap();
    assert!(!failed_review.ready_for_review);
    assert_eq!(
        failed_review.diagnostics,
        vec![CandidateReviewDiagnostic::AnalysisFailed {
            error_code: "analysis_dependency_unavailable".to_owned()
        }]
    );
    let missing_review = candidate_get_at_root(
        &fixture.root,
        &CandidateGetInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            candidate_id: missing_candidate_id.to_string(),
        },
    )
    .unwrap();
    assert!(!missing_review.ready_for_review);
    assert_eq!(
        missing_review.diagnostics,
        vec![CandidateReviewDiagnostic::AnalysisPending]
    );

    let discarded = candidate_discard_at_root(
        &fixture.root,
        &CandidateDiscardInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: active.task_id.to_string(),
            expected_intent_revision_id: active
                .current_intent_revision()
                .unwrap()
                .revision_id
                .to_string(),
            candidate_id: review_candidate_id.to_string(),
            expected_review_version: 1,
            reason: "not worth retaining".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(discarded.status, CandidateDiscardResponseStatus::Discarded);
    assert_eq!(
        discarded.review.review_status,
        CandidateReviewStatus::Discarded
    );
    assert!(!discarded.review.ready_for_review);
    let retry = candidate_discard_at_root(
        &fixture.root,
        &CandidateDiscardInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: active.task_id.to_string(),
            expected_intent_revision_id: active
                .current_intent_revision()
                .unwrap()
                .revision_id
                .to_string(),
            candidate_id: review_candidate_id.to_string(),
            expected_review_version: 1,
            reason: "not worth retaining".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(
        retry.status,
        CandidateDiscardResponseStatus::AlreadyDiscarded
    );
    assert_eq!(
        candidate_list_at_root(
            &fixture.root,
            &CandidateListInput {
                agent_kind: "codex".to_owned(),
                external_session_id: session.to_owned(),
                status: CandidateReviewStatus::Discarded,
                limit: 10,
                cursor: None,
                token_budget: 32_768,
            },
        )
        .unwrap()
        .reviews[0]
            .0
            .candidate_id,
        review_candidate_id
    );
    let private_reason = candidate_discard_at_root(
        &fixture.root,
        &CandidateDiscardInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: active.task_id.to_string(),
            expected_intent_revision_id: active
                .current_intent_revision()
                .unwrap()
                .revision_id
                .to_string(),
            candidate_id: build.items[3].candidate_id.unwrap().to_string(),
            expected_review_version: 1,
            reason: "AKIAIOSFODNN7EXAMPLE".to_owned(),
        },
    )
    .unwrap_err();
    assert_eq!(
        private_reason.kind(),
        sctx_domain::ErrorKind::PrivacyRejected
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_and_codex_candidate_review_tools_list_get_and_discard_without_confirmation() {
    for (client, framing, agent_kind) in [
        (ClientKind::Cursor, FixtureFraming::Newline, "cursor"),
        (ClientKind::Codex, FixtureFraming::ContentLength, "codex"),
    ] {
        let fixture = Fixture::new();
        let session = format!("candidate-review-{agent_kind}");
        let task = task_intent_update_at_root(
            &fixture.root,
            &TaskIntentUpdateInput {
                agent_kind: agent_kind.to_owned(),
                external_session_id: session.clone(),
                ..update_input(
                    &session,
                    TaskBoundary::New,
                    None,
                    IntentMaturity::Provisional,
                    "review one complete Candidate",
                )
            },
        )
        .unwrap();
        let closed = task_checkpoint_at_root(
            &fixture.root,
            &TaskCheckpointInput {
                agent_kind: agent_kind.to_owned(),
                external_session_id: session.clone(),
                expected_task_id: task.context.task_id.to_string(),
                expected_intent_revision_id: task.context.intent_revision_id.to_string(),
                expected_episode_version: 0,
                boundary: TaskCheckpointBoundary::Close,
                claims: vec![TaskCheckpointClaimInput {
                    context_kind_hint: Some(ContextKind::Validation),
                    topic_key_hint: Some("candidate/review-contract".to_owned()),
                    statement: "Candidate Review returns the original complete draft".to_owned(),
                    rationale: "Review must not ask the user to reconstruct Evidence".to_owned(),
                    applicability: Applicability::default(),
                    assumptions: Vec::new(),
                    recheck_when: vec!["the Review schema changes".to_owned()],
                    evidence: vec![TaskCheckpointEvidenceInput::InlineValidation {
                        evidence: EvidenceSnapshotDraft {
                            kind: EvidenceType::ExperimentRecord,
                            supports: "The Candidate Review MCP fixture passed".to_owned(),
                            content: json!({"tool": "candidate_get", "actual": "complete"}),
                            interpretation: "The full draft is reviewable without retyping"
                                .to_owned(),
                            limitations: vec!["local fixture".to_owned()],
                        },
                    }],
                    artifact_refs: Vec::new(),
                    related_contexts: Vec::new(),
                }],
                unknowns: Vec::new(),
            },
        )
        .unwrap();
        let candidate_id = closed.candidate_build.as_ref().unwrap().items[0]
            .candidate_id
            .unwrap();
        let owner = json!({
            "agent_kind": agent_kind,
            "external_session_id": session,
        });
        let responses = run_session(
            &mut fixture.server(client),
            framing,
            &[
                request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
                tool_call(
                    2,
                    "candidate_list",
                    json!({
                        "agent_kind": agent_kind,
                        "external_session_id": session,
                        "limit": 10,
                        "token_budget": 32768
                    }),
                ),
                tool_call(
                    3,
                    "candidate_get",
                    json!({
                        "agent_kind": agent_kind,
                        "external_session_id": session,
                        "candidate_id": candidate_id
                    }),
                ),
                tool_call(
                    4,
                    "candidate_discard",
                    json!({
                        "agent_kind": agent_kind,
                        "external_session_id": session,
                        "expected_task_id": task.context.task_id,
                        "expected_intent_revision_id": task.context.intent_revision_id,
                        "candidate_id": candidate_id,
                        "expected_review_version": 1,
                        "reason": "explicitly not worth retaining"
                    }),
                ),
                tool_call(
                    5,
                    "candidate_discard",
                    json!({
                        "agent_kind": agent_kind,
                        "external_session_id": session,
                        "expected_task_id": task.context.task_id,
                        "expected_intent_revision_id": task.context.intent_revision_id,
                        "candidate_id": candidate_id,
                        "expected_review_version": 1,
                        "reason": "explicitly not worth retaining"
                    }),
                ),
                tool_call(
                    6,
                    "candidate_discard",
                    json!({
                        "agent_kind": agent_kind,
                        "external_session_id": session,
                        "expected_task_id": task.context.task_id,
                        "expected_intent_revision_id": task.context.intent_revision_id,
                        "candidate_id": candidate_id,
                        "expected_review_version": 1,
                        "reason": "conflicting retry meaning"
                    }),
                ),
                tool_call(7, "candidate_list", owner.clone()),
                tool_call(
                    8,
                    "candidate_list",
                    json!({
                        "agent_kind": agent_kind,
                        "external_session_id": session,
                        "status": "discarded",
                        "token_budget": 32768
                    }),
                ),
            ],
        );
        let listed = &responses[1]["result"]["structuredContent"];
        assert_eq!(listed["reviews"].as_array().unwrap().len(), 1);
        assert_eq!(
            listed["reviews"][0]["candidate_id"],
            candidate_id.to_string()
        );
        assert_eq!(listed["reviews"][0]["untrusted_data"], true);
        assert_eq!(listed["reviews"][0]["ready_for_review"], true);
        assert_eq!(
            listed["reviews"][0]["content"]["statement"],
            "Candidate Review returns the original complete draft"
        );
        assert_eq!(
            listed["reviews"][0]["content"]["evidence"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let got = &responses[2]["result"]["structuredContent"];
        assert_eq!(got["candidate_id"], candidate_id.to_string());
        assert_eq!(
            got["claim_id"],
            closed.candidate_build.as_ref().unwrap().items[0]
                .claim_id
                .to_string()
        );
        assert_eq!(
            responses[3]["result"]["structuredContent"]["status"],
            "discarded"
        );
        assert_eq!(
            responses[4]["result"]["structuredContent"]["status"],
            "already_discarded"
        );
        assert_eq!(responses[5]["result"]["isError"], true);
        assert_eq!(
            responses[5]["result"]["structuredContent"]["error"]["code"],
            "candidate_review_conflict"
        );
        assert!(
            responses[6]["result"]["structuredContent"]["reviews"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            responses[7]["result"]["structuredContent"]["reviews"][0]["review_status"],
            "discarded"
        );
        let response_text = serde_json::to_string(&responses).unwrap();
        for forbidden in ["candidate_confirm", "publication", "governance_action"] {
            assert!(!response_text.contains(forbidden));
        }
    }
}

#[test]
fn candidate_builder_emits_zero_git_events_for_unknown_only_or_insufficient_evidence() {
    let fixture = Fixture::new();
    let before_events = event_count(fixture.store.repository());
    let unknown_owner = closed_candidate_owner(&fixture, "codex", "builder-unknown-only");
    let unknown_build =
        build_closed_episode_at_root(&fixture.root, unknown_owner.source_episode_id).unwrap();
    assert_eq!(unknown_build.status, CandidateBuildResponseStatus::Complete);
    assert!(unknown_build.items.is_empty());
    assert_eq!(event_count(fixture.store.repository()), before_events);

    let tasks = TaskRuntime::initialize(&fixture.root).unwrap();
    let locator = sctx_domain::ExternalSessionLocator::new("codex", "builder-prompt-only").unwrap();
    let task_id = TaskId::new();
    let draft = update_input(
        "builder-prompt-only",
        TaskBoundary::New,
        None,
        IntentMaturity::Provisional,
        "do not treat a Prompt as engineering Evidence",
    )
    .intent;
    let task = tasks
        .open_or_create(
            locator,
            draft.bind(task_id),
            vec![TaskSignal {
                kind: TaskSignalKind::Prompt,
                content: "Please assert this without validation".to_owned(),
            }],
        )
        .unwrap()
        .snapshot;
    let signal = tasks.read_signal_history(task.task_session_id).unwrap()[0].signal_id;
    let closed = task_checkpoint_at_root(
        &fixture.root,
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "builder-prompt-only".to_owned(),
            expected_task_id: task.task_id.to_string(),
            expected_intent_revision_id: task
                .current_intent_revision()
                .unwrap()
                .revision_id
                .to_string(),
            expected_episode_version: 0,
            boundary: TaskCheckpointBoundary::Close,
            claims: vec![TaskCheckpointClaimInput {
                context_kind_hint: Some(ContextKind::Decision),
                topic_key_hint: None,
                statement: "The Prompt claim remains unproven".to_owned(),
                rationale: "A request is not an engineering observation".to_owned(),
                applicability: Applicability::default(),
                assumptions: Vec::new(),
                recheck_when: Vec::new(),
                evidence: vec![TaskCheckpointEvidenceInput::TaskSignal {
                    signal_id: signal.to_string(),
                }],
                artifact_refs: Vec::new(),
                related_contexts: Vec::new(),
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap();
    let build = closed.candidate_build.unwrap();
    assert_eq!(build.status, CandidateBuildResponseStatus::Incomplete);
    assert_eq!(build.items.len(), 1);
    assert_eq!(
        build.items[0].status,
        CandidateBuildItemResponseStatus::NeedsEvidence
    );
    assert_eq!(
        build.items[0].candidate_status,
        sctx_domain::AutomaticCandidateStatus::NeedsEvidence
    );
    assert_eq!(
        build.items[0].error_code.as_deref(),
        Some("task_signal_not_engineering_evidence")
    );
    assert!(build.items[0].candidate_id.is_none());
    assert!(build.items[0].event_id.is_none());
    assert_eq!(event_count(fixture.store.repository()), before_events);
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_builder_recovers_both_crash_windows_and_twenty_concurrent_retries() {
    let fixture = Fixture::new();
    let before_events = event_count(fixture.store.repository());
    let tasks = TaskRuntime::initialize(&fixture.root).unwrap();

    let (runtime_first, runtime_first_content) = directly_close_builder_episode(
        &fixture,
        "builder-runtime-first",
        "Runtime reservation survives before Git",
    );
    let runtime_first_claim = &runtime_first.checkpoint.claims[0];
    let runtime_first_prepared = tasks
        .prepare_candidate_build(
            runtime_first.episode.episode.episode_id,
            &[CandidateBuildItemPreparation {
                checkpoint_id: runtime_first.checkpoint.checkpoint_id,
                claim_id: runtime_first_claim.claim_id,
                content_hash: Some(candidate_submission_content_hash(
                    &runtime_first.episode.episode.ownership(),
                    &runtime_first_content,
                )),
                status: CandidateBuildItemStatus::Prepared,
                error_code: None,
            }],
        )
        .unwrap();
    assert_eq!(event_count(fixture.store.repository()), before_events);
    let runtime_first_retry =
        build_closed_episode_at_root(&fixture.root, runtime_first.episode.episode.episode_id)
            .unwrap();
    assert_eq!(
        runtime_first_retry.build_id,
        runtime_first_prepared.build_id
    );
    assert_eq!(
        runtime_first_retry.items[0].submission_id,
        runtime_first_prepared.items[0].submission_id
    );
    assert_eq!(
        runtime_first_retry.items[0].status,
        CandidateBuildItemResponseStatus::Created
    );

    let (git_first, git_first_content) = directly_close_builder_episode(
        &fixture,
        "builder-git-first",
        "Git submission survives before Runtime finalize",
    );
    let git_first_claim = &git_first.checkpoint.claims[0];
    let git_first_prepared = tasks
        .prepare_candidate_build(
            git_first.episode.episode.episode_id,
            &[CandidateBuildItemPreparation {
                checkpoint_id: git_first.checkpoint.checkpoint_id,
                claim_id: git_first_claim.claim_id,
                content_hash: Some(candidate_submission_content_hash(
                    &git_first.episode.episode.ownership(),
                    &git_first_content,
                )),
                status: CandidateBuildItemStatus::Prepared,
                error_code: None,
            }],
        )
        .unwrap();
    let index = ProjectionIndex::for_store(&fixture.store);
    let store = GitStore::initialize(&fixture.root)
        .unwrap()
        .with_candidate_submission_index(Arc::new(index));
    let direct = store
        .submit_candidate(CandidateSubmissionRequest {
            submission_id: git_first_prepared.items[0].submission_id,
            source_episode: git_first.episode.episode.ownership(),
            content: git_first_content,
        })
        .unwrap();
    let git_first_retry =
        build_closed_episode_at_root(&fixture.root, git_first.episode.episode.episode_id).unwrap();
    assert_eq!(
        git_first_retry.items[0].status,
        CandidateBuildItemResponseStatus::AlreadyExists
    );
    assert_eq!(
        git_first_retry.items[0].candidate_id,
        Some(direct.record.candidate_id)
    );

    let (concurrent, _) = directly_close_builder_episode(
        &fixture,
        "builder-concurrent",
        "Concurrent Builder retries converge",
    );
    let episode_id = concurrent.episode.episode.episode_id;
    let barrier = Arc::new(Barrier::new(20));
    let outcomes = (0..20)
        .map(|_| {
            let root = fixture.root.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                build_closed_episode_at_root(root, episode_id).unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.build_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.items[0].submission_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.items[0].candidate_id.unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(event_count(fixture.store.repository()), before_events + 3);
}

#[test]
fn distinct_claims_with_identical_drafts_keep_distinct_stable_submissions() {
    let fixture = Fixture::new();
    let before_events = event_count(fixture.store.repository());
    let tasks = TaskRuntime::initialize(&fixture.root).unwrap();
    let session = "builder-identical-claims";
    let locator = sctx_domain::ExternalSessionLocator::new("codex", session).unwrap();
    let task_id = TaskId::new();
    let task = tasks
        .open_or_create(
            locator.clone(),
            update_input(
                session,
                TaskBoundary::New,
                None,
                IntentMaturity::Provisional,
                "keep creation-operation identity",
            )
            .intent
            .bind(task_id),
            Vec::new(),
        )
        .unwrap()
        .snapshot;
    let evidence = EvidenceSnapshotDraft {
        kind: EvidenceType::ExperimentRecord,
        supports: "The identical draft fixture passed".to_owned(),
        content: json!({"test": "identical", "actual": "passed"}),
        interpretation: "Both Claims intentionally carry the same content".to_owned(),
        limitations: Vec::new(),
    };
    let claim = CheckpointClaimDraft {
        context_kind_hint: Some(ContextKind::Validation),
        topic_key_hint: None,
        statement: "Two creation operations may have identical text".to_owned(),
        rationale: "Submission identity is operational, not semantic".to_owned(),
        applicability: Applicability::default(),
        assumptions: Vec::new(),
        recheck_when: Vec::new(),
        evidence_refs: Vec::new(),
        inline_validations: vec![evidence],
        artifact_refs: Vec::new(),
        related_contexts: Vec::new(),
    };
    let revision_id = task.current_intent_revision().unwrap().revision_id;
    tasks
        .write_agent_checkpoint(&AgentCheckpointWrite {
            locator: locator.clone(),
            expected_task_id: task.task_id,
            expected_intent_revision_id: revision_id,
            expected_episode_version: 0,
            boundary: CheckpointBoundary::Continue,
            claims: vec![claim.clone()],
            unknowns: Vec::new(),
        })
        .unwrap();
    let closed = tasks
        .write_agent_checkpoint(&AgentCheckpointWrite {
            locator,
            expected_task_id: task.task_id,
            expected_intent_revision_id: revision_id,
            expected_episode_version: 1,
            boundary: CheckpointBoundary::Close,
            claims: vec![claim],
            unknowns: Vec::new(),
        })
        .unwrap();
    let built =
        build_closed_episode_at_root(&fixture.root, closed.episode.episode.episode_id).unwrap();
    assert_eq!(built.status, CandidateBuildResponseStatus::Complete);
    assert_eq!(built.items.len(), 2);
    assert_ne!(built.items[0].submission_id, built.items[1].submission_id);
    assert_ne!(built.items[0].candidate_id, built.items[1].candidate_id);
    let retried =
        build_closed_episode_at_root(&fixture.root, closed.episode.episode.episode_id).unwrap();
    assert_eq!(retried, built);
    assert_eq!(event_count(fixture.store.repository()), before_events + 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_and_codex_fixtures_initialize_read_create_candidate_and_list_spaces() {
    for (client, framing, agent_kind) in [
        (ClientKind::Cursor, FixtureFraming::Newline, "cursor"),
        (ClientKind::Codex, FixtureFraming::ContentLength, "codex"),
    ] {
        let fixture = Fixture::new();
        let before_count = event_count(fixture.store.repository());
        let owner =
            closed_candidate_owner(&fixture, agent_kind, &format!("candidate-{agent_kind}"));
        let submission_id = SubmissionId::new();
        let requests = vec![
            request(
                1,
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": format!("{client:?}"), "version": "fixture"}
                }),
            ),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
            request(2, "tools/list", json!({})),
            tool_call(
                3,
                "context_search",
                json!({
                    "query": "stdio MCP",
                    "space_ids": [fixture.space_id],
                    "statuses": ["accepted"]
                }),
            ),
            tool_call(
                4,
                "context_get",
                json!({
                    "space_id": fixture.space_id,
                    "context_id": fixture.context_id,
                    "revision_id": fixture.revision_id
                }),
            ),
            tool_call(
                5,
                "task_intent_update",
                serde_json::to_value(update_input(
                    "stdio-contract",
                    TaskBoundary::New,
                    None,
                    IntentMaturity::Provisional,
                    "verify stdio MCP contract",
                ))
                .unwrap(),
            ),
            tool_call(
                6,
                "candidate_create",
                candidate_arguments(submission_id, &owner, "new MCP candidate"),
            ),
            tool_call(7, "space_list", json!({})),
        ];
        let responses = run_session(&mut fixture.server(client), framing, &requests);
        assert_eq!(responses.len(), requests.len() - 1);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");

        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 16);
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "task_intent_update",
                "task_artifact_focus",
                "task_signal_supersede",
                "task_checkpoint",
                "task_context",
                "repository_scan",
                "engineering_reference_record",
                "association_explain",
                "association_rebuild",
                "context_search",
                "context_get",
                "candidate_list",
                "candidate_get",
                "candidate_discard",
                "candidate_create",
                "space_list"
            ]
        );
        let update_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "task_intent_update")
            .unwrap()["inputSchema"];
        assert_eq!(update_schema["additionalProperties"], false);
        assert_eq!(update_schema["required"].as_array().unwrap().len(), 7);
        assert_eq!(
            update_schema["properties"]["intent"]["required"]
                .as_array()
                .unwrap()
                .len(),
            11
        );
        let candidate_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "candidate_create")
            .unwrap()["inputSchema"];
        let schema_text = candidate_schema.to_string();
        let candidate_properties = candidate_schema["properties"].as_object().unwrap();
        for forbidden in [
            "event_id",
            "context_id",
            "revision_id",
            "path",
            "publication",
            "workspace",
        ] {
            assert!(
                !candidate_properties.contains_key(forbidden),
                "forbidden Candidate field: {forbidden}"
            );
        }
        assert!(!schema_text.contains("space"));
        let checkpoint_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "task_checkpoint")
            .unwrap()["inputSchema"];
        let checkpoint_claim = &checkpoint_schema["properties"]["claims"]["items"];
        assert_eq!(checkpoint_claim["additionalProperties"], false);
        assert!(
            checkpoint_claim["properties"]
                .get("context_kind_hint")
                .is_some()
        );
        assert!(
            checkpoint_claim["properties"]
                .get("topic_key_hint")
                .is_some()
        );
        assert!(
            !checkpoint_claim["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "context_kind_hint" || field == "topic_key_hint")
        );
        let task_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "task_context")
            .unwrap()["inputSchema"];
        assert_eq!(
            task_schema["required"],
            json!(["agent_kind", "external_session_id"])
        );
        assert_eq!(
            task_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "agent_kind".to_owned(),
                "external_session_id".to_owned(),
                "max_spaces".to_owned(),
                "token_budget".to_owned(),
            ]
            .into()
        );
        assert_eq!(task_schema["properties"]["max_spaces"]["minimum"], 1);
        assert_eq!(task_schema["properties"]["max_spaces"]["maximum"], 32);
        assert_eq!(task_schema["properties"]["max_spaces"]["default"], 8);
        assert_eq!(task_schema["properties"]["token_budget"]["minimum"], 256);
        let focus_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "task_artifact_focus")
            .unwrap()["inputSchema"];
        assert_eq!(focus_schema["additionalProperties"], false);
        assert_eq!(
            focus_schema["required"],
            json!([
                "agent_kind",
                "external_session_id",
                "expected_revision_id",
                "absolute_file_path",
                "locator"
            ])
        );
        let focus_schema_text = focus_schema.to_string();
        for forbidden in [
            "repository_id",
            "relative_path",
            "artifact_key",
            "generation",
            "hook",
            "corroboration",
            "workspace",
        ] {
            assert!(
                !focus_schema_text.contains(forbidden),
                "forbidden task_artifact_focus field: {forbidden}"
            );
        }
        assert!(!focus_schema_text.contains("\"path\""));
        let checkpoint_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "task_checkpoint")
            .unwrap()["inputSchema"];
        assert_eq!(checkpoint_schema["additionalProperties"], false);
        assert_eq!(
            checkpoint_schema["required"],
            json!([
                "agent_kind",
                "external_session_id",
                "expected_task_id",
                "expected_intent_revision_id",
                "expected_episode_version",
                "boundary",
                "claims",
                "unknowns"
            ])
        );
        let checkpoint_schema_text = checkpoint_schema.to_string();
        for forbidden in [
            "checkpoint_id",
            "claim_id",
            "episode_id",
            "space_id",
            "publication",
            "governance",
            "candidate",
        ] {
            assert!(
                !checkpoint_schema_text.contains(forbidden),
                "caller-owned Checkpoint field leaked: {forbidden}"
            );
        }
        let repository_scan_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "repository_scan")
            .unwrap()["inputSchema"];
        assert_eq!(
            repository_scan_schema["required"],
            json!(["checkout_path", "paths"])
        );
        assert_eq!(repository_scan_schema["properties"]["paths"]["minItems"], 1);
        assert_eq!(
            repository_scan_schema["properties"]["paths"]["maxItems"],
            10_000
        );
        for forbidden in ["repository_id", "declared_identity", "remote_hint"] {
            assert!(
                repository_scan_schema["properties"]
                    .get(forbidden)
                    .is_none(),
                "repository_scan must not accept Catalog identity input: {forbidden}"
            );
        }
        let reference_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "engineering_reference_record")
            .unwrap()["inputSchema"];
        assert_eq!(reference_schema["additionalProperties"], false);
        let reference_properties = reference_schema["properties"].as_object().unwrap();
        for forbidden in ["event_id", "reference_id", "event_path"] {
            assert!(
                !reference_properties.contains_key(forbidden),
                "caller-owned Reference/Event field leaked into schema: {forbidden}"
            );
        }
        assert_eq!(reference_schema["properties"]["limitations"]["minItems"], 1);
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "association_explain")
                .unwrap()["inputSchema"]["required"],
            json!(["reference_id"])
        );
        assert!(
            tools
                .iter()
                .find(|tool| tool["name"] == "association_rebuild")
                .unwrap()["inputSchema"]["properties"]
                .get("diagnose_only")
                .is_some()
        );
        let search_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "context_search")
            .unwrap()["inputSchema"];
        assert!(search_schema["properties"].get("space_ids").is_some());
        assert!(
            search_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .all(|field| !field.starts_with("preferred"))
        );
        assert_eq!(candidate_schema["additionalProperties"], false);
        assert!(
            candidate_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "source_episode_id")
        );
        let list_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "candidate_list")
            .unwrap()["inputSchema"];
        assert_eq!(list_schema["additionalProperties"], false);
        assert_eq!(
            list_schema["required"],
            json!(["agent_kind", "external_session_id"])
        );
        assert_eq!(list_schema["properties"]["status"]["default"], "pending");
        let discard_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "candidate_discard")
            .unwrap()["inputSchema"];
        assert_eq!(discard_schema["additionalProperties"], false);
        let discard_schema_text = discard_schema.to_string();
        for forbidden in ["confirm", "publish", "space_id", "context_id", "event_id"] {
            assert!(
                !discard_schema_text.contains(forbidden),
                "Candidate discard schema leaked governance field: {forbidden}"
            );
        }

        for response in &responses[2..] {
            assert_eq!(response["result"]["isError"], false, "{response:#}");
        }
        let search = &responses[2]["result"]["structuredContent"];
        assert_eq!(
            search["results"][0]["context_id"],
            fixture.context_id.to_string()
        );
        assert!(!search["results"][0]["match_reason"].is_null());
        let get = &responses[3]["result"]["structuredContent"];
        assert_eq!(get["context_id"], fixture.context_id.to_string());
        let pack = &responses[4]["result"]["structuredContent"];
        assert!(pack["task_session_id"].as_str().is_some());
        assert!(pack["task_id"].as_str().is_some());
        assert!(pack["intent_revision_id"].as_str().is_some());
        assert!(pack["candidate_spaces"].as_array().is_some());
        assert!(pack["items"].as_array().is_some());
        assert!(pack["retrieval_paths"].as_array().is_some());
        assert_eq!(pack["task_fingerprint"].as_str().unwrap().len(), 64);
        assert!(pack["tree"].as_str().is_some());
        assert!(pack["generation"].as_u64().is_some());
        let candidate = &responses[5]["result"]["structuredContent"];
        assert_eq!(candidate["status"], "candidate");
        assert_eq!(
            candidate["source_episode_id"],
            owner.source_episode_id.to_string()
        );
        assert_eq!(candidate["submission_id"], submission_id.to_string());
        assert!(candidate["candidate_id"].as_str().is_some());
        assert!(candidate.get("space_id").is_none());
        assert_eq!(event_count(fixture.store.repository()), before_count + 1);
        let spaces = &responses[6]["result"]["structuredContent"];
        assert_eq!(spaces["spaces"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn engineering_graph_tool_dispatch_uses_a_distinct_diagnose_contract() {
    let fixture = Fixture::new();
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(2, "association_rebuild", json!({"diagnose_only": true})),
        ],
    );
    let response = &responses[1]["result"]["structuredContent"];
    assert_eq!(response["diagnose_only"], true);
    assert_eq!(response["stored"], false);
    assert_eq!(response["reference_count"], 0);
    assert!(response["artifact_generation"].as_str().is_some());
}

#[test]
fn task_context_rejects_caller_owned_identity_and_space_or_workspace_routes() {
    let fixture = Fixture::new();
    for forbidden in [
        json!({"space_id": fixture.space_id}),
        json!({"space_ids": [fixture.space_id]}),
        json!({"workspace": "/work/must-not-route"}),
        json!({"task_id": "tsk_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}),
        json!({"expected_revision_id": "tir_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}),
        json!({"goal": "must reject Intent"}),
        json!({"desired_change": "must reject Intent"}),
        json!({"in_scope": []}),
        json!({"out_of_scope": []}),
        json!({"domains": []}),
        json!({"platforms": []}),
        json!({"constraints": []}),
        json!({"acceptance_conditions": []}),
        json!({"artifacts": []}),
        json!({"interfaces": []}),
        json!({"unknowns": []}),
        json!({"task_signals": []}),
    ] {
        let mut arguments = task_arguments("codex", "route-rejection");
        arguments
            .as_object_mut()
            .unwrap()
            .extend(forbidden.as_object().unwrap().clone());
        let responses = run_session(
            &mut fixture.server(ClientKind::Codex),
            FixtureFraming::Newline,
            &[
                request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
                tool_call(2, "task_context", arguments),
            ],
        );

        assert_eq!(responses[1]["result"]["isError"], true);
        assert_eq!(
            responses[1]["result"]["structuredContent"]["error"]["code"],
            "invalid_input"
        );
    }
}

#[test]
fn task_context_rejects_unsafe_budget_or_space_bounds() {
    let fixture = Fixture::new();
    for invalid in [
        json!({"token_budget": 255}),
        json!({"max_spaces": 0}),
        json!({"max_spaces": 33}),
    ] {
        let mut arguments = task_arguments("codex", "bound-rejection");
        arguments
            .as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        let responses = run_session(
            &mut fixture.server(ClientKind::Codex),
            FixtureFraming::Newline,
            &[
                request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
                tool_call(2, "task_context", arguments),
            ],
        );
        assert_eq!(responses[1]["result"]["isError"], true);
        assert_eq!(
            responses[1]["result"]["structuredContent"]["error"]["code"],
            "invalid_input"
        );
    }
}

#[test]
fn task_context_read_is_stable_for_an_authoritative_session() {
    let fixture = Fixture::new();
    let authoritative = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "evolving-session",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "verify MCP context",
        ),
    )
    .unwrap();
    let first = task_arguments("codex", "evolving-session");
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(2, "task_context", first.clone()),
            tool_call(3, "task_context", first.clone()),
            tool_call(4, "task_context", first.clone()),
            tool_call(5, "task_context", first),
        ],
    );
    let packs = responses[1..]
        .iter()
        .map(|response| &response["result"]["structuredContent"])
        .collect::<Vec<_>>();

    assert!(
        packs
            .iter()
            .all(|pack| pack["task_session_id"] == packs[0]["task_session_id"])
    );
    assert!(
        packs
            .iter()
            .all(|pack| pack["task_id"] == packs[0]["task_id"])
    );
    assert_eq!(
        packs[0]["intent_revision_id"],
        packs[1]["intent_revision_id"]
    );
    assert_eq!(
        packs[0]["intent_revision_id"],
        authoritative.context.intent_revision_id.to_string()
    );
    assert!(packs.iter().all(|pack| {
        pack["intent_revision_id"] == authoritative.context.intent_revision_id.to_string()
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn different_sessions_with_the_same_workspace_signal_remain_isolated() {
    let fixture = Fixture::new();
    let frontend_authoritative = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "frontend-session",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "frontend MCP context",
        ),
    )
    .unwrap();
    let backend_authoritative = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "backend-session",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "backend MCP context",
        ),
    )
    .unwrap();
    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    runtime
        .merge_signals(
            frontend_authoritative.context.task_session_id,
            vec![
                TaskSignal {
                    kind: TaskSignalKind::Workspace,
                    content: "/work/shared".to_owned(),
                },
                TaskSignal {
                    kind: TaskSignalKind::Diff,
                    content: "web/Search.tsx".to_owned(),
                },
            ],
        )
        .unwrap();
    runtime
        .merge_signals(
            backend_authoritative.context.task_session_id,
            vec![
                TaskSignal {
                    kind: TaskSignalKind::Workspace,
                    content: "/work/shared".to_owned(),
                },
                TaskSignal {
                    kind: TaskSignalKind::Diff,
                    content: "search-v2".to_owned(),
                },
            ],
        )
        .unwrap();
    let frontend = task_arguments("codex", "frontend-session");
    let backend = task_arguments("codex", "backend-session");
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(2, "task_context", frontend),
            tool_call(3, "task_context", backend),
        ],
    );
    let frontend = &responses[1]["result"]["structuredContent"];
    let backend = &responses[2]["result"]["structuredContent"];

    assert_ne!(frontend["task_session_id"], backend["task_session_id"]);
    assert_ne!(frontend["task_id"], backend["task_id"]);
    let frontend_snapshot = runtime
        .read_snapshot(
            frontend["task_session_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    let backend_snapshot = runtime
        .read_snapshot(
            backend["task_session_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(
        frontend_snapshot
            .task_signals
            .iter()
            .any(|signal| signal.content == "web/Search.tsx")
    );
    assert!(
        !frontend_snapshot
            .task_signals
            .iter()
            .any(|signal| signal.content == "search-v2")
    );
    assert!(
        backend_snapshot
            .task_signals
            .iter()
            .any(|signal| signal.content == "search-v2")
    );
}

#[test]
fn concurrent_task_context_reads_do_not_mutate_the_authoritative_task() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let fixture = Fixture::new();
    let created = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "concurrent-session",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "concurrent MCP context",
        ),
    )
    .unwrap();
    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    let locator = sctx_domain::ExternalSessionLocator::new("codex", "concurrent-session").unwrap();
    let before_session = serde_json::to_vec(
        &runtime
            .read_external_session_by_locator(&locator)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let before_signals = serde_json::to_vec(
        &runtime
            .read_signal_history(created.context.task_session_id)
            .unwrap(),
    )
    .unwrap();
    let worker_count = 8;
    let barrier = Arc::new(Barrier::new(worker_count));
    let root = Arc::new(fixture.root.clone());
    let mut workers = Vec::new();
    for _ in 0..worker_count {
        let root = Arc::clone(&root);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            let input: TaskContextReadInput =
                serde_json::from_value(task_arguments("codex", "concurrent-session")).unwrap();
            barrier.wait();
            task_context_readonly_at_root(root.as_path(), &input).unwrap()
        }));
    }
    let responses = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    let session_id = responses[0].task_session_id;
    assert!(
        responses
            .iter()
            .all(|response| response.task_session_id == session_id)
    );
    assert!(
        responses
            .iter()
            .all(|response| response.task_id == responses[0].task_id)
    );
    assert!(
        responses
            .iter()
            .all(|response| response.intent_revision_id == responses[0].intent_revision_id)
    );

    let snapshot = runtime.read_snapshot(session_id).unwrap().unwrap();
    assert_eq!(snapshot.intent_revisions.len(), 1);
    assert!(snapshot.task_signals.is_empty());
    assert_eq!(snapshot.task_id, created.context.task_id);
    assert!(snapshot.validate().is_ok());
    assert_eq!(
        serde_json::to_vec(
            &runtime
                .read_external_session_by_locator(&locator)
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        before_session
    );
    assert_eq!(
        serde_json::to_vec(&runtime.read_signal_history(session_id).unwrap()).unwrap(),
        before_signals
    );
}

#[test]
fn task_context_runtime_storage_failure_is_typed() {
    let fixture = Fixture::new();
    let mut server = fixture.server(ClientKind::Codex);
    let runtime_database = fixture.root.join("state/runtime.sqlite");
    fs::remove_file(&runtime_database).unwrap();
    fs::create_dir(&runtime_database).unwrap();
    let responses = run_session(
        &mut server,
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "task_context",
                task_arguments("codex", "storage-failure"),
            ),
        ],
    );

    assert_eq!(responses[1]["result"]["isError"], true);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["error"]["code"],
        "task_context_storage_failed"
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["error"]["kind"],
        "io_error"
    );
}

#[test]
fn task_intent_update_supports_provisional_grounded_continue_and_explicit_new() {
    let fixture = Fixture::new();
    let provisional = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "intent-lifecycle",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "MCP provisional intent",
        ),
    )
    .unwrap();
    assert_eq!(provisional.maturity, IntentMaturity::Provisional);
    assert_eq!(provisional.context.tree.len(), 40);

    let mut grounded_input = update_input(
        "intent-lifecycle",
        TaskBoundary::Continue,
        Some(provisional.context.intent_revision_id.to_string()),
        IntentMaturity::Grounded,
        "MCP grounded intent",
    );
    grounded_input.evidence_refs = vec!["validation:mcp-contract".to_owned()];
    let grounded = task_intent_update_at_root(&fixture.root, &grounded_input).unwrap();
    assert_eq!(grounded.context.task_id, provisional.context.task_id);
    assert_ne!(
        grounded.context.intent_revision_id,
        provisional.context.intent_revision_id
    );
    assert_eq!(grounded.maturity, IntentMaturity::Grounded);

    let next = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "intent-lifecycle",
            TaskBoundary::New,
            Some(grounded.context.intent_revision_id.to_string()),
            IntentMaturity::Provisional,
            "unrelated banana task",
        ),
    )
    .unwrap();
    assert_ne!(next.context.task_id, grounded.context.task_id);
    assert!(next.active_signals.is_empty());
    let external = TaskRuntime::initialize(&fixture.root)
        .unwrap()
        .read_external_session_by_locator(
            &sctx_domain::ExternalSessionLocator::new("codex", "intent-lifecycle").unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(external.tasks.len(), 2);
    assert_eq!(external.active_task_id, next.context.task_id);
}

#[test]
#[allow(clippy::too_many_lines)]
fn task_intent_update_enforces_cas_complete_shape_and_semantic_evidence() {
    let fixture = Fixture::new();
    let complete = serde_json::to_value(update_input(
        "shape-validation",
        TaskBoundary::New,
        None,
        IntentMaturity::Provisional,
        "complete shape",
    ))
    .unwrap();
    for field in [
        "in_scope",
        "out_of_scope",
        "domains",
        "platforms",
        "constraints",
        "acceptance_conditions",
        "artifacts",
        "interfaces",
        "unknowns",
    ] {
        let mut missing = complete.clone();
        missing["intent"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<TaskIntentUpdateInput>(missing).is_err());
    }
    let mut missing_expected = complete;
    missing_expected
        .as_object_mut()
        .unwrap()
        .remove("expected_revision_id");
    assert!(serde_json::from_value::<TaskIntentUpdateInput>(missing_expected).is_err());

    let created = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "intent-validation",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "validated goal",
        ),
    )
    .unwrap();
    let stale = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "intent-validation",
            TaskBoundary::Continue,
            Some(sctx_domain::TaskIntentRevisionId::new().to_string()),
            IntentMaturity::Provisional,
            "retry goal",
        ),
    )
    .unwrap_err();
    assert!(stale.message().contains("stale"));

    let mut duplicate = update_input(
        "intent-validation",
        TaskBoundary::Continue,
        Some(created.context.intent_revision_id.to_string()),
        IntentMaturity::Provisional,
        " Same   Goal ",
    );
    duplicate.intent.desired_change = "same goal".to_owned();
    assert!(
        task_intent_update_at_root(&fixture.root, &duplicate)
            .unwrap_err()
            .message()
            .contains("semantically distinct")
    );

    let mut overlap = duplicate.clone();
    overlap.intent.goal = "different goal".to_owned();
    overlap.intent.desired_change = "different change".to_owned();
    overlap.intent.in_scope = vec![" Search API ".to_owned()];
    overlap.intent.out_of_scope = vec!["search   api".to_owned()];
    assert!(
        task_intent_update_at_root(&fixture.root, &overlap)
            .unwrap_err()
            .message()
            .contains("overlap")
    );

    let mut unsupported = overlap;
    unsupported.intent.out_of_scope.clear();
    unsupported.intent.artifacts = vec!["symbol:Missing".to_owned()];
    unsupported.intent.interfaces = vec!["api:Missing".to_owned()];
    assert!(
        task_intent_update_at_root(&fixture.root, &unsupported)
            .unwrap_err()
            .message()
            .contains("lacks evidence_ref support")
    );
    unsupported.evidence_refs = vec!["symbol:Missing".to_owned(), "api:Missing".to_owned()];
    assert!(task_intent_update_at_root(&fixture.root, &unsupported).is_ok());

    let missing_array = json!({
        "agent_kind": "codex",
        "external_session_id": "missing-field",
        "task_boundary": "new",
        "expected_revision_id": null,
        "maturity": "provisional",
        "intent": {
            "goal": "missing arrays",
            "desired_change": "reject incomplete shape",
            "in_scope": [], "out_of_scope": [], "domains": [], "platforms": [],
            "constraints": [], "acceptance_conditions": [], "artifacts": [],
            "interfaces": []
        },
        "evidence_refs": []
    });
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(2, "task_intent_update", missing_array),
        ],
    );
    assert_eq!(responses[1]["result"]["isError"], true);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["error"]["code"],
        "invalid_input"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn signal_supersede_is_cas_guarded_and_removed_from_paths_but_retained_in_history() {
    let fixture = Fixture::new();
    let created = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "signal-supersede",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "MCP Signal Context",
        ),
    )
    .unwrap();
    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    let merged = runtime
        .merge_signals(
            created.context.task_session_id,
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "MCP Contract".to_owned(),
            }],
        )
        .unwrap();
    let signal_id = merged.inserted_signal_ids[0];
    let superseded = task_signal_supersede_at_root(
        &fixture.root,
        &TaskSignalSupersedeInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "signal-supersede".to_owned(),
            task_id: created.context.task_id.to_string(),
            expected_revision_id: created.context.intent_revision_id.to_string(),
            signal_ids: vec![signal_id.to_string()],
        },
    )
    .unwrap();
    assert!(superseded.active_signals.is_empty());
    let history = runtime
        .read_signal_history(created.context.task_session_id)
        .unwrap();
    assert_eq!(history[0].signal_id, signal_id);
    assert_eq!(
        history[0].lifecycle,
        sctx_domain::TaskSignalLifecycle::Superseded
    );

    let wrong_task = task_signal_supersede_at_root(
        &fixture.root,
        &TaskSignalSupersedeInput {
            task_id: TaskId::new().to_string(),
            ..TaskSignalSupersedeInput {
                agent_kind: "codex".to_owned(),
                external_session_id: "signal-supersede".to_owned(),
                task_id: created.context.task_id.to_string(),
                expected_revision_id: created.context.intent_revision_id.to_string(),
                signal_ids: vec![signal_id.to_string()],
            }
        },
    )
    .unwrap_err();
    assert!(wrong_task.message().contains("ActiveTask"));

    let other = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "other-signal-session",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "other signal task",
        ),
    )
    .unwrap();
    let other_signal = runtime
        .merge_signals(
            other.context.task_session_id,
            vec![TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "src/other.rs".to_owned(),
            }],
        )
        .unwrap()
        .inserted_signal_ids[0];
    let wrong_signal = task_signal_supersede_at_root(
        &fixture.root,
        &TaskSignalSupersedeInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "signal-supersede".to_owned(),
            task_id: created.context.task_id.to_string(),
            expected_revision_id: created.context.intent_revision_id.to_string(),
            signal_ids: vec![other_signal.to_string()],
        },
    )
    .unwrap_err();
    assert!(wrong_signal.message().contains("supplied Task Session"));

    let after = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "signal-supersede",
            TaskBoundary::Continue,
            Some(created.context.intent_revision_id.to_string()),
            IntentMaturity::Provisional,
            "MCP Signal Context after supersede",
        ),
    )
    .unwrap();
    let encoded_paths = serde_json::to_string(&after.context.retrieval_paths).unwrap();
    assert!(!encoded_paths.contains("MCP Contract"));
}

#[test]
fn shared_context_skill_contract_drives_mcp_runtime_and_search_response() {
    let fixture = Fixture::new();
    let skill = fs::read_to_string("../../skills/shared-context/SKILL.md")
        .or_else(|_| fs::read_to_string("skills/shared-context/SKILL.md"))
        .unwrap();
    for required in [
        "expected_revision_id",
        "maturity",
        "evidence_refs",
        "active_signals",
        "task_artifact_focus",
        "absolute_file_path",
        "artifact_not_reachable_in_graph",
        "task_signal_supersede",
    ] {
        assert!(skill.contains(required), "Skill is missing {required}");
    }
    let arguments = serde_json::to_value(update_input(
        "skill-e2e",
        TaskBoundary::New,
        None,
        IntentMaturity::Provisional,
        "MCP Contract",
    ))
    .unwrap();
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(2, "task_intent_update", arguments),
        ],
    );
    let data = &responses[1]["result"]["structuredContent"];
    assert_eq!(responses[1]["result"]["isError"], false);
    assert!(data["task_id"].as_str().unwrap().starts_with("tsk_"));
    assert!(
        data["intent_revision_id"]
            .as_str()
            .unwrap()
            .starts_with("tir_")
    );
    assert!(data["candidate_spaces"].as_array().is_some());
    assert!(data["items"].as_array().is_some());
    assert!(data["tree"].as_str().is_some());
}

#[test]
fn candidate_create_retries_are_strict_and_unassigned_candidates_are_not_retrieved() {
    let fixture = Fixture::new();
    let owner = closed_candidate_owner(&fixture, "codex", "candidate-create-retry");
    let submission_id = SubmissionId::new();
    let distinct_submission_id = SubmissionId::new();
    let before_count = event_count(fixture.store.repository());
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "candidate_create",
                candidate_arguments(submission_id, &owner, "hidden MCP episode knowledge"),
            ),
            tool_call(
                3,
                "candidate_create",
                candidate_arguments(submission_id, &owner, "hidden MCP episode knowledge"),
            ),
            tool_call(
                4,
                "candidate_create",
                candidate_arguments(submission_id, &owner, "different MCP episode knowledge"),
            ),
            tool_call(
                5,
                "candidate_create",
                candidate_arguments(
                    distinct_submission_id,
                    &owner,
                    "hidden MCP episode knowledge",
                ),
            ),
            tool_call(
                6,
                "context_search",
                json!({"query": "hidden MCP episode knowledge", "statuses": ["candidate"]}),
            ),
            tool_call(
                7,
                "task_intent_update",
                serde_json::to_value(update_input(
                    "candidate-isolation",
                    TaskBoundary::New,
                    None,
                    IntentMaturity::Provisional,
                    "hidden MCP episode knowledge",
                ))
                .unwrap(),
            ),
        ],
    );
    let created = &responses[1]["result"]["structuredContent"];
    let retry = &responses[2]["result"]["structuredContent"];
    let conflict = &responses[3]["result"]["structuredContent"];
    let distinct = &responses[4]["result"]["structuredContent"];
    assert_eq!(created["created"], true);
    assert_eq!(retry["created"], false);
    assert_eq!(created["submission_status"], "created");
    assert_eq!(retry["submission_status"], "already_exists");
    assert_eq!(retry["match_reason"], "already_exists");
    for field in [
        "candidate_id",
        "submission_id",
        "source_episode_id",
        "event_id",
        "batch_id",
        "commit_oid",
    ] {
        assert_eq!(created[field], retry[field]);
    }
    assert_eq!(responses[3]["result"]["isError"], true);
    assert_eq!(conflict["error"]["code"], "idempotency_key_conflict");
    assert_ne!(created["candidate_id"], distinct["candidate_id"]);
    assert_ne!(created["submission_id"], distinct["submission_id"]);
    assert_eq!(distinct["created"], true);
    assert_eq!(event_count(fixture.store.repository()), before_count + 2);
    assert!(
        responses[5]["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let task_context = &responses[6]["result"]["structuredContent"];
    assert!(
        !serde_json::to_string(task_context)
            .unwrap()
            .contains(created["candidate_id"].as_str().unwrap())
    );
    assert!(
        task_context["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| { item["context"]["statement"] != "hidden MCP episode knowledge" })
    );
}

#[test]
fn candidate_create_ignores_unrelated_bad_events_but_blocks_its_malformed_submission() {
    let fixture = Fixture::new();
    let owner = closed_candidate_owner(&fixture, "codex", "candidate-malformed-isolation");
    commit_raw_event(
        fixture.store.repository(),
        "unrelated-missing-required",
        &serde_json::from_str(include_str!(
            "../../../fixtures/events/v1/invalid/missing-required-field.json"
        ))
        .unwrap(),
    );
    let blocked_submission = SubmissionId::new();
    commit_raw_event(
        fixture.store.repository(),
        "same-submission-malformed",
        &json!({
            "schema_version": "1",
            "event_type": "context_candidate.created",
            "event_id": EventId::new(),
            "candidate": {
                "candidate_id": CandidateId::new(),
                "submission_id": blocked_submission
            }
        }),
    );
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "candidate_create",
                candidate_arguments(
                    SubmissionId::new(),
                    &owner,
                    "unrelated malformed Events remain isolated",
                ),
            ),
            tool_call(
                3,
                "candidate_create",
                candidate_arguments(
                    blocked_submission,
                    &owner,
                    "same malformed submission is blocked",
                ),
            ),
            tool_call(
                4,
                "candidate_create",
                candidate_arguments(
                    SubmissionId::new(),
                    &owner,
                    "another submission remains writable",
                ),
            ),
        ],
    );
    assert_eq!(responses[1]["result"]["isError"], false);
    assert_eq!(responses[1]["result"]["structuredContent"]["created"], true);
    assert_eq!(responses[2]["result"]["isError"], true);
    assert_eq!(
        responses[2]["result"]["structuredContent"]["error"]["code"],
        "idempotency_key_conflict"
    );
    assert_eq!(responses[3]["result"]["isError"], false);
    let diagnostics = ProjectionIndex::for_store(&fixture.store)
        .domain_snapshot()
        .unwrap()
        .diagnostics;
    assert!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "EVENT_PARSE_ERROR")
            .count()
            >= 2
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_create_rejects_missing_open_cross_task_and_stale_ownership_without_git_writes() {
    let fixture = Fixture::new();
    let before_events = event_count(fixture.store.repository());

    let missing_task = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "candidate-missing-episode",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "reject a missing source Episode",
        ),
    )
    .unwrap();
    let missing_owner = CandidateOwner {
        agent_kind: "codex".to_owned(),
        external_session_id: "candidate-missing-episode".to_owned(),
        task_id: missing_task.context.task_id,
        intent_revision_id: missing_task.context.intent_revision_id,
        source_episode_id: WorkEpisodeId::new(),
    };

    let open_task = task_intent_update_at_root(
        &fixture.root,
        &update_input(
            "candidate-open-episode",
            TaskBoundary::New,
            None,
            IntentMaturity::Provisional,
            "reject an open source Episode",
        ),
    )
    .unwrap();
    let open_checkpoint = task_checkpoint_at_root(
        &fixture.root,
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "candidate-open-episode".to_owned(),
            expected_task_id: open_task.context.task_id.to_string(),
            expected_intent_revision_id: open_task.context.intent_revision_id.to_string(),
            expected_episode_version: 0,
            boundary: TaskCheckpointBoundary::Continue,
            claims: Vec::new(),
            unknowns: vec![CaptureUnknown {
                statement: "Episode intentionally remains open".to_owned(),
                blocking: false,
                recheck_when: Vec::new(),
            }],
        },
    )
    .unwrap();
    let open_owner = CandidateOwner {
        agent_kind: "codex".to_owned(),
        external_session_id: "candidate-open-episode".to_owned(),
        task_id: open_task.context.task_id,
        intent_revision_id: open_task.context.intent_revision_id,
        source_episode_id: open_checkpoint.episode_id,
    };

    let source_owner = closed_candidate_owner(&fixture, "codex", "candidate-source-task");
    let target_owner = closed_candidate_owner(&fixture, "codex", "candidate-target-task");
    let cross_task_owner = CandidateOwner {
        source_episode_id: source_owner.source_episode_id,
        ..target_owner
    };

    let stale_owner = closed_candidate_owner(&fixture, "codex", "candidate-stale-intent");
    task_intent_update_at_root(
        &fixture.root,
        &update_input(
            &stale_owner.external_session_id,
            TaskBoundary::Continue,
            Some(stale_owner.intent_revision_id.to_string()),
            IntentMaturity::Provisional,
            "advance the Candidate owner Intent",
        ),
    )
    .unwrap();

    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "candidate_create",
                candidate_arguments(SubmissionId::new(), &missing_owner, "missing source"),
            ),
            tool_call(
                3,
                "candidate_create",
                candidate_arguments(SubmissionId::new(), &open_owner, "open source"),
            ),
            tool_call(
                4,
                "candidate_create",
                candidate_arguments(SubmissionId::new(), &cross_task_owner, "cross Task source"),
            ),
            tool_call(
                5,
                "candidate_create",
                candidate_arguments(SubmissionId::new(), &stale_owner, "stale Intent owner"),
            ),
        ],
    );
    for response in &responses[1..] {
        assert_eq!(response["result"]["isError"], true, "{response:#}");
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            "invalid_input"
        );
    }
    assert_eq!(event_count(fixture.store.repository()), before_events);
}

#[test]
fn malformed_json_invalid_arguments_and_writer_rejection_are_typed() {
    let fixture = Fixture::new();
    let mut input = b"{not-json}\n".to_vec();
    input.extend(encode_frames(
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "candidate_create",
                json!({
                    "context_id": fixture.context_id,
                    "space_id": fixture.space_id,
                    "source_episode_id": WorkEpisodeId::new(),
                    "kind": "decision",
                    "statement": "caller supplied identity",
                    "rationale": "must fail",
                    "evidence": []
                }),
            ),
        ],
        FixtureFraming::Newline,
    ));
    let mut output = Vec::new();
    let outcome = fixture
        .server(ClientKind::Cursor)
        .serve(&mut BufReader::new(Cursor::new(input)), &mut output)
        .unwrap();
    assert_eq!(outcome.requests_handled, 3);
    let responses = decode_frames(&output, FixtureFraming::Newline);
    assert_eq!(responses[0]["error"]["code"], -32_700);
    assert_eq!(responses[0]["error"]["data"]["code"], "parse_error");
    assert_eq!(responses[2]["result"]["isError"], true);
    assert_eq!(
        responses[2]["result"]["structuredContent"]["error"]["code"],
        "invalid_input"
    );

    let owner = closed_candidate_owner(&fixture, "codex", "candidate-writer-rejection");
    dirty_first_event(fixture.store.repository());
    let writer_responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(4, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                5,
                "candidate_create",
                candidate_arguments(
                    SubmissionId::new(),
                    &owner,
                    "Writer must reject dirty managed input",
                ),
            ),
        ],
    );
    assert_eq!(writer_responses[1]["result"]["isError"], true);
    assert_eq!(
        writer_responses[1]["result"]["structuredContent"]["error"]["code"],
        "writer_rejected"
    );
    assert_eq!(
        writer_responses[1]["result"]["structuredContent"]["error"]["kind"],
        "invariant_violation"
    );
}

#[test]
fn disconnects_and_invalid_framing_have_typed_transport_results() {
    let fixture = Fixture::new();
    let responses = run_session(
        &mut fixture.server(ClientKind::Cursor),
        FixtureFraming::Newline,
        &[request(1, "tools/list", json!({}))],
    );
    assert_eq!(responses[0]["error"]["code"], -32_002);
    assert_eq!(
        responses[0]["error"]["data"]["code"],
        "server_not_initialized"
    );

    let outcome = fixture
        .server(ClientKind::Cursor)
        .serve(
            &mut BufReader::new(Cursor::new(Vec::<u8>::new())),
            &mut Vec::new(),
        )
        .unwrap();
    assert_eq!(outcome.disconnect, DisconnectReason::CleanEof);
    assert_eq!(outcome.requests_handled, 0);

    let truncated = b"Content-Length: 20\r\n\r\n{}".to_vec();
    let error = fixture
        .server(ClientKind::Cursor)
        .serve(&mut BufReader::new(Cursor::new(truncated)), &mut Vec::new())
        .unwrap_err();
    assert_eq!(error.kind(), TransportErrorKind::UnexpectedEof);

    let invalid = b"Content-Length: nope\r\n\r\n".to_vec();
    let error = fixture
        .server(ClientKind::Codex)
        .serve(&mut BufReader::new(Cursor::new(invalid)), &mut Vec::new())
        .unwrap_err();
    assert_eq!(error.kind(), TransportErrorKind::InvalidFrame);
}

fn event_count(repository: &Path) -> usize {
    git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .filter(|path| path.starts_with("events/"))
        .count()
}

fn commit_raw_event(repository: &Path, label: &str, value: &Value) {
    let relative = format!("events/ab/{label}.json");
    let path = repository.join(&relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    git(repository, &["add", "--", &relative]);
    git(
        repository,
        &["commit", "-m", &format!("Add {label} fixture")],
    );
}

fn dirty_first_event(repository: &Path) {
    let path = git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .find(|path| path.starts_with("events/"))
        .unwrap()
        .to_owned();
    fs::write(repository.join(path), b"{}\n").unwrap();
}

fn git(repository: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap()
}
