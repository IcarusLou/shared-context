use sctx_adapter_cursor::{
    CanonicalAgentEventKind, ResolvedAgentAction, capabilities, decode_hook_input,
    encode_hook_output,
};
use sctx_agent_adapter::{
    ARTIFACT_FOCUS_REMINDER_MAX_BYTES, AgentKind, ArtifactFocusReminderContext, CapabilityMode,
    EpisodeFinalizationTrigger, PathHint, ResolvedActivationDecision, TaskRuntimeOperation,
    ToolCategory, artifact_focus_reminder_file, plan_action_for_activation,
    render_artifact_focus_reminder, shared_context_activation_marker,
};
use serde_json::Value;
use std::path::PathBuf;

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
fn cursor_shell_fixture_emits_a_strict_test_runner_with_only_its_working_directory() {
    let (event, _) =
        decode_hook_input(&serde_json::to_vec(&fixtures().remove(2)).unwrap()).unwrap();
    let sctx_adapter_cursor::CanonicalAgentEvent::PostToolUse {
        tool_category,
        path_hints,
        ..
    } = event
    else {
        panic!("fixture must decode as PostToolUse");
    };
    assert_eq!(tool_category, ToolCategory::TestRunner);
    assert_eq!(
        path_hints,
        vec![PathHint::WorkingDirectory(PathBuf::from(
            "/workspace/shared context"
        ))]
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
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Enabled);
    assert!(action.task_operation.is_none());
    assert!(action.system_message.is_none());

    let output = encode_hook_output(
        event.kind(),
        &ResolvedAgentAction {
            additional_context: action.additional_context,
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
            .contains("<shared-context-active")
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
            plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Enabled);
        assert!(matches!(
            action.task_operation,
            Some(TaskRuntimeOperation::FinalizeCheckpointedEpisode { trigger, .. })
                if trigger == expected_trigger
        ));
        let message = action.system_message.as_deref().unwrap();
        assert!(message.contains("task_checkpoint"));
        assert!(message.contains("complete direct Claims/Unknowns"));
        assert!(message.contains("server resolves the current Task, Intent, and lifecycle"));
        assert!(message.contains("Hook lifecycle data is not Claim evidence"));
        for forbidden in [
            "expected_task_id",
            "expected_intent_revision_id",
            "expected_episode_version",
            "boundary",
            "inline_validation",
            "capture",
        ] {
            assert!(
                !message.contains(forbidden),
                "stale Hook guidance: {message}"
            );
        }
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
    let disabled_output = encode_hook_output(
        event.kind(),
        &ResolvedAgentAction {
            additional_context: disabled.additional_context,
            system_message: disabled.system_message,
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&disabled_output).unwrap(),
        serde_json::json!({})
    );

    let action =
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Enabled);
    assert!(action.task_operation.is_none());
    let enabled_output = encode_hook_output(
        event.kind(),
        &ResolvedAgentAction {
            additional_context: action.additional_context,
            system_message: action.system_message,
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&enabled_output).unwrap(),
        serde_json::json!({
            "additional_context":
                shared_context_activation_marker(AgentKind::Cursor, "conv-real-shape-01")
        })
    );
}

#[test]
fn cursor_accepts_every_host_version_string_when_a_hook_is_available() {
    for version in [
        Some("3.13.0"),
        Some("4.0.0"),
        Some("3.12.99"),
        // `cursor-agent --version` reports a date-like build id that is not semver.
        Some("2026.08.25-3e8eec8"),
        Some(""),
        None,
    ] {
        let capability = capabilities(version, true);
        assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
        assert!(capability.session_start);
        assert_eq!(capability.detected_version.as_deref(), version);
        assert_eq!(capability.fixture_profile_version, "3.13.0");
    }
}

