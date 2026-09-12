//! Hook-observed clues reach the Task Runtime as Task Signals.
//!
//! Every assertion here is about a *clue*: a Signal is never Evidence, never a Claim, never a
//! Git Event, and never something the model or the user sees. These tests therefore check three
//! things together at each step — what was stored, what was *not* stored, and that the Hook's
//! stdout stayed an empty object.

mod logging_harness;

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker_with_policy};
use sctx_domain::{
    ExternalSessionLocator, RepositoryId, TaskId, TaskSignalKind, TaskSignalLifecycle,
    WorkingIntentSnapshot,
};
use sctx_git_store::GitStore;
use sctx_local_state::Policy;
use sctx_local_state::{AuthorizedSessionScopeRead, AuthorizedSessionScopeStore, UserConfigStore};
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};

use logging_harness::LoggingHarness;

/// A GitHub token shaped exactly like the privacy scanner's `ghp_` pattern (24..96 trailing
/// characters), so a Prompt carrying it must never reach storage verbatim.
const PROMPT_SECRET: &str = "ghp_0123456789abcdefghijklmnopqrstuvwx";

struct Fixture {
    logging: LoggingHarness,
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
        let logging = LoggingHarness::start(&home);
        Self {
            logging,
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
        self.logging.apply(&mut command);
        let mut child = command
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

    /// Whether the lease has already spent this Session's one-shot marker delivery.
    fn activation_marker_delivered(&self, agent: &str, session: &str) -> bool {
        let read = AuthorizedSessionScopeStore::initialize(&self.root)
            .unwrap()
            .read(&ExternalSessionLocator::new(agent, session).unwrap())
            .unwrap();
        let AuthorizedSessionScopeRead::Current(scope) = read else {
            panic!("the event under test must have built a lease");
        };
        scope.activation_marker_delivered
    }

    /// One Codex `PreCompact`, the shape a real compaction boundary arrives in.
    fn codex_pre_compact(&self, session: &str, turn: &str) -> Value {
        json!({
            "session_id": session, "transcript_path": null, "cwd": self.repository,
            "hook_event_name": "PreCompact", "model": "gpt-5.6-sol",
            "permission_mode": "default", "turn_id": turn, "trigger": "auto"
        })
    }

    /// One Codex `Stop`, the shape a completed turn arrives in.
    fn codex_stop(&self, session: &str, turn: &str) -> Value {
        json!({
            "session_id": session, "transcript_path": null, "cwd": self.repository,
            "hook_event_name": "Stop", "model": "gpt-5.6-sol",
            "permission_mode": "default", "turn_id": turn,
            "stop_hook_active": false, "last_assistant_message": "Done."
        })
    }

    fn cursor_pre_compact(&self, session: &str, turn: &str) -> Value {
        json!({
            "conversation_id": session, "generation_id": format!("gen-{turn}"),
            "model": "claude-opus-4-7", "hook_event_name": "preCompact",
            "cursor_version": "3.13.10", "workspace_roots": [self.repository],
            "user_email": null, "transcript_path": null, "trigger": "auto"
        })
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
        self.hook_events()
            .into_iter()
            .map(|(reason, _)| reason)
            .collect()
    }

    fn hook_events(&self) -> Vec<(String, Option<String>)> {
        self.logging
            .diagnostics()
            .recent_events
            .into_iter()
            .map(|event| (event.reason.unwrap_or_default(), event.summary))
            .collect()
    }

    fn hook_event_summary(&self, reason: &str) -> Option<String> {
        self.hook_events()
            .into_iter()
            .find(|(event_reason, _)| event_reason == reason)
            .map(|(_, detail)| detail)?
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

    /// One Codex `exec` call: the shape every tool call of a real Codex Session arrives in.
    fn codex_exec(&self, session: &str, turn: &str, command: &Value) -> Value {
        json!({
            "session_id": session, "transcript_path": null, "cwd": self.repository,
            "hook_event_name": "PostToolUse", "model": "gpt-5.6-sol",
            "permission_mode": "default", "turn_id": turn,
            "tool_name": "shell", "tool_use_id": format!("call-{turn}"),
            "tool_input": {"command": command, "workdir": self.repository},
            "tool_response": {"output": "Done!"}
        })
    }

    fn cursor_tool(&self, session: &str, turn: &str, tool: &str, input: &Value) -> Value {
        json!({
            "conversation_id": session, "generation_id": format!("gen-{turn}"),
            "model": "claude-opus-4-7", "hook_event_name": "postToolUse",
            "cursor_version": "3.13.10", "workspace_roots": [self.repository],
            "user_email": null, "transcript_path": null,
            "tool_name": tool, "tool_input": input,
            "tool_output": json!({"contents": "ok"}).to_string(),
            "tool_use_id": format!("tool-{turn}"), "duration": 10
        })
    }

    fn start_cursor_session(&self, session: &str) {
        assert_eq!(
            self.hook(
                "cursor",
                &json!({
                    "conversation_id": session, "generation_id": "gen-0",
                    "model": "claude-opus-4-7", "hook_event_name": "sessionStart",
                    "cursor_version": "3.13.10", "workspace_roots": [self.repository],
                    "user_email": null, "transcript_path": null, "session_id": session,
                    "is_background_agent": false, "composer_mode": "agent"
                }),
            ),
            json!({
                "additional_context": shared_context_activation_marker(AgentKind::Cursor, session)
            })
        );
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

/// A Prompt joins an `ActiveTask` the Agent already declared, redacted and bounded. Before
/// `task_intent_update` there is nothing to attach it to, so it is held — without inventing a
/// Task, an Intent, or even a Runtime database to hold one — and attached when the Task arrives.
#[test]
fn prompt_submit_records_a_redacted_bounded_signal_and_backfills_the_one_that_came_first() {
    let fixture = Fixture::new("prompt signal");
    let session = "prompt-signal";
    fixture.start_codex_session(session);

    // No Task Runtime at all: a Prompt must never be the event that first writes it.
    assert_eq!(
        fixture.hook("codex", &fixture.codex_prompt(session, "turn-0", "explore")),
        json!({})
    );
    assert!(!fixture.root.join("state/runtime.sqlite").exists());
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"prompt_signal_skipped_no_task".to_owned())
    );

    // A Runtime that exists but holds no ActiveTask: the Prompt is held, not dropped, and holding
    // it still creates no Task state of any kind.
    TaskRuntime::initialize(&fixture.root).unwrap();
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_prompt(session, "turn-1", "修复登录流程在搜索结果页的回退路径")
        ),
        json!({})
    );
    assert!(fixture.signal_history("codex", session).is_empty());
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"prompt_signal_stashed".to_owned())
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
    assert_eq!(
        prompts.len(),
        2,
        "the Prompt that stated the work is backfilled beside the one submitted after: {prompts:?}"
    );
    assert_eq!(
        prompts[0], "修复登录流程在搜索结果页的回退路径",
        "what came first takes the lower ordinal, which is what earlier means here"
    );
    let stored = &prompts[1];
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

