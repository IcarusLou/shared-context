use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker_with_policy};
use sctx_git_store::GitStore;
use sctx_local_state::Policy;
use serde_json::{Value, json};

const CODEX_FIXTURE: &str = include_str!("../../../fixtures/agents/codex-0.147.json");
const BOOTSTRAP_REMINDER: &str = "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.";

fn run_json_cli(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_hook(home: &Path, payload: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["hook", "--agent", "codex", "--agent-version", "0.147.0"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&serde_json::to_vec(payload).unwrap())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn mcp_tool(home: &Path, session: &str, name: &str, arguments: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "codex"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05"}
    });
    let mut arguments = arguments;
    let object = arguments.as_object_mut().unwrap();
    object
        .entry("agent_kind".to_owned())
        .or_insert_with(|| json!("codex"));
    object
        .entry("external_session_id".to_owned())
        .or_insert_with(|| json!(session));
    let call = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    });
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{initialize}").unwrap();
    writeln!(stdin, "{call}").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "MCP {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2, "{responses:#?}");
    assert_eq!(
        responses[1]["result"]["isError"], false,
        "session={session} tool={name} responses={responses:#?}"
    );
    responses[1]["result"]["structuredContent"].clone()
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_repository(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src/quality")).unwrap();
    fs::write(
        path.join("src/quality/fallback.rs"),
        "pub fn retrieval_quality_fallback() {}\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Retrieval Quality Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "Add retrieval quality fixture",
        ],
    );
    fs::canonicalize(path).unwrap()
}

fn codex_event(index: usize, session: &str, checkout: &Path) -> Value {
    let mut payload = serde_json::from_str::<Vec<Value>>(CODEX_FIXTURE)
        .unwrap()
        .remove(index);
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(checkout);
    payload["transcript_path"] = Value::Null;
    payload
}

fn session_start(session: &str, checkout: &Path) -> Value {
    codex_event(0, session, checkout)
}

fn post_tool(session: &str, checkout: &Path, sequence: usize) -> Value {
    let mut payload = codex_event(2, session, checkout);
    payload["tool_use_id"] = json!(format!("bootstrap-tool-{sequence}"));
    payload["tool_input"] = json!({
        "file_path": checkout.join("src/quality/fallback.rs")
    });
    payload["tool_response"] = json!({"output": "SANITIZED_BOOTSTRAP_OUTPUT"});
    payload
}

fn claim(statement: &str, suffix: &str) -> Value {
    json!({
        "context_kind": "decision",
        "statement": statement,
        "rationale": format!("Independent Claim {suffix} validates the retrieval quality boundary"),
        "conditions": [],
        "evidence": [{
            "evidence_type": "experiment_record",
            "summary": format!("Retrieval quality Claim {suffix} passed"),
            "limitations": ["sanitized M2 acceptance fixture"]
        }]
    })
}

