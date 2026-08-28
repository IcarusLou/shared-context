use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use rusqlite::Connection;
use sctx_domain::{
    Applicability, ArtifactKind, ArtifactLocator, ContextId, ContextKind, ContextRevisionDraft,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, PublicationAction,
    PublicationDraft, ReferenceRelation, RepoRelativePath, ReviewDraft, ReviewVerdict, RevisionId,
    TaskId, WorkingIntentSnapshot,
};
use sctx_engineering_graph::{EngineeringProjectionStore, RepositoryRegistry};
use sctx_event_schema::{Event, IntentSnapshot};
use sctx_git_store::{AppendRequest, GitStore, TextObject};
use sctx_index::ProjectionIndex;
use sctx_installer::{
    Agent, Architecture, CheckStatus, DataResetOptions, Host, InstallContext, Installer,
    KnowledgeRemoteType, KnowledgeStoreUrl, ResetStage, SetupOptions, SetupStage, SkillStatus,
};
use sctx_local_state::{MaintenanceLock, PrivacyScanner, UserConfigStore};
use sctx_mcp::{
    ArtifactFocusQuery, ArtifactFocusQueryCoordinates, AssociationExplainInput,
    AssociationRebuildInput, EngineeringReferenceRecordInput, ExpectedRevisionId,
    RepositoryScanInput, TaskBoundary, TaskIntentUpdateInput, association_explain_at_root,
    association_rebuild_at_root, engineering_reference_record_at_root, repository_scan_at_root,
    task_artifact_focus_at_root, task_intent_update_at_root,
};
use sctx_search::TaskRetrievalPath;
use sctx_task_runtime::TaskRuntime;
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};

const KNOWLEDGE_SYNC_PUSH_ATTEMPTS_FOR_TEST: usize = 3;

#[derive(serde::Deserialize)]
struct TeamSharingOracle {
    schema: String,
    version: u32,
    repository_id: String,
    relative_path: String,
    source: String,
    context_statement: String,
    context_topic: String,
    base_branch: String,
    work_branch_prefix: String,
}

fn team_sharing_oracle() -> TeamSharingOracle {
    let raw = include_str!("../../../fixtures/team-sharing/team-sharing-v1.json");
    assert!(PrivacyScanner::default().scan(raw).unwrap().is_clean());
    assert!(!raw.to_ascii_lowercase().contains("/users/"));
    assert!(!raw.contains(".codex/") && !raw.contains(".cursor/"));
    serde_json::from_str(raw).unwrap()
}

#[derive(Clone)]
struct FakeHost {
    platform: &'static str,
    architecture: Option<Architecture>,
    git_ok: bool,
    signature_ok: bool,
    space: u64,
    cursor_version: Option<&'static str>,
    codex_version: Option<&'static str>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            platform: "macos",
            architecture: Some(Architecture::Arm64),
            git_ok: true,
            signature_ok: true,
            space: u64::MAX,
            cursor_version: Some("3.13.10"),
            codex_version: Some("0.147.0"),
        }
    }
}

impl Host for FakeHost {
    fn platform(&self) -> &str {
        self.platform
    }

    fn architecture(&self) -> Option<Architecture> {
        self.architecture
    }

    fn git_version(&self) -> sctx_installer::Result<String> {
        if self.git_ok {
            Ok("git version fixture".to_owned())
        } else {
            Err(sctx_installer::Error::new(
                sctx_installer::ErrorKind::External,
                "Git unavailable",
            ))
        }
    }

    fn verify_signature(&self, _executable: &Path) -> sctx_installer::Result<()> {
        if self.signature_ok {
            Ok(())
        } else {
            Err(sctx_installer::Error::new(
                sctx_installer::ErrorKind::InvariantViolation,
                "invalid signature",
            ))
        }
    }

    fn available_space(&self, _path: &Path) -> sctx_installer::Result<u64> {
        Ok(self.space)
    }

    fn agent_version(&self, agent: Agent) -> Option<String> {
        match agent {
            Agent::Cursor => self.cursor_version,
            Agent::Codex => self.codex_version,
        }
        .map(str::to_owned)
    }
}

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    runtime: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("用户 Home 空格");
        let root = temporary.path().join("安装 根目录 中文");
        let runtime = temporary.path().join("runtime source/sctx fixture");
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(&runtime, b"signed-runtime-v1").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            runtime,
        }
    }

    fn context(&self, version: &str) -> InstallContext {
        InstallContext::injected(&self.home, &self.root, &self.runtime, version)
    }

    fn installer(&self, version: &str) -> Installer {
        Installer::new(self.context(version), Arc::new(FakeHost::default()))
    }

    fn skill_root(&self) -> PathBuf {
        self.home.join(".agents/skills/shared-context")
    }

    fn seed_configs(&self) -> Vec<(PathBuf, Vec<u8>, u32)> {
        let fixtures = [
            (
                self.home.join(".cursor/mcp.json"),
                r#"{
  "unknownTop": {"中文": true},
  "mcpServers": {"existing": {"command": "keep me"}}
}
"#
                .as_bytes()
                .to_vec(),
                0o640,
            ),
            (
                self.home.join(".cursor/hooks.json"),
                br#"{
  "version": 1,
  "unknown": "keep",
  "hooks": {"stop": [{"command": "user-stop", "future": 7}]}
}
"#
                .to_vec(),
                0o604,
            ),
            (
                self.home.join(".codex/config.toml"),
                b"# keep this leading comment\nmodel = \"fixture\" # keep inline\n\n[mcp_servers.existing]\ncommand = \"keep me\"\nunknown = true\n"
                    .to_vec(),
                0o600,
            ),
            (
                self.home.join(".codex/hooks.json"),
                br#"{
  "description": "keep unknown metadata",
  "hooks": {"Stop": [{"matcher": "custom", "hooks": [{"type": "command", "command": "keep"}]}]}
}
"#
                .to_vec(),
                0o620,
            ),
        ];
        fixtures
            .into_iter()
            .map(|(path, bytes, mode)| {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, &bytes).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                (path, bytes, mode)
            })
            .collect()
    }
}

fn runtime_database(root: &Path) -> PathBuf {
    root.join("state/runtime.sqlite")
}

fn runtime_files(root: &Path) -> [PathBuf; 3] {
    let database = runtime_database(root);
    [
        database.clone(),
        database.with_extension("sqlite-wal"),
        database.with_extension("sqlite-shm"),
    ]
}

fn mark_runtime_as_legacy_schema(root: &Path, version: u32, with_operation: bool) {
    assert!(matches!(version, 11 | 12));
    let connection = Connection::open(runtime_database(root)).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
            CREATE TABLE IF NOT EXISTS capture_ingestion (
                capture_id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL,
                observation_id TEXT NOT NULL UNIQUE,
                task_session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                FOREIGN KEY (observation_id, episode_id)
                    REFERENCES work_observation (observation_id, episode_id),
                FOREIGN KEY (episode_id, task_session_id, task_id)
                    REFERENCES work_episode (episode_id, task_session_id, task_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS work_episode_diagnostic (
                episode_id TEXT NOT NULL,
                diagnostic_ordinal INTEGER NOT NULL CHECK (diagnostic_ordinal >= 0),
                capture_id TEXT NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN (
                    'capture_repository_not_configured',
                    'capture_unsafe_artifact_path'
                )),
                PRIMARY KEY (episode_id, diagnostic_ordinal),
                UNIQUE (episode_id, capture_id, kind),
                FOREIGN KEY (episode_id) REFERENCES work_episode (episode_id)
            ) STRICT;",
        )
        .unwrap();
    if with_operation {
        connection
            .execute_batch(
                "INSERT INTO checkpoint_operation (
                    operation_key, operation_id, semantic_json, task_session_id, task_id,
                    intent_revision_id, checkpoint_id, episode_id, build_id
                ) VALUES (
                    'legacy-operation-key', 'cop_00000000-0000-4000-8000-000000000001', '{}',
                    'tss_00000000-0000-4000-8000-000000000002',
                    'tsk_00000000-0000-4000-8000-000000000003',
                    'tir_00000000-0000-4000-8000-000000000004',
                    'ckp_00000000-0000-4000-8000-000000000005',
                    'wep_00000000-0000-4000-8000-000000000006',
                    'bld_00000000-0000-4000-8000-000000000007'
                );",
            )
            .unwrap();
    } else {
        connection
            .execute_batch("DROP TABLE checkpoint_operation;")
            .unwrap();
    }
    connection
        .pragma_update(None, "user_version", version)
        .unwrap();
    let stored_version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(stored_version, i64::from(version));
    assert_eq!(
        sqlite_table_exists(&connection, "checkpoint_operation"),
        with_operation
    );
    assert!(sqlite_table_exists(&connection, "capture_ingestion"));
    assert!(sqlite_table_exists(&connection, "work_episode_diagnostic"));
    if with_operation {
        assert_eq!(runtime_operation_count(&connection), 1);
    }
}

fn mark_runtime_as_schema_12(root: &Path) {
    mark_runtime_as_legacy_schema(root, 12, true);
}

fn set_runtime_schema_version(root: &Path, version: u32) {
    let connection = Connection::open(runtime_database(root)).unwrap();
    connection
        .pragma_update(None, "user_version", version)
        .unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        i64::from(version)
    );
}

fn sqlite_table_exists(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
}

fn runtime_operation_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM checkpoint_operation", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn assert_runtime_schema_13(root: &Path) {
    let connection = Connection::open(runtime_database(root)).unwrap();
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(version, 13);
    assert!(sqlite_table_exists(&connection, "task_signal"));
    assert!(!sqlite_table_exists(&connection, "capture_ingestion"));
    assert!(!sqlite_table_exists(&connection, "work_episode_diagnostic"));
    assert_eq!(runtime_operation_count(&connection), 0);
}

fn seed_runtime_task(root: &Path, session_id: &str) -> ExternalSessionLocator {
    let locator = ExternalSessionLocator::new("codex", session_id).unwrap();
    TaskRuntime::initialize(root)
        .unwrap()
        .open_or_create(
            locator.clone(),
            TaskId::new(),
            WorkingIntentSnapshot::new("Retain the compatible Runtime task").unwrap(),
            Vec::new(),
        )
        .unwrap();
    locator
}

fn legacy_capture_paths(root: &Path) -> [PathBuf; 3] {
    [
        root.join("state/capture"),
        root.join("state/capture.lock"),
        root.join("state/capture-metadata.json"),
    ]
}

fn seed_legacy_capture_state(root: &Path) {
    let [directory, lock, metadata] = legacy_capture_paths(root);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("cap-legacy.json"), b"legacy capture").unwrap();
    fs::write(lock, b"legacy lock").unwrap();
    fs::write(metadata, b"legacy metadata").unwrap();
}

fn hook_count(path: &Path, events: &[&str]) -> usize {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    events
        .iter()
        .map(|event| value["hooks"][event].as_array().map_or(0, Vec::len))
        .sum()
}

