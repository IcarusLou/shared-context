//! Contract for the permanently Session-bound activation lease.
//!
//! These cases exercise only the public Store surface: a lease is created once
//! by `SessionStart`, never expires, follows later Catalog edits through its
//! recorded `startup_cwd`, and is bounded by orphan reclamation and least
//! recently issued eviction rather than by a TTL.

use std::{
    fs::{self, Permissions},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use sctx_domain::{ExternalSessionLocator, RepositoryId};
use sctx_local_state::{
    AuthorizedSessionScopeDecision, AuthorizedSessionScopePolicy, AuthorizedSessionScopeRead,
    AuthorizedSessionScopeStore, ORPHAN_LEASE_MAX_AGE, RepositoryCatalogEntry,
    RepositoryCatalogSnapshot,
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

/// The directory both registered checkouts live under: starting an Agent here is the
/// "common parent" case that replaced explicitly registered Groups.
const PARENT_ROOT: &str = "/private/leases";
const CHECKOUT: &str = "/private/leases/member";
const SIBLING_CHECKOUT: &str = "/private/leases/sibling";
const OUTSIDE: &str = "/private/unregistered";

struct Fixture {
    _temporary: TempDir,
    root: PathBuf,
    repository_id: RepositoryId,
    sibling_repository_id: RepositoryId,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("shared-context");
        Self {
            _temporary: temporary,
            root,
            repository_id: RepositoryId::new(),
            sibling_repository_id: RepositoryId::new(),
        }
    }

    fn store(&self) -> AuthorizedSessionScopeStore {
        AuthorizedSessionScopeStore::initialize(&self.root).unwrap()
    }

    /// Catalog with both sibling Repositories registered under the same parent.
    fn registered(&self) -> RepositoryCatalogSnapshot {
        RepositoryCatalogSnapshot {
            repositories: vec![
                RepositoryCatalogEntry {
                    repository_id: self.repository_id.clone(),
                    checkout_paths: vec![CHECKOUT.into()],
                },
                RepositoryCatalogEntry {
                    repository_id: self.sibling_repository_id.clone(),
                    checkout_paths: vec![SIBLING_CHECKOUT.into()],
                },
            ],
            ..RepositoryCatalogSnapshot::default()
        }
    }

    /// Catalog before `repository add`, or after the registration was removed.
    fn unregistered() -> RepositoryCatalogSnapshot {
        RepositoryCatalogSnapshot::default()
    }

    fn enabled(repository_ids: &[&RepositoryId]) -> AuthorizedSessionScopeDecision {
        let mut repository_ids = repository_ids
            .iter()
            .map(|id| (*id).clone())
            .collect::<Vec<_>>();
        repository_ids.sort();
        AuthorizedSessionScopeDecision::Enabled { repository_ids }
    }
}

fn locator(session: &str) -> ExternalSessionLocator {
    ExternalSessionLocator::new("codex", session).unwrap()
}

fn current(read: AuthorizedSessionScopeRead) -> sctx_local_state::AuthorizedSessionScope {
    match read {
        AuthorizedSessionScopeRead::Current(scope) => scope,
        AuthorizedSessionScopeRead::Missing => panic!("expected a current lease"),
    }
}

fn write_record(store: &AuthorizedSessionScopeStore, session: &str, record: &Value) -> PathBuf {
    // The record filename is the locator digest; recovering it from the directory
    // keeps this test free of the private hashing helper.
    let before = entry_names(store);
    let path = store
        .directory()
        .join(format!("scope-{}.json", digest_of(store, session, &before)));
    fs::write(&path, serde_json::to_vec_pretty(record).unwrap()).unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path
}

/// Creates and removes a real lease for `session` so the exact digest filename is observed.
fn digest_of(store: &AuthorizedSessionScopeStore, session: &str, before: &[String]) -> String {
    let catalog = RepositoryCatalogSnapshot::default();
    let external = locator(session);
    store
        .try_authorize_missing(&external, &catalog, Path::new(OUTSIDE))
        .unwrap();
    let created = entry_names(store)
        .into_iter()
        .find(|name| !before.contains(name))
        .expect("one new lease entry");
    store.try_remove(&external).unwrap();
    created
        .trim_start_matches("scope-")
        .trim_end_matches(".json")
        .to_owned()
}

fn entry_names(store: &AuthorizedSessionScopeStore) -> Vec<String> {
    let mut names = fs::read_dir(store.directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn a_lease_issued_long_ago_still_authorizes_its_session() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let catalog = fixture.registered();
    let session = "long-running-review";
    let record = json!({
        "version": "v1",
        "external_session_locator": {"agent_kind": "codex", "external_session_id": session},
        "decision": {
            "kind": "enabled",
            "repository_ids": [fixture.repository_id.to_string()],
        },
        "startup_cwd": CHECKOUT,
        "issued_at_unix_seconds": 1_000,
    });
    write_record(&store, session, &record);

    let scope = current(store.try_read(&locator(session)).unwrap());
    assert_eq!(scope.issued_at_unix_seconds, 1_000);
    assert_eq!(scope.decision, Fixture::enabled(&[&fixture.repository_id]));
    let reconciled = current(
        store
            .try_read_reconciled(&locator(session), &catalog)
            .unwrap(),
    );
    assert_eq!(reconciled, scope);
}

#[test]
fn a_lease_written_by_a_superseded_schema_reads_as_missing() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let catalog = fixture.registered();
    let session = "installed-before-the-upgrade";
    let path = write_record(
        &store,
        session,
        &json!({
            "version": "v1",
            "external_session_locator": {"agent_kind": "codex", "external_session_id": session},
            "decision": {"kind": "direct", "repository_id": fixture.repository_id.to_string()},
            "allowed_repository_ids": [fixture.repository_id.to_string()],
            "startup_cwd": CHECKOUT,
            "catalog_revision": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "issued_at_unix_seconds": 1_000,
            "expires_at_unix_seconds": 8_200,
        }),
    );

    assert_eq!(
        store.try_read(&locator(session)).unwrap(),
        AuthorizedSessionScopeRead::Missing
    );
    assert_eq!(
        store
            .try_read_reconciled(&locator(session), &catalog)
            .unwrap(),
        AuthorizedSessionScopeRead::Missing
    );

    // The next SessionStart overwrites it in place rather than leaking a second entry.
    let rewritten = store
        .try_authorize_missing(&locator(session), &catalog, Path::new(CHECKOUT))
        .unwrap();
    assert!(rewritten.created);
    assert_eq!(entry_names(&store).len(), 1);
    let stored = fs::read_to_string(&path).unwrap();
    assert!(!stored.contains("expires_at"));
    assert!(!stored.contains("catalog_revision"));
}

#[test]
fn registering_and_removing_a_repository_reaches_an_already_running_session() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let external = locator("started-before-registration");

    // The Session started inside a directory nobody had registered yet.
    let initial = store
        .try_authorize_missing(&external, &Fixture::unregistered(), Path::new(CHECKOUT))
        .unwrap()
        .scope;
    assert_eq!(initial.decision, AuthorizedSessionScopeDecision::Disabled);
    let issued_at = initial.issued_at_unix_seconds;

    // `sctx repository add` takes effect on the Session's next event.
    let enabled = current(
        store
            .try_read_reconciled(&external, &fixture.registered())
            .unwrap(),
    );
    assert_eq!(
        enabled.decision,
        Fixture::enabled(&[&fixture.repository_id])
    );
    assert_eq!(
        enabled.decision.repository_ids(),
        std::slice::from_ref(&fixture.repository_id)
    );
    assert_eq!(enabled.startup_cwd, PathBuf::from(CHECKOUT));
    assert_eq!(enabled.issued_at_unix_seconds, issued_at);
    assert_eq!(current(store.try_read(&external).unwrap()), enabled);

    // Removing the registration disables the same Session just as promptly.
    let disabled = current(
        store
            .try_read_reconciled(&external, &Fixture::unregistered())
            .unwrap(),
    );
    assert_eq!(disabled.decision, AuthorizedSessionScopeDecision::Disabled);
    assert!(disabled.decision.repository_ids().is_empty());
    assert_eq!(
        current(store.try_read(&external).unwrap()).decision,
        AuthorizedSessionScopeDecision::Disabled
    );
}

