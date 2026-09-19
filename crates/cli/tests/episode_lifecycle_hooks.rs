use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker_with_policy};
use sctx_domain::{
    Applicability, CandidateReviewStatus, EvidenceSnapshotDraft, EvidenceType,
    ExternalSessionLocator, TaskId, WorkEpisodeStatus, WorkingIntentSnapshot,
};
use sctx_git_store::GitStore;
use sctx_index::ProjectionIndex;
use sctx_local_state::Policy;
use sctx_local_state::UserConfigStore;
use sctx_task_runtime::{
    AgentCheckpointWrite, CandidateBuildStatus, CheckpointBoundary, CheckpointClaimDraft,
    TaskRuntime,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    workspace: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("episode lifecycle home");
        let root = home.join(".shared-context");
        let workspace = temporary.path().join("episode lifecycle repository");
        fs::create_dir_all(&home).unwrap();
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
        GitStore::bootstrap_local(&root).unwrap();
        UserConfigStore::initialize(&root)
            .unwrap()
            .add_repository(
                sctx_domain::RepositoryId::new(),
                std::slice::from_ref(&workspace),
            )
            .unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            workspace,
        }
    }

    fn hook(&self, agent: &str, payload: &Value) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
        command
            .args(["hook", "--agent", agent])
            .env("HOME", &self.home);
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
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn mcp(&self, name: &str, arguments: &Value) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["mcp", "serve", "--client", "codex"])
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "episode-lifecycle", "version": "1"}
            }
        });
        let call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
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
        assert_eq!(responses.len(), 2, "unexpected MCP output: {responses:#?}");
        assert_eq!(responses[1]["result"]["isError"], false, "{responses:#?}");
        responses[1]["result"]["structuredContent"].clone()
    }

    fn activate(&self, agent: &str, session: &str) {
        let payload = if agent == "codex" {
            json!({
                "session_id": session,
                "transcript_path": null,
                "cwd": self.workspace,
                "hook_event_name": "SessionStart",
                "model": "gpt-5.6-sol",
                "permission_mode": "default",
                "source": "startup"
            })
        } else {
            json!({
                "conversation_id": session,
                "generation_id": format!("generation-{session}"),
                "model": "claude-opus-4-7-thinking-max",
                "hook_event_name": "sessionStart",
                "cursor_version": "3.13.10",
                "workspace_roots": [self.workspace],
                "user_email": null,
                "transcript_path": null,
                "session_id": session,
                "is_background_agent": false,
                "composer_mode": "agent"
            })
        };
        let response = self.hook(agent, &payload);
        let expected = if agent == "codex" {
            json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext":
                    shared_context_activation_marker(AgentKind::Codex, session)
            }})
        } else {
            json!({
                "additional_context":
                    shared_context_activation_marker(AgentKind::Cursor, session)
            })
        };
        assert_eq!(response, expected);
    }
}

fn intent(session: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: format!("solidify {session} engineering conclusions"),
        current_direction: Some("close only explicitly checkpointed cognition".to_owned()),
        in_scope: vec!["Agent lifecycle automation".to_owned()],
        out_of_scope: Vec::new(),
        domains: vec!["lifecycle".to_owned()],
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec!["one closed Episode produces one Candidate".to_owned()],
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn checkpoint_claim(session: &str) -> CheckpointClaimDraft {
    CheckpointClaimDraft {
        context_kind_hint: None,
        topic_key_hint: Some(format!("capture/{session}")),
        statement: format!("{session} keeps lifecycle automation idempotent"),
        rationale: "A direct validation records the expected lifecycle result".to_owned(),
        applicability: Applicability {
            domains: vec!["lifecycle".to_owned()],
            platforms: Vec::new(),
            conditions: vec![session.to_owned()],
        },
        evidence_refs: Vec::new(),
        inline_validations: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: format!("{session} lifecycle behavior"),
            content: json!({"actual": "passed", "session": session}),
            interpretation: "The persisted Checkpoint is sufficient for a Candidate draft"
                .to_owned(),
            limitations: Vec::new(),
        }],
        engineering_references: Vec::new(),
    }
}

fn assert_flat_checkpoint_guidance(response: &Value) {
    let message = response["systemMessage"].as_str().unwrap();
    assert!(message.contains("task_checkpoint"));
    assert!(message.contains("complete direct Claims/Unknowns"));
    assert!(message.contains("server resolves the current Task, Intent, and lifecycle"));
    // A reminder that asks for a Checkpoint also carries the team's `## stop` standard for one:
    // this installation has no `policy.md`, so that is the built-in default's line.
    let stop = Policy::inline(Policy::compiled_default().stop());
    assert!(
        message.contains(stop.trim()),
        "the boundary reminder must carry the team stop policy: {message}"
    );
    for forbidden in [
        "expected_task_id",
        "expected_intent_revision_id",
        "expected_episode_version",
        "boundary",
        "inline_validation",
        "capture",
    ] {
        assert!(
            !message.contains(forbidden),
            "stale Hook guidance: {message}"
        );
    }
}

