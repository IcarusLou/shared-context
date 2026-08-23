use std::{
    fs::{self, OpenOptions},
    io::{BufRead as _, BufReader, Write as _},
    path::Path,
    process::{Child, Command, Stdio},
};

use fs2::FileExt;
use sctx_domain::{ExternalSessionLocator, IntentSnapshot, TaskId, WorkingIntentSnapshot};
use sctx_event_schema::Event;
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::ProjectionIndex;
use sctx_local_state::UserConfigStore;
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const DIAGNOSTIC: &str = "Shared Context task retrieval is temporarily unavailable. Coding can continue; retry through MCP or CLI later.";
const PROMPT_GUIDANCE: &str = "Shared Context PromptEnvelope received. No Working Intent was inferred from prompt text. Use $shared-context and task_intent_update to record naturally formed understanding before precise retrieval.";

struct SqliteLock {
    child: Child,
}

impl SqliteLock {
    fn acquire(database: &Path) -> Self {
        let script = concat!(
            "import sqlite3,sys\n",
            "connection=sqlite3.connect(sys.argv[1], timeout=1)\n",
            "connection.execute('BEGIN EXCLUSIVE')\n",
            "print('locked', flush=True)\n",
            "sys.stdin.readline()\n",
            "connection.rollback()\n",
        );
        let mut child = Command::new("python3")
            .args(["-c", script])
            .arg(database)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "locked");
        Self { child }
    }
}

impl Drop for SqliteLock {
    fn drop(&mut self) {
        if let Some(mut stdin) = self.child.stdin.take() {
            let _ = stdin.write_all(b"\n");
        }
        let _ = self.child.wait();
    }
}

struct Harness {
    _temporary: TempDir,
    home: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("hook failure home");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self.home.join(".shared-context")
    }

    fn hook(&self, agent: &str, payload: &Value) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
        command
            .args(["hook", "--agent", agent])
            .env("HOME", &self.home);
        if agent == "codex" {
            command.args(["--agent-version", "0.147.0"]);
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        serde_json::to_writer(child.stdin.as_mut().unwrap(), payload).unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    fn explicit_task_context(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args([
                "--json",
                "task",
                "context",
                "--agent-kind",
                "codex",
                "--external-session-id",
                "explicit-session",
            ])
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }
}

fn codex_prompt(cwd: &Path, session_id: &str, prompt: &str) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": format!("/tmp/{prompt}.jsonl"),
        "cwd": cwd,
        "hook_event_name": "UserPromptSubmit",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": format!("turn-{session_id}"),
        "prompt": prompt
    })
}

fn cursor_post_tool(cwd: &Path, file: &Path, raw_marker: &str) -> Value {
    json!({
        "conversation_id": "cursor-fail-open",
        "generation_id": "generation-fail-open",
        "model": "claude-opus-4-7",
        "hook_event_name": "postToolUse",
        "cursor_version": "3.13.10",
        "workspace_roots": [cwd],
        "user_email": null,
        "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
        "tool_name": "ContractTest",
        "tool_input": {"file_path": file, "command": raw_marker},
        "tool_output": raw_marker,
        "tool_use_id": "cursor-tool-fail-open",
        "cwd": cwd,
        "duration": 1
    })
}

