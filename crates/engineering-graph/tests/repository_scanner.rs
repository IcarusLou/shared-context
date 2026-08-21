use std::{collections::BTreeSet, fs, path::Path, process::Command};

use sctx_domain::{
    ArtifactKind, ArtifactLocator, RepoRelativePath, RepositoryId, RepositoryIdentity,
};
use sctx_engineering_graph::{
    ArtifactSourceState, MAX_REPOSITORY_SCAN_PLAN_PATHS, RepositoryScanOutcome, RepositoryScanPlan,
    RepositoryScanner, RepositoryScannerLimits, RepositorySnapshot, SkippedFileReason,
    SourceLanguage,
};
use tempfile::TempDir;

fn git(path: &Path, args: &[&str]) {
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
}

fn init_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let output = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    git(path, &["config", "user.name", "Scanner Test"]);
    git(path, &["config", "user.email", "scanner@example.invalid"]);
}

fn write(path: &Path, relative: &str, content: &str) {
    let path = path.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn commit_all(path: &Path) {
    git(path, &["add", "--all"]);
    git(path, &["commit", "-q", "-m", "scanner fixture"]);
}

fn identity(name: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        repository_id: RepositoryId::new(),
        canonical_name: name.to_owned(),
    }
}

fn plan(repository: &RepositoryIdentity, paths: &[&str]) -> RepositoryScanPlan {
    RepositoryScanPlan::new(
        repository.repository_id,
        paths
            .iter()
            .map(|path| RepoRelativePath::new(*path).unwrap())
            .collect(),
    )
    .unwrap()
}

fn available(outcome: RepositoryScanOutcome) -> RepositorySnapshot {
    match outcome {
        RepositoryScanOutcome::Available(snapshot) => snapshot,
        RepositoryScanOutcome::Unavailable { reason, .. } => {
            panic!("Repository unexpectedly unavailable: {reason}")
        }
    }
}

