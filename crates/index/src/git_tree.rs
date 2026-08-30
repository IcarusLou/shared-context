use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use sctx_domain::{Error, ErrorKind, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TreeBlob {
    pub(crate) path: String,
    pub(crate) oid: String,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TreeEntry {
    pub(crate) path: String,
    pub(crate) oid: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeadTree {
    pub(crate) oid: String,
    pub(crate) blobs: Vec<TreeBlob>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TreeChange {
    Added { path: String, oid: String },
    Modified { path: String, oid: String },
    Deleted { path: String },
    Renamed { old_path: String, new_path: String },
}

impl TreeChange {
    pub(crate) fn is_addition(&self) -> bool {
        matches!(self, Self::Added { .. })
    }

    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Added { path, .. } | Self::Modified { path, .. } | Self::Deleted { path } => path,
            Self::Renamed { new_path, .. } => new_path,
        }
    }
}

pub(crate) fn tree_oid(repository: &Path) -> Result<String> {
    output_text(repository, ["rev-parse", "HEAD^{tree}"])
}

/// Resolves the `HEAD` commit from Git's own files, without spawning a process.
///
/// This is only ever a cache key for [`tree_oid`]: a commit's Tree is immutable, so a `HEAD` that
/// still names the same commit still names the same Tree. Any layout this cannot read returns
/// `None`, which simply costs the `git` process it was trying to avoid.
pub(crate) fn head_commit_oid(repository: &Path) -> Option<String> {
    let git_dir = resolve_git_dir(repository)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim().to_owned();
    if head.is_empty() {
        return None;
    }
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return Some(head);
    };
    let mut bases = vec![git_dir.clone()];
    if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
        let common = common.trim();
        if !common.is_empty() {
            let common = Path::new(common);
            bases.push(if common.is_absolute() {
                common.to_path_buf()
            } else {
                git_dir.join(common)
            });
        }
    }
    for base in &bases {
        if let Ok(loose) = fs::read_to_string(base.join(reference)) {
            let oid = loose.trim();
            if !oid.is_empty() {
                return Some(oid.to_owned());
            }
        }
        if let Some(oid) = packed_reference(&base.join("packed-refs"), reference) {
            return Some(oid);
        }
    }
    None
}

fn resolve_git_dir(repository: &Path) -> Option<PathBuf> {
    let dot_git = repository.join(".git");
    let metadata = fs::metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    let pointer = fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        repository.join(target)
    })
}

fn packed_reference(packed_refs: &Path, reference: &str) -> Option<String> {
    let packed = fs::read_to_string(packed_refs).ok()?;
    packed.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('^') {
            return None;
        }
        let (oid, name) = line.split_once(' ')?;
        (name.trim() == reference).then(|| oid.trim().to_owned())
    })
}

pub(crate) fn tree_exists(repository: &Path, oid: &str) -> bool {
    run(
        repository,
        [
            OsString::from("cat-file"),
            OsString::from("-e"),
            OsString::from(format!("{oid}^{{tree}}")),
        ],
    )
    .is_ok()
}

