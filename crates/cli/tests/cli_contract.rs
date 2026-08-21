use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    str::FromStr,
    sync::Arc,
};

use sctx_domain::{
    Applicability, ContextKind, ContextRevisionDraft, Error, ErrorKind, EventId,
    EvidenceSnapshotDraft, ExternalSessionLocator, IntentSnapshot, PublicationAction,
    PublicationDraft, Result, SpaceId, TaskIntentDraft, TaskSignalKind, WorkEpisodeId,
};
use sctx_engineering_graph::{RepositoryLocatorQuery, RepositoryRegistry};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, CrashInjector, CrashSeam, GitStore};
use sctx_mcp::{
    ExpectedRevisionId, IntentMaturity, TaskBoundary, TaskIntentUpdateInput,
    task_intent_update_at_root,
};
use sctx_task_runtime::TaskRuntime;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

const EVIDENCE: &str = r#"{"kind":"experiment_record","supports":"CLI command completed","content":{"command":"contract"},"interpretation":"the contract is executable","limitations":["synthetic CLI fixture"]}"#;

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(args)
            .env("HOME", &self.home)
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

fn create_candidate(harness: &Harness, episode_id: &str, statement: &str) -> Value {
    harness.success(&[
        "candidate",
        "create",
        "--source-episode-id",
        episode_id,
        "--kind",
        "discovery",
        "--statement",
        statement,
        "--rationale",
        "the task produced governable knowledge",
        "--domain",
        "cli",
        "--evidence-json",
        EVIDENCE,
    ])
}

