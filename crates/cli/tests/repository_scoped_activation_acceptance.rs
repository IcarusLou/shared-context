use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{ExternalSessionLocator, RepositoryId};
use sctx_git_store::GitStore;
use sctx_local_state::{
    AuthorizedSessionScope, AuthorizedSessionScopeDecision, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeStore, UserConfigStore,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const ORACLE_BYTES: &[u8] =
    include_bytes!("../../../fixtures/m2/repository-scoped-activation-v1.json");
const CODEX_FIXTURE: &str = include_str!("../../../fixtures/agents/codex-0.147.json");
const CURSOR_FIXTURE: &str = include_str!("../../../fixtures/agents/cursor-3.13.json");

#[derive(Debug, Deserialize)]
struct Oracle {
    schema: String,
    activation_marker: String,
    activation_marker_max_bytes: usize,
    wire: WireOracle,
    enabled_without_active_task: LifecycleOracle,
    disabled_business_residue: ResidueOracle,
}

#[derive(Debug, Deserialize)]
struct WireOracle {
    codex_enabled_session_start: Value,
    cursor_enabled_session_start: Value,
    neutral: Value,
}

#[derive(Debug, Deserialize)]
struct LifecycleOracle {
    pre_compact: String,
    turn_stop: String,
}

#[derive(Debug, Deserialize)]
struct ResidueOracle {
    runtime_files: usize,
    capture_records: usize,
    report_files: usize,
    knowledge_commits_delta: usize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct GitSnapshot {
    head: String,
    status: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct BusinessSnapshot {
    files: Vec<(PathBuf, Vec<u8>)>,
    git: GitSnapshot,
}

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
    sibling_file: PathBuf,
    repository_a_id: RepositoryId,
    repository_b_id: RepositoryId,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("acceptance home");
        let group_root = temporary.path().join("registered parent");
        let repository_a = group_root.join("member a");
        let repository_b = group_root.join("member b");
        let sibling = group_root.join("unregistered sibling");
        let outside = temporary.path().join("outside catalog");
        fs::create_dir_all(&home).unwrap();
        for path in [&repository_a, &repository_b, &sibling] {
            initialize_repository(path);
        }
        fs::create_dir_all(&outside).unwrap();
        let group_root = fs::canonicalize(group_root).unwrap();
        let repository_a = fs::canonicalize(repository_a).unwrap();
        let repository_b = fs::canonicalize(repository_b).unwrap();
        let sibling = fs::canonicalize(sibling).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let file_a = repository_a.join("src/fixture.rs");
        let file_b = repository_b.join("src/fixture.rs");
        let sibling_file = sibling.join("src/fixture.rs");

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
            sibling_file,
            repository_a_id: first_id,
            repository_b_id: second_id,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn repository(&self) -> PathBuf {
        self.root().join("repository")
    }

    fn run(&self, agent: &str, payload: &Value) -> Output {
        run_hook(&self.home, agent, payload)
    }

    fn read_scope(&self, agent: &str, session: &str) -> AuthorizedSessionScopeRead {
        let catalog = UserConfigStore::open_existing(self.root())
            .unwrap()
            .repository_catalog()
            .unwrap();
        AuthorizedSessionScopeStore::initialize(self.root())
            .unwrap()
            .read(
                &ExternalSessionLocator::new(agent, session).unwrap(),
                &catalog,
            )
            .unwrap()
    }

    fn current_scope(&self, agent: &str, session: &str) -> AuthorizedSessionScope {
        match self.read_scope(agent, session) {
            AuthorizedSessionScopeRead::Current(scope) => scope,
            other => panic!("expected Current scope, got {other:?}"),
        }
    }

    fn business_snapshot(&self) -> BusinessSnapshot {
        BusinessSnapshot {
            files: business_files(&self.root()),
            git: git_snapshot(&self.repository()),
        }
    }
}

fn oracle() -> Oracle {
    serde_json::from_slice(ORACLE_BYTES).unwrap()
}

fn initialize_repository(path: &Path) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("src/fixture.rs"), "pub fn fixture() {}\n").unwrap();
    let output = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
}

