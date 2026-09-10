use std::{
    fs,
    io::Write as _,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::Arc,
};

use fs2::FileExt;
use sctx_agent_adapter::{AgentKind, shared_context_activation_marker_with_policy};
use sctx_domain::ExternalSessionLocator;
use sctx_git_store::GitStore;
use sctx_installer::{
    Agent, Architecture, Host, InstallContext, Installer, SetupOptions, SetupStage, SkillStatus,
};
use sctx_local_state::Policy;
use sctx_local_state::{
    AuthorizedSessionScope, AuthorizedSessionScopeRead, AuthorizedSessionScopeStore,
    UserConfigStore,
};
use sctx_task_runtime::TaskRuntime;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const ORACLE_BYTES: &[u8] =
    include_bytes!("../../../fixtures/m5/repository-scoped-context-v1.json");
const SKILL_GATE_BYTES: &[u8] = include_bytes!("../../../skills/shared-context/SKILL.md");
const WORKFLOW_BYTES: &[u8] =
    include_bytes!("../../../skills/shared-context/references/workflow.md");
const SKILL_METADATA_BYTES: &[u8] =
    include_bytes!("../../../skills/shared-context/agents/openai.yaml");
const REVIEW_GATE_BYTES: &[u8] = include_bytes!("../../../skills/sctx-review/SKILL.md");
const REVIEW_REFERENCE_BYTES: &[u8] =
    include_bytes!("../../../skills/sctx-review/references/review.md");
const REVIEW_METADATA_BYTES: &[u8] =
    include_bytes!("../../../skills/sctx-review/agents/openai.yaml");

#[derive(Debug, Deserialize)]
struct Oracle {
    schema: String,
    activation_marker_template: String,
    /// The team `## session` policy half of the template, stated separately so the fixture says
    /// which bytes are protocol and which are one installation's policy.
    session_policy: String,
    source_assets: SourceAssets,
    token_proxy: TokenProxy,
    enabled_chain: EnabledChain,
    disabled_residue: Residue,
}

#[derive(Debug, Deserialize)]
struct SourceAssets {
    #[serde(rename = "gate_bytes")]
    gate: usize,
    #[serde(rename = "workflow_bytes")]
    workflow: usize,
    #[serde(rename = "metadata_bytes")]
    metadata: usize,
    #[serde(rename = "review_gate_bytes")]
    review_gate: usize,
    #[serde(rename = "review_reference_bytes")]
    review_reference: usize,
    #[serde(rename = "review_metadata_bytes")]
    review_metadata: usize,
}

#[derive(Debug, Deserialize)]
struct TokenProxy {
    disabled: DisabledProxy,
    enabled: EnabledProxy,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct DisabledProxy {
    activation_bytes: usize,
    workflow_reads: usize,
    mcp_calls: usize,
    tool_result_bytes: usize,
    business_residue: usize,
}

#[derive(Debug, Deserialize)]
struct EnabledProxy {
    activation_fixed_bytes: usize,
    activation_max_bytes: usize,
    workflow_reads: usize,
    mcp_calls: usize,
    sanitized_tool_result_bytes_min: usize,
    sanitized_tool_result_bytes_max: usize,
}

#[derive(Debug, Deserialize)]
struct EnabledChain {
    mcp_tools: Vec<String>,
    pending_candidates: usize,
    confirmed_candidates: usize,
}

#[derive(Debug, Deserialize)]
struct Residue {
    runtime_files: usize,
    report_files: usize,
    knowledge_commits_delta: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BusinessSnapshot {
    files: Vec<(PathBuf, Vec<u8>)>,
    git_head: String,
    git_status: String,
}

#[derive(Clone, Copy)]
enum AgentProfile {
    Codex,
    Cursor,
}

impl AgentProfile {
    fn agent(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Cursor => "cursor",
        }
    }

    fn client(self) -> &'static str {
        self.agent()
    }

    fn activation(self, marker: &str) -> Value {
        match self {
            Self::Codex => json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": marker
            }}),
            Self::Cursor => json!({"additional_context": marker}),
        }
    }
}

