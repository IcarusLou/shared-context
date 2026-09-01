use std::{
    fs,
    io::{BufReader, Cursor},
    time::{Duration, Instant},
};

use sctx_domain::{ExternalSessionLocator, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::{AuthorizedSessionScopeStore, UserConfigStore};
use sctx_mcp::{
    ClientKind, DisconnectReason, ExpectedRevisionId, McpServer, TaskBoundary,
    TaskIntentUpdateInput, task_intent_update_at_root,
};
use serde_json::{Value, json};

const SAMPLES: usize = 32;

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

/// The 800ms bound is the documented relaxed *local* budget, not a latency SLO. This sample runs
/// beside the rest of the suite and pays real Index and Git reads on every call, so the assertion
/// is deliberately an order of magnitude above the steady-state cost; it exists to catch an
/// order-of-magnitude regression in the Candidate listing path — a lost cache, a re-opened
/// Runtime, a full-history scan per call — rather than to certify how fast `candidate_list` is.
#[test]
fn thirty_two_sequential_candidate_listings_stay_within_the_relaxed_local_budget() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("candidate list performance");
    let store = GitStore::bootstrap_local(&root).unwrap();
    let checkout_path = fs::canonicalize(store.repository()).unwrap();
    let _repository = UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&checkout_path),
        )
        .unwrap()
        .repository;
    let session = "candidate-listing-performance";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog_wait()
        .unwrap();
    AuthorizedSessionScopeStore::initialize(&root)
        .unwrap()
        .try_authorize_missing(&locator, &catalog, &checkout_path)
        .unwrap();
    task_intent_update_at_root(
        &root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("measure repeated Candidate listings").unwrap(),
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
    let mut durations = Vec::with_capacity(SAMPLES);
    for iteration in 0..SAMPLES {
        // The default `scope` is the Task-local listing, which is the shape every client pays for.
        let arguments = json!({
            "agent_kind": "codex",
            "external_session_id": session
        });
        let started = Instant::now();
        let response = run(
            &mut server,
            &[tool_call(
                u64::try_from(iteration).unwrap() + 2,
                "candidate_list",
                arguments,
            )],
        );
        durations.push(started.elapsed());
        assert!(response[0]["result"]["structuredContent"].is_object());
    }
    durations.sort_unstable();
    let p95 = durations[(SAMPLES * 95).div_ceil(100) - 1];
    eprintln!("candidate_list p95={p95:?}");
    assert!(
        p95 <= Duration::from_millis(800),
        "candidate_list p95 {p95:?} exceeds the relaxed 800ms local budget"
    );
}
