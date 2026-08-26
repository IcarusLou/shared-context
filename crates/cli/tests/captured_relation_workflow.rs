use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_git_store::GitStore;
use serde_json::{Value, json};

const CODEX_FIXTURE: &str = include_str!("../../../fixtures/agents/codex-0.147.json");
const CURSOR_FIXTURE: &str = include_str!("../../../fixtures/agents/cursor-3.13.json");

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
    run_agent_hook(home, "codex", "0.147.0", payload)
}

fn run_agent_hook(home: &Path, agent: &str, version: &str, payload: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["hook", "--agent", agent, "--agent-version", version])
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
    mcp_agent_tool(home, session, "codex", name, arguments)
}

fn mcp_agent_tool(home: &Path, session: &str, agent: &str, name: &str, arguments: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", agent])
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
        .or_insert_with(|| json!(agent));
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

fn git(path: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn initialize_repository(path: &Path, source: &str) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("src/contract.rs"), source).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["add", "src/contract.rs"]);
    git(
        path,
        &[
            "-c",
            "user.name=Shared Context Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "Add captured relation fixture",
        ],
    );
    fs::canonicalize(path).unwrap()
}

fn session_start(session: &str, checkout: &Path) -> Value {
    let mut payload = serde_json::from_str::<Vec<Value>>(CODEX_FIXTURE)
        .unwrap()
        .remove(0);
    payload["session_id"] = json!(session);
    payload["transcript_path"] = Value::Null;
    payload["cwd"] = json!(checkout);
    payload
}

fn cursor_session_start(session: &str, checkout: &Path) -> Value {
    let mut payload = serde_json::from_str::<Vec<Value>>(CURSOR_FIXTURE)
        .unwrap()
        .remove(0);
    payload["conversation_id"] = json!(session);
    payload["generation_id"] = json!(format!("generation-{session}"));
    payload["session_id"] = json!(session);
    payload["workspace_roots"] = json!([checkout]);
    payload
}

fn post_tool(session: &str, checkout: &Path, tool: &str, raw: &str) -> Value {
    let mut payload = serde_json::from_str::<Vec<Value>>(CODEX_FIXTURE)
        .unwrap()
        .remove(2);
    payload["session_id"] = json!(session);
    payload["transcript_path"] = json!(format!("/tmp/{raw}.jsonl"));
    payload["cwd"] = json!(checkout);
    payload["turn_id"] = json!(format!("turn-{session}"));
    payload["tool_name"] = json!(tool);
    payload["tool_use_id"] = json!(format!("tool-{tool}"));
    payload["tool_input"] = json!({
        "file_path": checkout.join("src/contract.rs"),
        "command": raw
    });
    payload["tool_response"] = json!({"output": raw});
    payload
}