fn replace_owned_skill_bytes(path: &Path, bytes: &[u8], manifest_path: &Path) {
    fs::write(path, bytes).unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    let owned = manifest["skills"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|owned| owned["path"] == path.to_string_lossy().as_ref())
        .unwrap();
    owned["sha256"] = format!("{:x}", Sha256::digest(bytes)).into();
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

fn remove_owned_skill(path: &Path, manifest_path: &Path) {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    manifest["skills"]
        .as_array_mut()
        .unwrap()
        .retain(|owned| owned["path"] != path.to_string_lossy().as_ref());
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

fn manifest_skill_paths(manifest_path: &Path) -> Vec<PathBuf> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    manifest["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|owned| PathBuf::from(owned["path"].as_str().unwrap()))
        .collect()
}

fn init_catalog_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    fs::canonicalize(path).unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct RemoteFixture {
    repository: PathBuf,
    remote: PathBuf,
}

fn remote_fixture(harness: &Harness, name: &str) -> RemoteFixture {
    let seed_root = harness.home.join(format!("{name}-seed-installation"));
    let store = GitStore::bootstrap_local(&seed_root).unwrap();
    let event = Event::space_created(
        IntentSnapshot {
            title: "Remote setup fixture".to_owned(),
            problem: "A second installation needs governed team knowledge".to_owned(),
            desired_outcome: "The remote Event and object validate before activation".to_owned(),
            in_scope: vec!["remote bootstrap".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["projection rebuild succeeds".to_owned()],
            domain_terms: vec!["KnowledgeStore".to_owned()],
        },
        None,
    )
    .unwrap();
    store
        .append_event(
            AppendRequest::event(event).with_object(TextObject::new("remote evidence object")),
        )
        .unwrap();
    let repository = store.repository().to_path_buf();
    let remote = harness.home.join(format!("{name}-knowledge.git"));
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet", "--initial-branch=main"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    git(
        &repository,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repository, &["push", "origin", "main"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    RemoteFixture { repository, remote }
}

fn remote_setup_options(remote: &Path) -> SetupOptions {
    SetupOptions {
        knowledge_store_url: Some(
            remote
                .to_str()
                .unwrap()
                .parse::<KnowledgeStoreUrl>()
                .unwrap(),
        ),
        ..SetupOptions::default()
    }
}

fn append_space(store: &GitStore, title: &str) -> String {
    let event = Event::space_created(
        IntentSnapshot {
            title: title.to_owned(),
            problem: format!("{title} needs shared knowledge"),
            desired_outcome: format!("{title} is synchronized"),
            in_scope: vec!["knowledge sync".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["the Event is present".to_owned()],
            domain_terms: vec!["KnowledgeStore".to_owned()],
        },
        None,
    )
    .unwrap();
    store
        .append_event(AppendRequest::event(event))
        .unwrap()
        .commit_oid
}

fn clone_work_store(root: &Path, remote: &Path, branch: &str) -> GitStore {
    let config = UserConfigStore::initialize(root).unwrap();
    assert!(
        Command::new("git")
            .args(["clone", "--quiet", "--branch", branch, "--"])
            .arg(remote)
            .arg(config.repository())
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("state/pending")).unwrap();
    git(
        config.repository(),
        &["config", "user.name", "Sync Race Writer"],
    );
    git(
        config.repository(),
        &["config", "user.email", "sync-race@localhost"],
    );
    GitStore::open_existing(root).unwrap()
}

fn install_remote_hook(remote: &Path, script: &str) -> PathBuf {
    let hook = remote.join("hooks/pre-receive");
    fs::write(&hook, script).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    hook
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn commit_event_variant(repository: &Path, event: &Event, pretty: bool) -> String {
    let event_id = event.event_id().to_string();
    let uuid = event_id.strip_prefix("evt_").unwrap();
    let relative = format!("events/{}/{event_id}.json", &uuid[..2]);
    let path = repository.join(&relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = if pretty {
        serde_json::to_vec_pretty(event).unwrap()
    } else {
        serde_json::to_vec(event).unwrap()
    };
    fs::write(path, bytes).unwrap();
    git(repository, &["add", "--", &relative]);
    git(repository, &["commit", "-m", "add sync conflict fixture"]);
    git(repository, &["rev-parse", "HEAD"])
}

fn init_team_checkout(path: &Path, oracle: &TeamSharingOracle) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["config", "user.name", "Team Sharing Fixture"]);
    git(path, &["config", "user.email", "team-sharing@localhost"]);
    let file = path.join(&oracle.relative_path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, &oracle.source).unwrap();
    git(path, &["add", "--", &oracle.relative_path]);
    git(path, &["commit", "--quiet", "-m", "team sharing fixture"]);
    fs::canonicalize(path).unwrap()
}

fn append_accepted_team_context(
    root: &Path,
    oracle: &TeamSharingOracle,
) -> (ContextId, RevisionId) {
    let store = GitStore::open_existing(root).unwrap();
    let space = Event::space_created(
        IntentSnapshot {
            title: "Team Shared FE Contract".to_owned(),
            problem: "Independent FE installations need the same governed fact".to_owned(),
            desired_outcome: "Both installations retrieve one accepted Context".to_owned(),
            in_scope: vec![oracle.relative_path.clone()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["B resolves the A-authored reference".to_owned()],
            domain_terms: vec!["KnowledgeStore".to_owned(), "RepositoryId".to_owned()],
        },
        None,
    )
    .unwrap();
    let space_id = match space.payload() {
        sctx_event_schema::EventPayload::SpaceCreated { space_id, .. } => *space_id,
        _ => unreachable!(),
    };
    store.append_event(AppendRequest::event(space)).unwrap();
    let revision = Event::context_revision_added(
        space_id,
        ContextRevisionDraft {
            kind: ContextKind::Contract,
            topic_key: Some(oracle.context_topic.clone()),
            statement: oracle.context_statement.clone(),
            rationale: "A fixed tracked FE artifact proves the cross-install contract".to_owned(),
            applicability: Applicability {
                domains: vec!["team-sharing".to_owned()],
                platforms: vec!["FE".to_owned()],
                conditions: vec!["shared KnowledgeStore".to_owned()],
            },
            assumptions: Vec::new(),
            recheck_when: vec!["sharedSearch implementation changes".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "Both checkouts contain the fixed tracked artifact".to_owned(),
                content: serde_json::json!({
                    "oracle": "shared-context.team-sharing-oracle",
                    "relative_path": oracle.relative_path,
                    "result": "tracked"
                }),
                interpretation: "RepositoryId plus locator is portable across checkout paths"
                    .to_owned(),
                limitations: vec!["Synthetic two-installation fixture".to_owned()],
            }],
        },
        None,
    )
    .unwrap();
    let (context_id, revision_id) = match revision.payload() {
        sctx_event_schema::EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    };
    store.append_event(AppendRequest::event(revision)).unwrap();
    let review = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict: ReviewVerdict::Approve,
            reason: "Fixed cross-install evidence is complete".to_owned(),
        },
        None,
    )
    .unwrap();
    let review_event_id = review.event_id();
    store.append_event(AppendRequest::event(review)).unwrap();
    store
        .append_event(AppendRequest::event(
            Event::publication_changed(
                space_id,
                context_id,
                PublicationDraft {
                    previous_publication_ids: Vec::new(),
                    action: PublicationAction::Publish,
                    revision_id,
                    review_event_ids: vec![review_event_id],
                },
                None,
            )
            .unwrap(),
        ))
        .unwrap();
    (context_id, revision_id)
}

struct SeededResetState {
    preserved: Vec<(PathBuf, Vec<u8>)>,
    remote: PathBuf,
    remote_head: String,
}

fn seed_reset_state(harness: &Harness) -> SeededResetState {
    let installer = harness.installer("1.2.3");
    installer.setup(&SetupOptions::default()).unwrap();
    let catalog_root = harness.home.join("team repositories");
    let fe = init_catalog_repo(&catalog_root.join("fe"));
    let android = init_catalog_repo(&catalog_root.join("android"));
    let catalog_root = fs::canonicalize(catalog_root).unwrap();
    let config = UserConfigStore::open_existing(&harness.root).unwrap();
    let fe_id: sctx_domain::RepositoryId = "FE".parse().unwrap();
    let android_id: sctx_domain::RepositoryId = "Android".parse().unwrap();
    config
        .add_repository(fe_id.clone(), std::slice::from_ref(&fe))
        .unwrap();
    config
        .add_repository(android_id.clone(), std::slice::from_ref(&android))
        .unwrap();
    config
        .add_repository_group(&catalog_root, &[fe_id, android_id])
        .unwrap();

    let repository = harness.root.join("repository");
    git(
        &repository,
        &["commit", "--allow-empty", "-m", "seed reset knowledge"],
    );
    let remote = harness.home.join("knowledge-remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    git(
        &repository,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repository, &["push", "-u", "origin", "main"]);
    let remote_head = git(&remote, &["rev-parse", "refs/heads/main"]);

    for database in [
        "index.sqlite",
        "engineering.sqlite",
        "repository-registry.sqlite",
    ] {
        fs::write(
            harness.root.join("state").join(database),
            format!("seeded-{database}"),
        )
        .unwrap();
    }
    for sidecar in runtime_files(&harness.root).into_iter().skip(1) {
        fs::write(&sidecar, format!("seeded-{}", sidecar.display())).unwrap();
    }
    seed_legacy_capture_state(&harness.root);
    for (directory, file) in [
        ("pending", "batch.json"),
        ("pending-aside", "aside.json"),
        ("authorized-session-scopes", "scope.json"),
    ] {
        let directory = harness.root.join("state").join(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(file),
            format!("seeded-{}", directory.display()),
        )
        .unwrap();
    }
    fs::write(
        harness.root.join("logs/reset-sentinel.log"),
        "preserve logs",
    )
    .unwrap();

    let preserved_paths = [
        harness.root.join("bin/current/sctx"),
        harness.root.join("state/install-manifest.json"),
        harness.root.join("logs/reset-sentinel.log"),
        harness.home.join(".cursor/mcp.json"),
        harness.home.join(".cursor/hooks.json"),
        harness.home.join(".codex/config.toml"),
        harness.home.join(".codex/hooks.json"),
        harness.skill_root().join("SKILL.md"),
        harness.skill_root().join("references/workflow.md"),
        harness.skill_root().join("agents/openai.yaml"),
    ];
    let preserved = preserved_paths
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    SeededResetState {
        preserved,
        remote,
        remote_head,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn setup_three_times_is_idempotent_and_preserves_existing_configuration() {
    let harness = Harness::new();
    harness.seed_configs();
    let installer = harness.installer("1.2.3");
    let first = installer.setup(&SetupOptions::default()).unwrap();
    assert!(first.changed);
    let second = installer.setup(&SetupOptions::default()).unwrap();
    let third = installer.setup(&SetupOptions::default()).unwrap();
    assert!(!second.changed);
    assert!(!third.changed);
    assert_eq!(first.skill.status, SkillStatus::Installed);
    assert_eq!(second.skill.status, SkillStatus::Current);
    assert_eq!(third.skill.status, SkillStatus::Current);
    assert_eq!(first.skill.path, harness.skill_root());
    assert_eq!(
        fs::read(harness.skill_root().join("SKILL.md")).unwrap(),
        include_bytes!("../../../skills/shared-context/SKILL.md")
    );
    assert_eq!(
        fs::read(harness.skill_root().join("agents/openai.yaml")).unwrap(),
        include_bytes!("../../../skills/shared-context/agents/openai.yaml")
    );
    assert_eq!(
        fs::read(harness.skill_root().join("references/workflow.md")).unwrap(),
        include_bytes!("../../../skills/shared-context/references/workflow.md")
    );
    for asset in [
        harness.skill_root().join("SKILL.md"),
        harness.skill_root().join("references/workflow.md"),
        harness.skill_root().join("agents/openai.yaml"),
    ] {
        assert_eq!(
            fs::metadata(asset).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }
    assert_eq!(
        manifest_skill_paths(&harness.root.join("state/install-manifest.json")),
        vec![
            harness.skill_root().join("SKILL.md"),
            harness.skill_root().join("agents/openai.yaml"),
            harness.skill_root().join("references/workflow.md"),
        ]
    );

    let cursor_mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.home.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(cursor_mcp["unknownTop"]["中文"], true);
    assert_eq!(cursor_mcp["mcpServers"].as_object().unwrap().len(), 2);
    assert_eq!(
        hook_count(
            &harness.home.join(".cursor/hooks.json"),
            &[
                "sessionStart",
                "beforeSubmitPrompt",
                "postToolUse",
                "preCompact",
                "stop",
                "sessionEnd"
            ]
        ),
        7
    );
    let cursor_hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.home.join(".cursor/hooks.json")).unwrap())
            .unwrap();
    for event in [
        "sessionStart",
        "beforeSubmitPrompt",
        "postToolUse",
        "preCompact",
        "stop",
        "sessionEnd",
    ] {
        let command = cursor_hooks["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|hook| hook["command"].as_str())
            .find(|command| command.contains(" hook --agent cursor "))
            .unwrap();
        assert!(command.ends_with("--agent-version '3.13.10'"));
    }
    assert_eq!(
        hook_count(
            &harness.home.join(".codex/hooks.json"),
            &[
                "SessionStart",
                "UserPromptSubmit",
                "PostToolUse",
                "PreCompact",
                "Stop",
                "SessionEnd"
            ]
        ),
        7
    );
    let codex_hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.home.join(".codex/hooks.json")).unwrap()).unwrap();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "PreCompact",
        "Stop",
        "SessionEnd",
    ] {
        let command = codex_hooks["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|hook| hook["hooks"][0]["command"].as_str())
            .find(|command| command.contains(" hook --agent codex "))
            .unwrap();
        assert!(command.ends_with("--agent-version '0.147.0'"));
    }
    let toml = fs::read_to_string(harness.home.join(".codex/config.toml")).unwrap();
    assert!(toml.contains("# keep this leading comment"));
    assert!(toml.contains("# keep inline"));
    assert!(toml.contains("[mcp_servers.existing]"));
    assert!(toml.contains("[mcp_servers.shared-context]"));
    assert!(harness.root.join("repository/.git").is_dir());
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("1.2.3/arm64")
    );
}

#[test]
fn setup_rebuilds_schema_12_runtime_and_discards_cached_task_and_operation() {
    let harness = Harness::new();
    let installer = harness.installer("1.2.3");
    installer.setup(&SetupOptions::default()).unwrap();
    let locator = seed_runtime_task(&harness.root, "schema-12-rebuild");
    mark_runtime_as_schema_12(&harness.root);
    seed_legacy_capture_state(&harness.root);

    let report = installer.setup(&SetupOptions::default()).unwrap();

    assert!(report.changed);
    assert_runtime_schema_13(&harness.root);
    for path in legacy_capture_paths(&harness.root) {
        assert!(
            !path.exists(),
            "legacy Capture state survived setup: {}",
            path.display()
        );
    }
    assert!(
        TaskRuntime::initialize(&harness.root)
            .unwrap()
            .read_external_session_by_locator(&locator)
            .unwrap()
            .is_none()
    );
}

#[test]
fn setup_rebuilds_schema_11_runtime_and_discards_cached_task_and_capture_state() {
    let harness = Harness::new();
    let installer = harness.installer("1.2.3");
    installer.setup(&SetupOptions::default()).unwrap();
    let locator = seed_runtime_task(&harness.root, "schema-11-rebuild");
    mark_runtime_as_legacy_schema(&harness.root, 11, false);
    seed_legacy_capture_state(&harness.root);

    let report = installer.setup(&SetupOptions::default()).unwrap();

    assert!(report.changed);
    assert_runtime_schema_13(&harness.root);
    for path in legacy_capture_paths(&harness.root) {
        assert!(
            !path.exists(),
            "legacy Capture state survived setup: {}",
            path.display()
        );
    }
    assert!(
        TaskRuntime::initialize(&harness.root)
            .unwrap()
            .read_external_session_by_locator(&locator)
            .unwrap()
            .is_none()
    );
}

#[test]
fn setup_rejects_unknown_or_future_runtime_schemas_without_mutating_state() {
    for version in [10_u32, 14, 999] {
        let harness = Harness::new();
        let installer = harness.installer("1.2.3");
        installer.setup(&SetupOptions::default()).unwrap();
        seed_runtime_task(&harness.root, &format!("schema-{version}-rejected"));
        set_runtime_schema_version(&harness.root, version);
        seed_legacy_capture_state(&harness.root);
        let paths = runtime_files(&harness.root);
        fs::write(&paths[1], []).unwrap();
        fs::write(&paths[2], []).unwrap();
        fs::set_permissions(&paths[0], fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(&paths[1], fs::Permissions::from_mode(0o620)).unwrap();
        fs::set_permissions(&paths[2], fs::Permissions::from_mode(0o600)).unwrap();
        let prior = paths
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    fs::read(path).unwrap(),
                    fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                )
            })
            .collect::<Vec<_>>();
        let [capture_directory, capture_lock, capture_metadata] =
            legacy_capture_paths(&harness.root);
        let legacy_prior = [
            capture_directory.join("cap-legacy.json"),
            capture_lock,
            capture_metadata,
        ]
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        });

        let error = installer.setup(&SetupOptions::default()).unwrap_err();

        assert_eq!(
            error.message(),
            format!("unsupported task runtime schema version {version}; expected 13")
        );
        for (path, bytes, mode) in prior {
            assert_eq!(fs::read(&path).unwrap(), bytes, "{}", path.display());
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                mode,
                "{}",
                path.display()
            );
        }
        assert!(capture_directory.is_dir());
        for (path, bytes) in legacy_prior {
            assert_eq!(fs::read(&path).unwrap(), bytes, "{}", path.display());
        }
        let connection = Connection::open(runtime_database(&harness.root)).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            i64::from(version)
        );
    }
}

