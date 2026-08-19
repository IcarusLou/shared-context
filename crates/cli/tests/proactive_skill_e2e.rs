use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::Arc,
};

use sctx_installer::{
    Agent, Architecture, Host, InstallContext, Installer, SetupOptions, SkillStatus,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const EVIDENCE: &str = r#"{"kind":"experiment_record","supports":"the acceptance command completed","content":{"command":"proactive-skill-e2e","actual":"success"},"interpretation":"the behavior is executable through the public boundary","limitations":["local deterministic fixture"]}"#;

#[derive(Clone)]
struct FakeHost;

impl Host for FakeHost {
    fn platform(&self) -> &'static str {
        "macos"
    }

    fn architecture(&self) -> Option<Architecture> {
        Some(Architecture::Arm64)
    }

    fn git_version(&self) -> sctx_installer::Result<String> {
        Ok("git version proactive-skill-e2e".to_owned())
    }

    fn verify_signature(&self, _executable: &Path) -> sctx_installer::Result<()> {
        Ok(())
    }

    fn available_space(&self, _path: &Path) -> sctx_installer::Result<u64> {
        Ok(u64::MAX)
    }

    fn agent_version(&self, agent: Agent) -> Option<String> {
        Some(
            match agent {
                Agent::Cursor => "3.13.10",
                Agent::Codex => "0.147.0",
            }
            .to_owned(),
        )
    }
}

struct Harness {
    temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    workspace: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("用户 HOME with spaces");
        let root = home.join(".shared-context");
        let workspace = temporary.path().join("绑定 workspace 中文");
        let runtime = temporary.path().join("runtime source/sctx");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::write(&runtime, b"signed runtime fixture").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();

        let context = InstallContext::injected(&home, &root, &runtime, "1.0.0-e2e");
        let report = Installer::new(context, Arc::new(FakeHost))
            .setup(&SetupOptions::default())
            .unwrap();
        assert_eq!(report.skill.status, SkillStatus::Installed);
        assert_eq!(
            report.skill.path,
            home.join(".agents/skills/shared-context")
        );

        Self {
            temporary,
            home,
            root,
            workspace,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sctx"))
            .arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("sctx should start")
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn mcp(&self, calls: &[(&str, Value)]) -> Vec<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["mcp", "serve", "--client", "codex"])
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut requests = vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2024-11-05"}
            }),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        ];
        requests.extend(calls.iter().enumerate().map(|(index, (name, arguments))| {
            json!({
                "jsonrpc": "2.0",
                "id": index + 3,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            })
        }));
        let stdin = child.stdin.as_mut().unwrap();
        for request in requests {
            writeln!(stdin, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "MCP failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn event_count(&self) -> usize {
        git(
            &self.root.join("repository"),
            &["ls-tree", "-r", "--name-only", "HEAD"],
        )
        .lines()
        .filter(|path| path.starts_with("events/"))
        .count()
    }
}

fn data<'a>(response: &'a Value, field: &str) -> &'a str {
    response["data"][field].as_str().unwrap()
}

fn tool_data(response: &Value) -> &Value {
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    &response["result"]["structuredContent"]
}

