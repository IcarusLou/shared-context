use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use sctx_telemetry::{EntryPoint, Event, Outcome};
use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode, Result, fs::read_bounded, spool::StoredEvent};

const MAX_INPUT_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_INPUT_FILES: usize = 10_000;
const MAX_TOTAL_INPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_REPORT_EVENTS: u64 = 1_000_000;
const MAX_REPORT_GROUPS: usize = 100_000;
const MAX_TRACE_EVENTS: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub files_scanned: u64,
    pub events_scanned: u64,
    pub rows: Vec<ReportRow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportRow {
    pub program_version: String,
    pub entry_point: EntryPoint,
    pub operation: String,
    pub count: u64,
    pub failures: u64,
    pub results_with_count: u64,
    pub empty_results: u64,
    pub failure_rate: f64,
    pub empty_result_rate: f64,
    pub duration_p50_ms: Option<u32>,
    pub duration_p95_ms: Option<u32>,
    pub duration_p99_ms: Option<u32>,
    pub error_counts: Vec<ErrorCount>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorCount {
    pub error_code: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub schema_version: u32,
    pub invocation_id: String,
    pub events: Vec<Event>,
}

#[derive(Default)]
struct Aggregate {
    count: u64,
    failures: u64,
    results_with_count: u64,
    empty: u64,
    durations: Vec<u32>,
    errors: BTreeMap<String, u64>,
}

/// Computes deterministic aggregate statistics from exported JSONL files.
///
/// # Errors
/// Returns an error for unsafe paths, oversized input, I/O failures, or malformed events.
pub fn report(input: &Path) -> Result<Report> {
    let files = input_files(input)?;
    let mut events_scanned = 0u64;
    let mut groups = BTreeMap::<(String, EntryPoint, String), Aggregate>::new();
    for file in &files {
        for stored in read_events(file)? {
            if events_scanned >= MAX_REPORT_EVENTS {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "analysis input exceeds event limit",
                ));
            }
            events_scanned = events_scanned.saturating_add(1);
            let event = stored.event;
            if event.outcome == Outcome::Started {
                continue;
            }
            let key = (
                event.program_version.clone(),
                event.entry_point,
                event
                    .operation
                    .clone()
                    .unwrap_or_else(|| "unspecified".to_owned()),
            );
            if !groups.contains_key(&key) && groups.len() >= MAX_REPORT_GROUPS {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "analysis input exceeds group limit",
                ));
            }
            let aggregate = groups.entry(key).or_default();
            aggregate.count = aggregate.count.saturating_add(1);
            if event.outcome == Outcome::Failure {
                aggregate.failures = aggregate.failures.saturating_add(1);
            }
            if let Some(result_count) = event.result_count {
                aggregate.results_with_count = aggregate.results_with_count.saturating_add(1);
                if result_count == 0 {
                    aggregate.empty = aggregate.empty.saturating_add(1);
                }
            }
            if let Some(error_code) = event.error_code {
                let current = aggregate.errors.get(&error_code).copied().unwrap_or(0);
                aggregate
                    .errors
                    .insert(error_code, current.saturating_add(1));
            }
            if let Some(duration) = event.duration_ms {
                aggregate.durations.push(duration);
            }
        }
    }
    let rows = groups
        .into_iter()
        .map(|((program_version, entry_point, operation), mut value)| {
            value.durations.sort_unstable();
            ReportRow {
                program_version,
                entry_point,
                operation,
                count: value.count,
                failures: value.failures,
                results_with_count: value.results_with_count,
                empty_results: value.empty,
                failure_rate: rate(value.failures, value.count),
                empty_result_rate: rate(value.empty, value.results_with_count),
                duration_p50_ms: percentile(&value.durations, 50),
                duration_p95_ms: percentile(&value.durations, 95),
                duration_p99_ms: percentile(&value.durations, 99),
                error_counts: value
                    .errors
                    .into_iter()
                    .map(|(error_code, count)| ErrorCount { error_code, count })
                    .collect(),
            }
        })
        .collect();
    Ok(Report {
        schema_version: 1,
        files_scanned: u64::try_from(files.len()).unwrap_or(u64::MAX),
        events_scanned,
        rows,
    })
}

