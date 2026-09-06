//! Periodic installation maintenance -- the cycle work nobody remembers to do by hand.
//!
//! An installation accumulates three kinds of drift between the interactive commands that create
//! knowledge: the Engineering Graph falls behind checkouts that moved, Candidate Reviews sit
//! Pending until they quietly expire, and the Knowledge Store's work branch diverges from the
//! team's default branch. Each already has an explicit command. What was missing is one thing to
//! run on a cycle that performs the safe part, *counts* the part only a human may decide, and
//! leaves a durable record of what it found.
//!
//! ## What it deliberately does not do
//!
//! - **No disposition.** ADR-0005 puts every Candidate confirmation and discard behind an explicit
//!   act -- a human's, or the session Agent's inside a server-verified surface. A scheduled job is
//!   neither. So steps 2 and 3 count and stop; the digest is the handoff, not the decision.
//! - **No `doctor --fix`.** That reinstalls the runtime and rewrites Agent configuration. Those are
//!   repairs an operator asks for, not side effects a timer is allowed to have.
//! - **No embedding warm.** It costs a 9--12 second model load and about a gigabyte of resident
//!   memory, and a long-lived `sctx mcp serve` is already filling the same `semantic.sqlite` --
//!   two writers would lose each other's rows for no gain.
//!
//! ## Lock shape
//!
//! Steps 1--3 run under one *shared* maintenance lease, exactly like the interactive commands they
//! wrap, so a maintenance run and an editor's in-flight MCP call coexist. The lease is released
//! before step 4, because `knowledge sync` takes the *exclusive* lease and would otherwise deadlock
//! against the run's own shared one. That release is also why step 4 can find the installation busy
//! and has a retry schedule at all.
//!
//! ## Two failure vocabularies
//!
//! `skipped` means "this installation does not have this work to do" -- a locally bootstrapped
//! Knowledge Store has no remote to sync with, and an opportunistic run that finds the lease taken
//! deliberately steps aside. `failed` means the step had work to do and did not finish it. Only
//! `failed` reaches `sctx doctor` as a warning, which is what keeps a laptop that has never been
//! near a Knowledge Store remote from reporting a problem it does not have.

use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sctx_local_state::{MaintenanceLock, UserConfigStore};
use sctx_mcp::AssociationRebuildInput;
use sctx_task_runtime::TaskRuntime;
use serde::{Deserialize, Serialize};

use crate::{
    Error, ErrorKind, Installer, KnowledgeStoreSource, ProjectionIndex, Result, atomic_write,
    ensure_private_directory, invalid, io_error, read_manifest, require_existing_installation,
    validate_context,
};

/// Digest shape version. Bumped only when an existing field changes meaning, never when one is
/// added: every field is optional on read, so an older reader tolerates a newer file.
pub const MAINTAIN_DIGEST_SCHEMA_VERSION: u32 = 1;

/// How far ahead a Pending Review counts as "expiring". One week is the shortest horizon on which
/// a person who reads the digest weekly still has a chance to act before the Review retires itself.
pub const CANDIDATE_EXPIRY_HORIZON_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Default wait before each `knowledge sync` retry when the installation is busy.
///
/// One entry per retry, so the default is four attempts spread over three and a half minutes. The
/// exclusive lease is held by whole operations (a setup, another sync, a data reset), and those
/// finish in seconds to a minute; waiting minutes costs a scheduled run nothing and converts most
/// collisions into a completed sync instead of a reported failure.
pub(crate) const MAINTAIN_SYNC_BACKOFF: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];

/// Step names, stable across versions because the digest is read by people and by `doctor`.
pub const STEP_ASSOCIATION_REBUILD: &str = "association_rebuild";
pub const STEP_CANDIDATE_SURVEY: &str = "candidate_survey";
pub const STEP_PROVISIONAL_SPACE_SURVEY: &str = "provisional_space_survey";
pub const STEP_KNOWLEDGE_SYNC: &str = "knowledge_sync";
pub const STEP_LOGS_SYNC: &str = "logs_sync";

