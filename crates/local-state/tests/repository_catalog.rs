use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use sctx_domain::{ArtifactLocator, ErrorKind, RepositoryId};
use sctx_local_state::{
    ActivationScope, ActivationSettings, CatalogCheckoutStatus, RepositoryCatalogDiagnostic,
    RepositoryCatalogEntry, RepositoryCatalogSnapshot, RetrievalSettings, UserConfigStore,
    migrate_legacy_repository_groups,
};
use tempfile::TempDir;

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

fn init_repo(path: &Path, marker: &str) -> PathBuf {
    fs::create_dir_all(path.join("src/search")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["config", "user.name", "Catalog Test"]);
    git(path, &["config", "user.email", "catalog@example.invalid"]);
    fs::write(path.join("src/search/Search.kt"), format!("// {marker}\n")).unwrap();
    git(path, &["add", "--", "."]);
    git(path, &["commit", "-q", "-m", "fixture"]);
    fs::canonicalize(path).unwrap()
}

#[test]
fn catalog_binds_readable_ids_atomically_and_supports_explicit_worktrees() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let repository = init_repo(&temporary.path().join("primary repo"), "primary");
    let worktree = temporary.path().join("linked worktree");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "catalog-worktree",
            worktree.to_str().unwrap(),
        ],
    );
    let worktree = fs::canonicalize(worktree).unwrap();
    let config = UserConfigStore::initialize(&root).unwrap();
    let repository_id: RepositoryId = "FE".parse().unwrap();

    let created = config
        .add_repository(repository_id.clone(), std::slice::from_ref(&repository))
        .unwrap();
    assert!(created.created_identity);
    assert_eq!(created.repository.repository_id, repository_id);
    let extended = config
        .add_repository(
            created.repository.repository_id.clone(),
            std::slice::from_ref(&worktree),
        )
        .unwrap();
    assert!(!extended.created_identity);
    assert_eq!(extended.added_paths, 1);
    assert_eq!(
        extended.repository.checkout_paths,
        vec![worktree, repository.clone()]
    );
    let repeated = config
        .add_repository(repository_id.clone(), std::slice::from_ref(&repository))
        .unwrap();
    assert!(!repeated.created_identity);
    assert_eq!(repeated.added_paths, 0);

    let case_conflict = init_repo(&temporary.path().join("case conflict"), "case-conflict");
    let error = config
        .add_repository("fe".parse().unwrap(), std::slice::from_ref(&case_conflict))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.message().contains("differs only by ASCII case"));
    assert_eq!(config.repository_catalog().unwrap().repositories.len(), 1);
    let document = fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(document.contains("[[repositories]]"));
    assert!(document.contains(&format!("id = \"{}\"", created.repository.repository_id)));
    assert!(!document.contains("repository_groups"));
    assert!(!document.contains("space"));
    assert!(!document.contains("workspace"));
    assert!(
        !UserConfigStore::open_existing(&root)
            .unwrap()
            .repository_catalog()
            .unwrap()
            .activation
            .allow_home,
        "an absent [activation] table keeps the home-directory guard on"
    );
    let metadata = fs::metadata(root.join("config.toml")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn same_team_id_maps_different_installation_checkouts_without_path_identity() {
    let temporary = TempDir::new().unwrap();
    let first_checkout = init_repo(&temporary.path().join("member-a/frontend"), "member-a");
    let second_checkout = init_repo(&temporary.path().join("member-b/web"), "member-b");
    let repository_id: RepositoryId = "FE".parse().unwrap();

    let first = UserConfigStore::initialize(temporary.path().join("install-a")).unwrap();
    let second = UserConfigStore::initialize(temporary.path().join("install-b")).unwrap();
    let first_added = first
        .add_repository(repository_id.clone(), std::slice::from_ref(&first_checkout))
        .unwrap();
    let second_added = second
        .add_repository(
            repository_id.clone(),
            std::slice::from_ref(&second_checkout),
        )
        .unwrap();

    assert_eq!(first_added.repository.repository_id, repository_id);
    assert_eq!(second_added.repository.repository_id, repository_id);
    assert_ne!(
        first_added.repository.checkout_paths,
        second_added.repository.checkout_paths
    );
    let first_file = first_checkout.join("src/search/Search.kt");
    let second_file = second_checkout.join("src/search/Search.kt");
    assert_eq!(
        first
            .repository_catalog()
            .unwrap()
            .resolve_file_path(&first_file, std::slice::from_ref(&first_checkout))
            .unwrap()
            .repository_id,
        repository_id
    );
    assert_eq!(
        second
            .repository_catalog()
            .unwrap()
            .resolve_file_path(&second_file, std::slice::from_ref(&second_checkout))
            .unwrap()
            .repository_id,
        repository_id
    );
}

#[test]
fn cross_workspace_resolution_is_stable_isolated_and_rejects_unsafe_paths() {
    let temporary = TempDir::new().unwrap();
    let cross = temporary.path().join("workspace cross 中文");
    let fe = init_repo(&cross.join("fe/search_web_monorepo"), "fe");
    let android = init_repo(&cross.join("android/TikTok"), "android");
    let ios = init_repo(&cross.join("ios/TikTok"), "ios");
    let sibling = init_repo(&cross.join("unconfigured/TikTok"), "sibling");
    let config = UserConfigStore::initialize(temporary.path().join("state root")).unwrap();
    let fe_id = config
        .add_repository("FE".parse().unwrap(), std::slice::from_ref(&fe))
        .unwrap()
        .repository
        .repository_id;
    let android_id = config
        .add_repository("Android".parse().unwrap(), std::slice::from_ref(&android))
        .unwrap()
        .repository
        .repository_id;
    let ios_id = config
        .add_repository("iOS".parse().unwrap(), std::slice::from_ref(&ios))
        .unwrap()
        .repository
        .repository_id;
    let catalog = config.repository_catalog().unwrap();
    let cross = fs::canonicalize(cross).unwrap();

    for (repository, expected_id) in [
        (&fe, fe_id.clone()),
        (&android, android_id.clone()),
        (&ios, ios_id.clone()),
    ] {
        let file = repository.join("src/search/Search.kt");
        for workspace in [&cross, repository, &repository.join("src")] {
            let resolved = catalog
                .resolve_file_path(&file, std::slice::from_ref(workspace))
                .unwrap();
            assert_eq!(resolved.repository_id, expected_id);
            assert_eq!(resolved.checkout_path, *repository);
            assert_eq!(resolved.relative_path.as_str(), "src/search/Search.kt");
            let focus = resolved.into_resolved_file_focus();
            assert_eq!(focus.repository_id, expected_id);
            assert!(matches!(
                focus.locator,
                ArtifactLocator::File { ref path }
                    if path.as_str() == "src/search/Search.kt"
            ));
        }
    }
    assert_ne!(android_id, ios_id);
    let unconfigured = catalog
        .resolve_file_path(
            &sibling.join("src/search/Search.kt"),
            std::slice::from_ref(&cross),
        )
        .unwrap_err();
    assert_eq!(unconfigured.kind(), ErrorKind::RepositoryNotConfigured);
    let outside = catalog
        .resolve_file_path(
            &android.join("src/search/Search.kt"),
            std::slice::from_ref(&fe),
        )
        .unwrap_err();
    assert_eq!(outside.kind(), ErrorKind::InvalidInput);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            android.join("src/search/Search.kt"),
            android.join("src/search/link.kt"),
        )
        .unwrap();
        assert!(
            catalog
                .resolve_file_path(
                    &android.join("src/search/link.kt"),
                    std::slice::from_ref(&cross),
                )
                .unwrap_err()
                .message()
                .contains("symlink")
        );
        std::os::unix::fs::symlink(android.join("src/search"), android.join("linked-search"))
            .unwrap();
        assert!(
            catalog
                .resolve_file_path(
                    &android.join("linked-search/Search.kt"),
                    std::slice::from_ref(&cross),
                )
                .is_err(),
            "a symlinked directory component must not escape canonical mapping"
        );
    }
}

