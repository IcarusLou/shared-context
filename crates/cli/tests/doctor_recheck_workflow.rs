//! `sctx doctor --recheck` acceptance test (WP-S2).
//!
//! Confirms a Context through the public chain (`task_intent_update` ->
//! `task_checkpoint` -> `candidate_list`/`candidate_get` -> `candidate_confirm`)
//! with a structured `recheck_when: ["branch_advanced:main@<commit>"]` entry,
//! advances the registered project checkout past that commit, then runs
//! `sctx --json doctor --recheck` and checks that:
//!
//! - the CLI response names the command `doctor.recheck`;
//! - the confirmed Context is reported and marked stale;
//! - `sctx --json search` surfaces the same `derived_state.stale_reason`;
//! - the knowledge Git repository's `HEAD` does not move, because staleness is
//!   a local-only derivation, never a Git Event.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker_with_policy};
use sctx_git_store::GitStore;
use sctx_local_state::Policy;
use serde_json::{Value, json};

fn run_json_cli(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_hook(home: &Path, payload: &Value) -> Value {
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
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn mcp_tool(home: &Path, session: &str, name: &str, arguments: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "codex"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05"}
    });
    let mut arguments = arguments;
    let object = arguments.as_object_mut().unwrap();
    object
        .entry("agent_kind".to_owned())
        .or_insert_with(|| json!("codex"));
    object
        .entry("external_session_id".to_owned())
        .or_insert_with(|| json!(session));
    let call = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    });
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{initialize}").unwrap();
    writeln!(stdin, "{call}").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "MCP {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2, "{responses:#?}");
    assert_eq!(
        responses[1]["result"]["isError"], false,
        "session={session} tool={name} responses={responses:#?}"
    );
    responses[1]["result"]["structuredContent"].clone()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_commit(path: &Path, message: &str) {
    git(
        path,
        &[
            "-c",
            "user.name=Doctor Recheck Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// A real temporary Git repository standing in for a registered project checkout.
fn initialize_project_repository(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join("README.md"), "# Doctor recheck fixture\n").unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["add", "."]);
    git_commit(path, "Add doctor recheck fixture");
    fs::canonicalize(path).unwrap()
}

fn session_start_payload(session: &str, checkout: &Path) -> Value {
    json!({
        "session_id": session,
        "transcript_path": null,
        "cwd": checkout,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
    })
}

/// Picks the confirmation `primary` from a Candidate Review: an existing Space if one was
/// recommended, otherwise the system-proposed new Space (there are no Spaces yet here, so this
/// always takes the `proposed` branch on first use).
fn primary_selection(review: &Value) -> Value {
    let recommendations = review["space_recommendations"].as_array().unwrap();
    if let Some(proposed) = recommendations
        .iter()
        .find(|recommendation| recommendation["kind"] == "proposed_new_space_intent")
    {
        return json!({"new_space_recommendation_id": proposed["recommendation_id"]});
    }
    let existing = recommendations
        .iter()
        .find(|recommendation| recommendation["kind"] == "existing")
        .unwrap_or_else(|| panic!("no usable Space recommendation: {review:#}"));
    json!({"existing_space_id": existing["space_id"]})
}

#[test]
#[allow(clippy::too_many_lines)]
fn structured_recheck_when_marks_a_confirmed_context_stale_without_writing_a_git_event() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("doctor recheck home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".shared-context");
    let store = GitStore::bootstrap_local(&root).unwrap();

    let project = initialize_project_repository(&home.join("project checkout"));
    run_json_cli(
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            "RecheckRepo",
            "--path",
            project.to_str().unwrap(),
        ],
    );

    let session = "doctor-recheck-session".to_owned();
    assert_eq!(
        run_hook(&home, &session_start_payload(&session, &project)),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, &session)
        }}),
        "the registered checkout must activate a Direct Session"
    );

    let task = mcp_tool(
        &home,
        &session,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {
                "goal": "Track when the recheck harness marker branch advances past its recorded commit"
            }
        }),
    );
    let task_id = task["task_id"].as_str().unwrap().to_owned();
    let intent_revision_id = task["intent_revision_id"].as_str().unwrap().to_owned();

    let checkpoint = mcp_tool(
        &home,
        &session,
        "task_checkpoint",
        json!({
            "claims": [{
                "context_kind": "decision",
                "statement": "The recheck harness marker context is only valid while main stays at the recorded commit",
                "rationale": "Direct inspection of the fixture checkout confirmed the recorded branch position",
                "conditions": ["main tracked at the recorded commit"],
                "evidence": [{
                    "evidence_type": "experiment_record",
                    "summary": "git rev-parse HEAD on the registered checkout matched the recorded commit",
                    "limitations": ["single local checkout observation"]
                }]
            }],
            "unknowns": []
        }),
    );
    assert_eq!(
        checkpoint["candidate_build"]["status"], "pending",
        "Checkpoint did not produce a pending Candidate Build: {checkpoint:#}"
    );

    let pending = mcp_tool(
        &home,
        &session,
        "candidate_list",
        json!({"status": "pending", "limit": 10, "token_budget": 32768}),
    );
    let candidate_ids = pending["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|review| review["candidate_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        candidate_ids.len(),
        1,
        "expected exactly one pending Candidate: {pending:#}"
    );
    let candidate_id = candidate_ids[0].clone();

    let review = mcp_tool(
        &home,
        &session,
        "candidate_get",
        json!({"candidate_id": candidate_id}),
    );

    // The commit the structured `recheck_when` entry pins the Context to. Recorded before the
    // Candidate is confirmed and before any further commit advances the branch.
    let pinned_commit = git(&project, &["rev-parse", "HEAD"]);

    let confirmed = mcp_tool(
        &home,
        &session,
        "candidate_confirm",
        json!({
            "expected_task_id": task_id,
            "expected_intent_revision_id": intent_revision_id,
            "candidate_id": candidate_id,
            "expected_review_version": review["review_version"],
            "primary": primary_selection(&review),
            "related_space_ids": [],
            "edits": {
                "recheck_when": [format!("branch_advanced:main@{pinned_commit}")]
            }
        }),
    );
    assert_eq!(
        confirmed["status"], "confirmed",
        "Candidate was not confirmed: {confirmed:#}"
    );
    let context_id = confirmed["context_id"].as_str().unwrap().to_owned();

    // Staleness is local derived state; it must never touch the knowledge Git repository. Any
    // further commit count observed on `store.repository()` after this point is a bug.
    let knowledge_head_before_recheck = git(store.repository(), &["rev-parse", "HEAD"]);

    // Advance the registered checkout's branch past the commit the Context was pinned to.
    fs::write(
        project.join("README.md"),
        "# Doctor recheck fixture\n\nUpdated.\n",
    )
    .unwrap();
    git(&project, &["add", "."]);
    git_commit(&project, "Advance main past the recorded commit");
    let advanced_commit = git(&project, &["rev-parse", "HEAD"]);
    assert_ne!(advanced_commit, pinned_commit);

    let recheck = run_json_cli(&home, &["doctor", "--recheck"]);
    assert_eq!(
        recheck["command"], "doctor.recheck",
        "unexpected doctor --recheck envelope: {recheck:#}"
    );
    let results = recheck["data"]["results"].as_array().unwrap();
    let evaluated = results
        .iter()
        .find(|result| result["context_id"] == context_id)
        .unwrap_or_else(|| panic!("confirmed Context missing from recheck results: {recheck:#}"));
    let stale_reason = evaluated["stale_reason"]
        .as_str()
        .unwrap_or_else(|| panic!("Context was not marked stale: {recheck:#}"));
    assert!(
        stale_reason.starts_with("branch_advanced: main moved from"),
        "unexpected stale reason: {stale_reason}"
    );
    assert_eq!(recheck["data"]["stale_contexts"], 1);

    // `search` must surface the same locally derived staleness on the same Context.
    let search = run_json_cli(
        &home,
        &["search", "--query", "recheck harness marker branch"],
    );
    assert_eq!(search["command"], "search");
    let search_results = search["data"]["results"].as_array().unwrap();
    let matched = search_results
        .iter()
        .find(|result| result["context_id"] == context_id)
        .unwrap_or_else(|| panic!("confirmed Context missing from search results: {search:#}"));
    let search_stale_reason = matched["derived_state"]["stale_reason"]
        .as_str()
        .unwrap_or_else(|| panic!("search did not report stale_reason: {search:#}"));
    assert!(!search_stale_reason.is_empty());

    // Neither `doctor --recheck` nor `search` may write a Git Event: staleness is a local
    // projection column, never a shared fact.
    let knowledge_head_after = git(store.repository(), &["rev-parse", "HEAD"]);
    assert_eq!(
        knowledge_head_before_recheck, knowledge_head_after,
        "doctor --recheck / search must not write to the knowledge Git repository"
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
