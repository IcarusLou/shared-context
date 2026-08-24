use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use fs2::FileExt;
use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_domain::{ExternalSessionLocator, TaskId, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::{
    AuthorizedSessionScopeRead, AuthorizedSessionScopeStore, CaptureStore, UserConfigStore,
};
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

struct Fixture {
    _temporary: TempDir,
    home: PathBuf,
    group_root: PathBuf,
    repository_a: PathBuf,
    repository_b: PathBuf,
    sibling: PathBuf,
    outside: PathBuf,
    file_a: PathBuf,
    file_b: PathBuf,
    other_file: PathBuf,
    sibling_file: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("hook attribution home");
        let group_root = temporary.path().join("android fels");
        let repository_a = group_root.join("app a");
        let repository_b = group_root.join("app b");
        let sibling = group_root.join("unregistered sibling");
        let other_repository = temporary.path().join("other registered repo");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&home).unwrap();
        for repository in [&repository_a, &repository_b, &sibling, &other_repository] {
            initialize_repository(repository);
        }
        fs::create_dir_all(&outside).unwrap();
        let group_root = fs::canonicalize(group_root).unwrap();
        let repository_a = fs::canonicalize(repository_a).unwrap();
        let repository_b = fs::canonicalize(repository_b).unwrap();
        let sibling = fs::canonicalize(sibling).unwrap();
        let other_repository = fs::canonicalize(other_repository).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let file_a = repository_a.join("src/file.rs");
        let file_b = repository_b.join("src/file.rs");
        let sibling_file = sibling.join("src/file.rs");
        let other_file = other_repository.join("src/file.rs");
        let root = home.join(".shared-context");
        GitStore::initialize(&root).unwrap();
        let config = UserConfigStore::open_existing(&root).unwrap();
        let first_id = config
            .add_repository(None, std::slice::from_ref(&repository_a))
            .unwrap()
            .repository
            .repository_id;
        let second_id = config
            .add_repository(None, std::slice::from_ref(&repository_b))
            .unwrap()
            .repository
            .repository_id;
        config
            .add_repository(None, std::slice::from_ref(&other_repository))
            .unwrap();
        config
            .add_repository_group(&group_root, &[first_id, second_id])
            .unwrap();
        Self {
            _temporary: temporary,
            home,
            group_root,
            repository_a,
            repository_b,
            sibling,
            outside,
            file_a,
            file_b,
            other_file,
            sibling_file,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn hook(&self, payload: &Value) -> Output {
        run_hook(&self.home, payload)
    }

    fn start(&self, session: &str, cwd: &Path) {
        let output = self.hook(&session_start(session, cwd));
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"systemMessage": SHARED_CONTEXT_ACTIVATION_MARKER})
        );
    }

    fn capture_count(&self, session: &str) -> usize {
        CaptureStore::initialize(self.root())
            .unwrap()
            .list(256)
            .unwrap()
            .captures
            .into_iter()
            .filter(|capture| {
                capture.record.external_session_locator.external_session_id == session
            })
            .count()
    }

    fn captures(&self, session: &str) -> Vec<sctx_local_state::CaptureRead> {
        CaptureStore::initialize(self.root())
            .unwrap()
            .list(256)
            .unwrap()
            .captures
            .into_iter()
            .filter(|capture| {
                capture.record.external_session_locator.external_session_id == session
            })
            .collect()
    }

    fn read_scope(&self, session: &str) -> AuthorizedSessionScopeRead {
        let catalog = UserConfigStore::open_existing(self.root())
            .unwrap()
            .repository_catalog()
            .unwrap();
        AuthorizedSessionScopeStore::initialize(self.root())
            .unwrap()
            .read(
                &ExternalSessionLocator::new("codex", session).unwrap(),
                &catalog,
            )
            .unwrap()
    }
}

