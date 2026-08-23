use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_domain::{
    ExternalSessionLocator, NormalizedBreadcrumbKind, NormalizedWorkObservation, TaskId,
    WorkSourceRef, WorkingIntentSnapshot,
};
use sctx_git_store::GitStore;
use sctx_local_state::{
    CaptureClaim, CaptureDiagnosticKind, CaptureStore, UserConfigStore, map_capture_artifacts,
};
use sctx_task_runtime::{CaptureIngestion, TaskRuntime, WorkEpisodeDiagnosticKind};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("capture owner home");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn hook(&self, payload: &Value) -> Value {
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
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn git_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    fs::write(path.join("src/feature.rs"), "pub fn feature() {}\n").unwrap();
    fs::canonicalize(path).unwrap()
}

fn task() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: "Persist a verifiable Work Episode".to_owned(),
        current_direction: Some("Ingest redacted Capture meaning".to_owned()),
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

fn post_tool(session_id: &str, cwd: &Path, file: &Path, raw: &str, tool: &str) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": format!("/tmp/{raw}.jsonl"),
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": format!("turn-{session_id}"),
        "tool_name": tool,
        "tool_use_id": format!("tool-{session_id}"),
        "tool_input": {"file_path": file, "command": raw},
        "tool_response": {"output": raw}
    })
}

