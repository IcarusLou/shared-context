use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;
use sctx_agent_adapter::{AgentKind, shared_context_activation_marker};
use sctx_domain::{ExternalSessionLocator, RepositoryId};
use sctx_git_store::GitStore;
use sctx_local_state::{
    AuthorizedSessionScopeDecision, AuthorizedSessionScopeRead, AuthorizedSessionScopeStore,
    MaintenanceLock, UserConfigStore,
};
use sctx_task_runtime::TaskRuntime;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

fn codex_marker(session: &str) -> String {
    shared_context_activation_marker(AgentKind::Codex, session)
}

fn cursor_marker(session: &str) -> String {
    shared_context_activation_marker(AgentKind::Cursor, session)
}

/// Asserts one Hook wire output activated exactly this Agent kind and host Session id.
fn assert_activated(output: &Output, agent: AgentKind, session: &str) {
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let actual = match agent {
        AgentKind::Codex => &response["hookSpecificOutput"]["additionalContext"],
        AgentKind::Cursor => &response["additional_context"],
    };
    assert_eq!(
        actual.as_str(),
        Some(shared_context_activation_marker(agent, session).as_str())
    );
}

struct Fixture {
    _temporary: TempDir,
    home: PathBuf,
    /// The directory both registered checkouts live under. Nobody registered it: a
    /// Session that starts here derives both Repositories.
    common_parent: PathBuf,
    direct_repository: PathBuf,
    second_repository: PathBuf,
    outside: PathBuf,
    direct_repository_id: RepositoryId,
    second_repository_id: RepositoryId,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("activation home");
        let common_parent = temporary.path().join("android fels");
        let direct_repository = common_parent.join("direct app");
        let second_repository = common_parent.join("second app");
        let outside = temporary.path().join("unregistered work");
        for path in [&home, &direct_repository, &second_repository, &outside] {
            fs::create_dir_all(path).unwrap();
        }
        for repository in [&direct_repository, &second_repository] {
            assert!(
                Command::new("git")
                    .args(["init", "-q", "-b", "main"])
                    .arg(repository)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let common_parent = fs::canonicalize(common_parent).unwrap();
        let direct_repository = fs::canonicalize(direct_repository).unwrap();
        let second_repository = fs::canonicalize(second_repository).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let root = home.join(".shared-context");
        GitStore::bootstrap_local(&root).unwrap();
        let config = UserConfigStore::open_existing(&root).unwrap();
        let direct_repository_id = config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&direct_repository),
            )
            .unwrap()
            .repository
            .repository_id;
        let second_repository_id = config
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&second_repository),
            )
            .unwrap()
            .repository
            .repository_id;
        Self {
            _temporary: temporary,
            home,
            common_parent,
            direct_repository,
            second_repository,
            outside,
            direct_repository_id,
            second_repository_id,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn hook(&self, agent: &str, payload: &Value) -> Output {
        run_hook(&self.home, agent, payload)
    }

    fn read_scope(&self, agent: &str, session: &str) -> AuthorizedSessionScopeRead {
        AuthorizedSessionScopeStore::initialize(self.root())
            .unwrap()
            .read(&ExternalSessionLocator::new(agent, session).unwrap())
            .unwrap()
    }
}

/// Exact on-disk lease path for one locator, found by removing and re-creating nothing:
/// the `SessionStart` that produced it is the only writer, so the newest entry is unique.
fn lease_record_path(root: &Path, agent: &str, session: &str) -> PathBuf {
    let store = AuthorizedSessionScopeStore::initialize(root).unwrap();
    let locator = ExternalSessionLocator::new(agent, session).unwrap();
    let expected = match store.read(&locator).unwrap() {
        AuthorizedSessionScopeRead::Current(scope) => scope,
        AuthorizedSessionScopeRead::Missing => panic!("expected a lease for {agent}/{session}"),
    };
    fs::read_dir(store.directory())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|value| {
                    value["external_session_locator"]["external_session_id"] == session
                        && value["external_session_locator"]["agent_kind"] == agent
                })
        })
        .filter(|_| expected.external_session_locator == locator)
        .expect("one lease entry")
}

