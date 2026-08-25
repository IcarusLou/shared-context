use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    str::FromStr,
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};

use sctx_domain::{ExternalSessionLocator, RepositoryGroupId, RepositoryId};
use sctx_git_store::GitStore;
use sctx_local_state::{
    ActivationScope, ActivationScopeDecision, AuthorizedSessionScopeDecision,
    AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead, AuthorizedSessionScopeStore,
    RepositoryCatalogSnapshot, UserConfigStore,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const ORACLE_JSON: &str =
    include_str!("../../../tests/oracles/repository-scope-foundation-v1.json");
const BUSINESS_BODY: &str = "PROMPT_CANARY_M1_Z8Q4\n\
TRANSCRIPT_CANARY_M1_P7V2\n\
TOOL_OUTPUT_CANARY_M1_C6N9\n\
REPORT_CANARY_M1_K3D5\n\
BUSINESS_BODY_CANARY_M1_H2W7\n";

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("isolated home");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sctx"))
            .arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("sctx should start")
    }

    fn success(&self, args: &[&str]) -> Value {
        if !self.root().join("repository").is_dir() {
            GitStore::bootstrap_local(self.root()).unwrap();
        }
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

struct FoundationFixture {
    harness: Harness,
    group_root: PathBuf,
    outer_checkout: PathBuf,
    nested_checkout: PathBuf,
    sibling: PathBuf,
    replacement_root: PathBuf,
    outer_repository_id: RepositoryId,
    nested_repository_id: RepositoryId,
    repository_group_id: RepositoryGroupId,
    first_group_add: Value,
    repeated_group_add: Value,
}

impl FoundationFixture {
    fn catalog(&self) -> RepositoryCatalogSnapshot {
        UserConfigStore::open_existing(self.harness.root())
            .unwrap()
            .repository_catalog()
            .unwrap()
    }

    fn resolve(&self, path: &Path) -> ActivationScope {
        let canonical = fs::canonicalize(path).unwrap();
        self.catalog().resolve_activation_scope(&canonical).unwrap()
    }
}

fn init_git_repository(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
    fs::canonicalize(path).unwrap()
}

fn repository_id(value: &Value) -> RepositoryId {
    RepositoryId::from_str(
        value["data"]["catalog"]["repository"]["repository_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

fn setup_foundation() -> FoundationFixture {
    let harness = Harness::new();
    let group_root = harness.home.join("explicit android fels");
    let outer_checkout = init_git_repository(&group_root.join("outer"));
    let nested_checkout = init_git_repository(&outer_checkout.join("nested"));
    let sibling = init_git_repository(&group_root.join("unregistered sibling"));
    fs::create_dir_all(outer_checkout.join("feature/deep")).unwrap();
    fs::create_dir_all(nested_checkout.join("feature/deep")).unwrap();
    fs::write(sibling.join("business.txt"), BUSINESS_BODY).unwrap();
    let group_root = fs::canonicalize(group_root).unwrap();

    let replacement_root = harness.home.join("replacement android fels");
    let replacement_outer = init_git_repository(&replacement_root.join("outer"));
    let replacement_nested = init_git_repository(&replacement_outer.join("nested"));
    fs::write(
        replacement_root.join("business-sentinel.txt"),
        BUSINESS_BODY,
    )
    .unwrap();
    let replacement_root = fs::canonicalize(replacement_root).unwrap();

    let outer_added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "Android",
        "--path",
        outer_checkout.to_str().unwrap(),
    ]);
    let outer_repository_id = repository_id(&outer_added);
    harness.success(&[
        "repository",
        "add",
        "--repository-id",
        &outer_repository_id.to_string(),
        "--path",
        replacement_outer.to_str().unwrap(),
    ]);
    let nested_added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "Nested",
        "--path",
        nested_checkout.to_str().unwrap(),
    ]);
    let nested_repository_id = repository_id(&nested_added);
    harness.success(&[
        "repository",
        "add",
        "--repository-id",
        &nested_repository_id.to_string(),
        "--path",
        replacement_nested.to_str().unwrap(),
    ]);

    let member_arguments = [
        outer_repository_id.to_string(),
        nested_repository_id.to_string(),
    ];
    let add_args = [
        "repository",
        "group",
        "add",
        "--root",
        group_root.to_str().unwrap(),
        "--member-repository-id",
        &member_arguments[0],
        "--member-repository-id",
        &member_arguments[1],
    ];
    let first_group_add = harness.success(&add_args);
    let repeated_group_add = harness.success(&add_args);
    let repository_group_id = RepositoryGroupId::from_str(
        first_group_add["data"]["catalog"]["repository_group"]["repository_group_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();

    FoundationFixture {
        harness,
        group_root,
        outer_checkout,
        nested_checkout,
        sibling,
        replacement_root,
        outer_repository_id,
        nested_repository_id,
        repository_group_id,
        first_group_add,
        repeated_group_add,
    }
}

fn repository_alias(
    repository_id: &RepositoryId,
    outer_repository_id: &RepositoryId,
    nested_repository_id: &RepositoryId,
) -> &'static str {
    if repository_id == outer_repository_id {
        "outer"
    } else if repository_id == nested_repository_id {
        "nested"
    } else {
        panic!("unexpected Repository identity: {repository_id}")
    }
}

fn normalized_scope(case: &str, scope: &ActivationScope, fixture: &FoundationFixture) -> Value {
    let kind = match &scope.decision {
        ActivationScopeDecision::Direct { repository_id, .. } => {
            repository_alias(
                repository_id,
                &fixture.outer_repository_id,
                &fixture.nested_repository_id,
            );
            "direct"
        }
        ActivationScopeDecision::Group {
            repository_group_id,
            ..
        } => {
            assert_eq!(*repository_group_id, fixture.repository_group_id);
            "group"
        }
        ActivationScopeDecision::Disabled => "disabled",
    };
    let mut allowed = scope
        .allowed_repository_ids
        .iter()
        .map(|repository_id| {
            repository_alias(
                repository_id,
                &fixture.outer_repository_id,
                &fixture.nested_repository_id,
            )
        })
        .collect::<Vec<_>>();
    allowed.sort_unstable();
    json!({"case": case, "kind": kind, "allowed": allowed})
}

fn scope_for_repository(scope: &ActivationScope, expected: &RepositoryId) {
    assert!(matches!(
        &scope.decision,
        ActivationScopeDecision::Direct { repository_id, .. } if repository_id == expected
    ));
    assert_eq!(
        scope.allowed_repository_ids.as_slice(),
        std::slice::from_ref(expected)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_cross_layer_oracle_closes_routing_group_lifecycle_and_no_scan_boundaries() {
    let oracle: Value = serde_json::from_str(ORACLE_JSON).unwrap();
    assert_eq!(oracle["version"], 1);
    let fixture = setup_foundation();

    let group = fixture.resolve(&fixture.group_root);
    let outer = fixture.resolve(&fixture.outer_checkout.join("feature/deep"));
    let nested = fixture.resolve(&fixture.nested_checkout.join("feature/deep"));
    let ancestor = fixture.resolve(&fixture.harness.home);
    let sibling = fixture.resolve(&fixture.sibling);
    scope_for_repository(&outer, &fixture.outer_repository_id);
    scope_for_repository(&nested, &fixture.nested_repository_id);
    let actual_routing = json!([
        normalized_scope("group_exact_root", &group, &fixture),
        normalized_scope("outer_checkout", &outer, &fixture),
        normalized_scope("nested_longest_prefix", &nested, &fixture),
        normalized_scope("arbitrary_ancestor", &ancestor, &fixture),
        normalized_scope("unregistered_sibling", &sibling, &fixture)
    ]);
    assert_eq!(actual_routing, oracle["routing"]);

    let first_list = fixture.harness.success(&["repository", "group", "list"]);
    let repeated_list = fixture.harness.success(&["repository", "group", "list"]);
    assert_eq!(
        first_list["data"]["repository_groups"],
        repeated_list["data"]["repository_groups"]
    );
    assert_eq!(
        first_list["data"]["repository_groups"]
            .as_array()
            .unwrap()
            .len(),
        usize::try_from(
            oracle["group_lifecycle"]["initial_group_count"]
                .as_u64()
                .unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        first_list["data"]["repository_groups"][0]["status"],
        oracle["group_lifecycle"]["initial_status"]
    );
    let first_doctor = fixture.harness.success(&["repository", "group", "doctor"]);
    let repeated_doctor = fixture.harness.success(&["repository", "group", "doctor"]);
    assert_eq!(first_doctor["data"], repeated_doctor["data"]);
    assert_eq!(
        first_doctor["data"]["healthy"],
        oracle["group_lifecycle"]["initial_doctor_healthy"]
    );

    let fake_bin = fixture.harness.home.join("fake bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nprintf invoked > \"$SCTX_M1_GIT_SENTINEL\"\nexit 93\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
    let git_sentinel = fixture.harness.home.join("git-was-invoked");
    let unreadable = fixture.group_root.join("must-not-scan");
    fs::create_dir_all(&unreadable).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scope_resolution_helper_has_no_git_or_scan",
            "--nocapture",
        ])
        .env("PATH", &fake_bin)
        .env("SCTX_M1_HELPER_ROOT", fixture.harness.root())
        .env("SCTX_M1_HELPER_GROUP", &fixture.group_root)
        .env("SCTX_M1_HELPER_DIRECT", &fixture.nested_checkout)
        .env("SCTX_M1_GIT_SENTINEL", &git_sentinel)
        .output()
        .unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        helper.status.success(),
        "isolated no-Git helper failed: {}",
        String::from_utf8_lossy(&helper.stderr)
    );
    assert!(!git_sentinel.exists(), "scope hot path invoked Git");

    assert_eq!(
        json!([
            fixture.first_group_add["data"]["catalog"]["created"],
            fixture.repeated_group_add["data"]["catalog"]["created"]
        ]),
        oracle["group_lifecycle"]["add_created"]
    );
    let group_id = fixture.repository_group_id.to_string();
    let outer_id = fixture.outer_repository_id.to_string();
    let changed = fixture.harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--member-repository-id",
        &outer_id,
    ]);
    let unchanged = fixture.harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--member-repository-id",
        &outer_id,
    ]);
    assert_eq!(
        json!([
            changed["data"]["catalog"]["changed"],
            unchanged["data"]["catalog"]["changed"]
        ]),
        oracle["group_lifecycle"]["membership_update_changed"]
    );

    let moved_group_root = fixture.harness.home.join("moved explicit android fels");
    fs::rename(&fixture.group_root, &moved_group_root).unwrap();
    let drifted_list = fixture.harness.success(&["repository", "group", "list"]);
    let drifted_doctor = fixture.harness.success(&["repository", "group", "doctor"]);
    assert_eq!(
        drifted_list["data"]["repository_groups"][0]["status"],
        oracle["group_lifecycle"]["drift_status"]
    );
    assert!(!drifted_doctor["data"]["healthy"].as_bool().unwrap());

    let repaired = fixture.harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--root",
        fixture.replacement_root.to_str().unwrap(),
    ]);
    let repair_retry = fixture.harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--root",
        fixture.replacement_root.to_str().unwrap(),
    ]);
    assert_eq!(
        json!([
            repaired["data"]["catalog"]["changed"],
            repair_retry["data"]["catalog"]["changed"]
        ]),
        oracle["group_lifecycle"]["root_update_changed"]
    );
    let repaired_doctor = fixture.harness.success(&["repository", "group", "doctor"]);
    assert!(repaired_doctor["data"]["healthy"].as_bool().unwrap());
    assert_eq!(
        repaired_doctor["data"]["repository_groups"][0]["status"],
        oracle["group_lifecycle"]["repaired_status"]
    );

    let removed = fixture.harness.success(&[
        "repository",
        "group",
        "remove",
        "--repository-group-id",
        &group_id,
    ]);
    let remove_retry = fixture.harness.success(&[
        "repository",
        "group",
        "remove",
        "--repository-group-id",
        &group_id,
    ]);
    assert_eq!(
        json!([
            removed["data"]["catalog"]["removed"],
            remove_retry["data"]["catalog"]["removed"]
        ]),
        oracle["group_lifecycle"]["remove_removed"]
    );
    let final_inspection = UserConfigStore::open_existing(fixture.harness.root())
        .unwrap()
        .inspect_repository_catalog()
        .unwrap();
    assert!(final_inspection.catalog.repository_groups.is_empty());
    assert_eq!(
        final_inspection
            .catalog
            .repositories
            .iter()
            .map(|repository| repository.repository_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            fixture.outer_repository_id.clone(),
            fixture.nested_repository_id.clone(),
        ])
    );
    assert_eq!(
        fs::read_to_string(moved_group_root.join("unregistered sibling/business.txt")).unwrap(),
        BUSINESS_BODY
    );
    assert_eq!(
        fs::read_to_string(fixture.replacement_root.join("business-sentinel.txt")).unwrap(),
        BUSINESS_BODY
    );
}

