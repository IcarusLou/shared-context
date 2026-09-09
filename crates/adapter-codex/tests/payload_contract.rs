use sctx_adapter_codex::{
    CanonicalAgentEventKind, HookDecodeErrorClass, HookDecodeField, HookHostSchema,
    ResolvedAgentAction, TrustState, capabilities, decode_hook_input,
    decode_hook_input_with_diagnostic, encode_hook_output,
};
use sctx_agent_adapter::{
    AgentKind, CapabilityMode, EpisodeFinalizationTrigger, PathHint, ResolvedActivationDecision,
    TaskRuntimeOperation, ToolCategory, plan_action_for_activation,
    shared_context_activation_marker,
};
use serde_json::Value;
use std::path::PathBuf;

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("../../../fixtures/agents/codex-0.147.json")).unwrap()
}

#[test]
fn codex_fixture_and_live_session_end_shapes_map_to_all_canonical_events() {
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
            CanonicalAgentEventKind::SessionEnd,
        ]
    );
}

#[test]
fn codex_model_less_session_end_matches_the_live_five_key_fingerprint() {
    let payload = fixtures().remove(6);
    let keys = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "cwd",
            "hook_event_name",
            "reason",
            "session_id",
            "transcript_path"
        ]
    );
    let event = decode_hook_input(&serde_json::to_vec(&payload).unwrap()).unwrap();
    let sctx_adapter_codex::CanonicalAgentEvent::SessionEnd { context, reason } = event else {
        panic!("live fingerprint must decode as SessionEnd");
    };
    assert_eq!(context.session_id, "thr_live_session_end_shape_01");
    assert_eq!(reason, "completed");
}

#[test]
fn codex_five_model_bearing_events_keep_strict_model_validation() {
    for fixture in fixtures().into_iter().take(5) {
        for (model, class) in [
            (None, HookDecodeErrorClass::MissingField),
            (Some(Value::Null), HookDecodeErrorClass::Type),
            (Some(serde_json::json!(42)), HookDecodeErrorClass::Type),
            (
                Some(serde_json::json!("")),
                HookDecodeErrorClass::MissingField,
            ),
            (
                Some(serde_json::json!("  ")),
                HookDecodeErrorClass::MissingField,
            ),
        ] {
            let mut payload = fixture.clone();
            payload.as_object_mut().unwrap().remove("model");
            if let Some(model) = model {
                payload["model"] = model;
            }
            let failure = decode_hook_input_with_diagnostic(&serde_json::to_vec(&payload).unwrap())
                .unwrap_err();
            assert_eq!(failure.diagnostic().error_class, class, "{payload}");
            assert_eq!(
                failure.diagnostic().field,
                Some(HookDecodeField::Model),
                "{payload}"
            );
        }
    }
}

#[test]
fn codex_session_end_optional_model_retains_supplied_value_boundaries() {
    for model in [Value::Null, serde_json::json!("gpt-5.6-sol")] {
        let mut payload = fixtures().remove(6);
        payload["model"] = model;
        assert_eq!(
            decode_hook_input(&serde_json::to_vec(&payload).unwrap())
                .unwrap()
                .kind(),
            CanonicalAgentEventKind::SessionEnd
        );
    }
    for (model, class) in [
        (serde_json::json!(42), HookDecodeErrorClass::Type),
        (serde_json::json!("  "), HookDecodeErrorClass::MissingField),
    ] {
        let mut payload = fixtures().remove(6);
        payload["model"] = model;
        let failure =
            decode_hook_input_with_diagnostic(&serde_json::to_vec(&payload).unwrap()).unwrap_err();
        assert_eq!(failure.diagnostic().error_class, class);
        assert_eq!(failure.diagnostic().field, Some(HookDecodeField::Model));
    }
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
fn codex_boundaries_plan_runtime_finalization_and_encode_its_resolved_notice() {
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
        assert!(
            action.system_message.is_none(),
            "the planner must not invent runtime guidance"
        );
        let message = "Runtime-resolved checkpoint guidance";

        // Neither event's Codex output object has a `hookSpecificOutput` variant, so the
        // checkpoint request travels as the one field both of them accept. A `PreCompact`
        // plan still renders the activation marker, and it is dropped here rather than
        // taking the whole output down with it; the marker reaches a compacted session
        // through the `SessionStart` Codex re-sends with `source: "compact"`.
        let output = encode_hook_output(
            event.kind(),
            &ResolvedAgentAction {
                additional_context: action.additional_context.clone(),
                system_message: Some(message.to_owned()),
            },
        )
        .unwrap();
        if expected_trigger == EpisodeFinalizationTrigger::PreCompact {
            assert_eq!(
                action.additional_context.as_deref(),
                Some(
                    shared_context_activation_marker(AgentKind::Codex, "thr_real_shape_01")
                        .as_str()
                )
            );
        }
        let output: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            output,
            serde_json::json!({"systemMessage": message}),
            "{expected_trigger:?} must encode as systemMessage alone"
        );
        assert!(output.get("hookSpecificOutput").is_none());
    }
}

