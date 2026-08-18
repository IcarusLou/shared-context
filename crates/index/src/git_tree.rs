use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::Path,
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