#[test]
fn declared_path_resolution_allows_missing_tail_but_rejects_escape_and_symlink_components() {
    let temporary = TempDir::new().unwrap();
    let repository = init_repo(&temporary.path().join("declared repo"), "declared");
    let outside = init_repo(&temporary.path().join("outside repo"), "outside");
    let config = UserConfigStore::initialize(temporary.path().join("declared state")).unwrap();
    let repository_id = config
        .add_repository(RepositoryId::new(), std::slice::from_ref(&repository))
        .unwrap()
        .repository
        .repository_id;
    let catalog = config.repository_catalog().unwrap();

    let missing = repository.join("src/future/generated/SearchContract.proto");
    assert!(!missing.exists());
    let resolved = catalog.resolve_declared_path(&missing).unwrap();
    assert_eq!(resolved.repository_id, repository_id);
    assert_eq!(
        resolved.relative_path.as_str(),
        "src/future/generated/SearchContract.proto"
    );
    let existing = catalog
        .resolve_declared_path(&repository.join("src/search/Search.kt"))
        .unwrap();
    assert_eq!(existing.repository_id, repository_id);
    assert_eq!(
        catalog
            .resolve_declared_path(&outside.join("missing.kt"))
            .unwrap_err()
            .kind(),
        ErrorKind::RepositoryNotConfigured
    );
    assert!(
        catalog
            .resolve_declared_path(&repository.join("src/../escaped.kt"))
            .is_err()
    );

    fs::write(repository.join("src/not-a-directory"), "fixture").unwrap();
    assert!(
        catalog
            .resolve_declared_path(&repository.join("src/not-a-directory/tail.kt"))
            .unwrap_err()
            .message()
            .contains("non-directory")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            repository.join("src/search"),
            repository.join("src/linked-search"),
        )
        .unwrap();
        assert!(
            catalog
                .resolve_declared_path(&repository.join("src/linked-search/missing.kt"))
                .unwrap_err()
                .message()
                .contains("symlink")
        );
    }
}

