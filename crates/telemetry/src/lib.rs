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
/// repository is a Git checkout, so the no-Git outcome has no other way of being covered. The
/// probe lives in its own module so rust-analyzer sees one canonical home for it; `build.rs`
/// mounts the same file with `#[path]` because a build script cannot import its own crate.
#[cfg(test)]
mod build_fingerprint_probe;