struct Fixture {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    ancestor: PathBuf,
    /// The directory `repository_a` and `repository_b` share; a Session that starts here
    /// derives both without anything being registered for the directory itself.
    common_parent: PathBuf,
    repository_a: PathBuf,
    repository_b: PathBuf,
    sibling: PathBuf,
    /// A directory holding no registered checkout at all.
    outside: PathBuf,
    file_a: PathBuf,
    file_b: PathBuf,
    file_c: PathBuf,
    sibling_file: PathBuf,
    repository_a_id: sctx_domain::RepositoryId,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("acceptance home");
        let ancestor = temporary.path().join("workspace parent");
        let common_parent = ancestor.join("shared parent");
        let repository_a = common_parent.join("member a");
        let repository_b = common_parent.join("member b");
        let sibling = common_parent.join("unregistered sibling");
        let repository_c = ancestor.join("registered elsewhere");
        let outside = temporary.path().join("outside catalog");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        for repository in [&repository_a, &repository_b, &repository_c, &sibling] {
            initialize_repository(repository);
        }
        let ancestor = fs::canonicalize(ancestor).unwrap();
        let common_parent = fs::canonicalize(common_parent).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let repository_a = fs::canonicalize(repository_a).unwrap();
        let repository_b = fs::canonicalize(repository_b).unwrap();
        let repository_c = fs::canonicalize(repository_c).unwrap();
        let sibling = fs::canonicalize(sibling).unwrap();
        let file_a = repository_a.join("src/fixture.rs");
        let file_b = repository_b.join("src/fixture.rs");
        let file_c = repository_c.join("src/fixture.rs");
        let sibling_file = sibling.join("src/fixture.rs");
        let root = home.join(".shared-context");
        GitStore::bootstrap_local(&root).unwrap();
        let config = UserConfigStore::open_existing(&root).unwrap();
        let repository_a_id = config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&repository_a),
            )
            .unwrap()
            .repository
            .repository_id;
        let _second = config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&repository_b),
            )
            .unwrap()
            .repository
            .repository_id;
        config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&repository_c),
            )
            .unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            ancestor,
            common_parent,
            repository_a,
            repository_b,
            sibling,
            outside,
            file_a,
            file_b,
            file_c,
            sibling_file,
            repository_a_id,
        }
    }

    fn catalog(&self) -> sctx_local_state::RepositoryCatalogSnapshot {
        UserConfigStore::open_existing(&self.root)
            .unwrap()
            .repository_catalog_wait()
            .unwrap()
    }

    fn repository_id(&self, path: &Path) -> sctx_domain::RepositoryId {
        self.catalog()
            .resolve_declared_path(path)
            .unwrap()
            .repository_id
    }

    fn scope(&self, profile: AgentProfile, session: &str) -> AuthorizedSessionScopeRead {
        AuthorizedSessionScopeStore::initialize(&self.root)
            .unwrap()
            .read(&ExternalSessionLocator::new(profile.agent(), session).unwrap())
            .unwrap()
    }

    fn hook(&self, profile: AgentProfile, payload: &Value) -> Value {
        output_json(&run_hook(&self.home, profile, payload))
    }

    fn business_snapshot(&self) -> BusinessSnapshot {
        business_snapshot(&self.root)
    }
}

#[derive(Default)]
struct McpTrace {
    tool_names: Vec<String>,
    sanitized_result_bytes: usize,
}

struct McpResponse {
    is_error: bool,
    structured: Value,
}

fn oracle() -> Oracle {
    serde_json::from_slice(ORACLE_BYTES).unwrap()
}

impl Oracle {
    /// The exact marker the Hook must emit for one Agent profile and host Session id.
    fn activation_marker(&self, profile: AgentProfile, session: &str) -> String {
        self.activation_marker_template
            .replace("{host_session_id}", session)
            .replace("{agent_kind}", profile.agent())
    }
}

fn initialize_repository(path: &Path) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("src/fixture.rs"), "pub fn fixture() {}\n").unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

fn run_hook(home: &Path, profile: AgentProfile, payload: &Value) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    command
        .args(["hook", "--agent", profile.agent()])
        .env("HOME", home);
    if matches!(profile, AgentProfile::Codex) {
        command.args(["--agent-version", "0.147.0"]);
    }
    let mut child = command
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
    child.wait_with_output().unwrap()
}

fn output_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "Hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "Hook leaked a diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn session_start(profile: AgentProfile, session: &str, cwd: &Path, source: &str) -> Value {
    match profile {
        AgentProfile::Codex => json!({
            "session_id": session,
            "transcript_path": null,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "source": source
        }),
        AgentProfile::Cursor => json!({
            "conversation_id": session,
            "generation_id": "synthetic-generation",
            "model": "claude-opus-4-7-thinking-max",
            "model_id": "claude-opus-4-7",
            "model_params": [],
            "hook_event_name": "sessionStart",
            "cursor_version": "3.13.10",
            "workspace_roots": [cwd],
            "user_email": null,
            "transcript_path": null,
            "session_id": session,
            "is_background_agent": false,
            "composer_mode": "agent"
        }),
    }
}

fn prompt_submit(profile: AgentProfile, session: &str, cwd: &Path) -> Value {
    match profile {
        AgentProfile::Codex => json!({
            "session_id": session,
            "transcript_path": null,
            "cwd": cwd,
            "hook_event_name": "UserPromptSubmit",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": "synthetic-turn",
            "prompt": "SYNTHETIC_PRIVATE_PROMPT"
        }),
        AgentProfile::Cursor => json!({
            "conversation_id": session,
            "generation_id": "synthetic-generation",
            "model": "claude-opus-4-7-thinking-max",
            "model_id": "claude-opus-4-7",
            "model_params": [],
            "hook_event_name": "beforeSubmitPrompt",
            "cursor_version": "3.13.10",
            "workspace_roots": [cwd],
            "user_email": null,
            "transcript_path": null,
            "prompt": "SYNTHETIC_PRIVATE_PROMPT",
            "attachments": []
        }),
    }
}

fn post_tool(
    profile: AgentProfile,
    session: &str,
    cwd: &Path,
    file: &Path,
    tool: &str,
    raw_marker: &str,
) -> Value {
    match profile {
        AgentProfile::Codex => json!({
            "session_id": session,
            "transcript_path": null,
            "cwd": cwd,
            "hook_event_name": "PostToolUse",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": "synthetic-turn",
            "tool_name": tool,
            "tool_use_id": "synthetic-tool",
            "tool_input": {"file_path": file, "command": raw_marker},
            "tool_response": {"output": raw_marker}
        }),
        AgentProfile::Cursor => json!({
            "conversation_id": session,
            "generation_id": "synthetic-generation",
            "model": "claude-opus-4-7-thinking-max",
            "model_id": "claude-opus-4-7",
            "model_params": [],
            "hook_event_name": "postToolUse",
            "cursor_version": "3.13.10",
            "workspace_roots": [cwd],
            "user_email": null,
            "transcript_path": null,
            "tool_name": tool,
            "tool_input": {"file_path": file, "command": raw_marker},
            "tool_output": raw_marker,
            "tool_use_id": "synthetic-tool",
            "cwd": cwd,
            "duration": 1
        }),
    }
}