#[test]
fn catalog_writes_are_concurrent_and_doctor_reports_checkout_drift() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("catalog root");
    let repositories = (0..8)
        .map(|index| init_repo(&temporary.path().join(format!("repo-{index}")), "fixture"))
        .collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(repositories.len()));
    let mut workers = Vec::new();
    for repository in &repositories {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        let repository = repository.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            UserConfigStore::initialize(root)
                .unwrap()
                .add_repository(RepositoryId::new(), &[repository])
                .unwrap()
                .repository
                .repository_id
        }));
    }
    let ids = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), repositories.len());
    let config = UserConfigStore::initialize(&root).unwrap();
    assert_eq!(config.repository_catalog().unwrap().repositories.len(), 8);

    fs::remove_dir_all(&repositories[0]).unwrap();
    let doctor = config.doctor_repository_catalog().unwrap();
    assert!(!doctor.healthy);
    assert_eq!(doctor.repository_count, 8);
    assert_eq!(doctor.checkout_count, 8);
    assert_eq!(
        doctor
            .checkouts
            .iter()
            .filter(|checkout| checkout.status == CatalogCheckoutStatus::Missing)
            .count(),
        1
    );
}

#[test]
fn doctor_reports_legacy_repository_ids_with_a_stable_kind_without_affecting_healthy() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("catalog root");
    let legacy_repository = init_repo(&temporary.path().join("legacy repo"), "legacy");
    let readable_repository = init_repo(&temporary.path().join("readable repo"), "readable");
    let config = UserConfigStore::initialize(&root).unwrap();
    let legacy_id: RepositoryId = "rpo_00000000-0000-4000-8000-000000000901".parse().unwrap();
    let readable_id: RepositoryId = "FE".parse().unwrap();
    config
        .add_repository(legacy_id.clone(), std::slice::from_ref(&legacy_repository))
        .unwrap();
    config
        .add_repository(
            readable_id.clone(),
            std::slice::from_ref(&readable_repository),
        )
        .unwrap();

    let doctor = config.doctor_repository_catalog().unwrap();
    assert!(
        doctor.healthy,
        "a legacy identity spelling is a migration hint, not a health failure"
    );
    assert_eq!(doctor.diagnostics.len(), 1);
    let RepositoryCatalogDiagnostic::LegacyRepositoryId {
        repository_id,
        message,
        migration_command,
    } = &doctor.diagnostics[0];
    assert_eq!(*repository_id, legacy_id);
    assert!(message.contains("ADR-0001"));
    assert_eq!(
        migration_command,
        &format!("sctx repository rename --from {legacy_id} --to <ReadableRepositoryId>")
    );

    // Renaming the legacy identity away clears the diagnostic.
    config
        .rename_repository(&legacy_id, &"Android".parse().unwrap())
        .unwrap();
    assert!(
        config
            .doctor_repository_catalog()
            .unwrap()
            .diagnostics
            .is_empty()
    );
}

#[test]
fn rename_repository_is_local_only_and_rejects_conflicts() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("catalog root");
    let first = init_repo(&temporary.path().join("parent/first"), "first");
    let second = init_repo(&temporary.path().join("second"), "second");
    let config = UserConfigStore::initialize(&root).unwrap();
    let legacy_id: RepositoryId = "rpo_00000000-0000-4000-8000-000000000902".parse().unwrap();
    let existing_id: RepositoryId = "FE".parse().unwrap();
    config
        .add_repository(legacy_id.clone(), std::slice::from_ref(&first))
        .unwrap();
    config
        .add_repository(existing_id.clone(), std::slice::from_ref(&second))
        .unwrap();

    let readable_id: RepositoryId = "Android".parse().unwrap();
    let renamed = config.rename_repository(&legacy_id, &readable_id).unwrap();
    assert_eq!(renamed.previous_repository_id, legacy_id);
    assert_eq!(renamed.repository.repository_id, readable_id);
    assert_eq!(renamed.repository.checkout_paths, vec![first.clone()]);

    let catalog = config.repository_catalog().unwrap();
    assert!(
        catalog
            .repositories
            .iter()
            .any(|repository| repository.repository_id == readable_id)
    );
    assert!(
        !catalog
            .repositories
            .iter()
            .any(|repository| repository.repository_id == legacy_id)
    );

    // Same-content replay after a successful rename now reports the old
    // identity as not configured.
    assert_eq!(
        config
            .rename_repository(&legacy_id, &"iOS".parse().unwrap())
            .unwrap_err()
            .kind(),
        ErrorKind::RepositoryNotConfigured
    );

    // The target identity already names a configured Repository (exact and
    // case-insensitive) -- both are rejected as a typed Conflict.
    assert_eq!(
        config
            .rename_repository(&readable_id, &existing_id)
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );
    assert_eq!(
        config
            .rename_repository(&readable_id, &"fe".parse().unwrap())
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );

    // Renaming an identity to itself is rejected before any lock is taken.
    assert_eq!(
        config
            .rename_repository(&readable_id, &readable_id)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn concurrent_explicit_same_identity_converges_to_one_catalog_entry() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("shared identity catalog");
    let repositories = (0..8)
        .map(|index| init_repo(&temporary.path().join(format!("member-{index}/fe")), "FE"))
        .collect::<Vec<_>>();
    let repository_id: RepositoryId = "FE".parse().unwrap();
    let barrier = Arc::new(Barrier::new(repositories.len()));
    let mut workers = Vec::new();
    for repository in &repositories {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        let repository = repository.clone();
        let repository_id = repository_id.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            UserConfigStore::initialize(root)
                .unwrap()
                .add_repository(repository_id, &[repository])
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
            .filter(|outcome| outcome.created_identity)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| &outcome.repository.repository_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([&repository_id])
    );
    let catalog = UserConfigStore::initialize(root)
        .unwrap()
        .repository_catalog()
        .unwrap();
    assert_eq!(catalog.repositories.len(), 1);
    assert_eq!(catalog.repositories[0].repository_id, repository_id);
    assert_eq!(
        catalog.repositories[0].checkout_paths.len(),
        repositories.len()
    );
}