/// The stash holds two Prompts and spends neither on a slash command, and everything it holds has
/// already been through the redactor.
#[test]
fn the_pending_prompt_stash_is_bounded_redacted_and_never_spent_on_a_slash_command() {
    let fixture = Fixture::new("pending prompt stash");
    let session = "pending-stash";
    fixture.start_codex_session(session);
    TaskRuntime::initialize(&fixture.root).unwrap();

    for (turn, prompt) in [
        ("turn-0", "/sctx-review"),
        ("turn-1", "/compact please shorten the transcript"),
    ] {
        assert_eq!(
            fixture.hook("codex", &fixture.codex_prompt(session, turn, prompt)),
            json!({})
        );
    }
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"prompt_signal_skipped_slash_command".to_owned())
    );

    for (turn, prompt) in [
        (
            "turn-2",
            format!("把 token {PROMPT_SECRET} 从搜索直播卡片里摘掉"),
        ),
        ("turn-3", "顺便把兜底样式也统一一下".to_owned()),
        ("turn-4", "再补一句：别动埋点".to_owned()),
    ] {
        assert_eq!(
            fixture.hook("codex", &fixture.codex_prompt(session, turn, &prompt)),
            json!({})
        );
    }
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"prompt_signal_pending_full".to_owned()),
        "the third Prompt before a Task is elaboration, and the Intent will carry it better"
    );
    assert!(
        !fixture.persisted_state_text().contains(PROMPT_SECRET),
        "a held Prompt went through the same redactor a recorded one does"
    );

    fixture.open_task("codex", session, "摘掉搜索直播卡片上的 token");
    let prompts = fixture.active_signals("codex", session, TaskSignalKind::Prompt);
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(prompts[0].starts_with("把 token [REDACTED:github_token]"));
    assert_eq!(prompts[1], "顺便把兜底样式也统一一下");
    assert!(!fixture.persisted_state_text().contains(PROMPT_SECRET));
}