/// What a maintenance run was allowed to cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MaintainOptions {
    /// Take what is free and leave immediately otherwise: one `knowledge sync` attempt, no waiting.
    pub opportunistic: bool,
}

/// Which budget the recorded run was operating under.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintainMode {
    #[default]
    Scheduled,
    Opportunistic,
}

/// Result of one maintenance step.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintainOutcome {
    Ok,
    /// Nothing to do here on this installation, or deliberately stepped aside.
    #[default]
    Skipped,
    Failed,
}

/// One recorded step of one run.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MaintainStep {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub outcome: MaintainOutcome,
    /// Why, for anything other than a plain `ok`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Attempts spent, including the first. Only `knowledge_sync` ever exceeds one.
    #[serde(default)]
    pub attempts: u32,
    /// Set when the step stopped on something no retry can clear -- today, a merge conflict.
    #[serde(default)]
    pub needs_human: bool,
    /// Wall-clock time spent in this step. Older digests deserialize this as zero.
    #[serde(default)]
    pub duration_ms: u64,
}

/// Everything one run observed but did not act on.
///
/// New counters are added here as the automatic triage surface grows; every field is optional on
/// read, so a digest written by an older version still loads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MaintainCounts {
    /// Pending Candidate Reviews across the whole installation.
    #[serde(default)]
    pub pending_candidate_reviews: u64,
    /// Subset of the above whose retention window closes within [`candidate_expiry_horizon_seconds`](Self::candidate_expiry_horizon_seconds).
    #[serde(default)]
    pub expiring_candidate_reviews: u64,
    #[serde(default)]
    pub candidate_expiry_horizon_seconds: u64,
    /// Spaces still carrying the server's provisional Intent, awaiting a human's naming.
    #[serde(default)]
    pub provisional_spaces: u64,
    #[serde(default)]
    pub engineering_references: u64,
    /// References the rebuild left Ambiguous, Missing, or Unavailable.
    #[serde(default)]
    pub unresolved_references: u64,
    /// Renames local history states beside a Missing Reference; each is a repair a human decides.
    #[serde(default)]
    pub relocation_candidates: u64,
}

/// The durable record one run leaves behind at `state/maintain-digest.json`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MaintainDigest {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub mode: MaintainMode,
    #[serde(default)]
    pub started_at_unix_seconds: u64,
    #[serde(default)]
    pub finished_at_unix_seconds: u64,
    #[serde(default)]
    pub steps: Vec<MaintainStep>,
    #[serde(default)]
    pub counts: MaintainCounts,
    /// Artifact Generation of the Engineering Graph this run rebuilt, absent when step 1 failed.
    #[serde(default)]
    pub graph_generation: Option<String>,
    #[serde(default)]
    pub projection_generation: u64,
}

impl MaintainDigest {
    /// Steps that had work to do and did not finish it.
    #[must_use]
    pub fn failed_steps(&self) -> Vec<&MaintainStep> {
        self.steps
            .iter()
            .filter(|step| step.outcome == MaintainOutcome::Failed)
            .collect()
    }
}

/// What `sctx maintain status` reads back.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct MaintainStatus {
    pub root: PathBuf,
    pub digest_path: PathBuf,
    pub last_run_path: PathBuf,
    /// Absent on an installation that has never run maintenance -- a normal state, not a fault.
    pub last_run_at_unix_seconds: Option<u64>,
    pub digest: Option<MaintainDigest>,
}

