//! End-to-end acceptance for `sctx embedding install|status|remove` (T5c).
//!
//! This test is `#[ignore]`d because a real run moves ~2.3 GB of model weights and an ONNX Runtime
//! release tarball into a temporary `HOME`, then pays the 9--12 second model load twice. It reads
//! both halves from local paths rather than the network, which is the whole reason it can be an
//! acceptance test at all: `curl` treats `file://` as an ordinary transfer, so the resume, the
//! digest verification, the `.part` publication and the unpacking all run exactly as they would
//! against `hf-mirror.com`, on bytes that are already on this machine.
//!
//! ```text
//! SCTX_EMBEDDING_MODEL_SOURCE=/path/to/bge-m3-onnx \
//! SCTX_EMBEDDING_RUNTIME_ARCHIVE=/path/to/onnxruntime-osx-arm64-1.28.1.tgz \
//!   cargo test --locked -p sctx-cli --test embedding_install_workflow -- --ignored --nocapture
//! ```
//!
//! The model directory must hold the exact files `sctx embedding install` pins -- the same
//! `model.onnx`, `model.onnx_data` and `tokenizer.json` whose digests are compiled into
//! `sctx_installer::embedding::MODEL_FILES` -- because the point of the test is that a mirror is
//! held to the built-in digests. A directory of *some other* export makes it fail, correctly.
//!
//! ## Why the corpus comes from the probe harness
//!
//! The warm-up half of `install` only has something to do when the installation holds accepted
//! Context revisions, and building those through the public confirmation chain is exactly what
//! [`association_probe_harness::build_harness`] already does. Reusing it means the embedded count
//! this test asserts is a count of real revisions produced by the real MCP surface, not of
//! fixtures written straight into a database.

mod association_probe_harness;

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use association_probe_harness::{build_harness, run_json_cli};
use serde_json::Value;

const PROBE_FIXTURE: &str = include_str!("../../../fixtures/association/probe-ext-v1.json");

/// bge-m3 dense is 1024-wide. Asserted rather than read back, so a model that loads and produces a
/// differently shaped vector fails here instead of silently scoring against the wrong space.
const EXPECTED_DIMENSIONS: u64 = 1024;

fn sources() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("SCTX_EMBEDDING_MODEL_SOURCE")?;
    let runtime = std::env::var_os("SCTX_EMBEDDING_RUNTIME_ARCHIVE")?;
    Some((PathBuf::from(model), PathBuf::from(runtime)))
}

/// Builds a `file://` URL for a local path.
///
/// The percent-encoding is not decoration: the probe harness deliberately puts its `HOME` at a
/// path containing a space, and `curl` rejects a raw space in a URL outright. Production callers
/// pass an `https://` URL someone else already encoded, so this belongs to the test rather than to
/// the command.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(char::from(*byte));
            }
            other => {
                let _ = write!(url, "%{other:02X}");
            }
        }
    }
    url
}

fn run_cli(home: &Path, args: &[&str]) -> Value {
    run_json_cli(home, args)
}

