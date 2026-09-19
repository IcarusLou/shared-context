use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::{RepositoryId, Result as SctxResult};
use sctx_engineering_graph::{CatalogRepositorySpec, RepositoryAvailability, RepositoryRegistry};
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

fn init_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["config", "user.name", "Registry Test"]);
    git(path, &["config", "user.email", "registry@example.invalid"]);
    fs::write(path.join("README.md"), "fixture\n").unwrap();
    git(path, &["add", "--", "."]);
    git(path, &["commit", "-q", "-m", "fixture"]);
    fs::canonicalize(path).unwrap()
}

fn spec(repository_id: &RepositoryId, paths: &[PathBuf]) -> CatalogRepositorySpec {
    CatalogRepositorySpec {
        repository_id: repository_id.clone(),
        checkout_paths: paths.to_vec(),
    }
}

fn remove_database(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove Registry database: {error}"),
        }
    }
}

#[test]
fn explicit_catalog_identity_owns_zero_or_many_checkouts_without_git_inference() {
    let temporary = TempDir::new().unwrap();
    let primary = init_repo(&temporary.path().join("primary"));
    let worktree = temporary.path().join("worktree");
    git(
        &primary,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "registry-worktree",
            worktree.to_str().unwrap(),
        ],
    );
    let worktree = fs::canonicalize(worktree).unwrap();
    let first_id = RepositoryId::new();
    let second_id = RepositoryId::new();
    let empty_id = RepositoryId::new();
    let registry = RepositoryRegistry::initialize(temporary.path().join("state")).unwrap();

    let report = registry
        .sync_catalog(&[
            spec(&first_id, std::slice::from_ref(&primary)),
            spec(&second_id, std::slice::from_ref(&worktree)),
            spec(&empty_id, &[]),
        ])
        .unwrap();
    assert_eq!(report.repository_count, 3);
    assert_eq!(report.locator_count, 2);
    assert_ne!(
        first_id, second_id,
        "Git common-dir must not merge Catalog IDs"
    );
    assert_eq!(
        registry
            .resolve_by_checkout_path(&primary)
            .unwrap()
            .unwrap()
            .identity
            .repository_id,
        first_id
    );
    assert_eq!(
        registry
            .resolve_by_checkout_path(&worktree)
            .unwrap()
            .unwrap()
            .identity
            .repository_id,
        second_id
    );
    let empty = registry.resolve_by_id(&empty_id).unwrap().unwrap();
    assert_eq!(empty.availability, RepositoryAvailability::Unavailable);
    assert!(empty.locators.is_empty());
}

#[test]
fn deleting_registry_and_syncing_catalog_restores_exact_ids_and_locators() {
    let temporary = TempDir::new().unwrap();
    let first = init_repo(&temporary.path().join("first/repo"));
    let second = init_repo(&temporary.path().join("second/repo"));
    let first_id = RepositoryId::new();
    let second_id = RepositoryId::new();
    let specs = vec![
        spec(&first_id, std::slice::from_ref(&first)),
        spec(&second_id, std::slice::from_ref(&second)),
    ];
    let root = temporary.path().join("registry");
    let registry = RepositoryRegistry::initialize(&root).unwrap();
    registry.sync_catalog(&specs).unwrap();
    let expected = registry.list().unwrap();
    let database = registry.database_path().to_path_buf();
    drop(registry);
    remove_database(&database);

    let rebuilt = RepositoryRegistry::initialize(&root).unwrap();
    rebuilt.sync_catalog(&specs).unwrap();
    assert_eq!(rebuilt.list().unwrap(), expected);
}

#[test]
fn missing_paths_restore_as_unavailable_and_invalid_catalog_is_atomic() {
    let temporary = TempDir::new().unwrap();
    let repository = init_repo(&temporary.path().join("repo"));
    let repository_id = RepositoryId::new();
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    registry
        .sync_catalog(&[spec(&repository_id, std::slice::from_ref(&repository))])
        .unwrap();
    let expected = registry.list().unwrap();

    assert!(
        registry
            .sync_catalog(&[
                spec(&repository_id, std::slice::from_ref(&repository)),
                spec(&RepositoryId::new(), std::slice::from_ref(&repository)),
            ])
            .is_err()
    );
    assert_eq!(registry.list().unwrap(), expected);

    fs::remove_dir_all(&repository).unwrap();
    let report = registry
        .sync_catalog(&[spec(&repository_id, std::slice::from_ref(&repository))])
        .unwrap();
    assert_eq!(report.unavailable_locator_count, 1);
    let restored = registry.resolve_by_id(&repository_id).unwrap().unwrap();
    assert_eq!(restored.availability, RepositoryAvailability::Unavailable);
    assert_eq!(restored.locators[0].checkout_path, repository);
}