impl Installer {
    /// Runs one periodic maintenance cycle and records what it found.
    ///
    /// Every step is independently fault-tolerant: a failure is recorded and the run continues, so
    /// one unreachable remote never costs the installation its Graph rebuild. The returned digest
    /// is the same value written to `state/maintain-digest.json`.
    ///
    /// # Errors
    ///
    /// Returns an error only when the installation itself is unusable (invalid context, missing
    /// installation) or the digest cannot be persisted. Step failures are reported, not raised.
    pub fn maintain(&self, options: &MaintainOptions) -> Result<MaintainDigest> {
        validate_context(&self.context)?;
        require_existing_installation(&self.context.root)?;
        let root = &self.context.root;
        let started_at = unix_seconds()?;
        let mut steps = Vec::new();
        let mut counts = MaintainCounts {
            candidate_expiry_horizon_seconds: CANDIDATE_EXPIRY_HORIZON_SECONDS,
            ..MaintainCounts::default()
        };
        let mut graph_generation = None;
        let mut projection_generation = 0;

        // Steps 1-3 share one lease for the whole group. Re-taking it per step would let an
        // exclusive operation slip in between two observations and report counts from two
        // different installations.
        match MaintenanceLock::open_or_create(root).and_then(|lock| lock.try_shared()) {
            Ok(guard) => {
                let step_started = Instant::now();
                match sctx_mcp::association_rebuild_at_root(
                    root,
                    &AssociationRebuildInput {
                        diagnose_only: false,
                    },
                ) {
                    Ok(response) => {
                        counts.engineering_references = as_u64(response.reference_count);
                        counts.unresolved_references = as_u64(
                            response.status_counts.ambiguous
                                + response.status_counts.missing
                                + response.status_counts.unavailable,
                        );
                        counts.relocation_candidates = as_u64(response.relocation_candidates.len());
                        graph_generation = Some(response.artifact_generation);
                        projection_generation = response.generation;
                        steps.push(succeeded(STEP_ASSOCIATION_REBUILD));
                    }
                    Err(error) => steps.push(failed(STEP_ASSOCIATION_REBUILD, &error)),
                }
                finish_step_duration(&mut steps, step_started);
                let step_started = Instant::now();
                match TaskRuntime::initialize(root).and_then(|runtime| {
                    runtime.survey_candidate_reviews(CANDIDATE_EXPIRY_HORIZON_SECONDS)
                }) {
                    Ok(survey) => {
                        counts.pending_candidate_reviews = survey.pending_count;
                        counts.expiring_candidate_reviews = survey.expiring_soon_count;
                        steps.push(succeeded(STEP_CANDIDATE_SURVEY));
                    }
                    Err(error) => steps.push(failed(STEP_CANDIDATE_SURVEY, &error)),
                }
                finish_step_duration(&mut steps, step_started);
                let step_started = Instant::now();
                match provisional_space_count(root) {
                    Ok(count) => {
                        counts.provisional_spaces = count;
                        steps.push(succeeded(STEP_PROVISIONAL_SPACE_SURVEY));
                    }
                    Err(error) => steps.push(failed(STEP_PROVISIONAL_SPACE_SURVEY, &error)),
                }
                finish_step_duration(&mut steps, step_started);
                drop(guard);
            }
            Err(error) => {
                for name in [
                    STEP_ASSOCIATION_REBUILD,
                    STEP_CANDIDATE_SURVEY,
                    STEP_PROVISIONAL_SPACE_SURVEY,
                ] {
                    steps.push(MaintainStep {
                        name: name.to_owned(),
                        outcome: MaintainOutcome::Skipped,
                        reason: Some(error.message().to_owned()),
                        attempts: 1,
                        needs_human: false,
                        duration_ms: 0,
                    });
                }
            }
        }

        // Start the independent logging subprocess after the shared business lease is gone. It
        // has its own process-group deadline and logging lock, so it can run concurrently with
        // knowledge sync without holding or waiting for any business lock.
        let logs_sync = self.spawn_maintain_logs_sync();
        steps.push(self.maintain_knowledge_sync(*options));
        steps.push(join_logs_sync(logs_sync));

        let digest = MaintainDigest {
            schema_version: MAINTAIN_DIGEST_SCHEMA_VERSION,
            mode: if options.opportunistic {
                MaintainMode::Opportunistic
            } else {
                MaintainMode::Scheduled
            },
            started_at_unix_seconds: started_at,
            finished_at_unix_seconds: unix_seconds()?,
            steps,
            counts,
            graph_generation,
            projection_generation,
        };
        emit_maintenance_steps(&digest.steps, &self.logs_root());
        write_digest(root, &digest)?;
        // The log is a convenience, never the record: the digest above is already durable, and a
        // logs directory an operator removed must not turn a completed run into a failed one.
        let _ = append_log_line(root, &digest);
        Ok(digest)
    }

