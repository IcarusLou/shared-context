use sctx_adapter_cursor::{
    CanonicalAgentEventKind, ResolvedAgentAction, capabilities, decode_hook_input,
    encode_hook_output,
};
use sctx_agent_adapter::{
    CapabilityMode, EpisodeFinalizationTrigger, ResolvedActivationDecision,
    SHARED_CONTEXT_ACTIVATION_MARKER, TaskRuntimeOperation, plan_action_for_activation,
};
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
fn cursor_prompt_hook_never_repeats_activation_marker() {
    let payload = fixtures().remove(1);
    let (event, _) = decode_hook_input(&serde_json::to_vec(&payload).unwrap()).unwrap();
    let capability = capabilities(Some("3.13.10"), true);
    assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
    assert!(capability.prompt_submit);
    assert!(!capability.prompt_aware_injection);
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
fn cursor_precompact_and_turn_stop_request_explicit_checkpoint_without_runtime_claims() {
    let capability = capabilities(Some("3.13.10"), true);
    for (index, expected_trigger) in [
        (3, EpisodeFinalizationTrigger::PreCompact),
        (4, EpisodeFinalizationTrigger::TurnStop),
    ] {
        let (event, _) =
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
fn cursor_session_start_encodes_disabled_as_neutral_and_both_enabled_scopes_identically() {
    let (event, _) =
        decode_hook_input(&serde_json::to_vec(&fixtures().remove(0)).unwrap()).unwrap();
    let capability = capabilities(Some("3.13.10"), true);

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
        serde_json::json!({"additional_context": SHARED_CONTEXT_ACTIVATION_MARKER})
    );
}

#[test]
fn cursor_accepts_every_version_at_or_above_the_minimum() {
    for version in ["3.13.0", "4.0.0"] {
        let capability = capabilities(Some(version), true);
        assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
        assert!(capability.session_start);
        assert_eq!(capability.verified_version_requirement, ">=3.13.0");
    }
}

#[test]
fn cursor_below_minimum_missing_version_or_hooks_keep_only_mcp_cli() {
    for capability in [
        capabilities(Some("3.12.99"), true),
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
fn cursor_post_tool_fail_open_diagnostic_uses_documented_context_field() {
    let output = encode_hook_output(
        CanonicalAgentEventKind::PostToolUse,
        &ResolvedAgentAction {
            additional_context: None,
            system_message: Some("task retrieval temporarily unavailable".to_owned()),
        },
    )
    .unwrap();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        output,
        serde_json::json!({"additional_context":"task retrieval temporarily unavailable"})
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