fn runtime_diagnostics(values: &[CaptureDiagnosticKind]) -> Vec<WorkEpisodeDiagnosticKind> {
    values
        .iter()
        .filter_map(|diagnostic| match diagnostic {
            CaptureDiagnosticKind::RepositoryNotConfigured => {
                Some(WorkEpisodeDiagnosticKind::CaptureRepositoryNotConfigured)
            }
            CaptureDiagnosticKind::UnsafeArtifactPath => {
                Some(WorkEpisodeDiagnosticKind::CaptureUnsafeArtifactPath)
            }
            CaptureDiagnosticKind::NoActiveTask | CaptureDiagnosticKind::RuntimeUnavailable => None,
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn hook_capture_keeps_locator_then_explicit_claim_and_ingestion_are_verifiable() {
    let harness = Harness::new();
    GitStore::initialize(harness.root()).unwrap();
    let cross = harness.home.join("cross workspace");
    let repository = git_repo(&cross.join("fe/repo"));
    let sibling = git_repo(&cross.join("unconfigured/repo"));
    let cross = fs::canonicalize(cross).unwrap();
    UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(None, std::slice::from_ref(&repository))
        .unwrap();

    let no_task_raw = "RAW_BEFORE_TASK_MUST_NOT_PERSIST";
    assert_eq!(
        harness.hook(&post_tool(
            "capture-before-task",
            &cross,
            &repository.join("src/feature.rs"),
            no_task_raw,
            "Inspect",
        )),
        json!({})
    );
    let store = CaptureStore::initialize(harness.root()).unwrap();
    let before_task = store
        .list(32)
        .unwrap()
        .captures
        .into_iter()
        .find(|capture| {
            capture.record.external_session_locator.external_session_id == "capture-before-task"
        })
        .unwrap();
    assert!(before_task.record.task_owner.is_none());
    assert_eq!(
        before_task.record.diagnostics,
        vec![CaptureDiagnosticKind::NoActiveTask]
    );

    let locator = ExternalSessionLocator::new("codex", "capture-owned").unwrap();
    let runtime = TaskRuntime::initialize(harness.root()).unwrap();
    let active = runtime
        .open_or_create(locator.clone(), TaskId::new(), task(), Vec::new())
        .unwrap()
        .snapshot;
    let raw = "RAW_COMMAND_AND_OUTPUT_MUST_NOT_PERSIST";
    assert_eq!(
        harness.hook(&post_tool(
            "capture-owned",
            &cross,
            &repository.join("src/feature.rs"),
            raw,
            "ContractTest",
        )),
        json!({})
    );
    assert!(
        runtime
            .list_work_episodes(active.task_session_id, 10)
            .unwrap()
            .is_empty(),
        "Hook capture must not auto-open Episode #163"
    );
    let owned_capture = store
        .list(32)
        .unwrap()
        .captures
        .into_iter()
        .find(|capture| {
            capture.record.external_session_locator.external_session_id == "capture-owned"
        })
        .unwrap();
    let task_owner = owned_capture.record.task_owner.unwrap();
    assert_eq!(task_owner.task_session_id, active.task_session_id);
    assert_eq!(task_owner.task_id, active.task_id);
    let stored_bytes = fs::read_to_string(
        store
            .directory()
            .join(format!("{}.json", owned_capture.record.capture_id)),
    )
    .unwrap();
    for forbidden in [
        raw,
        no_task_raw,
        "transcript_path",
        "tool_response",
        "command",
    ] {
        assert!(!stored_bytes.contains(forbidden));
    }

    let opened = runtime
        .open_work_episode(
            &locator,
            active.task_id,
            active.current_intent_revision().unwrap().revision_id,
        )
        .unwrap()
        .episode;
    let claim = CaptureClaim {
        episode_id: opened.episode.episode_id,
        task_session_id: active.task_session_id,
        task_id: active.task_id,
    };
    assert!(
        store
            .claim(owned_capture.record.capture_id, claim)
            .unwrap()
            .newly_claimed
    );
    assert!(
        !store
            .claim(owned_capture.record.capture_id, claim)
            .unwrap()
            .newly_claimed,
        "claim-before-runtime crash retry must remain available"
    );
    let claimed = store.read(owned_capture.record.capture_id).unwrap().record;
    let catalog = UserConfigStore::open_existing(harness.root())
        .unwrap()
        .repository_catalog()
        .unwrap();
    let mapping = map_capture_artifacts(&claimed, &catalog);
    assert_eq!(mapping.artifact_refs.len(), 1);
    assert!(mapping.diagnostics.is_empty());
    let input = CaptureIngestion {
        capture_id: claimed.capture_id,
        episode_id: opened.episode.episode_id,
        expected_episode_version: 0,
        task_session_id: active.task_session_id,
        task_id: active.task_id,
        intent_revision_id: task_owner.intent_revision_id,
        additional_sources: mapping
            .artifact_refs
            .into_iter()
            .map(WorkSourceRef::Artifact)
            .collect(),
        observation: NormalizedWorkObservation::Breadcrumb {
            category: NormalizedBreadcrumbKind::Validation,
            summary: claimed.summary.clone(),
        },
        diagnostics: runtime_diagnostics(&mapping.diagnostics),
    };
    let mut interrupted = input.clone();
    interrupted.expected_episode_version = 99;
    assert!(runtime.ingest_capture(&interrupted).is_err());
    assert!(
        store
            .read(claimed.capture_id)
            .unwrap()
            .record
            .claim
            .is_some(),
        "failed runtime commit must leave the claimed Capture retryable"
    );
    let ingested = runtime.ingest_capture(&input).unwrap();
    assert!(ingested.inserted);
    assert!(!runtime.ingest_capture(&input).unwrap().inserted);

    assert_eq!(
        harness.hook(&post_tool(
            "capture-owned",
            &cross,
            &sibling.join("src/feature.rs"),
            "RAW_UNCONFIGURED_PATH",
            "Inspect",
        )),
        json!({})
    );
    let unconfigured = store
        .list(32)
        .unwrap()
        .captures
        .into_iter()
        .find(|capture| {
            capture.record.external_session_locator.external_session_id == "capture-owned"
                && capture.record.capture_id != claimed.capture_id
        })
        .unwrap()
        .record;
    assert!(
        store
            .claim(
                unconfigured.capture_id,
                CaptureClaim {
                    episode_id: opened.episode.episode_id,
                    task_session_id: active.task_session_id,
                    task_id: active.task_id,
                },
            )
            .unwrap()
            .newly_claimed
    );
    let mapping = map_capture_artifacts(&unconfigured, &catalog);
    assert!(mapping.artifact_refs.is_empty());
    assert_eq!(
        mapping.diagnostics,
        vec![CaptureDiagnosticKind::RepositoryNotConfigured]
    );
    let second = runtime
        .ingest_capture(&CaptureIngestion {
            capture_id: unconfigured.capture_id,
            episode_id: opened.episode.episode_id,
            expected_episode_version: 1,
            task_session_id: active.task_session_id,
            task_id: active.task_id,
            intent_revision_id: task_owner.intent_revision_id,
            additional_sources: Vec::new(),
            observation: NormalizedWorkObservation::Breadcrumb {
                category: NormalizedBreadcrumbKind::Exploration,
                summary: unconfigured.summary,
            },
            diagnostics: runtime_diagnostics(&mapping.diagnostics),
        })
        .unwrap();
    assert_eq!(second.episode.episode.observations.len(), 2);
    assert_eq!(second.episode.diagnostics.len(), 1);
    let verified = runtime
        .verify_source_episode(opened.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(verified.observation_count, 2);
}