fn lifecycle_boundary(profile: AgentProfile, session: &str, cwd: &Path) -> Value {
    match profile {
        AgentProfile::Codex => json!({
            "session_id": session,
            "transcript_path": null,
            "cwd": cwd,
            "hook_event_name": "Stop",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": "synthetic-turn",
            "stop_hook_active": false,
            "last_assistant_message": null
        }),
        AgentProfile::Cursor => json!({
            "conversation_id": session,
            "generation_id": "synthetic-generation",
            "model": "claude-opus-4-7-thinking-max",
            "hook_event_name": "preCompact",
            "cursor_version": "3.13.10",
            "workspace_roots": [cwd],
            "user_email": null,
            "transcript_path": null,
            "trigger": "auto",
            "context_usage_percent": 85,
            "context_tokens": 120_000,
            "context_window_size": 128_000,
            "message_count": 45,
            "messages_to_compact": 30,
            "is_first_compaction": true
        }),
    }
}

fn session_end(profile: AgentProfile, session: &str, cwd: &Path) -> Value {
    match profile {
        AgentProfile::Codex => json!({
            "session_id": session,
            "transcript_path": null,
            "cwd": cwd,
            "hook_event_name": "SessionEnd",
            "model": "gpt-5.6-sol",
            "reason": "other"
        }),
        AgentProfile::Cursor => json!({
            "conversation_id": session,
            "generation_id": "synthetic-generation",
            "model": "claude-opus-4-7-thinking-max",
            "hook_event_name": "sessionEnd",
            "cursor_version": "3.13.10",
            "workspace_roots": [cwd],
            "user_email": null,
            "transcript_path": null,
            "session_id": session,
            "reason": "completed",
            "duration_ms": 1,
            "is_background_agent": false,
            "final_status": "completed"
        }),
    }
}

fn mcp_call(
    home: &Path,
    profile: AgentProfile,
    session: &str,
    tool: &str,
    arguments: &Value,
) -> McpResponse {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", profile.client()])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "repository-scope-acceptance", "version": "1"}
        }
    });
    let mut arguments = arguments.clone();
    let object = arguments.as_object_mut().unwrap();
    object.insert("agent_kind".to_owned(), json!(profile.agent()));
    object.insert("external_session_id".to_owned(), json!(session));
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}
    });
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{initialize}").unwrap();
    writeln!(stdin, "{call}").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "MCP {tool} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        responses.len(),
        2,
        "unexpected MCP response: {responses:#?}"
    );
    assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");
    McpResponse {
        is_error: responses[1]["result"]["isError"].as_bool().unwrap(),
        structured: responses[1]["result"]["structuredContent"].clone(),
    }
}

fn traced_call(
    trace: &mut McpTrace,
    fixture: &Fixture,
    profile: AgentProfile,
    session: &str,
    tool: &str,
    arguments: &Value,
) -> Value {
    let response = mcp_call(&fixture.home, profile, session, tool, arguments);
    assert!(!response.is_error, "MCP {tool}: {:#}", response.structured);
    let mut sanitized = response.structured.clone();
    sanitize_dynamic_data(&mut sanitized, &fixture.ancestor);
    let encoded = serde_json::to_vec(&sanitized).unwrap();
    assert!(!String::from_utf8_lossy(&encoded).contains(fixture.ancestor.to_str().unwrap()));
    trace.tool_names.push(tool.to_owned());
    trace.sanitized_result_bytes += encoded.len();
    response.structured
}

fn sanitize_dynamic_data(value: &mut Value, private_root: &Path) {
    match value {
        Value::String(text) => {
            let dynamic_id = [
                "tsk_", "tss_", "tir_", "sig_", "wke_", "ckp_", "clm_", "cnd_", "ref_", "spc_",
                "ctx_", "rev_", "evd_", "rgr_", "rep_", "obs_", "cap_", "sub_",
            ]
            .iter()
            .any(|prefix| text.starts_with(prefix));
            let digest = matches!(text.len(), 40 | 64)
                && text.chars().all(|character| character.is_ascii_hexdigit());
            let timestamp = text.contains('T') && text.ends_with('Z');
            if dynamic_id {
                "<id>".clone_into(text);
            } else if digest {
                "<digest>".clone_into(text);
            } else if timestamp {
                "<time>".clone_into(text);
            } else if text.contains(private_root.to_str().unwrap()) {
                "<path>".clone_into(text);
            }
        }
        Value::Array(values) => {
            for value in values {
                sanitize_dynamic_data(value, private_root);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                sanitize_dynamic_data(value, private_root);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn activate_skill(marker: Option<&str>, expected: &str) -> (usize, usize) {
    match marker {
        Some(marker) if marker == expected => {
            let workflow = fs::read("../../skills/shared-context/references/workflow.md")
                .or_else(|_| fs::read("skills/shared-context/references/workflow.md"))
                .unwrap();
            assert_eq!(workflow, WORKFLOW_BYTES);
            (1, workflow.len())
        }
        _ => (0, 0),
    }
}

fn business_snapshot(root: &Path) -> BusinessSnapshot {
    let state = root.join("state");
    let mut files = Vec::new();
    collect_business_files(&state, &state, &mut files);
    for directory in [root.join("report"), root.join("reports")] {
        collect_all_files(&directory, root, &mut files);
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let repository = root.join("repository");
    BusinessSnapshot {
        files,
        git_head: git(&repository, &["rev-parse", "HEAD"]),
        git_status: git(&repository, &["status", "--short"]),
    }
}

fn collect_business_files(
    directory: &Path,
    relative_to: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let business =
            name.starts_with("runtime.sqlite") || matches!(name, "capture" | "report" | "reports");
        if path.is_dir() {
            if business {
                collect_all_files(&path, relative_to, files);
            }
        } else if business {
            files.push((
                path.strip_prefix(relative_to).unwrap().to_path_buf(),
                fs::read(path).unwrap(),
            ));
        }
    }
}

fn collect_all_files(directory: &Path, relative_to: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_all_files(&path, relative_to, files);
        } else {
            files.push((
                path.strip_prefix(relative_to).unwrap().to_path_buf(),
                fs::read(path).unwrap(),
            ));
        }
    }
}

fn runtime_file_count(root: &Path) -> usize {
    fs::read_dir(root.join("state"))
        .ok()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.starts_with("runtime.sqlite"))
        })
        .count()
}

