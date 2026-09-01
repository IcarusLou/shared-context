//! Read-only local history lookup for a File or Module Reference that stopped resolving.
//!
//! Resolution never guesses. When an exact locator is absent from the Repository snapshot the
//! answer is `Missing`, and no amount of similarity may turn that into an association -- a Context
//! is a claim about a specific Artifact, and a Context silently reattached to a different one is
//! worse than a Context that resolves to nothing.
//!
//! That principle is about resolution, not about diagnosis. A rename recorded in the Repository's
//! own history is not a similarity heuristic: the Repository states that this exact path became
//! that exact path in this exact commit. This module reads that statement and reports it, and
//! nothing here ever writes a Reference, resolves one, or feeds an `ArtifactResolution`. The
//! repair stays an explicit human act through `engineering_reference_record`.

use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use sctx_domain::{ArtifactLocator, RepoRelativePath};
use serde::{Deserialize, Serialize};

/// Most bytes read back from one bounded history command.
const MAX_GIT_OUTPUT_BYTES: u64 = 64 * 1024;
/// Poll interval while waiting for one bounded history command.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One rename the Repository's own history states, offered as a diagnosis and never applied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RelocationCandidate {
    /// The Repository-relative path the Reference names.
    pub from: String,
    /// The Repository-relative path history says it became.
    pub to: String,
    /// The commit that performed the rename.
    pub commit: String,
}

/// The only locators this lookup answers for: the ones whose identity is a path.
///
/// A Symbol, Api, Schema, or Test locator carries a path as one component of a compound identity,
/// and a file rename tells you nothing about whether the symbol inside it survived. Reporting a
/// relocation for those would be exactly the guess this module refuses to make.
#[must_use]
pub const fn relocatable_path(locator: &ArtifactLocator) -> Option<&RepoRelativePath> {
    match locator {
        ArtifactLocator::File { path } | ArtifactLocator::Module { path } => Some(path),
        ArtifactLocator::Api { .. }
        | ArtifactLocator::Schema { .. }
        | ArtifactLocator::Symbol { .. }
        | ArtifactLocator::Test { .. } => None,
    }
}

/// Finds the one rename `path` followed, or nothing.
///
/// Two bounded read-only commands: the commit that removed the path from the tracked tree, and
/// that commit's rename entries. A commit that shows several renames out of the same path cannot
/// exist, so "several candidates" here means several distinct destinations across the answer --
/// and the answer to an ambiguous question is no answer. Every failure mode (no local `git`, a
/// path that was never tracked, a path that still exists, a plain delete, a merge commit, an
/// exhausted budget) returns `None`, because this is a diagnosis and not a result.
#[must_use]
pub fn find_relocation_candidate(
    checkout_path: &Path,
    path: &RepoRelativePath,
    budget: Duration,
) -> Option<RelocationCandidate> {
    let deadline = Instant::now().checked_add(budget)?;
    let spec = path.as_str();
    let commit = bounded_git(
        checkout_path,
        &[
            "log",
            "--max-count=1",
            "--diff-filter=D",
            "--format=%H",
            "--",
            spec,
        ],
        deadline,
    )?;
    let commit = commit.trim();
    if commit.is_empty()
        || !commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    let renames = bounded_git(
        checkout_path,
        &[
            "diff-tree",
            "-r",
            "-M",
            "--name-status",
            "--no-commit-id",
            "--diff-filter=R",
            commit,
        ],
        deadline,
    )?;
    let mut destinations = renames
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let status = fields.next()?;
            let from = fields.next()?;
            let to = fields.next()?;
            (status.starts_with('R') && from == spec).then(|| to.to_owned())
        })
        .collect::<Vec<_>>();
    destinations.sort();
    destinations.dedup();
    let [destination] = destinations.as_slice() else {
        return None;
    };
    Some(RelocationCandidate {
        from: spec.to_owned(),
        to: destination.clone(),
        commit: commit.to_owned(),
    })
}

/// Runs one read-only local history command, killing it rather than overrunning `deadline`.
fn bounded_git(root: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut output = String::new();
    child
        .stdout
        .take()?
        .take(MAX_GIT_OUTPUT_BYTES)
        .read_to_string(&mut output)
        .ok()?;
    Some(output)
}