fn initialize_repository(path: &Path) {
    fs::create_dir_all(path.join("src/workdir")).unwrap();
    fs::write(path.join("src/file.rs"), "pub fn fixture() {}\n").unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

fn run_hook(home: &Path, payload: &Value) -> Output {
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
    child.wait_with_output().unwrap()
}

fn session_start(session: &str, cwd: &Path) -> Value {
    json!({
        "session_id": session,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
    })
}

#[allow(clippy::needless_pass_by_value)]
fn post_tool(session: &str, cwd: &Path, tool_input: Value, tool: &str, raw: &str) -> Value {
    json!({
        "session_id": session,
        "transcript_path": format!("/tmp/{raw}.jsonl"),
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": format!("turn-{session}"),
        "tool_name": tool,
        "tool_use_id": format!("tool-{raw}"),
        "tool_input": tool_input,
        "tool_response": {"output": raw}
    })
}

fn session_end(session: &str, cwd: &Path) -> Value {
    json!({
        "session_id": session,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "SessionEnd",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "reason": "other"
    })
}

fn assert_neutral(output: &Output, forbidden: &[&str]) {
    assert!(
        output.status.success(),
        "Hook must fail open: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    let observable = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for value in forbidden {
        assert!(!observable.contains(value), "neutral Hook leaked {value:?}");
    }
}

fn task_intent() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: "Prove repository-scoped Hook attribution".to_owned(),
        current_direction: None,
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
#[allow(clippy::too_many_lines)]
fn direct_scope_accepts_only_complete_safe_attribution_before_capture_or_signals() {
    let fixture = Fixture::new();
    let session = "direct-attribution";
    fixture.start(session, &fixture.repository_a);
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let runtime = TaskRuntime::initialize(fixture.root()).unwrap();
    let task = runtime
        .open_or_create(locator, TaskId::new(), task_intent(), Vec::new())
        .unwrap()
        .snapshot;

    assert_neutral(
        &fixture.hook(&post_tool(
            session,
            &fixture.outside,
            json!({"file_path": fixture.file_a, "command": "RAW_ALLOWED_FILE"}),
            "Inspect",
            "RAW_ALLOWED_FILE",
        )),
        &["RAW_ALLOWED_FILE"],
    );
    assert_eq!(fixture.capture_count(session), 1);

    assert_neutral(
        &fixture.hook(&post_tool(
            session,
            &fixture.repository_a.join("src/workdir"),
            json!({"command": "RAW_ALLOWED_CWD"}),
            "Inspect",
            "RAW_ALLOWED_CWD",
        )),
        &["RAW_ALLOWED_CWD"],
    );
    assert_eq!(fixture.capture_count(session), 2);

    assert_neutral(
        &fixture.hook(&post_tool(
            session,
            &fixture.outside,
            json!({"working_directory": fixture.repository_a.join("src/workdir")}),
            "Inspect",
            "RAW_ALLOWED_WORKDIR",
        )),
        &["RAW_ALLOWED_WORKDIR"],
    );
    let captures = fixture.captures(session);
    assert_eq!(captures.len(), 3);
    assert!(
        captures
            .iter()
            .any(|capture| capture.record.file_hints.is_empty())
    );
    assert_neutral(
        &fixture.hook(&post_tool(
            session,
            &fixture.outside,
            json!({"path": fixture.repository_a.join("src/workdir")}),
            "Inspect",
            "RAW_ALLOWED_DIRECTORY_PATH",
        )),
        &["RAW_ALLOWED_DIRECTORY_PATH"],
    );
    assert_eq!(fixture.capture_count(session), 4);

    let missing = fixture.repository_a.join("src/missing.rs");
    let relative = PathBuf::from("src/file.rs");
    let symlink_file = fixture.repository_a.join("src/symlink-file.rs");
    let symlink_directory = fixture.repository_a.join("src/symlink-directory");
    symlink(&fixture.sibling_file, &symlink_file).unwrap();
    symlink(&fixture.sibling, &symlink_directory).unwrap();
    let rejected = [
        (
            json!({"file_path": fixture.other_file}),
            "RAW_OTHER_REGISTERED",
            "Inspect",
        ),
        (
            json!({"file_path": fixture.sibling_file}),
            "RAW_UNREGISTERED",
            "Inspect",
        ),
        (json!({"file_path": relative}), "RAW_RELATIVE", "Inspect"),
        (json!({"file_path": missing}), "RAW_MISSING", "Inspect"),
        (
            json!({"file_path": symlink_file}),
            "RAW_SYMLINK_FILE",
            "Inspect",
        ),
        (
            json!({"working_directory": symlink_directory}),
            "RAW_SYMLINK_DIRECTORY",
            "Inspect",
        ),
        (
            json!({"file_path": [fixture.file_a.clone()]}),
            "RAW_AMBIGUOUS",
            "Inspect",
        ),
        (
            json!({
                "file_path": fixture.file_a,
                "nested": {"path": fixture.sibling_file}
            }),
            "RAW_MIXED",
            "Inspect",
        ),
        (
            json!({"file_path": fixture.other_file}),
            "RAW_DROPPED_TEST",
            "ContractTest",
        ),
    ];
    for (input, raw, tool) in rejected {
        assert_neutral(
            &fixture.hook(&post_tool(session, &fixture.repository_a, input, tool, raw)),
            &[raw],
        );
        assert_eq!(fixture.capture_count(session), 4);
    }
    assert!(
        runtime
            .read_signal_history(task.task_session_id)
            .unwrap()
            .is_empty(),
        "a dropped test/check/lint tool must not create a TaskSignal"
    );
    let persisted = fixture
        .captures(session)
        .into_iter()
        .map(|capture| capture.record)
        .collect::<Vec<_>>();
    let persisted = serde_json::to_string(&persisted).unwrap();
    for raw in [
        "RAW_ALLOWED_FILE",
        "RAW_ALLOWED_CWD",
        "RAW_ALLOWED_WORKDIR",
        "RAW_ALLOWED_DIRECTORY_PATH",
        "RAW_OTHER_REGISTERED",
        "RAW_UNREGISTERED",
        "RAW_RELATIVE",
        "RAW_MISSING",
        "RAW_SYMLINK_FILE",
        "RAW_SYMLINK_DIRECTORY",
        "RAW_AMBIGUOUS",
        "RAW_MIXED",
        "RAW_DROPPED_TEST",
    ] {
        assert!(!persisted.contains(raw));
    }
    for path in [
        fixture.other_file.as_path(),
        fixture.sibling_file.as_path(),
        fixture.outside.as_path(),
    ] {
        assert!(!persisted.contains(path.to_str().unwrap()));
    }
}

#[test]
fn group_scope_accepts_member_files_and_cwd_but_rejects_parent_and_mixed_events() {
    let fixture = Fixture::new();
    let session = "group-attribution";
    fixture.start(session, &fixture.group_root);
    for (cwd, input, expected_count) in [
        (
            fixture.group_root.as_path(),
            json!({"file_path": fixture.file_a}),
            1,
        ),
        (fixture.repository_b.as_path(), json!({}), 2),
        (
            fixture.group_root.as_path(),
            json!({
                "file_path": fixture.file_a,
                "nested": {"filepath": fixture.file_b}
            }),
            3,
        ),
    ] {
        assert_neutral(
            &fixture.hook(&post_tool(
                session,
                cwd,
                input,
                "Inspect",
                "RAW_GROUP_ALLOWED",
            )),
            &["RAW_GROUP_ALLOWED"],
        );
        assert_eq!(fixture.capture_count(session), expected_count);
    }
    let captures = fixture.captures(session);
    assert!(captures.iter().any(|capture| {
        capture.record.file_hints.len() == 2
            && capture
                .record
                .file_hints
                .iter()
                .any(|path| path == fixture.file_a.to_str().unwrap())
            && capture
                .record
                .file_hints
                .iter()
                .any(|path| path == fixture.file_b.to_str().unwrap())
    }));

    for (cwd, input, raw) in [
        (fixture.group_root.as_path(), json!({}), "RAW_GROUP_ROOT"),
        (fixture.sibling.as_path(), json!({}), "RAW_GROUP_SIBLING"),
        (
            fixture.group_root.as_path(),
            json!({
                "file_path": fixture.file_a,
                "nested": {"path": fixture.sibling_file}
            }),
            "RAW_GROUP_MIXED",
        ),
        (
            fixture.group_root.as_path(),
            json!({"file_path": fixture.other_file}),
            "RAW_GROUP_NON_MEMBER",
        ),
    ] {
        assert_neutral(
            &fixture.hook(&post_tool(session, cwd, input, "Inspect", raw)),
            &[raw],
        );
        assert_eq!(fixture.capture_count(session), 3);
    }
    let persisted = serde_json::to_string(
        &fixture
            .captures(session)
            .into_iter()
            .map(|capture| capture.record)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    for path in [
        fixture.sibling.as_path(),
        fixture.sibling_file.as_path(),
        fixture.other_file.as_path(),
    ] {
        assert!(!persisted.contains(path.to_str().unwrap()));
    }
}

#[test]
fn session_end_removes_only_its_exact_lease_and_disabled_end_opens_no_business_state() {
    let fixture = Fixture::new();
    fixture.start("end-this", &fixture.repository_a);
    fixture.start("preserve-this", &fixture.repository_b);
    assert_neutral(
        &fixture.hook(&session_end("end-this", &fixture.outside)),
        &[fixture.outside.to_str().unwrap()],
    );
    assert!(matches!(
        fixture.read_scope("end-this"),
        AuthorizedSessionScopeRead::Missing
    ));
    assert!(matches!(
        fixture.read_scope("preserve-this"),
        AuthorizedSessionScopeRead::Current(_)
    ));

    let disabled = Fixture::new();
    let output = disabled.hook(&session_start("disabled-end", &disabled.outside));
    assert_neutral(&output, &[disabled.outside.to_str().unwrap()]);
    assert!(!disabled.root().join("state/runtime.sqlite").exists());
    assert!(!disabled.root().join("state/capture").exists());
    assert_neutral(
        &disabled.hook(&session_end("disabled-end", &disabled.repository_a)),
        &[disabled.repository_a.to_str().unwrap()],
    );
    assert!(matches!(
        disabled.read_scope("disabled-end"),
        AuthorizedSessionScopeRead::Missing
    ));
    assert!(!disabled.root().join("state/runtime.sqlite").exists());
    assert!(!disabled.root().join("state/capture").exists());
}

#[test]
fn busy_or_corrupt_scope_cleanup_is_neutral_and_fail_open_without_cross_locator_removal() {
    let busy = Fixture::new();
    busy.start("busy-end", &busy.repository_a);
    busy.start("busy-preserve", &busy.repository_b);
    let lock_path = busy.root().join("state/authorized-session-scopes.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert_neutral(
        &busy.hook(&session_end("busy-end", &busy.repository_a)),
        &[busy.repository_a.to_str().unwrap()],
    );
    FileExt::unlock(&lock).unwrap();
    assert!(matches!(
        busy.read_scope("busy-end"),
        AuthorizedSessionScopeRead::Current(_)
    ));
    assert!(matches!(
        busy.read_scope("busy-preserve"),
        AuthorizedSessionScopeRead::Current(_)
    ));

    let corrupt = Fixture::new();
    corrupt.start("corrupt-end", &corrupt.repository_a);
    let directory = AuthorizedSessionScopeStore::initialize(corrupt.root())
        .unwrap()
        .directory()
        .to_path_buf();
    let record = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    fs::write(&record, b"{invalid scope").unwrap();
    assert_neutral(
        &corrupt.hook(&session_end("corrupt-end", &corrupt.repository_a)),
        &[corrupt.repository_a.to_str().unwrap(), "invalid scope"],
    );
    assert!(record.exists());
    assert!(!corrupt.root().join("state/runtime.sqlite").exists());
    assert!(!corrupt.root().join("state/capture").exists());
}