fn create_space(home: &Path, title: &str, needle: &str) -> String {
    run_json_cli(
        home,
        &[
            "space",
            "create",
            "--title",
            title,
            "--problem",
            &format!("{needle} problem"),
            "--desired-outcome",
            &format!("{needle} outcome"),
            "--in-scope",
            needle,
            "--acceptance-condition",
            &format!("{needle} accepted"),
        ],
    )["data"]["space_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn event_count(repository: &Path) -> usize {
    git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .filter(|path| path.starts_with("events/"))
        .count()
}

fn git_contains(repository: &Path, needle: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["grep", "-q", needle, "HEAD", "--", "events"])
        .status()
        .unwrap()
        .success()
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_capture_relation_reference_and_related_space_chain_crosses_checkout_paths() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("公开主链 home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".shared-context");
    GitStore::bootstrap_local(&root).unwrap();
    let source = "pub fn captured_relation_contract() {}\n";
    let checkout_a = initialize_repository(&home.join("checkout A"), source);
    let session_a = "captured-relation-session-a";
    let raw_file = "RAW_CAPTURED_RELATION_FILE_OUTPUT";
    let raw_test = "RAW_CAPTURED_RELATION_TEST_OUTPUT";
    let repository = run_json_cli(
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            "Server",
            "--path",
            checkout_a.to_str().unwrap(),
        ],
    );
    assert_eq!(
        repository["data"]["catalog"]["repository"]["repository_id"],
        "Server"
    );
    assert_eq!(
        run_hook(&home, &session_start(session_a, &checkout_a)),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": SHARED_CONTEXT_ACTIVATION_MARKER
        }})
    );
    let primary_space = create_space(&home, "Primary Workflow", "primaryworkflowneedle");
    let related_space = create_space(&home, "Related Workflow", "relatedworkflowneedle");

    let target_task = mcp_tool(
        &home,
        session_a,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "Create the relation target Context"}
        }),
    );
    let target_checkpoint = mcp_tool(
        &home,
        session_a,
        "task_checkpoint",
        json!({
            "expected_task_id": target_task["task_id"],
            "expected_intent_revision_id": target_task["intent_revision_id"],
            "expected_episode_version": 0,
            "boundary": "close",
            "claims": [{
                "context_kind_hint": "contract",
                "topic_key_hint": "m1/target-contract",
                "statement": "The target contract remains stable across implementations",
                "rationale": "The target is independently validated before relation creation",
                "applicability": {"domains": ["search"], "platforms": ["server"], "conditions": []},
                "assumptions": [],
                "recheck_when": ["the target contract changes"],
                "evidence": [{
                    "kind": "inline_validation",
                    "evidence": {
                        "kind": "experiment_record",
                        "supports": "The target contract fixture passed",
                        "content": {"actual": "passed"},
                        "interpretation": "The target is reviewable",
                        "limitations": ["The evidence is limited to this sanitized acceptance fixture"]
                    }
                }],
                "artifact_refs": [],
                "relations": [],
                "engineering_references": [],
                "related_contexts": []
            }],
            "unknowns": []
        }),
    );
    let target_candidate = target_checkpoint["candidate_build"]["items"][0]["candidate_id"]
        .as_str()
        .unwrap();
    let target_review = mcp_tool(
        &home,
        session_a,
        "candidate_get",
        json!({"candidate_id": target_candidate}),
    );
    assert_eq!(target_review["ready_for_review"], true, "{target_review:#}");
    let target_confirmed = mcp_tool(
        &home,
        session_a,
        "candidate_confirm",
        json!({
            "expected_task_id": target_task["task_id"],
            "expected_intent_revision_id": target_task["intent_revision_id"],
            "candidate_id": target_candidate,
            "expected_review_version": target_review["review_version"],
            "primary": {"existing_space_id": primary_space},
            "related_space_ids": []
        }),
    );
    assert_eq!(target_confirmed["event_ids"].as_array().unwrap().len(), 4);

    let main_task = mcp_tool(
        &home,
        session_a,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": target_task["intent_revision_id"],
            "intent": {"goal": "Capture work and persist its relation and engineering reference"}
        }),
    );
    assert_eq!(
        run_hook(&home, &post_tool(session_a, &checkout_a, "Read", raw_file)),
        json!({})
    );
    assert_eq!(
        run_hook(
            &home,
            &post_tool(session_a, &checkout_a, "M1CapturedRelationTest", raw_test)
        ),
        json!({})
    );
    let captures = mcp_tool(&home, session_a, "task_capture_list", json!({"limit": 10}));
    let capture_ids = captures["captures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|capture| capture["capture_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(capture_ids.len(), 2);
    let checkpoint = mcp_tool(
        &home,
        session_a,
        "task_checkpoint",
        json!({
            "expected_task_id": main_task["task_id"],
            "expected_intent_revision_id": main_task["intent_revision_id"],
            "expected_episode_version": 0,
            "boundary": "close",
            "claims": [{
                "context_kind_hint": "validation",
                "topic_key_hint": "m1/captured-relation",
                "statement": "Captured implementation satisfies the stable target contract",
                "rationale": "Real file and test Captures support the implementation decision",
                "applicability": {"domains": ["search"], "platforms": ["server"], "conditions": []},
                "assumptions": [],
                "recheck_when": ["src/contract.rs moves or its test fails"],
                "evidence": capture_ids.iter().map(|capture_id| {
                    json!({"kind": "capture", "capture_id": capture_id})
                }).collect::<Vec<_>>(),
                "artifact_refs": [],
                "relations": [{
                    "target_context_id": target_confirmed["context_id"],
                    "kind": "implements",
                    "rationale": "The captured implementation realizes the target contract",
                    "supports": ["The file and test Captures were explicitly selected"]
                }],
                "engineering_references": [{
                    "repository_id": "Server",
                    "artifact_kind": "file",
                    "relation": "implements",
                    "locator": {"locator_kind": "file", "path": "src/contract.rs"},
                    "supports": "The tracked file implements the captured Context",
                    "limitations": []
                }],
                "related_contexts": [{
                    "context_id": target_confirmed["context_id"],
                    "revision_id": target_confirmed["revision_id"]
                }]
            }],
            "unknowns": []
        }),
    );
    assert_eq!(checkpoint["episode_version"], 3);
    assert_eq!(checkpoint["diagnostics"].as_array().unwrap().len(), 2);
    let candidate_id = checkpoint["candidate_build"]["items"][0]["candidate_id"]
        .as_str()
        .unwrap();
    let review = mcp_tool(
        &home,
        session_a,
        "candidate_get",
        json!({"candidate_id": candidate_id}),
    );
    assert_eq!(review["content"]["relations"][0]["kind"], "implements");
    assert_eq!(
        review["engineering_references"][0]["repository_id"],
        "Server"
    );
    assert_eq!(review["untrusted_data"], true);
    assert_eq!(review["ready_for_review"], true, "{review:#}");
    let review_text = serde_json::to_string(&review).unwrap();
    assert!(!review_text.contains(raw_file));
    assert!(!review_text.contains(raw_test));
    let knowledge = root.join("repository");
    let before_confirm = event_count(&knowledge);
    let confirmed = mcp_tool(
        &home,
        session_a,
        "candidate_confirm",
        json!({
            "expected_task_id": main_task["task_id"],
            "expected_intent_revision_id": main_task["intent_revision_id"],
            "candidate_id": candidate_id,
            "expected_review_version": review["review_version"],
            "primary": {"existing_space_id": primary_space},
            "related_space_ids": [related_space]
        }),
    );
    assert_eq!(confirmed["event_ids"].as_array().unwrap().len(), 5);
    assert_eq!(confirmed["graph_rebuild_pending"], false);
    assert_eq!(event_count(&knowledge), before_confirm + 5);

    let checkout_b = initialize_repository(&home.join("checkout B"), source);
    run_json_cli(
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            "Server",
            "--path",
            checkout_b.to_str().unwrap(),
        ],
    );
    let session_b = "captured-relation-session-b";
    assert_eq!(
        run_agent_hook(
            &home,
            "cursor",
            "3.13.10",
            &cursor_session_start(session_b, &checkout_b),
        ),
        json!({"additional_context": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
    let task_b = mcp_agent_tool(
        &home,
        session_b,
        "cursor",
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "relatedworkflowneedle"}
        }),
    );
    let related_pack = mcp_agent_tool(
        &home,
        session_b,
        "cursor",
        "task_context",
        json!({"token_budget": 12000, "max_spaces": 8}),
    );
    let related_item = related_pack["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["context"]["context_id"] == confirmed["context_id"])
        .unwrap();
    assert_eq!(related_item["association_space_id"], related_space);
    assert_eq!(related_item["context"]["space_id"], primary_space);
    assert!(
        related_item["retrieval_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| {
                path["source"] == "space_association"
                    && path["role"] == "related"
                    && path["matched_space_id"] == related_space
            })
    );
    let focused = mcp_agent_tool(
        &home,
        session_b,
        "cursor",
        "task_artifact_focus",
        json!({
            "expected_revision_id": task_b["intent_revision_id"],
            "absolute_file_path": checkout_b.join("src/contract.rs"),
            "locator": {"locator_kind": "file"},
            "token_budget": 12000,
            "max_spaces": 8
        }),
    );
    assert_eq!(focused["resolved_focus"]["repository_id"], "Server");
    assert_eq!(
        focused["resolved_focus"]["locator"]["path"],
        "src/contract.rs"
    );
    let focused_items = focused["context"]["items"].as_array().unwrap();
    let focused_main = focused_items
        .iter()
        .find(|item| item["context"]["context_id"] == confirmed["context_id"])
        .unwrap_or_else(|| panic!("missing focused source: {focused:#}"));
    assert!(
        focused_main["retrieval_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["source"] == "engineering_graph"
                && path["relation_hops"].as_array().unwrap().is_empty())
    );
    let focused_target = focused_items
        .iter()
        .find(|item| item["context"]["context_id"] == target_confirmed["context_id"])
        .unwrap_or_else(|| panic!("missing focused relation target: {focused:#}"));
    let relation_path = focused_target["retrieval_paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|path| {
            path["source"] == "engineering_graph"
                && path["relation_hops"].as_array().unwrap().len() == 1
        })
        .unwrap_or_else(|| panic!("missing one-hop relation path: {focused:#}"));
    let relation_hop = &relation_path["relation_hops"][0];
    assert_eq!(relation_hop["kind"], "implements");
    assert_eq!(relation_hop["source_context_id"], confirmed["context_id"]);
    assert_eq!(
        relation_hop["target_context_id"],
        target_confirmed["context_id"]
    );

    for forbidden in [
        raw_file,
        raw_test,
        checkout_a.to_str().unwrap(),
        checkout_b.to_str().unwrap(),
    ] {
        assert!(!git_contains(&knowledge, forbidden));
    }
}
