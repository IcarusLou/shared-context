use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
    process, thread,
    time::Duration,
};

use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Framing {
    Newline,
    ContentLength,
}

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    log(&format!("start:{}", arguments.join(",")));
    match arguments.as_slice() {
        [group, command, client_flag, client]
            if group == "mcp" && command == "serve" && client_flag == "--client" =>
        {
            serve_mcp(client);
        }
        [command, rest @ ..] if command == "hook" => serve_hook(rest),
        [flag, rest @ ..] if flag == "--json" => serve_cli(rest),
        _ => process::exit(9),
    }
}

fn serve_cli(arguments: &[String]) {
    if arguments.first().is_some_and(|value| value == "fake-exit") {
        eprintln!("SECRET_CHILD_STDERR_MUST_NOT_ESCAPE");
        process::exit(17);
    }
    if arguments
        .first()
        .is_some_and(|value| value == "fake-invalid")
    {
        print!("not-json SECRET_CHILD_STDOUT_MUST_NOT_ESCAPE");
        return;
    }
    if arguments.first().is_some_and(|value| value == "fake-sleep") {
        let milliseconds = arguments
            .get(1)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1_000);
        log("sleep-start");
        thread::sleep(Duration::from_millis(milliseconds));
        log("sleep-end");
    }
    let value = arguments.last().cloned().unwrap_or_default();
    let aliased_data = if arguments
        .first()
        .is_some_and(|value| value == "fake-secret-data")
    {
        Value::String("SECRET_ALIASED_RAW_DATA_MUST_NOT_ESCAPE".to_owned())
    } else {
        Value::Null
    };
    println!(
        "{}",
        json!({
            "command": "fake",
            "tree": "0000000000000000000000000000000000000000",
            "generation": 1,
            "data": {
                "value": value,
                "data": aliased_data,
                "argument_count": arguments.len(),
                "network_disabled": network_disabled(),
                "model_home_isolated": model_home_isolated(),
                "process_id": process::id()
            }
        })
    );
}

fn serve_hook(_arguments: &[String]) {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input).unwrap();
    let payload: Value = serde_json::from_slice(&input).unwrap();
    if let Some(milliseconds) = payload.get("sleep_ms").and_then(Value::as_u64) {
        thread::sleep(Duration::from_millis(milliseconds));
    }
    if payload.get("force_exit").and_then(Value::as_bool) == Some(true) {
        eprintln!("SECRET_HOOK_STDERR_MUST_NOT_ESCAPE");
        process::exit(18);
    }
    if payload.get("persist_canary").and_then(Value::as_bool) == Some(true)
        && let Some(canary) = find_canary(&payload)
    {
        let root = PathBuf::from(env::var_os("HOME").unwrap()).join(".shared-context");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("fake-canary-leak.bin"), canary.as_bytes()).unwrap();
    }
    println!(
        "{}",
        json!({
            "ok": true,
            "event": payload.get("hook_event_name").cloned().unwrap_or(Value::Null),
            "value": payload.get("value").cloned().unwrap_or(Value::Null),
            "network_disabled": network_disabled(),
            "model_home_isolated": model_home_isolated(),
            "process_id": process::id()
        })
    );
}