#[test]
fn later_setup_failure_restores_schema_12_runtime_and_sidecars_exactly() {
    let harness = Harness::new();
    harness
        .installer("1.2.3")
        .setup(&SetupOptions::default())
        .unwrap();
    seed_runtime_task(&harness.root, "schema-12-rollback");
    mark_runtime_as_schema_12(&harness.root);
    seed_legacy_capture_state(&harness.root);
    let paths = runtime_files(&harness.root);
    fs::write(&paths[1], []).unwrap();
    fs::write(&paths[2], []).unwrap();
    fs::set_permissions(&paths[0], fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&paths[1], fs::Permissions::from_mode(0o620)).unwrap();
    fs::set_permissions(&paths[2], fs::Permissions::from_mode(0o600)).unwrap();
    let prior = paths
        .iter()
        .map(|path| {
            (
                path.clone(),
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            )
        })
        .collect::<Vec<_>>();
    let [capture_directory, capture_lock, capture_metadata] = legacy_capture_paths(&harness.root);
    let capture_record = capture_directory.join("cap-legacy.json");
    let legacy_prior = [capture_record, capture_lock, capture_metadata].map(|path| {
        let bytes = fs::read(&path).unwrap();
        (path, bytes)
    });

    let error = harness
        .installer("1.2.3")
        .with_failure_after(SetupStage::GlobalSkillWritten)
        .setup(&SetupOptions::default())
        .unwrap_err();

    assert!(error.message().contains("injected setup failure"));
    for (path, bytes, mode) in prior {
        assert_eq!(fs::read(&path).unwrap(), bytes, "{}", path.display());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            mode,
            "{}",
            path.display()
        );
    }
    assert!(capture_directory.is_dir());
    for (path, bytes) in legacy_prior {
        assert_eq!(fs::read(&path).unwrap(), bytes, "{}", path.display());
    }
    let connection = Connection::open(runtime_database(&harness.root)).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        12
    );
    assert_eq!(runtime_operation_count(&connection), 1);
}

#[test]
fn idempotent_setup_retains_compatible_schema_13_runtime_data() {
    let harness = Harness::new();
    let installer = harness.installer("1.2.3");
    installer.setup(&SetupOptions::default()).unwrap();
    let locator = seed_runtime_task(&harness.root, "schema-13-retained");

    let report = installer.setup(&SetupOptions::default()).unwrap();

    assert!(!report.changed);
    assert_runtime_schema_13(&harness.root);
    assert!(
        TaskRuntime::initialize(&harness.root)
            .unwrap()
            .read_external_session_by_locator(&locator)
            .unwrap()
            .is_some()
    );
}

#[test]
fn setup_rejects_symlinked_legacy_capture_directory_without_following_it() {
    let harness = Harness::new();
    let installer = harness.installer("1.2.3");
    installer.setup(&SetupOptions::default()).unwrap();
    let capture = harness.root.join("state/capture");
    let outside = harness.home.join("outside-legacy-capture");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("preserve.txt"), b"preserve").unwrap();
    symlink(&outside, &capture).unwrap();

    let error = installer.setup(&SetupOptions::default()).unwrap_err();

    assert!(error.message().contains("non-symlink directory"));
    assert!(capture.is_symlink());
    assert_eq!(fs::read(outside.join("preserve.txt")).unwrap(), b"preserve");
}

#[test]
fn setup_refreshes_unchanged_managed_hooks_when_verified_agent_versions_change() {
    let harness = Harness::new();
    harness
        .installer("1.2.3")
        .setup(&SetupOptions::default())
        .unwrap();
    let refreshed = Installer::new(
        harness.context("1.2.3"),
        Arc::new(FakeHost {
            cursor_version: Some("4.0.0"),
            codex_version: Some("0.149.1"),
            ..FakeHost::default()
        }),
    )
    .setup(&SetupOptions::default())
    .unwrap();
    assert!(refreshed.changed);

    let cursor = fs::read_to_string(harness.home.join(".cursor/hooks.json")).unwrap();
    assert_eq!(cursor.matches("--agent-version '4.0.0'").count(), 6);
    assert!(!cursor.contains("--agent-version '3.13.10'"));
    let codex = fs::read_to_string(harness.home.join(".codex/hooks.json")).unwrap();
    assert_eq!(codex.matches("--agent-version '0.149.1'").count(), 6);
    assert!(!codex.contains("--agent-version '0.147.0'"));

    let unavailable = Installer::new(
        harness.context("1.2.3"),
        Arc::new(FakeHost {
            cursor_version: None,
            codex_version: None,
            ..FakeHost::default()
        }),
    )
    .setup(&SetupOptions::default())
    .unwrap();
    assert!(!unavailable.changed);
    assert_eq!(
        fs::read_to_string(harness.home.join(".cursor/hooks.json")).unwrap(),
        cursor
    );
    assert_eq!(
        fs::read_to_string(harness.home.join(".codex/hooks.json")).unwrap(),
        codex
    );
}

#[test]
fn installer_mutations_are_exclusive_and_doctor_reports_active_maintenance() {
    let harness = Harness::new();
    let installer = harness.installer("1.2.3");
    installer.setup(&SetupOptions::default()).unwrap();
    let maintenance = MaintenanceLock::open_or_create(&harness.root).unwrap();

    let shared = maintenance.try_shared().unwrap();
    assert_eq!(
        installer
            .setup(&SetupOptions::default())
            .unwrap_err()
            .kind(),
        sctx_installer::ErrorKind::MaintenanceBusy
    );
    assert_eq!(
        installer.uninstall().unwrap_err().kind(),
        sctx_installer::ErrorKind::MaintenanceBusy
    );
    assert_eq!(
        installer
            .reset_data(DataResetOptions {
                confirmed: true,
                dry_run: false,
            })
            .unwrap_err()
            .kind(),
        sctx_installer::ErrorKind::MaintenanceBusy
    );
    assert_eq!(
        installer.sync_knowledge().unwrap_err().kind(),
        sctx_installer::ErrorKind::MaintenanceBusy
    );
    drop(shared);

    let exclusive = maintenance.try_exclusive().unwrap();
    let doctor = installer.doctor();
    assert!(!doctor.healthy);
    assert_eq!(doctor.checks.len(), 1);
    assert_eq!(doctor.checks[0].name, "maintenance");
    drop(exclusive);

    assert!(!installer.setup(&SetupOptions::default()).unwrap().changed);
}