fn open_checkpoint(
    runtime: &TaskRuntime,
    agent: &str,
    session: &str,
) -> (ExternalSessionLocator, sctx_domain::WorkEpisodeId) {
    let locator = ExternalSessionLocator::new(agent, session).unwrap();
    let snapshot = runtime
        .open_or_create(locator.clone(), TaskId::new(), intent(session), Vec::new())
        .unwrap()
        .snapshot;
    let outcome = runtime
        .write_agent_checkpoint(&AgentCheckpointWrite {
            locator: locator.clone(),
            expected_task_id: snapshot.task_id,
            expected_intent_revision_id: snapshot.current_intent_revision().unwrap().revision_id,
            expected_episode_version: 0,
            boundary: CheckpointBoundary::Continue,
            claims: vec![checkpoint_claim(session)],
            unknowns: Vec::new(),
        })
        .unwrap();
    (locator, outcome.episode.episode.episode_id)
}

fn codex_event(session: &str, cwd: &Path, event: &str, raw_marker: &str) -> Value {
    match event {
        "PreCompact" => json!({
            "session_id": session,
            "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
            "cwd": cwd,
            "hook_event_name": "PreCompact",
            "model": "gpt-5.6-sol",
            "turn_id": format!("turn-{session}"),
            "trigger": "auto"
        }),
        "Stop" => json!({
            "session_id": session,
            "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
            "cwd": cwd,
            "hook_event_name": "Stop",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": format!("turn-{session}"),
            "stop_hook_active": false,
            "last_assistant_message": raw_marker
        }),
        "SessionEnd" => json!({
            "session_id": session,
            "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
            "cwd": cwd,
            "hook_event_name": "SessionEnd",
            "model": "gpt-5.6-sol",
            "reason": "other"
        }),
        // A generic Codex `shell` tool call: real "substantial tool activity" for the TurnStop
        // checkpoint reminder gate (WP-V6 fix 3), independent of whether it names a file this
        // installation's Repository catalog can attribute.
        "PostToolUse" => json!({
            "session_id": session,
            "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
            "cwd": cwd,
            "hook_event_name": "PostToolUse",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": format!("turn-{raw_marker}"),
            "tool_name": "shell",
            "tool_use_id": format!("call-{raw_marker}"),
            "tool_input": {"command": ["echo", raw_marker], "workdir": cwd},
            "tool_response": {"output": "done"}
        }),
        _ => unreachable!(),
    }
}

fn cursor_stop(session: &str, cwd: &Path, raw_marker: &str) -> Value {
    json!({
        "conversation_id": session,
        "generation_id": format!("generation-{session}"),
        "model": "claude-opus-4-7-thinking-max",
        "hook_event_name": "stop",
        "cursor_version": "3.13.10",
        "workspace_roots": [cwd],
        "user_email": null,
        "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
        "status": "completed",
        "loop_count": 0
    })
}

fn files_contain(path: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_dir() {
            if files_contain(&path, needle) {
                return true;
            }
        } else if metadata.file_type().is_file()
            && fs::read(&path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            })
        {
            return true;
        }
    }
    false
}