/// Finds and orders every event carrying one invocation identity.
///
/// # Errors
/// Returns an error for an invalid identity, unsafe input, I/O failure, or malformed event.
pub fn trace(input: &Path, invocation_id: &str) -> Result<Trace> {
    if invocation_id.is_empty() || invocation_id.len() > 128 {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "invalid invocation identity",
        ));
    }
    let mut events = Vec::new();
    for file in input_files(input)? {
        for stored in read_events(&file)? {
            if stored.event.invocation_id == invocation_id {
                if events.len() >= MAX_TRACE_EVENTS {
                    return Err(Error::new(
                        ErrorCode::InvalidInput,
                        "trace exceeds result event limit",
                    ));
                }
                events.push(stored.event);
            }
        }
    }
    events.sort_by_key(|event| (event.occurred_at_unix_ms, event.sequence));
    Ok(Trace {
        schema_version: 1,
        invocation_id: invocation_id.to_owned(),
        events,
    })
}

fn input_files(input: &Path) -> Result<Vec<PathBuf>> {
    let metadata =
        fs::symlink_metadata(input).map_err(|error| Error::io("inspect analysis input", error))?;
    if metadata.file_type().is_symlink() {
        return Err(Error::new(
            ErrorCode::InvalidPath,
            "analysis input must not be a symlink",
        ));
    }
    if metadata.is_file() {
        return if input
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            Ok(vec![input.to_path_buf()])
        } else {
            Err(Error::new(
                ErrorCode::InvalidInput,
                "analysis input file must end in .jsonl",
            ))
        };
    }
    if !metadata.is_dir() {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "analysis input must be a directory or JSONL file",
        ));
    }
    let mut files = Vec::new();
    walk(input, input, 0, &mut files)?;
    files.sort();
    let mut total_bytes = 0u64;
    for file in &files {
        total_bytes = total_bytes.saturating_add(
            fs::metadata(file)
                .map_err(|error| Error::io("inspect analysis input", error))?
                .len(),
        );
        if total_bytes > MAX_TOTAL_INPUT_BYTES {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "analysis input exceeds total byte limit",
            ));
        }
    }
    Ok(files)
}

fn walk(root: &Path, directory: &Path, depth: usize, files: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 12 || files.len() >= MAX_INPUT_FILES {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "analysis input exceeds traversal limits",
        ));
    }
    for entry in fs::read_dir(directory).map_err(|error| Error::io("read analysis input", error))? {
        let entry = entry.map_err(|error| Error::io("read analysis entry", error))?;
        let path = entry.path();
        if !path.starts_with(root) {
            return Err(Error::new(
                ErrorCode::InvalidPath,
                "analysis entry escaped input root",
            ));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| Error::io("inspect analysis entry", error))?;
        if metadata.file_type().is_symlink() {
            return Err(Error::new(
                ErrorCode::InvalidPath,
                "analysis input contains a symlink",
            ));
        }
        if metadata.is_dir() {
            walk(root, &path, depth + 1, files)?;
        } else if metadata.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            if files.len() >= MAX_INPUT_FILES {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    "analysis input exceeds file limit",
                ));
            }
            files.push(path);
        }
    }
    Ok(())
}

fn read_events(path: &Path) -> Result<Vec<StoredEvent>> {
    let metadata = fs::metadata(path).map_err(|error| Error::io("inspect JSONL input", error))?;
    if metadata.len() > MAX_INPUT_FILE_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "JSONL input file exceeds size limit",
        ));
    }
    let bytes = read_bounded(path, MAX_INPUT_FILE_BYTES, ErrorCode::InvalidInput)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "JSONL input has incomplete tail",
        ));
    }
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).map_err(|error| {
                Error::new(
                    ErrorCode::InvalidInput,
                    format!("invalid JSONL event: {error}"),
                )
            })
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn rate(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn percentile(values: &[u32], percentile: usize) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    let index = (values.len().saturating_sub(1) * percentile).div_ceil(100);
    values.get(index).copied()
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn flat_directories_and_sparse_totals_are_bounded_before_reading_content() {
        let many = TempDir::new().expect("many files");
        for index in 0..=MAX_INPUT_FILES {
            File::create(many.path().join(format!("{index}.jsonl"))).expect("create input");
        }
        assert_eq!(
            input_files(many.path())
                .expect_err("file count limit")
                .code(),
            ErrorCode::InvalidInput
        );

        let large = TempDir::new().expect("large input");
        for name in ["one.jsonl", "two.jsonl"] {
            File::create(large.path().join(name))
                .expect("create sparse input")
                .set_len(MAX_TOTAL_INPUT_BYTES / 2 + 1)
                .expect("size sparse input");
        }
        assert_eq!(
            input_files(large.path())
                .expect_err("total byte limit")
                .code(),
            ErrorCode::InvalidInput
        );
    }
}