fn run_hook(home: &Path, agent: &str, payload: &Value) -> Output {
    let version = (agent == "codex").then_some("0.147.0");
    run_hook_with_agent_version(home, agent, payload, version)
}

fn run_hook_with_agent_version(
    home: &Path,
    agent: &str,
    payload: &Value,
    agent_version: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    command.args(["hook", "--agent", agent]).env("HOME", home);
    if let Some(version) = agent_version {
        command.args(["--agent-version", version]);
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

fn codex_payload(index: usize, session: &str, cwd: &Path) -> Value {
    let mut payload: Value = serde_json::from_str::<Vec<Value>>(include_str!(
        "../../../fixtures/agents/codex-0.147.json"
    ))
    .unwrap()
    .remove(index);
    payload["session_id"] = Value::String(session.to_owned());
    payload["cwd"] = json!(cwd);
    payload
}

fn codex_start(session: &str, cwd: &Path, source: &str) -> Value {
    let mut payload = codex_payload(0, session, cwd);
    payload["source"] = Value::String(source.to_owned());
    payload
}

fn codex_prompt(session: &str, cwd: &Path) -> Value {
    codex_payload(1, session, cwd)
}

fn codex_post_tool(session: &str, cwd: &Path) -> Value {
    let mut payload = codex_payload(2, session, cwd);
    payload["transcript_path"] = Value::Null;
    payload["tool_input"] = json!({"file_path": cwd.join("bootstrap.rs")});
    payload["tool_response"] = json!({"output": "SANITIZED_BOOTSTRAP_TOOL_OUTPUT"});
    payload
}

fn codex_precompact(session: &str, cwd: &Path) -> Value {
    let mut payload = codex_payload(3, session, cwd);
    payload["transcript_path"] = Value::Null;
    payload
}

fn cursor_payload(index: usize, session: &str, cwd: &Path) -> Value {
    let mut payload = serde_json::from_str::<Vec<Value>>(include_str!(
        "../../../fixtures/agents/cursor-3.13.json"
    ))
    .unwrap()
    .remove(index);
    payload["conversation_id"] = Value::String(session.to_owned());
    payload["workspace_roots"] = json!([cwd]);
    payload["transcript_path"] = Value::Null;
    if payload.get("session_id").is_some() {
        payload["session_id"] = Value::String(session.to_owned());
    }
    if payload.get("cwd").is_some() {
        payload["cwd"] = json!(cwd);
    }
    payload
}

fn cursor_start(session: &str, cwd: &Path) -> Value {
    cursor_payload(0, session, cwd)
}

fn cursor_post_tool(session: &str, cwd: &Path) -> Value {
    let mut payload = cursor_payload(2, session, cwd);
    payload["tool_input"] = json!({
        "command": "SANITIZED_BOOTSTRAP_TOOL_COMMAND",
        "working_directory": cwd
    });
    payload["tool_output"] = Value::String("SANITIZED_BOOTSTRAP_TOOL_OUTPUT".to_owned());
    payload
}

fn cursor_stop(session: &str, cwd: &Path) -> Value {
    cursor_payload(4, session, cwd)
}

fn assert_neutral(output: &Output) {
    assert!(
        output.status.success(),
        "Hook must leave the Agent running: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    assert!(output.stderr.is_empty());
}

fn scope_records(root: &Path) -> Vec<PathBuf> {
    let directory = root.join("state/authorized-session-scopes");
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut records = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    records.sort();
    records
}

fn assert_no_runtime_or_capture(root: &Path) {
    assert!(!root.join("state/runtime.sqlite").exists());
    let capture = root.join("state/capture");
    assert!(!capture.exists() || fs::read_dir(capture).unwrap().next().is_none());
}

fn tree_contains(path: &Path, needle: &[u8]) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return false;
    }
    if metadata.is_file() {
        return fs::read(path)
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle));
    }
    metadata.is_dir()
        && fs::read_dir(path).is_ok_and(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .any(|entry| tree_contains(&entry.path(), needle))
        })
}