#[test]
#[allow(clippy::too_many_lines)]
fn data_reset_dry_run_is_read_only_and_confirmed_reset_preserves_installation() {
    let harness = Harness::new();
    let seeded = seed_reset_state(&harness);
    let installer = harness.installer("1.2.3");
    let repository = harness.root.join("repository");
    let head_before = git(&repository, &["rev-parse", "HEAD"]);
    let config_before = fs::read(harness.root.join("config.toml")).unwrap();
    let reset_backups_before = fs::read_dir(harness.root.join("backups"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("reset-"))
        .count();

    assert!(
        installer
            .reset_data(DataResetOptions::default())
            .unwrap_err()
            .message()
            .contains("--yes")
    );
    let dry_run = installer
        .reset_data(DataResetOptions {
            confirmed: false,
            dry_run: true,
        })
        .unwrap();
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.repository_count_cleared, 2);
    assert_eq!(dry_run.repository_group_count_cleared, 1);
    assert!(dry_run.backup.is_none());
    assert!(!dry_run.remote_detached);
    assert!(!dry_run.remote_mutated);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        fs::read(harness.root.join("config.toml")).unwrap(),
        config_before
    );
    assert_eq!(
        fs::read_dir(harness.root.join("backups"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("reset-"))
            .count(),
        reset_backups_before
    );

    let report = installer
        .reset_data(DataResetOptions {
            confirmed: true,
            dry_run: false,
        })
        .unwrap();
    assert!(!report.dry_run);
    assert_eq!(report.repository_count_cleared, 2);
    assert_eq!(report.repository_group_count_cleared, 1);
    assert!(report.remote_detached);
    assert!(!report.remote_mutated);
    let backup = report.backup.as_ref().unwrap();
    assert!(backup.join("journal.json").is_file());
    assert!(backup.join("old/repository/.git").is_dir());
    assert!(
        fs::read_to_string(backup.join("old/config.toml"))
            .unwrap()
            .contains("FE")
    );
    assert_eq!(
        git(&seeded.remote, &["rev-parse", "refs/heads/main"]),
        seeded.remote_head
    );
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
    assert!(git(&repository, &["remote"]).is_empty());
    assert!(
        UserConfigStore::open_existing(&harness.root)
            .unwrap()
            .repository_catalog()
            .unwrap()
            .repositories
            .is_empty()
    );
    assert!(
        ProjectionIndex::new(&repository, harness.root.join("state"))
            .quick_check()
            .unwrap()
            .healthy
    );
    TaskRuntime::initialize(harness.root.clone()).unwrap();
    assert_runtime_schema_13(&harness.root);
    for path in legacy_capture_paths(&harness.root) {
        assert!(
            !path.exists(),
            "legacy Capture state survived reset: {}",
            path.display()
        );
    }
    assert!(
        RepositoryRegistry::initialize(harness.root.clone())
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
    assert!(
        EngineeringProjectionStore::initialize(harness.root.clone())
            .unwrap()
            .read_projection()
            .unwrap()
            .is_none()
    );
    for directory in ["pending", "pending-aside", "authorized-session-scopes"] {
        assert!(
            fs::read_dir(harness.root.join("state").join(directory))
                .unwrap()
                .next()
                .is_none(),
            "{directory} is not empty"
        );
    }
    for (path, expected) in &seeded.preserved {
        assert_eq!(&fs::read(path).unwrap(), expected, "{}", path.display());
    }
    assert!(installer.doctor().healthy);

    let repeated = installer
        .reset_data(DataResetOptions {
            confirmed: true,
            dry_run: false,
        })
        .unwrap();
    assert_eq!(repeated.repository_count_cleared, 0);
    assert_eq!(repeated.repository_group_count_cleared, 0);
    assert_ne!(repeated.backup, report.backup);
    assert!(installer.doctor().healthy);
}

#[test]
fn every_reset_crash_seam_blocks_business_and_recovers_on_retry() {
    for stage in [
        ResetStage::Staged,
        ResetStage::FirstOriginalMoved,
        ResetStage::FirstReplacementInstalled,
        ResetStage::Swapped,
        ResetStage::SmokeTested,
    ] {
        let harness = Harness::new();
        let seeded = seed_reset_state(&harness);
        let crashing = harness.installer("1.2.3").with_reset_crash_after(stage);
        let error = crashing
            .reset_data(DataResetOptions {
                confirmed: true,
                dry_run: false,
            })
            .unwrap_err();
        assert!(
            error.message().contains("injected reset crash"),
            "{stage:?}"
        );
        assert!(harness.root.join("state/reset-journal.json").is_file());
        assert_eq!(
            MaintenanceLock::open_or_create(&harness.root)
                .unwrap()
                .try_shared()
                .unwrap_err()
                .kind(),
            sctx_installer::ErrorKind::MaintenanceBusy
        );

        let recovered = harness
            .installer("1.2.3")
            .reset_data(DataResetOptions {
                confirmed: true,
                dry_run: false,
            })
            .unwrap();
        assert!(!harness.root.join("state/reset-journal.json").exists());
        assert!(
            recovered
                .backup
                .as_ref()
                .unwrap()
                .join("journal.json")
                .is_file()
        );
        assert_eq!(
            git(&seeded.remote, &["rev-parse", "refs/heads/main"]),
            seeded.remote_head
        );
        assert!(harness.installer("1.2.3").doctor().healthy, "{stage:?}");
    }
}

#[test]
fn setup_recovers_an_incomplete_reset_before_reapplying_installation() {
    let harness = Harness::new();
    let seeded = seed_reset_state(&harness);
    for database in [
        "index.sqlite",
        "runtime.sqlite",
        "engineering.sqlite",
        "repository-registry.sqlite",
    ] {
        for suffix in ["", "-wal", "-shm"] {
            let path = harness
                .root
                .join("state")
                .join(format!("{database}{suffix}"));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove seeded database: {error}"),
            }
        }
    }
    ProjectionIndex::new(harness.root.join("repository"), harness.root.join("state"))
        .synchronize()
        .unwrap();
    TaskRuntime::initialize(harness.root.clone()).unwrap();
    sctx_mcp::sync_repository_catalog_at_root(&harness.root).unwrap();
    EngineeringProjectionStore::initialize(harness.root.clone()).unwrap();
    let crashing = harness
        .installer("1.2.3")
        .with_reset_crash_after(ResetStage::Swapped);
    assert!(
        crashing
            .reset_data(DataResetOptions {
                confirmed: true,
                dry_run: false,
            })
            .is_err()
    );
    assert!(harness.root.join("state/reset-journal.json").is_file());

    harness
        .installer("1.2.3")
        .setup(&SetupOptions::default())
        .unwrap();
    assert!(!harness.root.join("state/reset-journal.json").exists());
    let catalog = UserConfigStore::open_existing(&harness.root)
        .unwrap()
        .repository_catalog()
        .unwrap();
    assert_eq!(catalog.repositories.len(), 2);
    assert_eq!(catalog.repository_groups.len(), 1);
    assert_eq!(git(&harness.root.join("repository"), &["remote"]), "origin");
    assert_eq!(
        git(&seeded.remote, &["rev-parse", "refs/heads/main"]),
        seeded.remote_head
    );
}

#[test]
fn reset_rejects_symlinked_targets_without_touching_the_target() {
    let harness = Harness::new();
    harness
        .installer("1.2.3")
        .setup(&SetupOptions::default())
        .unwrap();
    let capture = harness.root.join("state/capture");
    if capture.exists() {
        fs::remove_dir_all(&capture).unwrap();
    }
    let outside = harness.home.join("outside-capture");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("preserve.txt"), "preserve").unwrap();
    symlink(&outside, &capture).unwrap();

    let error = harness
        .installer("1.2.3")
        .reset_data(DataResetOptions {
            confirmed: true,
            dry_run: false,
        })
        .unwrap_err();
    assert!(error.message().contains("must not be a symlink"));
    assert_eq!(
        fs::read_to_string(outside.join("preserve.txt")).unwrap(),
        "preserve"
    );
    assert!(!harness.root.join("state/reset-journal.json").exists());
}

#[test]
fn cursor_and_codex_share_one_global_skill_installation() {
    let harness = Harness::new();
    let cursor = SetupOptions {
        agents: [Agent::Cursor].into_iter().collect(),
        ..SetupOptions::default()
    };
    let codex = SetupOptions {
        agents: [Agent::Codex].into_iter().collect(),
        ..SetupOptions::default()
    };
    let first = harness.installer("1.0.0").setup(&cursor).unwrap();
    let second = harness.installer("1.0.0").setup(&codex).unwrap();

    assert_eq!(first.skill.status, SkillStatus::Installed);
    assert_eq!(second.skill.status, SkillStatus::Current);
    assert_eq!(first.skill.path, second.skill.path);
    assert!(harness.skill_root().join("SKILL.md").is_file());
    assert!(harness.skill_root().join("agents/openai.yaml").is_file());
    assert!(
        harness
            .skill_root()
            .join("references/workflow.md")
            .is_file()
    );
}

#[test]
fn setup_preserves_and_does_not_claim_an_external_same_name_skill() {
    let harness = Harness::new();
    let skill = harness.skill_root();
    fs::create_dir_all(skill.join("agents")).unwrap();
    fs::write(skill.join("SKILL.md"), b"user skill\n").unwrap();
    fs::write(skill.join("agents/openai.yaml"), b"user: metadata\n").unwrap();

    let installer = harness.installer("1.0.0");
    let report = installer.setup(&SetupOptions::default()).unwrap();
    assert_eq!(report.skill.status, SkillStatus::Conflict);
    assert!(report.notices.iter().any(|notice| {
        notice.contains("user-owned global Agent Skill") && notice.contains("did not overwrite")
    }));
    assert_eq!(fs::read(skill.join("SKILL.md")).unwrap(), b"user skill\n");
    assert_eq!(
        fs::read(skill.join("agents/openai.yaml")).unwrap(),
        b"user: metadata\n"
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(harness.root.join("state/install-manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["skills"].as_array().unwrap().len(), 0);

    let uninstall = installer.uninstall().unwrap();
    assert!(skill.join("SKILL.md").is_file());
    assert!(skill.join("agents/openai.yaml").is_file());
    assert!(!skill.join("references/workflow.md").exists());
    assert!(!uninstall.removed.contains(&skill));
}

#[test]
fn every_setup_write_seam_restores_exact_agent_bytes_and_permissions() {
    let stages = [
        SetupStage::RuntimeInstalled,
        SetupStage::CurrentSwitched,
        SetupStage::RepositoryInitialized,
        SetupStage::IndexInitialized,
        SetupStage::CursorMcpWritten,
        SetupStage::CursorHooksWritten,
        SetupStage::CodexMcpWritten,
        SetupStage::CodexHooksWritten,
        SetupStage::GlobalSkillGateWritten,
        SetupStage::GlobalSkillWorkflowWritten,
        SetupStage::GlobalSkillMetadataWritten,
        SetupStage::GlobalSkillWritten,
        SetupStage::ManifestWritten,
        SetupStage::SmokeTested,
    ];
    for stage in stages {
        let harness = Harness::new();
        let originals = harness.seed_configs();
        let parent_modes =
            [harness.home.join(".cursor"), harness.home.join(".codex")].map(|path| {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
                (path, mode)
            });
        let installer = harness.installer("1.2.3").with_failure_after(stage);
        let error = installer.setup(&SetupOptions::default()).unwrap_err();
        assert!(error.to_string().contains("injected setup failure"));
        for (path, bytes, mode) in &originals {
            assert_eq!(fs::read(path).unwrap(), *bytes, "bytes at {stage:?}");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                *mode,
                "mode at {stage:?}"
            );
        }
        for (path, mode) in parent_modes {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                mode,
                "parent mode at {stage:?}"
            );
        }
        assert!(
            !harness.root.join("bin/current").exists(),
            "current at {stage:?}"
        );
        assert!(
            !harness.root.join("bin/1.2.3/arm64/sctx").exists(),
            "runtime at {stage:?}"
        );
        assert!(!harness.root.join("state/install-manifest.json").exists());
        assert!(!harness.skill_root().exists(), "skill root at {stage:?}");
        assert!(
            !harness.home.join(".agents").exists(),
            "new global Skill parents at {stage:?}"
        );
        if stage >= SetupStage::RepositoryInitialized {
            assert!(harness.root.join("repository/.git").is_dir());
        }
    }
}

#[test]
fn failed_skill_install_preserves_existing_parent_directories_and_permissions() {
    let harness = Harness::new();
    let agents = harness.home.join(".agents");
    let skills = agents.join("skills");
    fs::create_dir_all(&skills).unwrap();
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o751)).unwrap();
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o711)).unwrap();

    let result = harness
        .installer("1.0.0")
        .with_failure_after(SetupStage::GlobalSkillWritten)
        .setup(&SetupOptions::default());
    assert!(result.is_err());
    assert_eq!(
        fs::metadata(&agents).unwrap().permissions().mode() & 0o7777,
        0o751
    );
    assert_eq!(
        fs::metadata(&skills).unwrap().permissions().mode() & 0o7777,
        0o711
    );
    assert!(!harness.skill_root().exists());
}

