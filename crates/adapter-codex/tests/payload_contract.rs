use sctx_adapter_codex::{
    CanonicalAgentEventKind, ResolvedAgentAction, TrustState, capabilities, decode_hook_input,
    encode_hook_output,
};
use sctx_agent_adapter::{
    ARTIFACT_FOCUS_REMINDER_MAX_BYTES, ARTIFACT_FOCUS_REMINDER_MAX_CONTEXTS, AgentKind,
    ArtifactFocusReminderContext, CapabilityMode, EpisodeFinalizationTrigger, PathHint,
    ResolvedActivationDecision, TaskRuntimeOperation, ToolCategory, artifact_focus_reminder_file,
    plan_action_for_activation, render_artifact_focus_reminder, shared_context_activation_marker,
};
use serde_json::Value;
use std::path::PathBuf;

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
fn codex_structured_tool_payload_emits_a_typed_file_operation() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(2)).unwrap()).unwrap();
    let sctx_adapter_codex::CanonicalAgentEvent::PostToolUse {
        tool_category,
        path_hints,
        ..
    } = event
    else {
        panic!("fixture must decode as PostToolUse");
    };
    assert_eq!(tool_category, ToolCategory::FileOperation);
    assert_eq!(
        path_hints,
        vec![PathHint::File(PathBuf::from(
            "/workspace/shared context/src/lib.rs"
        ))]
    );
}

#[test]
fn verified_and_trusted_codex_prompt_never_repeats_activation_marker() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(1)).unwrap()).unwrap();
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);
    assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
    assert!(capability.prompt_aware_injection);
    let action =
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Enabled);
    // A Prompt plans one purely local Signal and nothing else: no marker, no reminder, and
    // nothing the model or the user ever sees for this event.
    assert!(matches!(
        action.task_operation,
        Some(TaskRuntimeOperation::RecordPromptSignal { .. })
    ));
    assert!(action.additional_context.is_none());
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
fn codex_precompact_and_turn_stop_request_explicit_checkpoint_without_runtime_claims() {
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);
    for (index, expected_trigger) in [
        (3, EpisodeFinalizationTrigger::PreCompact),
        (4, EpisodeFinalizationTrigger::TurnStop),
    ] {
        let event =
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

        // Compaction re-states the activation marker as model-visible context, because
        // compaction is what drops the SessionStart marker — joined to the boundary line
        // so the checkpoint request the user sees is not the half the model loses. A Stop
        // has no marker of its own, so its user-visible line is mirrored on its own.
        let output = encode_hook_output(
            event.kind(),
            &ResolvedAgentAction {
                additional_context: action.additional_context.clone(),
                system_message: action.system_message.clone(),
            },
        )
        .unwrap();
        let (hook_event_name, expected_context) = match expected_trigger {
            EpisodeFinalizationTrigger::PreCompact => (
                "PreCompact",
                format!(
                    "{message}\n{}",
                    shared_context_activation_marker(AgentKind::Codex, "thr_real_shape_01")
                ),
            ),
            EpisodeFinalizationTrigger::TurnStop => ("Stop", message.to_owned()),
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap(),
            serde_json::json!({
                "systemMessage": message,
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": expected_context
                }
            })
        );
    }
}

