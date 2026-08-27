use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_domain::{
    Applicability, ContextKind, ExternalSessionLocator, NormalizedBreadcrumbKind,
    NormalizedWorkObservation, TaskId, WorkSourceRef, WorkingIntentSnapshot,
};
use sctx_git_store::GitStore;
use sctx_local_state::{
    CaptureClaim, CaptureDiagnosticKind, CaptureStore, UserConfigStore, map_capture_artifacts,
};
use sctx_mcp::{
    CandidateGetInput, TaskCheckpointBoundary, TaskCheckpointClaimInput,
    TaskCheckpointEvidenceInput, TaskCheckpointInput, candidate_get_at_root,
    task_checkpoint_at_root,
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

    fn mcp(&self, requests: &[Value]) -> Vec<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["mcp", "serve", "--client", "codex"])
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.as_mut().unwrap();
        for request in requests {
            stdin
                .write_all(&serde_json::to_vec(request).unwrap())
                .unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[allow(clippy::needless_pass_by_value)]
fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[allow(clippy::needless_pass_by_value)]
fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    rpc(
        id,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
}

fn initialize(id: u64) -> Value {
    rpc(id, "initialize", json!({"protocolVersion": "2024-11-05"}))
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

fn session_start(session_id: &str, cwd: &Path) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
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
            CaptureDiagnosticKind::NoActiveTask
            | CaptureDiagnosticKind::IntentBootstrapRequired
            | CaptureDiagnosticKind::RuntimeUnavailable => None,
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn hook_capture_keeps_locator_then_explicit_claim_and_ingestion_are_verifiable() {
    let harness = Harness::new();
    GitStore::bootstrap_local(harness.root()).unwrap();
    let cross = harness.home.join("cross workspace");
    let repository = git_repo(&cross.join("fe/repo"));
    let sibling = git_repo(&cross.join("unconfigured/repo"));
    let cross = fs::canonicalize(cross).unwrap();
    UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    assert!(
        harness.hook(&session_start("capture-before-task", &repository))["hookSpecificOutput"]
            ["additionalContext"]
            .as_str()
            .is_some_and(|message| message.contains("<shared-context-active>"))
    );
    assert!(
        harness.hook(&session_start("capture-owned", &repository))["hookSpecificOutput"]
            ["additionalContext"]
            .as_str()
            .is_some_and(|message| message.contains("<shared-context-active>"))
    );

    let no_task_raw = "RAW_BEFORE_TASK_MUST_NOT_PERSIST";
    assert_eq!(
        harness.hook(&post_tool(
            "capture-before-task",
            &cross,
            &repository.join("src/feature.rs"),
            no_task_raw,
            "Inspect",
        )),
        json!({
            "systemMessage": "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing."
        })
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

    let captures_before_sibling = store.list(32).unwrap().captures;
    let capture_ids_before_sibling = captures_before_sibling
        .iter()
        .map(|capture| capture.record.capture_id)
        .collect::<BTreeSet<_>>();
    let capture_count_before_sibling = captures_before_sibling.len();
    assert_eq!(
        harness.hook(&post_tool(
            "capture-owned",
            &cross,
            &sibling.join("src/feature.rs"),
            "RAW_UNCONFIGURED_PATH",
            "SiblingInspect",
        )),
        json!({})
    );
    let captures_after_sibling = store.list(32).unwrap().captures;
    assert_eq!(
        captures_after_sibling.len(),
        capture_count_before_sibling + 1,
        "safe unregistered work must preserve one non-locating Capture"
    );
    let sibling_capture = captures_after_sibling
        .iter()
        .find(|capture| !capture_ids_before_sibling.contains(&capture.record.capture_id))
        .unwrap();
    assert_eq!(sibling_capture.record.task_owner, Some(task_owner));
    assert!(sibling_capture.record.workspace_hint.is_none());
    assert!(sibling_capture.record.file_hints.is_empty());
    let sibling_record = fs::read_to_string(
        store
            .directory()
            .join(format!("{}.json", sibling_capture.record.capture_id)),
    )
    .unwrap();
    for forbidden in [
        "RAW_UNCONFIGURED_PATH",
        sibling.to_str().unwrap(),
        "transcript_path",
        "tool_response",
        "command",
    ] {
        assert!(!sibling_record.contains(forbidden));
    }

    assert!(
        store
            .claim(sibling_capture.record.capture_id, claim)
            .unwrap()
            .newly_claimed
    );
    let sibling_claimed = store
        .read(sibling_capture.record.capture_id)
        .unwrap()
        .record;
    let sibling_mapping = map_capture_artifacts(&sibling_claimed, &catalog);
    assert!(sibling_mapping.artifact_refs.is_empty());
    assert!(sibling_mapping.diagnostics.is_empty());
    let sibling_ingested = runtime
        .ingest_capture(&CaptureIngestion {
            capture_id: sibling_claimed.capture_id,
            episode_id: opened.episode.episode_id,
            expected_episode_version: 1,
            task_session_id: active.task_session_id,
            task_id: active.task_id,
            intent_revision_id: task_owner.intent_revision_id,
            additional_sources: Vec::new(),
            observation: NormalizedWorkObservation::Breadcrumb {
                category: NormalizedBreadcrumbKind::Exploration,
                summary: sibling_claimed.summary.clone(),
            },
            diagnostics: Vec::new(),
        })
        .unwrap();
    assert!(sibling_ingested.inserted);
    let observation = sibling_ingested
        .episode
        .episode
        .observations
        .iter()
        .find(|observation| observation.observation_id == sibling_ingested.observation_id)
        .unwrap();
    assert_eq!(
        observation.source_refs,
        vec![WorkSourceRef::Capture(sctx_domain::CaptureSourceRef {
            capture_id: sibling_claimed.capture_id,
            task_session_id: active.task_session_id,
            task_id: active.task_id,
        })]
    );
    let observation_text = serde_json::to_string(observation).unwrap();
    assert!(!observation_text.contains(sibling.to_str().unwrap()));
    assert!(!observation_text.contains("RAW_UNCONFIGURED_PATH"));

    let checkpoint = task_checkpoint_at_root(
        harness.root(),
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "capture-owned".to_owned(),
            expected_task_id: active.task_id.to_string(),
            expected_intent_revision_id: task_owner.intent_revision_id.to_string(),
            expected_episode_version: 2,
            boundary: TaskCheckpointBoundary::Close,
            claims: vec![TaskCheckpointClaimInput {
                context_kind_hint: Some(ContextKind::Validation),
                topic_key_hint: None,
                statement:
                    "The unregistered investigation completed with bounded non-locating meaning"
                        .to_owned(),
                rationale: "The Agent explicitly cites its owned normalized Observation".to_owned(),
                applicability: Applicability {
                    domains: vec!["testing".to_owned()],
                    platforms: Vec::new(),
                    conditions: vec!["target Repository remains unregistered".to_owned()],
                },
                assumptions: Vec::new(),
                recheck_when: vec!["the target Repository is registered".to_owned()],
                evidence: vec![TaskCheckpointEvidenceInput::Observation {
                    observation_id: sibling_ingested.observation_id.to_string(),
                }],
                artifact_refs: Vec::new(),
                relations: Vec::new(),
                engineering_references: Vec::new(),
                related_contexts: Vec::new(),
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap();
    let candidate_id = checkpoint.candidate_build.as_ref().unwrap().items[0]
        .candidate_id
        .unwrap();
    let review = candidate_get_at_root(
        harness.root(),
        &CandidateGetInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "capture-owned".to_owned(),
            candidate_id: candidate_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(review.content.evidence.len(), 1);
    assert_eq!(
        review.content.evidence[0].limitations,
        vec![
            "Only normalized engineering meaning is preserved; the raw Capture payload is excluded"
                .to_owned()
        ]
    );
    let review_text = serde_json::to_string(&review).unwrap();
    let configured_repository_id = catalog
        .resolve_declared_path(&repository.join("src/feature.rs"))
        .unwrap()
        .repository_id;
    let configured_repository_id = configured_repository_id.to_string();
    for forbidden in [
        "RAW_UNCONFIGURED_PATH",
        sibling.to_str().unwrap(),
        configured_repository_id.as_str(),
        "artifact_refs",
    ] {
        assert!(
            !review_text.contains(forbidden),
            "Candidate leaked {forbidden:?}"
        );
    }
    let verified = runtime
        .verify_source_episode(opened.episode.episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(verified.observation_count, 2);
    assert!(
        runtime
            .read_work_episode(opened.episode.episode_id)
            .unwrap()
            .unwrap()
            .diagnostics
            .is_empty()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_mcp_lists_and_ingests_owned_capture_into_candidate_evidence() {
    let harness = Harness::new();
    GitStore::bootstrap_local(harness.root()).unwrap();
    let repository = git_repo(&harness.home.join("public capture/repo"));
    UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    let session = "public-capture";
    assert!(
        harness.hook(&session_start(session, &repository))["hookSpecificOutput"]
            ["additionalContext"]
            .as_str()
            .is_some_and(|message| message.contains("<shared-context-active>"))
    );

    let started = harness.mcp(&[
        initialize(1),
        tool_call(
            2,
            "task_intent_update",
            json!({
                "agent_kind": "codex",
                "external_session_id": session,
                "task_boundary": "new",
                "expected_revision_id": null,
                "intent": {"goal": "Consume one real Hook Capture through public MCP"}
            }),
        ),
    ]);
    let task = &started[1]["result"]["structuredContent"];
    let task_id = task["task_id"].as_str().unwrap();
    let intent_revision_id = task["intent_revision_id"].as_str().unwrap();

    let raw = "RAW_PUBLIC_CAPTURE_PAYLOAD_MUST_NOT_PERSIST";
    assert_eq!(
        harness.hook(&post_tool(
            session,
            &repository,
            &repository.join("src/feature.rs"),
            raw,
            "Inspect",
        )),
        json!({})
    );
    let listed = harness.mcp(&[
        initialize(3),
        tool_call(
            4,
            "task_capture_list",
            json!({
                "agent_kind": "codex",
                "external_session_id": session,
                "limit": 10
            }),
        ),
    ]);
    let captures = listed[1]["result"]["structuredContent"]["captures"]
        .as_array()
        .unwrap();
    assert_eq!(captures.len(), 1);
    let capture_id = captures[0]["capture_id"].as_str().unwrap();
    let listed_text = serde_json::to_string(&listed[1]).unwrap();
    assert!(!listed_text.contains(raw));
    assert!(!listed_text.contains(repository.to_str().unwrap()));

    let checkpoint_arguments = json!({
        "agent_kind": "codex",
        "external_session_id": session,
        "expected_task_id": task_id,
        "expected_intent_revision_id": intent_revision_id,
        "expected_episode_version": 0,
        "boundary": "close",
        "claims": [{
            "context_kind_hint": "validation",
            "statement": "A real Hook Capture can support a reviewed Candidate",
            "rationale": "The Agent explicitly selected its owned normalized Capture",
            "applicability": {"domains": ["capture"], "platforms": [], "conditions": []},
            "assumptions": [],
            "recheck_when": ["the Capture evidence contract changes"],
            "evidence": [{"kind": "capture", "capture_id": capture_id}],
            "artifact_refs": [],
            "related_contexts": []
        }],
        "unknowns": []
    });
    let mut invalid_checkpoint = checkpoint_arguments.clone();
    invalid_checkpoint["claims"][0]["artifact_refs"] = json!([{
        "repository_id": "Ghost",
        "locator": {"locator_kind": "file", "path": "src/feature.rs"}
    }]);
    let invalid = harness.mcp(&[
        initialize(5),
        tool_call(6, "task_checkpoint", invalid_checkpoint),
        tool_call(
            7,
            "task_capture_list",
            json!({"agent_kind": "codex", "external_session_id": session}),
        ),
    ]);
    assert_eq!(invalid[1]["result"]["isError"], true);
    assert_eq!(
        invalid[2]["result"]["structuredContent"]["captures"][0]["capture_id"],
        capture_id
    );

    let checkpoint = harness.mcp(&[
        initialize(8),
        tool_call(9, "task_checkpoint", checkpoint_arguments.clone()),
    ]);
    let checkpoint = &checkpoint[1]["result"]["structuredContent"];
    assert_eq!(checkpoint["created"], true);
    assert_eq!(checkpoint["episode_version"], 2);
    assert_eq!(checkpoint["diagnostics"][0]["kind"], "capture_ingested");
    assert_eq!(checkpoint["diagnostics"][0]["capture_id"], capture_id);
    assert_eq!(checkpoint["diagnostics"][0]["inserted"], true);
    let candidate_id = checkpoint["candidate_build"]["items"][0]["candidate_id"]
        .as_str()
        .unwrap();

    let after = harness.mcp(&[
        initialize(10),
        tool_call(
            11,
            "task_capture_list",
            json!({"agent_kind": "codex", "external_session_id": session}),
        ),
        tool_call(12, "task_checkpoint", checkpoint_arguments.clone()),
        tool_call(
            13,
            "candidate_get",
            json!({
                "agent_kind": "codex",
                "external_session_id": session,
                "candidate_id": candidate_id
            }),
        ),
    ]);
    assert!(
        after[1]["result"]["structuredContent"]["captures"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(after[2]["result"]["structuredContent"]["created"], false);
    assert_eq!(
        after[2]["result"]["structuredContent"]["diagnostics"][0]["inserted"],
        false
    );
    let review = &after[3]["result"]["structuredContent"];
    assert_eq!(review["content"]["evidence"].as_array().unwrap().len(), 1);
    let review_text = serde_json::to_string(review).unwrap();
    assert!(review_text.contains("Only normalized engineering meaning is preserved"));
    assert!(!review_text.contains(raw));
    assert!(!review_text.contains(repository.to_str().unwrap()));

    let switched = harness.mcp(&[
        initialize(14),
        tool_call(
            15,
            "task_intent_update",
            json!({
                "agent_kind": "codex",
                "external_session_id": session,
                "task_boundary": "new",
                "expected_revision_id": intent_revision_id,
                "intent": {"goal": "Start a distinct Task after Capture ingestion"}
            }),
        ),
    ]);
    let next_task = &switched[1]["result"]["structuredContent"];
    let mut cross_task = checkpoint_arguments;
    cross_task["expected_task_id"] = next_task["task_id"].clone();
    cross_task["expected_intent_revision_id"] = next_task["intent_revision_id"].clone();
    let rejected = harness.mcp(&[
        initialize(16),
        tool_call(
            17,
            "task_capture_list",
            json!({"agent_kind": "codex", "external_session_id": session}),
        ),
        tool_call(18, "task_checkpoint", cross_task),
    ]);
    assert!(
        rejected[1]["result"]["structuredContent"]["captures"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(rejected[2]["result"]["isError"], true);
    assert_eq!(
        rejected[2]["result"]["structuredContent"]["error"]["code"],
        "invalid_input"
    );
}