#[test]
fn preflight_rejects_platform_arch_git_signature_and_space_independently() {
    let harness = Harness::new();
    let cases = [
        FakeHost {
            platform: "linux",
            ..FakeHost::default()
        },
        FakeHost {
            architecture: None,
            ..FakeHost::default()
        },
        FakeHost {
            git_ok: false,
            ..FakeHost::default()
        },
        FakeHost {
            signature_ok: false,
            ..FakeHost::default()
        },
        FakeHost {
            space: 1,
            ..FakeHost::default()
        },
    ];
    for host in cases {
        let installer = Installer::new(harness.context("1.0.0"), Arc::new(host));
        assert!(installer.setup(&SetupOptions::default()).is_err());
        assert!(!harness.root.join("repository").exists());
    }
}

#[test]
fn x86_64_uses_the_distinct_x64_runtime_directory() {
    let harness = Harness::new();
    let host = FakeHost {
        architecture: Some(Architecture::X86_64),
        ..FakeHost::default()
    };
    Installer::new(harness.context("1.0.0"), Arc::new(host))
        .setup(&SetupOptions::default())
        .unwrap();
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("1.0.0/x64")
    );
    assert!(harness.root.join("bin/1.0.0/x64/sctx").is_file());
}

#[test]
fn upgrade_switches_atomically_and_failed_upgrade_restores_previous_runtime() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();
    let report = harness
        .installer("2.0.0")
        .upgrade(&SetupOptions::default())
        .unwrap();
    assert!(report.changed);
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("2.0.0/arm64")
    );
    assert_eq!(
        fs::read(harness.root.join("bin/current/sctx")).unwrap(),
        b"signed-runtime-v2"
    );
    assert!(harness.root.join("bin/1.0.0/arm64/sctx").is_file());

    fs::write(&harness.runtime, b"signed-runtime-v3").unwrap();
    let failed = harness
        .installer("3.0.0")
        .with_failure_after(SetupStage::CurrentSwitched)
        .upgrade(&SetupOptions::default());
    assert!(failed.is_err());
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("2.0.0/arm64")
    );
    assert!(!harness.root.join("bin/3.0.0/arm64/sctx").exists());
}

#[test]
fn upgrade_and_uninstall_preserve_only_the_user_modified_skill_file() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let skill_md = harness.skill_root().join("SKILL.md");
    let openai_yaml = harness.skill_root().join("agents/openai.yaml");
    let workflow = harness.skill_root().join("references/workflow.md");
    fs::write(&skill_md, b"user modified skill\n").unwrap();
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

    let installer = harness.installer("2.0.0");
    let upgrade = installer.upgrade(&SetupOptions::default()).unwrap();
    assert_eq!(upgrade.skill.status, SkillStatus::Modified);
    assert_eq!(fs::read(&skill_md).unwrap(), b"user modified skill\n");
    assert!(openai_yaml.is_file());
    assert!(workflow.is_file());
    assert!(
        upgrade
            .notices
            .iter()
            .any(|notice| notice.contains("user-modified global Agent Skill file"))
    );

    let uninstall = installer.uninstall().unwrap();
    assert!(skill_md.is_file());
    assert!(!openai_yaml.exists());
    assert!(!workflow.exists());
    assert!(harness.skill_root().is_dir());
    assert!(uninstall.preserved.contains(&skill_md));
    assert!(uninstall.removed.contains(&openai_yaml));
    assert!(
        uninstall
            .warnings
            .iter()
            .any(|warning| warning.contains("user-modified global Agent Skill file"))
    );
}

#[test]
fn uninstall_never_follows_a_replaced_skill_parent_symlink() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    let skill = harness.skill_root();
    let displaced = harness.home.join("displaced-managed-skill");
    let outside = harness.home.join("outside-symlink-target");
    fs::rename(&skill, &displaced).unwrap();
    fs::create_dir_all(outside.join("agents")).unwrap();
    fs::write(
        outside.join("SKILL.md"),
        include_bytes!("../../../skills/shared-context/SKILL.md"),
    )
    .unwrap();
    fs::write(
        outside.join("agents/openai.yaml"),
        include_bytes!("../../../skills/shared-context/agents/openai.yaml"),
    )
    .unwrap();
    fs::create_dir_all(outside.join("references")).unwrap();
    symlink(&outside, &skill).unwrap();

    let report = installer.uninstall().unwrap();
    assert!(skill.is_symlink());
    assert!(outside.join("SKILL.md").is_file());
    assert!(outside.join("agents/openai.yaml").is_file());
    assert!(outside.join("references").is_dir());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("parent is not a non-symlink directory"))
    );
}

#[test]
fn upgrade_replaces_an_unchanged_managed_skill_from_an_older_build() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let skill_md = harness.skill_root().join("SKILL.md");
    let workflow = harness.skill_root().join("references/workflow.md");
    let openai_yaml = harness.skill_root().join("agents/openai.yaml");
    let manifest = harness.root.join("state/install-manifest.json");
    replace_owned_skill_bytes(&skill_md, b"older managed skill\n", &manifest);
    replace_owned_skill_bytes(&openai_yaml, b"older: metadata\n", &manifest);
    remove_owned_skill(&workflow, &manifest);
    fs::remove_file(&workflow).unwrap();
    fs::remove_dir(workflow.parent().unwrap()).unwrap();
    fs::set_permissions(&skill_md, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&openai_yaml, fs::Permissions::from_mode(0o604)).unwrap();
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

    let report = harness
        .installer("2.0.0")
        .upgrade(&SetupOptions::default())
        .unwrap();
    assert_eq!(report.skill.status, SkillStatus::Installed);
    assert_eq!(
        fs::read(&skill_md).unwrap(),
        include_bytes!("../../../skills/shared-context/SKILL.md")
    );
    assert_eq!(
        fs::metadata(&skill_md).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert_eq!(
        fs::read(&workflow).unwrap(),
        include_bytes!("../../../skills/shared-context/references/workflow.md")
    );
    assert_eq!(
        fs::metadata(&workflow).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    assert_eq!(
        fs::read(&openai_yaml).unwrap(),
        include_bytes!("../../../skills/shared-context/agents/openai.yaml")
    );
    assert_eq!(
        fs::metadata(&openai_yaml).unwrap().permissions().mode() & 0o7777,
        0o604
    );
    assert_eq!(manifest_skill_paths(&manifest).len(), 3);
}

#[test]
fn hostile_unowned_reference_blocks_the_complete_managed_bundle_upgrade() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let skill_md = harness.skill_root().join("SKILL.md");
    let workflow = harness.skill_root().join("references/workflow.md");
    let openai_yaml = harness.skill_root().join("agents/openai.yaml");
    let manifest = harness.root.join("state/install-manifest.json");
    replace_owned_skill_bytes(&skill_md, b"older managed gate\n", &manifest);
    replace_owned_skill_bytes(&openai_yaml, b"older: managed metadata\n", &manifest);
    remove_owned_skill(&workflow, &manifest);
    fs::write(&workflow, b"hostile user-owned workflow\n").unwrap();
    let before_paths = manifest_skill_paths(&manifest);
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

    let report = harness
        .installer("2.0.0")
        .upgrade(&SetupOptions::default())
        .unwrap();

    assert_eq!(report.skill.status, SkillStatus::Conflict);
    assert_eq!(fs::read(&skill_md).unwrap(), b"older managed gate\n");
    assert_eq!(
        fs::read(&openai_yaml).unwrap(),
        b"older: managed metadata\n"
    );
    assert_eq!(
        fs::read(&workflow).unwrap(),
        b"hostile user-owned workflow\n"
    );
    assert_eq!(manifest_skill_paths(&manifest), before_paths);
    assert!(report.notices.iter().any(|notice| {
        notice.contains("user-owned global Agent Skill file")
            && notice.contains("references/workflow.md")
    }));
}

#[test]
fn modified_owned_reference_is_preserved_by_setup_upgrade_and_uninstall() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    let skill_md = harness.skill_root().join("SKILL.md");
    let workflow = harness.skill_root().join("references/workflow.md");
    let gate_before = fs::read(&skill_md).unwrap();
    fs::write(&workflow, b"user-modified workflow\n").unwrap();

    let repeat = installer.setup(&SetupOptions::default()).unwrap();
    assert_eq!(repeat.skill.status, SkillStatus::Modified);
    assert_eq!(fs::read(&workflow).unwrap(), b"user-modified workflow\n");
    assert_eq!(fs::read(&skill_md).unwrap(), gate_before);

    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();
    let upgrade = harness
        .installer("2.0.0")
        .upgrade(&SetupOptions::default())
        .unwrap();
    assert_eq!(upgrade.skill.status, SkillStatus::Modified);
    assert_eq!(fs::read(&workflow).unwrap(), b"user-modified workflow\n");
    assert_eq!(fs::read(&skill_md).unwrap(), gate_before);

    let uninstall = harness.installer("2.0.0").uninstall().unwrap();
    assert!(workflow.is_file());
    assert_eq!(fs::read(&workflow).unwrap(), b"user-modified workflow\n");
    assert!(uninstall.preserved.contains(&workflow));
    assert!(!skill_md.exists());
}