#[test]
#[allow(clippy::too_many_lines)]
fn real_hooks_close_checkpointed_episodes_build_once_and_keep_sessions_isolated() {
    let harness = Arc::new(Harness::new());
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    harness.activate("codex", "codex-lifecycle");
    harness.activate("cursor", "cursor-lifecycle");
    let (_, codex_episode) = open_checkpoint(&runtime, "codex", "codex-lifecycle");
    let (_, cursor_episode) = open_checkpoint(&runtime, "cursor", "cursor-lifecycle");
    let raw_marker = "RAW_TRANSCRIPT_AND_LAST_MESSAGE_MUST_NOT_PERSIST";
    let barrier = Arc::new(Barrier::new(2));
    let codex = {
        let harness = Arc::clone(&harness);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            harness.hook(
                "codex",
                &codex_event("codex-lifecycle", &harness.home, "PreCompact", raw_marker),
            )
        })
    };
    let cursor = {
        let harness = Arc::clone(&harness);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            harness.hook(
                "cursor",
                &cursor_stop("cursor-lifecycle", &harness.home, raw_marker),
            )
        })
    };
    let codex_output = codex.join().unwrap();
    let cursor_output = cursor.join().unwrap();
    assert!(
        codex_output["systemMessage"]
            .as_str()
            .is_some_and(
                |message| message.contains("durably closed") && message.contains("complete")
            )
    );
    // Cursor reports the same closed-Episode boundary Codex does, on the one text field
    // a `stop` payload accepts.
    assert!(
        cursor_output["user_message"]
            .as_str()
            .is_some_and(
                |message| message.contains("durably closed") && message.contains("complete")
            ),
        "{cursor_output:#}"
    );

    for episode_id in [codex_episode, cursor_episode] {
        let episode = runtime.read_work_episode(episode_id).unwrap().unwrap();
        assert!(matches!(
            episode.episode.status,
            WorkEpisodeStatus::Closed { .. }
        ));
        let build = runtime.read_candidate_build(episode_id).unwrap().unwrap();
        assert_eq!(build.status, CandidateBuildStatus::Complete);
        assert_eq!(build.items.len(), 1);
        assert!(build.items[0].candidate_id.is_some());
    }
    let index = ProjectionIndex::for_store(&GitStore::bootstrap_local(&harness.root).unwrap());
    let before = index.domain_snapshot().unwrap().projection.candidates.len();
    assert_eq!(before, 2);
    let started = Instant::now();
    for _ in 0..8 {
        let retry = harness.hook(
            "codex",
            &codex_event("codex-lifecycle", &harness.home, "Stop", raw_marker),
        );
        assert_eq!(retry, json!({}));
        let cursor_retry = harness.hook(
            "cursor",
            &cursor_stop("cursor-lifecycle", &harness.home, raw_marker),
        );
        assert_eq!(cursor_retry, json!({}));
    }
    // Silence removes only the closure notice: Cursor still receives its compaction marker.
    let mut compact = cursor_stop("cursor-lifecycle", &harness.home, raw_marker);
    compact["hook_event_name"] = json!("preCompact");
    compact["trigger"] = json!("auto");
    assert_eq!(
        harness.hook("cursor", &compact),
        json!({
            "user_message": shared_context_activation_marker(AgentKind::Cursor, "cursor-lifecycle")
        })
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        index.domain_snapshot().unwrap().projection.candidates.len(),
        before,
        "duplicate lifecycle events must not create duplicate Candidates"
    );
    assert!(!files_contain(&harness.root, raw_marker));
}