fn assert_fail_open(
    output: &std::process::Output,
    field: &str,
    expected: &str,
    root: &Path,
    secret: &str,
) {
    assert!(
        output.status.success(),
        "Hook must fail open: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.as_object().unwrap().len(), 1);
    assert_eq!(response[field], expected);
    let observable = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let root_text = root.to_string_lossy().into_owned();
    for forbidden in [
        secret,
        root_text.as_str(),
        "runtime.sqlite",
        "index.sqlite",
        "database is locked",
        "file is not a database",
        "unable to open database file",
        "untrusted-data",
        "<shared-context",
    ] {
        assert!(
            !observable.contains(forbidden),
            "fail-open output leaked {forbidden:?}: {observable}"
        );
    }
}

fn initialize_store(harness: &Harness) -> GitStore {
    GitStore::initialize(harness.root()).unwrap()
}

fn task_intent() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: "Refresh the registered engineering repository".to_owned(),
        current_direction: Some("Observe verified local Git repository state".to_owned()),
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: Vec::new(),
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

#[test]
fn codex_hook_fails_open_when_runtime_database_path_cannot_open() {
    let harness = Harness::new();
    initialize_store(&harness);
    fs::create_dir(harness.root().join("state/runtime.sqlite")).unwrap();
    let secret = "PROMPT_SECRET_RUNTIME_OPEN";

    let output = harness.hook("codex", &codex_prompt(&harness.home, "open-fault", secret));
    assert_fail_open(
        &output,
        "systemMessage",
        PROMPT_GUIDANCE,
        &harness.root(),
        secret,
    );

    let explicit = harness.explicit_task_context();
    assert_eq!(explicit.status.code(), Some(2));
    let explicit_error: Value = serde_json::from_slice(&explicit.stderr).unwrap();
    assert_eq!(explicit_error["error"]["code"], "io_error");
}

#[test]
fn codex_hook_fails_open_when_runtime_database_is_corrupt() {
    let harness = Harness::new();
    initialize_store(&harness);
    let runtime = TaskRuntime::initialize(harness.root()).unwrap();
    let database = runtime.database_path().to_path_buf();
    drop(runtime);
    fs::write(database, b"RAW_CORRUPT_DATABASE_MESSAGE").unwrap();
    let secret = "PROMPT_SECRET_RUNTIME_CORRUPT";

    let output = harness.hook(
        "codex",
        &codex_prompt(&harness.home, "corrupt-fault", secret),
    );
    assert_fail_open(
        &output,
        "systemMessage",
        PROMPT_GUIDANCE,
        &harness.root(),
        secret,
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("RAW_CORRUPT_DATABASE_MESSAGE"));
}

#[test]
fn codex_hook_fails_open_when_runtime_database_is_busy() {
    let harness = Harness::new();
    initialize_store(&harness);
    let runtime = TaskRuntime::initialize(harness.root()).unwrap();
    let _lock = SqliteLock::acquire(runtime.database_path());
    let secret = "PROMPT_SECRET_RUNTIME_BUSY";

    let output = harness.hook("codex", &codex_prompt(&harness.home, "busy-fault", secret));
    assert_fail_open(
        &output,
        "systemMessage",
        PROMPT_GUIDANCE,
        &harness.root(),
        secret,
    );
}

#[test]
fn codex_hook_fails_open_when_index_update_is_busy() {
    let harness = Harness::new();
    let store = initialize_store(&harness);
    TaskRuntime::initialize(harness.root()).unwrap();
    let index = ProjectionIndex::for_store(&store);
    index.synchronize().unwrap();
    let _lock = SqliteLock::acquire(index.database_path());
    let event = Event::space_created(
        IntentSnapshot {
            title: "index fault fixture".to_owned(),
            problem: "the index must update".to_owned(),
            desired_outcome: "the Hook remains available".to_owned(),
            in_scope: vec!["Hook fail-open".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["coding continues".to_owned()],
            domain_terms: vec!["index-fault".to_owned()],
        },
        None,
    )
    .unwrap();
    store.append_event(AppendRequest::event(event)).unwrap();
    let secret = "PROMPT_SECRET_INDEX_BUSY";

    let output = harness.hook("codex", &codex_prompt(&harness.home, "index-fault", secret));
    assert_fail_open(
        &output,
        "systemMessage",
        PROMPT_GUIDANCE,
        &harness.root(),
        secret,
    );
}

#[test]
fn cursor_post_tool_hook_fails_open_when_runtime_is_unavailable() {
    let harness = Harness::new();
    initialize_store(&harness);
    let workspace = harness.home.join("cursor workspace");
    fs::create_dir_all(&workspace).unwrap();
    let file = workspace.join("contract.rs");
    fs::write(&file, "fn contract() {}\n").unwrap();
    fs::create_dir(harness.root().join("state/runtime.sqlite")).unwrap();
    let secret = "CURSOR_RAW_SECRET_MUST_NOT_LEAK";

    let output = harness.hook("cursor", &cursor_post_tool(&workspace, &file, secret));
    assert_fail_open(
        &output,
        "additional_context",
        DIAGNOSTIC,
        &harness.root(),
        secret,
    );
}

#[test]
fn cursor_post_tool_hook_ignores_repository_registry_failure() {
    let harness = Harness::new();
    initialize_store(&harness);
    let workspace = harness.home.join("registry workspace");
    fs::create_dir_all(&workspace).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let file = workspace.join("contract.rs");
    fs::write(&file, "fn contract() {}\n").unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let file = fs::canonicalize(file).unwrap();
    UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(None, std::slice::from_ref(&workspace))
        .unwrap();
    TaskRuntime::initialize(harness.root())
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("cursor", "cursor-fail-open").unwrap(),
            TaskId::new(),
            task_intent(),
            Vec::new(),
        )
        .unwrap();
    fs::create_dir(harness.root().join("state/repository-registry.sqlite")).unwrap();
    let secret = "CURSOR_REGISTRY_HOT_PATH_MUST_NOT_RUN";

    let output = harness.hook("cursor", &cursor_post_tool(&workspace, &file, secret));
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
}

#[test]
fn cursor_post_tool_hook_fails_open_when_catalog_config_is_invalid() {
    let harness = Harness::new();
    initialize_store(&harness);
    let workspace = harness.home.join("invalid catalog workspace");
    fs::create_dir_all(&workspace).unwrap();
    let file = workspace.join("contract.rs");
    fs::write(&file, "fn contract() {}\n").unwrap();
    TaskRuntime::initialize(harness.root())
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("cursor", "cursor-fail-open").unwrap(),
            TaskId::new(),
            task_intent(),
            Vec::new(),
        )
        .unwrap();
    fs::write(
        harness.root().join("config.toml"),
        "RAW_INVALID_CATALOG_CONFIG",
    )
    .unwrap();
    let secret = "CURSOR_CATALOG_CONFIG_MUST_NOT_LEAK";

    let output = harness.hook("cursor", &cursor_post_tool(&workspace, &file, secret));
    assert_fail_open(
        &output,
        "additional_context",
        DIAGNOSTIC,
        &harness.root(),
        secret,
    );
}

#[test]
fn cursor_post_tool_hook_fails_open_immediately_when_catalog_lock_is_busy() {
    let harness = Harness::new();
    initialize_store(&harness);
    let workspace = harness.home.join("locked catalog workspace");
    fs::create_dir_all(&workspace).unwrap();
    let file = workspace.join("contract.rs");
    fs::write(&file, "fn contract() {}\n").unwrap();
    TaskRuntime::initialize(harness.root())
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("cursor", "cursor-fail-open").unwrap(),
            TaskId::new(),
            task_intent(),
            Vec::new(),
        )
        .unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(harness.root().join("state/config.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let secret = "CURSOR_CATALOG_LOCK_MUST_NOT_LEAK";

    let started = std::time::Instant::now();
    let output = harness.hook("cursor", &cursor_post_tool(&workspace, &file, secret));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_fail_open(
        &output,
        "additional_context",
        DIAGNOSTIC,
        &harness.root(),
        secret,
    );
    FileExt::unlock(&lock).unwrap();
}
