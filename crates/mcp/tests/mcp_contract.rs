use std::{
    collections::BTreeSet,
    fs,
    io::{BufReader, Cursor},
    path::Path,
    process::Command,
};

use sctx_domain::{
    Applicability, ContextId, ContextKind, ContextRevisionDraft, EvidenceSnapshotDraft,
    EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft, RevisionId, SpaceId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_local_state::UserConfigStore;
use sctx_mcp::{ClientKind, DisconnectReason, McpServer, TransportErrorKind};
use serde_json::{Value, json};
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum FixtureFraming {
    Newline,
    ContentLength,
}

struct Fixture {
    temporary: TempDir,
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
        let proposed =
            Event::context_proposed(space_id, draft("stdio MCP contract"), None).unwrap();
        let (context_id, revision_id) = context_identity(&proposed);
        append(&store, proposed);
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
            temporary,
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

    fn bind(&self, workspace: &Path, space_id: SpaceId) {
        UserConfigStore::initialize(&self.root)
            .unwrap()
            .bind(workspace, space_id)
            .unwrap();
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

fn proposal_arguments(space_id: SpaceId, statement: &str) -> Value {
    json!({
        "space_id": space_id,
        "kind": "decision",
        "topic_key": "mcp/proposal",
        "statement": statement,
        "rationale": "candidate IDs and paths remain server-owned",
        "applicability": {"domains": ["mcp"], "platforms": ["macos"], "conditions": ["stdio"]},
        "assumptions": [],
        "recheck_when": ["the Writer contract changes"],
        "evidence": [{
            "kind": "experiment_record",
            "supports": "the proposal fixture called the Writer",
            "content": {"fixture": "context_propose", "actual": "candidate"},
            "interpretation": "the candidate was appended",
            "limitations": []
        }]
    })
}

fn workspace_proposal_arguments(workspace: &Path, statement: &str) -> Value {
    let mut arguments = proposal_arguments(SpaceId::new(), statement);
    let object = arguments.as_object_mut().unwrap();
    object.remove("space_id");
    object.insert(
        "workspace".to_owned(),
        Value::String(workspace.to_str().unwrap().to_owned()),
    );
    arguments
}

fn proposal_without_routing(statement: &str) -> Value {
    let mut arguments = proposal_arguments(SpaceId::new(), statement);
    arguments.as_object_mut().unwrap().remove("space_id");
    arguments
}

fn create_space(store: &GitStore, title: &str) -> SpaceId {
    let mut space_intent = intent();
    title.clone_into(&mut space_intent.title);
    let created = Event::space_created(space_intent, None).unwrap();
    let space_id = match created.payload() {
        EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    append(store, created);
    space_id
}

fn publish_context(store: &GitStore, space_id: SpaceId, statement: &str) -> ContextId {
    let proposed = Event::context_proposed(space_id, draft(statement), None).unwrap();
    let (context_id, revision_id) = context_identity(&proposed);
    append(store, proposed);
    append(
        store,
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
    context_id
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
fn cursor_and_codex_fixtures_initialize_list_search_get_propose_and_list_spaces() {
    for (client, framing) in [
        (ClientKind::Cursor, FixtureFraming::Newline),
        (ClientKind::Codex, FixtureFraming::ContentLength),
    ] {
        let fixture = Fixture::new();
        let before_count = event_count(fixture.store.repository());
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
                json!({"query": "stdio MCP", "statuses": ["accepted"]}),
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
                json!({"task": "verify stdio MCP contract", "space_id": fixture.space_id}),
            ),
            tool_call(
                6,
                "context_propose",
                proposal_arguments(fixture.space_id, "new MCP candidate"),
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
                "context_propose",
                "space_list"
            ]
        );
        let proposal_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "context_propose")
            .unwrap()["inputSchema"];
        let schema_text = proposal_schema.to_string();
        for forbidden in [
            "event_id",
            "context_id",
            "revision_id",
            "path",
            "publication",
        ] {
            assert!(
                !schema_text.contains(forbidden),
                "forbidden proposal field: {forbidden}"
            );
        }
        assert_eq!(proposal_schema["additionalProperties"], false);
        let task_schema = &tools
            .iter()
            .find(|tool| tool["name"] == "context_for_task")
            .unwrap()["inputSchema"];
        for schema in [task_schema, proposal_schema] {
            assert_eq!(schema["properties"]["workspace"]["pattern"], "^/");
            assert_eq!(schema["anyOf"].as_array().unwrap().len(), 2);
        }

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
        assert_eq!(
            pack["routing"],
            json!({
                "resolved_space_id": fixture.space_id,
                "source": "explicit_space_id"
            })
        );
        let proposal = &responses[5]["result"]["structuredContent"];
        assert_eq!(proposal["status"], "candidate");
        assert_eq!(proposal["routing"], pack["routing"]);
        assert_eq!(proposal["deduplicated"], false);
        assert_eq!(event_count(fixture.store.repository()), before_count + 1);
        let spaces = &responses[6]["result"]["structuredContent"];
        assert_eq!(spaces["spaces"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn exact_workspace_binding_routes_task_and_proposal_without_persisting_workspace() {
    let fixture = Fixture::new();
    let workspace = fixture.temporary.path().join("业务 workspace 中文");
    fs::create_dir_all(&workspace).unwrap();
    fixture.bind(&workspace, fixture.space_id);
    let before_paths = event_paths(fixture.store.repository());

    let responses = run_session(
        &mut fixture.server(ClientKind::Cursor),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "context_for_task",
                json!({"task": "stdio MCP contract", "workspace": workspace}),
            ),
            tool_call(
                3,
                "context_propose",
                workspace_proposal_arguments(&workspace, "workspace-routed candidate"),
            ),
        ],
    );

    for response in &responses[1..] {
        assert_eq!(response["result"]["isError"], false, "{response:#}");
        let data = &response["result"]["structuredContent"];
        assert_eq!(
            data["routing"],
            json!({
                "resolved_space_id": fixture.space_id,
                "source": "workspace_binding"
            })
        );
        assert!(!data.to_string().contains(workspace.to_str().unwrap()));
    }
    let pack_items = responses[1]["result"]["structuredContent"]["items"]
        .as_array()
        .unwrap();
    assert!(!pack_items.is_empty());
    assert!(
        pack_items
            .iter()
            .all(|item| item["space_id"] == fixture.space_id.to_string())
    );

    let after_paths = event_paths(fixture.store.repository());
    let new_paths = after_paths.difference(&before_paths).collect::<Vec<_>>();
    assert_eq!(new_paths.len(), 1);
    let event_text = git(
        fixture.store.repository(),
        &["show", &format!("HEAD:{}", new_paths[0])],
    );
    let event: Value = serde_json::from_str(&event_text).unwrap();
    assert_eq!(
        event["annotations"],
        json!({
            "producer": "shared-context-mcp",
            "origin_hint": {"client": "cursor"}
        })
    );
    assert!(!event_text.contains(workspace.to_str().unwrap()));

    let created = &responses[2]["result"]["structuredContent"];
    let retry = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::ContentLength,
        &[
            request(4, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                5,
                "context_propose",
                proposal_arguments(fixture.space_id, "workspace-routed candidate"),
            ),
        ],
    );
    let existing = &retry[1]["result"]["structuredContent"];
    assert_eq!(existing["deduplicated"], true);
    for id in ["event_id", "context_id", "revision_id"] {
        assert_eq!(existing[id], created[id], "{id}");
    }
    assert_eq!(
        event_count(fixture.store.repository()),
        before_paths.len() + 1
    );
}

#[test]
fn routing_fails_closed_for_unbound_invalid_and_missing_inputs() {
    let fixture = Fixture::new();
    let bound = fixture.temporary.path().join("bound workspace");
    let child = bound.join("child");
    let unbound = fixture.temporary.path().join("unbound workspace");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir_all(&unbound).unwrap();
    fixture.bind(&bound, fixture.space_id);
    let missing = fixture.temporary.path().join("missing workspace");
    let before_count = event_count(fixture.store.repository());

    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::ContentLength,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "context_for_task",
                json!({"task": "no prefix routing", "workspace": child}),
            ),
            tool_call(
                3,
                "context_propose",
                workspace_proposal_arguments(&unbound, "unbound must fail"),
            ),
            tool_call(
                4,
                "context_for_task",
                json!({"task": "relative must fail", "workspace": "."}),
            ),
            tool_call(
                5,
                "context_propose",
                workspace_proposal_arguments(&missing, "missing must fail"),
            ),
            tool_call(6, "context_for_task", json!({"task": "do not guess"})),
            tool_call(
                7,
                "context_propose",
                proposal_without_routing("do not guess the only Space"),
            ),
        ],
    );

    let error_codes = responses[1..]
        .iter()
        .map(|response| {
            assert_eq!(response["result"]["isError"], true, "{response:#}");
            response["result"]["structuredContent"]["error"]["code"]
                .as_str()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        error_codes,
        [
            "workspace_unbound",
            "workspace_unbound",
            "invalid_input",
            "invalid_input",
            "routing_unresolved",
            "routing_unresolved",
        ]
    );
    assert_eq!(event_count(fixture.store.repository()), before_count);
}

#[test]
fn explicit_space_wins_over_a_conflicting_workspace_binding() {
    let fixture = Fixture::new();
    let workspace = fixture.temporary.path().join("conflicting workspace");
    fs::create_dir_all(&workspace).unwrap();
    fixture.bind(&workspace, fixture.space_id);
    let explicit_space_id = create_space(&fixture.store, "Explicit routing target");
    publish_context(
        &fixture.store,
        explicit_space_id,
        "explicit routing target contract",
    );

    let mut proposal = proposal_arguments(explicit_space_id, "explicit proposal wins");
    proposal.as_object_mut().unwrap().insert(
        "workspace".to_owned(),
        Value::String(workspace.to_str().unwrap().to_owned()),
    );
    let responses = run_session(
        &mut fixture.server(ClientKind::Cursor),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(
                2,
                "context_for_task",
                json!({
                    "task": "explicit routing target contract",
                    "space_id": explicit_space_id,
                    "workspace": workspace,
                }),
            ),
            tool_call(3, "context_propose", proposal),
        ],
    );

    for response in &responses[1..] {
        assert_eq!(response["result"]["isError"], false, "{response:#}");
        assert_eq!(
            response["result"]["structuredContent"]["routing"],
            json!({
                "resolved_space_id": explicit_space_id,
                "source": "explicit_space_id"
            })
        );
    }
    let items = responses[1]["result"]["structuredContent"]["items"]
        .as_array()
        .unwrap();
    assert!(!items.is_empty());
    assert!(
        items
            .iter()
            .all(|item| item["space_id"] == explicit_space_id.to_string())
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["space_id"],
        explicit_space_id.to_string()
    );
}

#[test]
fn repeated_identical_workspace_routed_proposal_returns_existing_ids_without_an_event() {
    let fixture = Fixture::new();
    let workspace = fixture.temporary.path().join("idempotent workspace");
    fs::create_dir_all(&workspace).unwrap();
    fixture.bind(&workspace, fixture.space_id);
    let before_count = event_count(fixture.store.repository());
    let arguments = workspace_proposal_arguments(&workspace, "strictly identical proposal");
    let responses = run_session(
        &mut fixture.server(ClientKind::Codex),
        FixtureFraming::Newline,
        &[
            request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(2, "context_propose", arguments.clone()),
            tool_call(3, "context_propose", arguments),
        ],
    );
    let created = &responses[1]["result"]["structuredContent"];
    let existing = &responses[2]["result"]["structuredContent"];

    assert_eq!(created["status"], "candidate");
    assert_eq!(created["deduplicated"], false);
    assert!(created["batch_id"].as_str().is_some());
    assert!(created["commit_oid"].as_str().is_some());
    assert_eq!(existing["status"], "existing");
    assert_eq!(existing["deduplicated"], true);
    assert_eq!(
        existing["match_reason"],
        "exact_context_revision_draft_match"
    );
    assert!(existing["batch_id"].is_null());
    assert!(existing["commit_oid"].is_null());
    for id in ["space_id", "context_id", "revision_id", "event_id"] {
        assert_eq!(existing[id], created[id], "{id}");
    }
    assert_eq!(
        existing["routing"],
        json!({
            "resolved_space_id": fixture.space_id,
            "source": "workspace_binding"
        })
    );
    assert_eq!(event_count(fixture.store.repository()), before_count + 1);

    let mut changed = workspace_proposal_arguments(&workspace, "strictly identical proposal");
    changed["evidence"][0]["content"]["actual"] = json!("different candidate");
    let changed_response = run_session(
        &mut fixture.server(ClientKind::Cursor),
        FixtureFraming::ContentLength,
        &[
            request(4, "initialize", json!({"protocolVersion": "2024-11-05"})),
            tool_call(5, "context_propose", changed),
        ],
    );
    let changed = &changed_response[1]["result"]["structuredContent"];
    assert_eq!(changed["status"], "candidate");
    assert_ne!(changed["context_id"], created["context_id"]);
    assert_eq!(event_count(fixture.store.repository()), before_count + 2);
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
                "context_propose",
                json!({
                    "context_id": fixture.context_id,
                    "space_id": fixture.space_id,
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
                "context_propose",
                proposal_arguments(fixture.space_id, "Writer must reject dirty managed input"),
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

fn event_paths(repository: &Path) -> BTreeSet<String> {
    git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .filter(|path| path.starts_with("events/"))
        .map(str::to_owned)
        .collect()
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
