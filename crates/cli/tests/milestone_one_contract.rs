use sctx_domain::{ContextId, SpaceId, TaskId, TaskSpaceAssociation, WorkingIntentSnapshot};

fn task_intent() -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: "Close milestone one".to_owned(),
        current_direction: Some("Make task-first primitives the only routing model".to_owned()),
        in_scope: vec!["M1 integration".to_owned()],
        out_of_scope: vec!["M2 runtime retrieval".to_owned()],
        domains: vec!["shared-context".to_owned()],
        platforms: Vec::new(),
        constraints: vec!["No Workspace route".to_owned()],
        acceptance_conditions: vec!["One Task can have zero or many Space matches".to_owned()],
        artifact_hints: Vec::new(),
        interface_hints: vec!["task_checkpoint".to_owned()],
        open_questions: Vec::new(),
    }
}

fn association(task_id: TaskId, space_id: SpaceId, reason: &str) -> TaskSpaceAssociation {
    TaskSpaceAssociation {
        task_id,
        space_id,
        score: 0.8,
        matched_intent_fields: vec!["goal".to_owned()],
        matched_artifacts: Vec::new(),
        matched_contexts: vec![ContextId::new()],
        relation_paths: Vec::new(),
        reasons: vec![reason.to_owned()],
    }
}

#[test]
fn task_intent_has_no_route_and_accepts_zero_or_many_space_associations() {
    let task_id = TaskId::new();
    let intent = task_intent();
    intent.validate().unwrap();
    let serialized = serde_json::to_value(intent).unwrap();
    assert!(
        serialized
            .as_object()
            .unwrap()
            .keys()
            .all(|field| !field.contains("space") && !field.contains("workspace"))
    );

    TaskSpaceAssociation::validate_collection(task_id, &[]).unwrap();
    TaskSpaceAssociation::validate_collection(
        task_id,
        &[
            association(task_id, SpaceId::new(), "matched first Intent"),
            association(task_id, SpaceId::new(), "matched second Intent"),
        ],
    )
    .unwrap();
}