#[test]
fn concurrent_same_catalog_sync_converges_without_identity_drift() {
    let temporary = TempDir::new().unwrap();
    let repository = init_repo(&temporary.path().join("repo"));
    let repository_id = RepositoryId::new();
    let root = temporary.path().join("registry");
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut threads = Vec::new();
    for _ in 0..workers {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        let repository = repository.clone();
        let repository_id = repository_id.clone();
        threads.push(thread::spawn(move || -> SctxResult<()> {
            // Run every fallible step before `barrier.wait()`: an early
            // `unwrap()`/`?` here would panic ahead of the rendezvous and
            // strand the other workers on the barrier forever, turning a
            // single failure into a silent whole-binary hang. Instead, carry
            // the `Result` across the barrier and only report it afterwards.
            let outcome = RepositoryRegistry::initialize(root).and_then(|registry| {
                registry.sync_catalog(&[spec(&repository_id, &[repository])])?;
                Ok(())
            });
            barrier.wait();
            outcome
        }));
    }
    for worker in threads {
        worker.join().unwrap().unwrap();
    }
    let repositories = RepositoryRegistry::initialize(root)
        .unwrap()
        .list()
        .unwrap();
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].identity.repository_id, repository_id);
    assert_eq!(repositories[0].locators.len(), 1);
}

/// Stress-tests `RepositoryRegistry::initialize` under heavy concurrent
/// contention on a brand-new database, where `PRAGMA journal_mode=WAL`
/// previously raced ahead of `busy_timeout` and could fail with
/// `SQLITE_BUSY`/"database is locked". Kept small (8 threads, few rounds)
/// so it stays fast enough to run by default; not `#[ignore]`d because the
/// whole point is to catch the regression in normal CI runs.
#[test]
fn concurrent_initialize_of_new_registry_never_reports_database_locked() {
    for _round in 0..4 {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("registry");
        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut threads = Vec::new();
        for _ in 0..workers {
            let barrier = Arc::clone(&barrier);
            let root = root.clone();
            threads.push(thread::spawn(move || -> SctxResult<()> {
                barrier.wait();
                RepositoryRegistry::initialize(&root)?;
                Ok(())
            }));
        }
        for worker in threads {
            worker
                .join()
                .unwrap()
                .expect("concurrent initialize of a new Registry must not report database locked");
        }
    }
}

#[test]
fn symlink_subdirectory_relative_and_non_git_catalog_paths_are_rejected() {
    let temporary = TempDir::new().unwrap();
    let repository = init_repo(&temporary.path().join("repo"));
    fs::create_dir(repository.join("nested")).unwrap();
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    assert!(
        registry
            .sync_catalog(&[spec(
                &RepositoryId::new(),
                &[PathBuf::from("relative/repo")],
            )])
            .is_err()
    );
    assert!(
        registry
            .sync_catalog(&[spec(&RepositoryId::new(), &[repository.join("nested")],)])
            .is_err()
    );
    let non_git = temporary.path().join("not git");
    fs::create_dir(&non_git).unwrap();
    let non_git = fs::canonicalize(non_git).unwrap();
    assert!(
        registry
            .sync_catalog(&[spec(&RepositoryId::new(), &[non_git])])
            .is_err()
    );
    #[cfg(unix)]
    {
        let link = temporary.path().join("repo-link");
        std::os::unix::fs::symlink(&repository, &link).unwrap();
        assert!(
            registry
                .sync_catalog(&[spec(&RepositoryId::new(), &[link])])
                .is_err()
        );
    }
}

