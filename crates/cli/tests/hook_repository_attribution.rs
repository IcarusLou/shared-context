use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker};
use sctx_domain::{ExternalSessionLocator, TaskId, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::UserConfigStore;
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};

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
    fs::write(path.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::canonicalize(path).unwrap()
}

fn hook(home: &Path, payload: &Value) -> Value {
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
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

#[test]
fn post_tool_attribution_persists_no_evidence_or_unregistered_path() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("repository attribution home");
    let root = home.join(".shared-context");
    fs::create_dir_all(&home).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    let registered = git_repo(&home.join("registered"));
    let sibling = git_repo(&home.join("unregistered"));
    UserConfigStore::initialize(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&registered),
        )
        .unwrap();
    let session = "repository-attribution";
    assert_eq!(
        hook(
            &home,
            &json!({
                "session_id": session, "transcript_path": null, "cwd": registered,
                "hook_event_name": "SessionStart", "model": "gpt-5.6-sol",
                "permission_mode": "default", "source": "startup"
            }),
        ),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, session)
        }})
    );
    TaskRuntime::initialize(&root)
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("codex", session).unwrap(),
            TaskId::new(),
            WorkingIntentSnapshot::new("attribute safe PostTool paths").unwrap(),
            Vec::new(),
        )
        .unwrap();
    for (index, path) in [registered.join("src/lib.rs"), sibling.join("src/lib.rs")]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            hook(
                &home,
                &json!({
                    "session_id": session,
                    "transcript_path": format!("/tmp/RAW_ATTRIBUTION_{index}.jsonl"),
                    "cwd": path.parent().unwrap(), "hook_event_name": "PostToolUse",
                    "model": "gpt-5.6-sol", "permission_mode": "default",
                    "turn_id": format!("turn-{index}"),
                    "tool_name": "Read", "tool_use_id": format!("tool-{index}"),
                    "tool_input": {"absolute_file_path": path},
                    "tool_response": {"output": format!("RAW_ATTRIBUTION_{index}")}
                }),
            ),
            json!({})
        );
    }
    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!root.join("state").join(removed).exists());
    }
    let mut files = Vec::new();
    collect_files(&root.join("state"), &mut files);
    let persisted = files
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .flatten()
        .collect::<Vec<_>>();
    let persisted = String::from_utf8_lossy(&persisted);
    assert!(!persisted.contains("RAW_ATTRIBUTION_"));
    assert!(!persisted.contains(sibling.to_str().unwrap()));
}

/// A Session started at the common parent of two registered checkouts attributes a
/// `PostToolUse` that touches both, using its own startup directory as the Workspace root
/// (WP-N2). A path outside every registered checkout still makes the event non-locating.
#[test]
fn a_common_parent_session_attributes_across_its_registered_checkouts() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("common parent home");
    let root = home.join(".shared-context");
    fs::create_dir_all(&home).unwrap();
    GitStore::bootstrap_local(&root).unwrap();
    let parent = home.join("workspace");
    let first = git_repo(&parent.join("first"));
    let second = git_repo(&parent.join("second"));
    let sibling = git_repo(&parent.join("unregistered"));
    let parent = fs::canonicalize(&parent).unwrap();
    let config = UserConfigStore::initialize(&root).unwrap();
    for checkout in [&first, &second] {
        config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(checkout),
            )
            .unwrap();
    }

    let session = "common-parent-attribution";
    assert_eq!(
        hook(
            &home,
            &json!({
                "session_id": session, "transcript_path": null, "cwd": parent,
                "hook_event_name": "SessionStart", "model": "gpt-5.6-sol",
                "permission_mode": "default", "source": "startup"
            }),
        ),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, session)
        }}),
        "the common parent of two registered checkouts activates the Session"
    );
    TaskRuntime::initialize(&root)
        .unwrap()
        .open_or_create(
            ExternalSessionLocator::new("codex", session).unwrap(),
            TaskId::new(),
            WorkingIntentSnapshot::new("attribute across both registered checkouts").unwrap(),
            Vec::new(),
        )
        .unwrap();

    // Both files are registered, so the event locates and stays inside this Session's scope.
    assert_eq!(
        hook(
            &home,
            &json!({
                "session_id": session, "transcript_path": null, "cwd": parent,
                "hook_event_name": "PostToolUse", "model": "gpt-5.6-sol",
                "permission_mode": "default", "turn_id": "turn-both",
                "tool_name": "Read", "tool_use_id": "tool-both",
                "tool_input": {
                    "absolute_file_path": first.join("src/lib.rs"),
                    "nested": {"absolute_file_path": second.join("src/lib.rs")}
                },
                "tool_response": {"output": "RAW_PARENT_BOTH"}
            }),
        ),
        json!({})
    );

    // One unregistered path makes the whole event non-locating.
    assert_eq!(
        hook(
            &home,
            &json!({
                "session_id": session, "transcript_path": null, "cwd": parent,
                "hook_event_name": "PostToolUse", "model": "gpt-5.6-sol",
                "permission_mode": "default", "turn_id": "turn-sibling",
                "tool_name": "Read", "tool_use_id": "tool-sibling",
                "tool_input": {"absolute_file_path": sibling.join("src/lib.rs")},
                "tool_response": {"output": "RAW_PARENT_SIBLING"}
            }),
        ),
        json!({})
    );

    let mut files = Vec::new();
    collect_files(&root.join("state"), &mut files);
    let persisted = files
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .flatten()
        .collect::<Vec<_>>();
    let persisted = String::from_utf8_lossy(&persisted);
    assert!(!persisted.contains("RAW_PARENT_"));
    assert!(!persisted.contains(sibling.to_str().unwrap()));
}