    /// Reads back the last recorded maintenance run.
    ///
    /// # Errors
    ///
    /// Returns an error when the installation is missing or the digest exists but is unreadable.
    pub fn maintain_status(&self) -> Result<MaintainStatus> {
        validate_context(&self.context)?;
        require_existing_installation(&self.context.root)?;
        let root = &self.context.root;
        Ok(MaintainStatus {
            root: root.clone(),
            digest_path: digest_path(root),
            last_run_path: last_run_path(root),
            last_run_at_unix_seconds: read_last_run(root)?,
            digest: read_digest(root)?,
        })
    }

    /// Runs `knowledge sync` under the run's retry budget, translating every ending into a step.
    fn maintain_knowledge_sync(&self, options: MaintainOptions) -> MaintainStep {
        let started = Instant::now();
        let mut step = self.maintain_knowledge_sync_inner(options);
        step.duration_ms = elapsed_millis(started);
        step
    }

    fn maintain_knowledge_sync_inner(&self, options: MaintainOptions) -> MaintainStep {
        match read_manifest(&self.context.root) {
            Ok(Some(manifest))
                if matches!(
                    manifest.knowledge_store,
                    KnowledgeStoreSource::Remote { .. }
                ) => {}
            Ok(_) => {
                // A locally bootstrapped Store has no remote and `sync_knowledge` rejects it as
                // Unsupported. That is a property of the installation, not a fault in this run.
                return MaintainStep {
                    name: STEP_KNOWLEDGE_SYNC.to_owned(),
                    outcome: MaintainOutcome::Skipped,
                    reason: Some(
                        "Knowledge Store is local, so there is no remote to synchronize with"
                            .to_owned(),
                    ),
                    attempts: 0,
                    needs_human: false,
                    duration_ms: 0,
                };
            }
            Err(error) => return failed(STEP_KNOWLEDGE_SYNC, &error),
        }

        let backoff: &[Duration] = if options.opportunistic {
            &[]
        } else {
            &self.maintain_sync_backoff
        };
        let mut attempts = 0;
        loop {
            attempts += 1;
            let Err(error) = self.sync_knowledge() else {
                let mut step = succeeded(STEP_KNOWLEDGE_SYNC);
                step.attempts = attempts;
                return step;
            };
            // A conflict is the one sync ending a retry cannot improve: the merge already aborted
            // and rolled back, and the resolution is a person's.
            if error.kind() == ErrorKind::Conflict {
                let mut step = failed(STEP_KNOWLEDGE_SYNC, &error);
                step.attempts = attempts;
                step.needs_human = true;
                return step;
            }
            if error.kind() != ErrorKind::MaintenanceBusy {
                let mut step = failed(STEP_KNOWLEDGE_SYNC, &error);
                step.attempts = attempts;
                return step;
            }
            let Some(wait) = backoff.get(attempts as usize - 1) else {
                let mut step = if options.opportunistic {
                    // Stepping aside is the entire point of the mode, so it is not a fault.
                    MaintainStep {
                        name: STEP_KNOWLEDGE_SYNC.to_owned(),
                        outcome: MaintainOutcome::Skipped,
                        reason: Some(error.message().to_owned()),
                        attempts,
                        needs_human: false,
                        duration_ms: 0,
                    }
                } else {
                    failed(STEP_KNOWLEDGE_SYNC, &error)
                };
                step.attempts = attempts;
                return step;
            };
            thread::sleep(*wait);
        }
    }

