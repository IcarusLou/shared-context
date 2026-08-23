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
                data.insert("sequence".to_owned(), json!(sequence));
                data.insert("network_disabled".to_owned(), json!(network_disabled()));
                data.insert(
                    "model_home_isolated".to_owned(),
                    json!(model_home_isolated()),
                );
                data.insert("process_id".to_owned(), json!(process::id()));
                data.entry("task_id".to_owned())
                    .or_insert_with(|| json!(format!("tsk_{}", uuid::Uuid::new_v4().hyphenated())));
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
