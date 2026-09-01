//! Hook-observed clues reach the Task Runtime as Task Signals.
//!
//! Every assertion here is about a *clue*: a Signal is never Evidence, never a Claim, never a
//! Git Event, and never something the model or the user sees. These tests therefore check three
//! things together at each step — what was stored, what was *not* stored, and that the Hook's
//! stdout stayed an empty object.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker};
use sctx_domain::{
    ExternalSessionLocator, RepositoryId, TaskId, TaskSignalKind, TaskSignalLifecycle,
    WorkingIntentSnapshot,
};
use sctx_git_store::GitStore;
use sctx_local_state::UserConfigStore;
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};

/// A GitHub token shaped exactly like the privacy scanner's `ghp_` pattern (24..96 trailing
/// characters), so a Prompt carrying it must never reach storage verbatim.
const PROMPT_SECRET: &str = "ghp_0123456789abcdefghijklmnopqrstuvwx";

struct Fixture {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    repository: PathBuf,
    repository_id: RepositoryId,
    outside: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join(format!("{name} home"));
        let root = home.join(".shared-context");
        fs::create_dir_all(&home).unwrap();
        GitStore::bootstrap_local(&root).unwrap();
        let repository = git_repo(&home.join("registered"));
        let outside = git_repo(&home.join("outside"));
        let repository_id = RepositoryId::from_str("Registered").unwrap();
        UserConfigStore::initialize(&root)
            .unwrap()
            .add_repository(repository_id.clone(), std::slice::from_ref(&repository))
            .unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            repository,
            repository_id,
            outside,
        }
    }

    fn hook(&self, agent: &str, payload: &Value) -> Value {
        let version = if agent == "cursor" {
            "3.13.10"
        } else {
            "0.147.0"
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["hook", "--agent", agent, "--agent-version", version])
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

    fn start_codex_session(&self, session: &str) {
        assert_eq!(
            self.hook(
                "codex",
                &json!({
                    "session_id": session, "transcript_path": null, "cwd": self.repository,
                    "hook_event_name": "SessionStart", "model": "gpt-5.6-sol",
                    "permission_mode": "default", "source": "startup"
                }),
            ),
            json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": shared_context_activation_marker(AgentKind::Codex, session)
            }})
        );
    }

    fn open_task(&self, agent: &str, session: &str, goal: &str) {
        TaskRuntime::initialize(&self.root)
            .unwrap()
            .open_or_create(
                ExternalSessionLocator::new(agent, session).unwrap(),
                TaskId::new(),
                WorkingIntentSnapshot::new(goal).unwrap(),
                Vec::new(),
            )
            .unwrap();
    }

    /// Active Signal contents of one kind, in Signal order.
    fn active_signals(&self, agent: &str, session: &str, kind: TaskSignalKind) -> Vec<String> {
        self.signal_history(agent, session)
            .into_iter()
            .filter(|(signal_kind, _, lifecycle)| {
                *signal_kind == kind && *lifecycle == TaskSignalLifecycle::Active
            })
            .map(|(_, content, _)| content)
            .collect()
    }

    fn signal_history(
        &self,
        agent: &str,
        session: &str,
    ) -> Vec<(TaskSignalKind, String, TaskSignalLifecycle)> {
        let runtime = TaskRuntime::initialize(&self.root).unwrap();
        let locator = ExternalSessionLocator::new(agent, session).unwrap();
        let Some(snapshot) = runtime.read_snapshot_by_locator(&locator).unwrap() else {
            return Vec::new();
        };
        runtime
            .read_signal_history(snapshot.task_session_id)
            .unwrap()
            .into_iter()
            .map(|record| (record.signal.kind, record.signal.content, record.lifecycle))
            .collect()
    }

    fn hook_event_reasons(&self) -> Vec<String> {
        TaskRuntime::initialize(&self.root)
            .unwrap()
            .recent_hook_events(64)
            .unwrap()
            .into_iter()
            .map(|event| event.reason)
            .collect()
    }

    /// Every regular file under `state/`, by path, with its exact bytes.
    fn state_bytes(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect_files(&self.root.join("state"), &self.root, &mut files);
        files
    }

    fn persisted_state_text(&self) -> String {
        let bytes = self
            .state_bytes()
            .into_values()
            .flatten()
            .collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn codex_prompt(&self, session: &str, turn: &str, prompt: &str) -> Value {
        json!({
            "session_id": session, "transcript_path": null, "cwd": self.repository,
            "hook_event_name": "UserPromptSubmit", "model": "gpt-5.6-sol",
            "permission_mode": "default", "turn_id": turn, "prompt": prompt
        })
    }

    fn codex_tool(&self, session: &str, turn: &str, tool: &str, file: &Path) -> Value {
        json!({
            "session_id": session, "transcript_path": null, "cwd": self.repository,
            "hook_event_name": "PostToolUse", "model": "gpt-5.6-sol",
            "permission_mode": "default", "turn_id": turn,
            "tool_name": tool, "tool_use_id": format!("call-{turn}"),
            "tool_input": {"absolute_file_path": file},
            "tool_response": {"output": "Done!"}
        })
    }
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
    fs::write(path.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::canonicalize(path).unwrap()
}

fn collect_files(directory: &Path, root: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, root, files);
        } else if let Ok(bytes) = fs::read(&path) {
            files.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
        }
    }
}