fn assert_no_capture_state(root: &Path) {
    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!root.join("state").join(removed).exists());
    }
}

fn report_file_count(root: &Path) -> usize {
    let mut files = Vec::new();
    for directory in [
        root.join("state/report"),
        root.join("state/reports"),
        root.join("report"),
        root.join("reports"),
    ] {
        collect_all_files(&directory, root, &mut files);
    }
    files.len()
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

#[allow(clippy::too_many_lines)]
fn run_enabled_chain(
    fixture: &Fixture,
    profile: AgentProfile,
    session: &str,
    startup: &Path,
    target_file: &Path,
    target_repository_id: &sctx_domain::RepositoryId,
    oracle: &Oracle,
) -> McpTrace {
    assert_eq!(
        fixture.hook(
            profile,
            &session_start(profile, session, startup, "startup")
        ),
        profile.activation(&oracle.activation_marker(profile, session))
    );
    assert_eq!(
        fixture.hook(profile, &prompt_submit(profile, session, startup)),
        json!({})
    );
    let marker = oracle.activation_marker(profile, session);
    let (workflow_reads, workflow_bytes) = activate_skill(Some(&marker), &marker);
    assert_eq!(workflow_reads, oracle.token_proxy.enabled.workflow_reads);
    assert_eq!(workflow_bytes, oracle.source_assets.workflow);

    let mut trace = McpTrace::default();
    let task = traced_call(
        &mut trace,
        fixture,
        profile,
        session,
        "task_intent_update",
        &json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": format!("Verify {} repository-scoped chain", profile.agent())}
        }),
    );
    let task_id = task["task_id"].as_str().unwrap();
    let intent_revision_id = task["intent_revision_id"].as_str().unwrap();
    let focus = traced_call(
        &mut trace,
        fixture,
        profile,
        session,
        "task_artifact_focus",
        &json!({
            "expected_revision_id": intent_revision_id,
            "absolute_file_path": target_file,
            "locator": {"locator_kind": "file"},
            "token_budget": 2000,
            "max_spaces": 4
        }),
    );
    assert_eq!(
        focus["resolved_focus"]["repository_id"],
        target_repository_id.to_string()
    );
    let tool = "test";
    let raw_marker = format!("RAW_{}_TOOL_OUTPUT", profile.agent().to_uppercase());
    assert_eq!(
        fixture.hook(
            profile,
            &post_tool(profile, session, startup, target_file, tool, &raw_marker)
        ),
        json!({})
    );
    let locator = ExternalSessionLocator::new(profile.agent(), session).unwrap();
    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    let active = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    let signal = runtime
        .read_signal_history(active.task_session_id)
        .unwrap()
        .into_iter()
        .find(|signal| signal.signal.content.contains(tool))
        .unwrap();
    assert_no_capture_state(&fixture.root);
    let persisted = serde_json::to_string(&signal).unwrap();
    assert!(!persisted.contains(&raw_marker));

    let checkpoint = traced_call(
        &mut trace,
        fixture,
        profile,
        session,
        "task_checkpoint",
        &json!({
            "claims": [{
                "context_kind": "validation",
                "statement": format!("The {} registered cross-Repository check passed", profile.agent()),
                "rationale": "The owned normalized test outcome and exact resolved Focus agree",
                "conditions": [],
                "evidence": [{
                    "evidence_type": "experiment_record",
                    "summary": "the registered cross-Repository check passed",
                    "limitations": ["local acceptance fixture"]
                }]
            }],
            "unknowns": []
        }),
    );
    assert_eq!(checkpoint["episode_version"], 1);
    assert_eq!(checkpoint["candidate_build"]["status"], "pending");
    let episode = runtime
        .list_work_episodes(active.task_session_id, 4)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&episode.checkpoints[0].claims[0]).unwrap()["artifact_refs"],
        json!([]),
    );
    let boundary = fixture.hook(profile, &lifecycle_boundary(profile, session, startup));
    assert!(
        boundary.get("systemMessage").is_some()
            || boundary.get("user_message").is_some()
            || boundary == json!({})
    );

    let list = traced_call(
        &mut trace,
        fixture,
        profile,
        session,
        "candidate_list",
        &json!({"status": "pending", "limit": 10, "token_budget": 32768}),
    );
    assert_eq!(
        list["reviews"].as_array().unwrap().len(),
        oracle.enabled_chain.pending_candidates
    );
    let candidate_id = list["reviews"][0]["candidate_id"].as_str().unwrap();
    let review = traced_call(
        &mut trace,
        fixture,
        profile,
        session,
        "candidate_get",
        &json!({"candidate_id": candidate_id}),
    );
    assert_eq!(review["ready_for_review"], true);
    assert_eq!(review["untrusted_data"], true);
    assert_eq!(review["source_episode"]["task_id"], task_id);
    assert_eq!(
        oracle.enabled_chain.confirmed_candidates, 0,
        "the acceptance fixture must never confirm its Candidate"
    );
    trace
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_oracle_closes_single_and_multi_repository_disabled_and_token_proxy_contract() {
    let oracle = oracle();
    assert_eq!(
        oracle.schema,
        "sctx.repository-scoped-context.acceptance.v1"
    );
    assert_eq!(SKILL_GATE_BYTES.len(), oracle.source_assets.gate);
    assert_eq!(WORKFLOW_BYTES.len(), oracle.source_assets.workflow);
    assert_eq!(SKILL_METADATA_BYTES.len(), oracle.source_assets.metadata);
    assert_eq!(
        oracle.session_policy,
        Policy::compiled_default().session(),
        "the oracle's policy half must be the shipped default policy's session section"
    );
    assert_eq!(
        oracle
            .activation_marker_template
            .replace("{host_session_id}", "")
            .replace("{agent_kind}", "")
            .len(),
        oracle.token_proxy.enabled.activation_fixed_bytes,
        "only the host Session id and Agent kind may vary between markers"
    );
    for (profile, agent, session) in [
        (
            AgentProfile::Codex,
            AgentKind::Codex,
            "synthetic-proxy-codex",
        ),
        (
            AgentProfile::Cursor,
            AgentKind::Cursor,
            "synthetic-proxy-cursor",
        ),
    ] {
        let marker = oracle.activation_marker(profile, session);
        assert_eq!(
            marker,
            shared_context_activation_marker(agent, session),
            "the oracle marker must match the shipped renderer"
        );
        assert!(marker.len() <= oracle.token_proxy.enabled.activation_max_bytes);
        assert_eq!(
            marker.len(),
            oracle.token_proxy.enabled.activation_fixed_bytes
                + profile.agent().len()
                + 2 * session.len()
        );
    }
    let fixed_oracle = String::from_utf8(ORACLE_BYTES.to_vec()).unwrap();
    for forbidden in [
        "/Users/",
        "/home/",
        "bytedance",
        "transcript_path",
        "synthetic-",
        "user_email",
        "SYNTHETIC_PRIVATE_PROMPT",
    ] {
        assert!(
            !fixed_oracle.contains(forbidden),
            "oracle leaked {forbidden:?}"
        );
    }

    let fixture = Fixture::new();
    let direct = run_enabled_chain(
        &fixture,
        AgentProfile::Codex,
        "synthetic-direct-flow",
        &fixture.repository_a,
        &fixture.file_b,
        &fixture.repository_id(&fixture.file_b),
        &oracle,
    );
    let parent = run_enabled_chain(
        &fixture,
        AgentProfile::Cursor,
        "synthetic-parent-flow",
        &fixture.common_parent,
        &fixture.file_c,
        &fixture.repository_id(&fixture.file_c),
        &oracle,
    );
    for trace in [&direct, &parent] {
        assert_eq!(trace.tool_names, oracle.enabled_chain.mcp_tools);
        assert_eq!(trace.tool_names.len(), oracle.token_proxy.enabled.mcp_calls);
        assert!(
            trace.sanitized_result_bytes
                >= oracle.token_proxy.enabled.sanitized_tool_result_bytes_min
        );
        assert!(
            trace.sanitized_result_bytes
                <= oracle.token_proxy.enabled.sanitized_tool_result_bytes_max
        );
    }

    for (profile, session, cwd) in [
        (
            AgentProfile::Codex,
            "synthetic-disabled-sibling",
            fixture.sibling.as_path(),
        ),
        (
            AgentProfile::Cursor,
            "synthetic-disabled-outside",
            fixture.outside.as_path(),
        ),
    ] {
        let before = fixture.business_snapshot();
        assert_eq!(
            fixture.hook(profile, &session_start(profile, session, cwd, "startup")),
            json!({})
        );
        assert_eq!(
            fixture.hook(profile, &prompt_submit(profile, session, cwd)),
            json!({})
        );
        assert_eq!(
            fixture.hook(
                profile,
                &post_tool(
                    profile,
                    session,
                    cwd,
                    &fixture.sibling_file,
                    "DisabledRepositoryTest",
                    "RAW_DISABLED"
                )
            ),
            json!({})
        );
        assert_eq!(
            fixture.hook(profile, &lifecycle_boundary(profile, session, cwd)),
            json!({})
        );
        assert_eq!(
            fixture.hook(profile, &session_end(profile, session, cwd)),
            json!({})
        );
        assert_eq!(fixture.business_snapshot(), before);
        assert_eq!(
            activate_skill(None, &oracle.activation_marker(profile, session)),
            (0, 0)
        );
    }
    assert_eq!(
        oracle.token_proxy.disabled,
        DisabledProxy {
            activation_bytes: 0,
            workflow_reads: 0,
            mcp_calls: 0,
            tool_result_bytes: 0,
            business_residue: 0,
        }
    );
}

