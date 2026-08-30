//! P4.1 experiment acceptance: `[hooks] artifact_focus_reminder`.
//!
//! One real `sctx hook` process per assertion. The switch is off by default, so
//! the disabled `PostToolUse` output bytes are the contract; enabling it may only
//! add one bounded Artifact focus reminder, exactly once per Session per
//! Artifact, and must stay neutral whenever the Graph has nothing to offer.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr as _,
    time::{Duration, Instant},
};

use sctx_agent_adapter::{
    ARTIFACT_FOCUS_REMINDER_MAX_BYTES, AgentKind, shared_context_activation_marker,
};
use sctx_domain::{
    Applicability, ContextKind, ContextRevisionDraft, EvidenceSnapshotDraft, SpaceId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const SESSION: &str = "artifact-focus-reminder-session";
const EVIDENCE: &str = r#"{"kind":"experiment_record","supports":"the exact file was inspected","content":{"result":"passed"},"interpretation":"the rule is verified","limitations":["synthetic fixture"]}"#;
/// One-shot Intent bootstrap notice the enabled Hook already emits for the first
/// `PostToolUse` of a Session without an `ActiveTask`. It is unrelated to P4.1 and
/// must stay byte-identical with the switch off.
const INTENT_BOOTSTRAP: &str = "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.";
const STATEMENT: &str =
    "The default comment bottom bar must stay reachable when the vertical-domain service is absent";

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
    repository: PathBuf,
}