/// A Prompt joins an `ActiveTask` the Agent already declared, redacted and bounded — and never
/// creates one. Before `task_intent_update` there is nothing to attach a clue to, and the Hook
/// must not invent a Task, an Intent, or even a Runtime database to hold one.
#[test]
fn prompt_submit_records_a_redacted_bounded_signal_only_for_an_existing_active_task() {
    let fixture = Fixture::new("prompt signal");
    let session = "prompt-signal";
    fixture.start_codex_session(session);

    // No Task Runtime at all: an activated Session that never declared a Task stays inert.
    assert_eq!(
        fixture.hook("codex", &fixture.codex_prompt(session, "turn-0", "explore")),
        json!({})
    );
    assert!(!fixture.root.join("state/runtime.sqlite").exists());

    // A Runtime that exists but holds no ActiveTask is still not something to attach to.
    TaskRuntime::initialize(&fixture.root).unwrap();
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_prompt(session, "turn-1", "still exploring")
        ),
        json!({})
    );
    assert!(fixture.signal_history("codex", session).is_empty());
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"prompt_signal_skipped_no_task".to_owned())
    );

    fixture.open_task("codex", session, "record Prompt clues for an existing Task");
    let tail = "追加上下文 ".repeat(200);
    let prompt = format!("修复登录流程，token 是 {PROMPT_SECRET}，{tail}");
    assert!(prompt.chars().count() > 512);
    assert_eq!(
        fixture.hook("codex", &fixture.codex_prompt(session, "turn-2", &prompt)),
        json!({}),
        "a Prompt Signal is purely local and never becomes model-visible output"
    );

    let prompts = fixture.active_signals("codex", session, TaskSignalKind::Prompt);
    assert_eq!(prompts.len(), 1);
    let stored = &prompts[0];
    assert_eq!(
        stored.chars().count(),
        512,
        "the stored Prompt is truncated to its ceiling: {stored}"
    );
    assert!(stored.starts_with("修复登录流程，token 是 [REDACTED:github_token]"));
    assert!(!stored.contains(PROMPT_SECRET));
    // Redaction happens before truncation, so no secret can survive by sitting past the cut.
    assert!(!fixture.persisted_state_text().contains(PROMPT_SECRET));
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"prompt_signal_recorded".to_owned())
    );
}

