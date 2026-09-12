// The Git build fingerprint probe.
//
// This file is `include!`d twice on purpose: once by `build.rs`, which runs it at compile time
// and turns the result into `SCTX_BUILD_FINGERPRINT`, and once by a `#[cfg(test)]` module in the
// library, which is the only way to test both of its outcomes without building the crate twice.
// A build script cannot import the crate it builds, and the no-Git outcome is exactly the one a
// test in this repository can never reach by building normally -- the repository is a Git
// checkout.
//
// Nothing here may fail a build. A source tree with no `git` on `PATH`, no `.git` at all, or a
// repository with no commit yet still has to compile; it simply reports `unknown` and stops
// claiming to know which commit it came from.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What the probe learned about the checkout a build is running from.
struct BuildFingerprint {
    /// `0e54982, clean`, `0e54982, dirty`, `0e54982, unknown`, or `unknown`.
    text: String,
    /// Existing paths whose change should re-run the build script. Only paths that exist are
    /// listed: Cargo treats a missing `rerun-if-changed` path as permanently changed, which would
    /// re-run this probe on every single build of every downstream crate.
    rerun_paths: Vec<PathBuf>,
}

/// Reads the commit and worktree state of the checkout containing `manifest_dir`.
///
/// The dirty flag counts tracked files only. An untracked file is usually editor or tool residue
/// and would make nearly every developer build report `dirty` for reasons that never reached the
/// binary; a tracked modification is the case this flag exists to catch.
fn probe_build_fingerprint(manifest_dir: &Path) -> BuildFingerprint {
    let unknown = || BuildFingerprint {
        text: "unknown".to_owned(),
        rerun_paths: Vec::new(),
    };
    let Some(commit) = git_stdout(manifest_dir, &["rev-parse", "--short=7", "HEAD"])
        .filter(|commit| is_short_commit(commit))
    else {
        return unknown();
    };
    let state = match git_stdout(
        manifest_dir,
        &["status", "--porcelain", "--untracked-files=no"],
    ) {
        Some(changes) if changes.is_empty() => "clean",
        Some(_) => "dirty",
        // `rev-parse` answered and `status` did not. Saying `unknown` for the half that failed is
        // the only honest option: `clean` would be a claim nothing verified.
        None => "unknown",
    };
    BuildFingerprint {
        text: format!("{commit}, {state}"),
        rerun_paths: rerun_paths(manifest_dir),
    }
}

/// Paths that change when `HEAD` moves.
///
/// `git rev-parse --git-path` is used rather than joining a git directory by hand because it
/// resolves correctly inside a linked worktree, where `HEAD` and `index` live in the worktree's
/// own directory while refs live in the common one.
///
/// This tracks the commit exactly. It does not track the dirty flag exactly: editing a file in
/// another crate does not touch any of these paths, so a fingerprint that said `clean` can
/// survive an edit. `index` covers the transitions that matter for anything that gets installed,
/// because `git add` and `git commit` both rewrite it.
fn rerun_paths(manifest_dir: &Path) -> Vec<PathBuf> {
    let mut names = vec!["HEAD".to_owned(), "index".to_owned()];
    if let Some(reference) = git_stdout(manifest_dir, &["symbolic-ref", "--quiet", "HEAD"]) {
        names.push(reference);
    }
    names
        .iter()
        .filter_map(|name| git_stdout(manifest_dir, &["rev-parse", "--git-path", name]))
        .map(|path| absolute_against(manifest_dir, Path::new(&path)))
        .filter(|path| path.exists())
        .collect()
}

fn absolute_against(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Runs one read-only Git command, returning its trimmed stdout only on a clean exit.
///
/// `--no-optional-locks` keeps `status` from refreshing the index as a side effect: a build must
/// not write into the developer's repository, and a concurrent build must not contend for
/// `index.lock`.
fn git_stdout(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|stdout| stdout.trim().to_owned())
}

fn is_short_commit(commit: &str) -> bool {
    commit.len() == 7 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}
