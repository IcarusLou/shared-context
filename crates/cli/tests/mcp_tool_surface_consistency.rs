//! Keeps the three places that independently know the MCP tool surface from drifting apart.

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

use serde_json::Value;
use tempfile::tempdir;

const DEMO_ORACLE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/oracles/demo-v1.json"
));

/// Guards the concrete failure that shipped once already: `space_create` was added to
/// `tools/list`, but `sctx setup --demo` still asserted a hardcoded sixteen tools (so the demo
/// broke), the Agent adapter's whitelist still knew only sixteen names (so `space_create`'s
/// `PostToolUse` was no longer classified as a neutral Shared Context call), and the demo oracle
/// still listed sixteen. Each copy is only checked against a fixture of itself, so nothing failed
/// until the demo did. This asserts the live server, the shared name list, and the oracle agree.
#[test]
fn every_surface_that_lists_the_mcp_tools_lists_the_same_ones() {
    let emitted = live_tool_names();
    let shared = sctx_agent_adapter::shared_context_tool_names();
    let oracle = serde_json::from_str::<Value>(DEMO_ORACLE).unwrap()["mcp_tools"]
        .as_array()
        .expect("demo oracle should carry an mcp_tools array")
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        emitted, oracle,
        "tests/oracles/demo-v1.json must list exactly what tools/list emits, in emission order"
    );

    assert_eq!(
        emitted.len(),
        shared.len(),
        "sctx_agent_adapter::shared_context_tool_names() must cover every emitted MCP tool"
    );
    assert_eq!(
        emitted.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        shared.iter().copied().collect::<BTreeSet<_>>(),
        "the Agent adapter whitelist and tools/list must name the same tools"
    );
}

/// Drives one real `sctx mcp serve` over stdio and returns the emitted tool names in order.
fn live_tool_names() -> Vec<String> {
    let temporary = tempdir().unwrap();
    let home: PathBuf = temporary.path().join("用户 home 空格");
    fs::create_dir_all(&home).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "cursor"])
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2024-11-05"}
    });
    let list = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{initialize}").unwrap();
    writeln!(stdin, "{list}").unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    responses[1]["result"]["tools"]
        .as_array()
        .expect("tools/list should return a tools array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}
