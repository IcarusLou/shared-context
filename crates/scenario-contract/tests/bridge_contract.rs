use sctx_scenario_contract::{ContractErrorKind, parse_scenario, to_canonical_json};
use serde_json::{Value, json};

const CODEX: &[u8] = include_bytes!("fixtures/valid/codex-v1.json");

fn document() -> Value {
    serde_json::from_slice(CODEX).unwrap()
}

fn error(value: &Value, expected: ContractErrorKind) {
    let encoded = serde_json::to_vec(value).unwrap();
    let error = parse_scenario(&encoded).expect_err("bridge input must be rejected");
    assert_eq!(error.kind(), expected, "{}", error.message());
}

fn add_resource(value: &mut Value, paths: &[&str]) {
    value["resources"] = Value::Array(vec![json!({
        "id": "focus-repo",
        "synthetic_git": true,
        "files": paths.iter().map(|path| json!({
            "path": path,
            "content": "pub fn synthetic_focus() -> bool { true }\n"
        })).collect::<Vec<_>>()
    })]);
}

#[test]
fn resource_builtin_and_default_success_roundtrip_stably() {
    let mut value = document();
    add_resource(&mut value, &["src/focus.rs"]);
    value["actions"][1]["action"]["params"]["fields"]["resource_file"] = json!({
        "type": "builtin",
        "builtin": {
            "type": "resource_file",
            "resource": "focus-repo",
            "path": "src/focus.rs"
        }
    });
    value["actions"][1]["action"]["params"]["fields"]["session_key"] = json!({
        "type": "builtin",
        "builtin": {"type": "actor_session_key", "actor": "session"}
    });

    let scenario = parse_scenario(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(scenario.actions.iter().all(|action| {
        matches!(
            action.expectation,
            sctx_scenario_contract::ActionExpectation::Success
        )
    }));
    let canonical = to_canonical_json(&scenario).unwrap();
    assert_eq!(
        canonical,
        to_canonical_json(&parse_scenario(&canonical).unwrap()).unwrap()
    );
}

#[test]
fn unsafe_resource_paths_and_collisions_are_rejected() {
    for path in [
        "/absolute.rs",
        "../escape.rs",
        "src\\escape.rs",
        ".git/config",
        ".gitignore",
    ] {
        let mut value = document();
        add_resource(&mut value, &[path]);
        error(&value, ContractErrorKind::InvalidSchema);
    }

    for paths in [["src", "src/focus.rs"], ["src/Focus.rs", "src/focus.rs"]] {
        let mut value = document();
        add_resource(&mut value, &paths);
        error(&value, ContractErrorKind::InvalidResource);
    }
}

#[test]
fn unknown_resource_file_and_non_session_builtin_are_rejected() {
    let mut unknown = document();
    add_resource(&mut unknown, &["src/focus.rs"]);
    unknown["actions"][1]["action"]["params"]["fields"]["resource"] = json!({
        "type": "builtin",
        "builtin": {
            "type": "resource_file",
            "resource": "focus-repo",
            "path": "src/missing.rs"
        }
    });
    error(&unknown, ContractErrorKind::DanglingReference);

    let mut actor = document();
    actor["actions"][1]["action"]["params"]["fields"]["session"] = json!({
        "type": "builtin",
        "builtin": {"type": "actor_session_key", "actor": "task"}
    });
    error(&actor, ContractErrorKind::DanglingReference);
}

#[test]
fn typed_failure_shape_cannot_capture_fault_or_hook_output() {
    let expectation = json!({
        "type": "typed_failure",
        "code": "intent_stale",
        "kind": "stale_state"
    });

    let mut hook = document();
    hook["actions"][0]["expectation"] = expectation.clone();
    error(&hook, ContractErrorKind::InvalidExpectation);

    let mut capture = document();
    capture["actions"][1]["expectation"] = expectation;
    error(&capture, ContractErrorKind::InvalidCapture);

    let mut unsafe_code = document();
    unsafe_code["actions"][1]["expectation"] = json!({
        "type": "typed_failure",
        "code": "Not Safe",
        "kind": "stale_state"
    });
    error(&unsafe_code, ContractErrorKind::InvalidSchema);
}

#[test]
fn resources_reject_domain_ids_and_capacity_overflow() {
    let mut domain_id = document();
    add_resource(&mut domain_id, &["src/focus.rs"]);
    domain_id["resources"][0]["files"][0]["content"] =
        json!(format!("tsk_{}", uuid::Uuid::new_v4().hyphenated()));
    error(&domain_id, ContractErrorKind::HardcodedDomainId);

    let mut capacity = document();
    add_resource(&mut capacity, &["src/focus.rs"]);
    capacity["resources"][0]["files"][0]["content"] = json!("x".repeat(4_097));
    error(&capacity, ContractErrorKind::CapacityExceeded);
}

#[test]
fn invariant_response_cannot_point_at_the_wrong_operation() {
    let mut value = document();
    value["assertions"][0]["invariant"] = json!({
        "type": "working_intent_hint_has_no_graph_path",
        "response": "confirm"
    });
    error(&value, ContractErrorKind::InvalidSchema);
}