fn documented_events(agent: &str, session: &str, cwd: &Path) -> Vec<Value> {
    let fixture = if agent == "codex" {
        CODEX_FIXTURE
    } else {
        CURSOR_FIXTURE
    };
    let mut events = serde_json::from_str::<Vec<Value>>(fixture).unwrap();
    assert_eq!(events.len(), 6);
    for event in &mut events {
        event["transcript_path"] = Value::Null;
        if agent == "codex" {
            event["session_id"] = Value::String(session.to_owned());
            event["cwd"] = json!(cwd);
        } else {
            event["conversation_id"] = Value::String(session.to_owned());
            event["workspace_roots"] = json!([cwd]);
            if event.get("session_id").is_some() {
                event["session_id"] = Value::String(session.to_owned());
            }
            if event.get("cwd").is_some() {
                event["cwd"] = json!(cwd);
            }
        }
    }
    events[1]["prompt"] = Value::String("SYNTHETIC_ACCEPTANCE_PROMPT".to_owned());
    if agent == "cursor" {
        events[1]["attachments"] = json!([]);
        events[2]["tool_output"] = Value::String("SYNTHETIC_TOOL_OUTPUT".to_owned());
    } else {
        events[2]["tool_response"] = json!({"output": "SYNTHETIC_TOOL_OUTPUT"});
        events[4]["last_assistant_message"] = Value::Null;
    }
    events
}

fn set_event_cwd(agent: &str, event: &mut Value, cwd: &Path) {
    if agent == "codex" {
        event["cwd"] = json!(cwd);
    } else {
        event["workspace_roots"] = json!([cwd]);
        if event.get("cwd").is_some() {
            event["cwd"] = json!(cwd);
        }
    }
}