#[test]
fn an_unchanged_decision_never_rewrites_the_record() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let catalog = fixture.registered();
    let external = locator("stable-decision");
    store
        .try_authorize_missing(&external, &catalog, Path::new(CHECKOUT))
        .unwrap();
    let path = store
        .directory()
        .join(entry_names(&store).first().unwrap().as_str());
    let before = fs::metadata(&path).unwrap().modified().unwrap();

    for _ in 0..4 {
        assert!(matches!(
            store.try_read_reconciled(&external, &catalog).unwrap(),
            AuthorizedSessionScopeRead::Current(_)
        ));
    }
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);

    // An unrelated Catalog edit that does not change this Session's decision is
    // also not a rewrite: the lease no longer tracks a Catalog revision.
    // A Repository registered somewhere this Session's startup directory does not contain.
    let mut widened = catalog.clone();
    widened.repositories.push(RepositoryCatalogEntry {
        repository_id: RepositoryId::new(),
        checkout_paths: vec!["/private/elsewhere/other".into()],
    });
    assert!(matches!(
        store.try_read_reconciled(&external, &widened).unwrap(),
        AuthorizedSessionScopeRead::Current(scope) if scope.decision.is_enabled()
    ));
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
}

#[test]
fn orphan_reclamation_removes_month_old_and_uninterpretable_leases_only() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let catalog = fixture.registered();

    let live = locator("live-session");
    store
        .try_authorize_missing(&live, &catalog, Path::new(CHECKOUT))
        .unwrap();
    let orphan = "codex-desktop-never-sent-session-end";
    write_record(
        &store,
        orphan,
        &json!({
            "version": "v1",
            "external_session_locator": {"agent_kind": "codex", "external_session_id": orphan},
            "decision": {"kind": "disabled"},
            "startup_cwd": OUTSIDE,
            "issued_at_unix_seconds": 1_000,
        }),
    );
    let corrupt = store.directory().join("scope-corrupt.json");
    fs::write(&corrupt, b"not json").unwrap();
    fs::set_permissions(&corrupt, Permissions::from_mode(0o600)).unwrap();

    let survey = store.survey_stale_leases(ORPHAN_LEASE_MAX_AGE).unwrap();
    assert_eq!(survey.total_entries, 3);
    assert_eq!(survey.stale_entries, 1);
    assert_eq!(survey.unreadable_entries, 1);

    let reclaim = store.reclaim_stale_leases(ORPHAN_LEASE_MAX_AGE).unwrap();
    assert_eq!(reclaim.removed_entry_keys.len(), 1);
    assert_eq!(reclaim.removed_unreadable_entry_keys.len(), 1);
    assert_eq!(reclaim.retained_entries, 1);
    assert!(reclaim.reclaimed_bytes > 0);
    assert_eq!(entry_names(&store).len(), 1);
    assert!(matches!(
        store.try_read(&live).unwrap(),
        AuthorizedSessionScopeRead::Current(_)
    ));
    assert_eq!(
        store
            .survey_stale_leases(ORPHAN_LEASE_MAX_AGE)
            .unwrap()
            .stale_entries,
        0
    );
}