#[test]
fn failed_config_write_releases_exclusive_lock_before_immediate_catalog_read() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let temporary = TempDir::new().unwrap();
    let state_root = temporary.path().join("write failure state");
    let first = init_repo(&temporary.path().join("first repo"), "first");
    let second = init_repo(&temporary.path().join("second repo"), "second");
    let config = UserConfigStore::initialize(&state_root).unwrap();
    let first_id = config
        .add_repository(RepositoryId::new(), std::slice::from_ref(&first))
        .unwrap()
        .repository
        .repository_id;

    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o500)).unwrap();
    let failed_write = config.add_repository(RepositoryId::new(), std::slice::from_ref(&second));
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();

    let error = failed_write.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("create temporary config"));
    let catalog = config.repository_catalog().unwrap();
    assert_eq!(catalog.repositories.len(), 1);
    assert_eq!(catalog.repositories[0].repository_id, first_id);
}

#[test]
#[allow(clippy::too_many_lines)]
fn activation_is_derived_from_registered_checkouts_and_the_directories_above_them() {
    let temporary = TempDir::new().unwrap();
    let cross = temporary.path().join("activation cross");
    let android = init_repo(&cross.join("android/Product"), "android");
    let ios = init_repo(&cross.join("ios/Product"), "ios");
    let sibling = init_repo(&cross.join("unregistered/Product"), "sibling");
    let outer = init_repo(&cross.join("nested/outer"), "outer");
    let nested = init_repo(&outer.join("components/inner"), "nested");
    let common_dir_sibling = cross.join("unregistered-worktree");
    git(
        &android,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "unregistered-activation-worktree",
            common_dir_sibling.to_str().unwrap(),
        ],
    );
    let common_dir_sibling = fs::canonicalize(common_dir_sibling).unwrap();

    let config = UserConfigStore::initialize(temporary.path().join("activation state")).unwrap();
    let android_id = config
        .add_repository("Android".parse().unwrap(), std::slice::from_ref(&android))
        .unwrap()
        .repository
        .repository_id;
    let ios_id = config
        .add_repository("iOS".parse().unwrap(), std::slice::from_ref(&ios))
        .unwrap()
        .repository
        .repository_id;
    let outer_id = config
        .add_repository("Outer".parse().unwrap(), std::slice::from_ref(&outer))
        .unwrap()
        .repository
        .repository_id;
    let nested_id = config
        .add_repository("Nested".parse().unwrap(), std::slice::from_ref(&nested))
        .unwrap()
        .repository
        .repository_id;
    let cross = fs::canonicalize(cross).unwrap();
    let catalog = config.repository_catalog().unwrap();

    let enabled = |cwd: &Path| match catalog.resolve_activation_scope(cwd).unwrap() {
        ActivationScope::Enabled { repository_ids } => {
            repository_ids.into_iter().collect::<BTreeSet<_>>()
        }
        ActivationScope::Disabled => BTreeSet::new(),
    };

    // Rule 1: inside a registered checkout, the deepest checkout owns the Session.
    assert_eq!(
        enabled(&android.join("src/search")),
        BTreeSet::from([android_id.clone()])
    );
    assert_eq!(
        enabled(&nested.join("src/search")),
        BTreeSet::from([nested_id.clone()])
    );
    // Standing on a checkout root that itself contains another registered checkout is
    // still rule 1: the Session is inside `Outer`, so that is what it records for.
    assert_eq!(enabled(&outer), BTreeSet::from([outer_id.clone()]));

    // Rule 2: the common parent of several checkouts derives every one of them, with no
    // Group registered anywhere.
    assert_eq!(
        enabled(&cross),
        BTreeSet::from([
            android_id.clone(),
            ios_id.clone(),
            outer_id.clone(),
            nested_id.clone(),
        ])
    );
    // An intermediate directory derives only what actually lives below it.
    assert_eq!(
        enabled(&fs::canonicalize(cross.join("android")).unwrap()),
        BTreeSet::from([android_id.clone()])
    );

    // Everything else stays Disabled: an unregistered checkout, an unregistered worktree,
    // and a directory that contains no registered checkout at all.
    for disabled_path in [
        sibling,
        common_dir_sibling,
        fs::canonicalize(cross.join("unregistered")).unwrap(),
    ] {
        assert_eq!(
            catalog.resolve_activation_scope(&disabled_path).unwrap(),
            ActivationScope::Disabled
        );
    }
}