#[test]
fn scope_resolution_helper_has_no_git_or_scan() {
    let Some(root) = std::env::var_os("SCTX_M1_HELPER_ROOT").map(PathBuf::from) else {
        return;
    };
    let group_path = PathBuf::from(std::env::var_os("SCTX_M1_HELPER_GROUP").unwrap());
    let direct_path = PathBuf::from(std::env::var_os("SCTX_M1_HELPER_DIRECT").unwrap());
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog()
        .unwrap();
    let group = catalog.resolve_activation_scope(&group_path).unwrap();
    let direct = catalog.resolve_activation_scope(&direct_path).unwrap();
    assert!(matches!(
        group.decision,
        ActivationScopeDecision::Group { .. }
    ));
    assert!(matches!(
        direct.decision,
        ActivationScopeDecision::Direct { .. }
    ));
    let store = AuthorizedSessionScopeStore::initialize(root).unwrap();
    let helper_locator = locator("codex", "isolated-no-git-scope-hot-path");
    store.authorize(&helper_locator, &group, &catalog).unwrap();
    assert!(matches!(
        store.read(&helper_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if matches!(scope.decision, AuthorizedSessionScopeDecision::Group { .. })
    ));
}

fn locator(agent: &str, session: &str) -> ExternalSessionLocator {
    ExternalSessionLocator::new(agent, session).unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_lease_oracle_closes_privacy_identity_retry_ttl_and_stale_catalog_boundaries() {
    let oracle: Value = serde_json::from_str(ORACLE_JSON).unwrap();
    let fixture = setup_foundation();
    let catalog = fixture.catalog();
    let store = Arc::new(AuthorizedSessionScopeStore::initialize(fixture.harness.root()).unwrap());
    let disabled_scope = fixture.resolve(&fixture.harness.home);
    let direct_scope = fixture.resolve(&fixture.nested_checkout);
    let group_scope = fixture.resolve(&fixture.group_root);

    let disabled_locator = locator("codex", "disabled-stays-disabled");
    store
        .authorize(&disabled_locator, &disabled_scope, &catalog)
        .unwrap();
    let later_direct_resolution = fixture.resolve(&fixture.nested_checkout);
    assert!(matches!(
        later_direct_resolution.decision,
        ActivationScopeDecision::Direct { .. }
    ));
    assert!(matches!(
        store.read(&disabled_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    let direct_locator = locator("codex", "same-external-key");
    let group_locator = locator("cursor", "same-external-key");
    store
        .authorize(&direct_locator, &direct_scope, &catalog)
        .unwrap();
    store
        .authorize(&group_locator, &group_scope, &catalog)
        .unwrap();
    assert!(matches!(
        store.read(&direct_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Direct {
                repository_id: fixture.nested_repository_id.clone()
            }
    ));
    assert!(matches!(
        store.read(&group_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Group {
                repository_group_id: fixture.repository_group_id
            }
    ));
    assert!(matches!(
        store
            .read(&locator("other-agent", "same-external-key"), &catalog)
            .unwrap(),
        AuthorizedSessionScopeRead::Missing
    ));

    let hostile_locator = locator("codex/../../hostile", "../session/业务文件?token=none");
    store
        .authorize(&hostile_locator, &disabled_scope, &catalog)
        .unwrap();
    assert!(matches!(
        store.read(&hostile_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.external_session_locator == hostile_locator
                && scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    let concurrent_locator = locator("codex", "semantic-retry-concurrent");
    let worker_count = 8;
    let barrier = Arc::new(Barrier::new(worker_count));
    let mut workers = Vec::new();
    for _ in 0..worker_count {
        let store = Arc::clone(&store);
        let catalog = catalog.clone();
        let direct_scope = direct_scope.clone();
        let concurrent_locator = concurrent_locator.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            store
                .authorize(&concurrent_locator, &direct_scope, &catalog)
                .unwrap()
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.semantic_changed)
            .count(),
        usize::try_from(
            oracle["lease"]["concurrent_semantic_changes"]
                .as_u64()
                .unwrap()
        )
        .unwrap()
    );

    let directory_mode = fs::metadata(store.directory())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        u64::from(directory_mode),
        oracle["lease"]["directory_mode"].as_u64().unwrap()
    );
    let entries = fs::read_dir(store.directory())
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 5);
    let mut decision_kinds = BTreeSet::new();
    for entry in &entries {
        let filename = entry.file_name().to_string_lossy().into_owned();
        assert_eq!(filename.len(), "scope-".len() + 64 + ".json".len());
        assert!(filename.starts_with("scope-"));
        assert_eq!(
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("json")
        );
        assert!(
            filename["scope-".len()..filename.len() - ".json".len()]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert!(!filename.contains("session"));
        assert!(!filename.contains("业务文件"));
        let record_mode = fs::metadata(entry.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            u64::from(record_mode),
            oracle["lease"]["record_mode"].as_u64().unwrap()
        );
        let record: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        decision_kinds.insert(record["decision"]["kind"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        decision_kinds,
        oracle["lease"]["decision_kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>()
    );

    let private_state = entries
        .iter()
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
    for forbidden in oracle["lease"]["forbidden_state_fragments"]
        .as_array()
        .unwrap()
    {
        let forbidden = forbidden.as_str().unwrap();
        assert!(
            !private_state.contains(forbidden),
            "private scope state contains forbidden fragment {forbidden:?}"
        );
    }

    let group_id = fixture.repository_group_id.to_string();
    let outer_id = fixture.outer_repository_id.to_string();
    fixture.harness.success(&[
        "repository",
        "group",
        "update",
        "--repository-group-id",
        &group_id,
        "--member-repository-id",
        &outer_id,
    ]);
    let changed_catalog = fixture.catalog();
    assert!(matches!(
        store.read(&group_locator, &changed_catalog).unwrap(),
        AuthorizedSessionScopeRead::StaleCatalog
    ));

    let ttl_seconds = oracle["lease"]["ttl_seconds"].as_u64().unwrap();
    let ttl_store = AuthorizedSessionScopeStore::with_policy(
        fixture.harness.root(),
        AuthorizedSessionScopePolicy {
            ttl: Duration::from_secs(ttl_seconds),
            ..AuthorizedSessionScopePolicy::default()
        },
    )
    .unwrap();
    let ttl_locator = locator("codex", "short-ttl");
    let current_direct_scope = fixture.resolve(&fixture.nested_checkout);
    let ttl = ttl_store
        .authorize(&ttl_locator, &current_direct_scope, &changed_catalog)
        .unwrap();
    assert_eq!(
        ttl.scope
            .expires_at_unix_seconds
            .saturating_sub(ttl.scope.issued_at_unix_seconds),
        ttl_seconds
    );
    thread::sleep(Duration::from_secs(ttl_seconds + 1));
    assert!(matches!(
        ttl_store.read(&ttl_locator, &changed_catalog).unwrap(),
        AuthorizedSessionScopeRead::Expired
    ));
    let max_ttl_seconds = oracle["lease"]["max_ttl_seconds"].as_u64().unwrap();
    assert!(
        AuthorizedSessionScopeStore::with_policy(
            fixture.harness.home.join("oversized ttl"),
            AuthorizedSessionScopePolicy {
                ttl: Duration::from_secs(max_ttl_seconds + 1),
                ..AuthorizedSessionScopePolicy::default()
            },
        )
        .is_err()
    );
}