/// Prompts accumulate for the life of a Session, so the Task keeps only its most recent eight.
/// Trimming supersedes; it never deletes, so the history stays complete.
#[test]
fn prompt_signals_are_deduplicated_and_bounded_to_the_most_recent_eight() {
    let fixture = Fixture::new("prompt retention");
    let session = "prompt-retention";
    fixture.start_codex_session(session);
    fixture.open_task("codex", session, "bound the Prompt Signal budget");

    for index in 0..10 {
        assert_eq!(
            fixture.hook(
                "codex",
                &fixture.codex_prompt(session, &format!("turn-{index}"), &format!("step {index}")),
            ),
            json!({})
        );
    }
    // A repeat of the newest Prompt is the same clue, not a ninth one.
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_prompt(session, "turn-repeat", "step 9")
        ),
        json!({})
    );

    let active = fixture.active_signals("codex", session, TaskSignalKind::Prompt);
    assert_eq!(
        active,
        (2..10)
            .map(|index| format!("step {index}"))
            .collect::<Vec<_>>()
    );
    let superseded = fixture
        .signal_history("codex", session)
        .into_iter()
        .filter(|(kind, _, lifecycle)| {
            *kind == TaskSignalKind::Prompt && *lifecycle == TaskSignalLifecycle::Superseded
        })
        .map(|(_, content, _)| content)
        .collect::<Vec<_>>();
    assert_eq!(superseded, vec!["step 0".to_owned(), "step 1".to_owned()]);
}

/// A file the Agent read and a file it rewrote are different clues, and both name a registered
/// Repository plus a checkout-relative path — never an absolute path from this machine.
#[test]
fn post_tool_file_signals_are_repository_relative_typed_and_idempotent() {
    let fixture = Fixture::new("file signal");
    let session = "file-signal";
    fixture.start_codex_session(session);
    fixture.open_task("codex", session, "record file clues from tool use");
    let file = fixture.repository.join("src/lib.rs");
    let expected = format!("{}:src/lib.rs", fixture.repository_id);

    for turn in ["read-1", "read-2"] {
        assert_eq!(
            fixture.hook(
                "codex",
                &fixture.codex_tool(session, turn, "read_file", &file)
            ),
            json!({})
        );
    }
    assert_eq!(
        fixture.active_signals("codex", session, TaskSignalKind::Workspace),
        vec![expected.clone()],
        "the same file read twice is one clue"
    );
    assert!(
        fixture
            .active_signals("codex", session, TaskSignalKind::Diff)
            .is_empty()
    );

    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_tool(session, "edit-1", "apply_patch", &file)
        ),
        json!({})
    );
    assert_eq!(
        fixture.active_signals("codex", session, TaskSignalKind::Diff),
        vec![expected.clone()],
        "rewriting a file it had only read is a new, stronger clue"
    );
    assert_eq!(
        fixture.active_signals("codex", session, TaskSignalKind::Workspace),
        vec![expected],
        "and it does not retract the read"
    );

    // An unregistered file makes the whole event non-locating: no clue, no absolute path.
    let outside = fixture.outside.join("src/lib.rs");
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_tool(session, "outside-1", "read_file", &outside)
        ),
        json!({})
    );
    assert_eq!(
        fixture
            .active_signals("codex", session, TaskSignalKind::Workspace)
            .len(),
        1
    );
    // The registered checkout path legitimately lives in the Catalog and the Session lease. What
    // must never appear is an unregistered path, or an absolute path inside a Signal.
    assert!(
        !fixture
            .persisted_state_text()
            .contains(fixture.outside.to_str().unwrap())
    );
    let checkout = fixture.repository.to_str().unwrap();
    assert!(
        fixture
            .signal_history("codex", session)
            .iter()
            .all(|(_, content, _)| !content.contains(checkout))
    );
}

