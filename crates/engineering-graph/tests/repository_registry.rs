use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use sctx_domain::ErrorKind;
use sctx_engineering_graph::{
    RegisterRepositoryRequest, RepositoryAvailability, RepositoryLocatorQuery, RepositoryRegistry,
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

fn init_repo(path: &Path, remote: Option<&str>) {
    fs::create_dir_all(path).unwrap();
    let output = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    git(path, &["config", "user.name", "Registry Test"]);
    git(path, &["config", "user.email", "registry@example.invalid"]);
    fs::write(path.join("tracked.txt"), "local registry fixture\n").unwrap();
    git(path, &["add", "--", "tracked.txt"]);
    git(path, &["commit", "-q", "-m", "fixture"]);
    if let Some(remote) = remote {
        git(path, &["remote", "add", "origin", remote]);
    }
}

fn request(path: impl Into<PathBuf>) -> RegisterRepositoryRequest {
    RegisterRepositoryRequest {
        checkout_path: path.into(),
        declared_identity: None,
        remote_hint: None,
    }
}

#[test]
fn linked_worktrees_converge_through_verified_common_dir() {
    let temporary = TempDir::new().unwrap();
    let main = temporary.path().join("主仓库 空格");
    let worktree = temporary.path().join("功能 worktree");
    init_repo(&main, None);
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    let registry = RepositoryRegistry::initialize(temporary.path().join("shared-context")).unwrap();

    let first = registry.register(&request(&main)).unwrap();
    let second = registry.register(&request(&worktree)).unwrap();

    assert!(first.created_identity);
    assert!(!second.created_identity);
    assert_eq!(first.repository.identity, second.repository.identity);
    assert_eq!(second.repository.locators.len(), 2);
    assert_eq!(
        second.repository.locators[0].git_common_dir_identity,
        second.repository.locators[1].git_common_dir_identity
    );
    assert_eq!(registry.list().unwrap(), vec![second.repository]);
}

#[test]
fn different_repositories_with_same_basename_and_remote_never_auto_merge() {
    let temporary = TempDir::new().unwrap();
    let first_path = temporary.path().join("one/repository");
    let second_path = temporary.path().join("two/repository");
    let remote = "ssh://example.invalid/team/repository.git";
    init_repo(&first_path, Some(remote));
    init_repo(&second_path, Some(remote));
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();

    let first = registry.register(&request(&first_path)).unwrap();
    let second = registry.register(&request(&second_path)).unwrap();

    assert_ne!(
        first.repository.identity.repository_id,
        second.repository.identity.repository_id
    );
    let listed = registry.list().unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].identity.repository_id < listed[1].identity.repository_id);
    let error = registry
        .resolve_by_locator(&RepositoryLocatorQuery::RemoteHint(remote.to_owned()))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.message().contains("ambiguous"));
}

#[test]
fn explicit_declared_identity_converges_independent_checkouts() {
    let temporary = TempDir::new().unwrap();
    let first_path = temporary.path().join("checkout-a");
    let second_path = temporary.path().join("checkout-b");
    init_repo(&first_path, None);
    init_repo(&second_path, None);
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    let declared = " Product / Search-Web ";

    let first = registry
        .register(&RegisterRepositoryRequest {
            checkout_path: first_path,
            declared_identity: Some(declared.to_owned()),
            remote_hint: None,
        })
        .unwrap();
    let second = registry
        .register(&RegisterRepositoryRequest {
            checkout_path: second_path,
            declared_identity: Some("product / search-web".to_owned()),
            remote_hint: None,
        })
        .unwrap();

    assert_eq!(first.repository.identity, second.repository.identity);
    assert_eq!(second.repository.locators.len(), 2);
    assert_eq!(
        registry
            .resolve_by_locator(&RepositoryLocatorQuery::DeclaredIdentity(
                declared.to_owned()
            ))
            .unwrap()
            .unwrap()
            .identity,
        first.repository.identity
    );
}

