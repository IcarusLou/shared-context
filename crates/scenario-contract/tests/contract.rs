use sctx_scenario_contract::{
    AgentFraming, AgentVendor, ContractErrorKind, MAX_ACTIONS, MAX_DOCUMENT_BYTES, WireName,
    parse_scenario, to_canonical_json,
};
use serde_json::{Value, json};

const CODEX: &[u8] = include_bytes!("fixtures/valid/codex-v1.json");
const CURSOR: &[u8] = include_bytes!("fixtures/valid/cursor-v1.json");

fn value(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("hand-written fixture must be JSON")
}

fn encoded(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("test mutation must remain serializable")
}

fn expect_error(value: &Value, expected: ContractErrorKind) {
    let error = parse_scenario(&encoded(value)).expect_err("invalid scenario must be rejected");
    assert_eq!(error.kind(), expected, "{}", error.message());
}

#[test]
fn handwritten_codex_and_cursor_contracts_roundtrip_stably() {
    for (fixture, vendor, framing) in [
        (CODEX, AgentVendor::Codex, AgentFraming::ContentLength),
        (
            CURSOR,
            AgentVendor::Cursor,
            AgentFraming::NewlineDelimitedJson,
        ),
    ] {
        let scenario = parse_scenario(fixture).expect("valid scenario should parse");
        assert_eq!(scenario.agent.vendor, vendor);
        assert_eq!(scenario.agent.framing, framing);

        let first = to_canonical_json(&scenario).expect("valid scenario should serialize");
        let reparsed = parse_scenario(&first).expect("canonical scenario should parse");
        let second = to_canonical_json(&reparsed).expect("roundtrip should serialize");

        assert_eq!(scenario, reparsed);
        assert_eq!(first, second, "canonical serialization must be stable");
    }
}

#[test]
fn fixtures_are_synthetic_and_contain_no_local_session_material() {
    for fixture in [CODEX, CURSOR] {
        let text = std::str::from_utf8(fixture).expect("fixture is UTF-8");
        for private_marker in [
            "/Users/",
            "transcript_path",
            "user_email",
            "tool_response",
            "last_assistant_message",
        ] {
            assert!(
                !text.contains(private_marker),
                "fixture must not contain {private_marker}"
            );
        }
    }
}

#[test]
fn invalid_schema_and_version_are_distinguished() {
    let mut invalid_shape = value(CODEX);
    invalid_shape["schema"] = Value::Bool(true);
    expect_error(&invalid_shape, ContractErrorKind::InvalidSchema);

    let mut unsupported_schema = value(CODEX);
    unsupported_schema["schema"] = json!("shared-context.dynamic-scenario-next");
    expect_error(&unsupported_schema, ContractErrorKind::UnsupportedSchema);

    let mut unsupported_version = value(CODEX);
    unsupported_version["version"] = json!("2");
    expect_error(&unsupported_version, ContractErrorKind::UnsupportedVersion);

    let mut invalid_agent_version = value(CODEX);
    invalid_agent_version["agent"]["version"] = json!("rolling-latest");
    expect_error(&invalid_agent_version, ContractErrorKind::InvalidSchema);

    let mut invalid_vendor_framing = value(CODEX);
    invalid_vendor_framing["agent"]["vendor"] = json!("cursor");
    expect_error(&invalid_vendor_framing, ContractErrorKind::InvalidSchema);
}

#[test]
fn duplicate_actor_step_variable_fault_assertion_and_event_are_rejected() {
    let cases = [
        (
            "/actors/1/id",
            json!("session"),
            ContractErrorKind::DuplicateActor,
        ),
        (
            "/actions/1/id",
            json!("session-start"),
            ContractErrorKind::DuplicateStep,
        ),
        (
            "/variables/1/name",
            json!("task-id"),
            ContractErrorKind::DuplicateVariable,
        ),
        (
            "/faults/0/id",
            json!("repeat-turn-stop"),
            ContractErrorKind::DuplicateFault,
        ),
        (
            "/assertions/1/id",
            json!("one-active-task"),
            ContractErrorKind::DuplicateAssertion,
        ),
        (
            "/events/1/event",
            json!("SessionStart"),
            ContractErrorKind::DuplicateEvent,
        ),
    ];

    for (pointer, replacement, expected) in cases {
        let mut scenario = value(CODEX);
        if pointer == "/faults/0/id" {
            let clone = scenario["faults"][0].clone();
            scenario["faults"]
                .as_array_mut()
                .expect("faults array")
                .push(clone);
        } else {
            *scenario.pointer_mut(pointer).expect("fixture pointer") = replacement;
        }
        expect_error(&scenario, expected);
    }
}