#[test]
fn first_locator_decision_is_sticky_and_disabled_residue_is_exactly_zero() {
    let fixture = Fixture::new();
    let oracle = oracle();
    let enabled = "synthetic-sticky-enabled";
    assert_eq!(
        fixture.hook(
            AgentProfile::Codex,
            &session_start(
                AgentProfile::Codex,
                enabled,
                &fixture.repository_a,
                "startup"
            )
        ),
        AgentProfile::Codex.activation(&oracle.activation_marker(AgentProfile::Codex, enabled))
    );
    let AuthorizedSessionScopeRead::Current(first) = fixture.scope(AgentProfile::Codex, enabled)
    else {
        panic!("enabled scope was not Current")
    };
    for (source, cwd) in [
        ("resume", fixture.repository_b.as_path()),
        ("compact", fixture.ancestor.as_path()),
    ] {
        assert_eq!(
            fixture.hook(
                AgentProfile::Codex,
                &session_start(AgentProfile::Codex, enabled, cwd, source)
            ),
            AgentProfile::Codex.activation(&oracle.activation_marker(AgentProfile::Codex, enabled))
        );
        assert!(matches!(
            fixture.scope(AgentProfile::Codex, enabled),
            AuthorizedSessionScopeRead::Current(ref scope) if scope == &first
        ));
    }

    let disabled = "synthetic-sticky-disabled";
    let before = fixture.business_snapshot();
    assert_eq!(
        fixture.hook(
            AgentProfile::Codex,
            &session_start(AgentProfile::Codex, disabled, &fixture.sibling, "startup")
        ),
        json!({})
    );
    assert_eq!(
        fixture.hook(
            AgentProfile::Codex,
            &session_start(
                AgentProfile::Codex,
                disabled,
                &fixture.repository_a,
                "resume"
            )
        ),
        json!({})
    );
    assert!(matches!(
        fixture.scope(AgentProfile::Codex, disabled),
        AuthorizedSessionScopeRead::Current(AuthorizedSessionScope {
            decision: sctx_local_state::AuthorizedSessionScopeDecision::Disabled,
            ..
        })
    ));
    assert_eq!(fixture.business_snapshot(), before);
    assert_eq!(
        runtime_file_count(&fixture.root),
        oracle.disabled_residue.runtime_files
    );
    assert_no_capture_state(&fixture.root);
    assert_eq!(
        report_file_count(&fixture.root),
        oracle.disabled_residue.report_files
    );
    assert_eq!(oracle.disabled_residue.knowledge_commits_delta, 0);
}