/// A backfill happens once. The Task that took the pending Prompts empties the stash, so a second
/// Task in the same Session starts from the Prompts it is actually given.
#[test]
fn a_backfilled_prompt_is_handed_over_once_and_the_stash_is_then_empty() {
    let fixture = Fixture::new("prompt backfill once");
    let session = "backfill-once";
    fixture.start_codex_session(session);
    TaskRuntime::initialize(&fixture.root).unwrap();
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_prompt(session, "turn-0", "把地图相机重入问题查清楚")
        ),
        json!({})
    );
    fixture.open_task("codex", session, "查清地图相机重入");
    assert_eq!(
        fixture.active_signals("codex", session, TaskSignalKind::Prompt),
        vec!["把地图相机重入问题查清楚".to_owned()]
    );

    let runtime = TaskRuntime::initialize(&fixture.root).unwrap();
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let active = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    runtime
        .start_new_task(
            &locator,
            active.task_id,
            &active
                .current_intent_revision()
                .unwrap()
                .working_intent
                .clone(),
            Vec::new(),
        )
        .unwrap();
    assert!(
        runtime
            .read_snapshot_by_locator(&locator)
            .unwrap()
            .unwrap()
            .task_signals
            .is_empty(),
        "the stash was emptied by the Task that took it"
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
    assert!(
        fixture
            .logging
            .diagnostics_or_empty()
            .recent_events
            .is_empty()
    );
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

/// A Codex Session spends essentially every tool call in `exec`, so before command candidates
/// existed such a Session contributed no file clue at all: the category was always `Shell`, the
/// hints were always empty, and the Task Signal table stayed empty for its whole life. The
/// command is still never stored — only the files it named, resolved against the Catalog, land
/// as `<RepositoryId>:<checkout-relative path>`.
#[test]
fn codex_exec_commands_contribute_repository_relative_workspace_signals() {
    let fixture = Fixture::new("exec signal");
    let session = "exec-signal";
    fixture.start_codex_session(session);
    fixture.open_task("codex", session, "record file clues from exec commands");
    let expected = format!("{}:src/lib.rs", fixture.repository_id);

    // The shape a model normally phrases an `exec` call in: an argument vector wrapping one
    // simple command. A relative path is resolved against the event's own working directory.
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_exec(session, "cat-1", &json!(["bash", "-lc", "cat src/lib.rs"])),
        ),
        json!({})
    );
    assert_eq!(
        fixture.active_signals("codex", session, TaskSignalKind::Workspace),
        vec![expected.clone()],
        "an exec that read a registered file is a Workspace clue"
    );
    // An `exec` cannot say whether it read or rewrote what it named, so it never claims the
    // stronger of the two kinds.
    assert!(
        fixture
            .active_signals("codex", session, TaskSignalKind::Diff)
            .is_empty()
    );

    // A guess never fails an event and never widens it: a file in another checkout, a path that
    // does not exist, and a directory are all dropped while the event stays attributed.
    let outside = fixture.outside.join("src/lib.rs");
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_exec(
                session,
                "mixed-1",
                &json!([
                    "cat",
                    outside.to_str().unwrap(),
                    "src/absent.rs",
                    "src",
                    "src/lib.rs"
                ]),
            ),
        ),
        json!({})
    );
    assert_eq!(
        fixture.active_signals("codex", session, TaskSignalKind::Workspace),
        vec![expected],
        "only the registered file survives the guess"
    );
    let reasons = fixture.hook_event_reasons();
    assert!(
        !reasons.contains(&"attribution_failed".to_owned()),
        "a rejected guess is not an attribution failure: {reasons:?}"
    );
    assert!(reasons.contains(&"file_signal_recorded".to_owned()));

    // The command text itself never crosses into storage — only attributed, relative paths.
    let state = fixture.persisted_state_text();
    for command_text in ["bash", "-lc", "src/absent.rs"] {
        assert!(
            !state.contains(command_text),
            "{command_text:?} must not reach local state"
        );
    }
    assert!(!state.contains(fixture.outside.to_str().unwrap()));
}

