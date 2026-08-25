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
use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_domain::{ExternalSessionLocator, RepositoryId};
use sctx_git_store::GitStore;
use sctx_local_state::{
    AuthorizedSessionScopeDecision, AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeStore, UserConfigStore,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

struct Fixture {
    _temporary: TempDir,
    home: PathBuf,
    group_root: PathBuf,
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
        let group_root = temporary.path().join("android fels");
        let direct_repository = group_root.join("direct app");
        let second_repository = group_root.join("second app");
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
        let group_root = fs::canonicalize(group_root).unwrap();
        let direct_repository = fs::canonicalize(direct_repository).unwrap();
        let second_repository = fs::canonicalize(second_repository).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let root = home.join(".shared-context");
        GitStore::initialize(&root).unwrap();
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
        config
            .add_repository_group(
                &group_root,
                &[direct_repository_id.clone(), second_repository_id.clone()],
            )
            .unwrap();
        Self {
            _temporary: temporary,
            home,
            group_root,
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

fn cursor_start(session: &str, cwd: &Path) -> Value {
    let mut payload: Value = serde_json::from_str::<Vec<Value>>(include_str!(
        "../../../fixtures/agents/cursor-3.13.json"
    ))
    .unwrap()
    .remove(0);
    payload["conversation_id"] = Value::String(session.to_owned());
    payload["session_id"] = Value::String(session.to_owned());
    payload["workspace_roots"] = json!([cwd]);
    payload
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
    let mut records = fs::read_dir(directory)
        .unwrap()
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
        json!({"systemMessage": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
    assert!(matches!(
        fixture.read_scope("codex", "codex-direct"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Direct {
                repository_id: fixture.direct_repository_id.clone()
            }
    ));

    let group = fixture.hook("cursor", &cursor_start("cursor-group", &fixture.group_root));
    assert!(group.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&group.stdout).unwrap(),
        json!({"additional_context": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
    let mut expected_members = vec![
        fixture.direct_repository_id.clone(),
        fixture.second_repository_id.clone(),
    ];
    expected_members.sort();
    assert!(matches!(
        fixture.read_scope("cursor", "cursor-group"),
        AuthorizedSessionScopeRead::Current(scope)
            if matches!(scope.decision, AuthorizedSessionScopeDecision::Group { .. })
                && scope.allowed_repository_ids == expected_members
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
            json!({"systemMessage": SHARED_CONTEXT_ACTIVATION_MARKER})
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
    assert!(String::from_utf8_lossy(&direct.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    for (cwd, source) in [
        (&fixture.second_repository, "startup"),
        (&fixture.outside, "resume"),
        (&fixture.group_root, "compact"),
    ] {
        let repeated = fixture.hook("codex", &codex_start("sticky-direct", cwd, source));
        assert!(
            String::from_utf8_lossy(&repeated.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER)
        );
    }
    assert!(matches!(
        fixture.read_scope("codex", "sticky-direct"),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Direct {
                repository_id: fixture.direct_repository_id.clone()
            }
                && scope.allowed_repository_ids == [fixture.direct_repository_id.clone()]
    ));

    let group = fixture.hook("cursor", &cursor_start("sticky-group", &fixture.group_root));
    assert!(String::from_utf8_lossy(&group.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    let repeated = fixture.hook(
        "cursor",
        &cursor_start("sticky-group", &fixture.direct_repository),
    );
    assert!(String::from_utf8_lossy(&repeated.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    assert!(matches!(
        fixture.read_scope("cursor", "sticky-group"),
        AuthorizedSessionScopeRead::Current(scope)
            if matches!(scope.decision, AuthorizedSessionScopeDecision::Group { .. })
    ));
}

#[test]
fn non_session_start_missing_expired_stale_and_disabled_leases_are_neutral() {
    let fixture = Fixture::new();
    assert_neutral(&fixture.hook(
        "codex",
        &codex_prompt("missing", &fixture.direct_repository),
    ));

    let catalog = UserConfigStore::open_existing(fixture.root())
        .unwrap()
        .repository_catalog()
        .unwrap();
    let scope = catalog
        .resolve_activation_scope(&fixture.direct_repository)
        .unwrap();
    let expiring = AuthorizedSessionScopeStore::with_policy(
        fixture.root(),
        AuthorizedSessionScopePolicy {
            ttl: Duration::from_secs(1),
            ..AuthorizedSessionScopePolicy::default()
        },
    )
    .unwrap();
    let expired_locator = ExternalSessionLocator::new("codex", "expired").unwrap();
    expiring
        .authorize(&expired_locator, &scope, &catalog)
        .unwrap();
    thread::sleep(Duration::from_millis(1_100));
    let unrelated = fixture.hook(
        "codex",
        &codex_start(
            "unrelated-after-expiry",
            &fixture.direct_repository,
            "startup",
        ),
    );
    assert!(String::from_utf8_lossy(&unrelated.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("expired", &fixture.second_repository, "resume"),
    ));
    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("expired", &fixture.group_root, "compact"),
    ));
    assert_eq!(
        expiring.try_read(&expired_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Expired
    );
    assert!(expiring.expire(&expired_locator).unwrap());
    assert_eq!(
        expiring.try_read(&expired_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Missing
    );

    let stale_start = fixture.hook(
        "codex",
        &codex_start("stale", &fixture.direct_repository, "startup"),
    );
    assert!(
        String::from_utf8_lossy(&stale_start.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER)
    );
    let third_repository = fixture.group_root.join("third app");
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
    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("stale", &fixture.second_repository, "resume"),
    ));
    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("stale", &fixture.group_root, "compact"),
    ));
    assert!(matches!(
        fixture.read_scope("codex", "stale"),
        AuthorizedSessionScopeRead::StaleCatalog
    ));

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
    let moved = drift.group_root.join("moved direct app");
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
    assert!(String::from_utf8_lossy(&retry.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    assert_no_runtime_or_capture(&busy.root());

    let corrupt = Fixture::new();
    let output = corrupt.hook(
        "codex",
        &codex_start("lease-corrupt", &corrupt.direct_repository, "startup"),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    let record = scope_records(&corrupt.root()).pop().unwrap();
    fs::write(record, "RAW_SCOPE_PARSE_ERROR").unwrap();
    assert_neutral(&corrupt.hook(
        "codex",
        &codex_start("lease-corrupt", &corrupt.direct_repository, "resume"),
    ));
    assert_no_runtime_or_capture(&corrupt.root());

    let unsafe_state = Fixture::new();
    let output = unsafe_state.hook(
        "cursor",
        &cursor_start("lease-symlink", &unsafe_state.direct_repository),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
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
        if response == json!({"systemMessage": SHARED_CONTEXT_ACTIVATION_MARKER}) {
            activated += 1;
        } else {
            assert_eq!(response, json!({}));
        }
    }
    assert!(activated >= 1);
    assert_eq!(scope_records(&fixture.root()).len(), 1);
    let first = match fixture.read_scope("codex", "same-session") {
        AuthorizedSessionScopeRead::Current(scope) => scope,
        other => panic!("expected current scope, got {other:?}"),
    };
    let repeated = fixture.hook(
        "codex",
        &codex_start("same-session", &fixture.direct_repository, "resume"),
    );
    assert!(String::from_utf8_lossy(&repeated.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    let retained = match fixture.read_scope("codex", "same-session") {
        AuthorizedSessionScopeRead::Current(scope) => scope,
        other => panic!("expected refreshed scope, got {other:?}"),
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
    assert!(String::from_utf8_lossy(&enabled.stdout).contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    assert_neutral(&fixture.hook(
        "codex",
        &codex_start("locator-b", &fixture.outside, "startup"),
    ));
    assert!(matches!(
        fixture.read_scope("codex", "locator-a"),
        AuthorizedSessionScopeRead::Current(scope)
            if matches!(scope.decision, AuthorizedSessionScopeDecision::Direct { .. })
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

#[test]
fn unverified_authorized_hook_retains_degradation_but_disabled_is_neutral() {
    let fixture = Fixture::new();
    let mut authorized = cursor_start("old-authorized", &fixture.direct_repository);
    authorized["cursor_version"] = Value::String("3.12.99".to_owned());
    let output = fixture.hook("cursor", &authorized);
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostic = response["additional_context"].as_str().unwrap();
    assert!(diagnostic.contains("MCP + CLI fallback"));
    assert!(!diagnostic.contains(SHARED_CONTEXT_ACTIVATION_MARKER));
    assert!(String::from_utf8_lossy(&output.stderr).contains("MCP + CLI fallback"));
    assert!(matches!(
        fixture.read_scope("cursor", "old-authorized"),
        AuthorizedSessionScopeRead::Current(scope)
            if matches!(scope.decision, AuthorizedSessionScopeDecision::Direct { .. })
    ));

    let mut disabled = cursor_start("old-disabled", &fixture.outside);
    disabled["cursor_version"] = Value::String("3.12.99".to_owned());
    assert_neutral(&fixture.hook("cursor", &disabled));
}
