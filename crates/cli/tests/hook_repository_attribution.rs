use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use fs2::FileExt;
use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_domain::{ExternalSessionLocator, TaskId, WorkingIntentSnapshot};
use sctx_git_store::GitStore;
use sctx_local_state::{
    AuthorizedSessionScopeRead, AuthorizedSessionScopeStore, CaptureStore, UserConfigStore,
    map_capture_artifacts,
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
    far_file: PathBuf,
    sibling_file: PathBuf,
    outside_file: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("hook attribution home");
        let group_root = temporary.path().join("android fels");
        let repository_a = group_root.join("app a");
        let repository_b = group_root.join("app b");
        let sibling = group_root.join("unregistered sibling");
        let other_repository = group_root.join("app c registered nonmember");
        let far_repository = temporary.path().join("far registered repo");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&home).unwrap();
        for repository in [
            &repository_a,
            &repository_b,
            &sibling,
            &other_repository,
            &far_repository,
        ] {
            initialize_repository(repository);
        }
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("outside.rs"), "outside canary\n").unwrap();
        let group_root = fs::canonicalize(group_root).unwrap();
        let repository_a = fs::canonicalize(repository_a).unwrap();
        let repository_b = fs::canonicalize(repository_b).unwrap();
        let sibling = fs::canonicalize(sibling).unwrap();
        let other_repository = fs::canonicalize(other_repository).unwrap();
        let far_repository = fs::canonicalize(far_repository).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let file_a = repository_a.join("src/file.rs");
        let file_b = repository_b.join("src/file.rs");
        let sibling_file = sibling.join("src/file.rs");
        let other_file = other_repository.join("src/file.rs");
        let far_file = far_repository.join("src/file.rs");
        let outside_file = outside.join("outside.rs");
        let root = home.join(".shared-context");
        GitStore::bootstrap_local(&root).unwrap();
        let config = UserConfigStore::open_existing(&root).unwrap();
        let first_id = config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&repository_a),
            )
            .unwrap()
            .repository
            .repository_id;
        let second_id = config
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
                std::slice::from_ref(&other_repository),
            )
            .unwrap();
        config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&far_repository),
            )
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
            far_file,
            sibling_file,
            outside_file,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn hook(&self, payload: &Value) -> Output {
        run_hook(&self.home, "codex", payload, None)
    }

    fn cursor_hook(&self, payload: &Value) -> Output {
        run_hook(&self.home, "cursor", payload, None)
    }

    fn hook_with_path(&self, payload: &Value, path: &Path) -> Output {
        run_hook(&self.home, "codex", payload, Some(path))
    }

    fn start(&self, session: &str, cwd: &Path) {
        let output = self.hook(&session_start(session, cwd));
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": SHARED_CONTEXT_ACTIVATION_MARKER
            }})
        );
    }

    fn start_cursor(&self, session: &str, cwd: &Path) {
        let output = self.cursor_hook(&cursor_session_start(session, cwd));
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"additional_context": SHARED_CONTEXT_ACTIVATION_MARKER})
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

