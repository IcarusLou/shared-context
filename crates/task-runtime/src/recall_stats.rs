//! Readonly installation-local injection and verdict metrics.

use crate::{SCHEMA_VERSION, invariant, io_error, sql_error};
use rusqlite::{Connection, OpenFlags};
use sctx_domain::Result;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OutcomeCounts {
    pub reused: u64,
    pub ignored: u64,
    pub refuted: u64,
}
impl OutcomeCounts {
    fn add(&mut self, outcome: &str, count: u64) -> Result<()> {
        match outcome {
            "reused" => self.reused += count,
            "ignored" => self.ignored += count,
            "refuted" => self.refuted += count,
            _ => return Err(invariant("unknown recall usage outcome")),
        }
        Ok(())
    }
    fn total(&self) -> u64 {
        self.reused + self.ignored + self.refuted
    }
    fn merge(&mut self, other: &Self) {
        self.reused += other.reused;
        self.ignored += other.ignored;
        self.refuted += other.refuted;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OutcomeBasisCounts {
    pub checkpoint_derived: OutcomeCounts,
    /// Conservative missing verdicts; ignored here means weak evidence of non-adoption.
    pub session_close: OutcomeCounts,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecallTotals {
    pub injections: u64,
    pub judged: u64,
    pub unjudged: u64,
    pub coverage_percent: Option<f64>,
    pub outcomes: OutcomeBasisCounts,
    pub strong_samples: u64,
    pub strong_reuse_rate_percent: Option<f64>,
}
impl RecallTotals {
    fn add(&mut self, count: u64, outcome: Option<&str>, basis: Option<&str>) -> Result<()> {
        self.injections += count;
        if let Some(outcome) = outcome {
            match basis {
                Some("checkpoint_derived") => {
                    self.outcomes.checkpoint_derived.add(outcome, count)?;
                }
                Some("session_close") => self.outcomes.session_close.add(outcome, count)?,
                _ => return Err(invariant("unknown recall usage evidence basis")),
            }
        }
        Ok(())
    }
    fn merge(&mut self, other: &Self) {
        self.injections += other.injections;
        self.outcomes
            .checkpoint_derived
            .merge(&other.outcomes.checkpoint_derived);
        self.outcomes
            .session_close
            .merge(&other.outcomes.session_close);
    }
    fn finish(&mut self) {
        self.strong_samples = self.outcomes.checkpoint_derived.total();
        self.judged = self.strong_samples + self.outcomes.session_close.total();
        self.unjudged = self.injections - self.judged;
        self.coverage_percent = percent(self.judged, self.injections);
        self.strong_reuse_rate_percent =
            percent(self.outcomes.checkpoint_derived.reused, self.strong_samples);
    }
}

#[allow(clippy::cast_precision_loss)]
fn percent(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator != 0).then(|| numerator as f64 / denominator as f64 * 100.0)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskRecallStats {
    pub task_id: String,
    pub external_session_id: String,
    pub agent_kind: String,
    pub totals: RecallTotals,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionRecallStats {
    pub external_session_id: String,
    pub agent_kind: String,
    pub tasks: u64,
    pub totals: RecallTotals,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecallStats {
    pub runtime_available: bool,
    pub schema_version: Option<i64>,
    pub totals: RecallTotals,
    pub by_task: Vec<TaskRecallStats>,
    pub by_session: Vec<SessionRecallStats>,
}

/// Reads a verified temporary snapshot; never opens the source through `SQLite` or initializes it.
///
/// # Errors
/// Returns typed I/O, concurrent-change, incompatible-schema or malformed-data errors.
pub fn read_recall_stats(root: impl AsRef<Path>) -> Result<RecallStats> {
    let source = root.as_ref().join("state/runtime.sqlite");
    let Some(snapshot) = snapshot(&source)? else {
        return Ok(RecallStats::default());
    };
    let connection = Connection::open_with_flags(
        snapshot.path().join("runtime.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_error("open recall statistics snapshot"))?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(sql_error("read recall statistics schema"))?;
    if version != SCHEMA_VERSION {
        return Err(invariant(format!(
            "recall stats requires Runtime schema {SCHEMA_VERSION}; found {version}; upgrade explicitly"
        )));
    }
    let mut statement = connection.prepare(
        "SELECT task.task_id, task.external_session_id, session.agent_kind,
                COUNT(injection.context_id), usage.outcome, usage.basis
         FROM task_session AS task
         JOIN external_session AS session ON session.external_session_id = task.external_session_id
         LEFT JOIN task_injection AS injection ON injection.task_id = task.task_id
         LEFT JOIN context_usage AS usage ON usage.task_id = injection.task_id
                                        AND usage.context_id = injection.context_id
         GROUP BY task.task_id, task.external_session_id, session.agent_kind, usage.outcome, usage.basis
         ORDER BY task.task_id, usage.basis, usage.outcome"
    ).map_err(sql_error("prepare recall statistics"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(sql_error("query recall statistics"))?;
    let mut tasks = BTreeMap::new();
    for row in rows {
        let (task_id, external_session_id, agent_kind, count, outcome, basis) =
            row.map_err(sql_error("read recall statistics row"))?;
        let task = tasks
            .entry(task_id.clone())
            .or_insert_with(|| TaskRecallStats {
                task_id,
                external_session_id,
                agent_kind,
                totals: RecallTotals::default(),
            });
        task.totals
            .add(count, outcome.as_deref(), basis.as_deref())?;
    }
    let mut stats = RecallStats {
        runtime_available: true,
        schema_version: Some(version),
        ..RecallStats::default()
    };
    let mut sessions = BTreeMap::new();
    for mut task in tasks.into_values() {
        task.totals.finish();
        stats.totals.merge(&task.totals);
        let session = sessions
            .entry(task.external_session_id.clone())
            .or_insert_with(|| SessionRecallStats {
                external_session_id: task.external_session_id.clone(),
                agent_kind: task.agent_kind.clone(),
                tasks: 0,
                totals: RecallTotals::default(),
            });
        session.tasks += 1;
        session.totals.merge(&task.totals);
        stats.by_task.push(task);
    }
    stats.totals.finish();
    for mut session in sessions.into_values() {
        session.totals.finish();
        stats.by_session.push(session);
    }
    Ok(stats)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn fingerprint(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect recall snapshot source")(error)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invariant("recall stats requires regular database files"));
    }
    let mut file = fs::File::open(path).map_err(io_error("open recall snapshot source"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(io_error("hash recall snapshot source"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(Some(digest.finalize().to_vec()))
}

fn fingerprints(database: &Path) -> Result<[Option<Vec<u8>>; 2]> {
    Ok([
        fingerprint(database)?,
        fingerprint(&sidecar(database, "-wal"))?,
    ])
}

fn snapshot(source: &Path) -> Result<Option<TempDir>> {
    if !source
        .try_exists()
        .map_err(io_error("inspect recall database"))?
    {
        return Ok(None);
    }
    for _ in 0..3 {
        if sidecar(source, "-journal").exists() {
            return Err(invariant(
                "recall stats refuses an active rollback journal; retry later",
            ));
        }
        let before = fingerprints(source)?;
        let temporary = tempfile::tempdir().map_err(io_error("create private recall snapshot"))?;
        let target = temporary.path().join("runtime.sqlite");
        let mut copied = true;
        for (index, suffix) in ["", "-wal"].into_iter().enumerate() {
            if before[index].is_some()
                && fs::copy(sidecar(source, suffix), sidecar(&target, suffix)).is_err()
            {
                copied = false;
                break;
            }
        }
        if copied
            && before[0].is_some()
            && before == fingerprints(source)?
            && before == fingerprints(&target)?
            && !sidecar(source, "-journal").exists()
        {
            return Ok(Some(temporary));
        }
    }
    Err(invariant(
        "Runtime changed while snapshotting recall stats; retry later",
    ))
}