#[test]
fn the_filesystem_root_and_the_home_directory_never_derive_activation() {
    // The guard reads `$HOME`, so the assertions run in a child with a controlled one.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "home_guard_child", "--nocapture"])
        .env("HOME", GUARDED_HOME)
        .env("SCTX_HOME_GUARD_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "home guard child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const GUARDED_HOME: &str = "/private/guarded-home";

#[test]
fn home_guard_child() {
    if std::env::var_os("SCTX_HOME_GUARD_CHILD").is_none() {
        return;
    }
    let home = PathBuf::from(GUARDED_HOME);
    let checkout = home.join("work/product");
    let repository_id = RepositoryId::new();
    let guarded = RepositoryCatalogSnapshot {
        repositories: vec![RepositoryCatalogEntry {
            repository_id: repository_id.clone(),
            checkout_paths: vec![checkout],
        }],
        ..RepositoryCatalogSnapshot::default()
    };
    let permissive = RepositoryCatalogSnapshot {
        activation: ActivationSettings { allow_home: true },
        ..guarded.clone()
    };
    let home_parent = home.parent().unwrap().to_path_buf();
    let expected = ActivationScope::Enabled {
        repository_ids: vec![repository_id],
    };

    // Starting at home, at the directory that holds home, or at the filesystem root would
    // otherwise sweep in every registered Repository on the machine.
    for guarded_cwd in [home.clone(), home_parent.clone(), PathBuf::from("/")] {
        assert_eq!(
            guarded
                .resolve_recorded_activation_scope(&guarded_cwd)
                .unwrap(),
            ActivationScope::Disabled,
            "{} must not derive activation",
            guarded_cwd.display()
        );
    }

    // A directory between home and the checkout is an ordinary parent and still derives.
    assert_eq!(
        guarded
            .resolve_recorded_activation_scope(&home.join("work"))
            .unwrap(),
        expected
    );

    // `[activation] allow_home` lifts the two home guards, and only those.
    for allowed_cwd in [home, home_parent] {
        assert_eq!(
            permissive
                .resolve_recorded_activation_scope(&allowed_cwd)
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        permissive
            .resolve_recorded_activation_scope(Path::new("/"))
            .unwrap(),
        ActivationScope::Disabled,
        "the filesystem root is never derivable"
    );
}

#[test]
fn a_config_document_that_still_declares_repository_groups_is_migrated_once() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("legacy state");
    let repository = init_repo(&temporary.path().join("legacy/member"), "member");
    let store = UserConfigStore::initialize(&root).unwrap();
    store
        .add_repository("FE".parse::<RepositoryId>().unwrap(), &[repository])
        .unwrap();

    let config_path = root.join("config.toml");
    let current = fs::read_to_string(&config_path).unwrap();
    assert!(
        migrate_legacy_repository_groups(&current)
            .unwrap()
            .is_none(),
        "a document without the removed section is left byte-identical"
    );

    let legacy = format!(
        "{current}\n[[repository_groups]]\n\
         id = \"rpg_00000000-0000-4000-8000-000000000001\"\n\
         root = \"/legacy\"\n\
         members = [\"FE\"]\n"
    );
    fs::write(&config_path, &legacy).unwrap();
    assert_eq!(
        store.repository_catalog().unwrap_err().kind(),
        ErrorKind::InvalidInput,
        "the removed section is refused rather than silently ignored"
    );

    let migrated = migrate_legacy_repository_groups(&legacy)
        .unwrap()
        .expect("the legacy section is migrated");
    assert!(!migrated.contains("repository_groups"));
    fs::write(&config_path, &migrated).unwrap();
    let catalog = store.repository_catalog().unwrap();
    assert_eq!(catalog.repositories.len(), 1);
    assert!(!catalog.activation.allow_home);
    assert!(
        migrate_legacy_repository_groups(&migrated)
            .unwrap()
            .is_none(),
        "migration is idempotent"
    );
}

#[test]
fn an_activation_decision_carries_repository_identity_and_never_a_path() {
    fn assert_local_metadata_type<T: Clone + std::fmt::Debug + Eq + serde::Serialize>() {}

    assert_local_metadata_type::<ActivationScope>();
    assert!(
        std::any::type_name::<ActivationScope>().starts_with("sctx_local_state::"),
        "ActivationScope must remain a local-state type, not a durable domain type"
    );

    let repository_id = RepositoryId::new();
    let sibling_id = RepositoryId::new();
    let scope = ActivationScope::Enabled {
        repository_ids: vec![repository_id.clone(), sibling_id.clone()],
    };
    let serialized = serde_json::to_value(&scope).unwrap();
    let object = serialized.as_object().unwrap();
    assert_eq!(
        object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["kind", "repository_ids"])
    );
    assert_eq!(object["kind"], "enabled");
    assert_eq!(scope.repository_ids().len(), 2);
    assert!(scope.is_enabled());

    let disabled = serde_json::to_value(ActivationScope::Disabled).unwrap();
    assert_eq!(
        disabled.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["kind"]
    );
    assert_eq!(disabled["kind"], "disabled");
    assert!(ActivationScope::Disabled.repository_ids().is_empty());

    // No location, and no boundary the decision must never cross, appears anywhere in it.
    let text = serde_json::to_string(&scope).unwrap();
    for forbidden in [
        "checkout_path",
        "root_path",
        "repository_group",
        "startup_cwd",
        "prompt",
        "mcp_response",
        "report",
        "durable_context",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} must not be present");
    }
}