/// Every refusal stays in the one `session_not_authorized` family, and its `code` names
/// the single cause the caller can act on: re-copy the marker id, register the directory,
/// or retry a transient local failure. None of them may name a path or Repository.
fn assert_denied(response: &McpResponse, fixture: &Fixture, code: &str) {
    assert!(response.is_error);
    assert_eq!(
        response.structured["error"]["kind"],
        "session_not_authorized"
    );
    assert_eq!(response.structured["error"]["code"], code);
    let text = response.structured.to_string();
    assert!(!text.contains(fixture.ancestor.to_str().unwrap()));
    assert!(!text.contains(&fixture.repository_a_id.to_string()));
}

#[test]
#[allow(clippy::too_many_lines)]
fn server_negative_matrix_is_uniform_and_writes_no_business_state() {
    let disabled = Fixture::new();
    disabled.hook(
        AgentProfile::Codex,
        &session_start(
            AgentProfile::Codex,
            "negative-disabled",
            &disabled.sibling,
            "startup",
        ),
    );
    let before = disabled.business_snapshot();
    let response = mcp_call(
        &disabled.home,
        AgentProfile::Codex,
        "negative-disabled",
        "context_search",
        &json!({"query": "bounded"}),
    );
    assert_denied(&response, &disabled, "activation_disabled");
    assert_eq!(disabled.business_snapshot(), before);

    // A lease never expires and an unrelated registration no longer demotes a live
    // Session; losing the registration under the Session's own startup directory does.
    let deregistered = Fixture::new();
    deregistered.hook(
        AgentProfile::Cursor,
        &session_start(
            AgentProfile::Cursor,
            "negative-deregistered",
            &deregistered.common_parent,
            "startup",
        ),
    );
    let unrelated = deregistered.ancestor.join("registered after lease");
    initialize_repository(&unrelated);
    let unrelated = fs::canonicalize(unrelated).unwrap();
    let config = UserConfigStore::open_existing(&deregistered.root).unwrap();
    config
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&unrelated),
        )
        .unwrap();
    let still_authorized = mcp_call(
        &deregistered.home,
        AgentProfile::Cursor,
        "negative-deregistered",
        "context_search",
        &json!({"query": "bounded"}),
    );
    assert!(
        !still_authorized.is_error,
        "an unrelated registration must not demote a live Session"
    );
    // Losing every registration under the Session's own startup directory does demote it,
    // even though the Catalog still configures Repositories elsewhere.
    fs::write(
        deregistered.root.join("config.toml"),
        UserConfigStore::empty_document(&deregistered.root).unwrap(),
    )
    .unwrap();
    for elsewhere in [
        deregistered.ancestor.join("registered elsewhere"),
        unrelated,
    ] {
        config
            .add_repository(sctx_domain::RepositoryId::new(), &[elsewhere])
            .unwrap();
    }
    let before = deregistered.business_snapshot();
    let response = mcp_call(
        &deregistered.home,
        AgentProfile::Cursor,
        "negative-deregistered",
        "context_search",
        &json!({"query": "bounded"}),
    );
    assert_denied(&response, &deregistered, "activation_disabled");
    assert_eq!(deregistered.business_snapshot(), before);

    let cross = Fixture::new();
    cross.hook(
        AgentProfile::Codex,
        &session_start(
            AgentProfile::Codex,
            "negative-owner",
            &cross.repository_a,
            "startup",
        ),
    );
    let before = cross.business_snapshot();
    for (profile, session) in [
        (AgentProfile::Codex, "negative-other-session"),
        (AgentProfile::Cursor, "negative-owner"),
    ] {
        let response = mcp_call(
            &cross.home,
            profile,
            session,
            "context_search",
            &json!({"query": "bounded"}),
        );
        assert_denied(&response, &cross, "lease_missing");
    }
    assert_eq!(cross.business_snapshot(), before);

    let corrupt = Fixture::new();
    corrupt.hook(
        AgentProfile::Codex,
        &session_start(
            AgentProfile::Codex,
            "negative-corrupt",
            &corrupt.repository_a,
            "startup",
        ),
    );
    let scope_store = AuthorizedSessionScopeStore::initialize(&corrupt.root).unwrap();
    let record = fs::read_dir(scope_store.directory())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .unwrap();
    fs::write(record, b"{}").unwrap();
    let before = corrupt.business_snapshot();
    let response = mcp_call(
        &corrupt.home,
        AgentProfile::Codex,
        "negative-corrupt",
        "context_search",
        &json!({"query": "bounded"}),
    );
    assert_denied(&response, &corrupt, "lease_missing");
    assert_eq!(corrupt.business_snapshot(), before);

    let busy = Fixture::new();
    busy.hook(
        AgentProfile::Codex,
        &session_start(
            AgentProfile::Codex,
            "negative-busy",
            &busy.repository_a,
            "startup",
        ),
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(busy.root.join("state/authorized-session-scopes.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let before = busy.business_snapshot();
    let response = mcp_call(
        &busy.home,
        AgentProfile::Codex,
        "negative-busy",
        "context_search",
        &json!({"query": "bounded"}),
    );
    assert_denied(&response, &busy, "authorization_internal");
    assert_eq!(busy.business_snapshot(), before);
    FileExt::unlock(&lock).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn safe_unregistered_claim_requires_owned_non_locating_observation_and_unsafe_paths_drop() {
    let fixture = Fixture::new();
    let oracle = oracle();
    let session = "synthetic-non-locating";
    assert_eq!(
        fixture.hook(
            AgentProfile::Codex,
            &session_start(
                AgentProfile::Codex,
                session,
                &fixture.repository_a,
                "startup"
            )
        ),
        AgentProfile::Codex.activation(&oracle.activation_marker(AgentProfile::Codex, session))
    );
    let task = mcp_call(
        &fixture.home,
        AgentProfile::Codex,
        session,
        "task_intent_update",
        &json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "Verify bounded non-locating investigation"}
        }),
    );
    assert!(!task.is_error, "{:#}", task.structured);
    assert_eq!(
        fixture.hook(
            AgentProfile::Codex,
            &post_tool(
                AgentProfile::Codex,
                session,
                &fixture.sibling,
                &fixture.sibling_file,
                "UnregisteredInvestigationTest",
                "RAW_UNREGISTERED_PRIVATE"
            )
        ),
        json!({})
    );
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    let active = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    assert_no_capture_state(&fixture.root);
    let checkpoint = mcp_call(
        &fixture.home,
        AgentProfile::Codex,
        session,
        "task_checkpoint",
        &json!({
            "claims": [{
                "context_kind": "validation",
                "statement": "The unregistered investigation retained only bounded non-locating meaning",
                "rationale": "The Agent retained only the focused engineering conclusion",
                "conditions": ["the investigated Repository remains unregistered"],
                "evidence": [{
                    "evidence_type": "experiment_record",
                    "summary": "the bounded non-locating investigation completed",
                    "limitations": ["the Repository identity remains unregistered"]
                }]
            }],
            "unknowns": []
        }),
    );
    assert!(!checkpoint.is_error, "{:#}", checkpoint.structured);
    assert_eq!(
        checkpoint.structured["candidate_build"]["status"],
        "pending"
    );
    let candidates = mcp_call(
        &fixture.home,
        AgentProfile::Codex,
        session,
        "candidate_list",
        &json!({"status": "pending", "limit": 10, "token_budget": 32768}),
    );
    let candidate_id = candidates.structured["reviews"][0]["candidate_id"]
        .as_str()
        .unwrap();
    let review = mcp_call(
        &fixture.home,
        AgentProfile::Codex,
        session,
        "candidate_get",
        &json!({"candidate_id": candidate_id}),
    );
    assert!(!review.is_error, "{:#}", review.structured);
    assert_eq!(
        review.structured["content"]["evidence"][0]["limitations"][0],
        "the Repository identity remains unregistered"
    );
    let review_text = review.structured.to_string();
    for forbidden in [
        fixture.sibling.to_str().unwrap(),
        fixture.sibling_file.to_str().unwrap(),
        &fixture.repository_id(&fixture.file_a).to_string(),
        "RAW_UNREGISTERED_PRIVATE",
    ] {
        assert!(
            !review_text.contains(forbidden),
            "review leaked {forbidden:?}"
        );
    }
    assert!(review.structured["content"].get("artifact_refs").is_none());

    let signals_before = runtime
        .read_signal_history(active.task_session_id)
        .unwrap()
        .len();
    let symlink_file = fixture.repository_a.join("src/unsafe-link.rs");
    symlink(&fixture.sibling_file, &symlink_file).unwrap();
    for file in [
        PathBuf::from("src/relative.rs"),
        fixture.repository_a.join("src/missing.rs"),
        symlink_file,
    ] {
        assert_eq!(
            fixture.hook(
                AgentProfile::Codex,
                &post_tool(
                    AgentProfile::Codex,
                    session,
                    &fixture.repository_a,
                    &file,
                    "UnsafePathTest",
                    "RAW_UNSAFE"
                )
            ),
            json!({})
        );
    }
    assert_no_capture_state(&fixture.root);
    assert_eq!(
        runtime
            .read_signal_history(active.task_session_id)
            .unwrap()
            .len(),
        signals_before
    );
}