#[test]
fn deleting_registry_never_touches_context_or_runtime_state() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("shared-context");
    let registry = RepositoryRegistry::initialize(&root).unwrap();
    fs::write(root.join("state/index.sqlite"), b"context-index").unwrap();
    fs::write(root.join("state/runtime.sqlite"), b"task-runtime").unwrap();
    let database = registry.database_path().to_path_buf();
    drop(registry);
    remove_database(&database);
    assert_eq!(
        fs::read(root.join("state/index.sqlite")).unwrap(),
        b"context-index"
    );
    assert_eq!(
        fs::read(root.join("state/runtime.sqlite")).unwrap(),
        b"task-runtime"
    );
}

/// The Catalog projection is rewritten only when the Catalog or a checkout's
/// local state actually changed. An unchanged Catalog must reach exactly the
/// same report without touching the Registry, and every change that can flip an
/// inspection outcome — a new identity, a new path, a checkout disappearing, a
/// checkout returning — must still rewrite it.
#[test]
fn unchanged_catalog_is_reprojected_only_when_its_content_or_checkout_state_changes() {
    let temporary = TempDir::new().unwrap();
    let repository = init_repo(&temporary.path().join("repo"));
    let repository_id = RepositoryId::new();
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    let catalog = [spec(&repository_id, std::slice::from_ref(&repository))];
    let first = registry.sync_catalog(&catalog).unwrap();
    assert_eq!(first.repository_count, 1);
    assert_eq!(first.locator_count, 1);
    assert_eq!(first.unavailable_locator_count, 0);
    let projected = registry.list().unwrap();

    // A skipped synchronization is observable exactly where it matters: rows the
    // Registry did not rewrite stay as they are, and the report is unchanged.
    let connection = rusqlite::Connection::open(registry.database_path()).unwrap();
    connection
        .execute("DELETE FROM repository_locator", [])
        .unwrap();
    drop(connection);
    assert_eq!(registry.sync_catalog(&catalog).unwrap(), first);
    assert!(registry.list().unwrap()[0].locators.is_empty());

    // A second identity is different Catalog content, so the projection is rebuilt.
    let other = init_repo(&temporary.path().join("other"));
    let other_id = RepositoryId::new();
    let widened = [
        spec(&repository_id, std::slice::from_ref(&repository)),
        spec(&other_id, std::slice::from_ref(&other)),
    ];
    let second = registry.sync_catalog(&widened).unwrap();
    assert_eq!(second.repository_count, 2);
    assert_eq!(second.locator_count, 2);
    assert_eq!(registry.list().unwrap().len(), 2);

    // Returning to the first Catalog is again different content.
    assert_eq!(registry.sync_catalog(&catalog).unwrap(), first);
    assert_eq!(registry.list().unwrap(), projected);

    // Local checkout state is part of the fingerprint in both directions.
    fs::remove_dir_all(&repository).unwrap();
    let unavailable = registry.sync_catalog(&catalog).unwrap();
    assert_eq!(unavailable.unavailable_locator_count, 1);
    assert_eq!(
        registry
            .resolve_by_id(&repository_id)
            .unwrap()
            .unwrap()
            .availability,
        RepositoryAvailability::Unavailable
    );
    init_repo(&repository);
    assert_eq!(registry.sync_catalog(&catalog).unwrap(), first);
    assert_eq!(
        registry
            .resolve_by_id(&repository_id)
            .unwrap()
            .unwrap()
            .availability,
        RepositoryAvailability::Available
    );

    // A dropped Registry has no fingerprint, so it fully reprojects.
    remove_database(registry.database_path());
    let restored = RepositoryRegistry::initialize(registry.root()).unwrap();
    assert_eq!(restored.sync_catalog(&catalog).unwrap(), first);
    assert_eq!(restored.list().unwrap(), projected);
}

/// A configured checkout that stops being a Git worktree root must still be
/// rejected, even though the previous synchronization accepted the same path.
#[test]
fn checkout_that_stops_being_a_worktree_root_is_rejected_after_a_successful_sync() {
    let temporary = TempDir::new().unwrap();
    let repository = init_repo(&temporary.path().join("repo"));
    let repository_id = RepositoryId::new();
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    let catalog = [spec(&repository_id, std::slice::from_ref(&repository))];
    registry.sync_catalog(&catalog).unwrap();

    fs::remove_dir_all(repository.join(".git")).unwrap();
    assert!(registry.sync_catalog(&catalog).is_err());
}
