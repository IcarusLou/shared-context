//! `sctx doctor --hooks` reports the collector's independent 24h decision/reason view.

mod logging_harness;

use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use sctx_git_store::GitStore;
use serde_json::{Value, json};

use logging_harness::LoggingHarness;

fn run_hook_raw(home: &Path, logging: &LoggingHarness, stdin: &[u8]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    logging.apply(&mut command);
    let mut child = command
        .args(["hook", "--agent", "cursor"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

fn run_hook(home: &Path, logging: &LoggingHarness, payload: &Value) -> std::process::Output {
    run_hook_raw(home, logging, &serde_json::to_vec(payload).unwrap())
}

fn cursor_session_start(cwd: &Path, session_id: &str) -> Value {
    json!({
        "conversation_id": session_id,
        "generation_id": format!("generation-{session_id}"),
        "model": "claude-opus-4-7",
        "hook_event_name": "sessionStart",
        "cursor_version": "3.13.10",
        "workspace_roots": [cwd],
        "user_email": null,
        "transcript_path": null,
        "session_id": session_id,
        "is_background_agent": false,
        "composer_mode": "agent"
    })
}

fn run_json_cli(home: &Path, logging: &LoggingHarness, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    logging.apply(&mut command);
    let output = command
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sctx {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn doctor_hooks_reports_counts_and_recent_rows_for_this_installations_hook_activity() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("doctor hooks home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".shared-context");
    GitStore::bootstrap_local(&root).unwrap();
    let logging = LoggingHarness::start(&home);

    // A Disabled SessionStart is the product's normal, by-design silent path — it must leave
    // zero local residue, so the Hook path never records it. To see an `ok` row this uses an
    // Enabled SessionStart instead: a workspace registered as a Repository.
    let workspace = home.join("registered workspace");
    fs::create_dir_all(&workspace).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    run_json_cli(
        &home,
        &logging,
        &[
            "repository",
            "add",
            "--repository-id",
            "DoctorHooksRepo",
            "--path",
            workspace.to_str().unwrap(),
        ],
    );
    let enabled = run_hook(
        &home,
        &logging,
        &cursor_session_start(&workspace, "doctor-hooks-enabled"),
    );
    assert!(enabled.status.success());

    // An undecodable payload: the Hook fails open before an Agent Session even exists and
    // records one `payload_decode_failed` / `fail_open` row.
    let undecodable = run_hook_raw(&home, &logging, b"NOT_JSON_AT_ALL");
    assert!(undecodable.status.success());

    let _ = logging.diagnostics();
    let report = run_json_cli(&home, &logging, &["doctor", "--hooks"]);
    assert_eq!(report["window_hours"], json!(24));

    let counts = report["counts"].as_array().unwrap();
    let find_count = |decision: &str, reason: &str| {
        counts
            .iter()
            .find(|entry| entry["decision"] == json!(decision) && entry["reason"] == json!(reason))
    };
    assert!(
        find_count("enabled", "ok").is_some(),
        "expected an enabled/ok count entry, got {counts:#?}"
    );
    assert!(
        find_count("fail_open", "payload_decode_failed").is_some(),
        "expected a fail_open/payload_decode_failed count entry, got {counts:#?}"
    );

    let recent = report["recent_events"].as_array().unwrap();
    assert!(
        recent.len() >= 2,
        "expected at least 2 recent events, got {recent:#?}"
    );
    for event in recent {
        assert!(event["recorded_at_unix_ms"].as_u64().is_some());
        assert!(event["agent_kind"].as_str().is_some());
        assert!(event["event_kind"].as_str().is_some());
        assert!(event["decision"].as_str().is_some());
        assert!(event["reason"].as_str().is_some());
        assert!(event["duration_ms"].as_u64().is_some());
    }

    // The lease survey ran and found the Enabled Session's own lease, so the active lease count
    // is reported rather than skipped.
    assert!(
        report["active_leases"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
}

#[test]
fn doctor_hooks_rejects_combination_with_fix_or_recheck() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("doctor hooks conflict home");
    fs::create_dir_all(&home).unwrap();
    GitStore::bootstrap_local(home.join(".shared-context")).unwrap();
    let logging = LoggingHarness::start(&home);

    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    logging.apply(&mut command);
    let output = command
        .args(["doctor", "--hooks", "--fix"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!output.status.success());
}
