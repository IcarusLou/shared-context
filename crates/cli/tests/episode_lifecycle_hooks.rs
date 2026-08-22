use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use sctx_domain::{
    Applicability, EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, TaskId, TaskIntent,
    WorkEpisodeStatus,
};
use sctx_git_store::GitStore;
use sctx_index::ProjectionIndex;
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
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("episode lifecycle home");
        let root = home.join(".shared-context");
        fs::create_dir_all(&home).unwrap();
        GitStore::initialize(&root).unwrap();
        UserConfigStore::initialize(&root).unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
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
}

fn intent(task_id: TaskId, session: &str) -> TaskIntent {
    TaskIntent {
        task_id,
        goal: format!("solidify {session} engineering conclusions"),
        desired_change: "close only explicitly checkpointed cognition".to_owned(),
        in_scope: vec!["Agent lifecycle automation".to_owned()],
        out_of_scope: Vec::new(),
        domains: vec!["capture".to_owned()],
        platforms: Vec::new(),
        constraints: Vec::new(),
        acceptance_conditions: vec!["one closed Episode produces one Candidate".to_owned()],
        artifacts: Vec::new(),
        interfaces: Vec::new(),
        unknowns: Vec::new(),
    }
}

fn checkpoint_claim(session: &str) -> CheckpointClaimDraft {
    CheckpointClaimDraft {
        context_kind_hint: None,
        topic_key_hint: Some(format!("capture/{session}")),
        statement: format!("{session} keeps lifecycle automation idempotent"),
        rationale: "A direct validation records the expected lifecycle result".to_owned(),
        applicability: Applicability {
            domains: vec!["capture".to_owned()],
            platforms: Vec::new(),
            conditions: vec![session.to_owned()],
        },
        assumptions: Vec::new(),
        recheck_when: vec!["the Agent lifecycle contract changes".to_owned()],
        evidence_refs: Vec::new(),
        inline_validations: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: format!("{session} lifecycle behavior"),
            content: json!({"actual": "passed", "session": session}),
            interpretation: "The persisted Checkpoint is sufficient for a Candidate draft"
                .to_owned(),
            limitations: Vec::new(),
        }],
        artifact_refs: Vec::new(),
        related_contexts: Vec::new(),
    }
}

fn open_checkpoint(
    runtime: &TaskRuntime,
    agent: &str,
    session: &str,
) -> (ExternalSessionLocator, sctx_domain::WorkEpisodeId) {
    let locator = ExternalSessionLocator::new(agent, session).unwrap();
    let snapshot = runtime
        .open_or_create(locator.clone(), intent(TaskId::new(), session), Vec::new())
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
    assert_eq!(cursor_output, json!({}));

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
    let index = ProjectionIndex::for_store(&GitStore::initialize(&harness.root).unwrap());
    let before = index.domain_snapshot().unwrap().projection.candidates.len();
    assert_eq!(before, 2);
    let started = Instant::now();
    for _ in 0..8 {
        let retry = harness.hook(
            "codex",
            &codex_event("codex-lifecycle", &harness.home, "Stop", raw_marker),
        );
        assert!(
            retry["systemMessage"]
                .as_str()
                .is_some_and(|message| message.contains("durably closed"))
        );
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        index.domain_snapshot().unwrap().projection.candidates.len(),
        before,
        "duplicate lifecycle events must not create duplicate Candidates"
    );
    assert!(!files_contain(&harness.root, raw_marker));
}

#[test]
fn out_of_order_stop_requires_checkpoint_and_session_end_never_closes_or_builds() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let session = "out-of-order";
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    let snapshot = runtime
        .open_or_create(locator, intent(TaskId::new(), session), Vec::new())
        .unwrap()
        .snapshot;
    let stop = harness.hook(
        "codex",
        &codex_event(session, &harness.home, "Stop", "RAW_OUT_OF_ORDER"),
    );
    assert!(
        stop["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("no Work Episode is open"))
    );
    assert!(
        runtime
            .list_work_episodes(snapshot.task_session_id, 10)
            .unwrap()
            .is_empty()
    );

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

#[test]
fn concurrent_turn_stop_processes_converge_on_one_build_and_candidate() {
    let harness = Arc::new(Harness::new());
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
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
    assert!(outputs.iter().all(|output| {
        output["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("durably closed"))
    }));
    let build = runtime.read_candidate_build(episode_id).unwrap().unwrap();
    assert_eq!(build.status, CandidateBuildStatus::Complete);
    assert_eq!(build.items.len(), 1);
    let index = ProjectionIndex::for_store(&GitStore::initialize(&harness.root).unwrap());
    assert_eq!(
        index.domain_snapshot().unwrap().projection.candidates.len(),
        1
    );
}

#[test]
fn turn_stop_fails_open_after_close_and_later_retry_recovers_builder() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
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
            claims: vec![checkpoint_claim("builder-recovery-newer")],
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
}

#[test]
fn lifecycle_boundary_does_not_depend_on_noncritical_breadcrumb_storage() {
    let harness = Harness::new();
    let runtime = TaskRuntime::initialize(&harness.root).unwrap();
    let (_, episode_id) = open_checkpoint(&runtime, "codex", "capture-unavailable");
    let capture = harness.root.join("state/capture");
    if capture.exists() {
        fs::remove_dir_all(&capture).unwrap();
    }
    fs::write(&capture, b"capture path intentionally unavailable").unwrap();
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
}
