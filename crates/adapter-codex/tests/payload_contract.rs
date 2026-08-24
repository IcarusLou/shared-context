use sctx_adapter_codex::{
    CanonicalAgentEventKind, ResolvedAgentAction, TrustState, capabilities, decode_hook_input,
    encode_hook_output,
};
use sctx_agent_adapter::{
    CapabilityMode, EpisodeFinalizationTrigger, ResolvedActivationDecision,
    SHARED_CONTEXT_ACTIVATION_MARKER, TaskRuntimeOperation, plan_action_for_activation,
};
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
fn verified_and_trusted_codex_prompt_never_repeats_activation_marker() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(1)).unwrap()).unwrap();
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);
    assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
    assert!(capability.prompt_aware_injection);
    let action =
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Direct);
    assert!(action.task_operation.is_none());
    assert!(action.breadcrumb.is_none());
    assert!(action.system_message.is_none());

    let output = encode_hook_output(
        event.kind(),
        &ResolvedAgentAction {
            additional_context: None,
            system_message: action.system_message,
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap(),
        serde_json::json!({})
    );
    assert!(
        !String::from_utf8(output)
            .unwrap()
            .contains(SHARED_CONTEXT_ACTIVATION_MARKER)
    );
}

#[test]
fn codex_precompact_and_turn_stop_request_explicit_checkpoint_without_runtime_claims() {
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);
    for (index, expected_trigger) in [
        (3, EpisodeFinalizationTrigger::PreCompact),
        (4, EpisodeFinalizationTrigger::TurnStop),
    ] {
        let event =
            decode_hook_input(&serde_json::to_vec(&fixtures().remove(index)).unwrap()).unwrap();
        let action =
            plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Direct);
        assert!(matches!(
            action.task_operation,
            Some(TaskRuntimeOperation::FinalizeCheckpointedEpisode { trigger, .. })
                if trigger == expected_trigger
        ));
        assert!(action.system_message.as_deref().is_some_and(|message| {
            message.contains("task_checkpoint")
                && message.contains("Hook summary text is not Claim evidence")
        }));
    }
}

#[test]
fn codex_session_start_encodes_disabled_as_neutral_and_both_enabled_scopes_identically() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(0)).unwrap()).unwrap();
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);

    let disabled =
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Disabled);
    assert!(disabled.task_operation.is_none());
    assert!(disabled.breadcrumb.is_none());
    let disabled_output = encode_hook_output(
        event.kind(),
        &ResolvedAgentAction {
            additional_context: None,
            system_message: disabled.system_message,
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&disabled_output).unwrap(),
        serde_json::json!({})
    );

    let mut outputs = Vec::new();
    for activation in [
        ResolvedActivationDecision::Direct,
        ResolvedActivationDecision::Group,
    ] {
        let action = plan_action_for_activation(&event, &capability, activation);
        assert!(action.task_operation.is_none());
        assert!(action.breadcrumb.is_none());
        outputs.push(
            encode_hook_output(
                event.kind(),
                &ResolvedAgentAction {
                    additional_context: None,
                    system_message: action.system_message,
                },
            )
            .unwrap(),
        );
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        serde_json::from_slice::<Value>(&outputs[0]).unwrap(),
        serde_json::json!({"systemMessage": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
}

#[test]
fn codex_unconfirmed_trust_is_action_required() {
    let trust = capabilities(Some("0.147.0"), true, TrustState::Unconfirmed);
    assert_eq!(trust.mode, CapabilityMode::ActionRequired);
    assert!(trust.diagnostic.starts_with("ACTION REQUIRED:"));
    assert!(trust.mcp && trust.cli);
    assert!(!trust.session_start);
}

#[test]
fn codex_accepts_every_version_at_or_above_the_minimum() {
    for version in ["codex-cli 0.147.0", "codex-cli 0.149.1"] {
        let capability = capabilities(Some(version), true, TrustState::Confirmed);
        assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
        assert!(capability.prompt_aware_injection);
        assert_eq!(capability.verified_version_requirement, ">=0.147.0");
    }

    let below_minimum = capabilities(Some("0.146.99"), true, TrustState::Confirmed);
    assert_eq!(below_minimum.mode, CapabilityMode::McpCliFallback);
    assert!(below_minimum.mcp && below_minimum.cli);
    assert!(!below_minimum.prompt_aware_injection);
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