impl Harness {
    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn source_file(&self) -> PathBuf {
        self.repository.join("src/contract.rs")
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("sctx should start");
        assert!(
            output.status.success(),
            "sctx {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn hook(&self, payload: &Value) -> Value {
        let (value, _) = self.hook_timed(payload);
        value
    }

    fn hook_timed(&self, payload: &Value) -> (Value, Duration) {
        let started = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["hook", "--agent", "codex", "--agent-version", "0.147.0"])
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
        let elapsed = started.elapsed();
        assert!(
            output.status.success(),
            "hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (serde_json::from_slice(&output.stdout).unwrap(), elapsed)
    }

    fn enable_artifact_focus_reminder(&self) {
        let path = self.root().join("config.toml");
        let mut text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[hooks]"));
        text.push_str("\n[hooks]\nartifact_focus_reminder = true\n");
        fs::write(&path, text).unwrap();
    }
}

fn git(repository: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn session_start_payload(cwd: &Path) -> Value {
    json!({
        "session_id": SESSION,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
    })
}

fn read_payload(cwd: &Path, file: &Path, tool_use_id: &str) -> Value {
    json!({
        "session_id": SESSION,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": "turn-artifact-focus",
        "tool_name": "Read",
        "tool_use_id": tool_use_id,
        "tool_input": {"file_path": file},
        "tool_response": {"output": "fn contract() {}"}
    })
}

/// Builds an installation whose Engineering Graph associates one accepted
/// Context with `src/contract.rs`, plus one activated Direct Session.
#[allow(clippy::too_many_lines)]
fn seeded_harness() -> Harness {
    let temporary = tempdir().unwrap();
    let home = temporary.path().join("用户 home 空格");
    fs::create_dir_all(&home).unwrap();
    let repository = home.join("工程 repo");
    fs::create_dir_all(repository.join("src")).unwrap();
    fs::write(repository.join("src/contract.rs"), "pub fn contract() {}\n").unwrap();
    git(&home, &["init", "-q", "-b", "main", "工程 repo"]);
    git(&repository, &["config", "user.name", "Artifact Focus"]);
    git(
        &repository,
        &["config", "user.email", "focus@example.invalid"],
    );
    git(&repository, &["add", "--", "."]);
    git(&repository, &["commit", "-q", "-m", "fixture"]);
    let repository = fs::canonicalize(&repository).unwrap();
    let harness = Harness {
        _temporary: temporary,
        home,
        repository,
    };
    GitStore::bootstrap_local(harness.root()).unwrap();

    let space = harness.success(&[
        "space",
        "create",
        "--title",
        "Artifact focus reminder",
        "--problem",
        "cold start",
        "--desired-outcome",
        "shared knowledge",
        "--in-scope",
        "hook",
        "--acceptance-condition",
        "works",
    ]);
    let space_id = space["data"]["space_id"].as_str().unwrap().to_owned();

    let event = Event::context_revision_added(
        SpaceId::from_str(&space_id).unwrap(),
        ContextRevisionDraft {
            problem_view: None,
            hints: Vec::new(),
            kind: ContextKind::Decision,
            topic_key: Some("hook/artifact-focus".to_owned()),
            statement: STATEMENT.to_owned(),
            rationale: "the reminder fixture needs one accepted Context".to_owned(),
            applicability: Applicability {
                domains: vec!["hook".to_owned()],
                ..Applicability::default()
            },
            assumptions: Vec::new(),
            recheck_when: Vec::new(),
            relations: Vec::new(),
            evidence: vec![serde_json::from_str::<EvidenceSnapshotDraft>(EVIDENCE).unwrap()],
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision,
        ..
    } = event.payload()
    else {
        unreachable!()
    };
    let (context_id, revision_id) = (context_id.to_string(), revision.revision_id.to_string());
    GitStore::bootstrap_local(harness.root())
        .unwrap()
        .append_event(AppendRequest::event(event))
        .unwrap();

    let review = harness.success(&[
        "context",
        "review",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--verdict",
        "approve",
        "--reason",
        "verified",
    ]);
    let review_event_id = review["data"]["event_id"].as_str().unwrap().to_owned();
    harness.success(&[
        "context",
        "publish",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--expect-no-publication-head",
        "--review-event-id",
        &review_event_id,
    ]);

    harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "FE",
        "--path",
        harness.repository.to_str().unwrap(),
    ]);
    let scan = harness.success(&[
        "repository",
        "scan",
        "--checkout-path",
        harness.repository.to_str().unwrap(),
        "--path",
        "src/contract.rs",
    ]);
    assert_eq!(scan["data"]["status"], "available");

    let reference_input = harness.home.join("reference.json");
    fs::write(
        &reference_input,
        serde_json::to_vec_pretty(&json!({
            "context_id": context_id,
            "revision_id": revision_id,
            "repository_id": "FE",
            "artifact_kind": "file",
            "relation": "implements",
            "locator": {"locator_kind": "file", "path": "src/contract.rs"},
            "supports": "Direct inspection verified the implementation",
            "limitations": ["Synthetic fixture"]
        }))
        .unwrap(),
    )
    .unwrap();
    harness.success(&[
        "engineering-reference",
        "record",
        "--input",
        reference_input.to_str().unwrap(),
    ]);
    let rebuilt = harness.success(&["association", "rebuild"]);
    assert_eq!(rebuilt["data"]["status_counts"]["resolved"], 1);

    let start = harness.hook(&session_start_payload(&harness.repository));
    assert_eq!(
        start,
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, SESSION)
        }})
    );
    harness
}

fn reminder_of(output: &Value) -> Option<String> {
    output
        .get("hookSpecificOutput")?
        .get("additionalContext")?
        .as_str()
        .map(str::to_owned)
}

