use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    str::FromStr,
    sync::{Arc, Barrier},
    thread,
};

use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_domain::{
    Applicability, CaptureUnknown, ContextKind, ContextRevisionDraft, Error, ErrorKind, EventId,
    EvidenceSnapshotDraft, ExternalSessionLocator, IntentSnapshot, PublicationAction,
    PublicationDraft, Result, SpaceId, SubmissionId, TaskSignalKind, WorkEpisodeId,
    WorkingIntentSnapshot,
};
use sctx_engineering_graph::RepositoryRegistry;
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{
    AppendRequest, CandidateSubmissionRequest, CrashInjector, CrashSeam, GitStore,
};
use sctx_index::ProjectionIndex;
use sctx_local_state::{MaintenanceLock, UserConfigStore};
use sctx_mcp::{
    ExpectedRevisionId, TaskBoundary, TaskCheckpointBoundary, TaskCheckpointClaimInput,
    TaskCheckpointEvidenceInput, TaskCheckpointInput, TaskIntentUpdateInput,
    task_checkpoint_at_root, task_intent_update_at_root,
};
use sctx_task_runtime::TaskRuntime;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

const EVIDENCE: &str = r#"{"kind":"experiment_record","supports":"CLI command completed","content":{"command":"contract"},"interpretation":"the contract is executable","limitations":["synthetic CLI fixture"]}"#;

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
}

