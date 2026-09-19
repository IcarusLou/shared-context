//! Cross-layer contract for derived Session activation and its private lease.
//!
//! Nothing here registers an activation boundary by hand: the Catalog knows only
//! Repository checkouts, and every decision below is derived from where the Agent
//! Session started relative to them.

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    str::FromStr,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{ExternalSessionLocator, RepositoryId};
use sctx_git_store::GitStore;
use sctx_local_state::{
    ActivationScope, AuthorizedSessionScopeDecision, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeStore, ORPHAN_LEASE_MAX_AGE, RepositoryCatalogSnapshot, UserConfigStore,
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
        let home = fs::canonicalize(home).unwrap();
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
    /// The directory that holds both registered checkouts. Nobody registered it.
    common_parent: PathBuf,
    outer_checkout: PathBuf,
    nested_checkout: PathBuf,
    sibling: PathBuf,
    unregistered_directory: PathBuf,
    outer_repository_id: RepositoryId,
    nested_repository_id: RepositoryId,
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
    let common_parent = harness.home.join("explicit android fels");
    let outer_checkout = init_git_repository(&common_parent.join("outer"));
    let nested_checkout = init_git_repository(&outer_checkout.join("nested"));
    let sibling = init_git_repository(&common_parent.join("unregistered sibling"));
    let unregistered_directory = harness.home.join("no registered checkouts");
    fs::create_dir_all(outer_checkout.join("feature/deep")).unwrap();
    fs::create_dir_all(nested_checkout.join("feature/deep")).unwrap();
    fs::create_dir_all(&unregistered_directory).unwrap();
    fs::write(sibling.join("business.txt"), BUSINESS_BODY).unwrap();
    let common_parent = fs::canonicalize(common_parent).unwrap();
    let unregistered_directory = fs::canonicalize(unregistered_directory).unwrap();

    let outer_added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "Android",
        "--path",
        outer_checkout.to_str().unwrap(),
    ]);
    let outer_repository_id = repository_id(&outer_added);
    let nested_added = harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "Nested",
        "--path",
        nested_checkout.to_str().unwrap(),
    ]);
    let nested_repository_id = repository_id(&nested_added);

    FoundationFixture {
        harness,
        common_parent,
        outer_checkout,
        nested_checkout,
        sibling,
        unregistered_directory,
        outer_repository_id,
        nested_repository_id,
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
    let kind = if scope.is_enabled() {
        "enabled"
    } else {
        "disabled"
    };
    let mut allowed = scope
        .repository_ids()
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
    assert_eq!(scope.repository_ids(), std::slice::from_ref(expected));
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_cross_layer_oracle_closes_derived_routing_and_no_scan_boundaries() {
    let oracle: Value = serde_json::from_str(ORACLE_JSON).unwrap();
    assert_eq!(oracle["version"], 1);
    let fixture = setup_foundation();

    let common_parent = fixture.resolve(&fixture.common_parent);
    let outer = fixture.resolve(&fixture.outer_checkout.join("feature/deep"));
    let nested = fixture.resolve(&fixture.nested_checkout.join("feature/deep"));
    let unregistered = fixture.resolve(&fixture.unregistered_directory);
    let sibling = fixture.resolve(&fixture.sibling);
    scope_for_repository(&outer, &fixture.outer_repository_id);
    scope_for_repository(&nested, &fixture.nested_repository_id);
    let actual_routing = json!([
        normalized_scope("common_parent", &common_parent, &fixture),
        normalized_scope("outer_checkout", &outer, &fixture),
        normalized_scope("nested_longest_prefix", &nested, &fixture),
        normalized_scope("unregistered_directory", &unregistered, &fixture),
        normalized_scope("unregistered_sibling", &sibling, &fixture)
    ]);
    assert_eq!(actual_routing, oracle["routing"]);

    // The Catalog surface names Repositories and activation switches, and nothing else.
    let first_list = fixture.harness.success(&["repository", "list"]);
    let repeated_list = fixture.harness.success(&["repository", "list"]);
    assert_eq!(first_list["data"], repeated_list["data"]);
    assert_eq!(
        first_list["data"]["repositories"].as_array().unwrap().len(),
        2
    );
    assert!(first_list["data"].get("repository_groups").is_none());
    assert_eq!(first_list["data"]["activation"]["allow_home"], false);
    let doctor = fixture.harness.success(&["repository", "doctor"]);
    assert!(doctor["data"]["catalog"]["healthy"].as_bool().unwrap());
    assert!(doctor["data"]["catalog"].get("repository_groups").is_none());
    assert!(
        doctor["data"]["catalog"]
            .get("repository_group_count")
            .is_none()
    );

    // Deriving activation reads configuration only: no Git child, no Repository scan.
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
    let unreadable = fixture.common_parent.join("must-not-scan");
    fs::create_dir_all(&unreadable).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scope_resolution_helper_has_no_git_or_scan",
            "--nocapture",
        ])
        .env("PATH", &fake_bin)
        // The child owns the home-directory guard's only input.
        .env("HOME", &fixture.harness.home)
        .env("SCTX_M1_HELPER_ROOT", fixture.harness.root())
        .env("SCTX_M1_HELPER_PARENT", &fixture.common_parent)
        .env("SCTX_M1_HELPER_DIRECT", &fixture.nested_checkout)
        .env("SCTX_M1_HELPER_HOME", &fixture.harness.home)
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

    // Nothing in this flow read or rewrote a business file.
    assert_eq!(
        fs::read_to_string(fixture.sibling.join("business.txt")).unwrap(),
        BUSINESS_BODY
    );
}