#[test]
fn failed_upgrade_restores_managed_skill_bytes_and_permissions() {
    for stage in [
        SetupStage::RuntimeInstalled,
        SetupStage::CurrentSwitched,
        SetupStage::RepositoryInitialized,
        SetupStage::IndexInitialized,
        SetupStage::CursorMcpWritten,
        SetupStage::CursorHooksWritten,
        SetupStage::CodexMcpWritten,
        SetupStage::CodexHooksWritten,
        SetupStage::GlobalSkillGateWritten,
        SetupStage::GlobalSkillWorkflowWritten,
        SetupStage::GlobalSkillMetadataWritten,
        SetupStage::GlobalSkillWritten,
        SetupStage::ManifestWritten,
        SetupStage::SmokeTested,
    ] {
        let harness = Harness::new();
        harness
            .installer("1.0.0")
            .setup(&SetupOptions::default())
            .unwrap();
        let managed_configs = [
            harness.home.join(".cursor/mcp.json"),
            harness.home.join(".cursor/hooks.json"),
            harness.home.join(".codex/config.toml"),
            harness.home.join(".codex/hooks.json"),
        ]
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            (path, bytes, mode)
        });
        let manifest = harness.root.join("state/install-manifest.json");
        let originals = [
            (
                harness.skill_root().join("SKILL.md"),
                b"old skill\n".as_slice(),
                0o640,
            ),
            (
                harness.skill_root().join("references/workflow.md"),
                b"old workflow\n".as_slice(),
                0o644,
            ),
            (
                harness.skill_root().join("agents/openai.yaml"),
                b"old: metadata\n".as_slice(),
                0o604,
            ),
        ];
        for (path, bytes, mode) in &originals {
            replace_owned_skill_bytes(path, bytes, &manifest);
            fs::set_permissions(path, fs::Permissions::from_mode(*mode)).unwrap();
        }
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o640)).unwrap();
        let original_manifest = fs::read(&manifest).unwrap();
        fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

        let result = harness
            .installer("2.0.0")
            .with_failure_after(stage)
            .upgrade(&SetupOptions::default());
        assert!(result.is_err());
        for (path, bytes, mode) in &managed_configs {
            assert_eq!(fs::read(path).unwrap(), *bytes, "config bytes at {stage:?}");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                *mode,
                "config mode at {stage:?}"
            );
        }
        for (path, bytes, mode) in &originals {
            assert_eq!(fs::read(path).unwrap(), *bytes, "bytes at {stage:?}");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                *mode,
                "mode at {stage:?}"
            );
        }
        assert_eq!(fs::read(&manifest).unwrap(), original_manifest);
        assert_eq!(
            fs::metadata(&manifest).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert_eq!(
            fs::read_link(harness.root.join("bin/current")).unwrap(),
            PathBuf::from("1.0.0/arm64")
        );
        assert!(!harness.root.join("bin/2.0.0/arm64/sctx").exists());
    }
}

#[test]
fn doctor_reports_codex_trust_as_action_required() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let report = harness.installer("1.0.0").doctor();
    assert!(report.healthy);
    let codex = report
        .capabilities
        .iter()
        .find(|capability| format!("{:?}", capability.agent) == "Codex")
        .unwrap();
    assert!(codex.diagnostic.starts_with("ACTION REQUIRED:"));
    assert!(report.checks.iter().any(|check| {
        check.name == "codex_hook_trust.codex" && format!("{:?}", check.status) == "ActionRequired"
    }));
    assert!(
        report.checks.iter().any(|check| {
            check.name == "global_skill.skill_md" && check.status == CheckStatus::Ok
        })
    );
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.openai_yaml" && check.status == CheckStatus::Ok
    }));
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.workflow_reference" && check.status == CheckStatus::Ok
    }));
}

#[test]
fn doctor_distinguishes_modified_and_missing_managed_skill_files() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    fs::write(harness.skill_root().join("SKILL.md"), b"modified\n").unwrap();
    fs::write(
        harness.skill_root().join("references/workflow.md"),
        b"modified workflow\n",
    )
    .unwrap();
    fs::remove_file(harness.skill_root().join("agents/openai.yaml")).unwrap();

    let report = installer.doctor();
    assert!(!report.healthy);
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.skill_md" && check.status == CheckStatus::ActionRequired
    }));
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.openai_yaml" && check.status == CheckStatus::Error
    }));
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.workflow_reference"
            && check.status == CheckStatus::ActionRequired
    }));
}

#[test]
fn setup_and_doctor_restore_registry_from_catalog_and_report_invalid_config() {
    let harness = Harness::new();
    let checkout = init_catalog_repo(&harness.home.join("configured checkout"));
    let configured = UserConfigStore::initialize(&harness.root)
        .unwrap()
        .add_repository(sctx_domain::RepositoryId::new(), &[checkout])
        .unwrap();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    assert_eq!(
        RepositoryRegistry::initialize(&harness.root)
            .unwrap()
            .list()
            .unwrap()[0]
            .identity
            .repository_id,
        configured.repository.repository_id,
        "setup must synchronize the pre-existing Catalog"
    );
    let database = harness.root.join("state/repository-registry.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", database.display()));
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove Registry database: {error}"),
        }
    }

    let report = installer.doctor();
    assert!(
        report.checks.iter().any(|check| {
            check.name == "repository_registry" && check.status == CheckStatus::Ok
        })
    );
    assert_eq!(
        RepositoryRegistry::initialize(&harness.root)
            .unwrap()
            .list()
            .unwrap()[0]
            .identity
            .repository_id,
        configured.repository.repository_id
    );

    let config_path = harness.root.join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[[repositories]]\nid = \"FE/mobile\"\npaths = []\n");
    fs::write(config_path, config).unwrap();
    let invalid = installer.doctor();
    assert!(!invalid.healthy);
    assert!(
        invalid.checks.iter().any(|check| {
            check.name == "repository_catalog" && check.status == CheckStatus::Error
        })
    );
}

#[test]
fn uninstall_removes_only_exact_owned_entries_and_retains_repository() {
    let harness = Harness::new();
    let originals = harness.seed_configs();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    seed_legacy_capture_state(&harness.root);
    let runtime_paths = runtime_files(&harness.root);
    fs::write(&runtime_paths[1], b"legacy wal").unwrap();
    fs::write(&runtime_paths[2], b"legacy shm").unwrap();

    let cursor_hooks_path = harness.home.join(".cursor/hooks.json");
    let mut cursor_hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&cursor_hooks_path).unwrap()).unwrap();
    cursor_hooks["hooks"]["sessionStart"][0]["changed_by_user"] = true.into();
    cursor_hooks["hooks"]["userFuture"] = serde_json::json!([{"command": "keep user future hook"}]);
    fs::write(
        &cursor_hooks_path,
        serde_json::to_vec_pretty(&cursor_hooks).unwrap(),
    )
    .unwrap();

    let report = installer.uninstall().unwrap();
    assert!(report.repository_retained);
    assert!(harness.root.join("repository/.git").is_dir());
    assert!(!harness.root.join("bin").exists());
    assert!(!harness.skill_root().exists());
    for path in runtime_paths
        .into_iter()
        .chain(legacy_capture_paths(&harness.root))
    {
        assert!(!path.exists(), "uninstall retained {}", path.display());
        assert!(report.removed.contains(&path));
    }
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("modified after setup"))
    );

    let cursor_hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&cursor_hooks_path).unwrap()).unwrap();
    assert_eq!(
        cursor_hooks["hooks"]["sessionStart"][0]["changed_by_user"],
        true
    );
    assert_eq!(
        cursor_hooks["hooks"]["userFuture"][0]["command"],
        "keep user future hook"
    );
    assert!(cursor_hooks["hooks"].get("preCompact").is_none());

    let cursor_mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.home.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert!(cursor_mcp["mcpServers"].get("shared-context").is_none());
    assert_eq!(cursor_mcp["mcpServers"]["existing"]["command"], "keep me");

    let codex = fs::read_to_string(harness.home.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("# keep this leading comment"));
    assert!(codex.contains("[mcp_servers.existing]"));
    assert!(!codex.contains("mcp_servers.shared-context"));
    for (path, bytes, mode) in originals
        .iter()
        .filter(|(path, _, _)| path != &cursor_hooks_path)
    {
        assert_eq!(
            fs::read(path).unwrap(),
            *bytes,
            "baseline bytes for {}",
            path.display()
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            *mode,
            "baseline mode for {}",
            path.display()
        );
    }
}

#[test]
fn knowledge_deletion_requires_path_and_phrase_as_two_confirmations() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    let repository = harness.root.join("repository");
    assert!(
        installer
            .delete_knowledge(
                &harness.root.join("wrong"),
                "DELETE-SHARED-CONTEXT-KNOWLEDGE"
            )
            .is_err()
    );
    assert!(installer.delete_knowledge(&repository, "DELETE").is_err());
    assert!(repository.exists());
    assert_eq!(
        installer
            .delete_knowledge(&repository, "DELETE-SHARED-CONTEXT-KNOWLEDGE")
            .unwrap(),
        repository
    );
    assert!(!repository.exists());
}

#[test]
fn remote_setup_clones_once_to_stable_work_branch_without_mutating_default() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "team");
    let remote_main = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);
    let options = remote_setup_options(&fixture.remote);

    let first = harness.installer("1.0.0").setup(&options).unwrap();
    assert!(first.changed);
    assert_eq!(first.knowledge_store.source, "remote");
    assert_eq!(
        first.knowledge_store.remote_type,
        Some(KnowledgeRemoteType::Local)
    );
    assert_eq!(first.knowledge_store.default_branch, "main");
    let work_branch = first.knowledge_store.work_branch.as_deref().unwrap();
    assert_eq!(
        work_branch,
        format!("shared-context/{}", first.knowledge_store.installation_id)
    );
    assert_eq!(
        git(
            &harness.root.join("repository"),
            &["symbolic-ref", "--short", "HEAD"]
        ),
        work_branch
    );
    assert_eq!(
        git(
            &harness.root.join("repository"),
            &["rev-parse", "refs/heads/main"]
        ),
        remote_main
    );
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        remote_main
    );
    let projection = ProjectionIndex::for_store(&GitStore::open_existing(&harness.root).unwrap())
        .domain_snapshot()
        .unwrap();
    assert_eq!(projection.projection.spaces.len(), 1);
    let installed_store = GitStore::open_existing(&harness.root).unwrap();
    let first_write = Event::space_created(
        IntentSnapshot {
            title: "First post-Setup write".to_owned(),
            problem: "Remote bootstrap must leave the Writer usable".to_owned(),
            desired_outcome: "The first Event commits without layout repair".to_owned(),
            in_scope: vec!["active pending state".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["append succeeds".to_owned()],
            domain_terms: vec!["KnowledgeStore".to_owned()],
        },
        None,
    )
    .unwrap();
    let appended_head = installed_store
        .append_event(AppendRequest::event(first_write))
        .unwrap()
        .commit_oid;
    assert_eq!(
        git(installed_store.repository(), &["rev-parse", "HEAD"]),
        appended_head
    );
    assert!(harness.root.join("state/pending").is_dir());
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&fixture.remote)
            .args(["show-ref", "--verify", &format!("refs/heads/{work_branch}")])
            .output()
            .unwrap()
            .status
            .success()
    );

    let unavailable_remote = harness.home.join("team-knowledge-unavailable.git");
    fs::rename(&fixture.remote, &unavailable_remote).unwrap();
    let second = harness.installer("1.0.0").setup(&options).unwrap();
    assert!(!second.changed);
    assert_eq!(second.knowledge_store, first.knowledge_store);
    fs::rename(&unavailable_remote, &fixture.remote).unwrap();
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        remote_main
    );
    let manifest = fs::read_to_string(harness.root.join("state/install-manifest.json")).unwrap();
    assert!(!manifest.contains(fixture.remote.to_str().unwrap()));
    assert!(manifest.contains("url_digest"));
    assert!(manifest.contains("installation_id"));
}

#[test]
fn remote_setup_rejects_a_different_url_and_an_existing_local_store() {
    let harness = Harness::new();
    let first = remote_fixture(&harness, "first");
    let second = remote_fixture(&harness, "second");
    harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&first.remote))
        .unwrap();
    let work_head = git(&harness.root.join("repository"), &["rev-parse", "HEAD"]);
    let error = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&second.remote))
        .unwrap_err();
    assert!(error.message().contains("does not match"));
    assert!(!error.message().contains(second.remote.to_str().unwrap()));
    assert_eq!(
        git(&harness.root.join("repository"), &["rev-parse", "HEAD"]),
        work_head
    );

    let local = Harness::new();
    local
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let error = local
        .installer("1.0.0")
        .setup(&remote_setup_options(&second.remote))
        .unwrap_err();
    assert!(error.message().contains("cannot replace"));
    assert!(local.root.join("repository/.git").is_dir());
}