fn run_cli_failure(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "CLI {args:?} unexpectedly succeeded"
    );
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
#[ignore = "moves ~2.3 GB and loads a real model; set SCTX_EMBEDDING_MODEL_SOURCE and SCTX_EMBEDDING_RUNTIME_ARCHIVE"]
fn install_configures_status_confirms_and_remove_cleans_up() {
    let Some((model_source, runtime_archive)) = sources() else {
        eprintln!(
            "skipping: set SCTX_EMBEDDING_MODEL_SOURCE and SCTX_EMBEDDING_RUNTIME_ARCHIVE to run"
        );
        return;
    };
    let fixture: Value = serde_json::from_str(PROBE_FIXTURE).unwrap();
    let harness = build_harness(&fixture);
    let home = harness.home.clone();
    let root = home.join(".shared-context");

    // Before: the channel does not exist, and doctor says so without calling it a problem.
    let before = run_cli(&home, &["embedding", "status"]);
    assert_eq!(before["configured"], Value::Bool(false));
    assert_eq!(before["ready"], Value::Bool(false));
    assert_eq!(before["loads"], Value::Null);
    let doctor = run_cli(&home, &["doctor"]);
    let check = retrieval_check(&doctor);
    assert_eq!(check["status"], "ok");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("sctx embedding install"),
        "doctor must point at the command that fixes this: {check:#}"
    );

    // Install, reading both halves from local paths through the real download path.
    let installed = run_cli(
        &home,
        &[
            "embedding",
            "install",
            "--model-url",
            &file_url(&model_source),
            "--runtime-url",
            &file_url(&runtime_archive),
        ],
    );
    assert_eq!(installed["configured"], Value::Bool(true));
    assert_eq!(installed["platform"], "darwin-arm64");
    assert_eq!(installed["self_check_dimensions"], EXPECTED_DIMENSIONS);
    assert_eq!(installed["warm_error"], Value::Null);
    let embedded = installed["embedded"].as_u64().unwrap();
    assert!(
        embedded > 0,
        "the warm-up must have embedded the corpus the harness built: {installed:#}"
    );
    assert_eq!(installed["cached"].as_u64().unwrap(), embedded);
    assert_eq!(
        installed["model_path"].as_str().unwrap(),
        root.join("embedding/model").to_str().unwrap()
    );
    assert_eq!(
        installed["runtime_path"].as_str().unwrap(),
        root.join("embedding/runtime/libonnxruntime.dylib")
            .to_str()
            .unwrap()
    );
    let downloaded = installed["files"].as_array().unwrap();
    assert_eq!(downloaded.len(), 4, "three model files plus the library");
    for file in downloaded {
        assert!(
            file["bytes"].as_u64().unwrap() > 0,
            "every installed file has content: {file:#}"
        );
    }

    // The configuration is a real `[retrieval]` table, and nothing else in the document moved.
    let config = fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(config.contains("[retrieval]"), "{config}");
    assert!(config.contains("embedding_model_path"), "{config}");
    assert!(config.contains("embedding_runtime_path"), "{config}");
    assert!(
        config.contains("[[repositories]]"),
        "the Catalog the harness registered survived the write: {config}"
    );

    // Status, including a real load.
    let status = run_cli(&home, &["embedding", "status", "--verify"]);
    assert_eq!(status["configured"], Value::Bool(true));
    assert_eq!(status["ready"], Value::Bool(true));
    assert_eq!(status["loads"], Value::Bool(true));
    assert_eq!(status["load_error"], Value::Null);
    assert_eq!(status["embedded_revisions"].as_u64().unwrap(), embedded);
    assert!(status["model_fingerprint"].as_str().is_some());
    for file in status["files"].as_array().unwrap() {
        assert_eq!(file["ok"], Value::Bool(true), "{file:#}");
        assert_eq!(file["present"], Value::Bool(true), "{file:#}");
    }
    assert!(
        root.join("state/semantic.sqlite").is_file(),
        "the warm-up wrote the vector cache"
    );

    // Doctor now reports the configured channel and the revisions it holds.
    let doctor = run_cli(&home, &["doctor"]);
    let check = retrieval_check(&doctor);
    assert_eq!(check["status"], "ok");
    assert!(
        check["message"].as_str().unwrap().contains("Configured:"),
        "{check:#}"
    );

    assert_reinstall_is_idempotent(&home, &model_source, &runtime_archive, embedded);
    assert_remove_cleans_up(&home, &root);
}