/// Two very different outcomes used to share one reason, and the louder one hid the quieter: an
/// event that could place no file at all was reported as an event whose Signals were already
/// known. That is what made a Codex Session's empty Signal table look like healthy deduplication.
#[test]
fn an_event_with_no_attributable_file_is_not_reported_as_deduplicated() {
    let fixture = Fixture::new("empty signal reason");
    let session = "empty-signal-reason";
    fixture.start_codex_session(session);
    fixture.open_task("codex", session, "separate the two empty-Signal outcomes");

    // Nothing in this command names a file, so this event places nothing.
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_exec(session, "check-1", &json!(["cargo", "check"])),
        ),
        json!({})
    );
    assert!(fixture.signal_history("codex", session).is_empty());
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"no_attributable_files".to_owned())
    );
    assert!(
        !fixture
            .hook_event_reasons()
            .contains(&"signal_write_skipped_nothing_new".to_owned())
    );

    // The same file read twice does place a file — the second read is genuinely deduplicated.
    let file = fixture.repository.join("src/lib.rs");
    for turn in ["read-1", "read-2"] {
        assert_eq!(
            fixture.hook(
                "codex",
                &fixture.codex_tool(session, turn, "read_file", &file)
            ),
            json!({})
        );
    }
    assert!(
        fixture
            .hook_event_reasons()
            .contains(&"signal_write_skipped_nothing_new".to_owned())
    );
}

/// Shared Context's own MCP tool calls plan no Signal merge on purpose. Running attribution over
/// them reported that by-design path as `attribution_failed` on every single call, which is how
/// a healthy Session accumulated dozens of failure rows describing nothing.
#[test]
fn a_shared_context_tool_call_records_no_attribution_failure() {
    let fixture = Fixture::new("shared context tool");
    let session = "shared-context-tool";
    fixture.start_codex_session(session);
    fixture.open_task(
        "codex",
        session,
        "keep our own tool calls out of observation",
    );

    let file = fixture.repository.join("src/lib.rs");
    assert_eq!(
        fixture.hook(
            "codex",
            &fixture.codex_tool(session, "checkpoint-1", "task_checkpoint", &file)
        ),
        json!({})
    );
    let reasons = fixture.hook_event_reasons();
    assert!(
        !reasons.contains(&"attribution_failed".to_owned()),
        "our own tool call is not an attribution failure: {reasons:?}"
    );
    assert!(fixture.signal_history("codex", session).is_empty());
}