fn seed_languages(repo: &Path) {
    write(
        repo,
        "rust/src/lib.rs",
        r#"
pub struct RustResult { value: String }
pub fn rust_search() { client.get("/api/search"); }
#[test]
fn rust_contract_test() { assert!(true); }
"#,
    );
    write(
        repo,
        "web/src/search.ts",
        r#"
export interface SearchResponse { value: string }
export function webSearch() { return fetch("/api/search"); }
test("web contract test", () => true);
"#,
    );
    write(
        repo,
        "ios/Sources/Search.swift",
        r#"
struct SwiftResult { let value: String }
func swiftSearch() { let url = URL(string: "/api/search") }
func testSwiftContract() { }
"#,
    );
    write(
        repo,
        "android/src/Search.kt",
        r#"
data class KotlinResult(val value: String)
fun kotlinSearch() { client.get("/api/search") }
@Test
fun kotlinContract() { }
"#,
    );
    write(
        repo,
        "schema/openapi.json",
        r#"{"openapi":"3.0.0","paths":{"/api/search":{}},"components":{"schemas":{"SearchResponse":{}}}}"#,
    );
    write(
        repo,
        "schema/search.proto",
        r"
message SearchResponse { string value = 1; }
service SearchService { rpc Search (SearchResponse) returns (SearchResponse); }
",
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn discovers_bounded_cross_language_artifacts_in_stable_order() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("多语言 repo");
    init_repo(&repo);
    seed_languages(&repo);
    write(
        &repo,
        "web/src/secret_marker.ts",
        "export const visible = 'FULL_SOURCE_MUST_NOT_BE_STORED';",
    );
    commit_all(&repo);
    let repository = identity("cross-language");
    let scanner = RepositoryScanner::default();
    let scan_plan = plan(
        &repository,
        &[
            "rust/src/lib.rs",
            "web/src/search.ts",
            "ios/Sources/Search.swift",
            "android/src/Search.kt",
            "schema/openapi.json",
            "schema/search.proto",
            "web/src/secret_marker.ts",
        ],
    );

    let first = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());
    let second = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());

    assert_eq!(first, second);
    assert_eq!(
        first.policy_version,
        "planned-paths-plus-safe-tracked-modifications-v2"
    );
    assert!(first.generation.starts_with("snap_"));
    assert!(first.artifacts.windows(2).all(
        |pair| pair[0].artifact.artifact_key.digest() < pair[1].artifact.artifact_key.digest()
    ));
    let kinds = first
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact.artifact_key.kind())
        .collect::<BTreeSet<_>>();
    for expected in [
        ArtifactKind::Module,
        ArtifactKind::File,
        ArtifactKind::Symbol,
        ArtifactKind::Api,
        ArtifactKind::Schema,
        ArtifactKind::Test,
    ] {
        assert!(kinds.contains(&expected), "missing {expected:?}");
    }
    assert!(first.artifacts.iter().any(|artifact| matches!(
        artifact.artifact.artifact_key.locator(),
        ArtifactLocator::Symbol {
            path,
            language,
            module,
            enclosing_type: None,
            symbol_name,
            signature,
        } if path.as_str() == "web/src/search.ts"
            && language == "typescript-javascript"
            && module == "web/src"
            && symbol_name == "webSearch"
            && signature == "export function webSearch()"
    )));
    assert!(first.artifacts.iter().any(|artifact| matches!(
        artifact.artifact.artifact_key.locator(),
        ArtifactLocator::Api {
            path,
            protocol,
            operation,
            normalized_route,
        } if path.as_str() == "web/src/search.ts"
            && protocol == "http"
            && operation == "GET"
            && normalized_route == "/api/search"
    )));
    assert!(first.artifacts.iter().any(|artifact| matches!(
        artifact.artifact.artifact_key.locator(),
        ArtifactLocator::Schema {
            path,
            namespace,
            version,
            qualified_name,
        } if path.as_str() == "web/src/search.ts"
            && namespace == "web/src"
            && version == "unversioned"
            && qualified_name == "web/src::SearchResponse"
    )));
    assert!(first.artifacts.iter().any(|artifact| matches!(
        artifact.artifact.artifact_key.locator(),
        ArtifactLocator::Test {
            path,
            qualified_test_name,
        } if path.as_str() == "rust/src/lib.rs"
            && qualified_test_name == "rust/src::rust_contract_test"
    )));
    let apis = first
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact.artifact.artifact_key.kind() == ArtifactKind::Api
                && artifact.artifact.display_name == "/api/search"
        })
        .collect::<Vec<_>>();
    assert!(!apis.is_empty());
    assert_eq!(
        apis.iter()
            .map(|artifact| &artifact.artifact.artifact_key)
            .collect::<BTreeSet<_>>()
            .len(),
        apis.len(),
        "same route in different repository-relative files remains distinct"
    );
    assert!(
        apis.iter()
            .all(|artifact| !artifact.observations.is_empty())
    );
    let schemas = first
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact.artifact.artifact_key.kind() == ArtifactKind::Schema
                && artifact.artifact.display_name == "SearchResponse"
        })
        .collect::<Vec<_>>();
    assert!(!schemas.is_empty());
    assert_eq!(
        schemas
            .iter()
            .map(|artifact| &artifact.artifact.artifact_key)
            .collect::<BTreeSet<_>>()
            .len(),
        schemas.len(),
        "same Schema name in different files remains distinct"
    );
    assert!(
        schemas
            .iter()
            .all(|artifact| !artifact.observations.is_empty())
    );
    let test_languages = first
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact.artifact_key.kind() == ArtifactKind::Test)
        .flat_map(|artifact| artifact.observations.iter())
        .map(|observation| observation.language)
        .collect::<BTreeSet<_>>();
    assert!(
        test_languages.is_superset(
            &[
                SourceLanguage::Rust,
                SourceLanguage::TypeScriptJavaScript,
                SourceLanguage::Swift,
                SourceLanguage::Kotlin,
            ]
            .into()
        )
    );
    assert!(first.artifacts.iter().all(|artifact| {
        artifact.snapshot_generation == first.generation
            && artifact
                .observations
                .iter()
                .all(|observation| observation.source_state == ArtifactSourceState::TrackedHead)
    }));
    assert!(!format!("{first:?}").contains("FULL_SOURCE_MUST_NOT_BE_STORED"));
}

