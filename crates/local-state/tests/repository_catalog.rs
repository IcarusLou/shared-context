use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use sctx_domain::{ArtifactLocator, ErrorKind, RepositoryGroupId, RepositoryId};
use sctx_local_state::{
    ActivationScope, ActivationScopeDecision, CatalogCheckoutStatus, CatalogRepositoryGroupStatus,
    RepositoryCatalogEntry, RepositoryCatalogSnapshot, RepositoryGroupCatalogEntry,
    UserConfigStore,
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
fn catalog_assigns_typed_ids_atomically_and_supports_explicit_worktrees() {
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

    let created = config
        .add_repository(None, std::slice::from_ref(&repository))
        .unwrap();
    assert!(created.created_identity);
    assert!(
        created
            .repository
            .repository_id
            .to_string()
            .starts_with("Repository-")
    );
    let extended = config
        .add_repository(
            Some(created.repository.repository_id.clone()),
            std::slice::from_ref(&worktree),
        )
        .unwrap();
    assert!(!extended.created_identity);
    assert_eq!(extended.added_paths, 1);
    assert_eq!(
        extended.repository.checkout_paths,
        vec![worktree, repository]
    );
    let document = fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(document.contains("[[repositories]]"));
    assert!(document.contains(&format!("id = \"{}\"", created.repository.repository_id)));
    assert!(!document.contains("repository_groups"));
    assert!(!document.contains("space"));
    assert!(!document.contains("workspace"));
    assert!(
        UserConfigStore::open_existing(&root)
            .unwrap()
            .repository_catalog()
            .unwrap()
            .repository_groups
            .is_empty(),
        "configuration written before RepositoryGroups remains readable with an empty default"
    );
    let metadata = fs::metadata(root.join("config.toml")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
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
        .add_repository(None, std::slice::from_ref(&fe))
        .unwrap()
        .repository
        .repository_id;
    let android_id = config
        .add_repository(None, std::slice::from_ref(&android))
        .unwrap()
        .repository
        .repository_id;
    let ios_id = config
        .add_repository(None, std::slice::from_ref(&ios))
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
        .add_repository(None, std::slice::from_ref(&repository))
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
                .add_repository(None, &[repository])
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
fn failed_config_write_releases_exclusive_lock_before_immediate_catalog_read() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let temporary = TempDir::new().unwrap();
    let state_root = temporary.path().join("write failure state");
    let first = init_repo(&temporary.path().join("first repo"), "first");
    let second = init_repo(&temporary.path().join("second repo"), "second");
    let config = UserConfigStore::initialize(&state_root).unwrap();
    let first_id = config
        .add_repository(None, std::slice::from_ref(&first))
        .unwrap()
        .repository
        .repository_id;

    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o500)).unwrap();
    let failed_write = config.add_repository(None, std::slice::from_ref(&second));
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
fn activation_scope_is_direct_group_or_disabled_only_at_explicit_boundaries() {
    let temporary = TempDir::new().unwrap();
    let cross = temporary.path().join("activation cross");
    let android = init_repo(&cross.join("android/TikTok"), "android");
    let ios = init_repo(&cross.join("ios/TikTok"), "ios");
    let sibling = init_repo(&cross.join("unregistered/TikTok"), "sibling");
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
    git(
        &android,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/shared.git",
        ],
    );
    git(
        &sibling,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/shared.git",
        ],
    );

    let config = UserConfigStore::initialize(temporary.path().join("activation state")).unwrap();
    let android_id = config
        .add_repository(None, std::slice::from_ref(&android))
        .unwrap()
        .repository
        .repository_id;
    let ios_id = config
        .add_repository(None, std::slice::from_ref(&ios))
        .unwrap()
        .repository
        .repository_id;
    let outer_id = config
        .add_repository(None, std::slice::from_ref(&outer))
        .unwrap()
        .repository
        .repository_id;
    let nested_id = config
        .add_repository(None, std::slice::from_ref(&nested))
        .unwrap()
        .repository
        .repository_id;
    let cross = fs::canonicalize(cross).unwrap();
    let cross_group = config
        .add_repository_group(&cross, &[ios_id.clone(), android_id.clone()])
        .unwrap()
        .repository_group;
    config
        .add_repository_group(&outer, std::slice::from_ref(&nested_id))
        .unwrap();

    let catalog = config.repository_catalog().unwrap();
    let direct = catalog
        .resolve_activation_scope(&android.join("src/search"))
        .unwrap();
    assert!(matches!(
        direct.decision,
        ActivationScopeDecision::Direct {
            repository_id,
            ref checkout_path,
        } if repository_id == android_id && checkout_path == &android
    ));
    assert_eq!(direct.allowed_repository_ids, vec![android_id.clone()]);

    let nested_direct = catalog
        .resolve_activation_scope(&nested.join("src/search"))
        .unwrap();
    assert!(matches!(
        nested_direct.decision,
        ActivationScopeDecision::Direct {
            repository_id,
            ref checkout_path,
        } if repository_id == nested_id && checkout_path == &nested
    ));
    assert_eq!(
        nested_direct.allowed_repository_ids,
        vec![nested_id.clone()]
    );

    let direct_precedes_exact_group = catalog.resolve_activation_scope(&outer).unwrap();
    assert!(matches!(
        direct_precedes_exact_group.decision,
        ActivationScopeDecision::Direct { repository_id, .. }
            if repository_id == outer_id
    ));
    assert_eq!(
        direct_precedes_exact_group.allowed_repository_ids,
        vec![outer_id.clone()]
    );

    let group = catalog.resolve_activation_scope(&cross).unwrap();
    assert!(matches!(
        group.decision,
        ActivationScopeDecision::Group {
            repository_group_id,
            ref root_path,
        } if repository_group_id == cross_group.repository_group_id && root_path == &cross
    ));
    let expected_members = [android_id.clone(), ios_id.clone()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        group
            .allowed_repository_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        expected_members
    );
    assert!(!group.allowed_repository_ids.contains(&outer_id));
    assert!(!group.allowed_repository_ids.contains(&nested_id));

    for disabled_path in [
        fs::canonicalize(temporary.path()).unwrap(),
        fs::canonicalize(cross.join("android")).unwrap(),
        sibling,
        common_dir_sibling,
    ] {
        let disabled = catalog.resolve_activation_scope(&disabled_path).unwrap();
        assert_eq!(disabled.decision, ActivationScopeDecision::Disabled);
        assert!(disabled.allowed_repository_ids.is_empty());
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn repository_groups_reject_unsafe_roots_unknown_or_ineligible_members_and_drift() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("group root");
    let member = init_repo(&root.join("member-a"), "member-a");
    let second_member = init_repo(&root.join("member-b"), "member-b");
    let outside = init_repo(&temporary.path().join("outside"), "outside");
    let root = fs::canonicalize(root).unwrap();
    let config = UserConfigStore::initialize(temporary.path().join("group state")).unwrap();
    let member_id = config
        .add_repository(None, std::slice::from_ref(&member))
        .unwrap()
        .repository
        .repository_id;
    let second_member_id = config
        .add_repository(None, std::slice::from_ref(&second_member))
        .unwrap()
        .repository
        .repository_id;
    let outside_id = config
        .add_repository(None, std::slice::from_ref(&outside))
        .unwrap()
        .repository
        .repository_id;

    assert_eq!(
        config.add_repository_group(&root, &[]).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        config
            .add_repository_group(&root, &[member_id.clone(), member_id.clone()])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        config
            .add_repository_group(&root, &[RepositoryId::new()])
            .unwrap_err()
            .kind(),
        ErrorKind::RepositoryNotConfigured
    );
    assert_eq!(
        config
            .add_repository_group(&root, &[outside_id])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        config
            .add_repository_group(&member, std::slice::from_ref(&member_id))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput,
        "a member checkout must be strictly below, not equal to, the Group root"
    );
    assert_eq!(
        config
            .add_repository_group(&root.join("member-a/.."), std::slice::from_ref(&member_id))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    let file_root = root.join("not-a-directory");
    fs::write(&file_root, "fixture").unwrap();
    assert_eq!(
        config
            .add_repository_group(&file_root, std::slice::from_ref(&member_id))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    #[cfg(unix)]
    {
        let symlink_root = temporary.path().join("group root link");
        std::os::unix::fs::symlink(&root, &symlink_root).unwrap();
        assert_eq!(
            config
                .add_repository_group(&symlink_root, std::slice::from_ref(&member_id))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    let created = config
        .add_repository_group(&root, std::slice::from_ref(&member_id))
        .unwrap();
    assert!(created.created);
    assert!(
        created
            .repository_group
            .repository_group_id
            .to_string()
            .starts_with("rpg_")
    );
    let retry = config
        .add_repository_group(&root, std::slice::from_ref(&member_id))
        .unwrap();
    assert!(!retry.created);
    assert_eq!(
        retry.repository_group.repository_group_id,
        created.repository_group.repository_group_id
    );
    assert_eq!(
        config
            .add_repository_group(&root, &[member_id.clone(), second_member_id])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput,
        "one root cannot identify two different RepositoryGroups"
    );

    let snapshot = config.repository_catalog().unwrap();
    fs::rename(&member, root.join("moved-member")).unwrap();
    let cached_scope = snapshot.resolve_activation_scope(&root).unwrap();
    assert!(matches!(
        cached_scope.decision,
        ActivationScopeDecision::Group { .. }
    ));
    let refreshed_scope = config.resolve_activation_scope(&root).unwrap();
    assert!(matches!(
        refreshed_scope.decision,
        ActivationScopeDecision::Group { .. }
    ));
    assert_eq!(
        config
            .doctor_repository_catalog()
            .unwrap()
            .checkouts
            .iter()
            .filter(|checkout| checkout.status == CatalogCheckoutStatus::Missing)
            .count(),
        1,
        "SessionStart trusts Catalog while doctor owns checkout health drift"
    );
}

#[test]
#[cfg(unix)]
fn configured_repository_group_root_error_names_identity_path_and_reason() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("drifting group root");
    let member = init_repo(&root.join("member"), "member");
    let root = fs::canonicalize(root).unwrap();
    let config = UserConfigStore::initialize(temporary.path().join("drift state")).unwrap();
    let member_id = config
        .add_repository(None, std::slice::from_ref(&member))
        .unwrap()
        .repository
        .repository_id;
    let group = config
        .add_repository_group(&root, &[member_id])
        .unwrap()
        .repository_group;

    let moved_root = temporary.path().join("moved group root");
    fs::rename(&root, &moved_root).unwrap();
    std::os::unix::fs::symlink(&moved_root, &root).unwrap();

    let error = config.repository_catalog().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(
        error
            .message()
            .contains(&group.repository_group_id.to_string())
    );
    assert!(error.message().contains(root.to_str().unwrap()));
    assert!(error.message().contains("must not be a symlink"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn repository_group_inspection_update_and_remove_repair_drift_without_touching_repositories() {
    let temporary = TempDir::new().unwrap();
    let original_root = temporary.path().join("original group");
    let first = init_repo(&original_root.join("first"), "first");
    let second = init_repo(&original_root.join("second"), "second");
    let original_root = fs::canonicalize(original_root).unwrap();
    let replacement_root = temporary.path().join("replacement group");
    let replacement_first = init_repo(&replacement_root.join("first"), "replacement-first");
    let replacement_second = init_repo(&replacement_root.join("second"), "replacement-second");
    let replacement_root = fs::canonicalize(replacement_root).unwrap();
    let config = UserConfigStore::initialize(temporary.path().join("repair state")).unwrap();
    let first_id = config
        .add_repository(None, std::slice::from_ref(&first))
        .unwrap()
        .repository
        .repository_id;
    config
        .add_repository(
            Some(first_id.clone()),
            std::slice::from_ref(&replacement_first),
        )
        .unwrap();
    let second_id = config
        .add_repository(None, std::slice::from_ref(&second))
        .unwrap()
        .repository
        .repository_id;
    config
        .add_repository(
            Some(second_id.clone()),
            std::slice::from_ref(&replacement_second),
        )
        .unwrap();
    let group = config
        .add_repository_group(&original_root, std::slice::from_ref(&first_id))
        .unwrap()
        .repository_group;

    let moved_original = temporary.path().join("moved original group");
    fs::rename(&original_root, &moved_original).unwrap();
    assert!(config.repository_catalog().is_err());
    let inspection = config.inspect_repository_catalog().unwrap();
    assert_eq!(inspection.catalog.repositories.len(), 2);
    assert_eq!(inspection.repository_groups.len(), 1);
    assert_eq!(
        inspection.repository_groups[0].status,
        CatalogRepositoryGroupStatus::Missing
    );
    assert_eq!(inspection.repository_groups[0].root_path, original_root);
    assert_eq!(
        inspection.repository_groups[0].repository_group_id,
        group.repository_group_id
    );
    let doctor = config.doctor_repository_catalog().unwrap();
    assert!(!doctor.healthy);
    assert_eq!(doctor.repository_group_count, 1);
    assert_eq!(
        doctor.repository_groups[0].status,
        CatalogRepositoryGroupStatus::Missing
    );

    let unknown_member = RepositoryId::new();
    assert_eq!(
        config
            .update_repository_group(
                group.repository_group_id,
                Some(&replacement_root),
                Some(&[unknown_member]),
            )
            .unwrap_err()
            .kind(),
        ErrorKind::RepositoryNotConfigured
    );
    let updated = config
        .update_repository_group(
            group.repository_group_id,
            Some(&replacement_root),
            Some(&[first_id.clone(), second_id.clone()]),
        )
        .unwrap();
    assert!(updated.changed);
    assert_eq!(updated.repository_group.root_path, replacement_root);
    assert_eq!(
        updated
            .repository_group
            .member_repository_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first_id.clone(), second_id.clone()])
    );
    let retry = config
        .update_repository_group(
            group.repository_group_id,
            Some(&replacement_root),
            Some(&[first_id, second_id]),
        )
        .unwrap();
    assert!(!retry.changed);

    let moved_replacement = temporary.path().join("moved replacement group");
    fs::rename(&replacement_root, &moved_replacement).unwrap();
    let removed = config
        .remove_repository_group(group.repository_group_id)
        .unwrap();
    assert!(removed.removed);
    let removal_retry = config
        .remove_repository_group(group.repository_group_id)
        .unwrap();
    assert!(!removal_retry.removed);
    let repaired = config.repository_catalog().unwrap();
    assert!(repaired.repository_groups.is_empty());
    assert_eq!(repaired.repositories.len(), 2);
    assert!(moved_original.exists());
    assert!(moved_replacement.exists());
}

#[test]
fn activation_scope_absolute_paths_are_local_decision_metadata() {
    fn assert_local_metadata_type<T: Clone + std::fmt::Debug + Eq + serde::Serialize>() {}

    assert_local_metadata_type::<ActivationScope>();
    assert_local_metadata_type::<ActivationScopeDecision>();
    assert!(
        std::any::type_name::<ActivationScopeDecision>().starts_with("sctx_local_state::"),
        "ActivationScopeDecision must remain a local-state type, not a durable domain type"
    );

    let temporary = TempDir::new().unwrap();
    let checkout = fs::canonicalize(temporary.path()).unwrap();
    let repository_id = RepositoryId::new();
    let scope = ActivationScope {
        decision: ActivationScopeDecision::Direct {
            repository_id: repository_id.clone(),
            checkout_path: checkout.clone(),
        },
        allowed_repository_ids: vec![repository_id.clone()],
    };
    let serialized = serde_json::to_value(scope).unwrap();
    let top_level = serialized.as_object().unwrap();
    assert_eq!(
        top_level
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["allowed_repository_ids", "decision"])
    );
    let decision = top_level["decision"].as_object().unwrap();
    assert_eq!(
        decision.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["checkout_path", "kind", "repository_id"])
    );
    assert_eq!(decision["checkout_path"], checkout.to_str().unwrap());
    for forbidden_boundary in [
        "activation_hint",
        "prompt",
        "mcp_response",
        "report",
        "durable_context",
    ] {
        assert!(!top_level.contains_key(forbidden_boundary));
        assert!(!decision.contains_key(forbidden_boundary));
    }

    let repository_group_id = RepositoryGroupId::new();
    let group_scope = ActivationScope {
        decision: ActivationScopeDecision::Group {
            repository_group_id,
            root_path: checkout.clone(),
        },
        allowed_repository_ids: vec![repository_id.clone()],
    };
    let serialized_group = serde_json::to_value(group_scope).unwrap();
    let group_decision = serialized_group["decision"].as_object().unwrap();
    assert_eq!(
        group_decision
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["kind", "repository_group_id", "root_path"])
    );
    assert_eq!(group_decision["root_path"], checkout.to_str().unwrap());
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
    let group_root = temporary.path().join("group");
    let checkout = group_root.join("member");
    let direct_cwd = checkout.join("src");
    let sibling = group_root.join("unregistered sibling");
    for directory in [&direct_cwd, &sibling] {
        fs::create_dir_all(directory).unwrap();
    }
    let group_root = fs::canonicalize(group_root).unwrap();
    let checkout = fs::canonicalize(checkout).unwrap();
    let direct_cwd = fs::canonicalize(direct_cwd).unwrap();
    let sibling = fs::canonicalize(sibling).unwrap();
    let repository_id = RepositoryId::new();
    let repository_group_id = RepositoryGroupId::new();
    let catalog = RepositoryCatalogSnapshot {
        repositories: vec![RepositoryCatalogEntry {
            repository_id: repository_id.clone(),
            checkout_paths: vec![checkout.clone()],
        }],
        repository_groups: vec![RepositoryGroupCatalogEntry {
            repository_group_id,
            root_path: group_root.clone(),
            member_repository_ids: vec![repository_id.clone()],
        }],
    };

    let direct = catalog.resolve_activation_scope(&direct_cwd).unwrap();
    assert!(matches!(
        direct.decision,
        ActivationScopeDecision::Direct {
            repository_id: matched,
            ref checkout_path,
        } if matched == repository_id && checkout_path == &checkout
    ));
    let group = catalog.resolve_activation_scope(&group_root).unwrap();
    assert!(matches!(
        group.decision,
        ActivationScopeDecision::Group {
            repository_group_id: matched,
            ..
        } if matched == repository_group_id
    ));
    let disabled = catalog.resolve_activation_scope(&sibling).unwrap();
    assert_eq!(disabled.decision, ActivationScopeDecision::Disabled);
}

#[test]
fn concurrent_repository_group_add_is_atomic_and_semantically_idempotent() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("concurrent group");
    let member = init_repo(&root.join("member"), "member");
    let root = fs::canonicalize(root).unwrap();
    let config = UserConfigStore::initialize(temporary.path().join("concurrent state")).unwrap();
    let member_id = config
        .add_repository(None, std::slice::from_ref(&member))
        .unwrap()
        .repository
        .repository_id;
    let worker_count = 8;
    let barrier = Arc::new(Barrier::new(worker_count));
    let mut workers = Vec::new();
    for _ in 0..worker_count {
        let barrier = Arc::clone(&barrier);
        let config = config.clone();
        let root = root.clone();
        let member_id = member_id.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            config.add_repository_group(&root, &[member_id]).unwrap()
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.created).count(), 1);
    let ids = outcomes
        .iter()
        .map(|outcome| outcome.repository_group.repository_group_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 1);
    let catalog = config.repository_catalog_wait().unwrap();
    assert_eq!(catalog.repository_groups.len(), 1);
    let group = catalog.resolve_activation_scope(&root).unwrap();
    assert!(matches!(
        group.decision,
        ActivationScopeDecision::Group { .. }
    ));
    assert_eq!(group.allowed_repository_ids, vec![member_id]);
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
            .add_repository(None, std::slice::from_ref(repository))
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