#[test]
fn cursor_keeps_only_mcp_cli_when_no_hook_is_available() {
    for capability in [
        capabilities(Some("3.13.10"), false),
        capabilities(Some("2026.08.25-3e8eec8"), false),
        capabilities(None, false),
    ] {
        assert_eq!(capability.mode, CapabilityMode::McpCliFallback);
        assert!(capability.mcp && capability.cli);
        assert!(!capability.session_start);
        assert_eq!(
            capability.diagnostic,
            "Agent hooks are unavailable; using MCP + CLI fallback."
        );
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

/// P4.1 experiment. Cursor's documented `PostToolUse` fixture is a shell test
/// runner, so the located file case uses the same envelope with a documented
/// file-operation tool. The switch is off by default and the disabled bytes stay
/// the contract.
fn cursor_file_operation_payload() -> Value {
    let mut payload = fixtures().remove(2);
    payload["tool_name"] = serde_json::json!("Read");
    payload["tool_input"] =
        serde_json::json!({"file_path": "/workspace/shared context/src/lib.rs"});
    payload
}

fn cursor_reminder_contexts() -> Vec<ArtifactFocusReminderContext> {
    vec![ArtifactFocusReminderContext {
        context_id: "ctx_2f0d2a2f4c7a4f0f8f8f0a1b2c3d4e5f".to_owned(),
        title:
            "The default comment bottom bar must survive an absent vertical-domain service module"
                .to_owned(),
    }]
}

#[test]
fn cursor_post_tool_reminder_is_off_by_default_and_keeps_the_current_bytes() {
    let (event, _) =
        decode_hook_input(&serde_json::to_vec(&cursor_file_operation_payload()).unwrap()).unwrap();
    let capabilities = capabilities(Some("3.13.10"), true);
    let action =
        plan_action_for_activation(&event, &capabilities, ResolvedActivationDecision::Enabled);
    assert!(action.additional_context.is_none());
    assert!(
        artifact_focus_reminder_file(
            &event,
            ResolvedActivationDecision::Enabled,
            &capabilities,
            false,
        )
        .is_none()
    );
    let resolved = ResolvedAgentAction {
        additional_context: action.additional_context,
        system_message: action.system_message,
    };
    assert_eq!(
        encode_hook_output(CanonicalAgentEventKind::PostToolUse, &resolved).unwrap(),
        b"{}".to_vec()
    );
}

#[test]
fn cursor_enabled_reminder_is_bounded_and_encoded_as_read_only_additional_context() {
    let (event, _) =
        decode_hook_input(&serde_json::to_vec(&cursor_file_operation_payload()).unwrap()).unwrap();
    let capabilities = capabilities(Some("3.13.10"), true);
    let file = artifact_focus_reminder_file(
        &event,
        ResolvedActivationDecision::Enabled,
        &capabilities,
        true,
    )
    .expect("a located Cursor file operation is eligible");
    assert_eq!(
        file,
        std::path::Path::new("/workspace/shared context/src/lib.rs")
    );

    let contexts = cursor_reminder_contexts();
    let reminder = render_artifact_focus_reminder("src/lib.rs", &contexts).unwrap();
    assert!(reminder.len() <= ARTIFACT_FOCUS_REMINDER_MAX_BYTES);
    assert!(reminder.contains(&contexts[0].context_id));
    assert!(reminder.contains("call task_artifact_focus for src/lib.rs to load them"));
    assert!(!reminder.contains("/workspace/shared context"));

    let resolved = ResolvedAgentAction {
        additional_context: Some(reminder.clone()),
        system_message: None,
    };
    let encoded = encode_hook_output(CanonicalAgentEventKind::PostToolUse, &resolved).unwrap();
    let decoded: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded["additional_context"], Value::String(reminder));
}

#[test]
fn cursor_shell_and_group_activation_never_reach_the_reminder_lookup() {
    let (shell, _) =
        decode_hook_input(&serde_json::to_vec(&fixtures().remove(2)).unwrap()).unwrap();
    let capabilities = capabilities(Some("3.13.10"), true);
    assert!(
        artifact_focus_reminder_file(
            &shell,
            ResolvedActivationDecision::Enabled,
            &capabilities,
            true,
        )
        .is_none()
    );

    let (file_event, _) =
        decode_hook_input(&serde_json::to_vec(&cursor_file_operation_payload()).unwrap()).unwrap();
    assert!(
        artifact_focus_reminder_file(
            &file_event,
            ResolvedActivationDecision::Disabled,
            &capabilities,
            true,
        )
        .is_none()
    );
}