#[test]
fn scope_resolution_helper_has_no_git_or_scan() {
    let Some(root) = std::env::var_os("SCTX_M1_HELPER_ROOT").map(PathBuf::from) else {
        return;
    };
    let oracle: Value = serde_json::from_str(ORACLE_JSON).unwrap();
    let parent_path = PathBuf::from(std::env::var_os("SCTX_M1_HELPER_PARENT").unwrap());
    let direct_path = PathBuf::from(std::env::var_os("SCTX_M1_HELPER_DIRECT").unwrap());
    let home_path = PathBuf::from(std::env::var_os("SCTX_M1_HELPER_HOME").unwrap());
    let catalog = UserConfigStore::open_existing(&root)
        .unwrap()
        .repository_catalog()
        .unwrap();

    let kind = |path: &Path| {
        if catalog
            .resolve_activation_scope(path)
            .unwrap_or(ActivationScope::Disabled)
            .is_enabled()
        {
            "enabled"
        } else {
            "disabled"
        }
    };
    let guard = &oracle["home_guard"];
    assert_eq!(kind(&parent_path), guard["common_parent_below_home"]);
    assert_eq!(kind(&home_path), guard["home_directory"]);
    assert_eq!(
        kind(home_path.parent().unwrap()),
        guard["home_parent"],
        "the directory that holds home must not derive activation either"
    );
    assert_eq!(kind(Path::new("/")), guard["filesystem_root"]);
    assert!(
        catalog
            .resolve_activation_scope(&direct_path)
            .unwrap()
            .is_enabled()
    );

    let store = AuthorizedSessionScopeStore::initialize(root).unwrap();
    let helper_locator = locator("codex", "isolated-no-git-scope-hot-path");
    store
        .try_authorize_missing(&helper_locator, &catalog, &parent_path)
        .unwrap();
    assert!(matches!(
        store.read(&helper_locator).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.repository_ids().len() == 2
    ));
    assert!(matches!(
        store.try_read_reconciled(&helper_locator, &catalog).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.repository_ids().len() == 2
    ));
}

