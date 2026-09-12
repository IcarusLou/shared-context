//! Stamps the built binary with the commit it came from.
//!
//! Three generations of this product have been misidentified from a version number alone, because
//! `0.2.0-dev.9` is what every build between two version bumps reports -- a release, a developer's
//! working tree, and an installed artifact from last week are indistinguishable. The fingerprint
//! below is what lets an installed binary say which source it is.

#[path = "src/build_fingerprint_probe.rs"]
mod build_fingerprint_probe;
use std::path::PathBuf;

use build_fingerprint_probe::{BuildFingerprint, probe_build_fingerprint};

fn main() {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let fingerprint = manifest_dir.as_deref().map_or_else(
        || BuildFingerprint {
            text: "unknown".to_owned(),
            rerun_paths: Vec::new(),
        },
        probe_build_fingerprint,
    );
    for path in &fingerprint.rerun_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=SCTX_BUILD_FINGERPRINT={}",
        fingerprint.text
    );
}
