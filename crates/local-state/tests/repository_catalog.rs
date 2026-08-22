use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use sctx_domain::{ArtifactLocator, ErrorKind};
use sctx_local_state::{CatalogCheckoutStatus, UserConfigStore};
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
            .starts_with("rpo_")
    );
    let extended = config
        .add_repository(
            Some(created.repository.repository_id),
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
    assert!(!document.contains("space"));
    assert!(!document.contains("workspace"));
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

    for (repository, expected_id) in [(&fe, fe_id), (&android, android_id), (&ios, ios_id)] {
        let file = repository.join("src/search/Search.kt");
        for workspace in [&cross, repository, &repository.join("src")] {
            let resolved = catalog
                .resolve_file_path(&file, std::slice::from_ref(workspace))
                .unwrap();
            assert_eq!(resolved.repository_id, expected_id);
            assert_eq!(resolved.checkout_path, *repository);
            assert_eq!(resolved.relative_path.as_str(), "src/search/Search.kt");
            let focus = resolved.into_file_focus();
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