/// `Diff` and `Workspace` share one budget of sixteen, because both describe files this Task
/// touched. The oldest are superseded, so what survives is what the Task touched most recently.
#[test]
fn file_signals_are_bounded_to_sixteen_across_diff_and_workspace() {
    let fixture = Fixture::new("file retention");
    let session = "file-retention";
    fixture.start_codex_session(session);
    fixture.open_task("codex", session, "bound the file Signal budget");

    for index in 0..20 {
        let file = fixture.repository.join(format!("src/unit{index:02}.rs"));
        fs::write(&file, format!("pub fn unit{index}() {{}}\n")).unwrap();
        assert_eq!(
            fixture.hook(
                "codex",
                &fixture.codex_tool(session, &format!("read-{index}"), "read_file", &file),
            ),
            json!({})
        );
    }

    let active = fixture.active_signals("codex", session, TaskSignalKind::Workspace);
    assert_eq!(
        active,
        (4..20)
            .map(|index| format!("{}:src/unit{index:02}.rs", fixture.repository_id))
            .collect::<Vec<_>>()
    );
}

/// A Session Shared Context was never authorized for must leave exactly zero local residue, and
/// a Prompt — the one event that carries the user's own words — is no exception.
#[test]
fn a_disabled_session_prompt_writes_nothing_at_all() {
    let fixture = Fixture::new("disabled prompt");
    let session = "disabled-prompt";
    let start = json!({
        "session_id": session, "transcript_path": null, "cwd": fixture.outside,
        "hook_event_name": "SessionStart", "model": "gpt-5.6-sol",
        "permission_mode": "default", "source": "startup"
    });
    assert_eq!(fixture.hook("codex", &start), json!({}));

    let before = fixture.state_bytes();
    let prompt = json!({
        "session_id": session, "transcript_path": null, "cwd": fixture.outside,
        "hook_event_name": "UserPromptSubmit", "model": "gpt-5.6-sol",
        "permission_mode": "default", "turn_id": "turn-1",
        "prompt": format!("deploy with {PROMPT_SECRET} right now")
    });
    assert_eq!(fixture.hook("codex", &prompt), json!({}));

    assert_eq!(fixture.state_bytes(), before);
    assert!(!fixture.root.join("state/runtime.sqlite").exists());
}

/// Cursor sends its tool result as a JSON string, and its `exitCode` is a decidable failure
/// marker. Reading it is what makes `test runner failed` reachable on Cursor at all.
#[test]
fn cursor_tool_output_exit_code_decides_the_test_outcome_signal() {
    let fixture = Fixture::new("cursor outcome");
    let session = "cursor-outcome";
    let start = json!({
        "conversation_id": session, "generation_id": "gen-0", "model": "claude-opus-4-7",
        "hook_event_name": "sessionStart", "cursor_version": "3.13.10",
        "workspace_roots": [fixture.repository], "user_email": null,
        "transcript_path": null, "session_id": session,
        "is_background_agent": false, "composer_mode": "agent"
    });
    assert_eq!(
        fixture.hook("cursor", &start),
        json!({"additional_context": shared_context_activation_marker(AgentKind::Cursor, session)})
    );
    fixture.open_task("cursor", session, "decide Cursor tool outcomes");

    for (turn, tool_output, expected) in [
        (
            "pass",
            json!({"exitCode": 0, "stdout": "All tests passed"}).to_string(),
            "test runner succeeded",
        ),
        (
            "fail",
            json!({"exitCode": 101, "stdout": "test failed"}).to_string(),
            "test runner failed",
        ),
    ] {
        let payload = json!({
            "conversation_id": session, "generation_id": format!("gen-{turn}"),
            "model": "claude-opus-4-7", "hook_event_name": "postToolUse",
            "cursor_version": "3.13.10", "workspace_roots": [fixture.repository],
            "user_email": null, "transcript_path": null,
            "tool_name": "Shell",
            "tool_input": {"command": "cargo test", "working_directory": fixture.repository},
            "tool_output": tool_output,
            "tool_use_id": format!("tool-{turn}"),
            "cwd": fixture.repository, "duration": 10
        });
        assert_eq!(fixture.hook("cursor", &payload), json!({}));
        assert!(
            fixture
                .active_signals("cursor", session, TaskSignalKind::TestOutcome)
                .contains(&expected.to_owned()),
            "{turn} run must record {expected}"
        );
    }
}
