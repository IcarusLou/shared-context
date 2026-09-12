#![forbid(unsafe_code)]

//! Bounded, best-effort telemetry transport. The implementation deliberately has
//! no dependency on any Shared Context business crate.

mod client;
mod event;
mod wire;

/// This build's identity: the package version plus the commit it was built from.
///
/// `0.2.0-dev.9 (0e54982, clean)`, or `0.2.0-dev.9 (unknown)` where the build had no Git to ask.
///
/// A version number alone cannot tell a release apart from a developer's working tree or from an
/// artifact installed a week earlier, and every crate in this workspace shares one version, so
/// this crate's `CARGO_PKG_VERSION` is the workspace's. Every surface that reports which build is
/// running -- `sctx --version` and the `program_version` on every telemetry Event -- reads this
/// one constant, so they cannot drift apart.
///
/// The shape is chosen so that the telemetry normalizer's token filter, which keeps
/// alphanumerics, `-`, `_`, `.`, `:`, and spaces, reduces it to `0.2.0-dev.9 0e54982 clean`
/// within the 32-byte field bound rather than truncating any part of it away.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("SCTX_BUILD_FINGERPRINT"),
    ")"
);

pub use client::{EmitOutcome, default_endpoint, default_logs_root, emit_to};
pub use event::{Authorization, EntryPoint, Event, EventKind, Outcome, new_invocation_id};
pub use wire::{DecodeError, FRAME_MAGIC, FRAME_MAX_BYTES, FRAME_VERSION, FrameDecoder};

/// The build-script probe, compiled a second time so both of its outcomes can be tested.
///
/// Building this crate can only ever exercise the outcome of the tree it is built from, and this
/// repository is a Git checkout, so the no-Git outcome has no other way of being covered.
#[cfg(test)]
mod build_fingerprint {
    include!("../build_fingerprint.rs");

    /// A directory inside this repository reports the commit it is checked out at.
    #[test]
    fn a_git_checkout_reports_its_commit_and_worktree_state() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let probed = probe_build_fingerprint(manifest_dir);
        let expected = git_stdout(manifest_dir, &["rev-parse", "--short=7", "HEAD"])
            .expect("this test runs from a Git checkout with at least one commit");

        let (commit, state) = probed
            .text
            .split_once(", ")
            .unwrap_or_else(|| panic!("fingerprint is not `<commit>, <state>`: {}", probed.text));
        assert_eq!(commit, expected);
        assert!(
            matches!(state, "clean" | "dirty"),
            "worktree state was not read: {state}"
        );
        assert!(
            probed
                .rerun_paths
                .iter()
                .any(|path| path.ends_with("HEAD") && path.exists()),
            "an existing HEAD must be declared so a new commit restamps the binary: {:?}",
            probed.rerun_paths
        );
    }

    /// A source tree outside every Git checkout still builds; it just stops claiming a commit.
    ///
    /// The temporary directory is created under the system temporary directory rather than in the
    /// workspace, because a directory *inside* the repository would inherit the repository's Git
    /// answers and test nothing.
    #[test]
    fn a_tree_without_git_reports_unknown_and_declares_no_rerun_paths() {
        let outside = tempfile::tempdir().expect("create a directory outside every checkout");

        let probed = probe_build_fingerprint(outside.path());

        assert_eq!(probed.text, "unknown");
        assert!(
            probed.rerun_paths.is_empty(),
            "a build with nothing to watch must not declare paths Cargo would treat as \
             permanently changed: {:?}",
            probed.rerun_paths
        );
    }
}
