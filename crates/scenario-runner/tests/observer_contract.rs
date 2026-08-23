use std::{fs, path::Path, process::Command};

use rusqlite::Connection;
use sctx_scenario_contract::ObservationSource;
use sctx_scenario_runner::{
    ObservationEntity, ObservedGeneration, ReadOnlyObserver, state_fingerprint,
};
use serde_json::json;
use tempfile::tempdir;

fn observer() -> ReadOnlyObserver {
    ReadOnlyObserver::new("/usr/bin/git")
}

fn wal_database(path: &Path, schema: &str) -> Connection {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; {schema}"
        ))
        .unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    connection
}

#[test]
fn runtime_observer_reads_uncheckpointed_wal_head_without_touching_source_bytes() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("root");
    let database = root.join("state/runtime.sqlite");
    let writer = wal_database(
        &database,
        "CREATE TABLE work_episode (
            episode_id TEXT PRIMARY KEY,
            status TEXT NOT NULL
        ); PRAGMA user_version = 11;",
    );
    writer
        .execute(
            "INSERT INTO work_episode (episode_id, status) VALUES (?1, 'open')",
            [format!("wep_{}", uuid::Uuid::new_v4().hyphenated())],
        )
        .unwrap();
    assert!(database.with_file_name("runtime.sqlite-wal").exists());

    let before = state_fingerprint(&root).unwrap();
    let observed = observer()
        .observe(
            &root,
            ObservationSource::Runtime,
            &json!({"entity": "work_episode"}),
        )
        .unwrap();
    let after = state_fingerprint(&root).unwrap();

    assert_eq!(observed.summary.entity, ObservationEntity::WorkEpisode);
    assert_eq!(observed.summary.count, 1);
    assert_eq!(observed.summary.status.as_deref(), Some("open"));
    assert_eq!(observed.summary.schema_version, Some(11));
    assert!(observed.unchanged);
    assert_eq!(before, after);
    drop(writer);
}

#[test]
fn index_and_graph_observers_read_latest_wal_generations_without_source_writes() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("root");
    let index_database = root.join("state/index.sqlite");
    let index_writer = wal_database(
        &index_database,
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE source_file (path TEXT PRIMARY KEY);
         PRAGMA user_version = 11;",
    );
    index_writer
        .execute_batch(
            "INSERT INTO meta VALUES ('indexed_tree_oid', '1111111111111111111111111111111111111111');
             INSERT INTO meta VALUES ('projection_generation', '7');
             INSERT INTO source_file VALUES ('events/synthetic.json');",
        )
        .unwrap();

    let graph_database = root.join("state/engineering.sqlite");
    let graph_writer = wal_database(
        &graph_database,
        "CREATE TABLE projection_meta (
            singleton INTEGER PRIMARY KEY,
            policy_version TEXT NOT NULL,
            artifact_generation TEXT NOT NULL,
            context_tree_oid TEXT
         );
         CREATE TABLE graph_context_snapshot (
            context_id TEXT NOT NULL,
            revision_id TEXT NOT NULL,
            artifact_generation TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            PRIMARY KEY (context_id, revision_id)
         );
         CREATE TABLE resolved_reference (
            reference_id TEXT PRIMARY KEY,
            artifact_generation TEXT NOT NULL,
            payload_json TEXT NOT NULL
         );
         PRAGMA user_version = 4;",
    );
    graph_writer
        .execute_batch(
            "INSERT INTO projection_meta VALUES (
                1, 'policy-v1', 'eng_synthetic', '2222222222222222222222222222222222222222'
             );
             INSERT INTO graph_context_snapshot VALUES (
                'synthetic-context', 'synthetic-revision', 'eng_synthetic', '{}'
             );",
        )
        .unwrap();

    let before = state_fingerprint(&root).unwrap();
    let index = observer()
        .observe(
            &root,
            ObservationSource::Index,
            &json!({"entity": "index_projection"}),
        )
        .unwrap();
    let graph = observer()
        .observe(
            &root,
            ObservationSource::Graph,
            &json!({"entity": "graph_projection"}),
        )
        .unwrap();
    let after = state_fingerprint(&root).unwrap();

    assert_eq!(
        index.summary.generation,
        Some(ObservedGeneration::Number(7))
    );
    assert_eq!(
        index.summary.tree.as_deref(),
        Some("1111111111111111111111111111111111111111")
    );
    assert_eq!(index.summary.count, 1);
    assert_eq!(
        graph.summary.generation,
        Some(ObservedGeneration::Opaque("eng_synthetic".to_owned()))
    );
    assert_eq!(
        graph.summary.tree.as_deref(),
        Some("2222222222222222222222222222222222222222")
    );
    assert_eq!(graph.summary.count, 1);
    assert_eq!(before, after);
    drop((index_writer, graph_writer));
}