/// A second `install` over a complete one transfers nothing and embeds nothing.
fn assert_reinstall_is_idempotent(
    home: &Path,
    model_source: &Path,
    runtime_archive: &Path,
    embedded: u64,
) {
    let again = run_cli(
        home,
        &[
            "embedding",
            "install",
            "--model-url",
            &file_url(model_source),
            "--runtime-url",
            &file_url(runtime_archive),
        ],
    );
    for file in again["files"].as_array().unwrap() {
        assert_eq!(
            file["reused"],
            Value::Bool(true),
            "a verified file must not be downloaded twice: {file:#}"
        );
    }
    assert_eq!(
        again["embedded"].as_u64().unwrap(),
        0,
        "an already embedded corpus is not embedded again"
    );
    assert_eq!(again["cached"].as_u64().unwrap(), embedded);
}

/// `remove` needs confirmation, deletes exactly its own three things, and leaves the installation
/// in the state it was in before `install` ran.
fn assert_remove_cleans_up(home: &Path, root: &Path) {
    let refused = run_cli_failure(home, &["embedding", "remove"]);
    assert_eq!(refused["error"]["code"], "invalid_input");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--yes"),
        "{refused:#}"
    );
    assert!(root.join("embedding").exists());

    let removed = run_cli(home, &["embedding", "remove", "--yes"]);
    assert_eq!(removed["config_cleared"], Value::Bool(true));
    assert!(!root.join("embedding").exists());
    assert!(!root.join("state/semantic.sqlite").exists());
    let config = fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(!config.contains("[retrieval]"), "{config}");
    assert!(
        config.contains("[[repositories]]"),
        "removal is scoped to `[retrieval]`: {config}"
    );

    let after = run_cli(home, &["embedding", "status"]);
    assert_eq!(after["configured"], Value::Bool(false));
    assert_eq!(after["ready"], Value::Bool(false));
    assert_eq!(after["embedded_revisions"].as_u64().unwrap(), 0);
    let doctor = run_cli(home, &["doctor"]);
    assert_eq!(retrieval_check(&doctor)["status"], "ok");
}

#[test]
#[ignore = "moves ~2.3 GB; set SCTX_EMBEDDING_MODEL_SOURCE and SCTX_EMBEDDING_RUNTIME_ARCHIVE"]
fn a_mirror_serving_different_bytes_is_rejected_and_configures_nothing() {
    let Some((model_source, runtime_archive)) = sources() else {
        eprintln!(
            "skipping: set SCTX_EMBEDDING_MODEL_SOURCE and SCTX_EMBEDDING_RUNTIME_ARCHIVE to run"
        );
        return;
    };
    let fixture: Value = serde_json::from_str(PROBE_FIXTURE).unwrap();
    let harness = build_harness(&fixture);
    let home = harness.home.clone();
    let root = home.join(".shared-context");

    // A "mirror" whose small files are right and whose tokenizer is not. The pinned digests are
    // properties of the files, so the wrong bytes are caught wherever they came from.
    let mirror = home.join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    for name in ["model.onnx", "model.onnx_data"] {
        std::os::unix::fs::symlink(model_source.join(name), mirror.join(name)).unwrap();
    }
    fs::write(mirror.join("tokenizer.json"), b"not a tokenizer").unwrap();

    let error = run_cli_failure(
        &home,
        &[
            "embedding",
            "install",
            "--model-url",
            &file_url(&mirror),
            "--runtime-url",
            &file_url(&runtime_archive),
        ],
    );

    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("tokenizer.json"), "{message}");
    assert!(
        message.contains("SHA-256") || message.contains("expected"),
        "{message}"
    );
    assert!(
        !root.join("embedding/model/tokenizer.json").exists(),
        "a file that failed verification must never take its real name"
    );
    assert!(
        !root.join("embedding/model/tokenizer.json.part").exists(),
        "the rejected partial is removed rather than left to poison a resume"
    );
    let config = fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        !config.contains("[retrieval]"),
        "a failed install configures nothing: {config}"
    );
    let status = run_cli(&home, &["embedding", "status"]);
    assert_eq!(status["configured"], Value::Bool(false));
}

fn retrieval_check(doctor: &Value) -> &Value {
    doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "retrieval_embedding")
        .unwrap_or_else(|| panic!("doctor reported no retrieval_embedding check: {doctor:#}"))
}