#[test]
fn activation_scope_resolution_succeeds_when_git_is_unavailable() {
    let empty_path = TempDir::new().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "activation_scope_without_git_child",
            "--nocapture",
        ])
        .env("PATH", empty_path.path())
        .env("SCTX_ACTIVATION_NO_GIT_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "no-Git child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn activation_scope_without_git_child() {
    if std::env::var_os("SCTX_ACTIVATION_NO_GIT_CHILD").is_none() {
        return;
    }
    assert_eq!(
        Command::new("git")
            .arg("--version")
            .status()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );

    let temporary = TempDir::new().unwrap();
    let parent = temporary.path().join("parent");
    let checkout = parent.join("member");
    let direct_cwd = checkout.join("src");
    let sibling = parent.join("unregistered sibling");
    for directory in [&direct_cwd, &sibling] {
        fs::create_dir_all(directory).unwrap();
    }
    let parent = fs::canonicalize(parent).unwrap();
    let checkout = fs::canonicalize(checkout).unwrap();
    let direct_cwd = fs::canonicalize(direct_cwd).unwrap();
    let sibling = fs::canonicalize(sibling).unwrap();
    let repository_id = RepositoryId::new();
    let catalog = RepositoryCatalogSnapshot {
        repositories: vec![RepositoryCatalogEntry {
            repository_id: repository_id.clone(),
            checkout_paths: vec![checkout],
        }],
        ..RepositoryCatalogSnapshot::default()
    };

    let expected = ActivationScope::Enabled {
        repository_ids: vec![repository_id],
    };
    assert_eq!(
        catalog.resolve_activation_scope(&direct_cwd).unwrap(),
        expected
    );
    assert_eq!(catalog.resolve_activation_scope(&parent).unwrap(), expected);
    assert_eq!(
        catalog.resolve_activation_scope(&sibling).unwrap(),
        ActivationScope::Disabled
    );
}

#[test]
fn pure_longest_prefix_resolution_meets_the_hot_path_budget() {
    let temporary = TempDir::new().unwrap();
    let cross = temporary.path().join("benchmark cross");
    let repositories = (0..12)
        .map(|index| init_repo(&cross.join(format!("team-{index}/repo")), "benchmark"))
        .collect::<Vec<_>>();
    let state_root = temporary.path().join("benchmark state");
    let config = UserConfigStore::initialize(&state_root).unwrap();
    for repository in &repositories {
        config
            .add_repository(RepositoryId::new(), std::slice::from_ref(repository))
            .unwrap();
    }
    let catalog = config.repository_catalog().unwrap();
    let workspace = fs::canonicalize(&cross).unwrap();
    let file = repositories[7].join("src/search/Search.kt");
    let mut micros = Vec::with_capacity(5_000);
    for _ in 0..5_000 {
        let started = Instant::now();
        let resolved = catalog
            .resolve_canonical_file(&file, std::slice::from_ref(&workspace))
            .unwrap();
        assert_eq!(resolved.relative_path.as_str(), "src/search/Search.kt");
        micros.push(started.elapsed().as_micros());
    }
    micros.sort_unstable();
    let p95 = micros[micros.len() * 95 / 100];
    let p99 = micros[micros.len() * 99 / 100];
    eprintln!("Repository resolver p95={p95}us p99={p99}us");
    assert!(p95 < 10_000, "Repository resolver p95 {p95}us >= 10ms");
    assert!(p99 < 25_000, "Repository resolver p99 {p99}us >= 25ms");

    let mut hook_mapping_micros = Vec::with_capacity(2_000);
    for _ in 0..2_000 {
        let started = Instant::now();
        let catalog = UserConfigStore::open_existing(&state_root)
            .unwrap()
            .repository_catalog()
            .unwrap();
        let resolved = catalog
            .resolve_file_path(&file, std::slice::from_ref(&workspace))
            .unwrap();
        assert_eq!(resolved.relative_path.as_str(), "src/search/Search.kt");
        hook_mapping_micros.push(started.elapsed().as_micros());
    }
    hook_mapping_micros.sort_unstable();
    let hook_p95 = hook_mapping_micros[hook_mapping_micros.len() * 95 / 100];
    let hook_p99 = hook_mapping_micros[hook_mapping_micros.len() * 99 / 100];
    eprintln!("Hook Repository mapping p95={hook_p95}us p99={hook_p99}us");
    assert!(
        hook_p95 < 10_000,
        "Hook Repository mapping p95 {hook_p95}us >= 10ms"
    );
    assert!(
        hook_p99 < 25_000,
        "Hook Repository mapping p99 {hook_p99}us >= 25ms"
    );
}

#[test]
fn hook_switches_default_to_off_and_survive_an_explicit_catalog_write() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let repository = init_repo(&temporary.path().join("switch repo"), "switch");
    let store = UserConfigStore::initialize(&root).unwrap();

    let (catalog, hooks) = store.repository_catalog_with_hooks().unwrap();
    assert!(catalog.repositories.is_empty());
    assert!(
        !hooks.artifact_focus_reminder,
        "an absent [hooks] table means every switch is off"
    );

    let config_path = root.join("config.toml");
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[hooks]\nartifact_focus_reminder = true\n");
    fs::write(&config_path, text).unwrap();
    assert!(
        store
            .repository_catalog_with_hooks()
            .unwrap()
            .1
            .artifact_focus_reminder
    );

    // A later explicit Catalog write must round-trip the switch, not drop it.
    store
        .add_repository(
            "FE".parse::<RepositoryId>().unwrap(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    let (catalog, hooks) = store.repository_catalog_with_hooks().unwrap();
    assert_eq!(catalog.repositories.len(), 1);
    assert!(hooks.artifact_focus_reminder);
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("[hooks]")
    );

    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("unknown_switch = true\n");
    fs::write(&config_path, text).unwrap();
    assert_eq!(
        store.repository_catalog_with_hooks().unwrap_err().kind(),
        ErrorKind::InvalidInput,
        "an undocumented [hooks] key is refused instead of silently ignored"
    );
}

#[test]
fn engineering_auto_scan_defaults_to_on_and_is_explicitly_switchable() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let repository = init_repo(&temporary.path().join("engineering repo"), "engineering");
    let store = UserConfigStore::initialize(&root).unwrap();

    assert!(
        store.engineering_settings().unwrap().auto_scan,
        "an absent [engineering] table keeps the Graph attached to new knowledge"
    );

    let config_path = root.join("config.toml");
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[engineering]\nauto_scan = false\n");
    fs::write(&config_path, text).unwrap();
    assert!(!store.engineering_settings().unwrap().auto_scan);

    // A later explicit Catalog write must round-trip the switch, not drop it.
    store
        .add_repository(
            "FE".parse::<RepositoryId>().unwrap(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    assert!(!store.engineering_settings().unwrap().auto_scan);
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("[engineering]")
    );

    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("unknown_switch = true\n");
    fs::write(&config_path, text).unwrap();
    assert_eq!(
        store.engineering_settings().unwrap_err().kind(),
        ErrorKind::InvalidInput,
        "an undocumented [engineering] key is refused instead of silently ignored"
    );
}

#[test]
fn the_encode_budget_is_optional_bounded_and_survives_a_model_reinstall() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("预算 配置");
    let store = UserConfigStore::initialize(&root).unwrap();
    let config_path = root.join("config.toml");
    let model = temporary.path().join("模型 目录");
    let runtime = temporary.path().join("libonnxruntime.dylib");

    store.set_retrieval_embedding(&model, &runtime).unwrap();
    let default = store.retrieval_settings().unwrap();
    assert_eq!(
        default.embedding_encode_budget_ms, None,
        "an installation that never needed a budget must not grow a key it did not write"
    );
    assert_eq!(
        default.encode_budget(),
        None,
        "absent means the compiled-in default, not zero"
    );
    assert!(
        !fs::read_to_string(&config_path)
            .unwrap()
            .contains("embedding_encode_budget_ms"),
        "the default must stay out of the document so it can change without a migration"
    );

    let mut text = fs::read_to_string(&config_path).unwrap();
    let _ = writeln!(text, "embedding_encode_budget_ms = 2500");
    fs::write(&config_path, text).unwrap();
    let tuned = store.retrieval_settings().unwrap();
    assert_eq!(tuned.embedding_encode_budget_ms, Some(2_500));
    assert_eq!(
        tuned.encode_budget(),
        Some(std::time::Duration::from_millis(2_500))
    );

    // Reinstalling the model rewrites both paths. A budget the operator measured for this machine
    // is not part of that, and losing it would silently restore the failure they tuned it away.
    store.set_retrieval_embedding(&model, &runtime).unwrap();
    assert_eq!(
        store
            .retrieval_settings()
            .unwrap()
            .embedding_encode_budget_ms,
        Some(2_500),
        "`sctx embedding install` must not discard a tuned encode budget"
    );

    for refused in ["0", "10", "60000"] {
        let mut text = fs::read_to_string(&config_path).unwrap();
        text = text.replace(
            "embedding_encode_budget_ms = 2500",
            &format!("embedding_encode_budget_ms = {refused}"),
        );
        fs::write(&config_path, text).unwrap();
        assert_eq!(
            store.retrieval_settings().unwrap_err().kind(),
            ErrorKind::InvalidInput,
            "a budget of {refused} ms is refused rather than clamped, so a typo is a message"
        );
        let text = fs::read_to_string(&config_path).unwrap().replace(
            &format!("embedding_encode_budget_ms = {refused}"),
            "embedding_encode_budget_ms = 2500",
        );
        fs::write(&config_path, text).unwrap();
    }
}