fn establish_cli_task(harness: &Harness, session: &str, goal: &str, desired_change: &str) {
    task_intent_update_at_root(
        harness.root(),
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: session.to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            maturity: IntentMaturity::Provisional,
            intent: TaskIntentDraft {
                goal: goal.to_owned(),
                desired_change: desired_change.to_owned(),
                in_scope: vec![],
                out_of_scope: vec![],
                domains: vec![],
                platforms: vec![],
                constraints: vec![],
                acceptance_conditions: vec![],
                artifacts: vec![],
                interfaces: vec![],
                unknowns: vec![],
            },
            evidence_refs: vec![],
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
        "knowledge delete --confirm-path PATH",
        "space create|intent revise|list|get",
        "candidate create",
        "context revise|review|publish|withdraw|get",
        "semantic conflict open|resolve",
        "task context",
        "search",
        "pending list|commit|move-aside",
        "validate --staged",
        "hook --agent cursor|codex",
        "mcp serve --client cursor|codex",
    ] {
        assert!(stdout.contains(command), "missing help surface: {command}");
    }

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
fn hook_capabilities_expose_version_fallback_and_codex_trust_action() {
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
        "cursor",
        "--capabilities",
        "--agent-version",
        "99.0.0",
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
fn session_start_and_prompt_submit_emit_capabilities_without_inferred_context() {
    let harness = Harness::new();
    let (alpha_space_id, _) = create_space(&harness, "Alpha Hook contract");
    let alpha = approve_publish(&harness, &alpha_space_id, "alpha needle accepted context");
    let (beta_space_id, _) = create_space(&harness, "Beta Hook contract");
    let beta = approve_publish(&harness, &beta_space_id, "beta decoy accepted context");
    let candidate = create_candidate(
        &harness,
        &WorkEpisodeId::new().to_string(),
        "alpha needle candidate $(touch /tmp/SCTX_MUST_NOT_EXECUTE)",
    );
    let candidate_id = text(&candidate, "candidate_id");
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
    assert_eq!(response.as_object().unwrap().len(), 1);
    assert!(
        response["systemMessage"]
            .as_str()
            .is_some_and(|message| message.contains("MCP and CLI"))
    );
    assert!(response.get("hookSpecificOutput").is_none());
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
    let context = response["systemMessage"].as_str().unwrap();
    assert!(
        context.contains("PromptEnvelope") && context.contains("task_intent_update"),
        "unexpected Prompt guidance: {context}"
    );
    assert!(!context.contains("alpha needle"));
    assert!(!context.contains("alpha needle accepted context"));
    assert!(!context.contains("beta decoy accepted context"));
    assert!(!context.contains("SCTX_MUST_NOT_EXECUTE"));
    assert!(response.get("hookSpecificOutput").is_none());
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
            maturity: IntentMaturity::Provisional,
            intent: TaskIntentDraft {
                goal: goal.to_owned(),
                desired_change: format!("Implement {goal}"),
                in_scope: vec![],
                out_of_scope: vec![],
                domains: vec![],
                platforms: vec![],
                constraints: vec![],
                acceptance_conditions: vec![],
                artifacts: vec![],
                interfaces: vec![],
                unknowns: vec![],
            },
            evidence_refs: vec![],
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

    let alpha_initial = hook(&prompt("session-alpha", "alphaquartz"));
    let beta_initial = hook(&prompt("session-beta", "betacobalt"));
    for guidance in [alpha_initial, beta_initial] {
        let guidance = guidance["systemMessage"].as_str().unwrap();
        assert!(guidance.contains("task_intent_update"));
        assert!(!guidance.contains("alphaquartz"));
        assert!(!guidance.contains("betacobalt"));
    }
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
                "command": raw_marker,
                "outside": {"path": outside_file},
                "missing": {"path": workspace.join("src/missing.rs")}
            },
            "tool_response": {"output": raw_marker}
        }));
        assert_eq!(response, serde_json::json!({}));
    }

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
        .resolve_by_locator(&RepositoryLocatorQuery::CheckoutPath(canonical_workspace))
        .unwrap()
        .expect("ActiveTask PostToolUse must register its canonical Git Workspace root");
    assert_eq!(registered.locators.len(), 1);
    task_intent_update_at_root(
        harness.root(),
        &intent_update("session-subdir", "subdirectory task", None),
    )
    .unwrap();
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
        .resolve_by_locator(&RepositoryLocatorQuery::CheckoutPath(
            fs::canonicalize(&workspace).unwrap(),
        ))
        .unwrap()
        .expect("a Workspace subdirectory must resolve and refresh its Git top-level");
    assert_eq!(
        after_subdirectory.identity.repository_id,
        registered.identity.repository_id
    );
    assert_eq!(after_subdirectory.locators.len(), 1);
    for (snapshot, own_file, own_test, other_file) in [
        (
            &alpha_snapshot,
            "src/alpha_feature.rs",
            "AlphaContractTest succeeded",
            "src/beta_feature.rs",
        ),
        (
            &beta_snapshot,
            "src/beta_feature.rs",
            "BetaContractTest succeeded",
            "src/alpha_feature.rs",
        ),
    ] {
        assert!(
            snapshot.task_signals.iter().any(|signal| {
                signal.kind == TaskSignalKind::File && signal.content == own_file
            })
        );
        assert!(
            snapshot.task_signals.iter().any(|signal| {
                signal.kind == TaskSignalKind::Test && signal.content == own_test
            })
        );
        assert!(
            !snapshot.task_signals.iter().any(|signal| {
                signal.kind == TaskSignalKind::File && signal.content == other_file
            })
        );
        assert!(!snapshot.task_signals.iter().any(|signal| {
            signal.kind == TaskSignalKind::File
                && (signal.content.contains("outside.rs") || signal.content.contains("missing.rs"))
        }));
        assert!(snapshot.task_signals.iter().all(|signal| {
            !signal.content.contains("RAW_ALPHA_MUST_NOT_PERSIST")
                && !signal.content.contains("RAW_BETA_MUST_NOT_PERSIST")
        }));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn hook_repository_discovery_ascends_only_the_explicit_sibling_repository() {
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
    let explicit = &repositories[1];
    let open_task = |session_id: &str| {
        task_intent_update_at_root(
            harness.root(),
            &TaskIntentUpdateInput {
                agent_kind: "codex".to_owned(),
                external_session_id: session_id.to_owned(),
                task_boundary: TaskBoundary::New,
                expected_revision_id: ExpectedRevisionId::Null(()),
                maturity: IntentMaturity::Provisional,
                intent: TaskIntentDraft {
                    goal: "Verify sparse Repository discovery".to_owned(),
                    desired_change: "Refresh only the explicit Repository".to_owned(),
                    in_scope: Vec::new(),
                    out_of_scope: Vec::new(),
                    domains: Vec::new(),
                    platforms: Vec::new(),
                    constraints: Vec::new(),
                    acceptance_conditions: Vec::new(),
                    artifacts: Vec::new(),
                    interfaces: Vec::new(),
                    unknowns: Vec::new(),
                },
                evidence_refs: Vec::new(),
            },
        )
        .unwrap();
    };
    let post_tool = |session_id: &str, cwd: &Path| {
        let output = harness.run_with_input(
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
                .resolve_by_locator(&RepositoryLocatorQuery::CheckoutPath(
                    fs::canonicalize(sibling).unwrap(),
                ))
                .unwrap()
                .is_none(),
            "a common parent must not recursively discover sibling Repositories"
        );
    }

    let repository_id = registered[0].identity.repository_id;
    open_task("root-discovery");
    post_tool("root-discovery", explicit);
    let refreshed = registry.list().unwrap();
    assert_eq!(refreshed.len(), 1);
    assert_eq!(refreshed[0].identity.repository_id, repository_id);
}

#[test]
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
fn candidate_create_is_unassigned_idempotent_and_absent_from_retrieval() {
    let harness = Harness::new();
    GitStore::initialize(harness.root()).unwrap();
    let source_episode_id = WorkEpisodeId::new().to_string();
    let initial_count = harness.event_count();

    let created = create_candidate(&harness, &source_episode_id, "hidden episode discovery");
    let retry = create_candidate(&harness, &source_episode_id, "hidden episode discovery");
    assert_eq!(created["data"]["created"], true);
    assert_eq!(retry["data"]["created"], false);
    for field in [
        "candidate_id",
        "source_episode_id",
        "event_id",
        "batch_id",
        "commit_oid",
    ] {
        assert_eq!(created["data"][field], retry["data"][field]);
    }
    assert_eq!(created["data"]["status"], "candidate");
    assert_eq!(harness.event_count(), initial_count + 1);

    let different = create_candidate(
        &harness,
        &source_episode_id,
        "different hidden episode discovery",
    );
    assert_ne!(
        created["data"]["candidate_id"],
        different["data"]["candidate_id"]
    );
    assert_eq!(harness.event_count(), initial_count + 2);

    let search = harness.success(&[
        "search",
        "--query",
        "hidden episode discovery",
        "--status",
        "candidate",
    ]);
    assert!(search["data"]["results"].as_array().unwrap().is_empty());
    establish_cli_task(
        &harness,
        "candidate-retrieval-isolation",
        "hidden episode discovery",
        "retrieve confirmed knowledge only",
    );
    let pack = harness.success(&[
        "task",
        "context",
        "--agent-kind",
        "codex",
        "--external-session-id",
        "candidate-retrieval-isolation",
        "--token-budget",
        "1000",
    ]);
    assert!(pack["data"]["items"].as_array().unwrap().is_empty());

    let rejected = harness.failure(&[
        "candidate",
        "create",
        "--source-episode-id",
        &source_episode_id,
        "--space-id",
        &SpaceId::new().to_string(),
        "--kind",
        "discovery",
        "--statement",
        "routing must be rejected",
        "--rationale",
        "candidates are unassigned",
        "--evidence-json",
        EVIDENCE,
    ]);
    assert_eq!(rejected["error"]["code"], "invalid_input");
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
        maturity: IntentMaturity::Provisional,
        intent: TaskIntentDraft {
            goal: "retrieve CLI task context".to_owned(),
            desired_change: "return published CLI knowledge".to_owned(),
            in_scope: vec![],
            out_of_scope: vec![],
            domains: vec!["cli".to_owned()],
            platforms: vec![],
            constraints: vec![],
            acceptance_conditions: vec![],
            artifacts: vec![],
            interfaces: vec![],
            unknowns: vec![],
        },
        evidence_refs: vec![],
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
            "maturity": "provisional",
            "intent": {
                "goal": "authoritative CLI task",
                "desired_change": "establish authoritative CLI intent",
                "in_scope": [], "out_of_scope": [], "domains": ["cli"],
                "platforms": [], "constraints": [], "acceptance_conditions": [],
                "artifacts": [], "interfaces": [], "unknowns": []
            },
            "evidence_refs": []
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
    assert!(text(&updated, "task_id").starts_with("tsk_"));
    assert!(text(&updated, "intent_revision_id").starts_with("tir_"));

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
                kind: TaskSignalKind::File,
                content: "src/stale.rs".to_owned(),
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
            "working_directory": harness.home.join("business workspace")
        },
        "tool_output": "RAW_OUTPUT_MUST_NOT_BE_CAPTURED",
        "tool_use_id": "tool_contract",
        "cwd": harness.home.join("business workspace"),
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
        assert_eq!(
            responses[1]["result"]["tools"].as_array().unwrap().len(),
            11
        );
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