fn proposed(review: &Value) -> (&str, &str, &Value) {
    let recommendation = review["space_recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recommendation| recommendation["kind"] == "proposed_new_space_intent")
        .unwrap();
    (
        recommendation["recommendation_id"].as_str().unwrap(),
        recommendation["proposed_space_group_key"].as_str().unwrap(),
        &recommendation["proposed_new_space_intent"],
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_m2_retrieval_quality_workflow() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("M2 retrieval quality home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".shared-context");
    GitStore::bootstrap_local(&root).unwrap();
    let checkout = initialize_repository(&home.join("checkout"));
    run_json_cli(
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            "Server",
            "--path",
            checkout.to_str().unwrap(),
        ],
    );
    let session = "m2-retrieval-quality-session";
    assert_eq!(
        run_hook(&home, &session_start(session, &checkout)),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, session)
        }})
    );
    assert!(
        shared_context_activation_marker(AgentKind::Codex, session).contains("task_intent_update")
    );
    assert_eq!(
        run_hook(&home, &post_tool(session, &checkout, 1)),
        json!({
            "systemMessage": BOOTSTRAP_REMINDER,
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": BOOTSTRAP_REMINDER
            }
        })
    );
    assert_eq!(
        run_hook(&home, &post_tool(session, &checkout, 2)),
        json!({})
    );

    let grouped_task = mcp_tool(
        &home,
        session,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {
                "goal": "System suggestion: Consolidate retrieval quality reviews",
                "acceptance_conditions": ["two Claims reuse one proposed Space"]
            }
        }),
    );
    let alpha_statement =
        "Server src/quality/fallback.rs qualitystrong exact phrase remains authoritative";
    let beta_statement = "unrelated sibling material remains independently reviewable";
    let checkpoint = mcp_tool(
        &home,
        session,
        "task_checkpoint",
        json!({
            "claims": [claim(alpha_statement, "alpha"), claim(beta_statement, "beta")],
            "unknowns": []
        }),
    );
    assert_eq!(checkpoint["candidate_build"]["status"], "pending");
    let recovered = mcp_tool(
        &home,
        session,
        "candidate_list",
        json!({"status": "pending", "limit": 10, "token_budget": 32768}),
    );
    let candidate_ids = recovered["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|review| review["candidate_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(candidate_ids.len(), 2);
    // Candidate identities are random, so the bounded page order never follows Claim order.
    // Order the two Reviews by their Claim so the rest of the workflow stays deterministic.
    let mut reviews = candidate_ids
        .iter()
        .map(|candidate_id| {
            (
                candidate_id.clone(),
                mcp_tool(
                    &home,
                    session,
                    "candidate_get",
                    json!({"candidate_id": candidate_id}),
                ),
            )
        })
        .collect::<Vec<_>>();
    reviews.sort_by_key(|(_, review)| review["content"]["statement"] != alpha_statement);
    let candidate_ids = reviews
        .iter()
        .map(|(candidate_id, _)| candidate_id.clone())
        .collect::<Vec<_>>();
    let first_review = reviews[0].1.clone();
    let second_review = reviews[1].1.clone();
    assert_eq!(first_review["content"]["statement"], alpha_statement);
    assert_eq!(second_review["content"]["statement"], beta_statement);
    let first_proposed = proposed(&first_review);
    let second_proposed = proposed(&second_review);
    assert_eq!(first_proposed.1, second_proposed.1);
    assert_eq!(first_proposed.2, second_proposed.2);
    assert_eq!(
        first_proposed.2["title"],
        "Consolidate retrieval quality reviews"
    );

    let first_confirmed = mcp_tool(
        &home,
        session,
        "candidate_confirm",
        json!({
            "expected_task_id": grouped_task["task_id"],
            "expected_intent_revision_id": grouped_task["intent_revision_id"],
            "candidate_id": candidate_ids[0],
            "expected_review_version": first_review["review_version"],
            "primary": {"new_space_recommendation_id": first_proposed.0},
            "related_space_ids": []
        }),
    );
    // Four base Confirmation Events, one created Space, and one server-derived Engineering
    // Reference for the `src/quality/fallback.rs` locator the Claim statement already names.
    assert_eq!(first_confirmed["event_ids"].as_array().unwrap().len(), 6);
    let mapped_second = mcp_tool(
        &home,
        session,
        "candidate_get",
        json!({"candidate_id": candidate_ids[1]}),
    );
    assert!(
        !mapped_second["space_recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|recommendation| recommendation["kind"] == "proposed_new_space_intent")
    );
    assert!(
        mapped_second["space_recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|recommendation| {
                recommendation["kind"] == "existing"
                    && recommendation["role"] == "primary"
                    && recommendation["space_id"] == first_confirmed["primary_space_id"]
            })
    );
    let second_confirmed = mcp_tool(
        &home,
        session,
        "candidate_confirm",
        json!({
            "expected_task_id": grouped_task["task_id"],
            "expected_intent_revision_id": grouped_task["intent_revision_id"],
            "candidate_id": candidate_ids[1],
            "expected_review_version": mapped_second["review_version"],
            "primary": {"existing_space_id": first_confirmed["primary_space_id"]},
            "related_space_ids": []
        }),
    );
    assert_eq!(second_confirmed["event_ids"].as_array().unwrap().len(), 4);
    assert_eq!(
        second_confirmed["primary_space_id"],
        first_confirmed["primary_space_id"]
    );

    let strong_task = mcp_tool(
        &home,
        session,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": grouped_task["intent_revision_id"],
            "intent": {"goal": "qualitystrong exact phrase"},
            "detail_level": "full"
        }),
    );
    let strong_items = strong_task["items"].as_array().unwrap();
    assert!(strong_items.iter().any(|item| {
        item["context"]["context_id"] == first_confirmed["context_id"]
            && item["retrieval_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path["source"] == "context_fts")
    }));
    assert!(
        strong_items
            .iter()
            .all(|item| item["context"]["context_id"] != second_confirmed["context_id"])
    );

    let generic_task = mcp_tool(
        &home,
        session,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": strong_task["intent_revision_id"],
            "intent": {"goal": "the code file task"}
        }),
    );
    assert!(generic_task["items"].as_array().unwrap().is_empty());

    let explicit = run_json_cli(
        &home,
        &[
            "search",
            "--query",
            "unrelated sibling material",
            "--status",
            "accepted",
        ],
    );
    assert!(
        explicit["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["context_id"] == second_confirmed["context_id"])
    );

    let focus_task = mcp_tool(
        &home,
        session,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": generic_task["intent_revision_id"],
            "intent": {"goal": "zzzzabsentfocusgoal"}
        }),
    );
    let focused = mcp_tool(
        &home,
        session,
        "task_artifact_focus",
        json!({
            "expected_revision_id": focus_task["intent_revision_id"],
            "absolute_file_path": checkout.join("src/quality/fallback.rs"),
            "locator": {"locator_kind": "file"},
            "token_budget": 12000,
            "max_spaces": 8,
            "detail_level": "full"
        }),
    );
    // The Claim statement names `src/quality/fallback.rs`, so confirmation records a
    // server-derived Engineering Reference for it and the Focus now resolves through the
    // Engineering Graph instead of the text fallback. The text-fallback path itself stays
    // covered by `sctx-search`'s graph_retrieval suite and the MCP engineering workflow.
    assert!(focused["context"]["artifact_generation"].is_string());
    let focused_item = focused["context"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["context"]["context_id"] == first_confirmed["context_id"])
        .unwrap_or_else(|| panic!("missing public focused item: {focused:#}"));
    assert!(
        focused_item["retrieval_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["source"] == "engineering_graph")
    );
    assert!(
        focused["context"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["context"]["context_id"] != second_confirmed["context_id"]),
        "the unreferenced sibling Context must stay out of the Focus payload: {focused:#}"
    );
}

/// The activation marker exactly as this installation renders it.
///
/// Protocol text plus the built-in team `## session` policy, which is what an installation with
/// no `policy.md` -- every temporary root in this file -- actually delivers.
fn shared_context_activation_marker(agent: AgentKind, external_session_id: &str) -> String {
    shared_context_activation_marker_with_policy(
        agent,
        external_session_id,
        Policy::compiled_default().session(),
    )
}