#[test]
fn retrieval_embedding_paths_default_to_absent_and_survive_a_catalog_write() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let repository = init_repo(&temporary.path().join("retrieval repo"), "retrieval");
    let store = UserConfigStore::initialize(&root).unwrap();

    let default = store.retrieval_settings().unwrap();
    assert_eq!(default.embedding_model_path, None);
    assert_eq!(default.embedding_runtime_path, None);
    assert!(
        !default.embedding_enabled(),
        "an absent [retrieval] table means the embedding channel does not exist"
    );
    assert!(!default.embedding_half_configured());

    let config_path = root.join("config.toml");
    let model = temporary.path().join("模型 目录");
    let runtime = temporary.path().join("libonnxruntime.dylib");

    // Half a configuration cannot run a model, and is reported as the mistake it is.
    let mut text = fs::read_to_string(&config_path).unwrap();
    let _ = write!(
        text,
        "\n[retrieval]\nembedding_model_path = {}\n",
        toml_string(&model)
    );
    fs::write(&config_path, text).unwrap();
    let half = store.retrieval_settings().unwrap();
    assert!(!half.embedding_enabled());
    assert!(half.embedding_half_configured());

    let mut text = fs::read_to_string(&config_path).unwrap();
    let _ = writeln!(text, "embedding_runtime_path = {}", toml_string(&runtime));
    fs::write(&config_path, text).unwrap();
    let both = store.retrieval_settings().unwrap();
    assert_eq!(both.embedding_model_path.as_deref(), Some(model.as_path()));
    assert_eq!(
        both.embedding_runtime_path.as_deref(),
        Some(runtime.as_path())
    );
    assert!(both.embedding_enabled());

    // A later explicit Catalog write must round-trip the table, not drop it.
    store
        .add_repository(
            "RT".parse::<RepositoryId>().unwrap(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    assert!(store.retrieval_settings().unwrap().embedding_enabled());
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("[retrieval]")
    );

    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("unknown_switch = true\n");
    fs::write(&config_path, text).unwrap();
    assert_eq!(
        store.retrieval_settings().unwrap_err().kind(),
        ErrorKind::InvalidInput,
        "an undocumented [retrieval] key is refused instead of silently ignored"
    );
}