#[test]
fn exclusive_maintenance_keeps_session_start_neutral_without_business_residue() {
    let fixture = Fixture::new();
    let before_records = scope_records(&fixture.root());
    let maintenance = MaintenanceLock::open_or_create(fixture.root()).unwrap();
    let exclusive = maintenance.try_exclusive().unwrap();

    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("maintenance-busy", &fixture.direct_repository, "startup"),
    ));
    assert_eq!(scope_records(&fixture.root()), before_records);
    assert_no_runtime_or_capture(&fixture.root());

    drop(exclusive);
    let activated = fixture.hook(
        "codex",
        &codex_start(
            "maintenance-released",
            &fixture.direct_repository,
            "startup",
        ),
    );
    assert_activated(&activated, AgentKind::Codex, "maintenance-released");
}

#[test]
fn real_codex_and_cursor_session_start_wire_outputs_follow_durable_scope() {
    let fixture = Fixture::new();

    let direct = fixture.hook(
        "codex",
        &codex_start("codex-direct", &fixture.direct_repository, "startup"),
    );
    assert!(direct.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&direct.stdout).unwrap(),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": codex_marker("codex-direct")
        }})
    );
    assert!(matches!(
        fixture.read_scope("codex", "codex-direct"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.repository_ids()
                == std::slice::from_ref(&fixture.direct_repository_id)
    ));

    // Starting at the directory both checkouts share activates the Session for both,
    // with the same marker and no Group registered anywhere.
    let parent = fixture.hook(
        "cursor",
        &cursor_start("cursor-parent", &fixture.common_parent),
    );
    assert!(parent.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&parent.stdout).unwrap(),
        json!({"additional_context": cursor_marker("cursor-parent")})
    );
    let mut expected_members = vec![
        fixture.direct_repository_id.clone(),
        fixture.second_repository_id.clone(),
    ];
    expected_members.sort();
    assert!(matches!(
        fixture.read_scope("cursor", "cursor-parent"),
        AuthorizedSessionScopeRead::Current(ref scope)
            if scope.decision.repository_ids() == expected_members
    ));

    let disabled = fixture.hook(
        "codex",
        &codex_start("codex-disabled", &fixture.outside, "startup"),
    );
    assert_neutral(&disabled);
    assert!(matches!(
        fixture.read_scope("codex", "codex-disabled"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));
    assert_no_runtime_or_capture(&fixture.root());
}