fn run_hook(home: &Path, agent: &str, payload: &Value) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    command.args(["hook", "--agent", agent]).env("HOME", home);
    if agent == "codex" {
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
        "Hook must fail open: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "verified Hook emitted stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_output(output: &Output, expected: &Value) {
    assert_eq!(&output_json(output), expected);
}

fn assert_codex_lifecycle_message(output: &Output, expected: &str) {
    let expected = json!({"systemMessage": expected});
    assert_output(output, &expected);
}

fn git_output(repository: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_snapshot(repository: &Path) -> GitSnapshot {
    GitSnapshot {
        head: git_output(repository, &["rev-parse", "HEAD"]),
        status: git_output(repository, &["status", "--short"]),
    }
}

fn business_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let state = root.join("state");
    let mut files = Vec::new();
    collect_business_files(&state, &state, &mut files);
    for reports in [root.join("report"), root.join("reports")] {
        collect_all_files(&reports, root, &mut files);
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
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
        let is_business_root =
            name.starts_with("runtime.sqlite") || matches!(name, "capture" | "report" | "reports");
        if path.is_dir() {
            if is_business_root {
                collect_all_files(&path, relative_to, files);
            }
        } else if is_business_root {
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

fn capture_record_count(root: &Path) -> usize {
    fs::read_dir(root.join("state/capture"))
        .ok()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.as_ref().ok().is_some_and(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
        })
        .count()
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

fn scope_record_count(root: &Path) -> usize {
    fs::read_dir(root.join("state/authorized-session-scopes"))
        .ok()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.as_ref().ok().is_some_and(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
        })
        .count()
}

fn assert_disabled_residue(
    fixture: &Fixture,
    baseline: &BusinessSnapshot,
    expected: &ResidueOracle,
) {
    let current = fixture.business_snapshot();
    assert_eq!(&current, baseline);
    assert_eq!(runtime_file_count(&fixture.root()), expected.runtime_files);
    assert_eq!(
        capture_record_count(&fixture.root()),
        expected.capture_records
    );
    assert_eq!(report_file_count(&fixture.root()), expected.report_files);
    let commits_before = git_output(
        &fixture.repository(),
        &["rev-list", "--count", &baseline.git.head],
    )
    .parse::<usize>()
    .unwrap();
    let commits_after = git_output(&fixture.repository(), &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .unwrap();
    assert_eq!(
        commits_after - commits_before,
        expected.knowledge_commits_delta
    );
}

#[test]
fn fixed_oracle_is_hand_written_bounded_and_privacy_safe() {
    let oracle = oracle();
    assert_eq!(
        oracle.schema,
        "sctx.repository-scoped-activation.acceptance.v1"
    );
    assert!(oracle.activation_marker.len() <= oracle.activation_marker_max_bytes);
    assert_eq!(oracle.activation_marker_max_bytes, 128);
    let raw = String::from_utf8(ORACLE_BYTES.to_vec()).unwrap();
    for forbidden in [
        "/Users/",
        "/home/",
        "bytedance",
        "transcript_path",
        "session_id",
        "conversation_id",
        "prompt",
        "tool_output",
    ] {
        assert!(!raw.contains(forbidden), "oracle leaked {forbidden:?}");
    }
    assert_eq!(
        oracle.wire.codex_enabled_session_start,
        json!({"systemMessage": oracle.activation_marker})
    );
    assert_eq!(oracle.wire.neutral, json!({}));
}

#[test]
fn documented_codex_direct_lifecycle_activates_before_prompt_and_keeps_git_clean() {
    let fixture = Fixture::new();
    let oracle = oracle();
    let session = "synthetic-codex-direct";
    let mut events = documented_events("codex", session, &fixture.repository_a);
    events[2]["tool_input"] = json!({"file_path": fixture.file_a});
    let git_before = git_snapshot(&fixture.repository());

    assert_output(
        &fixture.run("codex", &events[0]),
        &oracle.wire.codex_enabled_session_start,
    );
    assert!(matches!(
        fixture.read_scope("codex", session),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Direct {
                repository_id: fixture.repository_a_id
            }
    ));
    assert_output(&fixture.run("codex", &events[1]), &oracle.wire.neutral);
    assert_output(&fixture.run("codex", &events[2]), &oracle.wire.neutral);
    assert_codex_lifecycle_message(
        &fixture.run("codex", &events[3]),
        &oracle.enabled_without_active_task.pre_compact,
    );
    assert_codex_lifecycle_message(
        &fixture.run("codex", &events[4]),
        &oracle.enabled_without_active_task.turn_stop,
    );
    assert_output(&fixture.run("codex", &events[5]), &oracle.wire.neutral);

    assert!(matches!(
        fixture.read_scope("codex", session),
        AuthorizedSessionScopeRead::Missing
    ));
    assert_eq!(capture_record_count(&fixture.root()), 3);
    assert!(runtime_file_count(&fixture.root()) >= 1);
    assert_eq!(report_file_count(&fixture.root()), 0);
    assert_eq!(git_snapshot(&fixture.repository()), git_before);
}

#[test]
fn documented_cursor_group_lifecycle_accepts_members_and_drops_sibling_or_mixed_events() {
    let fixture = Fixture::new();
    let oracle = oracle();
    let session = "synthetic-cursor-group";
    let mut events = documented_events("cursor", session, &fixture.group_root);
    events[2]["tool_input"] = json!({
        "file_path": fixture.file_a,
        "nested": {"filepath": fixture.file_b}
    });
    let git_before = git_snapshot(&fixture.repository());

    assert_output(
        &fixture.run("cursor", &events[0]),
        &oracle.wire.cursor_enabled_session_start,
    );
    let scope = fixture.current_scope("cursor", session);
    let mut expected_members = vec![fixture.repository_a_id, fixture.repository_b_id];
    expected_members.sort();
    assert!(matches!(
        scope.decision,
        AuthorizedSessionScopeDecision::Group { .. }
    ));
    assert_eq!(scope.allowed_repository_ids, expected_members);
    assert_output(&fixture.run("cursor", &events[1]), &oracle.wire.neutral);
    assert_output(&fixture.run("cursor", &events[2]), &oracle.wire.neutral);
    assert_eq!(capture_record_count(&fixture.root()), 1);

    let before_rejected = fixture.business_snapshot();
    let mut sibling = events[2].clone();
    set_event_cwd("cursor", &mut sibling, &fixture.sibling);
    sibling["tool_input"] = json!({});
    assert_output(&fixture.run("cursor", &sibling), &oracle.wire.neutral);
    assert_eq!(fixture.business_snapshot(), before_rejected);

    let mut mixed = events[2].clone();
    mixed["tool_input"] = json!({
        "file_path": fixture.file_a,
        "nested": {"path": fixture.sibling_file}
    });
    assert_output(&fixture.run("cursor", &mixed), &oracle.wire.neutral);
    assert_eq!(fixture.business_snapshot(), before_rejected);

    assert_output(
        &fixture.run("cursor", &events[3]),
        &json!({"user_message": oracle.enabled_without_active_task.pre_compact}),
    );
    assert_output(&fixture.run("cursor", &events[4]), &oracle.wire.neutral);
    assert_output(&fixture.run("cursor", &events[5]), &oracle.wire.neutral);
    assert!(matches!(
        fixture.read_scope("cursor", session),
        AuthorizedSessionScopeRead::Missing
    ));
    assert_eq!(capture_record_count(&fixture.root()), 3);
    assert_eq!(report_file_count(&fixture.root()), 0);
    assert_eq!(git_snapshot(&fixture.repository()), git_before);
}

#[test]
fn documented_disabled_lifecycles_are_wire_neutral_and_leave_zero_business_residue() {
    let fixture = Fixture::new();
    let oracle = oracle();
    let baseline = fixture.business_snapshot();

    for (agent, session) in [
        ("codex", "synthetic-disabled-codex"),
        ("cursor", "synthetic-disabled-cursor"),
    ] {
        let mut events = documented_events(agent, session, &fixture.sibling);
        events[2]["tool_input"] = json!({"file_path": fixture.sibling_file});
        for event in &events {
            assert_output(&fixture.run(agent, event), &oracle.wire.neutral);
        }
        assert!(matches!(
            fixture.read_scope(agent, session),
            AuthorizedSessionScopeRead::Missing
        ));
        assert_eq!(scope_record_count(&fixture.root()), 0);
        assert_disabled_residue(&fixture, &baseline, &oracle.disabled_business_residue);
    }
}

#[test]
fn unavailable_catalog_keeps_both_documented_lifecycles_disabled_and_residue_free() {
    let fixture = Fixture::new();
    let oracle = oracle();
    fs::write(fixture.root().join("config.toml"), "INVALID_CATALOG").unwrap();
    let baseline = fixture.business_snapshot();

    for (agent, session) in [
        ("codex", "synthetic-no-catalog-codex"),
        ("cursor", "synthetic-no-catalog-cursor"),
    ] {
        let mut events = documented_events(agent, session, &fixture.repository_a);
        events[2]["tool_input"] = json!({"file_path": fixture.file_a});
        for event in &events {
            assert_output(&fixture.run(agent, event), &oracle.wire.neutral);
        }
    }
    assert_eq!(scope_record_count(&fixture.root()), 0);
    assert_disabled_residue(&fixture, &baseline, &oracle.disabled_business_residue);
}

#[test]
fn resume_compact_and_concurrent_repeated_starts_keep_the_first_successful_scope() {
    let fixture = Fixture::new();
    let oracle = oracle();
    let session = "synthetic-sticky-session";
    let mut events = documented_events("codex", session, &fixture.repository_a);
    assert_output(
        &fixture.run("codex", &events[0]),
        &oracle.wire.codex_enabled_session_start,
    );
    let first = fixture.current_scope("codex", session);
    assert!(matches!(
        first.decision,
        AuthorizedSessionScopeDecision::Direct { repository_id }
            if repository_id == fixture.repository_a_id
    ));

    for (source, cwd) in [
        ("resume", fixture.repository_b.as_path()),
        ("compact", fixture.outside.as_path()),
    ] {
        set_event_cwd("codex", &mut events[0], cwd);
        events[0]["source"] = Value::String(source.to_owned());
        assert_output(
            &fixture.run("codex", &events[0]),
            &oracle.wire.codex_enabled_session_start,
        );
        set_event_cwd("codex", &mut events[1], cwd);
        assert_output(&fixture.run("codex", &events[1]), &oracle.wire.neutral);
        assert_eq!(fixture.current_scope("codex", session), first);
    }

    let workers = 12;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();
    for index in 0..workers {
        let barrier = Arc::clone(&barrier);
        let home = fixture.home.clone();
        let cwd = match index % 3 {
            0 => fixture.repository_b.clone(),
            1 => fixture.group_root.clone(),
            _ => fixture.outside.clone(),
        };
        let mut payload = documented_events("codex", session, &cwd).remove(0);
        payload["source"] = Value::String(["startup", "resume", "compact"][index % 3].to_owned());
        handles.push(thread::spawn(move || {
            barrier.wait();
            run_hook(&home, "codex", &payload)
        }));
    }
    for handle in handles {
        assert_output(
            &handle.join().unwrap(),
            &oracle.wire.codex_enabled_session_start,
        );
    }
    assert_eq!(fixture.current_scope("codex", session), first);
    assert_eq!(scope_record_count(&fixture.root()), 1);

    set_event_cwd("codex", &mut events[5], &fixture.outside);
    assert_output(&fixture.run("codex", &events[5]), &oracle.wire.neutral);
    assert!(matches!(
        fixture.read_scope("codex", session),
        AuthorizedSessionScopeRead::Missing
    ));
}