fn run_hook(home: &Path, agent: &str, payload: &Value, path: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    command.args(["hook", "--agent", agent]).env("HOME", home);
    if agent == "codex" {
        command.args(["--agent-version", "0.147.0"]);
    }
    if let Some(path) = path {
        command.env("PATH", path);
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

fn cursor_session_start(session: &str, cwd: &Path) -> Value {
    json!({
        "conversation_id": session,
        "generation_id": format!("generation-{session}"),
        "model": "claude-opus-4-7",
        "hook_event_name": "sessionStart",
        "cursor_version": "3.13.10",
        "workspace_roots": [cwd],
        "user_email": null,
        "transcript_path": null,
        "session_id": session,
        "is_background_agent": false,
        "composer_mode": "agent"
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

#[allow(clippy::needless_pass_by_value)]
fn cursor_post_tool(session: &str, cwd: &Path, tool_input: Value, tool: &str, raw: &str) -> Value {
    json!({
        "conversation_id": session,
        "generation_id": format!("generation-{session}"),
        "model": "claude-opus-4-7",
        "hook_event_name": "postToolUse",
        "cursor_version": "3.13.10",
        "workspace_roots": [cwd],
        "user_email": null,
        "transcript_path": format!("/tmp/{raw}.jsonl"),
        "tool_name": tool,
        "tool_input": tool_input,
        "tool_output": raw,
        "tool_use_id": format!("tool-{raw}"),
        "cwd": cwd,
        "duration": 1
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
fn direct_scope_records_another_registered_repository_with_its_real_identity() {
    let fixture = Fixture::new();
    let session = "direct-cross-repository";
    fixture.start(session, &fixture.repository_a);
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let runtime = TaskRuntime::initialize(fixture.root()).unwrap();
    let task = runtime
        .open_or_create(locator, TaskId::new(), task_intent(), Vec::new())
        .unwrap()
        .snapshot;
    let catalog_before = fs::read(fixture.root().join("config.toml")).unwrap();

    assert_neutral(
        &fixture.hook(&post_tool(
            session,
            &fixture.outside,
            json!({"file_path": fixture.other_file, "command": "RAW_CROSS_REPOSITORY"}),
            "CrossRepositoryTest",
            "RAW_CROSS_REPOSITORY",
        )),
        &["RAW_CROSS_REPOSITORY"],
    );
    let captures = fixture.captures(session);
    assert_eq!(captures.len(), 1);
    let capture = &captures[0].record;
    let catalog = UserConfigStore::open_existing(fixture.root())
        .unwrap()
        .repository_catalog()
        .unwrap();
    let expected = catalog.resolve_declared_path(&fixture.other_file).unwrap();
    assert_eq!(
        capture.workspace_hint.as_deref(),
        expected.checkout_path.to_str()
    );
    assert_eq!(
        capture.file_hints,
        vec![fixture.other_file.to_str().unwrap()]
    );
    let mapped = map_capture_artifacts(capture, &catalog);
    assert_eq!(mapped.artifact_refs.len(), 1);
    assert!(mapped.diagnostics.is_empty());
    assert_eq!(
        mapped.artifact_refs[0].repository_id,
        expected.repository_id
    );
    let signals = runtime.read_signal_history(task.task_session_id).unwrap();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].signal.content, "CrossRepositoryTest succeeded");
    assert_eq!(
        fs::read(fixture.root().join("config.toml")).unwrap(),
        catalog_before,
        "Hook path must not mutate Repository Catalog"
    );
    let persisted = serde_json::to_string(&(capture, signals)).unwrap();
    assert!(!persisted.contains("RAW_CROSS_REPOSITORY"));
}

#[test]
fn group_scope_records_registered_nonmembers_and_preserves_multi_checkout_mapping() {
    let fixture = Fixture::new();
    let session = "group-attribution";
    fixture.start(session, &fixture.group_root);
    for (input, expected_count) in [
        (json!({"file_path": fixture.other_file}), 1),
        (
            json!({
                "file_path": fixture.file_a,
                "nested": {"filepath": fixture.other_file}
            }),
            2,
        ),
        (
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
                &fixture.group_root,
                input,
                "Inspect",
                "RAW_GROUP_ALLOWED",
            )),
            &["RAW_GROUP_ALLOWED"],
        );
        assert_eq!(fixture.capture_count(session), expected_count);
    }
    let captures = fixture.captures(session);
    let single_nonmember = captures
        .iter()
        .find(|capture| capture.record.file_hints == [fixture.other_file.to_str().unwrap()])
        .unwrap();
    assert_eq!(
        single_nonmember.record.workspace_hint.as_deref(),
        fixture
            .other_file
            .parent()
            .and_then(Path::parent)
            .and_then(Path::to_str)
    );
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
                .any(|path| path == fixture.other_file.to_str().unwrap())
            && capture.record.workspace_hint.as_deref() == fixture.group_root.to_str()
    }));
    let catalog = UserConfigStore::open_existing(fixture.root())
        .unwrap()
        .repository_catalog()
        .unwrap();
    let cross = captures
        .iter()
        .find(|capture| {
            capture
                .record
                .file_hints
                .iter()
                .any(|path| path == fixture.other_file.to_str().unwrap())
                && capture
                    .record
                    .file_hints
                    .iter()
                    .any(|path| path == fixture.file_a.to_str().unwrap())
        })
        .unwrap();
    let mapped = map_capture_artifacts(&cross.record, &catalog);
    assert_eq!(mapped.artifact_refs.len(), 2);
    assert_ne!(
        mapped.artifact_refs[0].repository_id,
        mapped.artifact_refs[1].repository_id
    );
    assert!(mapped.diagnostics.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn safe_unregistered_mixed_and_unrepresentable_multi_repo_events_are_non_locating() {
    let fixture = Fixture::new();
    let session = "non-locating";
    fixture.start(session, &fixture.repository_a);
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let runtime = TaskRuntime::initialize(fixture.root()).unwrap();
    let task = runtime
        .open_or_create(locator, TaskId::new(), task_intent(), Vec::new())
        .unwrap()
        .snapshot;
    let events = [
        (
            fixture.repository_a.as_path(),
            json!({"file_path": fixture.sibling_file, "command": "RAW_UNREGISTERED_FILE"}),
            "UnregisteredFileTest",
            "RAW_UNREGISTERED_FILE",
        ),
        (
            fixture.repository_a.as_path(),
            json!({"path": fixture.sibling}),
            "UnregisteredDirectoryTest",
            "RAW_UNREGISTERED_DIRECTORY",
        ),
        (
            fixture.outside.as_path(),
            json!({"command": "RAW_UNREGISTERED_CWD"}),
            "UnregisteredCwdTest",
            "RAW_UNREGISTERED_CWD",
        ),
        (
            fixture.repository_a.as_path(),
            json!({
                "file_path": fixture.file_a,
                "nested": {"path": fixture.outside_file}
            }),
            "MixedAttributionTest",
            "RAW_MIXED",
        ),
        (
            fixture.repository_a.as_path(),
            json!({
                "file_path": fixture.file_a,
                "nested": {"filepath": fixture.far_file}
            }),
            "MultiRepositoryTest",
            "RAW_MULTI_REPOSITORY",
        ),
    ];
    for (cwd, input, tool, raw) in events {
        assert_neutral(
            &fixture.hook(&post_tool(session, cwd, input, tool, raw)),
            &[raw],
        );
    }

    let captures = fixture.captures(session);
    assert_eq!(captures.len(), 5);
    assert!(captures.iter().all(|capture| {
        capture.record.workspace_hint.is_none() && capture.record.file_hints.is_empty()
    }));
    let signals = runtime.read_signal_history(task.task_session_id).unwrap();
    assert_eq!(signals.len(), 5);
    assert!(signals.iter().all(|signal| {
        signal.signal.content.ends_with(" succeeded")
            && !signal.signal.content.contains('/')
            && !signal.signal.content.contains("RAW_")
    }));
    let capture_records = captures
        .into_iter()
        .map(|capture| capture.record)
        .collect::<Vec<_>>();
    let persisted = serde_json::to_string(&(capture_records, signals)).unwrap();
    for forbidden in [
        fixture.sibling.to_str().unwrap(),
        fixture.sibling_file.to_str().unwrap(),
        fixture.outside.to_str().unwrap(),
        fixture.outside_file.to_str().unwrap(),
        fixture.file_a.to_str().unwrap(),
        fixture.far_file.to_str().unwrap(),
        "RAW_UNREGISTERED_FILE",
        "RAW_UNREGISTERED_DIRECTORY",
        "RAW_UNREGISTERED_CWD",
        "RAW_MIXED",
        "RAW_MULTI_REPOSITORY",
    ] {
        assert!(!persisted.contains(forbidden), "leaked {forbidden:?}");
    }
    assert!(
        GitStore::bootstrap_local(fixture.root())
            .unwrap()
            .list_pending()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unsafe_paths_remain_neutral_before_capture_and_signal_ingestion() {
    let fixture = Fixture::new();
    let session = "unsafe-paths";
    fixture.start(session, &fixture.repository_a);
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let runtime = TaskRuntime::initialize(fixture.root()).unwrap();
    let task = runtime
        .open_or_create(locator, TaskId::new(), task_intent(), Vec::new())
        .unwrap()
        .snapshot;
    let symlink_file = fixture.repository_a.join("src/symlink-file.rs");
    let symlink_directory = fixture.repository_a.join("src/symlink-directory");
    symlink(&fixture.sibling_file, &symlink_file).unwrap();
    symlink(&fixture.sibling, &symlink_directory).unwrap();
    let socket = fixture.outside.join("not-file-or-directory.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let rejected = [
        (json!({"file_path": "src/file.rs"}), "RAW_RELATIVE"),
        (
            json!({"file_path": fixture.repository_a.join("src/missing.rs")}),
            "RAW_MISSING",
        ),
        (json!({"file_path": symlink_file}), "RAW_SYMLINK_LEAF"),
        (
            json!({"file_path": symlink_directory.join("src/file.rs")}),
            "RAW_SYMLINK_ANCESTOR",
        ),
        (
            json!({"working_directory": symlink_directory}),
            "RAW_SYMLINK_DIRECTORY",
        ),
        (
            json!({"file_path": [fixture.file_a.clone()]}),
            "RAW_AMBIGUOUS",
        ),
        (
            json!({"file_path": fixture.repository_a}),
            "RAW_DIRECTORY_AS_FILE",
        ),
        (json!({"path": socket}), "RAW_SPECIAL_FILE"),
    ];
    for (input, raw) in rejected {
        assert_neutral(
            &fixture.hook(&post_tool(
                session,
                &fixture.repository_a,
                input,
                "RejectedTest",
                raw,
            )),
            &[raw],
        );
    }
    assert_eq!(fixture.capture_count(session), 0);
    assert!(
        runtime
            .read_signal_history(task.task_session_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn real_cursor_payload_uses_the_same_registered_and_non_locating_contract() {
    let fixture = Fixture::new();
    let session = "cursor-cross-repository";
    fixture.start_cursor(session, &fixture.repository_a);
    let locator = ExternalSessionLocator::new("cursor", session).unwrap();
    let runtime = TaskRuntime::initialize(fixture.root()).unwrap();
    let task = runtime
        .open_or_create(locator, TaskId::new(), task_intent(), Vec::new())
        .unwrap()
        .snapshot;
    assert_neutral(
        &fixture.cursor_hook(&cursor_post_tool(
            session,
            &fixture.repository_a,
            json!({"file_path": fixture.other_file, "command": "RAW_CURSOR_REGISTERED"}),
            "CursorRegisteredTest",
            "RAW_CURSOR_REGISTERED",
        )),
        &["RAW_CURSOR_REGISTERED"],
    );
    assert_neutral(
        &fixture.cursor_hook(&cursor_post_tool(
            session,
            &fixture.outside,
            json!({"path": fixture.outside_file, "command": "RAW_CURSOR_UNREGISTERED"}),
            "CursorUnregisteredTest",
            "RAW_CURSOR_UNREGISTERED",
        )),
        &["RAW_CURSOR_UNREGISTERED"],
    );
    let captures = fixture.captures(session);
    assert_eq!(captures.len(), 2);
    assert!(captures.iter().any(|capture| {
        capture.record.file_hints == [fixture.other_file.to_str().unwrap()]
            && capture.record.workspace_hint.is_some()
    }));
    assert!(captures.iter().any(|capture| {
        capture.record.workspace_hint.is_none() && capture.record.file_hints.is_empty()
    }));
    assert_eq!(
        runtime
            .read_signal_history(task.task_session_id)
            .unwrap()
            .len(),
        2
    );
    let capture_records = captures
        .into_iter()
        .map(|capture| capture.record)
        .collect::<Vec<_>>();
    let persisted = serde_json::to_string(&capture_records).unwrap();
    assert!(!persisted.contains("RAW_CURSOR_REGISTERED"));
    assert!(!persisted.contains("RAW_CURSOR_UNREGISTERED"));
    assert!(!persisted.contains(fixture.outside_file.to_str().unwrap()));
}

#[test]
fn hook_attribution_is_catalog_read_only_and_invokes_no_discovery_child() {
    let fixture = Fixture::new();
    let session = "no-discovery";
    fixture.start(session, &fixture.repository_a);
    let runtime = TaskRuntime::initialize(fixture.root()).unwrap();
    runtime
        .open_or_create(
            ExternalSessionLocator::new("codex", session).unwrap(),
            TaskId::new(),
            task_intent(),
            Vec::new(),
        )
        .unwrap();
    let fake_bin = fixture.home.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let canary = fixture.home.join("unexpected-discovery-child");
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\nexit 97\n",
            canary.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_git).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&fake_git, permissions).unwrap();
    let catalog_before = fs::read(fixture.root().join("config.toml")).unwrap();
    assert_neutral(
        &fixture.hook_with_path(
            &post_tool(
                session,
                &fixture.outside,
                json!({"file_path": fixture.other_file}),
                "Inspect",
                "RAW_NO_DISCOVERY",
            ),
            &fake_bin,
        ),
        &["RAW_NO_DISCOVERY"],
    );
    assert!(!canary.exists(), "Hook hot path invoked a discovery child");
    assert_eq!(
        fs::read(fixture.root().join("config.toml")).unwrap(),
        catalog_before
    );
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