#[test]
fn tracked_modifications_are_included_while_untracked_and_unsafe_files_are_skipped() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("bounded repo");
    init_repo(&repo);
    write(&repo, "src/current.ts", "export function before() {}\n");
    write(&repo, "vendor/ignored.rs", "pub fn vendor() {}\n");
    write(
        &repo,
        "src/client.generated.ts",
        "export const generated = 1;\n",
    );
    write(&repo, ".secrets/token.json", "{\"token\":\"secret\"}\n");
    write(&repo, "src/large.ts", &"x".repeat(512));
    fs::write(repo.join("src/binary.json"), b"{\0binary}").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("current.ts", repo.join("src/link.ts")).unwrap();
    commit_all(&repo);
    write(&repo, "src/current.ts", "export function after() {}\n");
    write(
        &repo,
        "src/untracked.ts",
        "export function untracked() {}\n",
    );
    let scanner = RepositoryScanner::new(RepositoryScannerLimits {
        max_files: 100,
        max_file_bytes: 128,
        max_total_bytes: 4096,
    });
    let repository = identity("bounded");
    let scan_plan = plan(
        &repository,
        &[
            "src/current.ts",
            "vendor/ignored.rs",
            "src/client.generated.ts",
            ".secrets/token.json",
            "src/large.ts",
            "src/binary.json",
            "src/link.ts",
            "src/untracked.ts",
        ],
    );

    let snapshot = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());

    assert!(snapshot.artifacts.iter().any(|artifact| {
        artifact.artifact.display_name == "after"
            && artifact.observations.iter().any(|observation| {
                observation.source_state == ArtifactSourceState::TrackedWorkingModification
            })
    }));
    assert!(
        !snapshot
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact.display_name == "before"
                || artifact.artifact.display_name == "untracked")
    );
    let reasons = snapshot
        .skipped_files
        .iter()
        .map(|skipped| skipped.reason)
        .collect::<BTreeSet<_>>();
    for expected in [
        SkippedFileReason::IgnoredDirectory,
        SkippedFileReason::Generated,
        SkippedFileReason::Oversized,
        SkippedFileReason::Binary,
        SkippedFileReason::Untracked,
    ] {
        assert!(reasons.contains(&expected), "missing {expected:?}");
    }
    #[cfg(unix)]
    assert!(reasons.contains(&SkippedFileReason::Symlink));
}

#[test]
fn file_move_changes_exact_path_key_without_relocation_guessing() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("move repo");
    init_repo(&repo);
    write(
        &repo,
        "old/search.rs",
        "pub fn stable_body() { answer(); }\n",
    );
    commit_all(&repo);
    let repository = identity("move");
    let scanner = RepositoryScanner::default();
    let before_plan = plan(&repository, &["old/search.rs"]);
    let before = available(scanner.scan(&repository, &repo, &before_plan).unwrap());
    fs::create_dir_all(repo.join("new")).unwrap();
    git(&repo, &["mv", "old/search.rs", "new/search.rs"]);
    let after_plan = plan(&repository, &["new/search.rs"]);
    let after = available(scanner.scan(&repository, &repo, &after_plan).unwrap());
    let before_file = before
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact.artifact_key.kind() == ArtifactKind::File)
        .unwrap();
    let after_file = after
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact.artifact_key.kind() == ArtifactKind::File)
        .unwrap();

    assert_ne!(
        before_file.artifact.artifact_key,
        after_file.artifact.artifact_key
    );
}

#[test]
fn symbol_rename_changes_qualified_locator_key() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("rename repo");
    init_repo(&repo);
    write(&repo, "src/lib.rs", "pub fn old_name() { answer(); }\n");
    commit_all(&repo);
    let repository = identity("rename");
    let scanner = RepositoryScanner::default();
    let scan_plan = plan(&repository, &["src/lib.rs"]);
    let before = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());
    write(&repo, "src/lib.rs", "pub fn new_name() { answer(); }\n");
    let after = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());
    let old = before
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact.display_name == "old_name")
        .unwrap();
    let new = after
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact.display_name == "new_name")
        .unwrap();

    assert_ne!(old.artifact.artifact_key, new.artifact.artifact_key);
}

#[test]
fn content_change_at_same_path_and_signature_keeps_locator_identity() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("content change repo");
    init_repo(&repo);
    write(&repo, "src/lib.rs", "pub fn stable() { before(); }\n");
    commit_all(&repo);
    let repository = identity("content-change");
    let scanner = RepositoryScanner::default();
    let scan_plan = plan(&repository, &["src/lib.rs"]);
    let before = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());
    write(&repo, "src/lib.rs", "pub fn stable() { after(); }\n");
    let after = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());
    assert_ne!(before.generation, after.generation);
    for kind in [ArtifactKind::File, ArtifactKind::Symbol] {
        let before_key = &before
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact.artifact_key.kind() == kind)
            .unwrap()
            .artifact
            .artifact_key;
        let after_key = &after
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact.artifact_key.kind() == kind)
            .unwrap()
            .artifact
            .artifact_key;
        assert_eq!(before_key, after_key);
    }
}

#[test]
fn same_name_symbols_in_different_modules_do_not_collide() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("modules repo");
    init_repo(&repo);
    write(&repo, "one/result.ts", "export class Result {}\n");
    write(&repo, "two/result.ts", "export class Result {}\n");
    commit_all(&repo);
    let repository = identity("modules");
    let scan_plan = plan(&repository, &["one/result.ts", "two/result.ts"]);
    let snapshot = available(
        RepositoryScanner::default()
            .scan(&repository, &repo, &scan_plan)
            .unwrap(),
    );
    let keys = snapshot
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact.display_name == "Result")
        .map(|artifact| artifact.artifact.artifact_key.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(keys.len(), 2);
}