#[test]
fn path_move_and_remote_rename_refresh_hints_without_changing_identity() {
    let temporary = TempDir::new().unwrap();
    let old_path = temporary.path().join("旧路径 repo");
    let new_path = temporary.path().join("新路径 repo");
    init_repo(&old_path, Some("ssh://example.invalid/team/old.git"));
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    let first = registry.register(&request(&old_path)).unwrap();

    fs::rename(&old_path, &new_path).unwrap();
    git(
        &new_path,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://example.invalid/team/renamed.git",
        ],
    );
    let moved = registry.register(&request(&new_path)).unwrap();

    assert_eq!(first.repository.identity, moved.repository.identity);
    assert_eq!(moved.repository.locators.len(), 2);
    assert!(moved.repository.locators.iter().any(|locator| {
        locator.checkout_path != fs::canonicalize(&new_path).unwrap()
            && locator.availability == RepositoryAvailability::Unavailable
    }));
    assert!(moved.repository.locators.iter().any(|locator| {
        locator.checkout_path == fs::canonicalize(&new_path).unwrap()
            && locator.remote_hint.as_deref() == Some("ssh://example.invalid/team/renamed")
    }));
    assert_eq!(
        registry
            .resolve_by_locator(&RepositoryLocatorQuery::CheckoutPath(new_path))
            .unwrap()
            .unwrap()
            .identity,
        first.repository.identity
    );
}

#[test]
fn no_remote_availability_and_forget_preserve_repository_identity() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("没有远端 repo");
    init_repo(&repo, None);
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    let registered = registry.register(&request(&repo)).unwrap();
    let repository_id = registered.repository.identity.repository_id;
    assert!(registered.repository.locators[0].remote_hint.is_none());

    let unavailable = registry.mark_unavailable(repository_id, &repo).unwrap();
    assert_eq!(
        unavailable.availability,
        RepositoryAvailability::Unavailable
    );
    assert_eq!(
        unavailable.locators[0].availability,
        RepositoryAvailability::Unavailable
    );
    let available = registry.observe_available(repository_id, &repo).unwrap();
    assert_eq!(available.availability, RepositoryAvailability::Available);

    let forgotten = registry.forget_local_locator(repository_id, &repo).unwrap();
    assert!(forgotten.locators.is_empty());
    assert_eq!(forgotten.availability, RepositoryAvailability::Unavailable);
    assert_eq!(
        registry
            .resolve_by_id(repository_id)
            .unwrap()
            .unwrap()
            .identity,
        registered.repository.identity
    );
}

#[test]
fn concurrent_registration_creates_one_identity_and_one_locator() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("concurrent repo");
    init_repo(&repo, None);
    let registry =
        Arc::new(RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap());
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let registry = Arc::clone(&registry);
        let barrier = Arc::clone(&barrier);
        let repo = repo.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            registry.register(&request(repo)).unwrap()
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    let repository_id = outcomes[0].repository.identity.repository_id;

    assert!(outcomes.iter().all(|outcome| {
        outcome.repository.identity.repository_id == repository_id
            && outcome.repository.locators.len() == 1
    }));
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
            .filter(|outcome| outcome.created_locator)
            .count(),
        1
    );
}

#[test]
fn unsafe_symlink_relative_and_non_repository_paths_are_rejected() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("real-repo");
    init_repo(&repo, None);
    let registry = RepositoryRegistry::initialize(temporary.path().join("registry")).unwrap();
    assert!(
        registry
            .register(&request(PathBuf::from("relative")))
            .is_err()
    );
    assert!(registry.register(&request(temporary.path())).is_err());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&repo, temporary.path().join("repo-link")).unwrap();
        let error = registry
            .register(&request(temporary.path().join("repo-link")))
            .unwrap_err();
        assert!(error.message().contains("symlink"));
    }
}

#[test]
fn deleting_registry_cannot_touch_context_index_or_runtime_state() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("shared-context");
    let repo = temporary.path().join("business-repo");
    init_repo(&repo, None);
    fs::create_dir_all(root.join("repository")).unwrap();
    fs::create_dir_all(root.join("state")).unwrap();
    fs::write(root.join("repository/context.fact"), b"context fact").unwrap();
    fs::write(root.join("state/index.sqlite"), b"index bytes").unwrap();
    fs::write(root.join("state/runtime.sqlite"), b"runtime bytes").unwrap();
    let registry = RepositoryRegistry::initialize(&root).unwrap();
    let repository_id = registry
        .register(&request(repo))
        .unwrap()
        .repository
        .identity
        .repository_id;
    fs::remove_file(registry.database_path()).unwrap();

    let empty_registry = RepositoryRegistry::initialize(&root).unwrap();
    assert!(
        empty_registry
            .resolve_by_id(repository_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read(root.join("repository/context.fact")).unwrap(),
        b"context fact"
    );
    assert_eq!(
        fs::read(root.join("state/index.sqlite")).unwrap(),
        b"index bytes"
    );
    assert_eq!(
        fs::read(root.join("state/runtime.sqlite")).unwrap(),
        b"runtime bytes"
    );
}
