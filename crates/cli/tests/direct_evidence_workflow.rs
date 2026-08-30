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
use tempfile::{TempDir, tempdir};

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("direct evidence home");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn hook(&self, payload: &Value) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["hook", "--agent", "codex", "--agent-version", "0.147.0"])
            .env("HOME", &self.home)
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
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn mcp(&self, requests: &[Value]) -> Vec<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["mcp", "serve", "--client", "codex"])
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.as_mut().unwrap();
        for request in requests {
            stdin
                .write_all(&serde_json::to_vec(request).unwrap())
                .unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[allow(clippy::needless_pass_by_value)]
fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[allow(clippy::needless_pass_by_value)]
fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    rpc(
        id,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
}

fn git_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    fs::write(path.join("src/feature.rs"), "pub fn feature() {}\n").unwrap();
    fs::canonicalize(path).unwrap()
}

fn session_start(session_id: &str, cwd: &Path) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
    })
}

fn post_tool(session_id: &str, cwd: &Path, file: &Path, raw: &str) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": format!("/tmp/{raw}.jsonl"),
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": format!("turn-{session_id}"),
        "tool_name": "Read",
        "tool_use_id": format!("tool-{session_id}"),
        "tool_input": {"file_path": file},
        "tool_response": {"output": raw}
    })
}

#[test]
fn hook_writes_zero_mechanical_state_and_direct_evidence_builds_candidate() {
    let harness = Harness::new();
    GitStore::bootstrap_local(harness.root()).unwrap();
    let repository = git_repo(&harness.home.join("repository"));
    UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    let session = "zero-mechanical-state-direct-evidence";
    assert_eq!(
        harness.hook(&session_start(session, &repository)),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, session)
        }})
    );
    let started = harness.mcp(&[
        rpc(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
        tool_call(
            2,
            "task_intent_update",
            json!({
                "agent_kind": "codex", "external_session_id": session,
                "task_boundary": "new", "expected_revision_id": null,
                "intent": {"goal": "persist direct Agent Evidence"}
            }),
        ),
    ]);
    assert_eq!(started[1]["result"]["isError"], false);
    let raw = "RAW_TOOL_OUTPUT_MUST_NOT_PERSIST";
    assert_eq!(
        harness.hook(&post_tool(
            session,
            &repository,
            &repository.join("src/feature.rs"),
            raw,
        )),
        json!({})
    );
    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!harness.root().join("state").join(removed).exists());
    }

    let workflow = harness.mcp(&[
        rpc(3, "initialize", json!({"protocolVersion": "2024-11-05"})),
        tool_call(
            4,
            "task_checkpoint",
            json!({
                "agent_kind": "codex", "external_session_id": session,
                "claims": [{
                    "context_kind": "validation",
                    "statement": "Direct Evidence remains reviewable",
                    "rationale": "The Agent selected the focused engineering conclusion",
                    "conditions": [],
                    "evidence": [{
                        "evidence_type": "experiment_record",
                        "summary": "the direct Evidence workflow passed",
                        "limitations": ["local fixture"]
                    }]
                }],
                "unknowns": []
            }),
        ),
        tool_call(
            5,
            "candidate_list",
            json!({
                "agent_kind": "codex",
                "external_session_id": session,
                "detail_level": "full"
            }),
        ),
    ]);
    assert_eq!(
        workflow[1]["result"]["structuredContent"]["status"],
        "accepted"
    );
    assert_eq!(
        workflow[1]["result"]["structuredContent"]["candidate_build"]["status"],
        "pending"
    );
    let review = &workflow[2]["result"]["structuredContent"]["reviews"][0];
    assert_eq!(
        review["content"]["evidence"][0]["content"]["summary"],
        "the direct Evidence workflow passed"
    );
    let persisted = serde_json::to_string(review).unwrap();
    assert!(!persisted.contains(raw));
    assert!(!persisted.contains(repository.to_str().unwrap()));

    let listed = harness.mcp(&[
        rpc(6, "initialize", json!({"protocolVersion": "2024-11-05"})),
        rpc(7, "tools/list", json!({})),
    ]);
    let tools = listed[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 17);
    assert!(tools.iter().all(|tool| tool["name"] != "task_capture_list"));
}