#[test]
fn codex_session_start_encodes_enabled_marker_as_model_context_and_disabled_as_neutral() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(0)).unwrap()).unwrap();
    let capability = capabilities(Some("codex-cli 0.147.0"), true, TrustState::Confirmed);

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
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(
                AgentKind::Codex,
                "thr_real_shape_01"
            )
        }})
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
fn codex_accepts_every_host_version_string_when_a_hook_is_available() {
    for version in [
        Some("codex-cli 0.147.0"),
        Some("codex-cli 0.149.1"),
        Some("0.146.99"),
        Some("2026.08.25-3e8eec8"),
        Some(""),
        None,
    ] {
        let capability = capabilities(version, true, TrustState::Confirmed);
        assert_eq!(capability.mode, CapabilityMode::VerifiedHooks);
        assert!(capability.prompt_aware_injection);
        assert_eq!(capability.detected_version.as_deref(), version);
        assert_eq!(capability.fixture_profile_version, "0.147.0");
    }

    let without_hooks = capabilities(Some("0.149.1"), false, TrustState::Confirmed);
    assert_eq!(without_hooks.mode, CapabilityMode::McpCliFallback);
    assert!(without_hooks.mcp && without_hooks.cli);
    assert!(!without_hooks.prompt_aware_injection);
    assert_eq!(
        without_hooks.diagnostic,
        "Agent hooks are unavailable; using MCP + CLI fallback."
    );
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

/// A message with no additional context of its own reaches both the user-visible
/// `systemMessage` line and the model-visible `additionalContext`, because a reminder
/// the model cannot read is not a reminder.
#[test]
fn codex_fail_open_diagnostic_is_mirrored_into_model_context() {
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
        serde_json::json!({
            "systemMessage": "task retrieval temporarily unavailable",
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": "task retrieval temporarily unavailable"
            }
        })
    );
    assert!(output.get("decision").is_none());
    assert!(output.get("continue").is_none());
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

/// P4.1 experiment. The switch is off by default: the disabled Codex `PostToolUse`
/// bytes are the contract, and the enabled path may only add one bounded
/// reminder that names Context identities and no Context body.
fn codex_post_tool_event() -> sctx_adapter_codex::CanonicalAgentEvent {
    decode_hook_input(&serde_json::to_vec(&fixtures().remove(2)).unwrap()).unwrap()
}

fn codex_capabilities() -> sctx_adapter_codex::AgentCapabilities {
    capabilities(Some("0.147.0"), true, TrustState::Confirmed)
}

fn reminder_contexts() -> Vec<ArtifactFocusReminderContext> {
    vec![
        ArtifactFocusReminderContext {
            context_id: "ctx_2f0d2a2f4c7a4f0f8f8f0a1b2c3d4e5f".to_owned(),
            title: "The default comment bottom bar must survive an absent vertical-domain service module".to_owned(),
        },
        ArtifactFocusReminderContext {
            context_id: "ctx_9a1b2c3d4e5f60718293a4b5c6d7e8f9".to_owned(),
            title: "Product anchor navigation returns early without a live entry implementation".to_owned(),
        },
    ]
}

#[test]
fn codex_post_tool_reminder_is_off_by_default_and_keeps_the_current_bytes() {
    let event = codex_post_tool_event();
    let capabilities = codex_capabilities();
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

/// The `PreCompact` join is scoped, and this is the event that proves it must be.
///
/// A `PostToolUse` under the enabled Artifact focus experiment is the one other event that
/// carries both fields: the Intent bootstrap line and one bounded, untrusted-data-fenced
/// reminder. The reminder reaches model context as exactly the block it was rendered and
/// budgeted as — joining the message into it would both break its byte budget and put a
/// trusted instruction inside a block fenced as untrusted data.
#[test]
fn codex_post_tool_keeps_a_bounded_reminder_separate_from_its_system_message() {
    const INTENT_BOOTSTRAP: &str = "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.";

    let reminder = render_artifact_focus_reminder("src/lib.rs", &reminder_contexts()).unwrap();
    let output = encode_hook_output(
        CanonicalAgentEventKind::PostToolUse,
        &ResolvedAgentAction {
            additional_context: Some(reminder.clone()),
            system_message: Some(INTENT_BOOTSTRAP.to_owned()),
        },
    )
    .unwrap();
    let output: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        output,
        serde_json::json!({
            "systemMessage": INTENT_BOOTSTRAP,
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": reminder
            }
        })
    );
    let model_context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!model_context.contains(INTENT_BOOTSTRAP));
    assert!(model_context.len() <= ARTIFACT_FOCUS_REMINDER_MAX_BYTES);
}

