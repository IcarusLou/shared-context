use std::{collections::BTreeSet, fs, path::PathBuf};

use sctx_local_state::{Breadcrumb, BreadcrumbKind, CaptureStore, PrivacyScanner, UserConfigStore};
use serde::Deserialize;
use tempfile::tempdir;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    kind: String,
    value: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "../../../fixtures/privacy/common-sensitive.json"
    ))
    .unwrap()
}

#[test]
fn shared_secret_and_pii_fixture_is_detected_and_redacted() {
    let scanner = PrivacyScanner::default();
    for case in fixture().cases {
        let scan = scanner.scan(&case.value).unwrap();
        let codes = scan.diagnostic_codes().into_iter().collect::<BTreeSet<_>>();
        let redacted = scanner.redact(&case.value).unwrap();

        assert!(
            codes.contains(case.kind.as_str()),
            "{}: {codes:?}",
            case.kind
        );
        assert!(!redacted.text.contains(&case.value), "{}", case.kind);
        assert!(redacted.text.contains("[REDACTED:"), "{}", case.kind);
    }
}

#[test]
fn capture_boundary_redacts_every_shared_privacy_fixture() {
    let temporary = tempdir().unwrap();
    let store = CaptureStore::initialize(temporary.path().join("capture home 中文")).unwrap();

    for case in fixture().cases {
        let receipt = store
            .capture(&Breadcrumb {
                kind: BreadcrumbKind::Checkpoint,
                summary: format!("observed {}", case.value),
                workspace_hint: None,
                file_hints: Vec::new(),
            })
            .unwrap();
        let stored = fs::read_to_string(receipt.path).unwrap();

        assert!(!stored.contains(&case.value), "{}", case.kind);
        assert!(
            receipt
                .finding_kinds
                .iter()
                .any(|kind| kind.code() == case.kind),
            "{}: {:?}",
            case.kind,
            receipt.finding_kinds
        );
    }
}

#[test]
fn configuration_contains_only_the_single_context_store() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("用户 配置");

    let config = UserConfigStore::initialize(&root).unwrap();
    let document = fs::read_to_string(root.join("config.toml")).unwrap();
    let parsed = document.parse::<toml::Table>().unwrap();

    assert_eq!(config.repository(), root.join("repository"));
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed["version"].as_integer(), Some(1));
    assert_eq!(parsed["store"].as_str(), config.repository().to_str());
}

#[test]
fn configuration_refuses_a_second_store() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("home");
    let config = UserConfigStore::initialize(&root).unwrap();
    let path = root.join("config.toml");
    let text = fs::read_to_string(&path).unwrap();
    let other_store = PathBuf::from("/tmp/other-context-store");
    fs::write(
        &path,
        text.replace(
            config.repository().to_str().unwrap(),
            other_store.to_str().unwrap(),
        ),
    )
    .unwrap();

    let error = UserConfigStore::initialize(&root).unwrap_err();

    assert!(error.message().contains("exactly the fixed Store"));
}
