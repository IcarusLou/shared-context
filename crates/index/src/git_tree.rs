use std::{
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
pub(crate) struct HeadTree {
    pub(crate) oid: String,
    pub(crate) blobs: Vec<TreeBlob>,
}

pub(crate) fn tree_oid(repository: &Path) -> Result<String> {
    output_text(repository, ["rev-parse", "HEAD^{tree}"])
}

pub(crate) fn read_head(repository: &Path) -> Result<HeadTree> {
    let oid = tree_oid(repository)?;
    let listing = run(
        repository,
        [
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--full-tree"),
            OsString::from(&oid),
            OsString::from("--"),
            OsString::from("events"),
            OsString::from("objects"),
        ],
    )?
    .stdout;

    let mut blobs = Vec::new();
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
        let blob_oid = blob_oid
            .ok_or_else(|| external("git ls-tree record is missing an object ID"))?
            .to_owned();
        let bytes = run(
            repository,
            [
                OsString::from("cat-file"),
                OsString::from("blob"),
                OsString::from(&blob_oid),
            ],
        )?
        .stdout;
        blobs.push(TreeBlob {
            path: path.to_owned(),
            oid: blob_oid,
            bytes,
        });
    }
    Ok(HeadTree { oid, blobs })
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