    fn spawn_maintain_logs_sync(&self) -> LogsSyncTask {
        let logs_root = self.logs_root();
        let config = match sctx_log_service::load_config(&logs_root) {
            Ok(config) => config,
            Err(error) if error.code() == sctx_log_service::ErrorCode::NotConfigured => {
                return LogsSyncTask::Ready(skipped_logs("logging is not configured"));
            }
            Err(_) => {
                return LogsSyncTask::Ready(failed_logs("logging configuration is invalid"));
            }
        };
        if !config.enabled {
            return LogsSyncTask::Ready(skipped_logs("logging is disabled"));
        }
        if !config.sync.on_maintain {
            return LogsSyncTask::Ready(skipped_logs(
                "logging synchronization on maintenance is disabled",
            ));
        }
        if !config.is_assigned() {
            return LogsSyncTask::Ready(skipped_logs(
                "logging has no upload remote and email assignment",
            ));
        }
        let executable = self.context.root.join("bin/current/sctx");
        let timeout = sctx_log_sync::SyncOptions::from_config(&config)
            .deadline
            .saturating_add(Duration::from_secs(5));
        match thread::Builder::new()
            .name("sctx-logs-sync".to_owned())
            .spawn(move || {
                let started = Instant::now();
                let spec = sctx_log_sync::runner::CommandSpec::new(executable, timeout)
                    .args([
                        "logs".into(),
                        "sync".into(),
                        "--logs-root".into(),
                        logs_root.into_os_string(),
                        "--json".into(),
                    ])
                    .output_limit(4096);
                let mut step = match sctx_log_sync::runner::run(&spec) {
                    Ok(output) if output.status.success() => parse_logs_sync_report(&output.stdout),
                    Ok(_) => failed_logs("logging synchronization exited unsuccessfully"),
                    Err(sctx_log_sync::runner::RunnerError::TimedOut { .. }) => {
                        failed_logs("logging synchronization exceeded its total deadline")
                    }
                    Err(_) => failed_logs("logging synchronization process was unavailable"),
                };
                step.duration_ms = elapsed_millis(started);
                step
            }) {
            Ok(handle) => LogsSyncTask::Running(handle),
            Err(_) => LogsSyncTask::Ready(failed_logs(
                "logging synchronization supervisor could not start",
            )),
        }
    }

    fn logs_root(&self) -> PathBuf {
        let process_home = std::env::var_os("HOME").map(PathBuf::from);
        if process_home.as_deref() == Some(self.context.home.as_path()) {
            sctx_telemetry::default_logs_root()
                .unwrap_or_else(|| self.context.home.join(".shared-context-logs"))
        } else {
            // Injected installers must never escape their injected home and observe or mutate the
            // real user's logging service during an in-process test or managed invocation.
            self.context.home.join(".shared-context-logs")
        }
    }
}

enum LogsSyncTask {
    Ready(MaintainStep),
    Running(JoinHandle<MaintainStep>),
}

fn join_logs_sync(task: LogsSyncTask) -> MaintainStep {
    match task {
        LogsSyncTask::Ready(step) => step,
        LogsSyncTask::Running(handle) => handle
            .join()
            .unwrap_or_else(|_| failed_logs("logging synchronization supervisor failed")),
    }
}

fn skipped_logs(reason: &str) -> MaintainStep {
    MaintainStep {
        name: STEP_LOGS_SYNC.to_owned(),
        outcome: MaintainOutcome::Skipped,
        reason: Some(reason.to_owned()),
        attempts: 0,
        needs_human: false,
        duration_ms: 0,
    }
}

fn failed_logs(reason: &str) -> MaintainStep {
    MaintainStep {
        name: STEP_LOGS_SYNC.to_owned(),
        outcome: MaintainOutcome::Failed,
        reason: Some(reason.to_owned()),
        attempts: 1,
        needs_human: false,
        duration_ms: 0,
    }
}