#[test]
fn incremental_scan_is_identical_to_scratch_for_same_generation() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("incremental repo");
    init_repo(&repo);
    write(&repo, "src/lib.rs", "pub fn first() {}\n");
    commit_all(&repo);
    let repository = identity("incremental");
    let scanner = RepositoryScanner::default();
    let scan_plan = plan(&repository, &["src/lib.rs"]);
    let initial = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());
    write(
        &repo,
        "src/lib.rs",
        "pub fn first() {}\npub fn second() {}\n",
    );

    let incremental = available(
        scanner
            .scan_incremental(&initial, &repository, &repo, &scan_plan)
            .unwrap(),
    );
    let scratch = available(scanner.scan(&repository, &repo, &scan_plan).unwrap());

    assert_eq!(incremental, scratch);
    assert_ne!(initial.generation, incremental.generation);
}

#[test]
fn unavailable_repository_is_a_typed_outcome() {
    let temporary = TempDir::new().unwrap();
    let repository = identity("unavailable");
    let scan_plan = plan(&repository, &["src/missing.rs"]);
    let outcome = RepositoryScanner::default()
        .scan(&repository, &temporary.path().join("missing"), &scan_plan)
        .unwrap();
    assert!(matches!(
        outcome,
        RepositoryScanOutcome::Unavailable {
            repository_id,
            ..
        } if repository_id == repository.repository_id
    ));
}

#[test]
fn sparse_plan_reads_only_deduplicated_reference_paths() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("sparse repo");
    init_repo(&repo);
    write(&repo, "src/referenced.rs", "pub fn referenced_only() {}\n");
    for index in 0..200 {
        write(
            &repo,
            &format!("src/unreferenced_{index}.rs"),
            &format!("pub fn unreferenced_{index}() {{}}\n"),
        );
    }
    write(
        &repo,
        "src/unreadable_unreferenced.rs",
        "pub fn must_never_be_read() {}\n",
    );
    commit_all(&repo);
    let repository = identity("sparse");
    let scan_plan = RepositoryScanPlan::new(
        repository.repository_id,
        vec![
            RepoRelativePath::new("src/referenced.rs").unwrap(),
            RepoRelativePath::new("src/referenced.rs").unwrap(),
        ],
    )
    .unwrap();

    assert_eq!(scan_plan.paths().len(), 1);
    #[cfg(unix)]
    let original_permissions = {
        use std::os::unix::fs::PermissionsExt;
        let path = repo.join("src/unreadable_unreferenced.rs");
        let permissions = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        permissions
    };
    let outcome = RepositoryScanner::default().scan(&repository, &repo, &scan_plan);
    #[cfg(unix)]
    fs::set_permissions(
        repo.join("src/unreadable_unreferenced.rs"),
        original_permissions,
    )
    .unwrap();
    let snapshot = available(outcome.unwrap());
    assert_eq!(snapshot.planned_paths, scan_plan.paths());
    assert_eq!(snapshot.scanned_files, 1);
    assert!(snapshot.artifacts.iter().all(|artifact| {
        artifact
            .observations
            .iter()
            .all(|observation| observation.path == "src/referenced.rs")
    }));
    assert!(!format!("{snapshot:?}").contains("unreferenced_"));
}

#[test]
fn empty_or_missing_plan_never_falls_back_to_repository_enumeration() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("missing plan repo");
    init_repo(&repo);
    write(
        &repo,
        "src/should_not_scan.rs",
        "pub fn forbidden_fallback() {}\n",
    );
    commit_all(&repo);
    let repository = identity("missing-plan");
    assert!(RepositoryScanPlan::new(repository.repository_id, Vec::new()).is_err());
    assert!(
        RepositoryScanPlan::new(
            repository.repository_id,
            (0..=MAX_REPOSITORY_SCAN_PLAN_PATHS)
                .map(|index| RepoRelativePath::new(format!("src/path_{index}.rs")).unwrap())
                .collect(),
        )
        .is_err()
    );
    let missing = plan(&repository, &["src/missing.rs"]);

    let snapshot = available(
        RepositoryScanner::default()
            .scan(&repository, &repo, &missing)
            .unwrap(),
    );
    assert_eq!(snapshot.scanned_files, 0);
    assert!(snapshot.artifacts.is_empty());
    assert_eq!(snapshot.skipped_files.len(), 1);
    assert_eq!(snapshot.skipped_files[0].path, "src/missing.rs");
    assert_eq!(snapshot.skipped_files[0].reason, SkippedFileReason::Missing);
    assert!(!format!("{snapshot:?}").contains("forbidden_fallback"));
}