#[test]
fn remote_setup_rolls_back_the_active_clone_after_a_later_failure() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "rollback");
    let remote_main = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);
    let options = remote_setup_options(&fixture.remote);
    let error = harness
        .installer("1.0.0")
        .with_failure_after(SetupStage::RepositoryInitialized)
        .setup(&options)
        .unwrap_err();
    assert!(error.message().contains("injected setup failure"));
    assert!(!harness.root.join("repository").exists());
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        remote_main
    );

    let recovered = harness.installer("1.0.0").setup(&options).unwrap();
    assert_eq!(recovered.knowledge_store.source, "remote");
    assert!(harness.root.join("repository/.git").is_dir());
}

#[test]
fn remote_setup_rejects_corrupt_committed_objects_before_activation() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "corrupt-object");
    let digest = "0".repeat(64);
    let object = fixture.repository.join("objects/sha256/00").join(&digest);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(&object, b"content whose digest is not zero").unwrap();
    git(
        &fixture.repository,
        &["add", "--", object.to_str().unwrap()],
    );
    git(&fixture.repository, &["commit", "-m", "add corrupt object"]);
    git(&fixture.repository, &["push", "origin", "main"]);

    let error = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&fixture.remote))
        .unwrap_err();
    assert!(error.message().contains("object path or digest"));
    assert!(!harness.root.join("repository").exists());
}

#[test]
fn remote_setup_rejects_invalid_events_before_activation() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "corrupt-event");
    let event = fixture.repository.join("events/invalid.json");
    fs::create_dir_all(event.parent().unwrap()).unwrap();
    fs::write(&event, b"{not valid JSON").unwrap();
    git(&fixture.repository, &["add", "--", "events/invalid.json"]);
    git(&fixture.repository, &["commit", "-m", "add invalid event"]);
    git(&fixture.repository, &["push", "origin", "main"]);

    let error = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&fixture.remote))
        .unwrap_err();
    assert!(
        error.message().contains("parse")
            || error.message().contains("JSON")
            || error.message().contains("event")
    );
    assert!(!harness.root.join("repository").exists());
}

#[test]
fn remote_url_validation_rejects_secrets_and_clone_errors_are_redacted() {
    let secret = "https://token-value@example.invalid/team/context.git";
    let error = secret.parse::<KnowledgeStoreUrl>().unwrap_err();
    assert!(error.message().contains("embedded credentials"));
    assert!(!error.message().contains("token-value"));

    let harness = Harness::new();
    let missing = harness.home.join("sensitive-project-name.git");
    let error = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&missing))
        .unwrap_err();
    assert!(error.message().contains("git clone failed"));
    assert!(!error.message().contains("sensitive-project-name"));
    assert!(!harness.root.join("repository").exists());
}

#[test]
fn remote_setup_rejects_an_empty_remote_without_creating_a_default_branch() {
    let harness = Harness::new();
    let remote = harness.home.join("empty-knowledge.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet", "--initial-branch=main"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    let error = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&remote))
        .unwrap_err();
    assert!(
        error.message().contains("HEAD")
            || error.message().contains("default branch")
            || error.message().contains("git exited")
    );
    assert!(!harness.root.join("repository").exists());
    let refs = Command::new("git")
        .arg("-C")
        .arg(&remote)
        .arg("show-ref")
        .output()
        .unwrap();
    assert!(refs.stdout.is_empty());
}

#[test]
fn knowledge_sync_uses_only_the_protected_installation_branch_and_recreates_it() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "protected-sync");
    let setup = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&fixture.remote))
        .unwrap();
    let work_branch = setup.knowledge_store.work_branch.unwrap();

    let source_root = fixture.repository.parent().unwrap();
    let source = GitStore::open_existing(source_root).unwrap();
    append_space(&source, "remote default advance");
    git(&fixture.repository, &["push", "origin", "main"]);
    let default_before = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);

    let seen_refs = fixture.remote.join("hooks/seen-refs");
    let protected_hook = format!(
        "#!/bin/sh\nwhile read old new ref; do\n  printf '%s\\n' \"$ref\" >> {}\n  if [ \"$ref\" = refs/heads/main ]; then exit 1; fi\ndone\nexit 0\n",
        shell_quote(&seen_refs)
    );
    install_remote_hook(&fixture.remote, &protected_hook);

    let local = GitStore::open_existing(&harness.root).unwrap();
    append_space(&local, "local work advance");
    let first = harness.installer("1.0.0").sync_knowledge().unwrap();
    assert_eq!(first.base_branch, "main");
    assert_eq!(first.work_branch, work_branch);
    assert_eq!(first.behind, 0);
    assert!(first.ahead > 0);
    assert!(first.pushed);
    assert!(first.needs_merge);
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        default_before
    );
    let work_ref = format!("refs/heads/{work_branch}");
    let published = git(&fixture.remote, &["rev-parse", &work_ref]);
    assert_eq!(published, git(local.repository(), &["rev-parse", "HEAD"]));

    git(
        &fixture.remote,
        &["update-ref", "refs/heads/main", &published],
    );
    git(&fixture.remote, &["update-ref", "-d", &work_ref]);
    let merged_default = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);
    let recreated = harness.installer("1.0.0").sync_knowledge().unwrap();
    assert!(recreated.pushed);
    assert_eq!(recreated.ahead, 0);
    assert_eq!(recreated.behind, 0);
    assert!(!recreated.needs_merge);
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        merged_default
    );
    assert_eq!(
        git(&fixture.remote, &["rev-parse", &work_ref]),
        merged_default
    );
    let unchanged = harness.installer("1.0.0").sync_knowledge().unwrap();
    assert!(!unchanged.pushed);
    assert_eq!(unchanged.ahead, 0);
    assert_eq!(unchanged.behind, 0);
    assert!(!unchanged.needs_merge);
    assert!(
        fs::read_to_string(seen_refs)
            .unwrap()
            .lines()
            .all(|reference| reference == work_ref)
    );
}

#[test]
fn knowledge_sync_aborts_conflicts_and_restores_the_original_local_head() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "sync-conflict");
    let setup = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&fixture.remote))
        .unwrap();
    let work_branch = setup.knowledge_store.work_branch.unwrap();
    harness.installer("1.0.0").sync_knowledge().unwrap();
    let alternate_root = harness.home.join("conflicting work clone");
    let alternate = clone_work_store(&alternate_root, &fixture.remote, &work_branch);

    let event = Event::space_created(
        IntentSnapshot {
            title: "same Event different encoding".to_owned(),
            problem: "two writers add the same path".to_owned(),
            desired_outcome: "sync aborts instead of choosing bytes".to_owned(),
            in_scope: vec!["merge conflict".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["original local HEAD is restored".to_owned()],
            domain_terms: Vec::new(),
        },
        None,
    )
    .unwrap();
    let local = GitStore::open_existing(&harness.root).unwrap();
    let original_local = commit_event_variant(local.repository(), &event, true);
    commit_event_variant(alternate.repository(), &event, false);
    let destination = format!("HEAD:refs/heads/{work_branch}");
    git(alternate.repository(), &["push", "origin", &destination]);
    let default_before = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);

    let error = harness.installer("1.0.0").sync_knowledge().unwrap_err();
    assert_eq!(error.kind(), sctx_installer::ErrorKind::Conflict);
    assert_eq!(
        git(local.repository(), &["rev-parse", "HEAD"]),
        original_local
    );
    assert!(git(local.repository(), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        default_before
    );
}

#[test]
fn knowledge_sync_retries_a_non_fast_forward_push_race_and_merges_both_heads() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "sync-race");
    let setup = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&fixture.remote))
        .unwrap();
    let work_branch = setup.knowledge_store.work_branch.unwrap();
    harness.installer("1.0.0").sync_knowledge().unwrap();

    let race_root = harness.home.join("race work clone");
    let race = clone_work_store(&race_root, &fixture.remote, &work_branch);
    let race_head = append_space(&race, "remote race Event");
    let local = GitStore::open_existing(&harness.root).unwrap();
    let local_head = append_space(&local, "local race Event");
    let default_before = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);

    let hook = local.repository().join(".git/hooks/pre-push");
    let destination = format!("HEAD:refs/heads/{work_branch}");
    let script = format!(
        "#!/bin/sh\nrm -- \"$0\"\n/usr/bin/git -C {} push origin {} >/dev/null 2>&1\n",
        shell_quote(race.repository()),
        shell_quote(Path::new(&destination))
    );
    fs::write(&hook, script).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let report = harness.installer("1.0.0").sync_knowledge().unwrap();
    assert!(report.pushed);
    assert_eq!(report.behind, 0);
    assert!(report.needs_merge);
    let final_head = git(local.repository(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git(
            &fixture.remote,
            &["rev-parse", &format!("refs/heads/{work_branch}")]
        ),
        final_head
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(local.repository())
            .args(["merge-base", "--is-ancestor", &local_head, &final_head])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(local.repository())
            .args(["merge-base", "--is-ancestor", &race_head, &final_head])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        default_before
    );
}

