use std::{
    fs,
    io::{BufReader, Cursor},
    path::Path,
    process::Command,
};

use sctx_domain::{
    Applicability, ContextId, ContextKind, ContextRevisionDraft, EvidenceSnapshotDraft,
    EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft, RevisionId, SpaceId,
    WorkEpisodeId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_mcp::{ClientKind, DisconnectReason, McpServer, TransportErrorKind};
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
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "the client fixture completed".to_owned(),
            content: json!({"request": "tools/call", "actual": "success"}),
            interpretation: "the protocol contract is executable".to_owned(),
            limitations: vec!["local fixture".to_owned()],
        }],
    }
}

fn candidate_arguments(source_episode_id: WorkEpisodeId, statement: &str) -> Value {
    json!({
        "source_episode_id": source_episode_id,
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

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_and_codex_fixtures_initialize_read_create_candidate_and_list_spaces() {
    for (client, framing) in [
        (ClientKind::Cursor, FixtureFraming::Newline),
        (ClientKind::Codex, FixtureFraming::ContentLength),
    ] {
        let fixture = Fixture::new();
        let before_count = event_count(fixture.store.repository());
        let source_episode_id = WorkEpisodeId::new();
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
                "context_for_task",
                json!({"task": "verify stdio MCP contract"}),
            ),
            tool_call(
                6,
                "candidate_create",
                candidate_arguments(source_episode_id, "new MCP candidate"),
            ),
            tool_call(7, "space_list", json!({})),
        ];
        let responses = run_session(&mut fixture.server(client), framing, &requests);
        assert_eq!(responses.len(), requests.len() - 1);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");

        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "context_for_task",
                "context_search",
                "context_get",
                "candidate_create",
                "space_list"
            ]
        );
        let candidate_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "candidate_create")
            .unwrap()["inputSchema"];
        let schema_text = candidate_schema.to_string();
        for forbidden in [
            "event_id",
            "context_id",
            "revision_id",
            "path",
            "publication",
            "workspace",
        ] {
            assert!(
                !schema_text.contains(forbidden),
                "forbidden Candidate field: {forbidden}"
            );
        }
        assert!(!schema_text.contains("space"));
        let task_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "context_for_task")
            .unwrap()["inputSchema"];
        assert!(task_schema["properties"].get("task").is_some());
        assert!(
            task_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .all(|field| !field.contains("space") && !field.contains("workspace"))
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

        for response in &responses[2..] {
            assert_eq!(response["result"]["isError"], false, "{response:#}");
            let data = &response["result"]["structuredContent"];
            assert!(data["indexed_tree_oid"].as_str().is_some());
            assert!(data["projection_generation"].as_u64().is_some());
            assert!(!data["conflicts"].is_null());
            assert!(!data["match_reason"].is_null());
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
        assert_eq!(pack["mode"], "automatic_injection");
        let candidate = &responses[5]["result"]["structuredContent"];
        assert_eq!(candidate["status"], "candidate");
        assert_eq!(
            candidate["source_episode_id"],
            source_episode_id.to_string()
        );
        assert!(candidate["candidate_id"].as_str().is_some());
        assert!(candidate.get("space_id").is_none());
        assert_eq!(event_count(fixture.store.repository()), before_count + 1);
        let spaces = &responses[6]["result"]["structuredContent"];
        assert_eq!(spaces["spaces"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn context_for_task_rejects_a_caller_supplied_space_route() {
    let fixture = Fixture::new();
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "context_for_task",
                json!({"task": "find task context", "space_id": fixture.space_id}),
            ),
        ],
    );

    assert_eq!(responses[1]["result"]["isError"], true);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["error"]["code"],
        "invalid_input"
    );
}

#[test]
fn candidate_create_retries_are_strict_and_unassigned_candidates_are_not_retrieved() {
    let fixture = Fixture::new();
    let source_episode_id = WorkEpisodeId::new();
    let before_count = event_count(fixture.store.repository());
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "candidate_create",
                candidate_arguments(source_episode_id, "hidden MCP episode knowledge"),
            ),
            tool_call(
                3,
                "candidate_create",
                candidate_arguments(source_episode_id, "hidden MCP episode knowledge"),
            ),
            tool_call(
                4,
                "candidate_create",
                candidate_arguments(source_episode_id, "different MCP episode knowledge"),
            ),
            tool_call(
                5,
                "context_search",
                json!({"query": "hidden MCP episode knowledge", "statuses": ["candidate"]}),
            ),
            tool_call(
                6,
                "context_for_task",
                json!({"task": "hidden MCP episode knowledge"}),
            ),
        ],
    );
    let created = &responses[1]["result"]["structuredContent"];
    let retry = &responses[2]["result"]["structuredContent"];
    let different = &responses[3]["result"]["structuredContent"];
    assert_eq!(created["created"], true);
    assert_eq!(retry["created"], false);
    for field in [
        "candidate_id",
        "source_episode_id",
        "event_id",
        "batch_id",
        "commit_oid",
    ] {
        assert_eq!(created[field], retry[field]);
    }
    assert_ne!(created["candidate_id"], different["candidate_id"]);
    assert_eq!(different["created"], true);
    assert_eq!(event_count(fixture.store.repository()), before_count + 2);
    assert!(
        responses[4]["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        responses[5]["result"]["structuredContent"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
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
                    WorkEpisodeId::new(),
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