#[derive(Clone)]
struct AcceptanceHost;

impl Host for AcceptanceHost {
    fn platform(&self) -> &'static str {
        "macos"
    }

    fn architecture(&self) -> Option<Architecture> {
        Some(Architecture::Arm64)
    }

    fn git_version(&self) -> sctx_installer::Result<String> {
        Ok("git version acceptance".to_owned())
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

struct InstallerFixture {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    runtime: PathBuf,
    business_repository: PathBuf,
}

impl InstallerFixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("installer home");
        let root = temporary.path().join("installer root");
        let runtime = temporary.path().join("runtime source/sctx");
        let business_repository = temporary.path().join("business repository");
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(&runtime, b"signed-runtime-v1").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        initialize_repository(&business_repository);
        Self {
            _temporary: temporary,
            home,
            root,
            runtime,
            business_repository,
        }
    }

    fn installer(&self, version: &str) -> Installer {
        Installer::new(
            InstallContext::injected(&self.home, &self.root, &self.runtime, version),
            Arc::new(AcceptanceHost),
        )
    }

    fn skill_root(&self) -> PathBuf {
        self.home.join(".agents/skills/shared-context")
    }

    fn review_skill_root(&self) -> PathBuf {
        self.home.join(".agents/skills/sctx-review")
    }
}