#[test]
fn dangling_forward_cycle_and_not_ready_references_are_rejected() {
    let mut dangling = value(CODEX);
    dangling["actions"][1]["after"] = json!(["missing-step"]);
    expect_error(&dangling, ContractErrorKind::DanglingReference);

    let mut forward = value(CODEX);
    forward["actions"][1]["after"] = json!(["checkpoint"]);
    forward["actions"][2]["after"] = json!(["session-start"]);
    expect_error(&forward, ContractErrorKind::ForwardReference);

    let mut cycle = value(CODEX);
    cycle["actions"][0]["after"] = json!(["turn-stop"]);
    expect_error(&cycle, ContractErrorKind::DependencyCycle);

    let mut not_ready = value(CODEX);
    not_ready["actions"][3]["after"] = json!([]);
    expect_error(&not_ready, ContractErrorKind::VariableNotReady);

    let mut dependency_order = value(CODEX);
    dependency_order["actions"][5]["after"] = json!(["confirm", "intent-update"]);
    expect_error(&dependency_order, ContractErrorKind::InvalidDependencyOrder);
}

#[test]
fn capture_sources_must_exist_precede_consumers_and_produce_output() {
    let mut dangling_capture = value(CODEX);
    dangling_capture["variables"][0]["capture"]["step"] = json!("missing-step");
    expect_error(&dangling_capture, ContractErrorKind::DanglingReference);

    let mut forward_capture = value(CODEX);
    forward_capture["variables"][0]["capture"]["step"] = json!("confirm");
    expect_error(&forward_capture, ContractErrorKind::ForwardReference);

    let mut no_output = value(CODEX);
    no_output["actions"][0]["action"] = json!({"type": "restart"});
    no_output["variables"][0]["capture"]["step"] = json!("session-start");
    expect_error(&no_output, ContractErrorKind::InvalidCapture);

    let mut whole_output = value(CODEX);
    whole_output["variables"][0]["capture"]["pointer"] = json!("");
    expect_error(&whole_output, ContractErrorKind::InvalidSchema);
}

#[test]
fn every_domain_id_prefix_is_rejected_but_business_prefix_text_is_allowed() {
    const PREFIXES: [&str; 27] = [
        "spc_", "rpo_", "ref_", "tsk_", "tss_", "xss_", "tir_", "sig_", "cap_", "wep_", "wob_",
        "ckp_", "clm_", "bld_", "rec_", "cnd_", "sub_", "cfm_", "asc_", "ctx_", "rev_", "evt_",
        "pub_", "evd_", "rvw_", "cnf_", "rsl_",
    ];
    for prefix in PREFIXES {
        let mut scenario = value(CODEX);
        scenario["actions"][0]["action"]["payload"]["fields"]["source"]["value"] = json!(format!(
            "captured={prefix}123e4567-e89b-42d3-a456-426614174000"
        ));
        expect_error(&scenario, ContractErrorKind::HardcodedDomainId);
    }

    parse_scenario(CODEX).expect("ordinary tsk_documentation text must not be rejected");
}

#[test]
fn forged_expected_documents_and_unknown_actions_are_rejected() {
    let mut expected = value(CODEX);
    expected["expected_output"] = json!({"candidate_id": "copied-production-output"});
    expect_error(&expected, ContractErrorKind::ForgedExpected);

    let mut model_text = value(CODEX);
    model_text["model_text"] = json!("a model-authored golden response");
    expect_error(&model_text, ContractErrorKind::ForgedExpected);

    let mut unsupported = value(CODEX);
    unsupported["actions"][0]["action"] = json!({"type": "network_request"});
    expect_error(&unsupported, ContractErrorKind::UnsupportedAction);
}