/// `SessionEnd` is the third event Codex rejects a `hookSpecificOutput` on, and the one whose
/// plan carries nothing to say in the first place: the neutral object is the whole output.
#[test]
fn codex_session_end_encodes_a_neutral_object_and_never_model_context() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(5)).unwrap()).unwrap();
    let capability = capabilities(Some("codex-cli 0.153.4"), true, TrustState::Confirmed);
    let action =
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Enabled);
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

    // And a `SessionEnd` that did have something to say keeps it in `systemMessage` alone.
    let output = encode_hook_output(
        CanonicalAgentEventKind::SessionEnd,
        &ResolvedAgentAction {
            additional_context: Some("model context that cannot be delivered".to_owned()),
            system_message: Some("a closing line".to_owned()),
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap(),
        serde_json::json!({"systemMessage": "a closing line"})
    );
}

/// The wire fact behind the whitelist, stated as a test: Codex 0.153.4 deserializes
/// `hookSpecificOutput` as an internally tagged enum with six variants, and `Stop`,
/// `PreCompact`, and `SessionEnd` are not among them. Emitting the block on one of those
/// takes the entire output down — `hook returned invalid stop hook JSON output` — so the
/// `systemMessage` a user was meant to read is lost with it.
#[test]
fn codex_events_without_a_hook_specific_output_variant_encode_system_message_alone() {
    for event in [
        CanonicalAgentEventKind::TurnStop,
        CanonicalAgentEventKind::PreCompact,
        CanonicalAgentEventKind::SessionEnd,
    ] {
        let output = encode_hook_output(
            event,
            &ResolvedAgentAction {
                additional_context: Some("marker the model will never see here".to_owned()),
                system_message: Some("call task_checkpoint".to_owned()),
            },
        )
        .unwrap();
        let output: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            output,
            serde_json::json!({"systemMessage": "call task_checkpoint"}),
            "{event:?}"
        );

        // Nothing to say at all stays the neutral object, not an empty block.
        let neutral = encode_hook_output(
            event,
            &ResolvedAgentAction {
                additional_context: Some("marker the model will never see here".to_owned()),
                system_message: None,
            },
        )
        .unwrap();
        assert_eq!(neutral, b"{}".to_vec(), "{event:?}");
    }
}

/// The three events that do have a variant keep mirroring a lone `systemMessage` into model
/// context, which is how the Intent bootstrap reaches the Agent it addresses.
#[test]
fn codex_events_with_a_hook_specific_output_variant_still_mirror_a_lone_system_message() {
    for (event, hook_event_name) in [
        (CanonicalAgentEventKind::SessionStart, "SessionStart"),
        (CanonicalAgentEventKind::PromptSubmit, "UserPromptSubmit"),
        (CanonicalAgentEventKind::PostToolUse, "PostToolUse"),
    ] {
        let output = encode_hook_output(
            event,
            &ResolvedAgentAction {
                additional_context: None,
                system_message: Some("Shared Context: no ActiveTask exists.".to_owned()),
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap(),
            serde_json::json!({
                "systemMessage": "Shared Context: no ActiveTask exists.",
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": "Shared Context: no ActiveTask exists."
                }
            })
        );
    }
}

/// Codex re-sends `SessionStart` after it compacts, and that payload is the only path the
/// activation marker has back into a compacted session's context.
#[test]
fn codex_session_start_accepts_the_post_compaction_source() {
    let mut payload = fixtures().remove(0);
    payload["source"] = Value::String("compact".to_owned());
    let event = decode_hook_input(&serde_json::to_vec(&payload).unwrap()).unwrap();
    assert_eq!(event.kind(), CanonicalAgentEventKind::SessionStart);
    let capability = capabilities(Some("codex-cli 0.153.4"), true, TrustState::Confirmed);
    let action =
        plan_action_for_activation(&event, &capability, ResolvedActivationDecision::Enabled);
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
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker(
                AgentKind::Codex,
                "thr_real_shape_01"
            )
        }})
    );

    payload["source"] = Value::String("teleport".to_owned());
    assert!(decode_hook_input(&serde_json::to_vec(&payload).unwrap()).is_err());
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

