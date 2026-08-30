use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker};
use sctx_git_store::GitStore;
use sctx_local_state::UserConfigStore;
use serde_json::{Value, json};

fn run_hook(home: &Path, payload: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["hook", "--agent", "codex", "--agent-version", "0.147.0"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.as_mut().unwrap(), payload).unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn mcp(home: &Path, requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "codex"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for request in requests {
        writeln!(child.stdin.as_mut().unwrap(), "{request}").unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[allow(clippy::needless_pass_by_value)]
fn tool(id: u64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
        "name":name,"arguments":arguments
    }})
}

fn repository(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(path)
        .status()
        .unwrap();
    fs::write(path.join("src/contract.rs"), "pub fn contract() {}\n").unwrap();
    fs::canonicalize(path).unwrap()
}

#[test]
fn direct_evidence_reaches_review_with_no_hook_derived_relation() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("direct Evidence workflow");
    let root = home.join(".shared-context");
    fs::create_dir_all(&home).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    let checkout = repository(&home.join("checkout"));
    UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&checkout),
        )
        .unwrap();
    let session = "direct-evidence-relation-workflow";
    assert_eq!(
        run_hook(
            &home,
            &json!({
                "session_id":session,"transcript_path":null,"cwd":checkout,
                "hook_event_name":"SessionStart","model":"gpt-5.6-sol",
                "permission_mode":"default","source":"startup"
            }),
        ),
        json!({"hookSpecificOutput":{
            "hookEventName":"SessionStart",
            "additionalContext":shared_context_activation_marker(AgentKind::Codex, session)
        }})
    );
    let post_tool = run_hook(
        &home,
        &json!({
            "session_id":session,"transcript_path":"/tmp/RAW_RELATION.jsonl","cwd":checkout,
            "hook_event_name":"PostToolUse","model":"gpt-5.6-sol","permission_mode":"default",
            "turn_id":"turn-relation",
            "tool_name":"Read","tool_use_id":"tool-relation",
            "tool_input":{"absolute_file_path":checkout.join("src/contract.rs")},
            "tool_response":{"output":"RAW_RELATION_OUTPUT"}
        }),
    );
    assert!(
        post_tool["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("task_intent_update"))
    );
    let responses = mcp(
        &home,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
            tool(
                2,
                "task_intent_update",
                json!({
                    "agent_kind":"codex","external_session_id":session,"task_boundary":"new",
                    "expected_revision_id":null,"intent":{"goal":"persist direct validation Evidence"}
                }),
            ),
            tool(
                3,
                "task_checkpoint",
                json!({
                    "agent_kind":"codex","external_session_id":session,
                    "claims":[{
                        "context_kind":"validation","statement":"The contract implementation passed",
                        "rationale":"The Agent retained the focused conclusion","conditions":[],
                        "evidence":[{"evidence_type":"experiment_record","summary":"contract test passed","limitations":[]}]
                    }],"unknowns":[]
                }),
            ),
            tool(
                4,
                "candidate_list",
                json!({
                    "agent_kind": "codex",
                    "external_session_id": session,
                    "detail_level": "full"
                }),
            ),
        ],
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["candidate_build"]["status"],
        "pending"
    );
    assert_eq!(
        responses[3]["result"]["structuredContent"]["reviews"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        responses[3]["result"]["structuredContent"]["reviews"][0]["content"]["relations"],
        json!([])
    );
    let review = responses[3]["result"]["structuredContent"].to_string();
    assert!(!review.contains("RAW_RELATION"));
    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!root.join("state").join(removed).exists());
    }
}