#[test]
#[allow(clippy::too_many_lines)]
fn enabled_tool_work_gets_one_intent_bootstrap_reminder_without_prompt_or_task_creation() {
    const REMINDER: &str = "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.";
    const PROMPT_CANARY: &str = "RAW_INTENT_BOOTSTRAP_PROMPT_MUST_NOT_PERSIST";

    let fixture = Fixture::new();
    fs::write(
        fixture.direct_repository.join("bootstrap.rs"),
        "fn bootstrap_fixture() {}\n",
    )
    .unwrap();
    let codex_session = "intent-bootstrap-codex";
    let cursor_session = "intent-bootstrap-cursor";
    for (agent, session, start) in [
        (
            "codex",
            codex_session,
            codex_start(codex_session, &fixture.direct_repository, "startup"),
        ),
        (
            "cursor",
            cursor_session,
            cursor_start(cursor_session, &fixture.direct_repository),
        ),
    ] {
        let activated = fixture.hook(agent, &start);
        assert!(activated.status.success());
        assert!(String::from_utf8_lossy(&activated.stdout).contains("task_intent_update"));
        assert!(matches!(
            fixture.read_scope(agent, session),
            AuthorizedSessionScopeRead::Current(scope)
                if !scope.intent_bootstrap_notified
                    && scope.decision.is_enabled()
        ));
    }

    let mut prompt = codex_prompt(codex_session, &fixture.direct_repository);
    prompt["prompt"] = Value::String(PROMPT_CANARY.to_owned());
    assert_neutral(&fixture.hook("codex", &prompt));

    let first_codex = fixture.hook(
        "codex",
        &codex_post_tool(codex_session, &fixture.direct_repository),
    );
    // Codex receives the reminder on both the user-visible line and in model context:
    // a reminder the model cannot read cannot be acted on.
    assert_eq!(
        serde_json::from_slice::<Value>(&first_codex.stdout).unwrap(),
        json!({
            "systemMessage": REMINDER,
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": REMINDER
            }
        }),
        "scope={:#?} stderr={}",
        fixture.read_scope("codex", codex_session),
        String::from_utf8_lossy(&first_codex.stderr)
    );
    assert_neutral(&fixture.hook(
        "codex",
        &codex_post_tool(codex_session, &fixture.direct_repository),
    ));

    let first_cursor = fixture.hook(
        "cursor",
        &cursor_post_tool(cursor_session, &fixture.direct_repository),
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&first_cursor.stdout).unwrap(),
        json!({"additional_context": REMINDER})
    );
    assert_neutral(&fixture.hook(
        "cursor",
        &cursor_post_tool(cursor_session, &fixture.direct_repository),
    ));

    for (agent, session) in [("codex", codex_session), ("cursor", cursor_session)] {
        assert!(matches!(
            fixture.read_scope(agent, session),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.intent_bootstrap_notified
                    && scope.decision.is_enabled()
        ));
        assert!(
            TaskRuntime::initialize(fixture.root())
                .unwrap()
                .read_snapshot_by_locator(&ExternalSessionLocator::new(agent, session).unwrap())
                .unwrap()
                .is_none()
        );
    }

    let resumed = fixture.hook(
        "codex",
        &codex_start(codex_session, &fixture.direct_repository, "resume"),
    );
    assert_activated(&resumed, AgentKind::Codex, codex_session);
    assert_neutral(&fixture.hook(
        "codex",
        &codex_post_tool(codex_session, &fixture.direct_repository),
    ));

    let precompact = fixture.hook(
        "codex",
        &codex_precompact(codex_session, &fixture.direct_repository),
    );
    assert!(
        serde_json::from_slice::<Value>(&precompact.stdout).unwrap()["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("no ActiveTask exists"))
    );
    // A Cursor `stop` carries the same Episode boundary line a Codex `Stop` does; it is
    // the host's one user-visible text field, not a neutral no-op.
    let stopped = fixture.hook(
        "cursor",
        &cursor_stop(cursor_session, &fixture.direct_repository),
    );
    assert!(
        serde_json::from_slice::<Value>(&stopped.stdout).unwrap()["user_message"]
            .as_str()
            .is_some_and(|message| message.contains("no ActiveTask exists")),
        "{}",
        String::from_utf8_lossy(&stopped.stdout)
    );

    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!fixture.root().join("state").join(removed).exists());
    }
    assert!(!tree_contains(
        &fixture.root().join("state"),
        PROMPT_CANARY.as_bytes()
    ));

    let disabled = Fixture::new();
    let disabled_session = "intent-bootstrap-disabled";
    assert_neutral(&disabled.hook(
        "codex",
        &codex_start(disabled_session, &disabled.outside, "startup"),
    ));
    let mut disabled_prompt = codex_prompt(disabled_session, &disabled.direct_repository);
    disabled_prompt["prompt"] = Value::String(PROMPT_CANARY.to_owned());
    assert_neutral(&disabled.hook("codex", &disabled_prompt));
    assert_neutral(&disabled.hook(
        "codex",
        &codex_post_tool(disabled_session, &disabled.direct_repository),
    ));
    assert!(matches!(
        disabled.read_scope("codex", disabled_session),
        AuthorizedSessionScopeRead::Current(scope)
            if !scope.intent_bootstrap_notified
                && scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));
    assert_no_runtime_or_capture(&disabled.root());
    assert!(!tree_contains(
        &disabled.root().join("state"),
        PROMPT_CANARY.as_bytes()
    ));
}

#[test]
fn marker_reappears_only_at_explicit_session_start_resume_or_compact_boundaries() {
    let fixture = Fixture::new();
    let session = "codex-resume";
    for source in ["startup", "resume", "compact"] {
        let output = fixture.hook(
            "codex",
            &codex_start(session, &fixture.direct_repository, source),
        );
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": codex_marker(session)
            }})
        );
        assert!(matches!(
            fixture.read_scope("codex", session),
            AuthorizedSessionScopeRead::Current(_)
        ));

        let prompt = fixture.hook("codex", &codex_prompt(session, &fixture.outside));
        assert_neutral(&prompt);
    }
    assert_eq!(scope_records(&fixture.root()).len(), 1);
}