#[test]
fn business_cli_returns_typed_busy_while_exclusive_maintenance_is_active() {
    let harness = Harness::new();
    let store = GitStore::initialize(harness.root()).unwrap();
    let before = git_output(store.repository(), &["rev-parse", "HEAD"]);
    let maintenance = MaintenanceLock::open_or_create(harness.root()).unwrap();
    let exclusive = maintenance.try_exclusive().unwrap();

    let busy = harness.failure(&["repository", "list"]);
    assert_eq!(busy["error"]["code"], "maintenance_busy");
    assert_eq!(
        git_output(store.repository(), &["rev-parse", "HEAD"]),
        before
    );

    drop(exclusive);
    let listed = harness.success(&["repository", "list"]);
    assert!(
        listed["data"]["repositories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("用户 home 空格");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sctx"))
            .arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("sctx should start")
    }

    fn run_with_input(&self, args: &[&str], input: &Value) -> Output {
        self.run_with_input_env(args, input, &[])
    }

    fn run_with_input_env(
        &self,
        args: &[&str],
        input: &Value,
        environment: &[(&str, &Path)],
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
        command.args(args).env("HOME", &self.home);
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sctx hook should start");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&serde_json::to_vec(input).unwrap())
            .unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value["tree"].as_str().is_some_and(|tree| !tree.is_empty()));
        assert!(value["generation"].as_u64().is_some());
        value
    }

    fn failure(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "command unexpectedly succeeded: {args:?}"
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn repository(&self) -> PathBuf {
        self.home.join(".shared-context/repository")
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn head(&self) -> String {
        git_output(&self.repository(), &["rev-parse", "HEAD"])
    }

    fn event_count(&self) -> usize {
        let output = git_output(
            &self.repository(),
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        output
            .lines()
            .filter(|path| path.starts_with("events/"))
            .count()
    }
}

#[allow(clippy::struct_field_names)]
struct Published {
    context_id: String,
    revision_id: String,
    review_event_id: String,
    publication_id: String,
}

fn create_space(harness: &Harness, title: &str) -> (String, String) {
    let value = harness.success(&[
        "space",
        "create",
        "--title",
        title,
        "--problem",
        "cold start",
        "--desired-outcome",
        "shared knowledge",
        "--in-scope",
        "CLI",
        "--acceptance-condition",
        "works",
    ]);
    (
        text(&value, "space_id").to_owned(),
        text(&value, "revision_id").to_owned(),
    )
}

fn seed_context(harness: &Harness, space_id: &str, statement: &str) -> (String, String) {
    let event = Event::context_revision_added(
        SpaceId::from_str(space_id).unwrap(),
        ContextRevisionDraft {
            kind: ContextKind::Decision,
            topic_key: Some("cli/output".to_owned()),
            statement: statement.to_owned(),
            rationale: "stable clients".to_owned(),
            applicability: Applicability {
                domains: vec!["cli".to_owned()],
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
    let (context_id, revision_id) = match event.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    };
    GitStore::initialize(harness.root())
        .unwrap()
        .append_event(AppendRequest::event(event))
        .unwrap();
    (context_id.to_string(), revision_id.to_string())
}

#[allow(clippy::struct_field_names)]
struct CandidateOwner {
    source_episode_id: WorkEpisodeId,
}

fn closed_candidate_owner(harness: &Harness, session: &str) -> CandidateOwner {
    let task = task_intent_update_at_root(
        harness.root(),
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot {
                goal: "create a verified Candidate".to_owned(),
                current_direction: Some("record one unassigned Candidate".to_owned()),
                in_scope: vec![],
                out_of_scope: vec![],
                domains: vec![],
                platforms: vec![],
                constraints: vec![],
                acceptance_conditions: vec![],
                artifact_hints: vec![],
                interface_hints: vec![],
                open_questions: vec![],
            },
        },
    )
    .unwrap();
    let closed = task_checkpoint_at_root(
        harness.root(),
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            expected_episode_version: 0,
            boundary: TaskCheckpointBoundary::Close,
            claims: Vec::new(),
            unknowns: vec![CaptureUnknown {
                statement: "Candidate confirmation remains outside creation".to_owned(),
                blocking: false,
                recheck_when: Vec::new(),
            }],
        },
    )
    .unwrap();
    CandidateOwner {
        source_episode_id: closed.episode_id,
    }
}

fn submit_git_only_candidate(
    harness: &Harness,
    submission_id: SubmissionId,
    owner: &CandidateOwner,
    statement: &str,
) -> String {
    let tasks = TaskRuntime::initialize(harness.root()).unwrap();
    let source_episode = tasks
        .verify_source_episode(owner.source_episode_id)
        .unwrap()
        .unwrap()
        .ownership;
    let base = GitStore::initialize(harness.root()).unwrap();
    let index = ProjectionIndex::for_store(&base);
    base.with_candidate_submission_index(Arc::new(index))
        .submit_candidate(CandidateSubmissionRequest {
            submission_id,
            source_episode,
            content: ContextRevisionDraft {
                kind: ContextKind::Discovery,
                topic_key: None,
                statement: statement.to_owned(),
                rationale: "the task produced governable knowledge".to_owned(),
                applicability: Applicability {
                    domains: vec!["cli".to_owned()],
                    ..Applicability::default()
                },
                assumptions: Vec::new(),
                recheck_when: Vec::new(),
                relations: Vec::new(),
                evidence: vec![EvidenceSnapshotDraft {
                    kind: sctx_domain::EvidenceType::ExperimentRecord,
                    supports: "CLI contract completed".to_owned(),
                    content: serde_json::json!({"command": "contract"}),
                    interpretation: "the contract is executable".to_owned(),
                    limitations: vec!["synthetic CLI fixture".to_owned()],
                }],
            },
        })
        .unwrap()
        .record
        .candidate_id
        .to_string()
}

fn establish_cli_task(harness: &Harness, session: &str, goal: &str, current_direction: &str) {
    task_intent_update_at_root(
        harness.root(),
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot {
                goal: goal.to_owned(),
                current_direction: Some(current_direction.to_owned()),
                in_scope: vec![],
                out_of_scope: vec![],
                domains: vec![],
                platforms: vec![],
                constraints: vec![],
                acceptance_conditions: vec![],
                artifact_hints: vec![],
                interface_hints: vec![],
                open_questions: vec![],
            },
        },
    )
    .unwrap();
}

fn approve_publish(harness: &Harness, space_id: &str, statement: &str) -> Published {
    let (context_id, revision_id) = seed_context(harness, space_id, statement);
    let review = harness.success(&[
        "context",
        "review",
        "--space-id",
        space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--verdict",
        "approve",
        "--reason",
        "verified",
    ]);
    let review_event_id = text(&review, "event_id").to_owned();
    let publication = harness.success(&[
        "context",
        "publish",
        "--space-id",
        space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--expect-no-publication-head",
        "--review-event-id",
        &review_event_id,
    ]);
    Published {
        context_id,
        revision_id,
        review_event_id,
        publication_id: text(&publication, "publication_id").to_owned(),
    }
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value["data"][field].as_str().unwrap()
}

fn git_output(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn init_cli_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    fs::canonicalize(path).unwrap()
}

#[test]
fn help_and_version_expose_the_complete_lifecycle_surface() {
    let help = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--help")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help.status.success());
    for command in [
        "setup [--demo] [--agents cursor,codex]",
        "demo",
        "doctor [--fix]",
        "upgrade [--agents cursor,codex]",
        "uninstall [--root PATH]",
        "data reset [--dry-run] [--yes]",
        "knowledge delete --confirm-path PATH",
        "space create|intent revise|list|get",
        "candidate list|get|discard|confirm|build-closed-episode|analyze",
        "context revise|review|publish|withdraw|get",
        "semantic conflict open|resolve",
        "task context",
        "repository group add|update|remove|list|doctor",
        "search",
        "pending list|commit|move-aside",
        "validate --staged",
        "hook --agent cursor|codex",
        "mcp serve --client cursor|codex",
    ] {
        assert!(stdout.contains(command), "missing help surface: {command}");
    }
    let removed_manual_command = ["candidate", " create"].concat();
    assert!(!stdout.contains(&removed_manual_command));

    let version = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("sctx {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn data_reset_requires_explicit_confirmation_before_initializing_state() {
    let harness = Harness::new();
    let rejected = harness.failure(&["data", "reset"]);
    assert_eq!(rejected["error"]["code"], "invalid_input");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--yes")
    );
    assert!(!harness.root().exists());
}

#[test]
fn hook_capabilities_require_only_minimum_versions_and_codex_trust() {
    let harness = Harness::new();
    let output = harness.run(&[
        "hook",
        "--agent",
        "codex",
        "--capabilities",
        "--agent-version",
        "0.147.0",
        "--hook-available",
        "true",
        "--trust",
        "unconfirmed",
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["mode"], "action_required");
    assert!(
        report["diagnostic"]
            .as_str()
            .unwrap()
            .starts_with("ACTION REQUIRED:")
    );
    assert_eq!(report["mcp"], true);
    assert_eq!(report["cli"], true);
    assert_eq!(report["prompt_aware_injection"], false);

    let output = harness.run(&[
        "hook",
        "--agent",
        "codex",
        "--capabilities",
        "--agent-version",
        "codex-cli 0.149.1",
        "--hook-available",
        "true",
        "--trust",
        "confirmed",
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["mode"], "verified_hooks");
    assert_eq!(report["verified_version_requirement"], ">=0.147.0");

    let output = harness.run(&[
        "hook",
        "--agent",
        "cursor",
        "--capabilities",
        "--agent-version",
        "99.0.0",
        "--hook-available",
        "true",
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["mode"], "verified_hooks");
    assert_eq!(report["verified_version_requirement"], ">=3.13.0");

    let output = harness.run(&[
        "hook",
        "--agent",
        "cursor",
        "--capabilities",
        "--agent-version",
        "3.12.99",
        "--hook-available",
        "true",
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["mode"], "mcp_cli_fallback");
    assert_eq!(report["session_start"], false);
    assert!(
        report["diagnostic"]
            .as_str()
            .unwrap()
            .contains("MCP + CLI fallback")
    );
}

#[test]
fn disabled_session_start_and_prompt_submit_are_agent_neutral() {
    let harness = Harness::new();
    let (alpha_space_id, _) = create_space(&harness, "Alpha Hook contract");
    let alpha = approve_publish(&harness, &alpha_space_id, "alpha needle accepted context");
    let (beta_space_id, _) = create_space(&harness, "Beta Hook contract");
    let beta = approve_publish(&harness, &beta_space_id, "beta decoy accepted context");
    let candidate_owner = closed_candidate_owner(&harness, "hook-candidate-fixture");
    let candidate_id = submit_git_only_candidate(
        &harness,
        SubmissionId::new(),
        &candidate_owner,
        "alpha needle candidate $(touch /tmp/SCTX_MUST_NOT_EXECUTE)",
    );
    assert_ne!(alpha.context_id, candidate_id);
    assert_ne!(beta.context_id, candidate_id);

    let workspace = harness.home.join("business workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session_start = serde_json::json!({
        "session_id": "thr_contract",
        "transcript_path": null,
        "cwd": workspace,
        "hook_event_name": "SessionStart",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "source": "startup"
    });
    let output = harness.run_with_input(
        &["hook", "--agent", "codex", "--agent-version", "0.147.0"],
        &session_start,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let response_text = serde_json::to_string(&response).unwrap();
    assert_eq!(response, serde_json::json!({}));
    assert!(!response_text.contains("alpha needle accepted context"));
    assert!(!response_text.contains("beta decoy accepted context"));
    assert!(!response_text.contains("untrusted-data"));

    let payload = serde_json::json!({
        "session_id": "thr_contract",
        "transcript_path": null,
        "cwd": workspace,
        "hook_event_name": "UserPromptSubmit",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": "turn_contract",
        "prompt": "alpha needle"
    });
    let output = harness.run_with_input(
        &["hook", "--agent", "codex", "--agent-version", "0.147.0"],
        &payload,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response, serde_json::json!({}));
    assert!(
        TaskRuntime::initialize(harness.root())
            .unwrap()
            .read_snapshot_by_locator(
                &ExternalSessionLocator::new("codex", "thr_contract").unwrap()
            )
            .unwrap()
            .is_none()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn codex_dynamic_task_sessions_isolate_prompts_files_and_updated_signal_lifecycle() {
    let harness = Harness::new();
    let (alpha_space_id, _) = create_space(&harness, "alphaquartz");
    approve_publish(
        &harness,
        &alpha_space_id,
        "alphaquartz src/alpha_feature.rs AlphaContractTest succeeded",
    );
    let (beta_space_id, _) = create_space(&harness, "betacobalt");
    approve_publish(
        &harness,
        &beta_space_id,
        "betacobalt src/beta_feature.rs BetaContractTest succeeded",
    );

    let workspace = harness.home.join("repo-9x7");
    fs::create_dir_all(workspace.join("src")).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let alpha_file = workspace.join("src/alpha_feature.rs");
    let beta_file = workspace.join("src/beta_feature.rs");
    let outside_file = harness.home.join("outside.rs");
    fs::write(&alpha_file, "pub fn alpha() {}\n").unwrap();
    fs::write(&beta_file, "pub fn beta() {}\n").unwrap();
    fs::write(&outside_file, "pub fn outside() {}\n").unwrap();
    let git = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&workspace)
        .status()
        .unwrap();
    assert!(git.success());
    let configured = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "FE",
        "--path",
        workspace.to_str().unwrap(),
    ]);
    let configured_repository_id = configured["data"]["catalog"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(configured_repository_id, "FE");

    let prompt = |session_id: &str, text: &str| {
        serde_json::json!({
            "session_id": session_id,
            "transcript_path": null,
            "cwd": workspace,
            "hook_event_name": "UserPromptSubmit",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": format!("turn-{session_id}"),
            "prompt": text
        })
    };
    let hook = |payload: &Value| {
        let output = harness.run_with_input(
            &["hook", "--agent", "codex", "--agent-version", "0.147.0"],
            payload,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let start = |session_id: &str, cwd: &Path| {
        hook(&serde_json::json!({
            "session_id": session_id,
            "transcript_path": null,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "source": "startup"
        }))
    };
    let intent_update =
        |session_id: &str, goal: &str, expected: Option<String>| TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session_id.to_owned(),
            task_boundary: if expected.is_some() {
                TaskBoundary::Continue
            } else {
                TaskBoundary::New
            },
            expected_revision_id: expected
                .map_or(ExpectedRevisionId::Null(()), ExpectedRevisionId::Revision),
            intent: WorkingIntentSnapshot {
                goal: goal.to_owned(),
                current_direction: Some(format!("Implement {goal}")),
                in_scope: vec![],
                out_of_scope: vec![],
                domains: vec![],
                platforms: vec![],
                constraints: vec![],
                acceptance_conditions: vec![],
                artifact_hints: vec![],
                interface_hints: vec![],
                open_questions: vec![],
            },
        };

    let before_prompt = hook(&serde_json::json!({
        "session_id": "session-without-prompt",
        "transcript_path": null,
        "cwd": workspace,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": "turn-without-prompt",
        "tool_name": "AlphaContractTest",
        "tool_use_id": "tool-without-prompt",
        "tool_input": {"file_path": alpha_file},
        "tool_response": {"output": "passed"}
    }));
    assert_eq!(before_prompt, serde_json::json!({}));
    assert!(
        TaskRuntime::initialize(harness.root())
            .unwrap()
            .read_snapshot_by_locator(
                &ExternalSessionLocator::new("codex", "session-without-prompt").unwrap(),
            )
            .unwrap()
            .is_none(),
        "PostToolUse without a Prompt must not invent a Task Session"
    );

    for response in [
        start("session-alpha", &workspace),
        start("session-beta", &workspace),
    ] {
        assert_eq!(
            response,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": SHARED_CONTEXT_ACTIVATION_MARKER
            }})
        );
    }
    assert_eq!(
        hook(&prompt("session-alpha", "alphaquartz")),
        serde_json::json!({})
    );
    assert_eq!(
        hook(&prompt("session-beta", "betacobalt")),
        serde_json::json!({})
    );
    let alpha_created = task_intent_update_at_root(
        harness.root(),
        &intent_update("session-alpha", "alphaquartz", None),
    )
    .unwrap();
    let beta_created = task_intent_update_at_root(
        harness.root(),
        &intent_update("session-beta", "betacobalt", None),
    )
    .unwrap();

    for (session_id, tool_name, file, raw_marker) in [
        (
            "session-alpha",
            "AlphaContractTest",
            &alpha_file,
            "RAW_ALPHA_MUST_NOT_PERSIST",
        ),
        (
            "session-beta",
            "BetaContractTest",
            &beta_file,
            "RAW_BETA_MUST_NOT_PERSIST",
        ),
    ] {
        let response = hook(&serde_json::json!({
            "session_id": session_id,
            "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
            "cwd": workspace,
            "hook_event_name": "PostToolUse",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": format!("turn-{session_id}"),
            "tool_name": tool_name,
            "tool_use_id": format!("tool-{session_id}"),
            "tool_input": {
                "file_path": file,
                "command": raw_marker
            },
            "tool_response": {"output": raw_marker}
        }));
        assert_eq!(response, serde_json::json!({}));
    }

    assert_eq!(
        hook(&serde_json::json!({
            "session_id": "session-alpha",
            "transcript_path": "/tmp/RAW_MIXED_TEST_MUST_NOT_PERSIST.jsonl",
            "cwd": workspace,
            "hook_event_name": "PostToolUse",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": "turn-session-alpha",
            "tool_name": "DroppedContractTest",
            "tool_use_id": "tool-mixed",
            "tool_input": {
                "file_path": alpha_file,
                "outside": {"path": outside_file},
                "missing": {"path": workspace.join("src/missing.rs")}
            },
            "tool_response": {"output": "RAW_MIXED_TEST_MUST_NOT_PERSIST"}
        })),
        serde_json::json!({})
    );

    let alpha_updated = task_intent_update_at_root(
        harness.root(),
        &intent_update(
            "session-alpha",
            "alphaquartz",
            Some(alpha_created.context.intent_revision_id.to_string()),
        ),
    )
    .unwrap();
    let beta_updated = task_intent_update_at_root(
        harness.root(),
        &intent_update(
            "session-beta",
            "betacobalt",
            Some(beta_created.context.intent_revision_id.to_string()),
        ),
    )
    .unwrap();
    let alpha_updated = serde_json::to_string(&alpha_updated.context).unwrap();
    let beta_updated = serde_json::to_string(&beta_updated.context).unwrap();
    for (pack, own_context, other_context) in [
        (alpha_updated.as_str(), "alphaquartz", "betacobalt"),
        (beta_updated.as_str(), "betacobalt", "alphaquartz"),
    ] {
        assert!(pack.contains(own_context), "missing own Context: {pack}");
        assert!(
            !pack.contains(other_context),
            "cross-session Context leak: {pack}"
        );
        assert!(!pack.contains("\"source\":\"engineering_graph\""));
    }

    let runtime = TaskRuntime::initialize(harness.root()).unwrap();
    let alpha_snapshot = runtime
        .read_snapshot_by_locator(&ExternalSessionLocator::new("codex", "session-alpha").unwrap())
        .unwrap()
        .unwrap();
    let beta_snapshot = runtime
        .read_snapshot_by_locator(&ExternalSessionLocator::new("codex", "session-beta").unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(alpha_snapshot.task_id, beta_snapshot.task_id);
    let canonical_workspace = fs::canonicalize(&workspace).unwrap();
    let registered = RepositoryRegistry::initialize(harness.root())
        .unwrap()
        .resolve_by_checkout_path(&canonical_workspace)
        .unwrap()
        .expect("ActiveTask PostToolUse must register its canonical Git Workspace root");
    assert_eq!(registered.locators.len(), 1);
    task_intent_update_at_root(
        harness.root(),
        &intent_update("session-subdir", "subdirectory task", None),
    )
    .unwrap();
    assert_eq!(
        start("session-subdir", &workspace.join("src")),
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": SHARED_CONTEXT_ACTIVATION_MARKER
        }})
    );
    let subdirectory_post = hook(&serde_json::json!({
        "session_id": "session-subdir",
        "transcript_path": null,
        "cwd": workspace.join("src"),
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": "turn-subdir",
        "tool_name": "ContractTest",
        "tool_use_id": "tool-subdir",
        "tool_input": {"file_path": alpha_file},
        "tool_response": {"output": "passed"}
    }));
    assert_eq!(subdirectory_post, serde_json::json!({}));
    let after_subdirectory = RepositoryRegistry::initialize(harness.root())
        .unwrap()
        .resolve_by_checkout_path(&fs::canonicalize(&workspace).unwrap())
        .unwrap()
        .expect("a Workspace subdirectory must resolve and refresh its Git top-level");
    assert_eq!(
        after_subdirectory.identity.repository_id,
        registered.identity.repository_id
    );
    assert_eq!(after_subdirectory.locators.len(), 1);
    for (snapshot, own_test) in [
        (&alpha_snapshot, "AlphaContractTest succeeded"),
        (&beta_snapshot, "BetaContractTest succeeded"),
    ] {
        assert!(snapshot.task_signals.iter().any(|signal| {
            signal.kind == TaskSignalKind::TestOutcome && signal.content == own_test
        }));
        assert!(!snapshot.task_signals.iter().any(|signal| {
            signal.content.contains("outside.rs") || signal.content.contains("missing.rs")
        }));
        assert!(snapshot.task_signals.iter().all(|signal| {
            !signal.content.contains("RAW_ALPHA_MUST_NOT_PERSIST")
                && !signal.content.contains("RAW_BETA_MUST_NOT_PERSIST")
                && !signal.content.contains("DroppedContractTest")
                && !signal.content.contains("RAW_MIXED_TEST_MUST_NOT_PERSIST")
        }));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn hook_catalog_mapping_never_discovers_sibling_repositories() {
    let harness = Harness::new();
    GitStore::initialize(harness.root()).unwrap();
    let siblings = harness.home.join("三个 sibling repos");
    let repositories = ["alpha", "明确 beta", "gamma"]
        .into_iter()
        .map(|name| siblings.join(name))
        .collect::<Vec<_>>();
    for repository in &repositories {
        fs::create_dir_all(repository.join("src/nested")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(repository)
                .status()
                .unwrap()
                .success()
        );
        git_output(repository, &["config", "user.name", "Sparse Graph"]);
        git_output(
            repository,
            &["config", "user.email", "sparse@example.invalid"],
        );
        fs::write(repository.join("src/target.rs"), "pub fn target() {}\n").unwrap();
        git_output(repository, &["add", "--", "."]);
        git_output(repository, &["commit", "-q", "-m", "fixture"]);
    }
    let repositories = repositories
        .into_iter()
        .map(|repository| fs::canonicalize(repository).unwrap())
        .collect::<Vec<_>>();
    let explicit = &repositories[1];
    let configured = UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(explicit),
        )
        .unwrap();
    sctx_mcp::sync_repository_catalog_at_root(harness.root()).unwrap();
    let fake_bin = harness.home.join("no-git-hot-path/bin");
    let git_sentinel = harness.home.join("no-git-hot-path/git-was-invoked");
    fs::create_dir_all(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nprintf invoked > \"$SCTX_GIT_SENTINEL\"\nexit 97\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let open_task = |session_id: &str| {
        task_intent_update_at_root(
            harness.root(),
            &TaskIntentUpdateInput {
                agent_kind: "codex".to_owned(),
                external_session_id: session_id.to_owned(),
                task_boundary: TaskBoundary::New,
                expected_revision_id: ExpectedRevisionId::Null(()),
                intent: WorkingIntentSnapshot {
                    goal: "Verify sparse Repository discovery".to_owned(),
                    current_direction: Some("Refresh only the explicit Repository".to_owned()),
                    in_scope: Vec::new(),
                    out_of_scope: Vec::new(),
                    domains: Vec::new(),
                    platforms: Vec::new(),
                    constraints: Vec::new(),
                    acceptance_conditions: Vec::new(),
                    artifact_hints: Vec::new(),
                    interface_hints: Vec::new(),
                    open_questions: Vec::new(),
                },
            },
        )
        .unwrap();
    };
    let post_tool = |session_id: &str, cwd: &Path| {
        let output = harness.run_with_input_env(
            &["hook", "--agent", "codex", "--agent-version", "0.147.0"],
            &serde_json::json!({
                "session_id": session_id,
                "transcript_path": null,
                "cwd": cwd,
                "hook_event_name": "PostToolUse",
                "model": "gpt-5.6-sol",
                "permission_mode": "default",
                "turn_id": format!("turn-{session_id}"),
                "tool_name": "SparseRepositoryTest",
                "tool_use_id": format!("tool-{session_id}"),
                "tool_input": {"file_path": explicit.join("src/target.rs")},
                "tool_response": {"output": "passed"}
            }),
            &[
                ("PATH", fake_bin.as_path()),
                ("SCTX_GIT_SENTINEL", git_sentinel.as_path()),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            serde_json::json!({})
        );
    };

    open_task("subdir-discovery");
    post_tool("subdir-discovery", &explicit.join("src/nested"));
    let registry = RepositoryRegistry::initialize(harness.root()).unwrap();
    let registered = registry.list().unwrap();
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].locators.len(), 1);
    assert_eq!(
        registered[0].locators[0].checkout_path,
        fs::canonicalize(explicit).unwrap()
    );
    for sibling in [&repositories[0], &repositories[2]] {
        assert!(
            registry
                .resolve_by_checkout_path(&fs::canonicalize(sibling).unwrap())
                .unwrap()
                .is_none(),
            "a common parent must not recursively discover sibling Repositories"
        );
    }

    let repository_id = configured.repository.repository_id;
    open_task("root-discovery");
    post_tool("root-discovery", explicit);
    let refreshed = registry.list().unwrap();
    assert_eq!(refreshed.len(), 1);
    assert_eq!(refreshed[0].identity.repository_id, repository_id);
    assert!(
        !git_sentinel.exists(),
        "PostTool Hook hot path must not launch Git"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn cross_parent_workspace_maps_three_catalog_repositories_without_cross_contamination() {
    let harness = Harness::new();
    GitStore::initialize(harness.root()).unwrap();
    let cross = harness.home.join("workspace cross");
    let repositories = [
        cross.join("fe/search_web_monorepo"),
        cross.join("android/TikTok"),
        cross.join("ios/TikTok"),
        cross.join("unconfigured/TikTok"),
    ];
    for (index, repository) in repositories.iter().enumerate() {
        fs::create_dir_all(repository.join("src/search")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(repository)
                .status()
                .unwrap()
                .success()
        );
        git_output(repository, &["config", "user.name", "Cross Catalog"]);
        git_output(
            repository,
            &["config", "user.email", "cross@example.invalid"],
        );
        fs::write(
            repository.join("src/search/Search.kt"),
            format!("// repository {index}\n"),
        )
        .unwrap();
        git_output(repository, &["add", "--", "."]);
        git_output(repository, &["commit", "-q", "-m", "fixture"]);
    }
    let cross = fs::canonicalize(cross).unwrap();
    let repositories = repositories
        .into_iter()
        .map(|repository| fs::canonicalize(repository).unwrap())
        .collect::<Vec<_>>();
    let mut configured_ids = Vec::new();
    for (repository, repository_id) in repositories[..3].iter().zip(["FE", "Android", "iOS"]) {
        let added = harness.success(&[
            "repository",
            "add",
            "--repository-id",
            repository_id,
            "--path",
            repository.to_str().unwrap(),
        ]);
        configured_ids.push(
            added["data"]["catalog"]["repository"]["repository_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(
        configured_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let open_task = |session_id: &str| {
        task_intent_update_at_root(
            harness.root(),
            &TaskIntentUpdateInput {
                agent_kind: "codex".to_owned(),
                external_session_id: session_id.to_owned(),
                task_boundary: TaskBoundary::New,
                expected_revision_id: ExpectedRevisionId::Null(()),
                intent: WorkingIntentSnapshot {
                    goal: "Resolve one configured Repository file".to_owned(),
                    current_direction: Some("Preserve stable local Repository identity".to_owned()),
                    in_scope: Vec::new(),
                    out_of_scope: Vec::new(),
                    domains: Vec::new(),
                    platforms: Vec::new(),
                    constraints: Vec::new(),
                    acceptance_conditions: Vec::new(),
                    artifact_hints: Vec::new(),
                    interface_hints: Vec::new(),
                    open_questions: Vec::new(),
                },
            },
        )
        .unwrap();
    };
    for (index, repository) in repositories.iter().take(3).enumerate() {
        let session_id = format!("cross-session-{index}");
        open_task(&session_id);
        let output = harness.run_with_input(
            &["hook", "--agent", "codex", "--agent-version", "0.147.0"],
            &serde_json::json!({
                "session_id": session_id,
                "transcript_path": null,
                "cwd": cross,
                "hook_event_name": "PostToolUse",
                "model": "gpt-5.6-sol",
                "permission_mode": "default",
                "turn_id": format!("turn-{index}"),
                "tool_name": "ReadFile",
                "tool_use_id": format!("tool-{index}"),
                "tool_input": {"file_path": repository.join("src/search/Search.kt")},
                "tool_response": {"output": "read"}
            }),
        );
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            serde_json::json!({})
        );
        let snapshot = TaskRuntime::initialize(harness.root())
            .unwrap()
            .read_snapshot_by_locator(&ExternalSessionLocator::new("codex", &session_id).unwrap())
            .unwrap()
            .unwrap();
        assert!(snapshot.task_signals.is_empty());
    }

    open_task("cross-unconfigured");
    let unconfigured = harness.run_with_input(
        &["hook", "--agent", "codex", "--agent-version", "0.147.0"],
        &serde_json::json!({
            "session_id": "cross-unconfigured",
            "transcript_path": null,
            "cwd": cross,
            "hook_event_name": "PostToolUse",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": "turn-unconfigured",
            "tool_name": "ReadFile",
            "tool_use_id": "tool-unconfigured",
            "tool_input": {"file_path": repositories[3].join("src/search/Search.kt")},
            "tool_response": {"output": "read"}
        }),
    );
    assert!(unconfigured.status.success());
    let unconfigured_snapshot = TaskRuntime::initialize(harness.root())
        .unwrap()
        .read_snapshot_by_locator(
            &ExternalSessionLocator::new("codex", "cross-unconfigured").unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(unconfigured_snapshot.task_signals.is_empty());

    let database = harness.root().join("state/repository-registry.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", database.display()));
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove Registry database: {error}"),
        }
    }
    let restored = harness.success(&["repository", "list"]);
    let restored_ids = restored["data"]["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repository| repository["repository_id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        restored_ids,
        configured_ids.iter().map(String::as_str).collect()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn engineering_graph_cli_commands_scan_record_rebuild_and_explain() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "Engineering CLI workflow");
    let published = approve_publish(
        &harness,
        &space_id,
        "Engineering CLI workflow uses src/contract.rs",
    );
    let repository = harness.home.join("工程 repo");
    fs::create_dir_all(repository.join("src")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    git_output(&repository, &["config", "user.name", "CLI Graph"]);
    git_output(
        &repository,
        &["config", "user.email", "cli-graph@example.invalid"],
    );
    fs::write(
        repository.join("src/contract.rs"),
        "pub fn cli_graph_contract() {}\n",
    )
    .unwrap();
    git_output(&repository, &["add", "--", "."]);
    git_output(&repository, &["commit", "-q", "-m", "fixture"]);

    let empty_plan = harness.failure(&[
        "repository",
        "scan",
        "--checkout-path",
        repository.to_str().unwrap(),
    ]);
    assert_eq!(empty_plan["error"]["code"], "invalid_input");
    assert!(
        empty_plan["error"]["message"]
            .as_str()
            .unwrap()
            .contains("paths")
    );
    let unconfigured = harness.failure(&[
        "repository",
        "scan",
        "--checkout-path",
        repository.to_str().unwrap(),
        "--path",
        "src/contract.rs",
    ]);
    assert_eq!(unconfigured["error"]["code"], "repository_not_configured");
    let added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "FE",
        "--path",
        repository.to_str().unwrap(),
    ]);
    assert_eq!(
        added["data"]["catalog"]["repository"]["repository_id"],
        "FE"
    );
    let listed = harness.success(&["repository", "list"]);
    assert_eq!(listed["data"]["repositories"].as_array().unwrap().len(), 1);
    let doctor = harness.success(&["repository", "doctor"]);
    assert_eq!(doctor["data"]["catalog"]["healthy"], true);
    let scan = harness.success(&[
        "repository",
        "scan",
        "--checkout-path",
        repository.to_str().unwrap(),
        "--path",
        "src/contract.rs",
    ]);
    assert_eq!(scan["data"]["status"], "available");
    assert!(scan["data"]["artifact_count"].as_u64().unwrap() > 0);
    let reference_input = harness.home.join("reference.json");
    fs::write(
        &reference_input,
        serde_json::to_vec_pretty(&serde_json::json!({
            "context_id": published.context_id,
            "revision_id": published.revision_id,
            "repository_id": scan["data"]["repository_id"],
            "artifact_kind": "file",
            "relation": "implements",
            "locator": {"locator_kind": "file", "path": "src/contract.rs"},
            "supports": "Direct CLI fixture inspection verified the implementation",
            "limitations": ["Synthetic CLI repository"]
        }))
        .unwrap(),
    )
    .unwrap();
    let recorded = harness.success(&[
        "engineering-reference",
        "record",
        "--input",
        reference_input.to_str().unwrap(),
    ]);
    assert!(recorded["data"]["reference_id"].as_str().is_some());
    let rebuilt = harness.success(&["association", "rebuild"]);
    assert_eq!(rebuilt["data"]["status_counts"]["resolved"], 1);
    let explained = harness.success(&[
        "association",
        "explain",
        "--reference-id",
        recorded["data"]["reference_id"].as_str().unwrap(),
    ]);
    assert_eq!(explained["data"]["status"], "resolved");
    assert!(
        !explained["data"]["graph_paths"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let diagnosed = harness.success(&["association", "rebuild", "--diagnose"]);
    assert_eq!(diagnosed["command"], "association.diagnose");
    assert_eq!(diagnosed["data"]["stored"], false);
}

#[test]
fn repository_doctor_rejects_invalid_catalog_identity_with_typed_error() {
    let harness = Harness::new();
    GitStore::initialize(harness.root()).unwrap();
    let config_path = harness.root().join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[[repositories]]\nid = \"FE/mobile\"\npaths = []\n");
    fs::write(config_path, config).unwrap();

    let doctor = harness.failure(&["repository", "doctor"]);
    assert_eq!(doctor["error"]["code"], "invalid_input");
    assert!(
        doctor["error"]["message"]
            .as_str()
            .unwrap()
            .contains("FE/mobile")
    );
}

#[test]
fn repository_add_requires_a_readable_id_and_upserts_only_the_exact_spelling() {
    let harness = Harness::new();
    let first = init_cli_repo(&harness.home.join("frontend-a"));
    let worktree = init_cli_repo(&harness.home.join("frontend-b"));
    let case_conflict = init_cli_repo(&harness.home.join("frontend-case-conflict"));

    let missing = harness.failure(&["repository", "add", "--path", first.to_str().unwrap()]);
    assert_eq!(missing["error"]["code"], "invalid_input");
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--repository-id")
    );

    let created = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "FE",
        "--path",
        first.to_str().unwrap(),
    ]);
    assert_eq!(
        created["data"]["catalog"]["repository"]["repository_id"],
        "FE"
    );
    assert_eq!(created["data"]["catalog"]["created_identity"], true);

    let extended = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "FE",
        "--path",
        worktree.to_str().unwrap(),
    ]);
    assert_eq!(extended["data"]["catalog"]["created_identity"], false);
    assert_eq!(extended["data"]["catalog"]["added_paths"], 1);

    let rejected = harness.failure(&[
        "repository",
        "add",
        "--repository-id",
        "fe",
        "--path",
        case_conflict.to_str().unwrap(),
    ]);
    assert_eq!(rejected["error"]["code"], "invalid_input");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("differs only by ASCII case")
    );

    let listed = harness.success(&["repository", "list"]);
    assert_eq!(listed["data"]["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"]["repositories"][0]["repository_id"], "FE");
    assert_eq!(
        listed["data"]["repositories"][0]["checkout_paths"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn repository_group_cli_is_explicit_idempotent_concurrent_and_repairs_root_drift() {
    let harness = Harness::new();
    let group_root = harness.home.join("android fels");
    let first = init_cli_repo(&group_root.join("first"));
    let second = init_cli_repo(&group_root.join("second"));
    let sibling = init_cli_repo(&group_root.join("unregistered sibling"));
    let group_root = fs::canonicalize(group_root).unwrap();
    let replacement_root = harness.home.join("replacement fels");
    let replacement_first = init_cli_repo(&replacement_root.join("first"));
    let replacement_second = init_cli_repo(&replacement_root.join("second"));
    let replacement_root = fs::canonicalize(replacement_root).unwrap();

    let first_added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "Android",
        "--path",
        first.to_str().unwrap(),
    ]);
    let first_id = first_added["data"]["catalog"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness.success(&[
        "repository",
        "add",
        "--repository-id",
        &first_id,
        "--path",
        replacement_first.to_str().unwrap(),
    ]);
    let second_added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "iOS",
        "--path",
        second.to_str().unwrap(),
    ]);
    let second_id = second_added["data"]["catalog"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness.success(&[
        "repository",
        "add",
        "--repository-id",
        &second_id,
        "--path",
        replacement_second.to_str().unwrap(),
    ]);

    let worker_count = 6;
    let barrier = Arc::new(Barrier::new(worker_count));
    let mut workers = Vec::new();
    for _ in 0..worker_count {
        let barrier = Arc::clone(&barrier);
        let home = harness.home.clone();
        let group_root = group_root.clone();
        let first_id = first_id.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            Command::new(env!("CARGO_BIN_EXE_sctx"))
                .arg("--json")
                .args([
                    "repository",
                    "group",
                    "add",
                    "--root",
                    group_root.to_str().unwrap(),
                    "--member-repository-id",
                    &first_id,
                ])
                .env("HOME", home)
                .output()
                .unwrap()
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| {
            let output = worker.join().unwrap();
            assert!(
                output.status.success(),
                "concurrent group add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|value| value["data"]["catalog"]["created"] == true)
            .count(),
        1
    );
    let group_ids = outcomes
        .iter()
        .map(|value| {
            value["data"]["catalog"]["repository_group"]["repository_group_id"]
                .as_str()
                .unwrap()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(group_ids.len(), 1);
    let group_id = (*group_ids.first().unwrap()).to_owned();
    assert!(group_id.starts_with("rpg_"));

    let retry = harness.success(&[
        "repository",
        "group",
        "add",
        "--root",
        group_root.to_str().unwrap(),
        "--member-repository-id",
        &first_id,
    ]);
    assert_eq!(retry["data"]["catalog"]["created"], false);
    let conflict = harness.failure(&[
        "repository",
        "group",
        "add",
        "--root",
        group_root.to_str().unwrap(),
        "--member-repository-id",
        &second_id,
    ]);
    assert_eq!(conflict["error"]["code"], "invalid_input");
    let unknown_id = sctx_domain::RepositoryId::new().to_string();
    let unknown = harness.failure(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--member-repository-id",
        &unknown_id,
    ]);
    assert_eq!(unknown["error"]["code"], "repository_not_configured");

    let membership_update = harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--member-repository-id",
        &first_id,
        "--member-repository-id",
        &second_id,
    ]);
    assert_eq!(membership_update["data"]["catalog"]["changed"], true);

    let listed = harness.success(&["repository", "group", "list"]);
    let listed_groups = listed["data"]["repository_groups"].as_array().unwrap();
    assert_eq!(listed_groups.len(), 1);
    assert_eq!(listed_groups[0]["status"], "available");
    assert_eq!(
        listed_groups[0]["member_repository_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first_id.as_str(), second_id.as_str()])
    );
    assert!(
        sibling.exists(),
        "unregistered sibling must remain untouched"
    );

    let moved_group = harness.home.join("moved android fels");
    fs::rename(&group_root, &moved_group).unwrap();
    let drifted_list = harness.success(&["repository", "group", "list"]);
    assert_eq!(
        drifted_list["data"]["repository_groups"][0]["status"],
        "missing"
    );
    assert_eq!(
        drifted_list["data"]["repository_groups"][0]["root_path"],
        group_root.to_str().unwrap()
    );
    let drifted_doctor = harness.success(&["repository", "group", "doctor"]);
    assert_eq!(drifted_doctor["data"]["healthy"], false);
    assert_eq!(
        drifted_doctor["data"]["repository_groups"][0]["status"],
        "missing"
    );
    let whole_catalog = harness.success(&["repository", "list"]);
    assert_eq!(whole_catalog["data"]["registry"], Value::Null);
    assert_eq!(
        whole_catalog["data"]["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let updated = harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--root",
        replacement_root.to_str().unwrap(),
    ]);
    assert_eq!(updated["data"]["catalog"]["changed"], true);
    assert_eq!(
        updated["data"]["catalog"]["repository_group"]["root_path"],
        replacement_root.to_str().unwrap()
    );
    let update_retry = harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--root",
        replacement_root.to_str().unwrap(),
    ]);
    assert_eq!(update_retry["data"]["catalog"]["changed"], false);

    let moved_replacement = harness.home.join("moved replacement fels");
    fs::rename(&replacement_root, &moved_replacement).unwrap();
    let removed = harness.success(&[
        "repository",
        "group",
        "remove",
        "--repository-group-id",
        &group_id,
    ]);
    assert_eq!(removed["data"]["catalog"]["removed"], true);
    let removal_retry = harness.success(&[
        "repository",
        "group",
        "remove",
        "--repository-group-id",
        &group_id,
    ]);
    assert_eq!(removal_retry["data"]["catalog"]["removed"], false);
    let repaired = harness.success(&["repository", "group", "list"]);
    assert!(
        repaired["data"]["repository_groups"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let repositories = harness.success(&["repository", "list"]);
    assert_eq!(
        repositories["data"]["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(moved_group.exists());
    assert!(moved_replacement.exists());
}
#[test]
#[allow(clippy::too_many_lines)]
fn twenty_cli_processes_confirm_one_review_in_one_atomic_commit() {
    let harness = Harness::new();
    let (primary_space_id, _) = create_space(&harness, "CLI Confirmation Primary");
    let session = "cli-confirm-multiprocess";
    let task = task_intent_update_at_root(
        harness.root(),
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot {
                goal: "confirm one CLI Candidate".to_owned(),
                current_direction: Some("write one atomic confirmation".to_owned()),
                in_scope: Vec::new(),
                out_of_scope: Vec::new(),
                domains: Vec::new(),
                platforms: Vec::new(),
                constraints: Vec::new(),
                acceptance_conditions: Vec::new(),
                artifact_hints: Vec::new(),
                interface_hints: Vec::new(),
                open_questions: Vec::new(),
            },
        },
    )
    .unwrap();
    let closed = task_checkpoint_at_root(
        harness.root(),
        &TaskCheckpointInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            expected_task_id: task.context.task_id.to_string(),
            expected_intent_revision_id: task.context.intent_revision_id.to_string(),
            expected_episode_version: 0,
            boundary: TaskCheckpointBoundary::Close,
            claims: vec![TaskCheckpointClaimInput {
                context_kind_hint: Some(ContextKind::Decision),
                topic_key_hint: Some("cli/confirmation".to_owned()),
                statement: "CLI processes share one Confirmation operation".to_owned(),
                rationale: "The Writer lock and Runtime reservation converge".to_owned(),
                applicability: Applicability::default(),
                assumptions: Vec::new(),
                recheck_when: Vec::new(),
                evidence: vec![TaskCheckpointEvidenceInput::InlineValidation {
                    evidence: EvidenceSnapshotDraft {
                        kind: sctx_domain::EvidenceType::ExperimentRecord,
                        supports: "The CLI confirmation fixture passed".to_owned(),
                        content: serde_json::json!({"actual": "passed"}),
                        interpretation: "The Candidate is confirmable".to_owned(),
                        limitations: Vec::new(),
                    },
                }],
                artifact_refs: Vec::new(),
                related_contexts: Vec::new(),
            }],
            unknowns: Vec::new(),
        },
    )
    .unwrap();
    let candidate_id = closed.candidate_build.unwrap().items[0]
        .candidate_id
        .unwrap();
    let input_path = harness.home.join("candidate-confirm.json");
    fs::write(
        &input_path,
        serde_json::to_vec(&serde_json::json!({
            "agent_kind": "codex",
            "external_session_id": session,
            "expected_task_id": task.context.task_id,
            "expected_intent_revision_id": task.context.intent_revision_id,
            "candidate_id": candidate_id,
            "expected_review_version": 1,
            "primary": {"existing_space_id": primary_space_id},
            "related_space_ids": [],
            "edits": {}
        }))
        .unwrap(),
    )
    .unwrap();
    let before = harness.event_count();
    let barrier = Arc::new(Barrier::new(20));
    let outputs = (0..20)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let home = harness.home.clone();
            let input_path = input_path.clone();
            thread::spawn(move || {
                barrier.wait();
                Command::new(env!("CARGO_BIN_EXE_sctx"))
                    .args([
                        "--json",
                        "candidate",
                        "confirm",
                        "--input",
                        input_path.to_str().unwrap(),
                    ])
                    .env("HOME", home)
                    .output()
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let values = outputs
        .iter()
        .map(|output| {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        values
            .iter()
            .filter(|value| value["data"]["created"] == true)
            .count(),
        1
    );
    assert_eq!(
        values
            .iter()
            .map(|value| value["data"]["context_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(harness.event_count(), before + 4);
}

#[test]
#[allow(clippy::too_many_lines)]
fn task_context_cli_entry_is_locator_only_and_read_only() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "CLI Task Context");
    approve_publish(
        &harness,
        &space_id,
        "CLI task context returns published knowledge",
    );
    let establish = |session: &str| TaskIntentUpdateInput {
        agent_kind: "codex".to_owned(),
        external_session_id: session.to_owned(),
        task_boundary: TaskBoundary::New,
        expected_revision_id: ExpectedRevisionId::Null(()),
        intent: WorkingIntentSnapshot {
            goal: "retrieve CLI task context".to_owned(),
            current_direction: Some("return published CLI knowledge".to_owned()),
            in_scope: vec![],
            out_of_scope: vec![],
            domains: vec!["cli".to_owned()],
            platforms: vec![],
            constraints: vec![],
            acceptance_conditions: vec![],
            artifact_hints: vec![],
            interface_hints: vec![],
            open_questions: vec![],
        },
    };
    task_intent_update_at_root(harness.root(), &establish("cli-session")).unwrap();
    task_intent_update_at_root(harness.root(), &establish("other-cli-session")).unwrap();
    let base = [
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "cli-session",
        "--token-budget",
        "2000",
        "--max-spaces",
        "1",
    ];
    let first = harness.success(&base);
    let same = harness.success(&base);
    let changed = harness.success(&[
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "cli-session",
    ]);

    assert_eq!(first["command"], "task.context");
    assert_eq!(first["tree"], first["data"]["tree"]);
    assert_eq!(first["generation"], first["data"]["generation"]);
    assert!(first["data"]["task_session_id"].as_str().is_some());
    assert!(first["data"]["task_id"].as_str().is_some());
    assert!(first["data"]["candidate_spaces"].as_array().is_some());
    assert!(first["data"]["candidate_spaces"].as_array().unwrap().len() <= 1);
    assert!(first["data"]["retrieval_paths"].as_array().is_some());
    assert_eq!(
        first["data"]["task_session_id"],
        changed["data"]["task_session_id"]
    );
    assert_eq!(first["data"]["task_id"], changed["data"]["task_id"]);
    assert_eq!(
        first["data"]["intent_revision_id"],
        same["data"]["intent_revision_id"]
    );
    assert_eq!(
        same["data"]["intent_revision_id"],
        changed["data"]["intent_revision_id"]
    );

    let other = harness.success(&[
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "other-cli-session",
    ]);
    assert_ne!(
        first["data"]["task_session_id"],
        other["data"]["task_session_id"]
    );
    assert_ne!(first["data"]["task_id"], other["data"]["task_id"]);

    let rejected = harness.failure(&[
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "routed-cli-session",
        "--space-id",
        &space_id,
    ]);
    assert_eq!(rejected["error"]["code"], "invalid_input");

    let invalid_max = harness.failure(&[
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "invalid-max-session",
        "--max-spaces",
        "33",
    ]);
    assert_eq!(invalid_max["error"]["code"], "invalid_input");
}

#[test]
#[allow(clippy::too_many_lines)]
fn task_intent_update_and_signal_supersede_cli_entries_use_strict_json_contracts() {
    let harness = Harness::new();
    GitStore::initialize(harness.root()).unwrap();
    let update_path = harness.home.join("task-update.json");
    fs::write(
        &update_path,
        serde_json::to_vec(&serde_json::json!({
            "agent_kind": "codex",
            "external_session_id": "cli-authoritative",
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {
                "goal": "authoritative CLI task",
                "current_direction": "establish authoritative CLI intent",
                "domains": ["cli"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let updated = harness.success(&[
        "task",
        "intent",
        "update",
        "--input",
        update_path.to_str().unwrap(),
    ]);
    assert_eq!(updated["command"], "task.intent.update");
    assert_eq!(updated["data"]["revision_status"], "created");
    assert!(text(&updated, "task_id").starts_with("tsk_"));
    assert!(text(&updated, "intent_revision_id").starts_with("tir_"));

    let checkpoint_path = harness.home.join("task-checkpoint.json");
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&serde_json::json!({
            "agent_kind": "codex",
            "external_session_id": "cli-authoritative",
            "expected_task_id": updated["data"]["task_id"],
            "expected_intent_revision_id": updated["data"]["intent_revision_id"],
            "expected_episode_version": 0,
            "boundary": "continue",
            "claims": [{
                "statement": "The CLI Checkpoint completed",
                "rationale": "The strict JSON entry called the shared MCP workflow",
                "applicability": {"domains": ["cli"], "platforms": [], "conditions": ["checkpoint"]},
                "assumptions": [],
                "recheck_when": ["the CLI contract changes"],
                "evidence": [{
                    "kind": "inline_validation",
                    "evidence": {
                        "kind": "experiment_record",
                        "supports": "the CLI returned a Checkpoint",
                        "content": {"command": "task.checkpoint", "actual": "success"},
                        "interpretation": "the explicit workflow is executable",
                        "limitations": []
                    }
                }],
                "artifact_refs": [],
                "related_contexts": []
            }],
            "unknowns": []
        }))
        .unwrap(),
    )
    .unwrap();
    let checkpoint = harness.success(&[
        "task",
        "checkpoint",
        "--input",
        checkpoint_path.to_str().unwrap(),
    ]);
    assert_eq!(checkpoint["command"], "task.checkpoint");
    assert_eq!(checkpoint["data"]["created"], true);
    assert_eq!(checkpoint["data"]["episode_version"], 1);
    assert!(text(&checkpoint, "checkpoint_id").starts_with("ckp_"));
    let retried_checkpoint = harness.success(&[
        "task",
        "checkpoint",
        "--input",
        checkpoint_path.to_str().unwrap(),
    ]);
    assert_eq!(retried_checkpoint["data"]["created"], false);
    assert_eq!(
        retried_checkpoint["data"]["checkpoint_id"],
        checkpoint["data"]["checkpoint_id"]
    );
    let close_path = harness.home.join("task-checkpoint-close.json");
    fs::write(
        &close_path,
        serde_json::to_vec(&serde_json::json!({
            "agent_kind": "codex",
            "external_session_id": "cli-authoritative",
            "expected_task_id": updated["data"]["task_id"],
            "expected_intent_revision_id": updated["data"]["intent_revision_id"],
            "expected_episode_version": 1,
            "boundary": "close",
            "claims": [],
            "unknowns": [{
                "statement": "Candidate confirmation remains separate",
                "blocking": false,
                "recheck_when": []
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let closed = harness.success(&[
        "task",
        "checkpoint",
        "--input",
        close_path.to_str().unwrap(),
    ]);
    assert_eq!(closed["data"]["candidate_build"]["status"], "complete");
    assert_eq!(
        closed["data"]["candidate_build"]["items"][0]["status"],
        "created"
    );
    let episode_id = closed["data"]["episode_id"].as_str().unwrap();
    let rebuilt = harness.success(&[
        "candidate",
        "build-closed-episode",
        "--episode-id",
        episode_id,
    ]);
    assert_eq!(rebuilt["command"], "candidate.build-closed-episode");
    assert_eq!(
        rebuilt["data"]["build_id"],
        closed["data"]["candidate_build"]["build_id"]
    );
    assert_eq!(
        rebuilt["data"]["items"][0]["submission_id"],
        closed["data"]["candidate_build"]["items"][0]["submission_id"]
    );
    let candidate_id = closed["data"]["candidate_build"]["items"][0]["candidate_id"]
        .as_str()
        .unwrap();
    let analyzed = harness.success(&[
        "candidate",
        "analyze",
        "--candidate-id",
        candidate_id,
        "--token-budget",
        "4096",
        "--top-k",
        "8",
    ]);
    assert_eq!(analyzed["command"], "candidate.analyze");
    assert!(analyzed["data"]["analysis_generation"].as_u64().unwrap() >= 2);
    assert_eq!(
        analyzed["data"]["candidate"]["analysis"]["status"],
        "complete"
    );
    assert_eq!(analyzed["data"]["candidate"]["candidate_id"], candidate_id);

    let listed = harness.success(&[
        "candidate",
        "list",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "cli-authoritative",
        "--limit",
        "10",
        "--token-budget",
        "32768",
    ]);
    assert_eq!(listed["command"], "candidate.list");
    assert_eq!(listed["data"]["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"]["reviews"][0]["candidate_id"], candidate_id);
    assert_eq!(listed["data"]["reviews"][0]["untrusted_data"], true);
    let got = harness.success(&[
        "candidate",
        "get",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "cli-authoritative",
        "--candidate-id",
        candidate_id,
    ]);
    assert_eq!(got["command"], "candidate.get");
    assert_eq!(got["data"]["candidate_id"], candidate_id);
    assert!(
        !got["data"]["content"]["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let discarded = harness.success(&[
        "candidate",
        "discard",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "cli-authoritative",
        "--expected-task-id",
        updated["data"]["task_id"].as_str().unwrap(),
        "--expected-intent-revision-id",
        updated["data"]["intent_revision_id"].as_str().unwrap(),
        "--candidate-id",
        candidate_id,
        "--expected-review-version",
        "1",
        "--reason",
        "explicit CLI discard",
    ]);
    assert_eq!(discarded["command"], "candidate.discard");
    assert_eq!(discarded["data"]["status"], "discarded");
    assert_eq!(discarded["data"]["review"]["review_version"], 2);
    assert!(
        harness.success(&[
            "candidate",
            "list",
            "--agent-kind",
            "codex",
            "--external-session-id",
            "cli-authoritative",
        ])["data"]["reviews"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        harness.success(&[
            "candidate",
            "list",
            "--agent-kind",
            "codex",
            "--external-session-id",
            "cli-authoritative",
            "--status",
            "discarded",
            "--token-budget",
            "32768",
        ])["data"]["reviews"][0]["candidate_id"],
        candidate_id
    );
    let forbidden_confirm = harness.failure(&[
        "candidate",
        "discard",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "cli-authoritative",
        "--candidate-id",
        candidate_id,
        "--confirm",
    ]);
    assert_eq!(forbidden_confirm["error"]["code"], "invalid_input");

    let business_repository = harness.home.join("business repository");
    fs::create_dir_all(&business_repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&business_repository)
            .status()
            .unwrap()
            .success()
    );
    let business_repository = fs::canonicalize(business_repository).unwrap();
    UserConfigStore::initialize(harness.root())
        .unwrap()
        .add_repository(
            sctx_domain::RepositoryId::new(),
            std::slice::from_ref(&business_repository),
        )
        .unwrap();
    let focus_path = harness.home.join("task-artifact-focus.json");
    fs::write(
        &focus_path,
        serde_json::to_vec(&serde_json::json!({
            "agent_kind": "codex",
            "external_session_id": "cli-authoritative",
            "expected_revision_id": updated["data"]["intent_revision_id"],
            "absolute_file_path": business_repository.join("src/future.rs"),
            "locator": {"locator_kind": "file"},
            "token_budget": 2000,
            "max_spaces": 8
        }))
        .unwrap(),
    )
    .unwrap();
    let focused = harness.success(&[
        "task",
        "artifact-focus",
        "--input",
        focus_path.to_str().unwrap(),
    ]);
    assert_eq!(focused["command"], "task.artifact-focus");
    assert!(focused["data"].get("created").is_none());
    assert!(focused["data"].get("focus").is_none());
    assert_eq!(
        focused["data"]["resolved_focus"]["locator"],
        serde_json::json!({"locator_kind": "file", "path": "src/future.rs"})
    );
    assert_eq!(
        focused["data"]["context"]["graph_diagnostics"][0]["kind"],
        "artifact_not_reachable_in_graph"
    );

    let task_session_id = updated["data"]["task_session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let task_id = updated["data"]["task_id"].as_str().unwrap();
    let revision_id = updated["data"]["intent_revision_id"].as_str().unwrap();
    let runtime = TaskRuntime::initialize(harness.root()).unwrap();
    let merged = runtime
        .merge_signals(
            task_session_id,
            vec![sctx_domain::TaskSignal {
                kind: TaskSignalKind::Diff,
                content: "stale diff".to_owned(),
            }],
        )
        .unwrap();
    let supersede_path = harness.home.join("signal-supersede.json");
    fs::write(
        &supersede_path,
        serde_json::to_vec(&serde_json::json!({
            "agent_kind": "codex",
            "external_session_id": "cli-authoritative",
            "task_id": task_id,
            "expected_revision_id": revision_id,
            "signal_ids": [merged.inserted_signal_ids[0]]
        }))
        .unwrap(),
    )
    .unwrap();
    let superseded = harness.success(&[
        "task",
        "signal",
        "supersede",
        "--input",
        supersede_path.to_str().unwrap(),
    ]);
    assert_eq!(superseded["command"], "task.signal.supersede");
    assert!(
        superseded["data"]["active_signals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn post_tool_hook_captures_a_bounded_breadcrumb_not_raw_payload() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "Capture contract");
    assert!(!space_id.is_empty());
    let workspace = harness.home.join("business workspace");
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
    harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "FE",
        "--path",
        workspace.to_str().unwrap(),
    ]);
    let start = serde_json::json!({
        "conversation_id": "conv_contract",
        "generation_id": "gen_contract",
        "model": "claude-opus-4-7",
        "hook_event_name": "sessionStart",
        "cursor_version": "3.13.10",
        "workspace_roots": [workspace],
        "user_email": null,
        "transcript_path": null,
        "session_id": "conv_contract",
        "is_background_agent": false,
        "composer_mode": "agent"
    });
    let start_output = harness.run_with_input(&["hook", "--agent", "cursor"], &start);
    assert!(start_output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&start_output.stdout).unwrap(),
        serde_json::json!({"additional_context": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
    let payload = serde_json::json!({
        "conversation_id": "conv_contract",
        "generation_id": "gen_contract",
        "model": "claude-opus-4-7",
        "hook_event_name": "postToolUse",
        "cursor_version": "3.13.10",
        "workspace_roots": [workspace],
        "user_email": null,
        "transcript_path": null,
        "tool_name": "Shell",
        "tool_input": {
            "command": "echo RAW_COMMAND_MUST_NOT_BE_CAPTURED",
            "working_directory": workspace
        },
        "tool_output": "RAW_OUTPUT_MUST_NOT_BE_CAPTURED",
        "tool_use_id": "tool_contract",
        "cwd": workspace,
        "duration": 10
    });
    let output = harness.run_with_input(&["hook", "--agent", "cursor"], &payload);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::json!({})
    );

    let captures = fs::read_dir(harness.root().join("state/capture"))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(captures.len(), 1);
    let stored = fs::read_to_string(captures[0].path()).unwrap();
    assert!(stored.contains("tool Shell succeeded"));
    assert!(!stored.contains("RAW_COMMAND_MUST_NOT_BE_CAPTURED"));
    assert!(!stored.contains("RAW_OUTPUT_MUST_NOT_BE_CAPTURED"));
}

#[test]
fn mcp_stdio_entry_serves_cursor_and_codex_without_extra_stdout() {
    for client in ["cursor", "codex"] {
        let harness = Harness::new();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["mcp", "serve", "--client", client])
            .env("HOME", &harness.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let initialize = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"}
        }))
        .unwrap();
        let list = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .unwrap();
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{initialize}").unwrap();
        writeln!(stdin, "{list}").unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{client}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let responses = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 16);
        assert!(tools.iter().any(|tool| tool["name"] == "task_checkpoint"));
        for name in [
            "candidate_list",
            "candidate_get",
            "candidate_discard",
            "candidate_confirm",
        ] {
            assert!(tools.iter().any(|tool| tool["name"] == name));
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn lifecycle_commands_share_stable_json_tree_and_generation_envelopes() {
    let harness = Harness::new();
    let (space_id, intent_revision) = create_space(&harness, "Lifecycle");

    let revised_intent = harness.success(&[
        "space",
        "intent",
        "revise",
        "--space-id",
        &space_id,
        "--parent-revision-id",
        &intent_revision,
        "--title",
        "Lifecycle revised",
        "--problem",
        "cold start",
        "--desired-outcome",
        "shared knowledge",
        "--in-scope",
        "CLI",
        "--acceptance-condition",
        "works",
    ]);
    let revised_intent_id = text(&revised_intent, "revision_id");
    assert_ne!(intent_revision, revised_intent_id);
    assert_eq!(
        harness.success(&["space", "list"])["data"]["spaces"][0]["space_id"],
        space_id
    );
    harness.success(&["space", "get", "--space-id", &space_id]);

    let (context_id, first_revision) = seed_context(&harness, &space_id, "first snapshot");
    let revised = harness.success(&[
        "context",
        "revise",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--parent-revision-id",
        &first_revision,
        "--kind",
        "decision",
        "--topic-key",
        "cli/output",
        "--statement",
        "stable output",
        "--rationale",
        "stable clients",
        "--domain",
        "cli",
        "--evidence-json",
        EVIDENCE,
    ]);
    let revision_id = text(&revised, "revision_id").to_owned();
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
    let review_event = text(&review, "event_id");
    let publication = harness.success(&[
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
        review_event,
    ]);
    let publication_id = text(&publication, "publication_id");

    let search = harness.success(&[
        "search",
        "--query",
        "stable",
        "--space-id",
        &space_id,
        "--status",
        "accepted",
    ]);
    assert_eq!(search["data"]["results"][0]["context_id"], context_id);
    establish_cli_task(
        &harness,
        "accepted-context-retrieval",
        "stable",
        "retrieve stable accepted Context",
    );
    let pack = harness.success(&[
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "accepted-context-retrieval",
        "--token-budget",
        "1000",
    ]);
    assert_eq!(
        pack["data"]["items"][0]["context"]["context_id"],
        context_id
    );
    harness.success(&[
        "context",
        "get",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
    ]);
    harness.success(&[
        "context",
        "withdraw",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--previous-publication-id",
        publication_id,
    ]);
    harness.success(&["index", "status"]);
    harness.success(&["index", "rebuild"]);
    harness.success(&["validate", "--staged"]);
    let human = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["space", "list"])
        .env("HOME", &harness.home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("tree:"));
    assert!(stdout.contains("generation:"));
    assert!(stdout.contains(&space_id));
}

#[test]
fn unrelated_unknown_schema_isolated_while_target_and_head_checks_stay_closed() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "Isolation");
    let repository = harness.repository();
    let unknown_path = repository.join("events/aa/evt_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.json");
    fs::create_dir_all(unknown_path.parent().unwrap()).unwrap();
    fs::write(
        &unknown_path,
        r#"{"schema_version":"999","event_id":"evt_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","event_type":"future.event"}"#,
    )
    .unwrap();
    git_output(
        &repository,
        &[
            "add",
            "--",
            "events/aa/evt_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.json",
        ],
    );
    git_output(&repository, &["commit", "-m", "unknown schema fixture"]);

    let published = approve_publish(&harness, &space_id, "legal aggregate remains writable");
    let status = harness.success(&["index", "status"]);
    assert_eq!(status["data"]["diagnostic_count"], 1);
    assert_eq!(
        status["data"]["diagnostics"][0]["code"],
        "UNKNOWN_SCHEMA_VERSION"
    );
    assert!(
        status["data"]["diagnostics"][0]["source_path"]
            .as_str()
            .unwrap()
            .contains("evt_aaaaaaaa")
    );
    harness.success(&["validate", "--staged"]);

    let head_before = harness.head();
    let events_before = harness.event_count();
    let invalid_context = "ctx_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let invalid_revision = "rev_cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let failure = harness.failure(&[
        "context",
        "review",
        "--space-id",
        &space_id,
        "--context-id",
        invalid_context,
        "--revision-id",
        invalid_revision,
        "--verdict",
        "approve",
        "--reason",
        "must fail",
    ]);
    assert_eq!(failure["error"]["code"], "invalid_input");
    assert_eq!(harness.head(), head_before);
    assert_eq!(harness.event_count(), events_before);

    let store = GitStore::initialize(harness.root()).unwrap();
    let concurrent = Event::publication_changed(
        SpaceId::from_str(&space_id).unwrap(),
        published.context_id.parse().unwrap(),
        PublicationDraft {
            previous_publication_ids: Vec::new(),
            action: PublicationAction::Publish,
            revision_id: published.revision_id.parse().unwrap(),
            review_event_ids: vec![EventId::from_str(&published.review_event_id).unwrap()],
        },
        None,
    )
    .unwrap();
    store
        .append_event(AppendRequest::event(concurrent))
        .unwrap();
    let conflict_head = harness.head();
    let failure = harness.failure(&[
        "context",
        "withdraw",
        "--space-id",
        &space_id,
        "--context-id",
        &published.context_id,
        "--revision-id",
        &published.revision_id,
        "--previous-publication-id",
        &published.publication_id,
    ]);
    assert_eq!(failure["error"]["code"], "invariant_violation");
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Head precondition failed")
    );
    assert_eq!(harness.head(), conflict_head);
}

#[test]
#[allow(clippy::too_many_lines)]
fn invalid_semantic_resolution_never_appends_an_event() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "Conflict");
    let first = approve_publish(&harness, &space_id, "first side");
    let second = approve_publish(&harness, &space_id, "second side");
    let third = approve_publish(&harness, &space_id, "unrelated side");
    let first_participant = format!(
        "{}:{}:{}",
        first.context_id, first.revision_id, first.publication_id
    );
    let second_participant = format!(
        "{}:{}:{}",
        second.context_id, second.revision_id, second.publication_id
    );
    let opened = harness.success(&[
        "semantic",
        "conflict",
        "open",
        "--space-id",
        &space_id,
        "--participant",
        &first_participant,
        "--participant",
        &second_participant,
        "--reason",
        "contradictory decisions",
        "--domain",
        "cli",
    ]);
    let conflict_id = text(&opened, "conflict_id").to_owned();
    let head_before = harness.head();
    let events_before = harness.event_count();
    let first_result = format!("{}:{}:retained", first.context_id, first.revision_id);

    let missing = harness.failure(&[
        "semantic",
        "conflict",
        "resolve",
        "--space-id",
        &space_id,
        "--conflict-id",
        &conflict_id,
        "--expect-no-resolution-head",
        "--related-publication-id",
        &first.publication_id,
        "--related-publication-id",
        &second.publication_id,
        "--result",
        &first_result,
        "--rationale",
        "incomplete",
    ]);
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("every conflict participant exactly once")
    );
    assert_eq!(harness.head(), head_before);
    assert_eq!(harness.event_count(), events_before);

    let second_result = format!("{}:{}:retained", second.context_id, second.revision_id);
    let third_result = format!("{}:{}:retained", third.context_id, third.revision_id);
    let extra = harness.failure(&[
        "semantic",
        "conflict",
        "resolve",
        "--space-id",
        &space_id,
        "--conflict-id",
        &conflict_id,
        "--expect-no-resolution-head",
        "--related-publication-id",
        &first.publication_id,
        "--related-publication-id",
        &second.publication_id,
        "--result",
        &first_result,
        "--result",
        &second_result,
        "--result",
        &third_result,
        "--rationale",
        "contains unrelated Context",
    ]);
    assert!(
        extra["error"]["message"]
            .as_str()
            .unwrap()
            .contains("every conflict participant exactly once")
    );
    assert_eq!(harness.head(), head_before);
    assert_eq!(harness.event_count(), events_before);

    let resolved = harness.success(&[
        "semantic",
        "conflict",
        "resolve",
        "--space-id",
        &space_id,
        "--conflict-id",
        &conflict_id,
        "--expect-no-resolution-head",
        "--related-publication-id",
        &first.publication_id,
        "--related-publication-id",
        &second.publication_id,
        "--result",
        &first_result,
        "--result",
        &second_result,
        "--rationale",
        "explicit complete resolution",
    ]);
    assert!(text(&resolved, "resolution_id").starts_with("rsl_"));
}

#[derive(Debug)]
struct StopAfterJournal;

impl CrashInjector for StopAfterJournal {
    fn check(&self, seam: CrashSeam) -> Result<()> {
        if seam == CrashSeam::AfterJournal {
            Err(Error::new(ErrorKind::Io, "injected stop"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn pending_commit_and_move_aside_are_explicit_and_validate_staged_rejects_modification()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let harness = Harness::new();
    create_space(&harness, "Pending baseline");
    let pending_store =
        GitStore::initialize(harness.root())?.with_crash_injector(Arc::new(StopAfterJournal));
    let pending_event = Event::space_created(intent("Pending commit"), None)?;
    assert!(
        pending_store
            .append_event(AppendRequest::event(pending_event))
            .is_err()
    );
    let listed = harness.success(&["pending", "list"]);
    let batch = listed["data"]["batches"][0]["batch_id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness.success(&["pending", "commit", &batch]);
    assert!(
        harness.success(&["pending", "list"])["data"]["batches"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let pending_store =
        GitStore::initialize(harness.root())?.with_crash_injector(Arc::new(StopAfterJournal));
    let pending_event = Event::space_created(intent("Pending aside"), None)?;
    assert!(
        pending_store
            .append_event(AppendRequest::event(pending_event))
            .is_err()
    );
    let listed = harness.success(&["pending", "list"]);
    let batch = listed["data"]["batches"][0]["batch_id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness.success(&["pending", "move-aside", &batch]);

    let event_path = git_output(
        &harness.repository(),
        &["ls-tree", "-r", "--name-only", "HEAD", "--", "events"],
    )
    .lines()
    .next()
    .unwrap()
    .to_owned();
    let absolute = harness.repository().join(&event_path);
    let mut bytes = fs::read(&absolute)?;
    bytes.push(b' ');
    fs::write(&absolute, bytes)?;
    git_output(&harness.repository(), &["add", "--", &event_path]);
    let failure = harness.failure(&["validate", "--staged"]);
    assert_eq!(failure["error"]["code"], "invariant_violation");
    Ok(())
}

fn intent(title: &str) -> IntentSnapshot {
    IntentSnapshot {
        title: title.to_owned(),
        problem: "pending recovery".to_owned(),
        desired_outcome: "explicit action".to_owned(),
        in_scope: vec!["CLI".to_owned()],
        out_of_scope: Vec::new(),
        acceptance_conditions: vec!["recoverable".to_owned()],
        domain_terms: Vec::new(),
    }
}