#[test]
#[allow(clippy::too_many_lines)]
fn precompact_resume_and_turn_stop_use_new_episodes_under_the_same_mcp_task() {
    let harness = Harness::new();
    let session = "precompact-resume-same-task";
    harness.activate("codex", session);
    let task = harness.mcp(
        "task_intent_update",
        &json!({
            "agent_kind": "codex",
            "external_session_id": session,
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "resume one Task after context compaction"}
        }),
    );
    let task_id = task["task_id"].as_str().unwrap().to_owned();
    let checkpoint = |statement: &str, episode: u64| {
        json!({
            "agent_kind": "codex",
            "external_session_id": session,
            "claims": [{
                "context_kind": "validation",
                "statement": statement,
                "rationale": "A real MCP write validates the lifecycle boundary",
                "conditions": ["resume"],
                "evidence": [{
                    "evidence_type": "experiment_record",
                    "summary": format!("Episode {episode} MCP write passed"),
                    "limitations": []
                }]
            }],
            "unknowns": []
        })
    };

    let first_input = checkpoint("PreCompact closes Episode one", 1);
    let first = harness.mcp("task_checkpoint", &first_input);
    let first_episode_id = first["episode_id"].as_str().unwrap().to_owned();
    let compacted = harness.hook(
        "codex",
        &codex_event(
            session,
            &harness.home,
            "PreCompact",
            "RAW_PRECOMPACT_RESUME",
        ),
    );
    assert!(
        compacted["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("durably closed"))
    );
    let first_retry = harness.mcp("task_checkpoint", &first_input);
    assert_eq!(first_retry["replayed"], true);
    assert_eq!(first_retry["episode_id"], first_episode_id);

    let second = harness.mcp(
        "task_checkpoint",
        &checkpoint("Resume creates Episode two without changing Task", 2),
    );
    let second_episode_id = second["episode_id"].as_str().unwrap().to_owned();
    assert_eq!(second["replayed"], false);
    assert_ne!(second_episode_id, first_episode_id);
    assert_eq!(second["episode_version"], 1);
    let stopped = harness.hook(
        "codex",
        &codex_event(session, &harness.home, "Stop", "RAW_RESUMED_TURN_STOP"),
    );
    assert!(
        stopped["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("durably closed"))
    );
    let third = harness.mcp(
        "task_checkpoint",
        &checkpoint("TurnStop permits Episode three without changing Task", 3),
    );
    let third_episode_id = third["episode_id"].as_str().unwrap().to_owned();
    assert_eq!(third["replayed"], false);
    assert_ne!(third_episode_id, first_episode_id);
    assert_ne!(third_episode_id, second_episode_id);

    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let active = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    assert_eq!(active.task_id.to_string(), task_id);
    let episodes = runtime
        .list_work_episodes(active.task_session_id, 10)
        .unwrap();
    assert_eq!(episodes.len(), 3);
    assert_eq!(
        episodes
            .iter()
            .filter(|episode| matches!(episode.episode.status, WorkEpisodeStatus::Closed { .. }))
            .count(),
        3
    );
    assert!(episodes.iter().any(|episode| {
        episode.episode.episode_id.to_string() == third_episode_id
            && matches!(episode.episode.status, WorkEpisodeStatus::Closed { .. })
    }));
    let candidate_sources = runtime
        .list_candidate_reviews(&locator, CandidateReviewStatus::Pending, 10, None)
        .unwrap()
        .records
        .into_iter()
        .map(|review| review.source_episode.episode_id.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        candidate_sources,
        std::collections::BTreeSet::from([first_episode_id, second_episode_id])
    );
}

#[test]
fn out_of_order_stop_requires_checkpoint_and_session_end_never_closes_or_builds() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let session = "out-of-order";
    harness.activate("codex", session);
    harness.activate("codex", "session-end-only");
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let snapshot = runtime
        .open_or_create(locator.clone(), TaskId::new(), intent(session), Vec::new())
        .unwrap()
        .snapshot;
    let stop = harness.hook(
        "codex",
        &codex_event(session, &harness.home, "Stop", "RAW_OUT_OF_ORDER"),
    );
    assert_flat_checkpoint_guidance(&stop);
    assert!(
        runtime
            .list_work_episodes(snapshot.task_session_id, 10)
            .unwrap()
            .is_empty()
    );
    let opened = runtime
        .open_work_episode(
            &locator,
            snapshot.task_id,
            snapshot.current_intent_revision().unwrap().revision_id,
        )
        .unwrap();
    // The TurnStop checkpoint reminder gate (WP-V6 fix 3) only repeats the reminder for a Stop
    // that saw real activity since the last one; this test's whole point is two reminders in
    // sequence, so it needs a tool call between them the way a real out-of-order Session would
    // have one. The tool call's `cwd` has to be the registered checkout (`harness.workspace`),
    // not `harness.home` the way the Session-lifecycle events on this page use: PostToolUse
    // activation is repository-scoped, and only Session-lifecycle events are not.
    harness.hook(
        "codex",
        &codex_event(
            session,
            &harness.workspace,
            "PostToolUse",
            "RAW_BETWEEN_STOPS",
        ),
    );
    let checkpoint_required = harness.hook(
        "codex",
        &codex_event(session, &harness.home, "Stop", "RAW_CHECKPOINT_REQUIRED"),
    );
    assert_flat_checkpoint_guidance(&checkpoint_required);
    let still_open = runtime
        .read_work_episode(opened.episode.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(still_open.episode.status, WorkEpisodeStatus::Open);

    let (_, episode_id) = open_checkpoint(&runtime, "codex", "session-end-only");
    let ended = harness.hook(
        "codex",
        &codex_event(
            "session-end-only",
            &harness.home,
            "SessionEnd",
            "RAW_SESSION_END",
        ),
    );
    assert_eq!(ended, json!({}));
    let episode = runtime.read_work_episode(episode_id).unwrap().unwrap();
    assert_eq!(episode.episode.status, WorkEpisodeStatus::Open);
    assert!(runtime.read_candidate_build(episode_id).unwrap().is_none());
}

/// End to end through the real `sctx hook` binary (WP-V6 fix 3, `docs/deferred-issues.md` #6): a
/// Session that never checkpoints gets at most three `call task_checkpoint` reminders across
/// however many `Stop` (Codex's `TurnStop`) events it produces, an idle `Stop` -- no tool call
/// since the last reminder -- is suppressed and spends none of the budget, and the fourth
/// reminder never fires even once real activity resumes.
#[test]
fn turn_stop_checkpoint_reminder_throttles_after_three_and_skips_idle_turns() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let session = "reminder-throttle";
    harness.activate("codex", session);
    runtime
        .open_or_create(
            ExternalSessionLocator::new("codex", session).unwrap(),
            TaskId::new(),
            intent(session),
            Vec::new(),
        )
        .unwrap();

    let stop = |marker: &str| {
        harness.hook(
            "codex",
            &codex_event(session, &harness.workspace, "Stop", marker),
        )
    };
    let tool_use = |marker: &str| {
        harness.hook(
            "codex",
            &codex_event(session, &harness.workspace, "PostToolUse", marker),
        )
    };
    let is_the_nag = |response: &Value| {
        response["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("call task_checkpoint"))
    };

    // 1st reminder always fires: no checkpoint exists yet and nothing has been said before.
    let first = stop("REMINDER_1");
    assert_flat_checkpoint_guidance(&first);

    // Immediately again with no tool call in between: an idle turn is suppressed, and it must not
    // spend part of the three-reminder budget either.
    assert!(
        !is_the_nag(&stop("REMINDER_IDLE_A")),
        "an idle Stop must not repeat the reminder"
    );
    assert!(
        !is_the_nag(&stop("REMINDER_IDLE_B")),
        "repeating the idle Stop must still not spend the budget"
    );

    // Real tool activity unlocks the 2nd reminder.
    tool_use("ACTIVITY_1");
    assert_flat_checkpoint_guidance(&stop("REMINDER_2"));

    tool_use("ACTIVITY_2");
    assert_flat_checkpoint_guidance(&stop("REMINDER_3"));

    // The budget is now spent: a 4th reminder never fires this Session, even with fresh activity
    // right before it.
    tool_use("ACTIVITY_3");
    assert!(
        !is_the_nag(&stop("REMINDER_4")),
        "a 4th reminder must not fire even with new activity: the Session budget is spent"
    );
    tool_use("ACTIVITY_4");
    assert!(
        !is_the_nag(&stop("REMINDER_5")),
        "the budget stays spent for the rest of the Session"
    );
}

#[test]
fn concurrent_turn_stop_processes_converge_on_one_build_and_candidate() {
    let harness = Arc::new(Harness::new());
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    harness.activate("codex", "concurrent-stop");
    let (_, episode_id) = open_checkpoint(&runtime, "codex", "concurrent-stop");
    let barrier = Arc::new(Barrier::new(12));
    let outputs = (0..12)
        .map(|_| {
            let harness = Arc::clone(&harness);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                harness.hook(
                    "codex",
                    &codex_event(
                        "concurrent-stop",
                        &harness.home,
                        "Stop",
                        "RAW_CONCURRENT_STOP",
                    ),
                )
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert!(outputs.iter().any(|output| {
        output["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("durably closed"))
    }));
    assert!(
        outputs.iter().all(|output| {
            output == &json!({})
                || output["systemMessage"].as_str().is_some_and(|message| {
                    message.contains("durably closed")
                        || message.contains("temporarily unavailable")
                })
        }),
        "unexpected concurrent Hook outputs: {outputs:#?}"
    );
    let build = runtime.read_candidate_build(episode_id).unwrap().unwrap();
    assert_eq!(build.status, CandidateBuildStatus::Complete);
    assert_eq!(build.items.len(), 1);
    let index = ProjectionIndex::for_store(&GitStore::bootstrap_local(&harness.root).unwrap());
    assert_eq!(
        index.domain_snapshot().unwrap().projection.candidates.len(),
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn turn_stop_fails_open_after_close_and_later_retry_recovers_builder() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    harness.activate("codex", "builder-recovery");
    let (locator, episode_id) = open_checkpoint(&runtime, "codex", "builder-recovery");
    let repository = UserConfigStore::open_existing(&harness.root)
        .unwrap()
        .repository()
        .to_path_buf();
    let head = repository.join(".git/HEAD");
    let original_head = fs::read(&head).unwrap();
    fs::write(&head, b"invalid git head\n").unwrap();

    let failed = harness.hook(
        "codex",
        &codex_event(
            "builder-recovery",
            &harness.home,
            "Stop",
            "RAW_FAILED_BUILDER",
        ),
    );
    assert!(
        failed["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("temporarily unavailable"))
    );
    assert!(matches!(
        runtime
            .read_work_episode(episode_id)
            .unwrap()
            .unwrap()
            .episode
            .status,
        WorkEpisodeStatus::Closed { .. }
    ));
    assert!(runtime.read_candidate_build(episode_id).unwrap().is_none());

    fs::write(&head, original_head).unwrap();
    let active = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    runtime
        .open_work_episode(
            &locator,
            active.task_id,
            active.current_intent_revision().unwrap().revision_id,
        )
        .unwrap();
    let newer_episode_id = runtime
        .write_agent_checkpoint(&AgentCheckpointWrite {
            locator,
            expected_task_id: active.task_id,
            expected_intent_revision_id: active.current_intent_revision().unwrap().revision_id,
            expected_episode_version: 0,
            boundary: CheckpointBoundary::Continue,
            // The newer Episode states a distinct fact on purpose: a near-restatement of the
            // first Episode's Claim is collapsed by Builder deduplication, which would hide the
            // recovery this test closes.
            claims: vec![CheckpointClaimDraft {
                statement: "The recovered Builder drafts an independent durable Candidate"
                    .to_owned(),
                ..checkpoint_claim("builder-recovery-newer")
            }],
            unknowns: Vec::new(),
        })
        .unwrap()
        .episode
        .episode
        .episode_id;
    let recovered = harness.hook(
        "codex",
        &codex_event(
            "builder-recovery",
            &harness.home,
            "Stop",
            "RAW_RECOVERED_BUILDER",
        ),
    );
    assert!(
        recovered["systemMessage"]
            .as_str()
            .is_some_and(
                |message| message.contains("durably closed") && message.contains("complete")
            )
    );
    let build = runtime.read_candidate_build(episode_id).unwrap().unwrap();
    assert_eq!(build.status, CandidateBuildStatus::Complete);
    assert_eq!(build.items.len(), 1);
    let newer = runtime
        .read_candidate_build(newer_episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(newer.status, CandidateBuildStatus::Complete);
    assert_eq!(newer.items.len(), 1);

    // A quiet replay of the already-complete current Episode must still run the older-Episode
    // recovery sweep. Its completion is not a second receipt for the current Episode.
    rusqlite::Connection::open(runtime.database_path())
        .unwrap()
        .execute(
            "UPDATE candidate_build SET status = 'pending' WHERE episode_id = ?1",
            [episode_id.to_string()],
        )
        .unwrap();
    let quiet = harness.hook(
        "codex",
        &codex_event(
            "builder-recovery",
            &harness.home,
            "Stop",
            "OLDER_BUILD_RECOVERY",
        ),
    );
    assert_eq!(quiet, json!({}));
    assert_eq!(
        runtime
            .read_candidate_build(episode_id)
            .unwrap()
            .unwrap()
            .status,
        CandidateBuildStatus::Complete
    );
}

#[test]
fn repeated_closed_episode_reports_only_new_terminal_build_recovery() {
    for pending in [false, true] {
        for evidenced in [false, true] {
            let harness = Harness::new();
            let runtime = TaskRuntime::initialize(&harness.root).unwrap();
            let session = "current-build-recovery";
            harness.activate("codex", session);
            let locator = ExternalSessionLocator::new("codex", session).unwrap();
            let active = runtime
                .open_or_create(
                    locator.clone(),
                    TaskId::new(),
                    intent(session),
                    vec![sctx_domain::TaskSignal {
                        kind: sctx_domain::TaskSignalKind::Prompt,
                        content: "A locating prompt is not engineering evidence".to_owned(),
                    }],
                )
                .unwrap()
                .snapshot;
            let mut claim = checkpoint_claim(session);
            if !evidenced {
                claim.inline_validations.clear();
                // A legitimate local reference can still be insufficient for Builder: Prompt
                // Signals locate work but cannot substantiate an engineering Claim.
                claim
                    .evidence_refs
                    .push(sctx_domain::CheckpointEvidenceRef::TaskSignal {
                        signal_id: runtime.read_signal_history(active.task_session_id).unwrap()[0]
                            .signal_id,
                    });
            }
            let episode_id = runtime
                .write_agent_checkpoint(&AgentCheckpointWrite {
                    locator: locator.clone(),
                    expected_task_id: active.task_id,
                    expected_intent_revision_id: active
                        .current_intent_revision()
                        .unwrap()
                        .revision_id,
                    expected_episode_version: 0,
                    boundary: CheckpointBoundary::Continue,
                    claims: vec![claim],
                    unknowns: Vec::new(),
                })
                .unwrap()
                .episode
                .episode
                .episode_id;
            runtime.close_checkpointed_work_episode(&locator).unwrap();
            assert!(runtime.read_candidate_build(episode_id).unwrap().is_none());
            if pending {
                sctx_mcp::build_closed_episode_at_root(&harness.root, episode_id).unwrap();
                // Reproduce a pending build-status projection over retained item outcomes. The
                // real Builder must converge it before this replay can earn a recovery receipt.
                rusqlite::Connection::open(runtime.database_path())
                    .unwrap()
                    .execute(
                        "UPDATE candidate_build SET status = 'pending' WHERE episode_id = ?1",
                        [episode_id.to_string()],
                    )
                    .unwrap();
                assert_eq!(
                    runtime
                        .read_candidate_build(episode_id)
                        .unwrap()
                        .unwrap()
                        .status,
                    CandidateBuildStatus::Pending
                );
            }
            let payload = codex_event(session, &harness.home, "Stop", "RECOVERY_FIXTURE");
            let receipt = harness.hook("codex", &payload);
            let status = if evidenced { "complete" } else { "incomplete" };
            let message = receipt["systemMessage"].as_str().unwrap();
            assert!(
                message.contains(&format!("Candidate Builder is {status} with")),
                "{receipt:#}"
            );
            assert!(message.contains(&episode_id.to_string()));
            assert_eq!(
                runtime
                    .read_candidate_build(episode_id)
                    .unwrap()
                    .unwrap()
                    .status,
                if evidenced {
                    CandidateBuildStatus::Complete
                } else {
                    CandidateBuildStatus::Incomplete
                }
            );
            assert_eq!(harness.hook("codex", &payload), json!({}));
        }
    }
}

#[test]
fn lifecycle_boundary_creates_no_capture_storage() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    harness.activate("codex", "capture-unavailable");
    let (_, episode_id) = open_checkpoint(&runtime, "codex", "capture-unavailable");
    let response = harness.hook(
        "codex",
        &codex_event(
            "capture-unavailable",
            &harness.home,
            "Stop",
            "RAW_CAPTURE_UNAVAILABLE",
        ),
    );
    assert!(
        response["systemMessage"]
            .as_str()
            .is_some_and(
                |message| message.contains("durably closed") && message.contains("complete")
            )
    );
    assert!(matches!(
        runtime
            .read_work_episode(episode_id)
            .unwrap()
            .unwrap()
            .episode
            .status,
        WorkEpisodeStatus::Closed { .. }
    ));
    assert_eq!(
        runtime
            .read_candidate_build(episode_id)
            .unwrap()
            .unwrap()
            .status,
        CandidateBuildStatus::Complete
    );
    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!harness.root.join("state").join(removed).exists());
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn session_end_records_weak_usage_and_preserves_checkpoint_verdicts_on_both_hosts() {
    use sctx_task_runtime::{ContextInjectionSource, InjectedContext};
    for agent in ["codex", "cursor"] {
        let harness = Harness::new();
        let runtime = TaskRuntime::initialize(&harness.root).unwrap();
        let session = "session-close-usage";
        harness.activate(agent, session);
        let (locator, _) = open_checkpoint(&runtime, agent, session);
        let first = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
        let context = InjectedContext {
            context_id: sctx_domain::ContextId::new(),
            revision_id: sctx_domain::RevisionId::new(),
        };
        runtime
            .record_task_injections_at(
                first.task_id,
                first.current_intent_revision().unwrap().revision_id,
                ContextInjectionSource::TaskContext,
                &[context],
                10,
            )
            .unwrap();
        let stop = if agent == "codex" {
            codex_event(session, &harness.workspace, "Stop", "usage-checkpoint")
        } else {
            cursor_stop(session, &harness.workspace, "usage-checkpoint")
        };
        harness.hook(agent, &stop);
        let connection = rusqlite::Connection::open(runtime.database_path()).unwrap();
        let read = |task: TaskId| {
            connection.query_row(
            "SELECT outcome, basis, recorded_at_unix_seconds FROM context_usage WHERE task_id = ?1",
            [task.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))).unwrap()
        };
        let strong_before = read(first.task_id);
        assert_eq!(strong_before.0, "ignored");
        assert_eq!(strong_before.1, "checkpoint_derived");
        let second = runtime
            .start_new_task(
                &locator,
                first.task_id,
                &intent("no-checkpoint"),
                Vec::new(),
            )
            .unwrap()
            .snapshot;
        runtime
            .record_task_injections_at(
                second.task_id,
                second.current_intent_revision().unwrap().revision_id,
                ContextInjectionSource::TaskContext,
                &[context],
                20,
            )
            .unwrap();
        let third = runtime
            .start_new_task(
                &locator,
                second.task_id,
                &intent("also-no-checkpoint"),
                Vec::new(),
            )
            .unwrap()
            .snapshot;
        runtime
            .record_task_injections_at(
                third.task_id,
                third.current_intent_revision().unwrap().revision_id,
                ContextInjectionSource::TaskContext,
                &[context],
                30,
            )
            .unwrap();
        let end = if agent == "codex" {
            let mut event = codex_event(session, &harness.workspace, "SessionEnd", "usage-end");
            event.as_object_mut().unwrap().remove("model");
            event
        } else {
            json!({
                "conversation_id": session, "generation_id": "close-generation", "model": "claude-opus-4-7-thinking-max",
                "hook_event_name": "sessionEnd", "cursor_version": "3.13.10", "workspace_roots": [harness.workspace],
                "user_email": null, "transcript_path": null, "session_id": session, "reason": "completed",
                "duration_ms": 45000, "is_background_agent": false, "final_status": "completed"
            })
        };
        assert_eq!(harness.hook(agent, &end), json!({}));
        assert_eq!(read(first.task_id), strong_before);
        for task in [second.task_id, third.task_id] {
            let row = read(task);
            assert_eq!(
                (row.0.as_str(), row.1.as_str()),
                ("ignored", "session_close")
            );
        }
    }
}

#[test]
fn session_end_usage_lock_contention_is_quiet_and_bounded() {
    let harness = Harness::new();
    harness.activate("codex", "locked-close");
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let connection = rusqlite::Connection::open(runtime.database_path()).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(
        harness.hook(
            "codex",
            &codex_event(
                "locked-close",
                &harness.workspace,
                "SessionEnd",
                "locked-close"
            )
        ),
        json!({})
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    connection.execute_batch("ROLLBACK").unwrap();
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

/// A `close`-boundary Checkpoint used to end every later reminder as well as the Episode.
///
/// From the turn after the close, every automated boundary resolves to
/// `Closed { newly_closed: false }`, which reached neither the reminder text nor the reminder
/// gate. Two replays measured the cost: a 21.8h Cursor Session ended with
/// `checkpoint_reminder_count = 0` against 429 recorded actions, and ten commits landed after its
/// Checkpoint with nothing recorded about any of them.
///
/// The revival is bounded by the same two conditions the gate already applied — real tool activity
/// since the last reminder or Checkpoint, and at most `CHECKPOINT_REMINDER_LIMIT` of them — and it
/// never brings back the closure receipt R2-3 removed.
#[test]
#[allow(clippy::too_many_lines)]
fn work_continuing_after_a_closed_episode_earns_capped_reminders_again() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let session = "codex-continued-work";
    harness.activate("codex", session);
    let (locator, episode) = open_checkpoint(&runtime, "codex", session);
    let raw = "CONTINUED_WORK";
    let stop = codex_event(session, &harness.workspace, "Stop", raw);

    // The closing turn still reports the closure exactly once.
    let closed = harness.hook("codex", &stop);
    assert!(
        closed["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("durably closed")),
        "{closed:#}"
    );
    assert!(matches!(
        runtime
            .read_work_episode(episode)
            .unwrap()
            .unwrap()
            .episode
            .status,
        WorkEpisodeStatus::Closed { .. }
    ));

    // Idle turns after the close stay exactly as silent as R2-3 (A8) made them: the Checkpoint
    // answered the question, and nothing has happened since to reopen it.
    for _ in 0..3 {
        assert_eq!(harness.hook("codex", &stop), json!({}));
    }
    assert_eq!(runtime.checkpoint_reminder_activity(&locator), 0);

    // Work resumes. Now the Session has something uncheckpointed again, and the boundary says so.
    let mut reminders = 0;
    for round in 0..4 {
        harness.hook(
            "codex",
            &codex_event(
                session,
                &harness.workspace,
                "PostToolUse",
                &format!("{raw}_{round}"),
            ),
        );
        assert!(runtime.checkpoint_reminder_activity(&locator) > 0);
        let response = harness.hook("codex", &stop);
        if response == json!({}) {
            continue;
        }
        reminders += 1;
        assert_flat_checkpoint_guidance(&response);
        let message = response["systemMessage"].as_str().unwrap();
        assert!(
            message.contains("kept working since"),
            "the revived reminder must name the work that is uncheckpointed: {message}"
        );
        assert!(
            !message.contains("durably closed"),
            "R2-3 removed the repeated closure receipt and it must stay removed: {message}"
        );
        // A turn with no activity of its own earns nothing, even inside the budget.
        assert_eq!(harness.hook("codex", &stop), json!({}));
    }
    assert_eq!(
        reminders, 3,
        "the revived reminder shares the gate's per-Session budget"
    );
    // Spent budget returns the branch to silence rather than substituting a sentence to repeat.
    harness.hook(
        "codex",
        &codex_event(
            session,
            &harness.workspace,
            "PostToolUse",
            "CONTINUED_WORK_LAST",
        ),
    );
    assert_eq!(harness.hook("codex", &stop), json!({}));
}
