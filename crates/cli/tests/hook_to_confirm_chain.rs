use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sctx_agent_adapter::SHARED_CONTEXT_ACTIVATION_MARKER;
use sctx_domain::{
    Applicability, ContextKind, ContextRevisionDraft, EvidenceSnapshotDraft, EvidenceType,
    ExternalSessionLocator, IntentSnapshot, PublicationAction, PublicationDraft, ReviewDraft,
    ReviewVerdict, TaskSignalKind, WorkEpisodeStatus,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_local_state::{BreadcrumbKind, CaptureStore};
use sctx_task_runtime::TaskRuntime;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct OracleEnvelope {
    hook_chain: HookOracle,
}

#[derive(Deserialize)]
struct HookOracle {
    session: String,
    prompt: String,
    working_intent_goal: String,
    source_relative_path: String,
    source_text: String,
    space_title: String,
    context_statement: String,
    claim_statement: String,
    claim_rationale: String,
    test_tool: String,
    raw_file_marker: String,
    raw_test_marker: String,
    raw_stop_marker: String,
    expected_confirmation_events: usize,
}

struct SeededContext {
    space: String,
    context: String,
    revision: String,
}

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

fn mcp_tool(home: &Path, session: &str, name: &str, arguments: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "codex"])
        .env("HOME", home)
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
            "clientInfo": {"name": "m4-hook-chain", "version": "1"}
        }
    });
    let mut arguments = arguments.clone();
    let arguments_object = arguments.as_object_mut().unwrap();
    arguments_object
        .entry("agent_kind".to_owned())
        .or_insert_with(|| json!("codex"));
    arguments_object
        .entry("external_session_id".to_owned())
        .or_insert_with(|| json!(session));
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
    assert_eq!(
        responses.len(),
        2,
        "unexpected MCP response: {responses:#?}"
    );
    assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(responses[1]["result"]["isError"], false, "{responses:#?}");
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

fn initialize_source_repository(path: &Path, relative_path: &str, source: &str) -> PathBuf {
    fs::create_dir_all(path.join(Path::new(relative_path).parent().unwrap())).unwrap();
    fs::write(path.join(relative_path), source).unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success());
    git(path, &["add", "--", relative_path]);
    git(
        path,
        &[
            "-c",
            "user.name=Shared Context Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "Add fixed hook chain source",
        ],
    );
    fs::canonicalize(path).unwrap()
}

fn append(store: &GitStore, event: Event) {
    store.append_event(AppendRequest::event(event)).unwrap();
}

fn seed_accepted_context(store: &GitStore, oracle: &HookOracle) -> SeededContext {
    let space = Event::space_created(
        IntentSnapshot {
            title: oracle.space_title.clone(),
            problem: "A verified file behavior needs governed history".to_owned(),
            desired_outcome: "An exact Graph focus retrieves the accepted rule".to_owned(),
            in_scope: vec!["exact File focus".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["Graph path is exact".to_owned()],
            domain_terms: Vec::new(),
        },
        None,
    )
    .unwrap();
    let EventPayload::SpaceCreated { space_id, .. } = space.payload() else {
        unreachable!()
    };
    let space_id = *space_id;
    append(store, space);
    let revision = Event::context_revision_added(
        space_id,
        ContextRevisionDraft {
            kind: ContextKind::Contract,
            topic_key: Some("m4/hook-chain".to_owned()),
            statement: oracle.context_statement.clone(),
            rationale: "The fixed Graph fixture is independently verified".to_owned(),
            applicability: Applicability {
                domains: vec!["search".to_owned()],
                platforms: vec!["fe".to_owned(), "ios".to_owned(), "android".to_owned()],
                conditions: Vec::new(),
            },
            assumptions: Vec::new(),
            recheck_when: vec!["the result path changes".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "The fixed graph source was scanned".to_owned(),
                content: json!({"fixture": "m4-hook-to-confirm", "actual": "passed"}),
                interpretation: "The Context is safe for exact Graph retrieval".to_owned(),
                limitations: vec!["local deterministic fixture".to_owned()],
            }],
        },
        None,
    )
    .unwrap();
    let EventPayload::ContextRevisionAdded {
        context_id,
        revision: snapshot,
        ..
    } = revision.payload()
    else {
        unreachable!()
    };
    let (context_id, revision_id) = (*context_id, snapshot.revision_id);
    append(store, revision);
    let review = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "fixed hook-chain fixture approval".to_owned(),
        },
        None,
    )
    .unwrap();
    let review_id = review.event_id();
    append(store, review);
    append(
        store,
        Event::publication_changed(
            space_id,
            context_id,
            PublicationDraft {
                previous_publication_ids: Vec::new(),
                action: PublicationAction::Publish,
                revision_id,
                review_event_ids: vec![review_id],
            },
            None,
        )
        .unwrap(),
    );
    SeededContext {
        space: space_id.to_string(),
        context: context_id.to_string(),
        revision: revision_id.to_string(),
    }
}