#[test]
fn decode_failures_expose_only_closed_shape_metadata_and_a_bounded_session_id() {
    let secret = "ghp_adapter_diagnostic_must_never_retain_this_value";
    let unknown = serde_json::json!({
        "hook_event_name": "SecretFutureHook",
        "session_id": "known-session",
        "prompt": secret,
        "secret key!": secret
    });
    let failure = decode_hook_input_with_diagnostic(&serde_json::to_vec(&unknown).unwrap())
        .expect_err("unknown event");
    assert_eq!(
        failure.diagnostic().error_class,
        HookDecodeErrorClass::UnknownEvent
    );
    assert_eq!(failure.diagnostic().event_kind, None);
    assert_eq!(failure.diagnostic().field, None);
    assert_eq!(
        failure.diagnostic().host_schema,
        HookHostSchema::UnknownEvent
    );
    assert_eq!(
        failure.diagnostic().session_id.as_deref(),
        Some("known-session")
    );
    let diagnostic = format!("{:?}", failure.diagnostic());
    assert!(!diagnostic.contains(secret));
    assert!(!diagnostic.contains("SecretFutureHook"));
    assert!(!diagnostic.contains("secret key"));

    let mut supported = fixtures().remove(1);
    supported.as_object_mut().unwrap().remove("turn_id");
    supported["prompt"] = Value::String(secret.to_owned());
    let failure =
        decode_hook_input_with_diagnostic(&serde_json::to_vec(&supported).unwrap()).unwrap_err();
    assert_eq!(
        failure.diagnostic().error_class,
        HookDecodeErrorClass::MissingField
    );
    assert_eq!(
        failure.diagnostic().event_kind,
        Some(CanonicalAgentEventKind::PromptSubmit)
    );
    assert_eq!(
        failure.diagnostic().host_schema,
        HookHostSchema::SupportedEvent
    );
    assert_eq!(failure.diagnostic().field, Some(HookDecodeField::TurnId));

    let failure = decode_hook_input_with_diagnostic(b"not json").unwrap_err();
    assert_eq!(
        failure.diagnostic().error_class,
        HookDecodeErrorClass::InvalidJson
    );
    assert_eq!(
        failure.diagnostic().host_schema,
        HookHostSchema::InvalidJson
    );
    assert_eq!(failure.diagnostic().field, None);
    assert_eq!(failure.diagnostic().session_id, None);
}

#[test]
fn codex_session_end_accepts_nonempty_live_host_reasons() {
    for reason in ["other", "completed", "window_close"] {
        let mut payload = fixtures().remove(5);
        payload["reason"] = Value::String(reason.to_owned());
        let event = decode_hook_input(&serde_json::to_vec(&payload).unwrap()).unwrap();
        let sctx_adapter_codex::CanonicalAgentEvent::SessionEnd {
            reason: decoded, ..
        } = event
        else {
            panic!("SessionEnd payload must remain a SessionEnd");
        };
        assert_eq!(decoded, reason);
    }

    let mut payload = fixtures().remove(5);
    payload["reason"] = Value::String("  ".to_owned());
    let failure =
        decode_hook_input_with_diagnostic(&serde_json::to_vec(&payload).unwrap()).unwrap_err();
    assert_eq!(
        failure.diagnostic().error_class,
        HookDecodeErrorClass::MissingField
    );
}

#[test]
fn codex_post_tool_policy_keeps_the_current_neutral_bytes() {
    let event = decode_hook_input(&serde_json::to_vec(&fixtures().remove(2)).unwrap()).unwrap();
    let capabilities = capabilities(Some("0.147.0"), true, TrustState::Confirmed);
    let action =
        plan_action_for_activation(&event, &capabilities, ResolvedActivationDecision::Enabled);
    assert!(action.additional_context.is_none());
    let resolved = ResolvedAgentAction {
        additional_context: action.additional_context,
        system_message: action.system_message,
    };
    assert_eq!(
        encode_hook_output(CanonicalAgentEventKind::PostToolUse, &resolved).unwrap(),
        b"{}".to_vec()
    );
}

/// Trusted system instructions stay separate from model-visible reference data.
#[test]
fn codex_post_tool_keeps_model_context_separate_from_its_system_message() {
    const INTENT_BOOTSTRAP: &str = "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.";
    const MODEL_CONTEXT: &str = "Reference data only: a stored Context title.";
    let output = encode_hook_output(
        CanonicalAgentEventKind::PostToolUse,
        &ResolvedAgentAction {
            additional_context: Some(MODEL_CONTEXT.to_owned()),
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
                "additionalContext": MODEL_CONTEXT
            }
        })
    );
    assert!(
        !output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(INTENT_BOOTSTRAP)
    );
}

/// The visibility predicate and the encoder must agree on every event, because a caller deciding
/// whether to spend a one-shot delivery reads the predicate and the host reads the encoder.
#[test]
fn model_visibility_predicate_agrees_with_the_encoder_on_every_event() {
    for kind in [
        CanonicalAgentEventKind::SessionStart,
        CanonicalAgentEventKind::PromptSubmit,
        CanonicalAgentEventKind::PostToolUse,
        CanonicalAgentEventKind::PreCompact,
        CanonicalAgentEventKind::TurnStop,
        CanonicalAgentEventKind::SessionEnd,
    ] {
        const CONTEXT: &str = "one line of model context";
        let encoded = encode_hook_output(
            kind,
            &ResolvedAgentAction {
                additional_context: Some(CONTEXT.to_owned()),
                system_message: None,
            },
        )
        .unwrap();
        let delivered = String::from_utf8(encoded).unwrap().contains(CONTEXT);
        assert_eq!(
            delivered,
            sctx_adapter_codex::delivers_model_visible_context(kind),
            "{kind:?}"
        );
    }
}
