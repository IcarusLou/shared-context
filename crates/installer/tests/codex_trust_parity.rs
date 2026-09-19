//! Bit-for-bit parity between the two ports of Codex's hook trust hash.
//!
//! The algorithm lives twice on purpose: the installer needs it in Rust to re-stamp
//! `~/.codex/config.toml` during setup, and the replay driver needs it in Python to trust a
//! `hooks.json` it wrote itself. `fixtures/codex-trust/` is the contract between them --
//! `hooks.json` is the input, `expected.json` holds the hashes
//! `tests/scripts/session_replay/codex_trust.py` produced for it, and this test asserts the Rust
//! port reproduces every one of them.
//!
//! The fixture is deliberately not just the six hooks the installer writes. It also carries a
//! second matcher group with a foreign command hook (non-default timeout, `async`, a raised
//! `additionalContextLimit`) and an `mcp_tool` handler, a matcher on `UserPromptSubmit` that Codex
//! must ignore, and a group whose first two handlers Codex skips -- because a port that only
//! agrees on the easy shape is a port that agrees by accident.
//!
//! The mirror of this test on the Python side is
//! `tests/scripts/session_replay/tests/test_codex_trust.py::FixtureParityTests`.

use std::{collections::BTreeMap, fs, path::PathBuf};

use sctx_installer::codex_trust;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codex-trust")
}

fn expected() -> Value {
    let bytes = fs::read(fixture_dir().join("expected.json")).expect("expected.json is readable");
    serde_json::from_slice(&bytes).expect("expected.json is JSON")
}

#[test]
fn the_rust_port_reproduces_every_hash_the_python_port_produced() {
    let document = fs::read(fixture_dir().join("hooks.json")).expect("hooks.json is readable");
    let document: Value = serde_json::from_slice(&document).expect("hooks.json is JSON");
    let document = document.as_object().expect("hooks.json is an object");
    let expected = expected();
    let key_source = expected["key_source"].as_str().expect("a key source");

    let computed = codex_trust::hook_state_entries(document, key_source).expect("hashable fixture");
    let expected_hashes = expected["trusted_hashes"]
        .as_object()
        .expect("a trusted_hashes object")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.as_str().expect("a hash string").to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        computed, expected_hashes,
        "the Rust and Python ports of Codex's hook trust hash disagree; regenerate with \
         `python3 tests/scripts/session_replay/codex_trust.py` only after confirming which side \
         is wrong"
    );
    assert_eq!(
        computed.len(),
        9,
        "the fixture is meant to exercise nine hashable handlers"
    );
}

#[test]
fn both_ports_name_the_same_upstream_commit() {
    // The hashes above are only meaningful as a reproduction of one specific Codex revision. If
    // the fixture is regenerated against a newer one, this fails until the Rust port's own
    // provenance constant is moved with it.
    assert_eq!(
        expected()["codex_source_commit"].as_str(),
        Some(codex_trust::CODEX_SOURCE_COMMIT)
    );
}

#[test]
fn skipped_handlers_own_an_address_without_owning_a_hash() {
    let document = fs::read(fixture_dir().join("hooks.json")).expect("hooks.json is readable");
    let document: Value = serde_json::from_slice(&document).expect("hooks.json is JSON");
    let document = document.as_object().expect("hooks.json is an object");
    let key_source = "/fixture/.codex/hooks.json";

    let addressed = codex_trust::addressed_keys(document, key_source).expect("addressable");
    let hashed = codex_trust::hook_state_entries(document, key_source).expect("hashable");
    assert_eq!(
        addressed.len(),
        hashed.len() + 2,
        "two handlers are skipped"
    );
    for skipped in [
        "/fixture/.codex/hooks.json:subagent_start:0:0",
        "/fixture/.codex/hooks.json:subagent_start:0:1",
    ] {
        assert!(
            addressed.contains(skipped),
            "{skipped} is still a position the file has, so pruning must not reclaim it"
        );
        assert!(!hashed.contains_key(skipped));
    }
}
