use std::io::Cursor;

use sctx_log_service::{Collector, CollectorOptions, InitOptions};
use sctx_mcp::{ClientKind, McpServer};
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