fn locator(agent: &str, session: &str) -> ExternalSessionLocator {
    ExternalSessionLocator::new(agent, session).unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn fixed_lease_oracle_closes_privacy_identity_permanence_and_catalog_reresolution() {
    let oracle: Value = serde_json::from_str(ORACLE_JSON).unwrap();
    let fixture = setup_foundation();
    let catalog = fixture.catalog();
    let store = Arc::new(AuthorizedSessionScopeStore::initialize(fixture.harness.root()).unwrap());

    // The startup directory the Session was created in owns its decision forever;
    // a later cwd never rewrites it and never widens it.
    let disabled_locator = locator("codex", "disabled-stays-disabled");
    store
        .try_authorize_missing(&disabled_locator, &catalog, &fixture.unregistered_directory)
        .unwrap();
    assert!(fixture.resolve(&fixture.nested_checkout).is_enabled());
    assert!(matches!(
        store
            .try_read_reconciled(&disabled_locator, &catalog)
            .unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    let direct_locator = locator("codex", "same-external-key");
    let parent_locator = locator("cursor", "same-external-key");
    store
        .try_authorize_missing(&direct_locator, &catalog, &fixture.nested_checkout)
        .unwrap();
    store
        .try_authorize_missing(&parent_locator, &catalog, &fixture.common_parent)
        .unwrap();
    assert!(matches!(
        store.read(&direct_locator).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.repository_ids()
                == std::slice::from_ref(&fixture.nested_repository_id)
    ));
    assert!(matches!(
        store.read(&parent_locator).unwrap(),
        AuthorizedSessionScopeRead::Current(ref scope)
            if scope.decision.repository_ids().iter().cloned().collect::<BTreeSet<_>>()
                == BTreeSet::from([
                    fixture.outer_repository_id.clone(),
                    fixture.nested_repository_id.clone(),
                ])
    ));
    assert!(matches!(
        store
            .read(&locator("other-agent", "same-external-key"))
            .unwrap(),
        AuthorizedSessionScopeRead::Missing
    ));

    let hostile_locator = locator("codex/../../hostile", "../session/业务文件?token=none");
    store
        .try_authorize_missing(&hostile_locator, &catalog, &fixture.unregistered_directory)
        .unwrap();
    assert!(matches!(
        store.read(&hostile_locator).unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.external_session_locator == hostile_locator
                && scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    let concurrent_locator = locator("codex", "first-authorization-concurrent");
    let worker_count = 8;
    let barrier = Arc::new(Barrier::new(worker_count));
    let mut workers = Vec::new();
    for _ in 0..worker_count {
        let store = Arc::clone(&store);
        let catalog = catalog.clone();
        let nested = fixture.nested_checkout.clone();
        let concurrent_locator = concurrent_locator.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            loop {
                if let Ok(outcome) =
                    store.try_authorize_missing(&concurrent_locator, &catalog, &nested)
                {
                    return outcome;
                }
            }
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.created).count(),
        usize::try_from(
            oracle["lease"]["concurrent_created_leases"]
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
    let expected_fields = oracle["lease"]["record_fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
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
        assert_eq!(
            record
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_fields
        );
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

    // Registering another Repository under the same parent reaches the live Session
    // that started there, through its recorded startup directory alone.
    let sibling_added = fixture.harness.success(&[
        "repository",
        "add",
        "--repository-id",
        "Sibling",
        "--path",
        fixture.sibling.to_str().unwrap(),
    ]);
    let sibling_repository_id = repository_id(&sibling_added);
    let widened_catalog = fixture.catalog();
    assert!(matches!(
        store
            .try_read_reconciled(&parent_locator, &widened_catalog)
            .unwrap(),
        AuthorizedSessionScopeRead::Current(ref scope)
            if scope.decision.repository_ids().iter().cloned().collect::<BTreeSet<_>>()
                == BTreeSet::from([
                    fixture.outer_repository_id.clone(),
                    fixture.nested_repository_id.clone(),
                    sibling_repository_id.clone(),
                ])
    ));
    // A Session that started inside one checkout is not widened by the same edit.
    assert!(matches!(
        store
            .try_read_reconciled(&direct_locator, &widened_catalog)
            .unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision.repository_ids()
                == std::slice::from_ref(&fixture.nested_repository_id)
    ));
    // And a Disabled Session stays Disabled: its startup directory still holds nothing.
    assert!(matches!(
        store
            .try_read_reconciled(&disabled_locator, &widened_catalog)
            .unwrap(),
        AuthorizedSessionScopeRead::Current(scope)
            if scope.decision == AuthorizedSessionScopeDecision::Disabled
    ));

    // Orphaned leases — Codex desktop never sends SessionEnd — are the only thing
    // that bounds the record directory now that leases are permanent.
    let orphan_path = store
        .directory()
        .join(format!("scope-{}.json", "a".repeat(64)));
    let orphan = json!({
        "version": "v1",
        "external_session_locator": {
            "agent_kind": "codex",
            "external_session_id": "orphaned-desktop-session"
        },
        "decision": {"kind": "disabled"},
        "startup_cwd": fixture.unregistered_directory,
        "issued_at_unix_seconds": 1_000,
    });
    fs::write(&orphan_path, serde_json::to_vec_pretty(&orphan).unwrap()).unwrap();
    fs::set_permissions(&orphan_path, fs::Permissions::from_mode(0o600)).unwrap();
    let survey = store.survey_stale_leases(ORPHAN_LEASE_MAX_AGE).unwrap();
    assert_eq!(survey.stale_entries, 1);
    let reclaim = store.reclaim_stale_leases(ORPHAN_LEASE_MAX_AGE).unwrap();
    assert_eq!(reclaim.removed_entry_keys.len(), 1);
    assert!(!orphan_path.exists());
    assert_eq!(
        ORPHAN_LEASE_MAX_AGE.as_secs(),
        oracle["lease"]["orphan_max_age_seconds"].as_u64().unwrap()
    );
    assert!(matches!(
        store.read(&direct_locator).unwrap(),
        AuthorizedSessionScopeRead::Current(_)
    ));
}
