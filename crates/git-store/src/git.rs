use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use sctx_domain::{Error, ErrorKind, Result};

pub(crate) struct Git<'a> {
    repository: &'a Path,
}

impl<'a> Git<'a> {
    pub(crate) const fn new(repository: &'a Path) -> Self {
        Self { repository }
    }

    pub(crate) fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command.arg("-C").arg(self.repository);
        command.args(args);
        let output = command.output().map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("failed to execute git: {error}"),
            )
        })?;
        if output.status.success() {
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(Error::new(
            ErrorKind::External,
            if stderr.is_empty() {
                format!("git exited with {}", output.status)
            } else {
                format!("git exited with {}: {stderr}", output.status)
            },
        ))
    }

    pub(crate) fn output_text<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(args)?;
        String::from_utf8(output.stdout)
            .map(|text| text.trim().to_owned())
            .map_err(|error| {
                Error::new(
                    ErrorKind::External,
                    format!("git returned non-UTF-8 output: {error}"),
                )
            })
    }

    pub(crate) fn output_bytes<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(args).map(|output| output.stdout)
    }

    pub(crate) fn stage_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec![OsString::from("add"), OsString::from("--")];
        args.extend(paths.iter().map(OsString::from));
        self.run(args).map(|_| ())
    }

    pub(crate) fn commit_paths(&self, message: &str, paths: &[String]) -> Result<()> {
        let mut args = vec![
            OsString::from("commit"),
            OsString::from("--only"),
            OsString::from("-m"),
            OsString::from(message),
            OsString::from("--"),
        ];
        args.extend(paths.iter().map(OsString::from));
        self.run(args).map(|_| ())
    }

    pub(crate) fn head_oid(&self) -> Result<String> {
        self.output_text(["rev-parse", "--verify", "HEAD"])
    }

    pub(crate) fn staged_tree_oid(&self) -> Result<String> {
        self.output_text(["write-tree"])
    }

    pub(crate) fn head_paths(&self, root: &str) -> Result<Vec<String>> {
        let bytes = self.output_bytes([
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--name-only"),
            OsString::from("HEAD"),
            OsString::from("--"),
            OsString::from(root),
        ])?;
        bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                std::str::from_utf8(path)
                    .map(ToOwned::to_owned)
                    .map_err(|error| {
                        Error::new(
                            ErrorKind::External,
                            format!("git returned a non-UTF-8 path: {error}"),
                        )
                    })
            })
            .collect()
    }

    pub(crate) fn head_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.file_at("HEAD", path)
    }

    pub(crate) fn file_at(&self, revision: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let spec = format!("{revision}:{path}");
        let exists = Command::new("git")
            .arg("-C")
            .arg(self.repository)
            .args(["cat-file", "-e"])
            .arg(&spec)
            .output()
            .map_err(|error| {
                Error::new(
                    ErrorKind::External,
                    format!("failed to execute git cat-file: {error}"),
                )
            })?;
        if !exists.status.success() {
            return Ok(None);
        }
        self.output_bytes([OsString::from("show"), OsString::from(spec)])
            .map(Some)
    }

    pub(crate) fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.repository)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .output()
            .map_err(|error| {
                Error::new(
                    ErrorKind::External,
                    format!("failed to execute git merge-base: {error}"),
                )
            })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::new(
                ErrorKind::External,
                format!(
                    "git merge-base failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            )),
        }
    }

    pub(crate) fn index_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let spec = format!(":{path}");
        let output = Command::new("git")
            .arg("-C")
            .arg(self.repository)
            .arg("show")
            .arg(spec)
            .output()
            .map_err(|error| {
                Error::new(
                    ErrorKind::External,
                    format!("failed to execute git show: {error}"),
                )
            })?;
        if output.status.success() {
            Ok(Some(output.stdout))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn changed_from_head(&self) -> Result<Vec<NameStatus>> {
        let bytes = self.output_bytes([
            "diff",
            "--name-status",
            "-z",
            "-M",
            "HEAD",
            "--",
            "events",
            "objects",
            "schemas",
        ])?;
        parse_name_status(&bytes)
    }

    pub(crate) fn staged_from_head(&self) -> Result<Vec<NameStatus>> {
        let bytes = self.output_bytes([
            "diff",
            "--cached",
            "--name-status",
            "-z",
            "--no-renames",
            "HEAD",
            "--",
        ])?;
        parse_name_status(&bytes)
    }
}

#[derive(Debug)]
pub(crate) struct NameStatus {
    pub(crate) status: String,
    pub(crate) paths: Vec<PathBuf>,
}

fn parse_name_status(bytes: &[u8]) -> Result<Vec<NameStatus>> {
    let fields: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = std::str::from_utf8(fields[index]).map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("git returned a non-UTF-8 status: {error}"),
            )
        })?;
        index += 1;
        let path_count = usize::from(status.starts_with('R') || status.starts_with('C')) + 1;
        if index + path_count > fields.len() {
            return Err(Error::new(
                ErrorKind::External,
                "git returned a truncated name-status record",
            ));
        }
        let mut paths = Vec::with_capacity(path_count);
        for raw in &fields[index..index + path_count] {
            let path = std::str::from_utf8(raw).map_err(|error| {
                Error::new(
                    ErrorKind::External,
                    format!("git returned a non-UTF-8 path: {error}"),
                )
            })?;
            paths.push(PathBuf::from(path));
        }
        index += path_count;
        entries.push(NameStatus {
            status: status.to_owned(),
            paths,
        });
    }
    Ok(entries)
}