fn directory_bytes(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    collect_all_files(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

#[test]
fn installer_assets_are_exact_atomic_recoverable_and_never_touch_business_repositories() {
    let fixture = InstallerFixture::new();
    let oracle = oracle();
    let business_before = directory_bytes(&fixture.business_repository);
    let installer = fixture.installer("1.0.0");
    let setup = installer.setup(&SetupOptions::default()).unwrap();
    assert_eq!(setup.skill.status, SkillStatus::Installed);
    let skill = fixture.skill_root();
    assert_eq!(fs::read(skill.join("SKILL.md")).unwrap(), SKILL_GATE_BYTES);
    assert_eq!(
        fs::read(skill.join("references/workflow.md")).unwrap(),
        WORKFLOW_BYTES
    );
    assert_eq!(
        fs::read(skill.join("agents/openai.yaml")).unwrap(),
        SKILL_METADATA_BYTES
    );
    let review = fixture.review_skill_root();
    assert_eq!(
        fs::read(review.join("SKILL.md")).unwrap(),
        REVIEW_GATE_BYTES
    );
    assert_eq!(
        fs::read(review.join("references/review.md")).unwrap(),
        REVIEW_REFERENCE_BYTES
    );
    assert_eq!(
        fs::read(review.join("agents/openai.yaml")).unwrap(),
        REVIEW_METADATA_BYTES
    );
    assert_eq!(SKILL_GATE_BYTES.len(), oracle.source_assets.gate);
    assert_eq!(WORKFLOW_BYTES.len(), oracle.source_assets.workflow);
    assert_eq!(SKILL_METADATA_BYTES.len(), oracle.source_assets.metadata);
    assert_eq!(
        oracle.session_policy,
        Policy::compiled_default().session(),
        "the oracle's policy half must be the shipped default policy's session section"
    );
    assert_eq!(REVIEW_GATE_BYTES.len(), oracle.source_assets.review_gate);
    assert_eq!(
        REVIEW_REFERENCE_BYTES.len(),
        oracle.source_assets.review_reference
    );
    assert_eq!(
        REVIEW_METADATA_BYTES.len(),
        oracle.source_assets.review_metadata
    );
    // The split only pays for itself if the session-hot reference actually shrank: the review
    // procedures moved into a second bundle instead of being duplicated into both.
    assert!(
        WORKFLOW_BYTES.len() < 21_000,
        "the core workflow reference must stay slimmer than the pre-split bundle"
    );

    fs::write(&fixture.runtime, b"signed-runtime-v2").unwrap();
    let failed = fixture
        .installer("2.0.0")
        .with_failure_after(SetupStage::GlobalSkillWorkflowWritten)
        .upgrade(&SetupOptions::default());
    assert!(failed.is_err());
    assert_eq!(fs::read(skill.join("SKILL.md")).unwrap(), SKILL_GATE_BYTES);
    assert_eq!(
        fs::read(skill.join("references/workflow.md")).unwrap(),
        WORKFLOW_BYTES
    );
    assert_eq!(
        fs::read(skill.join("agents/openai.yaml")).unwrap(),
        SKILL_METADATA_BYTES
    );
    assert_eq!(
        fs::read(review.join("references/review.md")).unwrap(),
        REVIEW_REFERENCE_BYTES
    );
    assert_eq!(
        fs::read_link(fixture.root.join("bin/current")).unwrap(),
        PathBuf::from("1.0.0/arm64")
    );
    let uninstall = installer.uninstall().unwrap();
    assert!(uninstall.repository_retained);
    assert!(!skill.join("SKILL.md").exists());
    assert!(!skill.join("references/workflow.md").exists());
    assert!(!skill.join("agents/openai.yaml").exists());
    assert!(!review.exists());
    assert_eq!(
        directory_bytes(&fixture.business_repository),
        business_before
    );

    let conflict = InstallerFixture::new();
    let conflict_skill = conflict.skill_root();
    fs::create_dir_all(&conflict_skill).unwrap();
    fs::write(conflict_skill.join("SKILL.md"), b"user-owned skill\n").unwrap();
    let report = conflict
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    assert_eq!(report.skill.status, SkillStatus::Conflict);
    assert_eq!(
        fs::read(conflict_skill.join("SKILL.md")).unwrap(),
        b"user-owned skill\n"
    );
    assert!(!conflict_skill.join("references/workflow.md").exists());
    // One conflicting bundle preserves the whole set: an installation never carries half the
    // managed Skills.
    assert!(!conflict.review_skill_root().exists());
    conflict.installer("1.0.0").uninstall().unwrap();
    assert!(conflict_skill.join("SKILL.md").exists());
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