#[test]
fn a_relative_retrieval_path_is_refused_rather_than_resolved() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let store = UserConfigStore::initialize(&root).unwrap();
    let config_path = root.join("config.toml");

    // The MCP server, the CLI and the Hooks all run from different working directories, so there
    // is no directory a relative path could honestly be resolved against.
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[retrieval]\nembedding_model_path = \"models/bge-m3\"\n");
    fs::write(&config_path, text).unwrap();
    assert_eq!(
        store.retrieval_settings().unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn setting_retrieval_embedding_leaves_every_other_table_exactly_as_it_was() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let repository = init_repo(&temporary.path().join("retrieval repo"), "retrieval");
    let store = UserConfigStore::initialize(&root).unwrap();
    let config_path = root.join("config.toml");

    // Everything an installation can legitimately carry, so the write has something to lose.
    store
        .add_repository(
            "RT".parse::<RepositoryId>().unwrap(),
            std::slice::from_ref(&repository),
        )
        .unwrap();
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str(
        "\n[hooks]\nartifact_focus_reminder = true\n\n[context_ttl]\nvalidation = \"30d\"\n\
         progress = \"14d\"\n\n[activation]\nallow_home = true\n\n[engineering]\n\
         auto_scan = false\n",
    );
    fs::write(&config_path, text).unwrap();
    let before = store.repository_catalog_with_context_ttl().unwrap();
    let hooks_before = store.repository_catalog_with_hooks().unwrap().1;
    let engineering_before = store.engineering_settings().unwrap();

    let model = temporary.path().join("模型 目录");
    let runtime = temporary.path().join("libonnxruntime.dylib");
    let written = store.set_retrieval_embedding(&model, &runtime).unwrap();

    assert_eq!(
        written.embedding_model_path.as_deref(),
        Some(model.as_path())
    );
    assert_eq!(
        written.embedding_runtime_path.as_deref(),
        Some(runtime.as_path())
    );
    assert!(written.embedding_enabled());
    assert_eq!(store.retrieval_settings().unwrap(), written);
    // The point of the test: a write aimed at one table is not a rewrite of the document.
    assert_eq!(store.repository_catalog_with_context_ttl().unwrap(), before);
    assert_eq!(
        store.repository_catalog_with_hooks().unwrap().1,
        hooks_before
    );
    assert_eq!(store.engineering_settings().unwrap(), engineering_before);

    let text = fs::read_to_string(&config_path).unwrap();
    for table in ["[hooks]", "[context_ttl]", "[activation]", "[engineering]"] {
        assert!(text.contains(table), "{table} disappeared from\n{text}");
    }
}

#[test]
fn clearing_retrieval_embedding_restores_the_document_of_an_installation_that_never_had_it() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let store = UserConfigStore::initialize(&root).unwrap();
    let config_path = root.join("config.toml");
    let pristine = fs::read_to_string(&config_path).unwrap();

    assert!(
        !store.clear_retrieval_embedding().unwrap(),
        "clearing an absent [retrieval] reports that there was nothing to clear"
    );
    assert_eq!(fs::read_to_string(&config_path).unwrap(), pristine);

    store
        .set_retrieval_embedding(
            &temporary.path().join("模型 目录"),
            &temporary.path().join("libonnxruntime.dylib"),
        )
        .unwrap();
    assert!(store.clear_retrieval_embedding().unwrap());
    assert_eq!(
        store.retrieval_settings().unwrap(),
        RetrievalSettings::default()
    );
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        pristine,
        "removal restores the exact document shape, not an empty [retrieval] table"
    );
}

#[test]
fn a_relative_retrieval_path_is_refused_by_the_writer_too() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("共享 配置");
    let store = UserConfigStore::initialize(&root).unwrap();
    let config_path = root.join("config.toml");
    let pristine = fs::read_to_string(&config_path).unwrap();
    let absolute = temporary.path().join("libonnxruntime.dylib");

    // A writer that accepted these would produce a document its own reader rejects.
    for (model, runtime) in [
        (Path::new("models/bge-m3"), absolute.as_path()),
        (temporary.path(), Path::new("lib/libonnxruntime.dylib")),
    ] {
        let error = store.set_retrieval_embedding(model, runtime).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("absolute"), "{error}");
    }
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        pristine,
        "a refused write must not have touched the document"
    );
}

/// One TOML basic string, with the escaping a quoted or Windows-style path would need.
fn toml_string(path: &std::path::Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}