fn parse_logs_sync_report(bytes: &[u8]) -> MaintainStep {
    let Ok(report) = serde_json::from_slice::<sctx_log_sync::SyncReport>(bytes) else {
        return failed_logs("logging synchronization returned an invalid bounded report");
    };
    match report.outcome {
        sctx_log_sync::SyncOutcome::Uploaded => {
            let mut step = succeeded(STEP_LOGS_SYNC);
            if !report.active_included {
                step.reason = Some(format!(
                    "uploaded {} sealed batch(es); the active batch was not included ({:?})",
                    report.uploaded_batches, report.seal_status
                ));
            }
            step
        }
        sctx_log_sync::SyncOutcome::Partial => MaintainStep {
            name: STEP_LOGS_SYNC.to_owned(),
            outcome: MaintainOutcome::Ok,
            reason: Some(format!(
                "uploaded {} batch(es); {} remain for a later maintenance run",
                report.uploaded_batches, report.remaining_batches
            )),
            attempts: 1,
            needs_human: false,
            duration_ms: 0,
        },
        sctx_log_sync::SyncOutcome::NoReady => skipped_logs(&format!(
            "no sealed logging batches were ready; active batch included: {} ({:?})",
            report.active_included, report.seal_status
        )),
        sctx_log_sync::SyncOutcome::SkippedBusy => {
            skipped_logs("another logging synchronization owns the independent log lock")
        }
    }
}

fn emit_maintenance_steps(steps: &[MaintainStep], logs_root: &Path) {
    let invocation_id = sctx_telemetry::new_invocation_id();
    for (sequence, step) in steps.iter().enumerate() {
        let mut event = sctx_telemetry::Event::finished(
            sctx_telemetry::EntryPoint::Maintenance,
            sctx_telemetry::EventKind::MaintenanceStepFinished,
            invocation_id.clone(),
            step.name.clone(),
            match step.outcome {
                MaintainOutcome::Ok => sctx_telemetry::Outcome::Success,
                MaintainOutcome::Skipped => sctx_telemetry::Outcome::Disabled,
                MaintainOutcome::Failed => sctx_telemetry::Outcome::Failure,
            },
        );
        event.sequence = u32::try_from(sequence).unwrap_or(u32::MAX);
        event.duration_ms = Some(u32::try_from(step.duration_ms).unwrap_or(u32::MAX));
        if step.outcome == MaintainOutcome::Failed {
            event.error_code = Some("maintenance_step_failed".to_owned());
        }
        let _ = sctx_telemetry::emit_to(logs_root, &event);
    }
}

fn provisional_space_count(root: &Path) -> Result<u64> {
    let config = UserConfigStore::open_existing(root)?;
    let index = ProjectionIndex::new(config.repository(), config.root().join("state"));
    // A conflicted Space has no winning Intent head and the projection records it as not
    // provisional, so this counts exactly the Spaces one server-proposed Intent is still naming.
    let snapshot = index.query_snapshot(|connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM space_projection WHERE provisional = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| {
                Error::new(ErrorKind::Io, format!("count provisional Spaces: {error}"))
            })
    })?;
    Ok(u64::try_from(snapshot.data).unwrap_or(0))
}