#[allow(clippy::too_many_lines)]
fn serve_mcp(_client: &str) {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut sequence = 0_u64;
    while let Some((request, framing)) = read_frame(&mut reader) {
        sequence = sequence.saturating_add(1);
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let response = match request.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
            }),
            Some("tools/call") => {
                let name = request
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if name == "fake-exit" {
                    eprintln!("SECRET_MCP_STDERR_MUST_NOT_ESCAPE");
                    process::exit(19);
                }
                if name == "fake-typed-failure" {
                    if arguments.get("mutate_semantic").and_then(Value::as_bool) == Some(true) {
                        mutate_semantic_state();
                    }
                    let code = arguments
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("fake_failure");
                    let kind = arguments
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("invalid_input");
                    let response = json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "SECRET_TYPED_FAILURE_BODY"}],
                            "structuredContent": {"error": {
                                "code": code, "kind": kind,
                                "message": "SECRET_TYPED_FAILURE_MESSAGE"
                            }},
                            "isError": true
                        }
                    });
                    write_frame(&mut writer, &response, framing);
                    continue;
                }
                if name == "fake-sleep" {
                    let milliseconds = arguments
                        .get("sleep_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(1_000);
                    log("mcp-sleep-start");
                    thread::sleep(Duration::from_millis(milliseconds));
                    log("mcp-sleep-end");
                }
                let mut data = arguments.as_object().cloned().unwrap_or_default();
                if name == "fake-open-episode" {
                    data.insert(
                        "episode_id".to_owned(),
                        Value::String(create_open_episode_state()),
                    );
                }
                data.insert("sequence".to_owned(), json!(sequence));
                data.insert("network_disabled".to_owned(), json!(network_disabled()));
                data.insert(
                    "model_home_isolated".to_owned(),
                    json!(model_home_isolated()),
                );
                data.insert("process_id".to_owned(), json!(process::id()));
                data.entry("task_id".to_owned())
                    .or_insert_with(|| json!(format!("tsk_{}", uuid::Uuid::new_v4().hyphenated())));
                data.entry("revision_id".to_owned())
                    .or_insert_with(|| json!(format!("tir_{}", uuid::Uuid::new_v4().hyphenated())));
                if name == "candidate_get" {
                    data.entry("candidate_id".to_owned()).or_insert_with(|| {
                        json!(format!("cnd_{}", uuid::Uuid::new_v4().hyphenated()))
                    });
                    data.entry("source_episode".to_owned()).or_insert_with(|| {
                        json!({"episode_id": format!("wep_{}", uuid::Uuid::new_v4().hyphenated())})
                    });
                }
                if name == "candidate_confirm" {
                    data.entry("confirmation_id".to_owned()).or_insert_with(|| {
                        json!(format!("cfm_{}", uuid::Uuid::new_v4().hyphenated()))
                    });
                    data.entry("event_ids".to_owned()).or_insert_with(|| {
                        Value::Array(
                            (0..5)
                                .map(|_| {
                                    json!(format!("evt_{}", uuid::Uuid::new_v4().hyphenated()))
                                })
                                .collect(),
                        )
                    });
                    data.entry("batch_id".to_owned()).or_insert_with(|| {
                        json!(format!("bat_{}", uuid::Uuid::new_v4().hyphenated()))
                    });
                    data.entry("commit_oid".to_owned())
                        .or_insert_with(|| json!("0123456789abcdef0123456789abcdef01234567"));
                    data.entry("status".to_owned())
                        .or_insert_with(|| json!("confirmed"));
                    data.entry("created".to_owned())
                        .or_insert_with(|| json!(true));
                    if arguments
                        .get("nested_confirmation")
                        .and_then(Value::as_bool)
                        == Some(true)
                        && let Some(confirmation) = data.remove("confirmation_id")
                    {
                        data.insert(
                            "nested".to_owned(),
                            json!({"confirmation_id": confirmation}),
                        );
                    }
                    if arguments.get("duplicate_events").and_then(Value::as_bool) == Some(true)
                        && let Some(events) =
                            data.get_mut("event_ids").and_then(Value::as_array_mut)
                        && events.len() > 1
                    {
                        events[1] = events[0].clone();
                    }
                    if arguments.get("missing_batch").and_then(Value::as_bool) == Some(true) {
                        data.remove("batch_id");
                    }
                    if arguments.get("invalid_batch").and_then(Value::as_bool) == Some(true) {
                        data.insert("batch_id".to_owned(), json!("invalid-batch"));
                    }
                    if arguments.get("missing_commit").and_then(Value::as_bool) == Some(true) {
                        data.remove("commit_oid");
                    }
                    if arguments.get("invalid_commit").and_then(Value::as_bool) == Some(true) {
                        data.insert("commit_oid".to_owned(), json!("ABCDEF"));
                    }
                }
                if name == "task_artifact_focus" {
                    // A Focus anchors the file its coordinate lives in; it does not consult the
                    // Engineering Graph (ADR-0007). The path rides on the item it explains,
                    // because that is the only place the real response carries one.
                    data.insert(
                        "items".to_owned(),
                        json!([{"retrieval_paths": [{"source": "file_anchor"}]}]),
                    );
                }
                if name == "task_context"
                    && arguments.get("emit_hint").and_then(Value::as_bool) == Some(true)
                {
                    // A Working Intent Hint that names a file is a Lane A anchor now, so the route
                    // it produces is `file_anchor` (ADR-0007). The scenario invariant still asks
                    // the same question: the Hint reached knowledge, and no Graph path came with it.
                    data.insert(
                        "items".to_owned(),
                        json!([{"retrieval_paths": [{"source": "file_anchor"}]}]),
                    );
                }
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"content": [], "structuredContent": data, "isError": false}
                })
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "SECRET_REMOTE_MESSAGE"}
            }),
        };
        write_frame(&mut writer, &response, framing);
    }
}

