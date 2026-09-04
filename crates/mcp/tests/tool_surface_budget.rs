//! Freezes the public tool surface's two separable halves.
//!
//! A tool description is the one text a Skill-less host always renders, so it is the channel
//! every prompt-side rule has to travel through — and the only channel with no natural limit.
//! `inputSchema` is the opposite: hosts generate client code from it, so a byte of drift there
//! is a compatibility event, not a wording choice.
//!
//! So the two are asserted separately. Every `inputSchema` is byte-frozen against a golden
//! captured from the tool surface as it shipped; descriptions may grow, but only inside a stated
//! budget, which forces each added sentence to be worth its bytes.
//!
//! Regenerate the golden — only together with a deliberate, reviewed schema change — with
//! `SCTX_UPDATE_TOOL_SURFACE_GOLDEN=1 cargo test -p sctx-mcp --test tool_surface_budget`.

use std::io::{BufReader, Cursor};

use sctx_mcp::{ClientKind, McpServer};
use serde_json::{Value, json};

/// Byte-frozen `inputSchema` of every public tool, keyed by tool name.
const SCHEMA_GOLDEN: &str = include_str!("fixtures/tool_input_schemas.json");

/// Total description bytes of the seventeen public tools as of `wp/v2-prompts`' parent commit,
/// measured after `tool_schema` appended its shared sentences.
const DESCRIPTION_BASELINE_BYTES: usize = 6644;

/// What the three-tier disposition policy is allowed to add on top of that baseline.
///
/// The same budget U1 held itself to. It is a discipline, not a capacity: the descriptions are
/// read on every single tools/list, by every host, for the whole session.
const DESCRIPTION_BUDGET_BYTES: usize = 1536;

fn tools() -> Vec<Value> {
    let temporary = tempfile::tempdir().unwrap();
    let mut server = McpServer::new(temporary.path(), ClientKind::Codex).unwrap();
    let mut input = Vec::new();
    for value in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ] {
        input.extend_from_slice(&serde_json::to_vec(&value).unwrap());
        input.push(b'\n');
    }
    let mut output = Vec::new();
    server
        .serve(&mut BufReader::new(Cursor::new(input)), &mut output)
        .unwrap();
    let responses: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    responses[1]["result"]["tools"].as_array().unwrap().clone()
}

fn golden_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tool_input_schemas.json")
}

#[test]
fn public_tool_input_schemas_are_byte_frozen() {
    let tools = tools();
    let mut observed = serde_json::Map::new();
    for tool in &tools {
        observed.insert(
            tool["name"].as_str().unwrap().to_owned(),
            tool["inputSchema"].clone(),
        );
    }
    let mut rendered = serde_json::to_string_pretty(&Value::Object(observed)).unwrap();
    rendered.push('\n');

    if std::env::var_os("SCTX_UPDATE_TOOL_SURFACE_GOLDEN").is_some() {
        let path = golden_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rendered.as_bytes()).unwrap();
        return;
    }

    assert_eq!(
        rendered, SCHEMA_GOLDEN,
        "a public inputSchema changed; hosts generate client code from these, so regenerate the \
         golden only with a deliberate schema change"
    );
}

#[test]
fn tool_descriptions_stay_inside_their_budget() {
    let tools = tools();
    assert_eq!(tools.len(), 17);
    let total: usize = tools
        .iter()
        .map(|tool| tool["description"].as_str().unwrap().len())
        .sum();
    let ceiling = DESCRIPTION_BASELINE_BYTES + DESCRIPTION_BUDGET_BYTES;
    assert!(
        total <= ceiling,
        "the seventeen tool descriptions total {total} bytes, over the {ceiling}-byte ceiling \
         ({DESCRIPTION_BASELINE_BYTES} baseline + {DESCRIPTION_BUDGET_BYTES} budget)"
    );
    for tool in &tools {
        let description = tool["description"].as_str().unwrap();
        assert!(
            !description.is_empty(),
            "tool {} has no description",
            tool["name"]
        );
    }
}
