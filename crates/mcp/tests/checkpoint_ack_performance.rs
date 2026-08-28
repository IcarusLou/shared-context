use std::{
    fs,
    io::{BufReader, Cursor},
    time::{Duration, Instant},
};

use sctx_domain::{ExternalSessionLocator, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::{
    ActivationScope, ActivationScopeDecision, AuthorizedSessionScopeStore, UserConfigStore,
};
use sctx_mcp::{
    ClientKind, DisconnectReason, ExpectedRevisionId, McpServer, TaskBoundary,
    TaskIntentUpdateInput, task_intent_update_at_root,
};
use serde_json::{Value, json};

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

fn run(server: &mut McpServer, requests: &[Value]) -> Vec<Value> {
    let mut input = requests
        .iter()
        .map(|request| serde_json::to_string(request).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    input.push(b'\n');
    let mut reader = BufReader::new(Cursor::new(input));
    let mut output = Vec::new();
    let outcome = server.serve(&mut reader, &mut output).unwrap();
    assert_eq!(outcome.disconnect, DisconnectReason::CleanEof);
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn one_hundred_distinct_checkpoint_creations_meet_durable_ack_slo() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("checkpoint ACK performance");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let checkout_path = fs::canonicalize(store.repository()).unwrap();
    let repository = UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&checkout_path),
        )
        .unwrap()
        .repository;
    let session = "checkpoint-creation-performance";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .authorize(
            &locator,
            &ActivationScope {
                decision: ActivationScopeDecision::Direct {
                    repository_id: repository.repository_id.clone(),
                    checkout_path,
                },
                allowed_repository_ids: vec![repository.repository_id],
            },
            &catalog,
        )
        .unwrap();
    task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("measure distinct durable Checkpoint ACKs").unwrap(),
        },
    )
    .unwrap();
    let mut server = McpServer::new(&root, ClientKind::Codex).unwrap();
    let initialized = run(
        &mut server,
        &[request(
            1,
            "initialize",
            json!({"protocolVersion": "2024-11-05"}),
        )],
    );
    assert!(initialized[0]["result"].is_object());
    let before_events = store.validate_committed_events().unwrap();
    let mut durations = Vec::with_capacity(100);
    let mut operation_ids = std::collections::BTreeSet::new();
    for iteration in 0..100 {
        let arguments = json!({
            "agent_kind": "codex",
            "external_session_id": session,
            "claims": [{
                "context_kind": "validation",
                "statement": format!("Checkpoint creation operation {iteration}"),
                "rationale": "The durable ACK excludes Candidate Build work",
                "conditions": ["distinct creation"],
                "evidence": [{
                    "evidence_type": "experiment_record",
                    "summary": format!("durable creation {iteration} persisted"),
                    "limitations": []
                }]
            }],
            "unknowns": []
        });
        let started = Instant::now();
        let response = run(
            &mut server,
            &[tool_call(
                u64::try_from(iteration).unwrap() + 2,
                "task_checkpoint",
                arguments,
            )],
        );
        durations.push(started.elapsed());
        let response = &response[0]["result"]["structuredContent"];
        assert_eq!(response["status"], "accepted");
        assert_eq!(response["replayed"], false);
        assert_eq!(response["candidate_build"]["status"], "pending");
        assert!(operation_ids.insert(response["operation_id"].as_str().unwrap().to_owned()));
    }
    assert_eq!(operation_ids.len(), 100);
    assert_eq!(store.validate_committed_events().unwrap(), before_events);
    durations.sort_unstable();
    let p95 = durations[94];
    let p99 = durations[98];
    eprintln!("Checkpoint creation ACK p95={p95:?} p99={p99:?}");
    assert!(p95 < Duration::from_millis(250), "ACK p95 was {p95:?}");
    assert!(p99 < Duration::from_millis(500), "ACK p99 was {p99:?}");
}