fn proposal(workspace: &Path) -> Value {
    json!({
        "workspace": workspace,
        "kind": "decision",
        "topic_key": "skill/active-capture",
        "statement": "Evidence-backed reusable findings remain Candidates",
        "rationale": "Human governance must remain separate from proactive capture",
        "applicability": {
            "domains": ["agent-integration"],
            "platforms": ["macos"],
            "conditions": ["shared-context skill is active"]
        },
        "assumptions": ["the workspace has one exact binding"],
        "recheck_when": ["the MCP governance surface changes"],
        "evidence": [{
            "kind": "experiment_record",
            "supports": "the Skill-shaped MCP proposal completed",
            "content": {"suite": "proactive_skill_e2e", "actual": "candidate"},
            "interpretation": "the public MCP write boundary creates a Candidate",
            "limitations": ["deterministic harness", "not a live Agent decision"]
        }]
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn setup_skill_mcp_workspace_and_candidate_boundaries_hold_end_to_end() {
    let harness = Harness::new();
    let installed_skill = harness.home.join(".agents/skills/shared-context/SKILL.md");
    let installed_openai = harness
        .home
        .join(".agents/skills/shared-context/agents/openai.yaml");
    assert_eq!(
        fs::read(&installed_skill).unwrap(),
        include_bytes!("../../../skills/shared-context/SKILL.md")
    );
    assert_eq!(
        fs::read(&installed_openai).unwrap(),
        include_bytes!("../../../skills/shared-context/agents/openai.yaml")
    );
    let skill = fs::read_to_string(installed_skill).unwrap();
    assert!(skill.contains("call `context_for_task`"));
    assert!(skill.contains("Call `context_propose` only when"));
    assert!(skill.contains("Never automatically review, accept, publish"));

    let space = harness.success(&[
        "space",
        "create",
        "--title",
        "Proactive Skill acceptance",
        "--problem",
        "Agents need bounded shared context",
        "--desired-outcome",
        "Retrieve accepted context and capture Candidates",
        "--in-scope",
        "Agent integration",
        "--acceptance-condition",
        "MCP boundaries hold",
    ]);
    let space_id = data(&space, "space_id").to_owned();
    harness.success(&[
        "workspace",
        "bind",
        "--workspace",
        harness.workspace.to_str().unwrap(),
        "--space-id",
        &space_id,
    ]);

    let accepted = harness.success(&[
        "context",
        "propose",
        "--space-id",
        &space_id,
        "--kind",
        "decision",
        "--topic-key",
        "skill/active-retrieve",
        "--statement",
        "Bound workspaces retrieve accepted proactive Skill context",
        "--rationale",
        "Task retrieval should prefer the exact local binding",
        "--domain",
        "agent-integration",
        "--evidence-json",
        EVIDENCE,
    ]);
    let accepted_context_id = data(&accepted, "context_id").to_owned();
    let accepted_revision_id = data(&accepted, "revision_id").to_owned();
    let review = harness.success(&[
        "context",
        "review",
        "--space-id",
        &space_id,
        "--context-id",
        &accepted_context_id,
        "--revision-id",
        &accepted_revision_id,
        "--verdict",
        "approve",
        "--reason",
        "verified by the integration harness",
    ]);
    harness.success(&[
        "context",
        "publish",
        "--space-id",
        &space_id,
        "--context-id",
        &accepted_context_id,
        "--revision-id",
        &accepted_revision_id,
        "--expect-no-publication-head",
        "--review-event-id",
        data(&review, "event_id"),
    ]);

    let retrieve = harness.mcp(&[(
        "context_for_task",
        json!({
            "task": "retrieve accepted proactive Skill context",
            "workspace": harness.workspace
        }),
    )]);
    assert_eq!(retrieve.len(), 3);
    let tools = retrieve[1]["result"]["tools"].as_array().unwrap();
    let tool_names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_names,
        [
            "context_for_task",
            "context_search",
            "context_get",
            "context_propose",
            "space_list"
        ]
    );
    assert!(!tool_names.contains(&"context_review"));
    assert!(!tool_names.contains(&"context_publish"));
    let pack = tool_data(&retrieve[2]);
    assert_eq!(pack["routing"]["source"], "workspace_binding");
    assert_eq!(pack["routing"]["resolved_space_id"], space_id);
    assert!(
        pack["items"].as_array().unwrap().iter().any(|item| {
            item["context_id"] == accepted_context_id && item["status"] == "accepted"
        })
    );

    let before_candidate = harness.event_count();
    let arguments = proposal(&harness.workspace);
    let first = harness.mcp(&[("context_propose", arguments.clone())]);
    let first = tool_data(&first[2]);
    assert_eq!(first["status"], "candidate");
    assert_eq!(first["deduplicated"], false, "{first:#}");
    assert_eq!(first["routing"]["source"], "workspace_binding");
    assert_eq!(harness.event_count(), before_candidate + 1);

    let duplicate = harness.mcp(&[("context_propose", arguments.clone())]);
    let duplicate = tool_data(&duplicate[2]);
    assert_eq!(duplicate["status"], "existing");
    assert_eq!(duplicate["deduplicated"], true);
    assert_eq!(
        duplicate["match_reason"],
        "exact_context_revision_draft_match"
    );
    assert_eq!(duplicate["context_id"], first["context_id"]);
    assert_eq!(duplicate["revision_id"], first["revision_id"]);
    assert_eq!(duplicate["event_id"], first["event_id"]);
    assert!(duplicate["batch_id"].is_null());
    assert!(duplicate["commit_oid"].is_null());
    assert_eq!(harness.event_count(), before_candidate + 1);

    let mut changed = arguments;
    changed["evidence"][0]["limitations"] =
        json!(["not a live Agent decision", "deterministic harness"]);
    let distinct = harness.mcp(&[("context_propose", changed)]);
    let distinct = tool_data(&distinct[2]);
    assert_eq!(distinct["status"], "candidate");
    assert_eq!(distinct["deduplicated"], false);
    assert_ne!(distinct["context_id"], first["context_id"]);
    assert_eq!(harness.event_count(), before_candidate + 2);

    let unbound = harness.temporary.path().join("unbound workspace");
    fs::create_dir_all(&unbound).unwrap();
    let rejected = harness.mcp(&[("context_propose", proposal(&unbound))]);
    assert_eq!(rejected[2]["result"]["isError"], true);
    assert_eq!(
        rejected[2]["result"]["structuredContent"]["error"]["code"],
        "workspace_unbound"
    );
    assert_eq!(harness.event_count(), before_candidate + 2);
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
