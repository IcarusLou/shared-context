use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_domain::{ExternalSessionLocator, TaskId, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::UserConfigStore;
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};

const WORKERS: usize = 32;
const HOOK_DIAGNOSTIC: &str = "Shared Context task retrieval is temporarily unavailable. Coding can continue; retry through MCP or CLI later.";

fn intent() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: "Bound the concurrent Hook signal path".to_owned(),
        current_direction: Some("Run structured test tools concurrently".to_owned()),
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: vec!["hooks".to_owned()],
        platforms: vec!["cursor".to_owned()],
        constraints: vec!["fail open".to_owned()],
        acceptance_conditions: vec!["p99 remains below 500ms".to_owned()],
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn run_hook(home: &Path, payload: &Value) -> (std::process::Output, Duration) {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["hook", "--agent", "cursor", "--agent-version", "3.13.10"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.as_mut().unwrap(), payload).unwrap();
    drop(child.stdin.take());
    (child.wait_with_output().unwrap(), started.elapsed())
}

fn session_start(session: &str, workspace: &Path) -> Value {
    json!({
        "conversation_id": session,
        "generation_id": "generation-hot-path",
        "model": "claude-opus-4-7",
        "hook_event_name": "sessionStart",
        "cursor_version": "3.13.10",
        "workspace_roots": [workspace],
        "user_email": null,
        "transcript_path": null,
        "session_id": session,
        "is_background_agent": false,
        "composer_mode": "agent"
    })
}

fn post_tool(session: &str, workspace: &Path, file: &Path, index: usize) -> Value {
    json!({
        "conversation_id": session,
        "generation_id": format!("generation-{index}"),
        "model": "claude-opus-4-7",
        "hook_event_name": "postToolUse",
        "cursor_version": "3.13.10",
        "workspace_roots": [workspace],
        "user_email": null,
        "transcript_path": null,
        "tool_name": "Read",
        "tool_input": {
            "absolute_file_path": file,
            "raw_marker": format!("RAW_HOT_PATH_{index}")
        },
        "tool_output": format!("RAW_HOT_PATH_{index}"),
        "tool_use_id": format!("tool-hot-path-{index}"),
        "cwd": workspace,
        "duration": 1
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn thirty_two_post_tool_hooks_are_bounded_fail_open_and_write_zero_capture_state() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("Hook 热路径 home");
    let root = home.join(".shared-context");
    fs::create_dir_all(&home).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let workspace = fs::canonicalize(workspace).unwrap();
    let source = workspace.join("hot_path.rs");
    fs::write(&source, "fn hot_path() {}\n").unwrap();
    let source = fs::canonicalize(source).unwrap();
    UserConfigStore::open_existing(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&workspace),
        )
        .unwrap();

    let session = "cursor-hot-path";
    let (start, _) = run_hook(&home, &session_start(session, &workspace));
    assert!(start.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&start.stdout).unwrap(),
        json!({"additional_context": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
    TaskRuntime::initialize(&root)
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("cursor", session).unwrap(),
            TaskId::new(),
            intent(),
            Vec::new(),
        )
        .unwrap();

    let barrier = Arc::new(Barrier::new(WORKERS));
    let mut workers = Vec::new();
    for index in 0..WORKERS {
        let barrier = Arc::clone(&barrier);
        let home = home.clone();
        let workspace = workspace.clone();
        let source = source.clone();
        workers.push(thread::spawn(move || {
            let payload = post_tool(session, &workspace, &source, index);
            barrier.wait();
            run_hook(&home, &payload)
        }));
    }
    let mut durations = Vec::new();
    for worker in workers {
        let (output, duration) = worker.join().unwrap();
        assert!(
            output.status.success(),
            "Hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response = serde_json::from_slice::<Value>(&output.stdout).unwrap();
        assert!(
            response == json!({}) || response == json!({"additional_context": HOOK_DIAGNOSTIC}),
            "unexpected fail-open response: {response}"
        );
        let observable = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!observable.contains("RAW_HOT_PATH_"));
        durations.push(duration);
    }
    durations.sort_unstable();
    let p99 = durations[(WORKERS * 99).div_ceil(100) - 1];
    eprintln!("32-way PostToolUse p99={p99:?}");
    assert!(p99 < Duration::from_millis(500), "Hook p99 was {p99:?}");

    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!root.join("state").join(removed).exists());
    }
    assert!(fs::read_dir(root.join("state")).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}