#[test]
fn a_full_store_evicts_the_oldest_lease_instead_of_locking_out_a_live_session() {
    let fixture = Fixture::new();
    let store = AuthorizedSessionScopeStore::with_policy(
        &fixture.root,
        AuthorizedSessionScopePolicy {
            max_entry_bytes: 4_096,
            max_entries: 3,
            max_total_bytes: 64 * 1_024,
        },
    )
    .unwrap();
    let catalog = fixture.registered();

    for index in 0..3 {
        let outcome = store
            .try_authorize_missing(
                &locator(&format!("filler-{index}")),
                &catalog,
                Path::new(CHECKOUT),
            )
            .unwrap();
        assert!(outcome.evicted_entry_keys.is_empty());
    }
    // A lease that predates every live one; it is the first thing eviction takes.
    let ancient = "ancient-session";
    write_record(
        &store,
        ancient,
        &json!({
            "version": "v1",
            "external_session_locator": {"agent_kind": "codex", "external_session_id": ancient},
            "decision": {"kind": "disabled"},
            "startup_cwd": OUTSIDE,
            "issued_at_unix_seconds": 1_000,
        }),
    );

    let admitted = store
        .try_authorize_missing(&locator("arriving-session"), &catalog, Path::new(CHECKOUT))
        .unwrap();
    assert!(admitted.created);
    assert!(!admitted.evicted_entry_keys.is_empty());
    assert_eq!(
        admitted.scope.decision,
        Fixture::enabled(&[&fixture.repository_id])
    );
    assert_eq!(
        store.try_read(&locator(ancient)).unwrap(),
        AuthorizedSessionScopeRead::Missing
    );
    assert!(entry_names(&store).len() <= 3);
}