#[test]
fn fault_targets_are_existing_independent_product_actions() {
    let mut dangling = value(CODEX);
    dangling["faults"][0]["fault"]["target"] = json!("missing-step");
    expect_error(&dangling, ContractErrorKind::InvalidFaultTarget);

    let mut observer = value(CODEX);
    observer["faults"][0]["fault"]["target"] = json!("confirmation-observation");
    expect_error(&observer, ContractErrorKind::InvalidFaultTarget);

    let mut causal_reorder = value(CODEX);
    causal_reorder["faults"][0]["fault"] = json!({
        "type": "reorder",
        "first": "intent-update",
        "second": "checkpoint"
    });
    expect_error(&causal_reorder, ContractErrorKind::InvalidFaultTarget);
}

#[test]
fn observed_only_and_unsupported_events_cannot_trigger_product_actions() {
    for event_index in [2, 3] {
        let mut scenario = value(CODEX);
        scenario["events"][event_index]["product_action"] = json!("post_tool_use");
        expect_error(&scenario, ContractErrorKind::InvalidEventClassification);
    }
}

fn assert_hook_action_rejected_by_parse_and_validate(event: &str) {
    let mut document = value(CODEX);
    document["actions"][0]["action"]["event"] = json!(event);
    expect_error(&document, ContractErrorKind::InvalidEventClassification);

    let mut scenario = parse_scenario(CODEX).expect("base scenario is valid");
    let sctx_scenario_contract::ActionKind::HookEvent {
        event: action_event,
        ..
    } = &mut scenario.actions[0].action
    else {
        panic!("first fixture action must remain a Hook event");
    };
    *action_event = WireName::new(event).expect("test event name is bounded");
    let error = scenario
        .validate()
        .expect_err("in-memory validation must enforce event classification");
    assert_eq!(error.kind(), ContractErrorKind::InvalidEventClassification);
}

#[test]
fn observed_only_event_cannot_be_a_hook_action() {
    assert_hook_action_rejected_by_parse_and_validate("afterAgentThought");
}

#[test]
fn unsupported_event_cannot_be_a_hook_action() {
    assert_hook_action_rejected_by_parse_and_validate("beforeTabFileRead");
}

#[test]
fn unclassified_event_cannot_be_a_hook_action() {
    assert_hook_action_rejected_by_parse_and_validate("unclassifiedEvent");
}

#[test]
fn supported_event_remains_a_valid_hook_action() {
    let parsed = parse_scenario(CODEX).expect("supported Hook actions must remain valid");
    parsed
        .validate()
        .expect("supported Hook actions must validate in memory");
}

#[test]
fn typed_invariants_reject_wrong_or_dangling_variable_kinds() {
    let mut wrong_kind = value(CODEX);
    wrong_kind["variables"][0]["value_type"] = json!("context_id");
    expect_error(&wrong_kind, ContractErrorKind::VariableTypeMismatch);

    let mut dangling = value(CODEX);
    dangling["assertions"][0]["invariant"]["task"] = json!("unknown-task-id");
    expect_error(&dangling, ContractErrorKind::DanglingReference);
}

#[test]
fn action_and_document_capacity_limits_are_enforced_before_execution() {
    let mut too_many_actions = value(CODEX);
    let template = too_many_actions["actions"][0].clone();
    let actions = too_many_actions["actions"]
        .as_array_mut()
        .expect("actions array");
    actions.clear();
    for index in 0..=MAX_ACTIONS {
        let mut action = template.clone();
        action["id"] = json!(format!("step-{index}"));
        actions.push(action);
    }
    expect_error(&too_many_actions, ContractErrorKind::CapacityExceeded);

    let oversized = vec![b' '; MAX_DOCUMENT_BYTES + 1];
    let error = parse_scenario(&oversized).expect_err("oversized input must fail first");
    assert_eq!(error.kind(), ContractErrorKind::DocumentTooLarge);
}

#[test]
fn malformed_json_and_excessive_template_depth_are_bounded() {
    let error = parse_scenario(b"{").expect_err("malformed JSON must fail");
    assert_eq!(error.kind(), ContractErrorKind::InvalidJson);

    let mut scenario = value(CODEX);
    let mut nested = json!({"type": "null"});
    for _ in 0..=20 {
        nested = json!({"type": "array", "items": [nested]});
    }
    scenario["actions"][0]["action"]["payload"] = nested;
    expect_error(&scenario, ContractErrorKind::CapacityExceeded);
}
