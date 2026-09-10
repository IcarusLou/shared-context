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

use sctx_local_state::{CHECKPOINT_SECTION_MAX_BYTES, TRIAGE_SECTION_MAX_BYTES};
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
///
/// Raised once, by 128 bytes, for the sixth "worth keeping" type accepted on 2026-09-10: the
/// triage policy has to say that a `progress` stage summary is auto-confirmable and that the
/// process-level discard ground does not swallow it. Both clauses were written as short as they
/// can be said; the raise is the deliberate decision the discipline exists to force.
const DESCRIPTION_BUDGET_BYTES: usize = 1664;

/// What this installation's `policy.md` is allowed to add on top of the protocol ceiling.
///
/// A separate number because it buys something different. The budget above disciplines *protocol*
/// wording, which only a release can change; this one bounds *team policy*, which any operator can
/// edit, and which must therefore be bounded by the code rather than by review.
///
/// It is *derived*, not chosen: the two sections that reach a tool description are `checkpoint`
/// and `triage`, and `sctx_local_state` already refuses either one over its ceiling. Restating
/// those ceilings here keeps the two ends honest -- any `policy.md` the loader accepts fits this
/// budget by construction, so a team editing their policy can never fail this test.
///
/// The strictest *host* limit we could establish is: none. Investigated 2026-09-10.
/// `docs/deferred-issues.md` and every adapter crate record nothing about description truncation.
/// Codex passes MCP descriptions straight through -- `mcp_tool_to_openai_tool` in
/// `codex-rs/core/src/tools/spec.rs` destructures `description` and forwards it, and every
/// truncation constant in that workspace (`MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES`, the
/// 256-line/10 KiB clamp, `output_token_limit`) applies to tool *output*. Cursor documents a
/// name-length limit (60 characters, server plus tool) and an active-tool cap, but no description
/// limit; its reported description truncation is forum anecdote, and the ~6,500-character figure
/// that circulates with it is blog hearsay rather than documentation. Anthropic documents
/// output-side limits only. So the binding constraint is per description, not in total, and it is
/// asserted separately by `no_single_description_approaches_the_reported_cursor_ceiling`.
const POLICY_DESCRIPTION_BUDGET_BYTES: usize =
    CHECKPOINT_SECTION_MAX_BYTES + TRIAGE_SECTION_MAX_BYTES + 2;

/// Exactly what the *shipped default* policy adds to the surface today.
///
/// The budget above is what any team may spend; this is what we spend, and it is frozen so that
/// editing `crates/local-state/src/default_policy.md` is a decision someone made on purpose rather
/// than a number that drifted. Update it together with that file.
///
/// Raised on 2026-09-10 when the Skill shrank to protocol: three "what is worth keeping" sentences
/// with no other runtime channel moved out of `references/workflow.md` into `## checkpoint`.
const DEFAULT_POLICY_DESCRIPTION_BYTES: usize = 1355;

/// A single description must stay well under the largest figure anyone reports a host truncating
/// at (~6,500 characters, Cursor, unverified). Half of it is the standing headroom.
const SINGLE_DESCRIPTION_MAX_BYTES: usize = 4096;

fn tools() -> Vec<Value> {
    let temporary = tempfile::tempdir().unwrap();
    tools_at(temporary.path())
}

/// The surface an installation gets from a `policy.md` that states no section at all, which is the
/// protocol-only baseline every budget above is measured against.
fn tools_without_policy() -> Vec<Value> {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join("policy.md"),
        "# team policy\n\nnothing stated here.\n",
    )
    .unwrap();
    tools_at(temporary.path())
}

fn tools_at(root: &std::path::Path) -> Vec<Value> {
    let mut server = McpServer::new(root, ClientKind::Codex).unwrap();
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
    let ceiling =
        DESCRIPTION_BASELINE_BYTES + DESCRIPTION_BUDGET_BYTES + POLICY_DESCRIPTION_BUDGET_BYTES;
    assert!(
        total <= ceiling,
        "the seventeen tool descriptions total {total} bytes, over the {ceiling}-byte ceiling \
         ({DESCRIPTION_BASELINE_BYTES} baseline + {DESCRIPTION_BUDGET_BYTES} protocol budget + \
         {POLICY_DESCRIPTION_BUDGET_BYTES} policy budget)"
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

/// The team policy half of the surface is bounded on its own, so an edit to `policy.md` cannot
/// quietly spend the protocol budget -- and so that raising one number is never mistaken for
/// raising the other.
#[test]
fn team_policy_text_stays_inside_its_own_budget() {
    let with_policy: usize = tools()
        .iter()
        .map(|tool| tool["description"].as_str().unwrap().len())
        .sum();
    let without_policy: usize = tools_without_policy()
        .iter()
        .map(|tool| tool["description"].as_str().unwrap().len())
        .sum();
    assert!(
        with_policy >= without_policy,
        "policy text can only add bytes"
    );
    let policy_bytes = with_policy - without_policy;
    assert!(
        policy_bytes <= POLICY_DESCRIPTION_BUDGET_BYTES,
        "policy.md adds {policy_bytes} bytes to the tool surface, over the \
         {POLICY_DESCRIPTION_BUDGET_BYTES}-byte policy budget"
    );
    assert_eq!(
        policy_bytes, DEFAULT_POLICY_DESCRIPTION_BYTES,
        "the shipped default policy changed size; update DEFAULT_POLICY_DESCRIPTION_BYTES with it"
    );
    assert!(
        without_policy <= DESCRIPTION_BASELINE_BYTES + DESCRIPTION_BUDGET_BYTES,
        "the protocol-only surface is {without_policy} bytes, over its own ceiling"
    );
}

/// The point of the whole mechanism: one team's `policy.md` reaches the model through the tool
/// surface, without a release and without touching a business Repository.
#[test]
fn an_installations_own_policy_reaches_the_task_checkpoint_and_triage_descriptions() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join("policy.md"),
        "# team\n\n## checkpoint\nKeep only what a code reviewer could not reconstruct.\n\n         ## triage\nConfirm nothing that names a person.\n",
    )
    .unwrap();
    let tools = tools_at(temporary.path());
    let description = |name: &str| {
        tools.iter().find(|tool| tool["name"] == name).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let checkpoint = description("task_checkpoint");
    assert!(checkpoint.contains("Keep only what a code reviewer could not reconstruct."));
    assert!(
        !checkpoint.contains("Also worth keeping"),
        "a stated section replaces the default outright"
    );
    assert!(description("candidate_list").contains("Confirm nothing that names a person."));
}

#[test]
fn no_single_description_approaches_the_reported_cursor_ceiling() {
    for tool in &tools() {
        let description = tool["description"].as_str().unwrap();
        assert!(
            description.len() <= SINGLE_DESCRIPTION_MAX_BYTES,
            "{} is {} bytes, over the {SINGLE_DESCRIPTION_MAX_BYTES}-byte single-description \
             ceiling",
            tool["name"],
            description.len()
        );
    }
}