#[test]
fn disabled_switch_keeps_the_current_post_tool_bytes_for_a_graph_hit() {
    let harness = seeded_harness();
    let output = harness.hook(&read_payload(
        &harness.repository,
        &harness.source_file(),
        "tool-disabled",
    ));
    assert_eq!(output, json!({"systemMessage": INTENT_BOOTSTRAP}));
    assert!(output.get("hookSpecificOutput").is_none());
    assert!(
        !harness.root().join("state/artifact-reminders").exists(),
        "the disabled path must not create reminder state"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn enabled_switch_reminds_once_per_artifact_and_stays_neutral_otherwise() {
    let harness = seeded_harness();
    harness.enable_artifact_focus_reminder();

    let first = harness.hook(&read_payload(
        &harness.repository,
        &harness.source_file(),
        "tool-first",
    ));
    assert_eq!(first["systemMessage"], json!(INTENT_BOOTSTRAP));
    let reminder = reminder_of(&first).expect("the first located read is reminded");
    assert!(
        reminder.len() <= ARTIFACT_FOCUS_REMINDER_MAX_BYTES,
        "reminder is {} bytes",
        reminder.len()
    );
    println!("reminder ({} bytes):\n{reminder}", reminder.len());
    assert!(reminder.contains("ctx_"));
    assert!(reminder.contains("call task_artifact_focus for src/contract.rs to load them"));
    for forbidden in [
        harness.repository.to_str().unwrap(),
        "rationale",
        "evidence",
        "experiment_record",
    ] {
        assert!(!reminder.contains(forbidden), "reminder leaked {forbidden}");
    }

    // Same Artifact, same Session: exactly one reminder.
    let second = harness.hook(&read_payload(
        &harness.repository,
        &harness.source_file(),
        "tool-second",
    ));
    assert_eq!(second, json!({}));

    // A located file with no Graph association stays neutral.
    let unassociated = harness.repository.join("src/other.rs");
    fs::write(&unassociated, "pub fn other() {}\n").unwrap();
    git(&harness.repository, &["add", "--", "src/other.rs"]);
    git(&harness.repository, &["commit", "-q", "-m", "other"]);
    let miss = harness.hook(&read_payload(
        &harness.repository,
        &unassociated,
        "tool-miss",
    ));
    assert_eq!(miss, json!({}));

    // A shell tool never triggers a lookup even for the associated file.
    let mut shell = read_payload(&harness.repository, &harness.source_file(), "tool-shell");
    shell["tool_name"] = json!("Shell");
    shell["tool_input"] = json!({
        "command": "cargo test",
        "working_directory": harness.repository
    });
    assert_eq!(harness.hook(&shell), json!({}));
}

#[test]
fn enabled_switch_is_neutral_without_an_engineering_projection() {
    let harness = seeded_harness();
    harness.enable_artifact_focus_reminder();
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!(
            "{}{suffix}",
            harness.root().join("state/engineering.sqlite").display()
        ));
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove Engineering projection: {error}"),
        }
    }
    assert!(!harness.root().join("state/engineering.sqlite").exists());

    let output = harness.hook(&read_payload(
        &harness.repository,
        &harness.source_file(),
        "tool-missing-projection",
    ));
    assert_eq!(output, json!({"systemMessage": INTENT_BOOTSTRAP}));
    assert!(output.get("hookSpecificOutput").is_none());
}

#[test]
fn enabled_post_tool_hook_meets_the_hot_path_budget() {
    let harness = seeded_harness();
    harness.enable_artifact_focus_reminder();
    let mut samples = Vec::with_capacity(100);
    for index in 0..100 {
        let (_, elapsed) = harness.hook_timed(&read_payload(
            &harness.repository,
            &harness.source_file(),
            &format!("tool-{index}"),
        ));
        samples.push(elapsed);
    }
    samples.sort_unstable();
    let p99 = samples[98];
    println!("enabled PostToolUse hook p99 = {p99:?} (includes process startup)");
    // The 200ms target applies to the Hook's own work. This end-to-end sample
    // also pays one `sctx` process start per iteration and runs beside the rest
    // of the suite, so the assertion uses the documented relaxed local bound;
    // the isolated read-only lookup is measured in
    // `sctx-engineering-graph`'s `read_only_lookup_meets_the_hook_hot_path_budget`.
    assert!(
        p99 <= Duration::from_millis(400),
        "enabled PostToolUse p99 {p99:?} exceeds the relaxed 400ms local budget"
    );
}
