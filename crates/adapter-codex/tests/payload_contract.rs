use sctx_adapter_codex::{
    CanonicalAgentEventKind, ResolvedAgentAction, TrustState, capabilities, decode_hook_input,
    encode_hook_output,
};
use sctx_agent_adapter::{CapabilityMode, plan_action};
use serde_json::Value;

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("../../../fixtures/agents/codex-0.147.json")).unwrap()
}

#[test]
fn documented_codex_0_147_shapes_map_to_all_canonical_events() {
    let actual = fixtures()
        .into_iter()
        .map(|payload| {
            decode_hook_input(&serde_json::to_vec(&payload).unwrap())
                .unwrap()
                .kind()
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
fn verified_and_trusted_codex_prompt_is_guidance_only() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(1)).unwrap()).unwrap();
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);
    assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
    assert!(capability.prompt_aware_injection);
    let action = plan_action(&event, &capability);
    assert!(action.task_operation.is_none());
    assert!(action.system_message.as_deref().is_some_and(|message| {
        message.contains("task_intent_update") && !message.contains("implement the adapter")
    }));
}

#[test]
fn codex_precompact_and_turn_stop_request_explicit_checkpoint_without_runtime_claims() {
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);
    for index in [3, 4] {
        let event =
            decode_hook_input(&serde_json::to_vec(&fixtures().remove(index)).unwrap()).unwrap();
        let action = plan_action(&event, &capability);
        assert!(action.task_operation.is_none());
        assert!(action.system_message.as_deref().is_some_and(|message| {
            message.contains("task_checkpoint")
                && message.contains("Hook summary text is not Claim evidence")
        }));
    }
}

#[test]
fn codex_unconfirmed_trust_is_action_required_and_unknown_versions_fallback() {
    let trust = capabilities(Some("0.147.0"), true, TrustState::Unconfirmed);
    assert_eq!(trust.mode, CapabilityMode::ActionRequired);
    assert!(trust.diagnostic.starts_with("ACTION REQUIRED:"));
    assert!(trust.mcp && trust.cli);
    assert!(!trust.session_start);

    let unknown = capabilities(Some("0.148.0"), true, TrustState::Confirmed);
    assert_eq!(unknown.mode, CapabilityMode::McpCliFallback);
    assert!(unknown.mcp && unknown.cli);
    assert!(!unknown.prompt_aware_injection);
}

#[test]
fn codex_output_uses_hook_specific_additional_context_without_control_fields() {
    let output = encode_hook_output(
        CanonicalAgentEventKind::PromptSubmit,
        &ResolvedAgentAction {
            additional_context: Some("read-only pack".to_owned()),
            system_message: None,
        },
    )
    .unwrap();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        output,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": "read-only pack"
        }})
    );
    assert!(output.get("decision").is_none());
    assert!(output.get("continue").is_none());
}

#[test]
fn codex_fail_open_diagnostic_uses_only_system_message() {
    let output = encode_hook_output(
        CanonicalAgentEventKind::PromptSubmit,
        &ResolvedAgentAction {
            additional_context: None,
            system_message: Some("task retrieval temporarily unavailable".to_owned()),
        },
    )
    .unwrap();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        output,
        serde_json::json!({"systemMessage": "task retrieval temporarily unavailable"})
    );
    assert!(output.get("hookSpecificOutput").is_none());
}

#[test]
fn malformed_or_unknown_codex_payload_fails_strictly() {
    let mut payload = fixtures().remove(0);
    payload["hook_event_name"] = Value::String("FutureHook".to_owned());
    assert!(decode_hook_input(&serde_json::to_vec(&payload).unwrap()).is_err());
    payload["hook_event_name"] = Value::String("SessionStart".to_owned());
    payload["cwd"] = Value::Bool(false);
    assert!(decode_hook_input(&serde_json::to_vec(&payload).unwrap()).is_err());
}