fn find_canary(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) if value.starts_with("sctx-canary-") => Some(value),
        Value::Array(values) => values.iter().find_map(find_canary),
        Value::Object(values) => values.values().find_map(find_canary),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn mutate_semantic_state() {
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    let state = home.join(".shared-context/state");
    fs::create_dir_all(&state).unwrap();
    let connection = rusqlite::Connection::open(state.join("runtime.sqlite")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS external_session (
                external_session_key TEXT PRIMARY KEY,
                active_task_id TEXT NOT NULL,
                active_task_session_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS task_session (
                task_session_id TEXT PRIMARY KEY,
                current_intent_revision_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS task_intent_revision (
                task_session_id TEXT NOT NULL,
                revision_id TEXT PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS work_episode (
                episode_id TEXT PRIMARY KEY,
                status TEXT NOT NULL
             );
             DELETE FROM external_session;
             DELETE FROM task_session;
             DELETE FROM task_intent_revision;
             PRAGMA user_version = 11;",
        )
        .unwrap();
    let task = format!("tsk_{}", uuid::Uuid::new_v4().hyphenated());
    let task_session = format!("tss_{}", uuid::Uuid::new_v4().hyphenated());
    let revision = format!("tir_{}", uuid::Uuid::new_v4().hyphenated());
    connection
        .execute(
            "INSERT INTO external_session VALUES ('scenario-session', ?1, ?2)",
            rusqlite::params![task, task_session],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO task_session VALUES (?1, ?2)",
            rusqlite::params![task_session, revision],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO task_intent_revision VALUES (?1, ?2)",
            rusqlite::params![task_session, revision],
        )
        .unwrap();
}

fn create_open_episode_state() -> String {
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    let state = home.join(".shared-context/state");
    fs::create_dir_all(&state).unwrap();
    let episode = format!("wep_{}", uuid::Uuid::new_v4().hyphenated());
    let runtime = rusqlite::Connection::open(state.join("runtime.sqlite")).unwrap();
    runtime
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS work_episode (
                episode_id TEXT PRIMARY KEY,
                status TEXT NOT NULL
             );
             DELETE FROM work_episode;
             PRAGMA user_version = 11;",
        )
        .unwrap();
    runtime
        .execute("INSERT INTO work_episode VALUES (?1, 'open')", [&episode])
        .unwrap();
    let index = rusqlite::Connection::open(state.join("index.sqlite")).unwrap();
    index
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS context_candidate (
                candidate_id TEXT PRIMARY KEY,
                source_episode_id TEXT NOT NULL
             );
             DELETE FROM meta;
             DELETE FROM context_candidate;
             INSERT INTO meta VALUES ('indexed_tree_oid', '4444444444444444444444444444444444444444');
             INSERT INTO meta VALUES ('projection_generation', '1');
             PRAGMA user_version = 11;",
        )
        .unwrap();
    episode
}

fn read_frame(reader: &mut impl BufRead) -> Option<(Value, Framing)> {
    let mut first = String::new();
    if reader.read_line(&mut first).ok()? == 0 {
        return None;
    }
    if first.to_ascii_lowercase().starts_with("content-length:") {
        let length = first.split_once(':')?.1.trim().parse::<usize>().ok()?;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).ok()?;
            if header == "\r\n" || header == "\n" {
                break;
            }
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).ok()?;
        Some((serde_json::from_slice(&body).ok()?, Framing::ContentLength))
    } else {
        Some((serde_json::from_str(first.trim()).ok()?, Framing::Newline))
    }
}

fn write_frame(writer: &mut impl Write, value: &Value, framing: Framing) {
    let body = serde_json::to_vec(value).unwrap();
    match framing {
        Framing::Newline => {
            writer.write_all(&body).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        Framing::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
            writer.write_all(&body).unwrap();
        }
    }
    writer.flush().unwrap();
}

fn log(line: &str) {
    let Some(home) = env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join("fake-executions.log");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
}

fn network_disabled() -> bool {
    env::var("SCTX_SCENARIO_NETWORK").as_deref() == Ok("disabled")
        && env::var("HTTP_PROXY").is_ok_and(|value| value == "http://127.0.0.1:9")
        && env::var("NO_PROXY").is_ok_and(|value| value.is_empty())
}

fn model_home_isolated() -> bool {
    let Some(home) = env::var_os("HOME") else {
        return false;
    };
    let Some(model_home) = env::var_os("CODEX_HOME") else {
        return false;
    };
    PathBuf::from(model_home).starts_with(PathBuf::from(home))
}