#[test]
fn codex_enabled_reminder_is_bounded_names_context_ids_and_carries_no_context_body() {
    let event = codex_post_tool_event();
    let capabilities = codex_capabilities();
    let file = artifact_focus_reminder_file(
        &event,
        ResolvedActivationDecision::Enabled,
        &capabilities,
        true,
    )
    .expect("a located Codex file operation is eligible");
    assert_eq!(
        file,
        std::path::Path::new("/workspace/shared context/src/lib.rs")
    );

    let contexts = reminder_contexts();
    let reminder = render_artifact_focus_reminder("src/lib.rs", &contexts).unwrap();
    assert!(reminder.len() <= ARTIFACT_FOCUS_REMINDER_MAX_BYTES);
    for context in &contexts {
        assert!(reminder.contains(&context.context_id));
    }
    assert!(reminder.contains("call task_artifact_focus for src/lib.rs to load them"));
    for forbidden in [
        "rationale",
        "evidence",
        "statement",
        "/workspace/shared context",
    ] {
        assert!(!reminder.contains(forbidden), "reminder leaked {forbidden}");
    }

    let resolved = ResolvedAgentAction {
        additional_context: Some(reminder.clone()),
        system_message: None,
    };
    let encoded = encode_hook_output(CanonicalAgentEventKind::PostToolUse, &resolved).unwrap();
    let decoded: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        decoded["hookSpecificOutput"]["additionalContext"],
        Value::String(reminder)
    );
}

#[test]
fn codex_reminder_target_requires_enabled_activation_a_file_tool_and_one_path() {
    let event = codex_post_tool_event();
    let capabilities = codex_capabilities();
    assert!(
        artifact_focus_reminder_file(
            &event,
            ResolvedActivationDecision::Disabled,
            &capabilities,
            true,
        )
        .is_none()
    );

    let unverified =
        sctx_adapter_codex::capabilities(Some("0.146.0"), false, TrustState::Confirmed);
    assert!(
        artifact_focus_reminder_file(
            &event,
            ResolvedActivationDecision::Enabled,
            &unverified,
            true,
        )
        .is_none()
    );

    let mut ambiguous = fixtures().remove(2);
    ambiguous["tool_input"] = serde_json::json!({"file_path": ["/a.rs", "/b.rs"]});
    let ambiguous = decode_hook_input(&serde_json::to_vec(&ambiguous).unwrap()).unwrap();
    assert!(
        artifact_focus_reminder_file(
            &ambiguous,
            ResolvedActivationDecision::Enabled,
            &capabilities,
            true,
        )
        .is_none()
    );

    let shell = decode_hook_input(
        &serde_json::to_vec(&{
            let mut payload = fixtures().remove(2);
            payload["tool_name"] = serde_json::json!("Shell");
            payload["tool_input"] =
                serde_json::json!({"command": "cargo test", "working_directory": "/workspace"});
            payload
        })
        .unwrap(),
    )
    .unwrap();
    assert!(
        artifact_focus_reminder_file(
            &shell,
            ResolvedActivationDecision::Enabled,
            &capabilities,
            true,
        )
        .is_none()
    );
}

#[test]
fn codex_reminder_drops_extra_contexts_and_truncates_long_titles() {
    let long = "\u{4e00}".repeat(400);
    let contexts = (0..6)
        .map(|index| ArtifactFocusReminderContext {
            context_id: format!("ctx_{index:032x}"),
            title: long.clone(),
        })
        .collect::<Vec<_>>();
    let reminder = render_artifact_focus_reminder("src/lib.rs", &contexts).unwrap();
    assert!(reminder.len() <= ARTIFACT_FOCUS_REMINDER_MAX_BYTES);
    assert!(
        reminder
            .lines()
            .filter(|line| line.starts_with("ctx_"))
            .count()
            <= ARTIFACT_FOCUS_REMINDER_MAX_CONTEXTS
    );
    assert!(reminder.contains('\u{2026}'));
    assert!(render_artifact_focus_reminder("src/lib.rs", &[]).is_none());
}
