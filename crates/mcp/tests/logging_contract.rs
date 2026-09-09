use std::{fs, io::Cursor};

use sctx_domain::{ExternalSessionLocator, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::{AuthorizedSessionScopeStore, UserConfigStore};
use sctx_log_service::{Collector, CollectorOptions, InitOptions};
use sctx_mcp::{
    ClientKind, ExpectedRevisionId, McpServer, TaskBoundary, TaskIntentUpdateInput,
    task_intent_update_at_root,
};
use serde_json::{Value, json};

#[test]
fn typed_tool_failure_is_logged_before_the_is_error_envelope() {
    let temporary = tempfile::tempdir().unwrap();
    let logs_root = temporary.path().join("logs");
    sctx_log_service::init(
        &logs_root,
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .unwrap();
    let mut collector = Collector::open(&logs_root, CollectorOptions::default()).unwrap();

    let mut server = McpServer::new(
        temporary.path().join("missing-business-root"),
        ClientKind::Codex,
    )
    .unwrap();
    server.set_telemetry_root_for_test(Some(logs_root.clone()));
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{
                "name":"context_search",
                "arguments":{
                    "agent_kind":"codex",
                    "external_session_id":"logging-contract-session",
                    "query":"fixed test query"
                }
            }
        }),
    ];
    let input = requests
        .iter()
        .map(|request| serde_json::to_string(request).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut output = Vec::new();
    server
        .serve(&mut Cursor::new(input.into_bytes()), &mut output)
        .unwrap();
    let responses = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses[1]["result"]["isError"], json!(true));

    for _ in 0..10 {
        if collector.collect_once().unwrap() >= 2 {
            break;
        }
    }
    let batch = collector.seal().unwrap().expect("telemetry batch");
    let events = std::fs::read_to_string(batch.events_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let finished = events
        .iter()
        .find(|stored| stored["event"]["kind"] == json!("tool_finished"))
        .expect("tool finish event");
    assert_eq!(finished["event"]["outcome"], json!("failure"));
    assert_eq!(finished["event"]["error_family"], json!("external_error"));
    assert_eq!(
        finished["event"]["error_code"],
        json!("maintenance_unavailable")
    );
    assert_eq!(finished["event"]["operation"], json!("context_search"));
    assert!(finished["event"]["session_digest"].as_str().is_some());
}

#[test]
#[allow(clippy::too_many_lines)]
fn accepted_checkpoint_operation_id_survives_response_wire_and_collector() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("business-root");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let checkout = fs::canonicalize(store.repository()).unwrap();
    UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&checkout),
        )
        .unwrap();
    let session = "checkpoint-telemetry-contract";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(&locator, &catalog, &checkout)
        .unwrap();
    task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("verify checkpoint telemetry identity").unwrap(),
        },
    )
    .unwrap();

    let logs_root = temporary.path().join("logs");
    sctx_log_service::init(
        &logs_root,
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .unwrap();
    let mut collector = Collector::open(&logs_root, CollectorOptions::default()).unwrap();
    let mut server = McpServer::new(&root, ClientKind::Codex).unwrap();
    server.set_telemetry_root_for_test(Some(logs_root.clone()));
    let common = json!({
        "agent_kind": "codex",
        "external_session_id": session,
    });
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"task_checkpoint","arguments":{
                "agent_kind": common["agent_kind"],
                "external_session_id": common["external_session_id"],
                "claims": [], "unknowns": []
            }}
        }),
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"task_checkpoint","arguments":{
                "agent_kind": common["agent_kind"],
                "external_session_id": common["external_session_id"],
                "claims": [{
                    "context_kind": "validation",
                    "statement": "The full operation identity crosses the telemetry wire",
                    "rationale": "The collector must correlate this accepted checkpoint",
                    "conditions": ["bounded atomic frame"],
                    "evidence": [{
                        "evidence_type": "experiment_record",
                        "summary": "MCP response and collector event match",
                        "limitations": []
                    }]
                }],
                "unknowns": []
            }}
        }),
    ];
    let input = requests
        .iter()
        .map(|request| serde_json::to_string(request).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut output = Vec::new();
    server
        .serve(&mut Cursor::new(input.into_bytes()), &mut output)
        .unwrap();
    let responses = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        responses[1]["result"]["structuredContent"]["status"],
        "no_op"
    );
    let operation_id = responses[2]["result"]["structuredContent"]["operation_id"]
        .as_str()
        .unwrap();
    assert_eq!(operation_id.len(), 71);
    assert!(operation_id.starts_with("sha256:"));
    assert!(
        operation_id[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );

    for _ in 0..10 {
        if collector.collect_once().unwrap() >= 4 {
            break;
        }
    }
    let batch = collector.seal().unwrap().expect("telemetry batch");
    let events = fs::read_to_string(batch.events_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let finished = events
        .iter()
        .filter(|stored| {
            stored["event"]["kind"] == json!("tool_finished")
                && stored["event"]["operation"] == json!("task_checkpoint")
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0]["event"]["operation_id"], Value::Null);
    assert_eq!(finished[1]["event"]["operation_id"], operation_id);
}
