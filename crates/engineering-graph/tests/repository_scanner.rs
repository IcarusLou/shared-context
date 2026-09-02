use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use sctx_domain::{
    ArtifactKind, ArtifactLocator, RepoRelativePath, RepositoryId, RepositoryIdentity,
};
use sctx_engineering_graph::{
    ArtifactSourceState, MAX_REPOSITORY_SCAN_PLAN_PATHS, RepositoryScanOutcome, RepositoryScanPlan,
    RepositoryScanner, RepositoryScannerLimits, RepositorySnapshot, ScanCoverage,
    SkippedFileReason, SourceLanguage,
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
        repository.repository_id.clone(),
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
        repository.repository_id.clone(),
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
    assert!(RepositoryScanPlan::new(repository.repository_id.clone(), Vec::new()).is_err());
    assert!(
        RepositoryScanPlan::new(
            repository.repository_id.clone(),
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

/// Wall-clock budget a Confirmation-time rescan is allowed to spend, mirrored from the MCP server.
const AUTO_SCAN_BUDGET: Duration = Duration::from_secs(2);
/// Plan size a real monorepo Confirmation was observed to produce.
const LARGE_PLAN_PATHS: usize = 1_000;

fn seed_large_plan(repo: &Path, files: usize, declarations: usize) -> Vec<String> {
    let mut paths = Vec::with_capacity(files);
    for index in 0..files {
        let relative = format!("module{:03}/src/unit{index:04}.rs", index % 64);
        let mut body = String::new();
        for item in 0..declarations {
            writeln!(body, "pub fn unit_{index}_{item}() -> u32 {{ {item} }}").unwrap();
        }
        write(repo, &relative, &body);
        paths.push(relative);
    }
    paths
}

#[test]
fn a_thousand_planned_paths_scan_inside_the_confirmation_budget() {
    // The per-path shape of this scan used to spawn two local Git processes per planned path, and
    // on a Repository large enough for either process to cost real time the budget was spent
    // before the first handful of files, so a Confirmation-time rescan produced nothing at all.
    // Batching the tracked-state questions makes the plan size stop multiplying the fixed cost.
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("monorepo");
    init_repo(&repo);
    let paths = seed_large_plan(&repo, LARGE_PLAN_PATHS, 4);
    commit_all(&repo);
    let repository = identity("monorepo");
    let scan_plan = plan(
        &repository,
        &paths.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let started = Instant::now();
    let outcome = RepositoryScanner::default()
        .scan_before(
            &repository,
            &repo,
            &scan_plan,
            Instant::now().checked_add(AUTO_SCAN_BUDGET),
        )
        .unwrap()
        .expect("a budgeted scan of a thousand planned paths must produce a snapshot");
    let elapsed = started.elapsed();

    let snapshot = available(outcome);
    assert_eq!(
        snapshot.scanned_files, LARGE_PLAN_PATHS,
        "every planned path must be read inside the budget"
    );
    assert!(
        snapshot.coverage.is_complete(),
        "a scan that finishes inside its budget is complete, not partial"
    );
    assert!(
        elapsed < AUTO_SCAN_BUDGET,
        "scanning {LARGE_PLAN_PATHS} planned paths took {elapsed:?}, over the \
         {AUTO_SCAN_BUDGET:?} Confirmation budget"
    );
    eprintln!("scanned {LARGE_PLAN_PATHS} planned paths in {elapsed:?}");
}

#[test]
fn an_expired_budget_commits_the_scanned_subset_and_names_what_it_never_reached() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("partial");
    init_repo(&repo);
    let paths = seed_large_plan(&repo, 300, 200);
    commit_all(&repo);
    let repository = identity("partial");
    let scan_plan = plan(
        &repository,
        &paths.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    // A budget that expires while the plan is still being read used to throw the whole scan away.
    let complete = available(
        RepositoryScanner::default()
            .scan(&repository, &repo, &scan_plan)
            .unwrap(),
    );
    let full_scan = {
        let started = Instant::now();
        let _ = RepositoryScanner::default()
            .scan(&repository, &repo, &scan_plan)
            .unwrap();
        started.elapsed()
    };
    // The exact wall-clock point a budget lands on is machine-dependent, so the fraction of one
    // full scan is widened until it lands inside the plan rather than before or after it.
    let mut partial = None;
    for percent in [40, 50, 60, 70, 75, 80, 85, 90, 95] {
        let Some(outcome) = RepositoryScanner::default()
            .scan_before(
                &repository,
                &repo,
                &scan_plan,
                Instant::now().checked_add(full_scan * percent / 100),
            )
            .unwrap()
        else {
            continue;
        };
        let candidate = available(outcome);
        if !candidate.coverage.is_complete() {
            partial = Some(candidate);
            break;
        }
    }
    let snapshot = partial.expect("a budget that expires mid-plan still commits what it read");

    let ScanCoverage::Partial {
        covered,
        unfinished,
    } = &snapshot.coverage
    else {
        panic!("a scan cut short by its budget must record partial coverage");
    };
    assert!(!covered.is_empty(), "the scanned subset must be committed");
    assert!(
        !unfinished.is_empty(),
        "the unread range must be named, not silently dropped"
    );
    assert_eq!(
        covered.len() + unfinished.len(),
        scan_plan.paths().len(),
        "coverage must account for every planned path exactly once"
    );
    assert!(
        !snapshot.artifacts.is_empty(),
        "the committed subset must carry the Artifacts it did read"
    );
    assert_ne!(
        snapshot.generation, complete.generation,
        "a partial snapshot must not claim the generation of a complete one"
    );
    assert!(
        !snapshot.coverage.unfinished_prefixes().is_empty(),
        "the projection needs directory prefixes for the unread range"
    );
    let first_covered = covered.first().unwrap().as_str().to_owned();
    assert!(snapshot.coverage.covers_path(&first_covered));
    assert!(
        !snapshot
            .coverage
            .covers_path(unfinished.first().unwrap().as_str())
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn android_java_and_xml_artifacts_are_scanned_rather_than_skipped() {
    // Fifteen of nineteen unresolved References in one real installation named Android artifacts
    // the language table did not recognise, so every one of them was skipped as an unsupported
    // language and then reported as missing from the Repository.
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("android");
    init_repo(&repo);
    write(
        repo.as_path(),
        "app/src/main/java/com/example/live/AvatarImageWithLive.java",
        r#"package com.example.live;

public class AvatarImageWithLive extends FrameLayout {
    private static final String TAG = "AvatarImageWithLive";

    public AvatarImageWithLive(Context context) {
        super(context);
    }

    public void bindAvatar(User user) {
        setUser(user);
    }

    protected int measureAvatarSize(int spec) {
        return spec;
    }

    interface LiveStatusListener {
        void onLiveStatusChanged(boolean live);
    }
}
"#,
    );
    write(
        repo.as_path(),
        "app/src/main/res/layout/avatar_image_with_live.xml",
        r#"<?xml version="1.0" encoding="utf-8"?>
<merge xmlns:android="http://schemas.android.com/apk/res/android">
    <ImageView android:id="@+id/avatar" />
</merge>
"#,
    );
    write(
        repo.as_path(),
        "app/src/main/res/drawable/live_ring.xml",
        r#"<?xml version="1.0" encoding="utf-8"?>
<shape xmlns:android="http://schemas.android.com/apk/res/android" />
"#,
    );
    commit_all(&repo);
    let repository = identity("android");
    let scan_plan = plan(
        &repository,
        &[
            "app/src/main/java/com/example/live/AvatarImageWithLive.java",
            "app/src/main/res/drawable/live_ring.xml",
            "app/src/main/res/layout/avatar_image_with_live.xml",
        ],
    );
    let snapshot = available(
        RepositoryScanner::default()
            .scan(&repository, &repo, &scan_plan)
            .unwrap(),
    );

    assert_eq!(snapshot.scanned_files, 3);
    assert!(
        snapshot.skipped_files.is_empty(),
        "no Android artifact may be skipped as an unsupported language: {:?}",
        snapshot.skipped_files
    );
    for relative in [
        "app/src/main/java/com/example/live/AvatarImageWithLive.java",
        "app/src/main/res/drawable/live_ring.xml",
        "app/src/main/res/layout/avatar_image_with_live.xml",
    ] {
        assert!(
            snapshot.artifacts.iter().any(|artifact| {
                artifact.artifact.artifact_key.kind() == ArtifactKind::File
                    && artifact.artifact.artifact_key.locator().path().as_str() == relative
            }),
            "{relative} must resolve as a File Artifact"
        );
    }
    let symbols = snapshot
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact.artifact_key.kind() == ArtifactKind::Symbol)
        .map(|artifact| artifact.artifact.display_name.clone())
        .collect::<BTreeSet<_>>();
    assert!(symbols.contains("AvatarImageWithLive"), "{symbols:?}");
    assert!(symbols.contains("bindAvatar"), "{symbols:?}");
    assert!(symbols.contains("measureAvatarSize"), "{symbols:?}");
    let schemas = snapshot
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact.artifact_key.kind() == ArtifactKind::Schema)
        .map(|artifact| artifact.artifact.display_name.clone())
        .collect::<BTreeSet<_>>();
    assert!(schemas.contains("LiveStatusListener"), "{schemas:?}");
    assert!(
        !snapshot.artifacts.iter().any(|artifact| {
            std::path::Path::new(artifact.artifact.artifact_key.locator().path().as_str())
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
                && artifact.artifact.artifact_key.kind() == ArtifactKind::Symbol
        }),
        "XML carries no symbol a deterministic locator could name"
    );
}