#[test]
fn session_end_still_removes_exactly_one_lease() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let catalog = fixture.registered();
    let ending = locator("ends-cleanly");
    let other = locator("keeps-running");
    for external in [&ending, &other] {
        store
            .try_authorize_missing(external, &catalog, Path::new(CHECKOUT))
            .unwrap();
    }
    assert!(store.try_remove(&ending).unwrap());
    assert!(!store.try_remove(&ending).unwrap());
    assert_eq!(
        store.try_read(&ending).unwrap(),
        AuthorizedSessionScopeRead::Missing
    );
    assert!(matches!(
        store.try_read(&other).unwrap(),
        AuthorizedSessionScopeRead::Current(_)
    ));
    assert_eq!(
        Duration::from_secs(30 * 24 * 60 * 60),
        ORPHAN_LEASE_MAX_AGE,
        "the documented orphan reclamation window is 30 days"
    );
}

#[test]
fn a_session_started_at_the_common_parent_records_for_every_checkout_below_it() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let external = locator("started-at-the-common-parent");

    let scope = store
        .try_authorize_missing(&external, &fixture.registered(), Path::new(PARENT_ROOT))
        .unwrap()
        .scope;
    assert_eq!(
        scope.decision,
        Fixture::enabled(&[&fixture.repository_id, &fixture.sibling_repository_id])
    );
    assert_eq!(scope.decision.repository_ids().len(), 2);
    assert_eq!(scope.startup_cwd, PathBuf::from(PARENT_ROOT));

    // The lease names identities, never the checkout or parent paths it derived them from.
    let stored = fs::read_to_string(
        store
            .directory()
            .join(entry_names(&store).first().unwrap().as_str()),
    )
    .unwrap();
    for forbidden in ["checkout_path", "root_path", "repository_group"] {
        assert!(!stored.contains(forbidden));
    }

    // Removing one of the two Repositories narrows the same Session on its next event.
    let mut narrowed = fixture.registered();
    narrowed
        .repositories
        .retain(|repository| repository.repository_id == fixture.repository_id);
    assert_eq!(
        current(store.try_read_reconciled(&external, &narrowed).unwrap()).decision,
        Fixture::enabled(&[&fixture.repository_id])
    );

    // A Session started in a directory that contains no registered checkout stays Disabled.
    let outside = locator("started-outside-every-checkout");
    assert_eq!(
        store
            .try_authorize_missing(&outside, &fixture.registered(), Path::new(OUTSIDE))
            .unwrap()
            .scope
            .decision,
        AuthorizedSessionScopeDecision::Disabled
    );
}
