use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker};
use serde_json::{Value, json};

fn wait_child(child: &mut Child, label: &str, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _killed = child.kill();
            let _status = child.wait();
            panic!("{label} exceeded {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_json_cli(binary: &Path, home: &Path, args: &[&str]) -> Value {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(binary)
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .stdout(Stdio::from(stdout.reopen().unwrap()))
        .stderr(Stdio::from(stderr.reopen().unwrap()))
        .spawn()
        .unwrap();
    let status = wait_child(
        &mut child,
        &format!("installed CLI {args:?}"),
        Duration::from_secs(30),
    );
    assert!(
        status.success(),
        "installed CLI {args:?} failed: {}",
        fs::read_to_string(stderr.path()).unwrap()
    );
    serde_json::from_slice(&fs::read(stdout.path()).unwrap()).unwrap()
}

fn run_configured_hook(home: &Path, command: &str, payload: &Value) -> Value {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout.reopen().unwrap()))
        .stderr(Stdio::from(stderr.reopen().unwrap()))
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.as_mut().unwrap(), payload).unwrap();
    drop(child.stdin.take());
    let status = wait_child(&mut child, "installed Hook", Duration::from_secs(30));
    assert!(
        status.success(),
        "installed Hook failed: {}",
        fs::read_to_string(stderr.path()).unwrap()
    );
    serde_json::from_slice(&fs::read(stdout.path()).unwrap()).unwrap()
}

struct InstalledMcp {
    client: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    error_codes: Vec<String>,
}

impl InstalledMcp {
    fn start(binary: &Path, home: &Path, client: &str) -> Self {
        let mut child = Command::new(binary)
            .args(["mcp", "serve", "--client", client])
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut process = Self {
            client: client.to_owned(),
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 2,
            error_codes: Vec::new(),
        };
        process.write(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"}
        }));
        let initialized = process.read();
        assert_eq!(initialized["result"]["protocolVersion"], "2024-11-05");
        process
    }

    fn call_response(&mut self, session: &str, name: &str, mut arguments: Value) -> Value {
        let object = arguments.as_object_mut().unwrap();
        object
            .entry("agent_kind".to_owned())
            .or_insert_with(|| json!(self.client));
        object
            .entry("external_session_id".to_owned())
            .or_insert_with(|| json!(session));
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
        let response = self.read();
        self.record_error_code(&response);
        assert_eq!(response["id"], id, "{response:#}");
        response
    }

    fn call_serialized_arguments(&mut self, name: &str, serialized_arguments: &[u8]) -> Value {
        let arguments: Value = serde_json::from_slice(serialized_arguments).unwrap();
        assert!(arguments.is_object());
        let id = self.next_id;
        self.next_id += 1;
        let prefix = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":{},\"arguments\":",
            serde_json::to_string(name).unwrap()
        );
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(prefix.as_bytes()).unwrap();
        stdin.write_all(serialized_arguments).unwrap();
        stdin.write_all(b"}}\n").unwrap();
        stdin.flush().unwrap();
        let response = self.read();
        self.record_error_code(&response);
        assert_eq!(response["id"], id, "{response:#}");
        assert_eq!(response["result"]["isError"], false, "{response:#}");
        response["result"]["structuredContent"].clone()
    }

    fn call(&mut self, session: &str, name: &str, arguments: Value) -> Value {
        let response = self.call_response(session, name, arguments);
        assert_eq!(response["result"]["isError"], false, "{response:#}");
        response["result"]["structuredContent"].clone()
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = wait_child(
            &mut self.child,
            "installed MCP shutdown",
            Duration::from_secs(30),
        );
        assert!(status.success());
    }

    fn assert_no_invalid_stale_or_conflict(&self) {
        let rejected = self
            .error_codes
            .iter()
            .filter(|code| {
                code.as_str() == "invalid_input"
                    || code.contains("stale")
                    || code.contains("conflict")
            })
            .collect::<Vec<_>>();
        assert!(
            rejected.is_empty(),
            "unexpected MCP rejections: {rejected:?}"
        );
    }

    fn record_error_code(&mut self, response: &Value) {
        if response["result"]["isError"] == true
            && let Some(code) = response["result"]["structuredContent"]["error"]["code"].as_str()
        {
            self.error_codes.push(code.to_owned());
        }
    }

    fn write(&mut self, value: &Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{value}").unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "installed MCP closed before responding");
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for InstalledMcp {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            let _killed = self.child.kill();
            let _status = self.child.wait();
        }
    }
}