#[test]
fn repeated_session_start_keeps_the_first_disabled_direct_or_group_decision() {
    let fixture = Fixture::new();

    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("sticky-disabled", &fixture.outside, "startup"),
    ));
    for source in ["startup", "resume", "compact"] {
        assert_neutral(&fixture.hook(
            "codex",
            &codex_start("sticky-disabled", &fixture.direct_repository, source),
        ));
    }
    assert!(matches!(
        fixture.read_scope("codex", "sticky-disabled"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    let direct = fixture.hook(
        "codex",
        &codex_start("sticky-direct", &fixture.direct_repository, "startup"),
    );
    assert_activated(&direct, AgentKind::Codex, "sticky-direct");
    for (cwd, source) in [
        (&fixture.second_repository, "startup"),
        (&fixture.outside, "resume"),
        (&fixture.common_parent, "compact"),
    ] {
        let repeated = fixture.hook("codex", &codex_start("sticky-direct", cwd, source));
        assert_activated(&repeated, AgentKind::Codex, "sticky-direct");
    }
    assert!(matches!(
        fixture.read_scope("codex", "sticky-direct"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.repository_ids()
                == std::slice::from_ref(&fixture.direct_repository_id)
    ));

    let parent = fixture.hook(
        "cursor",
        &cursor_start("sticky-parent", &fixture.common_parent),
    );
    assert_activated(&parent, AgentKind::Cursor, "sticky-parent");
    let repeated = fixture.hook(
        "cursor",
        &cursor_start("sticky-parent", &fixture.direct_repository),
    );
    assert_activated(&repeated, AgentKind::Cursor, "sticky-parent");
    assert!(matches!(
        fixture.read_scope("cursor", "sticky-parent"),
        AuthorizedSessionScopeRead::Current(scope) if scope.decision.repository_ids().len() == 2
    ));
}

#[test]
fn a_permanent_lease_survives_age_and_unrelated_catalog_edits() {
    let fixture = Fixture::new();
    assert_neutral(&fixture.hook(
        "codex",
        &codex_prompt("missing", &fixture.direct_repository),
    ));

    // A lease issued long ago still authorizes: there is no TTL to run out mid-review.
    let aged = fixture.hook(
        "codex",
        &codex_start("aged", &fixture.direct_repository, "startup"),
    );
    assert_activated(&aged, AgentKind::Codex, "aged");
    let record = lease_record_path(&fixture.root(), "codex", "aged");
    let mut lease: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    lease["issued_at_unix_seconds"] = json!(1_000);
    fs::write(&record, serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
    assert_activated(
        &fixture.hook("codex", &codex_start("aged", &fixture.outside, "resume")),
        AgentKind::Codex,
        "aged",
    );

    // An unrelated registration no longer silently demotes a running Session.
    let third_repository = fixture.common_parent.join("third app");
    fs::create_dir_all(&third_repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&third_repository)
            .status()
            .unwrap()
            .success()
    );
    UserConfigStore::open_existing(fixture.root())
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&fs::canonicalize(third_repository).unwrap()),
        )
        .unwrap();
    assert_activated(
        &fixture.hook(
            "codex",
            &codex_start("aged", &fixture.common_parent, "compact"),
        ),
        AgentKind::Codex,
        "aged",
    );
    assert!(matches!(
        fixture.read_scope("codex", "aged"),
        AuthorizedSessionScopeRead::Current(ref scope)
            if scope.decision.repository_ids()
                == std::slice::from_ref(&fixture.direct_repository_id)
                && scope.issued_at_unix_seconds == 1_000
    ));

    // A superseded-schema lease reads as Missing, so only SessionStart revives it.
    fs::write(
        &record,
        json!({
            "version": "v1",
            "external_session_locator": {
                "agent_kind": "codex", "external_session_id": "aged"
            },
            "decision": {
                "kind": "direct",
                "repository_id": fixture.direct_repository_id.to_string()
            },
            "allowed_repository_ids": [fixture.direct_repository_id.to_string()],
            "catalog_revision": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "issued_at_unix_seconds": 1_000,
            "expires_at_unix_seconds": 8_200,
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        fixture.read_scope("codex", "aged"),
        AuthorizedSessionScopeRead::Missing
    );
    assert_neutral(&fixture.hook("codex", &codex_prompt("aged", &fixture.direct_repository)));
    assert_activated(
        &fixture.hook(
            "codex",
            &codex_start("aged", &fixture.direct_repository, "resume"),
        ),
        AgentKind::Codex,
        "aged",
    );

    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("disabled", &fixture.outside, "startup"),
    ));
    assert_neutral(&fixture.hook(
        "codex",
        &codex_prompt("disabled", &fixture.direct_repository),
    ));
    assert_no_runtime_or_capture(&fixture.root());
}

#[test]
fn catalog_busy_corrupt_and_drift_errors_are_neutral_and_agent_successful() {
    let busy = Fixture::new();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(busy.root().join("state/config.lock"))
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    let started = Instant::now();
    assert_neutral(&busy.hook(
        "codex",
        &codex_start("catalog-busy", &busy.direct_repository, "startup"),
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    FileExt::unlock(&lock).unwrap();
    assert_no_runtime_or_capture(&busy.root());

    let corrupt = Fixture::new();
    fs::write(
        corrupt.root().join("config.toml"),
        "RAW_CATALOG_PARSE_ERROR",
    )
    .unwrap();
    assert_neutral(&corrupt.hook(
        "cursor",
        &cursor_start("catalog-corrupt", &corrupt.direct_repository),
    ));
    assert_no_runtime_or_capture(&corrupt.root());

    let drift = Fixture::new();
    let moved = drift.common_parent.join("moved direct app");
    fs::rename(&drift.direct_repository, &moved).unwrap();
    assert_neutral(&drift.hook(
        "codex",
        &codex_start("catalog-drift", &drift.outside, "startup"),
    ));
    assert_no_runtime_or_capture(&drift.root());
}

#[test]
fn busy_corrupt_and_symlink_lease_state_never_emit_activation() {
    let busy = Fixture::new();
    assert_neutral(&busy.hook(
        "codex",
        &codex_start("seed-disabled", &busy.outside, "startup"),
    ));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(busy.root().join("state/authorized-session-scopes.lock"))
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    let started = Instant::now();
    assert_neutral(&busy.hook(
        "codex",
        &codex_start("lease-busy", &busy.direct_repository, "startup"),
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    FileExt::unlock(&lock).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(matches!(
        busy.read_scope("codex", "lease-busy"),
        AuthorizedSessionScopeRead::Missing
    ));
    let retry = busy.hook(
        "codex",
        &codex_start("lease-busy", &busy.direct_repository, "resume"),
    );
    assert_activated(&retry, AgentKind::Codex, "lease-busy");
    assert_no_runtime_or_capture(&busy.root());

    let corrupt = Fixture::new();
    let output = corrupt.hook(
        "codex",
        &codex_start("lease-corrupt", &corrupt.direct_repository, "startup"),
    );
    assert_activated(&output, AgentKind::Codex, "lease-corrupt");
    let record = scope_records(&corrupt.root()).pop().unwrap();
    fs::write(&record, "RAW_SCOPE_PARSE_ERROR").unwrap();
    // A record that cannot be interpreted never authorizes as it stands. The next event
    // re-authorizes in place from its own cwd instead of leaving the Session silently
    // dead for the rest of its life, and the event itself stays wire-neutral.
    assert_neutral(&corrupt.hook(
        "codex",
        &codex_prompt("lease-corrupt", &corrupt.direct_repository),
    ));
    assert!(matches!(
        corrupt.read_scope("codex", "lease-corrupt"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.is_enabled()
    ));
    // An explicit SessionStart boundary overwrites the same one record.
    assert_activated(
        &corrupt.hook(
            "codex",
            &codex_start("lease-corrupt", &corrupt.direct_repository, "resume"),
        ),
        AgentKind::Codex,
        "lease-corrupt",
    );
    assert_eq!(scope_records(&corrupt.root()).len(), 1);
    assert!(matches!(
        corrupt.read_scope("codex", "lease-corrupt"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.is_enabled()
    ));
    assert_no_runtime_or_capture(&corrupt.root());

    let unsafe_state = Fixture::new();
    let output = unsafe_state.hook(
        "cursor",
        &cursor_start("lease-symlink", &unsafe_state.direct_repository),
    );
    assert_activated(&output, AgentKind::Cursor, "lease-symlink");
    let record = scope_records(&unsafe_state.root()).pop().unwrap();
    let target = unsafe_state.home.join("outside-scope-record.json");
    fs::write(&target, "{}\n").unwrap();
    fs::remove_file(&record).unwrap();
    symlink(&target, &record).unwrap();
    assert_neutral(&unsafe_state.hook(
        "cursor",
        &cursor_start("lease-symlink", &unsafe_state.direct_repository),
    ));
    assert_no_runtime_or_capture(&unsafe_state.root());
}

#[test]
fn concurrent_and_repeated_session_start_reuses_one_locator_record() {
    let fixture = Fixture::new();
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let barrier = Arc::clone(&barrier);
        let home = fixture.home.clone();
        let payload = codex_start("same-session", &fixture.direct_repository, "startup");
        handles.push(thread::spawn(move || {
            barrier.wait();
            run_hook(&home, "codex", &payload)
        }));
    }
    let mut activated = 0;
    for handle in handles {
        let output = handle.join().unwrap();
        assert!(output.status.success());
        let response = serde_json::from_slice::<Value>(&output.stdout).unwrap();
        if response
            == json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": codex_marker("same-session")
            }})
        {
            activated += 1;
        } else {
            assert_eq!(response, json!({}));
        }
    }
    assert!(activated >= 1);
    assert_eq!(scope_records(&fixture.root()).len(), 1);
    let first = match fixture.read_scope("codex", "same-session") {
        AuthorizedSessionScopeRead::Current(scope) => scope,
        other @ AuthorizedSessionScopeRead::Missing => {
            panic!("expected current scope, got {other:?}")
        }
    };
    let repeated = fixture.hook(
        "codex",
        &codex_start("same-session", &fixture.direct_repository, "resume"),
    );
    assert_activated(&repeated, AgentKind::Codex, "same-session");
    let retained = match fixture.read_scope("codex", "same-session") {
        AuthorizedSessionScopeRead::Current(scope) => scope,
        other @ AuthorizedSessionScopeRead::Missing => {
            panic!("expected refreshed scope, got {other:?}")
        }
    };
    assert_eq!(retained, first);
    assert_eq!(scope_records(&fixture.root()).len(), 1);
}

#[test]
fn locator_decisions_are_isolated_and_production_has_no_unscoped_planner() {
    let fixture = Fixture::new();
    let enabled = fixture.hook(
        "codex",
        &codex_start("locator-a", &fixture.direct_repository, "startup"),
    );
    assert_activated(&enabled, AgentKind::Codex, "locator-a");
    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("locator-b", &fixture.outside, "startup"),
    ));
    assert!(matches!(
        fixture.read_scope("codex", "locator-a"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.is_enabled()
    ));
    assert!(matches!(
        fixture.read_scope("codex", "locator-b"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    let main = include_str!("../src/main.rs");
    let adapter = include_str!("../../agent-adapter/src/lib.rs");
    assert!(!main.contains("plan_action(&"));
    assert!(!main.contains("HOOK_ACTIVATION_TIMEOUT"));
    assert!(!main.contains("mpsc::"));
    assert!(!adapter.contains("pub fn plan_action("));
}

/// Host version strings never gate activation.
///
/// `cursor-agent --version` reports a date-like build id (`2026.08.25-3e8eec8`) that is not
/// semver, and the same string arrives in the `cursor_version` payload field. When that string
/// was parsed and compared against a minimum, every Cursor CLI Session fell back to MCP + CLI and
/// `SessionStart` never emitted the activation marker, so the Skill gate could not open. A Direct
/// Session must activate for any host version string the strict decoder accepts.
#[test]
fn any_cursor_or_codex_host_version_string_activates_a_direct_session() {
    let fixture = Fixture::new();

    // The decoder still rejects an absent or empty `cursor_version`; every non-empty shape must
    // activate, whatever its syntax.
    for (index, version) in ["2026.08.25-3e8eec8", "3.12.99", "nightly"]
        .into_iter()
        .enumerate()
    {
        let session = format!("cursor-version-{index}");
        let mut payload = cursor_start(&session, &fixture.direct_repository);
        payload["cursor_version"] = Value::String(version.to_owned());
        let output = fixture.hook("cursor", &payload);
        assert!(
            output.stderr.is_empty(),
            "version {version:?} must not emit a capability diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_activated(&output, AgentKind::Cursor, &session);
        assert!(matches!(
            fixture.read_scope("cursor", &session),
            AuthorizedSessionScopeRead::Current(scope)
                if scope.decision.is_enabled()
        ));
    }

    for (index, version) in ["2026.08.25-3e8eec8", "0.146.0", "codex-cli 0.149.1"]
        .into_iter()
        .enumerate()
    {
        let session = format!("codex-version-{index}");
        let output = run_hook_with_agent_version(
            &fixture.home,
            "codex",
            &codex_start(&session, &fixture.direct_repository, "startup"),
            Some(version),
        );
        assert!(
            output.stderr.is_empty(),
            "version {version:?} must not emit a capability diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_activated(&output, AgentKind::Codex, &session);
    }
}

#[test]
fn an_unusual_host_version_outside_a_registered_repository_stays_neutral() {
    let fixture = Fixture::new();
    let mut disabled = cursor_start("old-disabled", &fixture.outside);
    disabled["cursor_version"] = Value::String("2026.08.25-3e8eec8".to_owned());
    assert_neutral(&fixture.hook("cursor", &disabled));
}

/// One authorized MCP call over the real stdio server, used to prove a repaired lease is
/// the same authorization a `SessionStart` would have produced.
fn mcp_context_search(home: &Path, session: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "codex"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"}
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "context_search",
                "arguments": {
                    "agent_kind": "codex",
                    "external_session_id": session,
                    "query": "bounded"
                }
            }
        })
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "MCP server failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|response| response["id"] == json!(2))
        .expect("one tools/call response")
}

/// A host that installs or starts the Hook after the Session began never delivers a
/// `SessionStart`. Before this, that Session stayed unauthorized for its whole life: no
/// marker, and every MCP call refused. Any event now creates the lease it is missing,
/// with the same decision `SessionStart` would have recorded.
#[test]
fn a_session_that_never_sent_session_start_is_authorized_by_its_next_event() {
    let fixture = Fixture::new();
    fs::write(
        fixture.direct_repository.join("bootstrap.rs"),
        "fn bootstrap_fixture() {}\n",
    )
    .unwrap();
    let session = "missed-session-start";
    assert!(matches!(
        fixture.read_scope("codex", session),
        AuthorizedSessionScopeRead::Missing
    ));

    let first = fixture.hook(
        "codex",
        &codex_post_tool(session, &fixture.direct_repository),
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(matches!(
        fixture.read_scope("codex", session),
        AuthorizedSessionScopeRead::Current(scope) if scope.decision.is_enabled()
    ));

    let response = mcp_context_search(&fixture.home, session);
    assert_eq!(
        response["result"]["isError"],
        json!(false),
        "a repaired lease must authorize MCP: {response:#}"
    );

    // Repair is not activation: a Session that started outside every registered
    // Repository still records Disabled and stays wire-neutral.
    let outside_session = "missed-session-start-outside";
    assert_neutral(&fixture.hook("codex", &codex_post_tool(outside_session, &fixture.outside)));
    assert!(matches!(
        fixture.read_scope("codex", outside_session),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));
    let refused = mcp_context_search(&fixture.home, outside_session);
    assert_eq!(refused["result"]["isError"], json!(true), "{refused:#}");
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["code"],
        json!("activation_disabled")
    );
}