/// Cursor's desktop build sends some tool events with a relative path. The absolute-path rule
/// rejected those outright, so every such event lost its file clue and left one
/// `attribution_failed` row behind. The event already says where it ran.
#[test]
fn cursor_relative_tool_paths_resolve_against_the_event_workspace() {
    let fixture = Fixture::new("cursor relative");
    let session = "cursor-relative";
    fixture.start_cursor_session(session);
    fixture.open_task("cursor", session, "place relative Cursor tool paths");
    let expected = format!("{}:src/lib.rs", fixture.repository_id);

    // With an explicit working directory, and — as the desktop build sends it — without one,
    // in which case the first declared Workspace root is the base.
    for (turn, cwd) in [
        ("with-cwd", Some(fixture.repository.clone())),
        ("without-cwd", None),
    ] {
        let mut payload = fixture.cursor_tool(
            session,
            turn,
            "read_file",
            &json!({"file_path": "src/lib.rs"}),
        );
        if let Some(cwd) = cwd {
            payload["cwd"] = json!(cwd);
        }
        assert_eq!(fixture.hook("cursor", &payload), json!({}));
        assert_eq!(
            fixture.active_signals("cursor", session, TaskSignalKind::Workspace),
            vec![expected.clone()],
            "{turn} must place the relative path"
        );
    }
    let reasons = fixture.hook_event_reasons();
    assert!(
        !reasons.contains(&"attribution_failed".to_owned()),
        "a relative path the event can place is not a failure: {reasons:?}"
    );

    // A relative path that escapes the Workspace still cannot be placed, and still records no
    // absolute path from this machine.
    let escape = fixture.cursor_tool(
        session,
        "escape",
        "read_file",
        &json!({"file_path": "../outside/src/lib.rs"}),
    );
    assert_eq!(fixture.hook("cursor", &escape), json!({}));
    assert_eq!(
        fixture.active_signals("cursor", session, TaskSignalKind::Workspace),
        vec![expected]
    );
    assert!(
        !fixture
            .persisted_state_text()
            .contains(fixture.outside.to_str().unwrap())
    );
}

/// Undecodable payloads produce only closed adapter metadata. The event type is never copied from
/// an unknown host string, while the documented Session id is hashed before it crosses the wire.
#[test]
fn an_undecodable_payload_records_only_closed_diagnostics() {
    let fixture = Fixture::new("undecodable payload");
    TaskRuntime::initialize(&fixture.root).unwrap();
    let secret = "ghp_undecodable_payload_value_must_not_be_recorded";
    assert_eq!(
        fixture.hook(
            "codex",
            &json!({
                "hook_event_name": "SomeUndocumentedEvent",
                "session_id": "undecodable",
                "novel field!": secret
            }),
        ),
        json!({})
    );
    assert_eq!(
        fixture.hook(
            "codex",
            &json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": "supported-but-invalid",
                "transcript_path": null,
                "cwd": fixture.root,
                "model": "gpt-5.6-sol",
                "prompt": secret
            }),
        ),
        json!({})
    );
    assert_eq!(
        fixture.hook(
            "codex",
            &json!({
                "hook_event_name": "PreCompact",
                "session_id": "invalid-enum",
                "transcript_path": null,
                "cwd": fixture.root,
                "model": "gpt-5.6-sol",
                "turn_id": "turn-invalid-enum",
                "trigger": secret
            }),
        ),
        json!({})
    );

    let events = fixture.logging.diagnostics().recent_events;
    let event = events
        .iter()
        .find(|event| event.reason.as_deref() == Some("payload_decode_failed"))
        .expect("an undecodable payload records a typed collector diagnostic");
    assert_eq!(event.operation.as_deref(), Some("hook.codex.undecodable"));
    assert_eq!(event.error_code.as_deref(), Some("unknown_event"));
    assert_eq!(event.error_family.as_deref(), Some("unknown_event"));
    assert!(
        event
            .session_digest
            .as_ref()
            .is_some_and(|value| value.len() == 64)
    );
    assert_eq!(
        event.summary, None,
        "free text is never persisted for decode failures"
    );
    assert_eq!(fixture.hook_event_summary("payload_decode_failed"), None);
    let supported = events
        .iter()
        .find(|event| event.operation.as_deref() == Some("hook.codex.prompt_submit"))
        .expect("supported event type remains visible without retaining host text");
    assert_eq!(supported.error_code.as_deref(), Some("missing_field"));
    assert_eq!(supported.error_family.as_deref(), Some("field.turn_id"));
    assert!(
        supported
            .session_digest
            .as_ref()
            .is_some_and(|value| value.len() == 64)
    );
    let invalid_enum = events
        .iter()
        .find(|event| event.operation.as_deref() == Some("hook.codex.pre_compact"))
        .expect("supported enum failure remains classifiable without its value");
    assert_eq!(invalid_enum.error_code.as_deref(), Some("enum"));
    assert_eq!(invalid_enum.error_family.as_deref(), Some("field.trigger"));
    let telemetry = fixture.logging.persisted_text();
    for forbidden in [
        secret,
        "SomeUndocumentedEvent",
        "novel field!",
        fixture.root.to_str().unwrap(),
    ] {
        assert!(
            !telemetry.contains(forbidden),
            "telemetry leaked {forbidden:?}"
        );
    }
    assert!(!fixture.persisted_state_text().contains(secret));
}