fn initialize_repository(path: &Path, source: &str) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("src/live.rs"), source).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "src/live.rs"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.name=Live Host Acceptance",
                "-c",
                "user.email=live-host@example.invalid",
                "commit",
                "-q",
                "-m",
                "Add live host source",
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::canonicalize(path).unwrap()
}

/// Builds a probe `PATH` that mimics a real Cursor install: `cursor-agent` is the CLI that runs
/// the Hook and reports a date-like build id, while `cursor` is the desktop shim reporting semver.
/// Setup must record the `cursor-agent` value, because that is the host the Hook belongs to.
fn live_host_probe_path(temporary_home: &Path) -> (String, String, String) {
    let codex_version = "0.147.0".to_owned();
    let cursor_version = "2026.08.25-3e8eec8".to_owned();
    let cursor_shim_version = "3.13.10".to_owned();
    let wrappers = temporary_home.join("live host probe bin");
    fs::create_dir_all(&wrappers).unwrap();
    for (agent, version) in [
        ("codex", &codex_version),
        ("cursor-agent", &cursor_version),
        ("cursor", &cursor_shim_version),
    ] {
        let wrapper = wrappers.join(agent);
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\n[ \"$1\" = \"--version\" ] || exit 64\nprintf '%s\\n' '{version}'\n"
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(wrapper, permissions).unwrap();
    }
    (
        format!(
            "{}:{}",
            wrappers.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        ),
        codex_version,
        cursor_version,
    )
}

fn configured_commands(home: &Path) -> (String, String) {
    let codex: Value =
        serde_json::from_slice(&fs::read(home.join(".codex/hooks.json")).unwrap()).unwrap();
    let codex_command = codex["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    let cursor: Value =
        serde_json::from_slice(&fs::read(home.join(".cursor/hooks.json")).unwrap()).unwrap();
    let cursor_command = cursor["hooks"]["sessionStart"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    (codex_command, cursor_command)
}

fn codex_event(event: &str, session: &str, checkout: &Path) -> Value {
    let mut payload = json!({
        "session_id": session,
        "transcript_path": null,
        "cwd": checkout,
        "hook_event_name": event,
        "model": "gpt-5.6-sol",
        "permission_mode": "default"
    });
    match event {
        "SessionStart" => payload["source"] = json!("startup"),
        "PostToolUse" => {
            payload["turn_id"] = json!(format!("turn-{session}"));
            payload["tool_name"] = json!("Read");
            payload["tool_use_id"] = json!(format!("tool-{session}"));
            payload["tool_input"] = json!({
                "absolute_file_path": checkout.join("src/live.rs"),
                "raw_marker": "RAW_LIVE_HOST_CODEX_A"
            });
            payload["tool_response"] = json!({"output": "RAW_LIVE_HOST_CODEX_A"});
        }
        "SessionEnd" => payload["reason"] = json!("other"),
        _ => unreachable!(),
    }
    payload
}

fn cursor_event(event: &str, session: &str, checkout: &Path, version: &str) -> Value {
    let mut payload = json!({
        "conversation_id": session,
        "generation_id": format!("generation-{event}"),
        "model": "claude-opus-4-7-thinking-max",
        "model_id": "claude-opus-4-7",
        "model_params": [],
        "hook_event_name": event,
        "cursor_version": version,
        "workspace_roots": [checkout],
        "user_email": null,
        "transcript_path": null
    });
    match event {
        "sessionStart" => {
            payload["session_id"] = json!(session);
            payload["is_background_agent"] = json!(false);
            payload["composer_mode"] = json!("agent");
        }
        "beforeSubmitPrompt" => payload["prompt"] = json!("exercise live Cursor lifecycle"),
        "postToolUse" => {
            payload["tool_name"] = json!("Shell");
            payload["tool_input"] = json!({
                "command": "cargo test",
                "working_directory": checkout,
                "raw_marker": "RAW_LIVE_HOST_CURSOR"
            });
            payload["tool_output"] = json!("RAW_LIVE_HOST_CURSOR");
            payload["tool_use_id"] = json!("tool-live-cursor");
            payload["cwd"] = json!(checkout);
            payload["duration"] = json!(1);
        }
        "preCompact" => {
            payload["trigger"] = json!("auto");
            payload["context_usage_percent"] = json!(85);
            payload["context_tokens"] = json!(120_000);
            payload["context_window_size"] = json!(128_000);
            payload["message_count"] = json!(45);
            payload["messages_to_compact"] = json!(30);
            payload["is_first_compaction"] = json!(true);
        }
        "stop" => {
            payload["status"] = json!("completed");
            payload["loop_count"] = json!(0);
        }
        "sessionEnd" => {
            payload["session_id"] = json!(session);
            payload["reason"] = json!("completed");
            payload["duration_ms"] = json!(1);
            payload["is_background_agent"] = json!(false);
            payload["final_status"] = json!("completed");
        }
        _ => unreachable!(),
    }
    payload
}

fn git_contains(repository: &Path, needle: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["grep", "-q", needle, "HEAD", "--", "events"])
        .status()
        .unwrap()
        .success()
}

fn git(repository: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn event_count(repository: &Path) -> usize {
    git(
        repository,
        &["ls-tree", "-r", "--name-only", "HEAD", "--", "events"],
    )
    .lines()
    .filter(|path| !path.is_empty())
    .count()
}

fn commit_count(repository: &Path) -> usize {
    git(repository, &["rev-list", "--count", "HEAD"])
        .parse()
        .unwrap()
}

fn assert_zero_capture_residue(root: &Path) {
    for removed in ["capture", "capture.lock", "capture-metadata.json"] {
        assert!(!root.join("state").join(removed).exists());
    }
    for runtime_file in ["runtime.sqlite", "runtime.sqlite-wal"] {
        let path = root.join("state").join(runtime_file);
        if path.is_file() {
            let mut bytes = fs::read(path).unwrap();
            bytes.make_ascii_lowercase();
            assert!(
                !bytes
                    .windows(b"capture".len())
                    .any(|window| window == b"capture"),
                "runtime storage retained a Capture schema or row"
            );
        }
    }
}

fn assert_checkpoint_replay(first: &Value, replay: &Value) {
    assert_eq!(replay["status"], "accepted");
    assert_eq!(replay["replayed"], true);
    for field in [
        "operation_id",
        "checkpoint_id",
        "claim_ids",
        "episode_id",
        "episode_version",
        "episode_status",
        "candidate_build",
    ] {
        assert_eq!(replay[field], first[field], "replay changed {field}");
    }
}

fn assert_only_untrusted_candidate_precedes_confirmation(
    repository: &Path,
    commits_before_checkpoint: usize,
    events_before_checkpoint: usize,
) {
    assert_eq!(commit_count(repository), commits_before_checkpoint + 1);
    assert_eq!(event_count(repository), events_before_checkpoint + 1);
    assert!(git_contains(repository, "context_candidate.created"));
    for forbidden in [
        "candidate.confirmed",
        "context.revision_added",
        "context.reviewed",
        "context.publication_changed",
        "context.space_association_changed",
    ] {
        assert!(
            !git_contains(repository, forbidden),
            "governed fact {forbidden} appeared before explicit confirmation"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn installed_codex_direct_evidence_replay_recovery_and_cursor_lifecycle_use_public_processes() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("installed live hosts home");
    fs::create_dir_all(&home).unwrap();
    let (probe_path, codex_version, cursor_version) = live_host_probe_path(&home);
    eprintln!("live-host stage=version-probes");
    let source_binary = PathBuf::from(env!("CARGO_BIN_EXE_sctx"));
    let setup_stdout = tempfile::NamedTempFile::new().unwrap();
    let setup_stderr = tempfile::NamedTempFile::new().unwrap();
    let mut setup = Command::new(&source_binary)
        .args(["--json", "setup", "--agents", "cursor,codex"])
        .env("HOME", &home)
        .env("PATH", probe_path)
        .stdout(Stdio::from(setup_stdout.reopen().unwrap()))
        .stderr(Stdio::from(setup_stderr.reopen().unwrap()))
        .spawn()
        .unwrap();
    let setup = wait_child(&mut setup, "installed setup", Duration::from_secs(30));
    assert!(
        setup.success(),
        "setup failed: {}",
        fs::read_to_string(setup_stderr.path()).unwrap()
    );
    let setup: Value = serde_json::from_slice(&fs::read(setup_stdout.path()).unwrap()).unwrap();
    eprintln!("live-host stage=installed");
    let installed = home.join(".shared-context/bin/current/sctx");
    let root = home.join(".shared-context");
    assert!(installed.is_file());
    assert_eq!(setup["runtime"], installed.to_string_lossy().as_ref());

    let (codex_hook, cursor_hook) = configured_commands(&home);
    assert!(codex_hook.contains(&format!("--agent-version '{codex_version}'")));
    assert!(
        cursor_hook.contains(&format!("--agent-version '{cursor_version}'")),
        "cursor_version={cursor_version:?} hook={cursor_hook:?}"
    );
    assert!(codex_hook.contains(installed.to_str().unwrap()));
    assert!(cursor_hook.contains(installed.to_str().unwrap()));
    let cursor_mcp: Value =
        serde_json::from_slice(&fs::read(home.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(
        cursor_mcp["mcpServers"]["shared-context"]["command"],
        installed.to_string_lossy().as_ref()
    );
    assert_eq!(
        cursor_mcp["mcpServers"]["shared-context"]["args"],
        json!(["mcp", "serve", "--client", "cursor"])
    );

    let source = "pub fn live_host_portable_contract() -> bool { true }\n";
    let checkout_a = initialize_repository(&home.join("checkout A"), source);
    let checkout_b = initialize_repository(&home.join("checkout B"), source);
    let repository = run_json_cli(
        &installed,
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            "Server",
            "--path",
            checkout_a.to_str().unwrap(),
            "--path",
            checkout_b.to_str().unwrap(),
        ],
    );
    eprintln!("live-host stage=repository-catalog");
    assert_eq!(
        repository["data"]["catalog"]["repository"]["repository_id"],
        "Server"
    );
    assert_eq!(
        repository["data"]["catalog"]["repository"]["checkout_paths"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let session_a = "live-codex-session-a";
    assert_eq!(
        run_configured_hook(
            &home,
            &codex_hook,
            &codex_event("SessionStart", session_a, &checkout_a)
        ),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, session_a)
        }})
    );
    let mut codex_mcp = InstalledMcp::start(&installed, &home, "codex");
    let task_a = codex_mcp.call(
        session_a,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "Publish the livehostportablecontract from checkout A"}
        }),
    );
    eprintln!("live-host stage=codex-a-task");
    let scan = codex_mcp.call(
        session_a,
        "repository_scan",
        json!({"checkout_path": checkout_a, "paths": ["src/live.rs"]}),
    );
    eprintln!("live-host stage=codex-a-scan");
    assert!(
        scan["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| {
                artifact["kind"] == "file" && artifact["locator"]["path"] == "src/live.rs"
            })
    );
    assert_eq!(
        run_configured_hook(
            &home,
            &codex_hook,
            &codex_event("PostToolUse", session_a, &checkout_a)
        ),
        json!({})
    );
    assert_zero_capture_residue(&root);
    let knowledge = root.join("repository");
    let knowledge_head_before_checkpoint = git(&knowledge, &["rev-parse", "HEAD"]);
    let commits_before_checkpoint = commit_count(&knowledge);
    let events_before_checkpoint = event_count(&knowledge);
    let claim_statement = "livehostportablecontract resolves across installed Codex checkouts";
    let claim_rationale = "Session A authored a focused portable contract conclusion";
    let evidence_summary = "the installed source contract was reviewed";
    let evidence_limitations = json!(["local installed-host fixture"]);
    let checkpoint_arguments = json!({
        "agent_kind": "codex",
        "external_session_id": session_a,
        "claims": [{
            "context_kind": "contract",
            "statement": claim_statement,
            "rationale": claim_rationale,
            "conditions": [],
            "evidence": [{
                "evidence_type": "source_snapshot",
                "summary": evidence_summary,
                "limitations": evidence_limitations
            }]
        }],
        "unknowns": []
    });
    let checkpoint_bytes = serde_json::to_vec(&checkpoint_arguments).unwrap();
    let checkpoint = codex_mcp.call_serialized_arguments("task_checkpoint", &checkpoint_bytes);
    assert_eq!(checkpoint["status"], "accepted");
    assert_eq!(checkpoint["replayed"], false);
    assert_eq!(checkpoint["candidate_build"]["status"], "pending");
    assert_eq!(
        git(&knowledge, &["rev-parse", "HEAD"]),
        knowledge_head_before_checkpoint
    );
    assert_eq!(event_count(&knowledge), events_before_checkpoint);

    let byte_same_replay =
        codex_mcp.call_serialized_arguments("task_checkpoint", &checkpoint_bytes);
    assert_checkpoint_replay(&checkpoint, &byte_same_replay);
    let mut semantic_replay_bytes = checkpoint_bytes.clone();
    semantic_replay_bytes.push(b' ');
    assert_ne!(semantic_replay_bytes, checkpoint_bytes);
    assert_eq!(
        serde_json::from_slice::<Value>(&semantic_replay_bytes).unwrap(),
        checkpoint_arguments
    );
    let semantic_same_replay =
        codex_mcp.call_serialized_arguments("task_checkpoint", &semantic_replay_bytes);
    assert_checkpoint_replay(&checkpoint, &semantic_same_replay);
    assert_eq!(
        git(&knowledge, &["rev-parse", "HEAD"]),
        knowledge_head_before_checkpoint
    );
    assert_eq!(event_count(&knowledge), events_before_checkpoint);
    assert_zero_capture_residue(&root);

    let candidates = codex_mcp.call(
        session_a,
        "candidate_list",
        json!({"status": "pending", "limit": 10, "token_budget": 32768}),
    );
    assert_eq!(
        candidates["recovery"],
        json!({
            "attempted": 1,
            "recovered": 1,
            "failed_attempts": 0,
            "pending": 0,
            "incomplete": 0
        })
    );
    assert_eq!(candidates["reviews"].as_array().unwrap().len(), 1);
    let candidate_id = candidates["reviews"][0]["candidate_id"].as_str().unwrap();
    let review = codex_mcp.call(
        session_a,
        "candidate_get",
        json!({"candidate_id": candidate_id}),
    );
    assert_eq!(review["ready_for_review"], true, "{review:#}");
    assert_eq!(review["untrusted_data"], true);
    assert_eq!(review["review_status"], "pending");
    assert_eq!(
        review["unknowns"],
        json!([{
            "statement": "Decision or Contract topic key remains unclassified",
            "blocking": false,
            "recheck_when": ["Before Candidate confirmation"]
        }])
    );
    assert_eq!(
        review["source_episode"]["episode_id"],
        checkpoint["episode_id"]
    );
    assert_eq!(review["final_checkpoint_id"], checkpoint["checkpoint_id"]);
    assert_eq!(review["checkpoint_id"], checkpoint["checkpoint_id"]);
    assert_eq!(review["claim_id"], checkpoint["claim_ids"][0]);
    assert_eq!(review["content"]["kind"], "contract");
    assert_eq!(review["content"]["statement"], claim_statement);
    assert_eq!(review["content"]["rationale"], claim_rationale);
    assert_eq!(review["content"]["applicability"]["conditions"], json!([]));
    let evidence = &review["content"]["evidence"][0];
    assert_eq!(evidence["kind"], "source_snapshot");
    assert_eq!(evidence["supports"], claim_statement);
    assert_eq!(evidence["content"], json!({"summary": evidence_summary}));
    assert_eq!(evidence["interpretation"], claim_rationale);
    assert_eq!(evidence["limitations"], evidence_limitations);
    assert_only_untrusted_candidate_precedes_confirmation(
        &knowledge,
        commits_before_checkpoint,
        events_before_checkpoint,
    );
    let unconfirmed_search = codex_mcp.call(
        session_a,
        "context_search",
        json!({
            "query": claim_statement,
            "statuses": ["accepted"],
            "page_size": 10
        }),
    );
    assert!(unconfirmed_search["results"].as_array().unwrap().is_empty());
    assert_zero_capture_residue(&root);
    let recommendation_id = review["space_recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recommendation| recommendation["kind"] == "proposed_new_space_intent")
        .unwrap()["recommendation_id"]
        .clone();
    let confirmed = codex_mcp.call(
        session_a,
        "candidate_confirm",
        json!({
            "expected_task_id": task_a["task_id"],
            "expected_intent_revision_id": task_a["intent_revision_id"],
            "candidate_id": candidate_id,
            "expected_review_version": review["review_version"],
            "primary": {"new_space_recommendation_id": recommendation_id},
            "related_space_ids": []
        }),
    );
    eprintln!("live-host stage=codex-a-confirmed");
    assert_eq!(confirmed["event_ids"].as_array().unwrap().len(), 5);
    assert_eq!(confirmed["graph_rebuild_pending"], false);
    assert_eq!(commit_count(&knowledge), commits_before_checkpoint + 2);
    assert_eq!(event_count(&knowledge), events_before_checkpoint + 6);
    assert!(git_contains(&knowledge, "candidate.confirmed"));
    assert!(git_contains(&knowledge, "context.revision_added"));
    assert!(git_contains(&knowledge, "context.publication_changed"));
    assert_zero_capture_residue(&root);
    let reference = codex_mcp.call(
        session_a,
        "engineering_reference_record",
        json!({
            "context_id": confirmed["context_id"],
            "revision_id": confirmed["revision_id"],
            "repository_id": "Server",
            "artifact_kind": "file",
            "relation": "implements",
            "locator": {"locator_kind": "file", "path": "src/live.rs"},
            "supports": "The confirmed direct Evidence Context is implemented by src/live.rs",
            "limitations": ["local installed-host fixture"]
        }),
    );
    assert!(
        reference["reference_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("ref_"))
    );
    let rebuilt = codex_mcp.call(
        session_a,
        "association_rebuild",
        json!({"diagnose_only": false}),
    );
    assert!(rebuilt["generation"].as_u64().is_some());

    let session_b = "live-codex-session-b";
    assert_eq!(
        run_configured_hook(
            &home,
            &codex_hook,
            &codex_event("SessionStart", session_b, &checkout_b)
        ),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(AgentKind::Codex, session_b)
        }})
    );
    let task_b = codex_mcp.call(
        session_b,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "Retrieve livehostportablecontract from checkout B"}
        }),
    );
    let focus = codex_mcp.call(
        session_b,
        "task_artifact_focus",
        json!({
            "expected_revision_id": task_b["intent_revision_id"],
            "absolute_file_path": checkout_b.join("src/live.rs"),
            "locator": {"locator_kind": "file"},
            "token_budget": 12_000,
            "max_spaces": 8,
            "detail_level": "full"
        }),
    );
    eprintln!("live-host stage=codex-b-focus");
    assert_eq!(focus["resolved_focus"]["repository_id"], "Server");
    assert_eq!(focus["resolved_focus"]["locator"]["path"], "src/live.rs");
    let focused = focus["context"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["context"]["context_id"] == confirmed["context_id"])
        .unwrap_or_else(|| panic!("Session B missed Session A Context: {focus:#}"));
    assert!(
        focused["retrieval_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["source"] == "engineering_graph")
    );
    let search = run_json_cli(
        &installed,
        &home,
        &[
            "search",
            "--query",
            "livehostportablecontract",
            "--status",
            "accepted",
        ],
    );
    assert!(
        search["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["context_id"] == confirmed["context_id"])
    );

    let cursor_session = "live-cursor-session";
    eprintln!("live-host stage=cursor-start");
    assert_eq!(
        run_configured_hook(
            &home,
            &cursor_hook,
            &cursor_event("sessionStart", cursor_session, &checkout_b, &cursor_version)
        ),
        json!({
            "additional_context":
                shared_context_activation_marker(AgentKind::Cursor, cursor_session)
        })
    );
    assert_eq!(
        run_configured_hook(
            &home,
            &cursor_hook,
            &cursor_event(
                "beforeSubmitPrompt",
                cursor_session,
                &checkout_b,
                &cursor_version,
            )
        ),
        json!({})
    );
    let mut cursor_mcp = InstalledMcp::start(&installed, &home, "cursor");
    let _cursor_task = cursor_mcp.call(
        cursor_session,
        "task_intent_update",
        json!({
            "task_boundary": "new",
            "expected_revision_id": null,
            "intent": {"goal": "Exercise the installed Cursor lifecycle"}
        }),
    );
    eprintln!("live-host stage=cursor-task");
    assert_eq!(
        run_configured_hook(
            &home,
            &cursor_hook,
            &cursor_event("postToolUse", cursor_session, &checkout_b, &cursor_version)
        ),
        json!({})
    );
    let compact = run_configured_hook(
        &home,
        &cursor_hook,
        &cursor_event("preCompact", cursor_session, &checkout_b, &cursor_version),
    );
    assert!(
        compact["user_message"]
            .as_str()
            .is_some_and(|message| message.contains("task_checkpoint"))
    );
    assert!(!compact.to_string().contains("RAW_LIVE_HOST_CURSOR"));
    assert_eq!(
        run_configured_hook(
            &home,
            &cursor_hook,
            &cursor_event("stop", cursor_session, &checkout_b, &cursor_version),
        ),
        json!({})
    );
    assert_eq!(
        run_configured_hook(
            &home,
            &cursor_hook,
            &cursor_event("sessionEnd", cursor_session, &checkout_b, &cursor_version)
        ),
        json!({})
    );
    let after_end = cursor_mcp.call_response(cursor_session, "task_context", json!({}));
    eprintln!("live-host stage=cursor-ended");
    assert_eq!(after_end["result"]["isError"], true);
    assert_eq!(
        after_end["result"]["structuredContent"]["error"]["code"],
        "session_not_authorized"
    );

    for (session, checkout) in [(session_a, &checkout_a), (session_b, &checkout_b)] {
        assert_eq!(
            run_configured_hook(
                &home,
                &codex_hook,
                &codex_event("SessionEnd", session, checkout)
            ),
            json!({})
        );
    }
    cursor_mcp.finish();
    codex_mcp.assert_no_invalid_stale_or_conflict();
    codex_mcp.finish();
    for forbidden in [
        "RAW_LIVE_HOST_CODEX_A",
        "RAW_LIVE_HOST_CURSOR",
        checkout_a.to_str().unwrap(),
        checkout_b.to_str().unwrap(),
    ] {
        assert!(!git_contains(&knowledge, forbidden));
    }
}
