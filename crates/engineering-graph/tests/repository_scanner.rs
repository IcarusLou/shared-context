use std::{collections::BTreeSet, fs, path::Path, process::Command};

use sctx_domain::{ArtifactKind, RepositoryId, RepositoryIdentity, SemanticFingerprint};
use sctx_engineering_graph::{
    ArtifactSourceState, RepositoryScanOutcome, RepositoryScanner, RepositoryScannerLimits,
    RepositorySnapshot, SkippedFileReason, SourceLanguage,
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
        semantic_fingerprint: SemanticFingerprint::new(format!("repo:{name}")).unwrap(),
    }
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

    let first = available(scanner.scan(&repository, &repo).unwrap());
    let second = available(scanner.scan(&repository, &repo).unwrap());

    assert_eq!(first, second);
    assert_eq!(
        first.policy_version,
        "tracked-head-plus-safe-tracked-modifications-v1"
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
    let api = first
        .artifacts
        .iter()
        .find(|artifact| {
            artifact.artifact.artifact_key.kind() == ArtifactKind::Api
                && artifact.artifact.display_name == "/api/search"
        })
        .unwrap();
    assert!(
        api.observations
            .iter()
            .map(|observation| observation.language)
            .collect::<BTreeSet<_>>()
            .is_superset(
                &[
                    SourceLanguage::Rust,
                    SourceLanguage::TypeScriptJavaScript,
                    SourceLanguage::Swift,
                    SourceLanguage::Kotlin,
                    SourceLanguage::Json,
                ]
                .into()
            )
    );
    let schema = first
        .artifacts
        .iter()
        .find(|artifact| {
            artifact.artifact.artifact_key.kind() == ArtifactKind::Schema
                && artifact.artifact.display_name == "SearchResponse"
        })
        .unwrap();
    assert!(
        schema
            .observations
            .iter()
            .map(|observation| observation.language)
            .collect::<BTreeSet<_>>()
            .is_superset(
                &[
                    SourceLanguage::TypeScriptJavaScript,
                    SourceLanguage::Json,
                    SourceLanguage::Proto,
                ]
                .into()
            )
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

    let snapshot = available(scanner.scan(&identity("bounded"), &repo).unwrap());

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
    ] {
        assert!(reasons.contains(&expected), "missing {expected:?}");
    }
    #[cfg(unix)]
    assert!(reasons.contains(&SkippedFileReason::Symlink));
}

#[test]
fn file_move_keeps_file_key_and_fingerprints() {
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
    let before = available(scanner.scan(&repository, &repo).unwrap());
    fs::create_dir_all(repo.join("new")).unwrap();
    git(&repo, &["mv", "old/search.rs", "new/search.rs"]);
    let after = available(scanner.scan(&repository, &repo).unwrap());
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

    assert_eq!(
        before_file.artifact.artifact_key,
        after_file.artifact.artifact_key
    );
    assert_eq!(
        before_file.artifact.content_fingerprint,
        after_file.artifact.content_fingerprint
    );
    assert_eq!(
        before_file.artifact.semantic_fingerprint,
        after_file.artifact.semantic_fingerprint
    );
}

#[test]
fn symbol_rename_changes_logical_key_but_keeps_body_semantic_fingerprint() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("rename repo");
    init_repo(&repo);
    write(&repo, "src/lib.rs", "pub fn old_name() { answer(); }\n");
    commit_all(&repo);
    let repository = identity("rename");
    let scanner = RepositoryScanner::default();
    let before = available(scanner.scan(&repository, &repo).unwrap());
    write(&repo, "src/lib.rs", "pub fn new_name() { answer(); }\n");
    let after = available(scanner.scan(&repository, &repo).unwrap());
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
    assert_eq!(
        old.artifact.semantic_fingerprint,
        new.artifact.semantic_fingerprint
    );
}

#[test]
fn same_name_symbols_in_different_modules_do_not_collide() {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("modules repo");
    init_repo(&repo);
    write(&repo, "one/result.ts", "export class Result {}\n");
    write(&repo, "two/result.ts", "export class Result {}\n");
    commit_all(&repo);
    let snapshot = available(
        RepositoryScanner::default()
            .scan(&identity("modules"), &repo)
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
    let initial = available(scanner.scan(&repository, &repo).unwrap());
    write(
        &repo,
        "src/lib.rs",
        "pub fn first() {}\npub fn second() {}\n",
    );

    let incremental = available(
        scanner
            .scan_incremental(&initial, &repository, &repo)
            .unwrap(),
    );
    let scratch = available(scanner.scan(&repository, &repo).unwrap());

    assert_eq!(incremental, scratch);
    assert_ne!(initial.generation, incremental.generation);
}

#[test]
fn unavailable_repository_is_a_typed_outcome() {
    let temporary = TempDir::new().unwrap();
    let repository = identity("unavailable");
    let outcome = RepositoryScanner::default()
        .scan(&repository, &temporary.path().join("missing"))
        .unwrap();
    assert!(matches!(
        outcome,
        RepositoryScanOutcome::Unavailable {
            repository_id,
            ..
        } if repository_id == repository.repository_id
    ));
}
