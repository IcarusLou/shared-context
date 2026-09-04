//! The two things a `SessionStart` owes periodic maintenance: one line about what is waiting for a
//! human, and -- when nobody has run maintenance for a day -- one detached process that runs it.
//!
//! Both are covered from the outside, through the real binary, because both are defined by what
//! the Agent and the filesystem observe rather than by any value a function returns. The
//! first-come rule the hint follows is a unit test in `main.rs`; what is here is the three states
//! that rule resolves to on a real installation, and the three ages the opportunistic gate reads.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker};
use sctx_git_store::GitStore;
use sctx_local_state::UserConfigStore;
use serde_json::{Value, json};

const SESSION: &str = "cursor-maintenance-schedule";

fn session_start(session: &str, workspace: &Path) -> Value {
    json!({
        "conversation_id": session,
        "generation_id": "generation-maintenance",
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

fn run_hook(home: &Path, payload: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["hook", "--agent", "cursor", "--agent-version", "3.13.10"])
        .env("HOME", home)
        .env("SCTX_SKIP_LAUNCHCTL", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.as_mut().unwrap(), payload).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

fn additional_context(output: &Output) -> String {
    assert!(
        output.status.success(),
        "hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    response["additional_context"].as_str().unwrap().to_owned()
}

/// Writes a maintenance digest by hand. The digest is a plain local record with an explicitly
/// forward-compatible shape, so writing one is the supported way to put an installation into the
/// state a real run would have left it in -- without paying for the Candidate lifecycle that
/// produced the count.
fn write_digest(root: &Path, pending_candidate_reviews: u64) {
    fs::write(
        root.join("state/maintain-digest.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "mode": "scheduled",
            "counts": {"pending_candidate_reviews": pending_candidate_reviews},
        }))
        .unwrap(),
    )
    .unwrap();
}

fn bootstrapped_installation() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("维护调度 home");
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
    UserConfigStore::open_existing(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&workspace),
        )
        .unwrap();
    (temporary, home, root, workspace)
}

#[test]
fn session_start_appends_the_maintenance_hint_only_while_reviews_are_pending() {
    let (_temporary, home, root, workspace) = bootstrapped_installation();
    let marker = shared_context_activation_marker(AgentKind::Cursor, SESSION);
    let payload = session_start(SESSION, &workspace);

    // No digest at all: the installation has never run maintenance, and the marker is untouched.
    assert_eq!(additional_context(&run_hook(&home, &payload)), marker);

    // A digest that counted nothing pending is the same silence, not a "0 pending" line.
    write_digest(&root, 0);
    assert_eq!(additional_context(&run_hook(&home, &payload)), marker);

    // Reviews waiting: one line behind the marker, and the marker's bytes are still exactly the
    // marker's bytes.
    write_digest(&root, 4);
    let context = additional_context(&run_hook(&home, &payload));
    let (kept, hint) = context.split_at(marker.len());
    assert_eq!(kept, marker);
    assert_eq!(
        hint,
        "\n<shared-context-maintenance>Shared Context maintenance: 4 pending Candidate Reviews \
         await a decision; call candidate_list after your first checkpoint.\
         </shared-context-maintenance>"
    );

    // The hint is retrieval steering, not a fact: nothing about any Candidate travels in it.
    assert!(!hint.contains("cnd_") && !hint.contains("ctx_"));

    // A digest this version cannot parse is local-state noise, never a degraded Session.
    fs::write(root.join("state/maintain-digest.json"), b"{not json").unwrap();
    assert_eq!(additional_context(&run_hook(&home, &payload)), marker);

    // And no other event grows a hint: the count is a starting decision, not a per-turn nag.
    write_digest(&root, 4);
    let post_tool = run_hook(
        &home,
        &json!({
            "conversation_id": SESSION,
            "generation_id": "generation-maintenance-2",
            "model": "claude-opus-4-7",
            "hook_event_name": "postToolUse",
            "cursor_version": "3.13.10",
            "workspace_roots": [&workspace],
            "user_email": null,
            "transcript_path": null,
            "tool_name": "Read",
            "tool_input": {"absolute_file_path": workspace.join("nothing.rs")},
            "tool_output": "",
            "tool_use_id": "tool-maintenance-1",
            "cwd": &workspace,
            "duration": 1
        }),
    );
    assert!(post_tool.status.success());
    let response: Value = serde_json::from_slice(&post_tool.stdout).unwrap();
    assert!(
        response
            .get("additional_context")
            .and_then(Value::as_str)
            .is_none_or(|context| !context.contains("shared-context-maintenance")),
        "{response}"
    );
}

/// A `SessionStart` on an installation whose maintenance has gone stale starts one detached run.
///
/// This one needs a real `sctx setup`, because the process the Hook spawns is
/// `<root>/bin/current/sctx` -- the installed runtime, not this test's binary path.
#[test]
fn session_start_starts_opportunistic_maintenance_only_once_it_has_gone_stale() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("机会维护 home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".shared-context");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_sctx"));
    let setup = Command::new(&binary)
        .args(["--json", "setup"])
        .env("HOME", &home)
        .env("SCTX_SKIP_LAUNCHCTL", "1")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
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
    UserConfigStore::open_existing(&root)
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&workspace),
        )
        .unwrap();
    let payload = session_start("cursor-opportunistic", &workspace);
    let last_run = root.join("state/maintain-last-run");
    let digest = root.join("state/maintain-digest.json");

    // Never run here: the gate opens, and the run leaves both records behind.
    assert!(!last_run.exists());
    assert!(run_hook(&home, &payload).status.success());
    assert!(
        wait_for(&digest, Duration::from_secs(30)),
        "an installation that never ran maintenance must start one"
    );
    let first = fs::read_to_string(&last_run).unwrap();
    // The scheduled track is not what ran: the Hook asks for the opportunistic budget by name.
    let recorded: Value = serde_json::from_slice(&fs::read(&digest).unwrap()).unwrap();
    assert_eq!(recorded["mode"], "opportunistic");

    // Freshly run: no second process, so the marker file keeps the second it already held.
    assert!(run_hook(&home, &payload).status.success());
    thread::sleep(Duration::from_secs(2));
    assert_eq!(fs::read_to_string(&last_run).unwrap(), first);

    // Stale: a marker older than the configured window opens the gate again.
    let stale = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 48 * 60 * 60;
    fs::write(&last_run, format!("{stale}\n")).unwrap();
    assert!(run_hook(&home, &payload).status.success());
    assert!(
        wait_for_change(&last_run, &format!("{stale}\n"), Duration::from_secs(30)),
        "a day-old installation must start a maintenance run"
    );

    // `opportunistic_after_hours = 0` turns the whole track off, which is the one setting that
    // makes a `SessionStart` byte-identical to the one that shipped before this track existed.
    let config = root.join("config.toml");
    let mut document = fs::read_to_string(&config).unwrap();
    document.push_str("\n[maintenance]\nopportunistic_after_hours = 0\n");
    fs::write(&config, document).unwrap();
    fs::write(&last_run, format!("{stale}\n")).unwrap();
    assert!(run_hook(&home, &payload).status.success());
    thread::sleep(Duration::from_secs(2));
    assert_eq!(fs::read_to_string(&last_run).unwrap(), format!("{stale}\n"));
}

fn wait_for(path: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_change(path: &Path, previous: &str, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if fs::read_to_string(path).is_ok_and(|current| current != previous) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}
