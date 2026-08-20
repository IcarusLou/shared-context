use std::{fs, process::Command};

use sctx_domain::{ContextId, SpaceId, TaskId, TaskIntent, TaskSpaceAssociation, WorkEpisodeId};
use serde_json::{Value, json};
use tempfile::tempdir;

const EVIDENCE: &str = r#"{"kind":"experiment_record","supports":"M1 candidate creation completed","content":{"gate":"milestone_one","actual":"created"},"interpretation":"the task-first Candidate path is executable","limitations":[]}"#;

fn task_intent(task_id: TaskId) -> TaskIntent {
    TaskIntent {
        task_id,
        goal: "Close milestone one".to_owned(),
        desired_change: "Make task-first primitives the only routing model".to_owned(),
        in_scope: vec!["M1 integration".to_owned()],
        out_of_scope: vec!["M2 runtime retrieval".to_owned()],
        domains: vec!["shared-context".to_owned()],
        platforms: Vec::new(),
        constraints: vec!["No Workspace route".to_owned()],
        acceptance_conditions: vec!["One Task can have zero or many Space matches".to_owned()],
        artifacts: Vec::new(),
        interfaces: vec!["candidate_create".to_owned()],
        unknowns: Vec::new(),
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
    let intent = task_intent(task_id);
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

#[test]
fn candidate_create_is_the_unassigned_main_path_and_never_auto_injects() {
    let temporary = tempdir().unwrap();
    let home = temporary.path().join("M1 home");
    fs::create_dir_all(&home).unwrap();
    let episode_id = WorkEpisodeId::new().to_string();
    let binary = env!("CARGO_BIN_EXE_sctx");

    let created = Command::new(binary)
        .arg("--json")
        .args([
            "candidate",
            "create",
            "--source-episode-id",
            &episode_id,
            "--kind",
            "discovery",
            "--statement",
            "M1 Candidate must remain outside automatic injection",
            "--rationale",
            "unconfirmed task knowledge cannot become trusted Context",
            "--evidence-json",
            EVIDENCE,
        ])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created["command"], "candidate.create");
    assert_eq!(created["data"]["status"], "candidate");
    assert!(created["data"]["candidate_id"].as_str().is_some());
    assert!(
        created["data"]
            .as_object()
            .unwrap()
            .keys()
            .all(|field| !field.contains("space") && !field.contains("workspace"))
    );

    let config = fs::read_to_string(home.join(".shared-context/config.toml")).unwrap();
    let config_keys = config
        .lines()
        .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
        .collect::<Vec<_>>();
    assert_eq!(config_keys, vec!["version", "store"]);

    let pack = Command::new(binary)
        .arg("--json")
        .args([
            "context-pack",
            "--query",
            "M1 Candidate",
            "--automatic",
            "--token-budget",
            "1000",
        ])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        pack.status.success(),
        "{}",
        String::from_utf8_lossy(&pack.stderr)
    );
    let pack: Value = serde_json::from_slice(&pack.stdout).unwrap();
    assert_eq!(pack["data"]["mode"], "automatic_injection");
    assert_eq!(pack["data"]["items"], json!([]));

    let help = Command::new(binary).arg("--help").output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("candidate create"));
    let removed_command = ["workspace", "bind"].join(" ");
    assert!(!help.to_ascii_lowercase().contains(&removed_command));
}