pub(crate) fn read_tree(repository: &Path, oid: &str) -> Result<HeadTree> {
    let entries = list_tree(repository, oid)?;
    let blobs = entries
        .into_iter()
        .map(|entry| {
            let bytes = read_blob(repository, &entry.oid)?;
            Ok(TreeBlob {
                path: entry.path,
                oid: entry.oid,
                bytes,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(HeadTree {
        oid: oid.to_owned(),
        blobs,
    })
}

pub(crate) fn list_head(repository: &Path) -> Result<(String, Vec<TreeEntry>)> {
    let oid = tree_oid(repository)?;
    let entries = list_tree(repository, &oid)?;
    Ok((oid, entries))
}

pub(crate) fn read_blob(repository: &Path, oid: &str) -> Result<Vec<u8>> {
    Ok(run(
        repository,
        [
            OsString::from("cat-file"),
            OsString::from("blob"),
            OsString::from(oid),
        ],
    )?
    .stdout)
}

/// One observed addition of one append-only Event path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventAddition {
    pub(crate) commit_oid: String,
    pub(crate) commit_time: i64,
}

/// Every addition of every append-only Event path, read in one `git log` pass.
///
/// Introducing commit and publication time are the same fact about the same walk, so both are
/// answered by a single process instead of one process per Event. Additions of one path are
/// returned oldest first, so the head of the list is the authoritative introduction.
pub(crate) fn introducing_commits(
    repository: &Path,
) -> Result<BTreeMap<String, Vec<EventAddition>>> {
    let output = output_text(
        repository,
        [
            OsString::from("log"),
            OsString::from("--format=%x01%H %ct"),
            OsString::from("--diff-filter=A"),
            OsString::from("--name-only"),
            OsString::from("--no-renames"),
            OsString::from("--"),
            OsString::from("events/"),
        ],
    )?;
    let mut additions: BTreeMap<String, Vec<EventAddition>> = BTreeMap::new();
    let mut current: Option<EventAddition> = None;
    for line in output.lines() {
        if let Some(header) = line.strip_prefix('\u{1}') {
            let mut fields = header.split_ascii_whitespace();
            current = match (fields.next(), fields.next()) {
                (Some(commit_oid), Some(stamp)) => {
                    stamp.parse::<i64>().ok().map(|commit_time| EventAddition {
                        commit_oid: commit_oid.to_owned(),
                        commit_time,
                    })
                }
                _ => None,
            };
            continue;
        }
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        if let Some(addition) = current.clone() {
            additions.entry(path.to_owned()).or_default().push(addition);
        }
    }
    for entries in additions.values_mut() {
        entries.sort_by(|left, right| {
            (left.commit_time, &left.commit_oid).cmp(&(right.commit_time, &right.commit_oid))
        });
        entries.dedup();
    }
    Ok(additions)
}

pub(crate) fn diff_trees(
    repository: &Path,
    old_oid: &str,
    new_oid: &str,
) -> Result<Vec<TreeChange>> {
    let old: BTreeMap<_, _> = list_tree(repository, old_oid)?
        .into_iter()
        .map(|entry| (entry.path, entry.oid))
        .collect();
    let new: BTreeMap<_, _> = list_tree(repository, new_oid)?
        .into_iter()
        .map(|entry| (entry.path, entry.oid))
        .collect();

    let mut deleted: BTreeMap<String, String> = old
        .iter()
        .filter(|(path, _)| !new.contains_key(*path))
        .map(|(path, oid)| (path.clone(), oid.clone()))
        .collect();
    let mut additions: Vec<_> = new
        .iter()
        .filter(|(path, _)| !old.contains_key(*path))
        .map(|(path, oid)| (path.clone(), oid.clone()))
        .collect();
    let mut changes = Vec::new();

    // Exact-blob renames are unambiguous. Rename-with-edit still safely appears as D+A, which
    // has the same full-rebuild behavior required for any append-only protocol bypass.
    let mut remaining_additions = Vec::new();
    for (path, oid) in additions.drain(..) {
        if let Some((old_path, _)) = deleted.iter().find(|(_, old_oid)| **old_oid == oid) {
            let old_path = old_path.clone();
            deleted.remove(&old_path);
            changes.push(TreeChange::Renamed {
                old_path,
                new_path: path,
            });
        } else {
            remaining_additions.push((path, oid));
        }
    }
    for (path, oid) in remaining_additions {
        changes.push(TreeChange::Added { path, oid });
    }
    for (path, oid) in &new {
        if old.get(path).is_some_and(|old_oid| old_oid != oid) {
            changes.push(TreeChange::Modified {
                path: path.clone(),
                oid: oid.clone(),
            });
        }
    }
    for path in deleted.into_keys() {
        changes.push(TreeChange::Deleted { path });
    }
    changes.sort_by(|left, right| left.path().cmp(right.path()));
    Ok(changes)
}

fn list_tree(repository: &Path, oid: &str) -> Result<Vec<TreeEntry>> {
    let listing = run(
        repository,
        [
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--full-tree"),
            OsString::from(oid),
            OsString::from("--"),
            OsString::from("events"),
            OsString::from("objects"),
        ],
    )?
    .stdout;

    let mut entries = Vec::new();
    for record in listing
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let separator = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| external("git ls-tree returned a record without a path separator"))?;
        let (header, path_with_separator) = record.split_at(separator);
        let path = &path_with_separator[1..];
        let header = std::str::from_utf8(header)
            .map_err(|error| external(format!("git ls-tree header is not UTF-8: {error}")))?;
        let path = std::str::from_utf8(path)
            .map_err(|error| external(format!("managed Git path is not UTF-8: {error}")))?;
        let mut fields = header.split_ascii_whitespace();
        let _mode = fields.next();
        let kind = fields.next();
        let blob_oid = fields.next();
        if fields.next().is_some() || kind != Some("blob") {
            return Err(external(format!(
                "managed HEAD entry is not a blob: {path}"
            )));
        }
        entries.push(TreeEntry {
            path: path.to_owned(),
            oid: blob_oid
                .ok_or_else(|| external("git ls-tree record is missing an object ID"))?
                .to_owned(),
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn output_text<I, S>(repository: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run(repository, args)?;
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|error| external(format!("git output is not UTF-8: {error}")))
}

fn run<I, S>(repository: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(|error| external(format!("failed to execute git: {error}")))?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(external(if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        format!("git exited with {}: {stderr}", output.status)
    }))
}

fn external(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::External, message)
}