fn event_count(repository: &Path) -> usize {
    git(repository, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .filter(|path| path.starts_with("events/"))
        .count()
}

fn post_tool_payload(
    oracle: &HookOracle,
    workspace: &Path,
    file: &Path,
    tool: &str,
    raw_marker: &str,
) -> Value {
    json!({
        "session_id": oracle.session,
        "transcript_path": format!("/tmp/{raw_marker}.jsonl"),
        "cwd": workspace,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": format!("turn-{}", oracle.session),
        "tool_name": tool,
        "tool_use_id": format!("tool-{tool}"),
        "tool_input": {"file_path": file, "command": raw_marker},
        "tool_response": {"output": raw_marker}
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn one_real_hook_to_confirm_identity_chain() {
    let envelope: OracleEnvelope =
        serde_json::from_str(include_str!("../../../fixtures/m4/fixed-oracle.json")).unwrap();
    let oracle = envelope.hook_chain;
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("中文 M4 home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".shared-context");
    let store = GitStore::initialize(&root).unwrap();
    let source_repository = initialize_source_repository(
        &home.join("工程 repo"),
        &oracle.source_relative_path,
        &oracle.source_text,
    );
    let absolute_source = source_repository.join(&oracle.source_relative_path);
    let repository = run_json_cli(
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            "Server",
            "--path",
            source_repository.to_str().unwrap(),
        ],
    );
    let repository_id = repository["data"]["catalog"]["repository"]["repository_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let session_start = run_hook(
        &home,
        &json!({
            "session_id": oracle.session,
            "transcript_path": null,
            "cwd": source_repository,
            "hook_event_name": "SessionStart",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "source": "startup"
        }),
    );
    assert_eq!(
        session_start,
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": SHARED_CONTEXT_ACTIVATION_MARKER
        }})
    );

    let prompt = run_hook(
        &home,
        &json!({
            "session_id": oracle.session,
            "transcript_path": null,
            "cwd": source_repository,
            "hook_event_name": "UserPromptSubmit",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "turn_id": format!("turn-{}", oracle.session),
            "prompt": oracle.prompt
        }),
    );
    assert_eq!(prompt, json!({}));

    let task = mcp_tool(
        &home,
        &oracle.session,
        "task_intent_update",
        &json!({
            "agent_kind": "codex",
            "external_session_id": oracle.session,
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": oracle.working_intent_goal}
        }),
    );
    let task_session_id = task["task_session_id"].as_str().unwrap().to_owned();
    let task_id = task["task_id"].as_str().unwrap().to_owned();
    let intent_revision_id = task["intent_revision_id"].as_str().unwrap().to_owned();
    assert!(task_session_id.starts_with("tss_"));
    assert!(task_id.starts_with("tsk_"));
    assert!(intent_revision_id.starts_with("tir_"));

    let seeded = seed_accepted_context(&store, &oracle);
    let scan = mcp_tool(
        &home,
        &oracle.session,
        "repository_scan",
        &json!({
            "checkout_path": source_repository,
            "paths": [oracle.source_relative_path],
            "max_artifacts": 20
        }),
    );
    assert_eq!(scan["repository_id"], repository_id);
    assert_eq!(scan["planned_path_count"], 1);
    let file_locator = scan["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "file")
        .unwrap()["locator"]
        .clone();
    let reference = mcp_tool(
        &home,
        &oracle.session,
        "engineering_reference_record",
        &json!({
            "context_id": seeded.context,
            "revision_id": seeded.revision,
            "repository_id": repository_id,
            "artifact_kind": "file",
            "relation": "implements",
            "locator": file_locator,
            "supports": "The tracked fixture file implements the accepted rule",
            "limitations": ["fixed local source fixture"]
        }),
    );
    assert!(
        reference["reference_id"]
            .as_str()
            .unwrap()
            .starts_with("ref_")
    );
    let rebuilt = mcp_tool(
        &home,
        &oracle.session,
        "association_rebuild",
        &json!({"diagnose_only": false}),
    );
    assert_eq!(rebuilt["stored"], true);

    let focus = mcp_tool(
        &home,
        &oracle.session,
        "task_artifact_focus",
        &json!({
            "agent_kind": "codex",
            "external_session_id": oracle.session,
            "expected_revision_id": intent_revision_id,
            "absolute_file_path": absolute_source,
            "locator": {"locator_kind": "file"},
            "token_budget": 4000,
            "max_spaces": 8
        }),
    );
    assert_eq!(focus["context"]["task_session_id"], task_session_id);
    assert_eq!(focus["context"]["task_id"], task_id);
    assert_eq!(focus["context"]["intent_revision_id"], intent_revision_id);
    assert_eq!(focus["resolved_focus"]["repository_id"], repository_id);
    assert_eq!(focus["resolved_focus"]["locator"], file_locator);
    let focus_json = serde_json::to_string(&focus).unwrap();
    assert!(focus_json.contains("engineering_graph"));
    assert!(focus_json.contains(&seeded.context));
    let ordinary = mcp_tool(
        &home,
        &oracle.session,
        "task_context",
        &json!({
            "agent_kind": "codex",
            "external_session_id": oracle.session,
            "token_budget": 4000,
            "max_spaces": 8
        }),
    );
    assert_eq!(ordinary["task_id"], task_id);
    assert!(
        !serde_json::to_string(&ordinary)
            .unwrap()
            .contains("engineering_graph")
    );

    let file_hook = run_hook(
        &home,
        &post_tool_payload(
            &oracle,
            &source_repository,
            &absolute_source,
            "Read",
            &oracle.raw_file_marker,
        ),
    );
    let test_hook = run_hook(
        &home,
        &post_tool_payload(
            &oracle,
            &source_repository,
            &absolute_source,
            &oracle.test_tool,
            &oracle.raw_test_marker,
        ),
    );
    assert_eq!(file_hook, json!({}));
    assert_eq!(test_hook, json!({}));
    let locator = ExternalSessionLocator::new("codex", &oracle.session).unwrap();
    let runtime = TaskRuntime::initialize(&root).unwrap();
    let active = runtime.read_snapshot_by_locator(&locator).unwrap().unwrap();
    assert_eq!(active.task_session_id.to_string(), task_session_id);
    assert_eq!(active.task_id.to_string(), task_id);
    assert_eq!(
        active
            .current_intent_revision()
            .unwrap()
            .revision_id
            .to_string(),
        intent_revision_id
    );
    let test_signal = runtime
        .read_signal_history(active.task_session_id)
        .unwrap()
        .into_iter()
        .find(|record| record.signal.kind == TaskSignalKind::TestOutcome)
        .unwrap();
    assert!(test_signal.signal.content.contains(&oracle.test_tool));
    let signal_id = test_signal.signal_id;
    let captures = CaptureStore::initialize(&root).unwrap().list(256).unwrap();
    let owned_captures = captures
        .captures
        .iter()
        .filter(|capture| capture.record.external_session_locator == locator)
        .collect::<Vec<_>>();
    assert_eq!(owned_captures.len(), 2);
    assert!(
        owned_captures
            .iter()
            .all(|capture| capture.record.kind == BreadcrumbKind::ToolOutcome)
    );
    assert!(
        owned_captures
            .iter()
            .any(|capture| capture.record.summary.contains(&oracle.test_tool))
    );
    assert!(owned_captures.iter().any(|capture| {
        capture
            .record
            .file_hints
            .iter()
            .any(|path| path.ends_with(&oracle.source_relative_path))
    }));
    assert_ne!(
        owned_captures[0].record.capture_id,
        owned_captures[1].record.capture_id
    );
    let capture_records = owned_captures
        .iter()
        .map(|capture| &capture.record)
        .collect::<Vec<_>>();
    let capture_json = serde_json::to_string(&capture_records).unwrap();
    for raw in [
        &oracle.raw_file_marker,
        &oracle.raw_test_marker,
        &oracle.prompt,
    ] {
        assert!(!capture_json.contains(raw));
    }
    assert!(owned_captures.iter().all(|capture| {
        capture
            .record
            .task_owner
            .is_some_and(|owner| owner.task_id.to_string() == task_id)
    }));

    let checkpoint = mcp_tool(
        &home,
        &oracle.session,
        "task_checkpoint",
        &json!({
            "agent_kind": "codex",
            "external_session_id": oracle.session,
            "expected_task_id": task_id,
            "expected_intent_revision_id": intent_revision_id,
            "expected_episode_version": 0,
            "boundary": "continue",
            "claims": [{
                "context_kind_hint": "validation",
                "topic_key_hint": "m4/hook-chain-result",
                "statement": oracle.claim_statement,
                "rationale": oracle.claim_rationale,
                "applicability": {
                    "domains": ["search"],
                    "platforms": ["fe", "ios", "android"],
                    "conditions": []
                },
                "assumptions": [],
                "recheck_when": ["the result path changes"],
                "evidence": [{"kind": "task_signal", "signal_id": signal_id}],
                "artifact_refs": [focus["resolved_focus"].clone()],
                "related_contexts": []
            }],
            "unknowns": []
        }),
    );
    let checkpoint_id = checkpoint["checkpoint_id"].as_str().unwrap().to_owned();
    let claim_id = checkpoint["claim_ids"][0].as_str().unwrap().to_owned();
    let episode_id = checkpoint["episode_id"].as_str().unwrap().to_owned();
    assert_eq!(checkpoint["episode_version"], 1);
    assert!(checkpoint.get("candidate_build").is_none());
    let episode_before = runtime
        .list_work_episodes(active.task_session_id, 10)
        .unwrap()
        .into_iter()
        .find(|episode| episode.episode.episode_id.to_string() == episode_id)
        .unwrap();
    assert!(matches!(
        episode_before.episode.status,
        WorkEpisodeStatus::Open
    ));
    assert_eq!(
        episode_before.checkpoints[0].claims[0].evidence_refs[0],
        sctx_domain::CaptureEvidenceRef::TaskSignal { signal_id }
    );
    assert_eq!(
        serde_json::to_value(&episode_before.checkpoints[0].claims[0].artifact_refs[0]).unwrap(),
        focus["resolved_focus"]
    );

    let turn_stop_payload = json!({
        "session_id": oracle.session,
        "transcript_path": format!("/tmp/{}.jsonl", oracle.raw_stop_marker),
        "cwd": source_repository,
        "hook_event_name": "Stop",
        "model": "gpt-5.6-sol",
        "permission_mode": "default",
        "turn_id": format!("turn-{}", oracle.session),
        "stop_hook_active": false,
        "last_assistant_message": oracle.raw_stop_marker
    });
    let stopped = run_hook(&home, &turn_stop_payload);
    let stopped_text = stopped["systemMessage"].as_str().unwrap();
    assert!(stopped_text.contains(&episode_id));
    assert!(stopped_text.contains(&checkpoint_id));
    assert!(!stopped_text.contains(&oracle.raw_stop_marker));
    let episode_after = runtime
        .list_work_episodes(active.task_session_id, 10)
        .unwrap()
        .into_iter()
        .find(|episode| episode.episode.episode_id.to_string() == episode_id)
        .unwrap();
    assert!(matches!(
        &episode_after.episode.status,
        WorkEpisodeStatus::Closed { final_checkpoint_id }
            if final_checkpoint_id.to_string() == checkpoint_id
    ));

    let owner = json!({
        "agent_kind": "codex",
        "external_session_id": oracle.session,
        "status": "pending",
        "limit": 10,
        "token_budget": 32768
    });
    let listed = mcp_tool(&home, &oracle.session, "candidate_list", &owner);
    assert_eq!(listed["reviews"].as_array().unwrap().len(), 1);
    let candidate_id = listed["reviews"][0]["candidate_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let repeated_stop = run_hook(&home, &turn_stop_payload);
    assert!(
        !repeated_stop["systemMessage"]
            .as_str()
            .unwrap()
            .contains(&oracle.raw_stop_marker)
    );
    let repeated_list = mcp_tool(&home, &oracle.session, "candidate_list", &owner);
    assert_eq!(repeated_list["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(repeated_list["reviews"][0]["candidate_id"], candidate_id);

    let review = mcp_tool(
        &home,
        &oracle.session,
        "candidate_get",
        &json!({
            "agent_kind": "codex",
            "external_session_id": oracle.session,
            "candidate_id": candidate_id
        }),
    );
    assert_eq!(review["source_episode"]["episode_id"], episode_id);
    assert_eq!(review["source_episode"]["task_id"], task_id);
    assert_eq!(review["final_checkpoint_id"], checkpoint_id);
    assert_eq!(review["checkpoint_id"], checkpoint_id);
    assert_eq!(review["claim_id"], claim_id);
    assert_eq!(review["content"]["statement"], oracle.claim_statement);
    assert_eq!(review["content"]["rationale"], oracle.claim_rationale);
    assert_eq!(review["untrusted_data"], true);
    assert_eq!(review["ready_for_review"], true);
    assert!(!review["content"]["evidence"].as_array().unwrap().is_empty());
    let review_json = serde_json::to_string(&review).unwrap();
    for raw in [
        &oracle.raw_file_marker,
        &oracle.raw_test_marker,
        &oracle.raw_stop_marker,
        &oracle.prompt,
    ] {
        assert!(!review_json.contains(raw));
    }
    let unconfirmed_pack = mcp_tool(
        &home,
        &oracle.session,
        "task_context",
        &json!({
            "agent_kind": "codex",
            "external_session_id": oracle.session,
            "token_budget": 4000,
            "max_spaces": 8
        }),
    );
    let unconfirmed_json = serde_json::to_string(&unconfirmed_pack).unwrap();
    assert!(!unconfirmed_json.contains(&candidate_id));
    assert!(!unconfirmed_json.contains(&oracle.claim_statement));

    let commits_before = git(store.repository(), &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .unwrap();
    let events_before = event_count(store.repository());
    let confirm_arguments = json!({
        "agent_kind": "codex",
        "external_session_id": oracle.session,
        "expected_task_id": task_id,
        "expected_intent_revision_id": intent_revision_id,
        "candidate_id": candidate_id,
        "expected_review_version": review["review_version"],
        "primary": {"existing_space_id": seeded.space},
        "related_space_ids": []
    });
    let confirmed = mcp_tool(
        &home,
        &oracle.session,
        "candidate_confirm",
        &confirm_arguments,
    );
    assert_eq!(confirmed["status"], "confirmed");
    assert_eq!(confirmed["created"], true);
    assert_eq!(
        confirmed["event_ids"].as_array().unwrap().len(),
        oracle.expected_confirmation_events
    );
    assert_eq!(
        git(store.repository(), &["rev-list", "--count", "HEAD"])
            .parse::<usize>()
            .unwrap(),
        commits_before + 1
    );
    assert_eq!(
        event_count(store.repository()),
        events_before + oracle.expected_confirmation_events
    );
    let retried = mcp_tool(
        &home,
        &oracle.session,
        "candidate_confirm",
        &confirm_arguments,
    );
    assert_eq!(retried["status"], "already_confirmed");
    assert_eq!(retried["created"], false);
    assert_eq!(retried["confirmation_id"], confirmed["confirmation_id"]);
    assert_eq!(retried["commit_oid"], confirmed["commit_oid"]);
    assert_eq!(
        git(store.repository(), &["rev-list", "--count", "HEAD"])
            .parse::<usize>()
            .unwrap(),
        commits_before + 1
    );
    let search = mcp_tool(
        &home,
        &oracle.session,
        "context_search",
        &json!({
            "query": oracle.claim_statement,
            "statuses": ["accepted"],
            "page_size": 10
        }),
    );
    assert!(search["results"].as_array().unwrap().iter().any(|result| {
        result["context_id"] == confirmed["context_id"] && result["status"] == "accepted"
    }));

    assert!(
        runtime
            .verify_source_episode(episode_after.episode.episode_id)
            .unwrap()
            .is_some()
    );
}