/// `SessionStart` is the only event that renders the activation marker, so a `SessionStart` that
/// failed open — one busy maintenance lock is enough — left its Session permanently without the
/// one datum the Agent cannot guess. The lease self-heal already rebuilds authorization from a
/// later event; it now rebuilds the marker with it, exactly once.
#[test]
fn a_session_that_never_started_gets_its_marker_from_the_self_healing_event() {
    let fixture = Fixture::new("self healed marker");
    for (agent, session) in [("codex", "healed-codex"), ("cursor", "healed-cursor")] {
        // The Task exists but the lease does not, so the marker is the only thing this event
        // owes and the Intent bootstrap reminder cannot compete for the field.
        fixture.open_task(
            agent,
            session,
            "re-state the marker a SessionStart never sent",
        );
        let kind = if agent == "codex" {
            AgentKind::Codex
        } else {
            AgentKind::Cursor
        };
        let marker = shared_context_activation_marker(kind, session);
        let payload = |turn: &str| {
            if agent == "codex" {
                fixture.codex_exec(session, turn, &json!(["cargo", "check"]))
            } else {
                let mut payload = fixture.cursor_tool(
                    session,
                    turn,
                    "read_file",
                    &json!({"file_path": "src/lib.rs"}),
                );
                payload["cwd"] = json!(fixture.repository);
                payload
            }
        };
        let expected = if agent == "codex" {
            json!({"hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": marker,
            }})
        } else {
            json!({"additional_context": marker})
        };

        // No SessionStart ever reached the Hook: this event builds the lease and owes the marker.
        assert_eq!(fixture.hook(agent, &payload("heal-1")), expected);
        // The delivery is recorded in the lease, so the next event stays silent.
        assert_eq!(fixture.hook(agent, &payload("heal-2")), json!({}));
    }
}