fn finish_step_duration(steps: &mut [MaintainStep], started: Instant) {
    if let Some(step) = steps.last_mut() {
        step.duration_ms = elapsed_millis(started);
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn succeeded(name: &str) -> MaintainStep {
    MaintainStep {
        name: name.to_owned(),
        outcome: MaintainOutcome::Ok,
        reason: None,
        attempts: 1,
        needs_human: false,
        duration_ms: 0,
    }
}

fn failed(name: &str, error: &Error) -> MaintainStep {
    MaintainStep {
        name: name.to_owned(),
        outcome: MaintainOutcome::Failed,
        reason: Some(error.message().to_owned()),
        attempts: 1,
        needs_human: false,
        duration_ms: 0,
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Path of the durable digest one run leaves behind.
#[must_use]
pub fn digest_path(root: &Path) -> PathBuf {
    root.join("state/maintain-digest.json")
}

/// Path of the single-line marker recording when maintenance last completed.
#[must_use]
pub fn last_run_path(root: &Path) -> PathBuf {
    root.join("state/maintain-last-run")
}

/// Reads the last recorded digest, or `None` when maintenance has never run here.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn read_digest(root: &Path) -> Result<Option<MaintainDigest>> {
    let path = digest_path(root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read maintenance digest")(error)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| invalid(format!("invalid maintenance digest: {error}")))
}

/// Reads the completion time of the last maintenance run, or `None` when there was none.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn read_last_run(root: &Path) -> Result<Option<u64>> {
    let path = last_run_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read maintenance last-run marker")(error)),
    };
    text.trim()
        .parse::<u64>()
        .map(Some)
        .map_err(|_| invalid("maintenance last-run marker is not a Unix second count"))
}

fn write_digest(root: &Path, digest: &MaintainDigest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(digest).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("serialize maintenance digest: {error}"),
        )
    })?;
    atomic_write(&digest_path(root), &bytes, 0o600)?;
    atomic_write(
        &last_run_path(root),
        format!("{}\n", digest.finished_at_unix_seconds).as_bytes(),
        0o600,
    )
}

fn append_log_line(root: &Path, digest: &MaintainDigest) -> Result<()> {
    let logs = root.join("logs");
    ensure_private_directory(&logs)?;
    let (year, month, day) = civil_date(digest.finished_at_unix_seconds);
    let path = logs.join(format!("maintain-{year:04}{month:02}{day:02}.log"));
    let summary = digest
        .steps
        .iter()
        .map(|step| {
            let outcome = match step.outcome {
                MaintainOutcome::Ok => "ok",
                MaintainOutcome::Skipped => "skipped",
                MaintainOutcome::Failed => "failed",
            };
            format!("{}={outcome}", step.name)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mode = match digest.mode {
        MaintainMode::Scheduled => "scheduled",
        MaintainMode::Opportunistic => "opportunistic",
    };
    let line = format!(
        "{} mode={mode} duration_s={} {summary} pending_reviews={} expiring_reviews={} \
         provisional_spaces={} unresolved_references={}\n",
        digest.finished_at_unix_seconds,
        digest
            .finished_at_unix_seconds
            .saturating_sub(digest.started_at_unix_seconds),
        digest.counts.pending_candidate_reviews,
        digest.counts.expiring_candidate_reviews,
        digest.counts.provisional_spaces,
        digest.counts.unresolved_references,
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(io_error("open maintenance log"))?;
    file.write_all(line.as_bytes())
        .map_err(io_error("append maintenance log"))
}

fn unix_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| invalid("system clock is before the Unix epoch"))
}

/// Civil date of a Unix second count in UTC.
///
/// UTC, not local time, so a log file name means the same thing on every machine that later reads
/// it. Howard Hinnant's `civil_from_days`, which is exact for the whole range this ever sees.
fn civil_date(unix_seconds: u64) -> (i64, u32, u32) {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_matches_known_instants() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(1_756_944_000), (2025, 9, 4));
        // A leap day, which is where an off-by-one in the era arithmetic would show.
        assert_eq!(civil_date(1_709_164_800), (2024, 2, 29));
    }

    #[test]
    fn every_digest_field_is_optional_on_read() {
        let digest: MaintainDigest = serde_json::from_str("{}").unwrap();
        assert_eq!(digest, MaintainDigest::default());
        assert_eq!(digest.mode, MaintainMode::Scheduled);
        assert!(digest.failed_steps().is_empty());
        // A digest written by a newer version that added a counter still loads here.
        let forward: MaintainDigest = serde_json::from_str(
            r#"{"schema_version":1,"counts":{"pending_candidate_reviews":3},
                "steps":[{"name":"knowledge_sync","outcome":"failed","reason":"boom"}]}"#,
        )
        .unwrap();
        assert_eq!(forward.counts.pending_candidate_reviews, 3);
        assert_eq!(forward.failed_steps().len(), 1);
    }
}