#[test]
fn missing_database_observation_never_initializes_state() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("missing-root");
    fs::create_dir(&root).unwrap();
    let before = state_fingerprint(&root).unwrap();
    let observed = observer()
        .observe(
            &root,
            ObservationSource::Runtime,
            &json!({"entity": "candidate_review"}),
        )
        .unwrap();
    assert!(!observed.summary.present);
    assert_eq!(observed.summary.count, 0);
    assert_eq!(before, state_fingerprint(&root).unwrap());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

#[test]
fn fixed_active_task_and_candidate_episode_selectors_return_only_safe_identity_counts() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("root");
    let runtime = wal_database(
        &root.join("state/runtime.sqlite"),
        "CREATE TABLE external_session (
            external_session_key TEXT PRIMARY KEY,
            active_task_id TEXT NOT NULL
         ); PRAGMA user_version = 11;",
    );
    let task_id = format!("tsk_{}", uuid::Uuid::new_v4().hyphenated());
    runtime
        .execute(
            "INSERT INTO external_session VALUES ('scenario-session', ?1)",
            [&task_id],
        )
        .unwrap();
    let active = observer()
        .observe(
            &root,
            ObservationSource::Runtime,
            &json!({"entity": "active_task", "session_key": "scenario-session"}),
        )
        .unwrap();
    assert_eq!(active.summary.count, 1);
    assert_eq!(active.summary.identity.as_deref(), Some(task_id.as_str()));

    let index = wal_database(
        &root.join("state/index.sqlite"),
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE context_candidate (
            candidate_id TEXT PRIMARY KEY,
            source_episode_id TEXT NOT NULL
         ); PRAGMA user_version = 11;",
    );
    index
        .execute_batch(
            "INSERT INTO meta VALUES ('indexed_tree_oid', '3333333333333333333333333333333333333333');
             INSERT INTO meta VALUES ('projection_generation', '1');",
        )
        .unwrap();
    let episode_id = format!("wep_{}", uuid::Uuid::new_v4().hyphenated());
    let candidate_id = format!("cnd_{}", uuid::Uuid::new_v4().hyphenated());
    index
        .execute(
            "INSERT INTO context_candidate VALUES (?1, ?2)",
            [&candidate_id, &episode_id],
        )
        .unwrap();
    let candidates = observer()
        .observe(
            &root,
            ObservationSource::Index,
            &json!({"entity": "index_candidate_for_episode", "identity": episode_id}),
        )
        .unwrap();
    assert_eq!(candidates.summary.count, 1);
    assert_eq!(
        candidates.summary.identity.as_deref(),
        Some(candidate_id.as_str())
    );
    drop((runtime, index));
}

#[test]
fn git_observer_reads_only_committed_head_tree_and_path_counts() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("root");
    let repository = root.join("repository");
    fs::create_dir_all(repository.join("events")).unwrap();
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["config", "user.name", "Scenario Test"]);
    git(
        &repository,
        &["config", "user.email", "scenario@example.invalid"],
    );
    fs::write(
        repository.join("events/synthetic.json"),
        b"{\"synthetic\":true}\n",
    )
    .unwrap();
    git(&repository, &["add", "events/synthetic.json"]);
    git(&repository, &["commit", "--quiet", "-m", "synthetic"]);

    let before = state_fingerprint(&root).unwrap();
    let observed = observer()
        .observe(
            &root,
            ObservationSource::Git,
            &json!({"entity": "git_event"}),
        )
        .unwrap();
    let after = state_fingerprint(&root).unwrap();
    assert_eq!(observed.summary.count, 1);
    assert!(observed.summary.tree.as_deref().is_some_and(is_oid));
    assert!(observed.summary.identity.as_deref().is_some_and(is_oid));
    assert!(observed.summary.status.is_none());
    assert_eq!(before, after);
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?}");
}

fn is_oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