/// The one-shot delivery is spent only where this host would actually carry it.
///
/// A Codex `PreCompact` and `Stop` cannot: neither has a `hookSpecificOutput` variant, so anything
/// written to model context is dropped with the rest of the object. Repairing a lease on one of
/// them must therefore leave the delivery unspent — recording a delivery the host discarded is a
/// lie about what the Session was told, and it is the lie that permanently costs that Session the
/// `external_session_id` it has to send back. The same two events on Cursor do carry
/// `user_message`, so there the marker is delivered exactly as before.
#[test]
fn a_codex_lease_repaired_at_a_compaction_or_stop_boundary_keeps_its_marker_delivery() {
    let fixture = Fixture::new("boundary self heal");

    for (turn, boundary) in [
        ("boundary-precompact", "PreCompact"),
        ("boundary-stop", "Stop"),
    ] {
        // Each boundary gets its own Session, because the question is what the *first* event
        // after a missing SessionStart does with the delivery.
        let session = format!("healed-codex-{turn}");
        fixture.open_task("codex", &session, "repair a lease at a boundary event");
        let payload = if boundary == "PreCompact" {
            fixture.codex_pre_compact(&session, turn)
        } else {
            fixture.codex_stop(&session, turn)
        };

        // The boundary line reaches the user; nothing reaches the model, and the lease still
        // owes a marker.
        let output = fixture.hook("codex", &payload);
        assert!(
            output.get("hookSpecificOutput").is_none(),
            "{boundary}: {output}"
        );
        assert!(
            !output.to_string().contains("<shared-context-active"),
            "{boundary}: {output}"
        );
        assert!(
            !fixture.activation_marker_delivered("codex", &session),
            "{boundary} must not spend a delivery Codex would drop"
        );

        // The delivery is still unspent on the following `PostToolUse`, and today nothing
        // hands it over there: the offer is made only by the event that *creates* the lease,
        // and this lease already exists. That gap is deferred-issues #37, and it is a
        // separate defect from the one under test here — what this asserts is that the flag
        // stays honest, so the repair that closes #37 has something true to act on.
        assert_eq!(
            fixture.hook(
                "codex",
                &fixture.codex_exec(&session, "after", &json!(["cargo", "check"]))
            ),
            json!({}),
            "{boundary}"
        );
        assert!(!fixture.activation_marker_delivered("codex", &session));
    }

    // Cursor is unchanged: its `preCompact` carries `user_message`, so the repair delivers there.
    let cursor_session = "healed-cursor-boundary";
    fixture.open_task(
        "cursor",
        cursor_session,
        "repair a lease at a boundary event",
    );
    let output = fixture.hook(
        "cursor",
        &fixture.cursor_pre_compact(cursor_session, "boundary-precompact"),
    );
    let user_message = output["user_message"].as_str().unwrap_or_default();
    assert!(
        user_message.contains(&shared_context_activation_marker(
            AgentKind::Cursor,
            cursor_session
        )),
        "{output}"
    );
    assert!(fixture.activation_marker_delivered("cursor", cursor_session));
}

/// An unauthorized Session must leave exactly zero local residue, and a self-heal that decides
/// Disabled is no exception: it delivers no marker and writes no lease bookkeeping.
#[test]
fn a_disabled_self_heal_delivers_no_marker_and_writes_nothing() {
    let fixture = Fixture::new("disabled self heal");
    let event = |turn: &str| {
        json!({
            "session_id": "disabled-heal", "transcript_path": null, "cwd": fixture.outside,
            "hook_event_name": "PostToolUse", "model": "gpt-5.6-sol",
            "permission_mode": "default", "turn_id": turn,
            "tool_name": "shell", "tool_use_id": format!("call-{turn}"),
            "tool_input": {"command": ["cat", "src/lib.rs"], "workdir": fixture.outside},
            "tool_response": {"output": "Done!"}
        })
    };
    // The first event records the permanent Disabled lease, exactly as it did before.
    assert_eq!(fixture.hook("codex", &event("turn-1")), json!({}));
    let before = fixture.state_bytes();
    assert_eq!(fixture.hook("codex", &event("turn-2")), json!({}));
    assert_eq!(fixture.state_bytes(), before);
    assert!(!fixture.root.join("state/runtime.sqlite").exists());
    assert!(
        fixture
            .logging
            .diagnostics_or_empty()
            .recent_events
            .is_empty()
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
