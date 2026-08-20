use sctx_adapter_cursor::{
    CanonicalAgentEventKind, ResolvedAgentAction, capabilities, decode_hook_input,
    encode_hook_output,
};
use sctx_agent_adapter::{CapabilityMode, plan_action};
use serde_json::Value;

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("../../../fixtures/agents/cursor-3.13.json")).unwrap()
}

#[test]
fn documented_cursor_3_13_shapes_map_to_all_canonical_events() {
    let actual = fixtures()
        .into_iter()
        .map(|payload| {
            let (event, version) =
                decode_hook_input(&serde_json::to_vec(&payload).unwrap()).unwrap();
            assert_eq!(version, "3.13.10");
            event.kind()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            CanonicalAgentEventKind::SessionStart,
            CanonicalAgentEventKind::PromptSubmit,
            CanonicalAgentEventKind::PostToolUse,
            CanonicalAgentEventKind::PreCompact,
            CanonicalAgentEventKind::TurnStop,
            CanonicalAgentEventKind::SessionEnd,
        ]
    );
}

#[test]
fn cursor_prompt_hook_is_observable_but_never_an_injection_dependency() {
    let payload = fixtures().remove(1);
    let (event, _) = decode_hook_input(&serde_json::to_vec(&payload).unwrap()).unwrap();
    let capability = capabilities(Some("3.13.10"), true);
    assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
    assert!(capability.prompt_submit);
    assert!(!capability.prompt_aware_injection);
    assert!(plan_action(&event, &capability).context_query.is_none());
}

#[test]
fn cursor_unknown_version_and_missing_hooks_keep_only_mcp_cli() {
    for capability in [
        capabilities(Some("4.0.0"), true),
        capabilities(Some("3.13.10"), false),
        capabilities(None, true),
    ] {
        assert_eq!(capability.mode, CapabilityMode::McpCliFallback);
        assert!(capability.mcp && capability.cli);
        assert!(!capability.session_start);
    }
}

#[test]
fn cursor_output_uses_only_documented_snake_case_context_field() {
    let output = encode_hook_output(
        CanonicalAgentEventKind::SessionStart,
        &ResolvedAgentAction {
            additional_context: None,
            system_message: Some("capability guidance".to_owned()),
        },
    )
    .unwrap();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        output,
        serde_json::json!({"additional_context":"capability guidance"})
    );
}

#[test]
fn malformed_or_unknown_cursor_payload_fails_strictly() {
    let mut payload = fixtures().remove(0);
    payload["hook_event_name"] = Value::String("futureHook".to_owned());
    assert!(decode_hook_input(&serde_json::to_vec(&payload).unwrap()).is_err());
    payload["hook_event_name"] = Value::String("sessionStart".to_owned());
    payload["workspace_roots"] = Value::String("not-an-array".to_owned());
    assert!(decode_hook_input(&serde_json::to_vec(&payload).unwrap()).is_err());
}