#[test]
fn knowledge_sync_reports_read_only_push_failure_without_targeting_default() {
    let harness = Harness::new();
    let fixture = remote_fixture(&harness, "read-only-sync");
    let setup = harness
        .installer("1.0.0")
        .setup(&remote_setup_options(&fixture.remote))
        .unwrap();
    let work_branch = setup.knowledge_store.work_branch.unwrap();
    let local = GitStore::open_existing(&harness.root).unwrap();
    let local_head = append_space(&local, "read-only local Event");
    let default_before = git(&fixture.remote, &["rev-parse", "refs/heads/main"]);
    let seen_refs = fixture.remote.join("hooks/rejected-refs");
    let reject = format!(
        "#!/bin/sh\nwhile read old new ref; do printf '%s\\n' \"$ref\" >> {}; done\nexit 1\n",
        shell_quote(&seen_refs)
    );
    install_remote_hook(&fixture.remote, &reject);

    let error = harness.installer("1.0.0").sync_knowledge().unwrap_err();
    assert_eq!(error.kind(), sctx_installer::ErrorKind::External);
    assert!(error.message().contains("write access"));
    assert_eq!(git(local.repository(), &["rev-parse", "HEAD"]), local_head);
    assert!(git(local.repository(), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
        default_before
    );
    let expected = format!("refs/heads/{work_branch}");
    let refs = fs::read_to_string(seen_refs).unwrap();
    assert_eq!(refs.lines().count(), KNOWLEDGE_SYNC_PUSH_ATTEMPTS_FOR_TEST);
    assert!(refs.lines().all(|reference| reference == expected));
}

#[test]
fn knowledge_sync_rejects_local_store_and_rolls_back_invalid_remote_facts() {
    let local = Harness::new();
    local
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let error = local.installer("1.0.0").sync_knowledge().unwrap_err();
    assert_eq!(error.kind(), sctx_installer::ErrorKind::Unsupported);

    let malformed = Harness::new();
    let malformed_fixture = remote_fixture(&malformed, "malformed-sync");
    malformed
        .installer("1.0.0")
        .setup(&remote_setup_options(&malformed_fixture.remote))
        .unwrap();
    let malformed_local = GitStore::open_existing(&malformed.root).unwrap();
    let malformed_original = git(malformed_local.repository(), &["rev-parse", "HEAD"]);
    let invalid_path = malformed_fixture.repository.join("events/invalid.json");
    fs::write(&invalid_path, b"{invalid Event").unwrap();
    git(
        &malformed_fixture.repository,
        &["add", "--", "events/invalid.json"],
    );
    git(
        &malformed_fixture.repository,
        &["commit", "-m", "add malformed sync Event"],
    );
    git(&malformed_fixture.repository, &["push", "origin", "main"]);
    let malformed_default = git(&malformed_fixture.remote, &["rev-parse", "refs/heads/main"]);
    let error = malformed.installer("1.0.0").sync_knowledge().unwrap_err();
    assert_eq!(error.kind(), sctx_installer::ErrorKind::InvariantViolation);
    assert_eq!(
        git(malformed_local.repository(), &["rev-parse", "HEAD"]),
        malformed_original
    );
    assert!(git(malformed_local.repository(), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&malformed_fixture.remote, &["rev-parse", "refs/heads/main"]),
        malformed_default
    );

    let modified = Harness::new();
    let modified_fixture = remote_fixture(&modified, "modified-sync");
    modified
        .installer("1.0.0")
        .setup(&remote_setup_options(&modified_fixture.remote))
        .unwrap();
    let modified_local = GitStore::open_existing(&modified.root).unwrap();
    let modified_original = git(modified_local.repository(), &["rev-parse", "HEAD"]);
    let event_path = git(&modified_fixture.repository, &["ls-files", "events"])
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let event_file = modified_fixture.repository.join(&event_path);
    let mut bytes = fs::read(&event_file).unwrap();
    bytes.push(b'\n');
    fs::write(&event_file, bytes).unwrap();
    git(&modified_fixture.repository, &["add", "--", &event_path]);
    git(
        &modified_fixture.repository,
        &["commit", "-m", "modify existing sync Event"],
    );
    git(&modified_fixture.repository, &["push", "origin", "main"]);
    let modified_default = git(&modified_fixture.remote, &["rev-parse", "refs/heads/main"]);
    let error = modified.installer("1.0.0").sync_knowledge().unwrap_err();
    assert_eq!(error.kind(), sctx_installer::ErrorKind::InvariantViolation);
    assert!(error.message().contains("not append-only"));
    assert_eq!(
        git(modified_local.repository(), &["rev-parse", "HEAD"]),
        modified_original
    );
    assert!(git(modified_local.repository(), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&modified_fixture.remote, &["rev-parse", "refs/heads/main"]),
        modified_default
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_two_installation_team_sharing_and_local_reset_oracle() {
    let oracle = team_sharing_oracle();
    assert_eq!(oracle.schema, "shared-context.team-sharing-oracle");
    assert_eq!(oracle.version, 1);
    assert_eq!(oracle.repository_id, "FE");
    assert!(!oracle.source.contains("/Users/"));

    let machine_a = Harness::new();
    let machine_b = Harness::new();
    let shared = remote_fixture(&machine_a, "team-sharing-final");
    let options = remote_setup_options(&shared.remote);
    let setup_a = machine_a.installer("1.0.0").setup(&options).unwrap();
    let setup_b = machine_b.installer("1.0.0").setup(&options).unwrap();
    assert_ne!(
        setup_a.knowledge_store.installation_id,
        setup_b.knowledge_store.installation_id
    );
    let branch_a = setup_a.knowledge_store.work_branch.unwrap();
    let branch_b = setup_b.knowledge_store.work_branch.unwrap();
    assert!(branch_a.starts_with(&oracle.work_branch_prefix));
    assert!(branch_b.starts_with(&oracle.work_branch_prefix));
    assert_ne!(branch_a, branch_b);

    let checkout_a = init_team_checkout(&machine_a.home.join("A FE checkout"), &oracle);
    let checkout_b = init_team_checkout(&machine_b.home.join("B FE checkout"), &oracle);
    assert_ne!(checkout_a, checkout_b);
    let repository_id: sctx_domain::RepositoryId = oracle.repository_id.parse().unwrap();
    for (root, checkout) in [
        (&machine_a.root, &checkout_a),
        (&machine_b.root, &checkout_b),
    ] {
        UserConfigStore::open_existing(root)
            .unwrap()
            .add_repository(repository_id.clone(), std::slice::from_ref(checkout))
            .unwrap();
    }

    let scan_a = repository_scan_at_root(
        &machine_a.root,
        &RepositoryScanInput {
            checkout_path: checkout_a.to_string_lossy().into_owned(),
            paths: vec![oracle.relative_path.clone()],
            max_artifacts: 20,
        },
    )
    .unwrap();
    assert_eq!(scan_a.repository_id, repository_id);
    let (context_id, revision_id) = append_accepted_team_context(&machine_a.root, &oracle);
    let recorded = engineering_reference_record_at_root(
        &machine_a.root,
        &EngineeringReferenceRecordInput {
            context_id: context_id.to_string(),
            revision_id: revision_id.to_string(),
            repository_id: repository_id.to_string(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator: ArtifactLocator::File {
                path: RepoRelativePath::new(&oracle.relative_path).unwrap(),
            },
            supports: "Machine A inspected the fixed tracked FE artifact".to_owned(),
            limitations: vec!["Resolution must be rebuilt on each installation".to_owned()],
        },
    )
    .unwrap();
    let default_ref = format!("refs/heads/{}", oracle.base_branch);
    let default_before_a = git(&shared.remote, &["rev-parse", &default_ref]);
    let sync_a = machine_a.installer("1.0.0").sync_knowledge().unwrap();
    assert!(sync_a.pushed);
    assert!(sync_a.needs_merge);
    assert_eq!(sync_a.base_branch, oracle.base_branch);
    assert_eq!(
        git(&shared.remote, &["rev-parse", &default_ref]),
        default_before_a
    );

    let branch_a_ref = format!("refs/heads/{branch_a}");
    let integrated_head = git(&shared.remote, &["rev-parse", &branch_a_ref]);
    git(
        &shared.remote,
        &["update-ref", &default_ref, &integrated_head],
    );
    git(&shared.remote, &["update-ref", "-d", &branch_a_ref]);
    let default_before_b = git(&shared.remote, &["rev-parse", &default_ref]);
    let sync_b = machine_b.installer("1.0.0").sync_knowledge().unwrap();
    assert!(sync_b.pushed);
    assert!(!sync_b.needs_merge);
    assert_eq!(sync_b.ahead, 0);
    assert_eq!(sync_b.behind, 0);
    assert_eq!(
        git(&shared.remote, &["rev-parse", &default_ref]),
        default_before_b
    );

    let scan_b = repository_scan_at_root(
        &machine_b.root,
        &RepositoryScanInput {
            checkout_path: checkout_b.to_string_lossy().into_owned(),
            paths: vec![oracle.relative_path.clone()],
            max_artifacts: 20,
        },
    )
    .unwrap();
    assert_eq!(scan_b.repository_id, repository_id);
    let rebuilt = association_rebuild_at_root(
        &machine_b.root,
        &AssociationRebuildInput {
            diagnose_only: false,
        },
    )
    .unwrap();
    assert_eq!(rebuilt.reference_count, 1);
    assert_eq!(rebuilt.status_counts.resolved, 1);
    assert_eq!(rebuilt.repositories.len(), 1);
    assert_eq!(rebuilt.repositories[0].repository_id, repository_id);
    assert_eq!(
        rebuilt.repositories[0].checkout_path.as_ref(),
        Some(&checkout_b)
    );
    let explained = association_explain_at_root(
        &machine_b.root,
        &AssociationExplainInput {
            reference_id: recorded.reference_id.to_string(),
        },
    )
    .unwrap();
    assert_eq!(explained.context_id, context_id);
    assert_eq!(explained.repository_id, repository_id);
    assert_eq!(explained.status, sctx_domain::ResolutionStatus::Resolved);
    let artifact = explained.resolved_artifact.unwrap();
    assert_eq!(artifact.repository_id(), repository_id);
    assert_eq!(
        artifact.locator(),
        &ArtifactLocator::File {
            path: RepoRelativePath::new(&oracle.relative_path).unwrap()
        }
    );

    let task = task_intent_update_at_root(
        &machine_b.root,
        &TaskIntentUpdateInput {
            agent_kind: "codex".to_owned(),
            external_session_id: "team-sharing-machine-b".to_owned(),
            task_boundary: TaskBoundary::New,
            expected_revision_id: ExpectedRevisionId::Null(()),
            intent: WorkingIntentSnapshot::new("Use the shared FE contract").unwrap(),
        },
    )
    .unwrap();
    let focused = task_artifact_focus_at_root(
        &machine_b.root,
        &ArtifactFocusQuery {
            agent_kind: "codex".to_owned(),
            external_session_id: "team-sharing-machine-b".to_owned(),
            expected_revision_id: task.context.intent_revision_id.to_string(),
            absolute_file_path: checkout_b
                .join(&oracle.relative_path)
                .to_string_lossy()
                .into_owned(),
            locator: ArtifactFocusQueryCoordinates::File,
            token_budget: 4_000,
            max_spaces: 8,
        },
    )
    .unwrap();
    assert_eq!(focused.resolved_focus.repository_id, repository_id);
    assert!(focused.context.items.iter().any(|item| {
        item.context.context_id == context_id
            && item
                .retrieval_paths
                .iter()
                .any(|path| matches!(path, TaskRetrievalPath::EngineeringGraph { .. }))
    }));

    let preserved_paths = [
        machine_b.root.join("bin/current/sctx"),
        machine_b.root.join("state/install-manifest.json"),
        machine_b.home.join(".cursor/mcp.json"),
        machine_b.home.join(".cursor/hooks.json"),
        machine_b.home.join(".codex/config.toml"),
        machine_b.home.join(".codex/hooks.json"),
        machine_b.skill_root().join("SKILL.md"),
    ];
    let preserved = preserved_paths
        .iter()
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect::<Vec<_>>();
    let remote_before_reset = git(&shared.remote, &["show-ref"]);
    let reset = machine_b
        .installer("1.0.0")
        .reset_data(DataResetOptions {
            confirmed: true,
            dry_run: false,
        })
        .unwrap();
    assert!(reset.remote_detached);
    assert!(!reset.remote_mutated);
    assert_eq!(reset.repository_count_cleared, 1);
    for (path, bytes) in preserved {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(git(&shared.remote, &["show-ref"]), remote_before_reset);
    let reset_store = GitStore::open_existing(&machine_b.root).unwrap();
    assert!(git(reset_store.repository(), &["remote"]).is_empty());
    assert_eq!(
        git(reset_store.repository(), &["rev-list", "--count", "HEAD"]),
        "1"
    );
    let empty = ProjectionIndex::for_store(&reset_store)
        .domain_snapshot()
        .unwrap();
    assert!(empty.projection.spaces.is_empty());
    assert!(empty.projection.engineering_references.is_empty());
    let empty_catalog = UserConfigStore::open_existing(&machine_b.root)
        .unwrap()
        .repository_catalog()
        .unwrap();
    assert!(empty_catalog.repositories.is_empty());
    assert!(empty_catalog.repository_groups.is_empty());
    assert!(
        RepositoryRegistry::initialize(&machine_b.root)
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
    assert!(
        TaskRuntime::initialize(&machine_b.root)
            .unwrap()
            .read_external_session_by_locator(
                &ExternalSessionLocator::new("codex", "team-sharing-machine-b").unwrap()
            )
            .unwrap()
            .is_none()
    );
    assert!(machine_b.installer("1.0.0").doctor().healthy);
    let a_snapshot = ProjectionIndex::for_store(&GitStore::open_existing(&machine_a.root).unwrap())
        .domain_snapshot()
        .unwrap();
    assert!(
        a_snapshot
            .projection
            .spaces
            .values()
            .any(|space| space.contexts.contains_key(&context_id))
    );
}

#[test]
fn next_setup_recovers_an_incomplete_durable_journal_before_reapplying() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    let report = installer.setup(&SetupOptions::default()).unwrap();
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&report.journal).unwrap()).unwrap();
    journal["complete"] = false.into();
    journal["phase"] = "simulated_process_exit".into();
    fs::write(
        &report.journal,
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let recovered = installer.setup(&SetupOptions::default()).unwrap();
    assert!(recovered.changed);
    assert!(harness.root.join("repository/.git").is_dir());
    assert!(harness.root.join("bin/current/sctx").is_file());
    let recovered_journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&report.journal).unwrap()).unwrap();
    assert_eq!(recovered_journal["phase"], "recovered_rollback");
    assert_eq!(recovered_journal["complete"], true);
}
