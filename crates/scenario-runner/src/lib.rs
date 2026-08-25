//! Isolated, test-only black-box execution for dynamic Shared Context scenarios.
//!
//! The runner consumes the contract from `sctx-scenario-contract`, compiles every fault and
//! dependency decision before creating a sandbox, starts only an explicitly configured `sctx`
//! executable, and retains only typed captures. It does not evaluate product invariants or copy
//! retrieval, Evidence, Space selection, safety, or governance logic. The read-only observer uses
//! fixed storage selectors and a copied `SQLite` WAL snapshot, never product initialization APIs.

mod error;
mod observer;
mod process;
mod report;
mod schedule;

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

pub use error::{FailureClassification, RunnerFailure};
pub use observer::{
    ObservationEntity, ObservationSummary, ObservedGeneration, ObserverError, ObserverErrorKind,
    ProvenObservation, ReadOnlyObserver, StateFingerprint, state_fingerprint,
};
use process::{
    CommandContext, McpManager, McpToolOutcome, ProcessError, ProcessErrorKind, binary_basename,
    parse_json_output, run_one_shot,
};
pub use report::{
    ReplayClassification, ReplayDiagnosticCode, ReplayDisposition, ReplayExecutor, ReplayHarness,
    ReplayReport, ReplayReportItem, ReplayScenario,
};
use schedule::{PlannedDisposition, PlannedStep, ScheduledBatch, compile_schedule};
use sctx_local_state::{PrivacyScanner, UserConfigStore};
use sctx_scenario_contract::{
    ActionExpectation, ActionKind, ActorKind, AgentProfile, AgentVendor, ConfirmationEventCount,
    CrashTiming, ExpectedFailureKind, InvariantKind, ProductAction, RawContentKind, SandboxBuiltin,
    ScenarioAction, ScenarioDefinition, TemplateValue, VariableKind, VariableName, parse_scenario,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::{Builder as TempDirBuilder, TempDir};
use uuid::{Variant, Version};

const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_STEP_TIMEOUT: Duration = Duration::from_millis(10);
const MAX_STEP_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CAPTURED_TEXT_BYTES: usize = 512;

/// Explicit execution policy. No executable is discovered from an Agent installation.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    sctx_binary: PathBuf,
    git_binary: PathBuf,
    step_timeout: Duration,
    sandbox_parent: Option<PathBuf>,
    search_path: OsString,
}

impl RunnerConfig {
    #[must_use]
    pub fn new(sctx_binary: impl Into<PathBuf>, git_binary: impl Into<PathBuf>) -> Self {
        Self {
            sctx_binary: sctx_binary.into(),
            git_binary: git_binary.into(),
            step_timeout: DEFAULT_STEP_TIMEOUT,
            sandbox_parent: None,
            search_path: env::var_os("PATH").unwrap_or_default(),
        }
    }

    #[must_use]
    pub const fn with_step_timeout(mut self, timeout: Duration) -> Self {
        self.step_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_sandbox_parent(mut self, parent: impl Into<PathBuf>) -> Self {
        self.sandbox_parent = Some(parent.into());
        self
    }
}

/// One typed runtime capture. Its JSON value has already passed [`VariableKind`] validation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedValue {
    pub value_type: VariableKind,
    pub value: Value,
}

/// Stable disposition of one scheduled action occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Completed,
    ExpectedFailure,
    Dropped,
    Barrier,
    CrashedBefore,
    CompletedThenCrashed,
    Restarted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepRecord {
    pub step: String,
    pub occurrence: u8,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

/// One closed invariant result. Diagnostic codes are fixed and contain no observed values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssertionRecord {
    pub id: String,
    pub passed: bool,
    pub diagnostic_code: String,
}

/// Deterministic, raw-output-free run result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOutcome {
    pub scenario: String,
    pub seed: u64,
    pub steps: Vec<StepRecord>,
    pub variables: BTreeMap<String, CapturedValue>,
    pub assertions: Vec<AssertionRecord>,
}

/// Completed run plus its still-live isolated filesystem for test inspection.
pub struct ScenarioRun {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    workspace: PathBuf,
    pub outcome: RunOutcome,
}

impl ScenarioRun {
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

#[derive(Clone, Debug)]
pub struct ScenarioRunner {
    config: RunnerConfig,
}

impl ScenarioRunner {
    #[must_use]
    pub const fn new(config: RunnerConfig) -> Self {
        Self { config }
    }

    /// Parse, preflight, and execute one serialized scenario.
    ///
    /// Contract and static fault/capture failures occur before a temp directory or process exists.
    ///
    /// # Errors
    ///
    /// Returns a sanitized failure without child stderr, template input, or response output.
    pub fn run_bytes(&self, bytes: &[u8], seed: u64) -> Result<ScenarioRun, RunnerFailure> {
        let scenario = parse_scenario(bytes)
            .map_err(|_| RunnerFailure::invalid(seed, "scenario contract validation failed"))?;
        self.run(&scenario, seed)
    }

    /// Preflight and execute one already parsed scenario.
    ///
    /// # Errors
    ///
    /// Returns a sanitized contract, policy, process, timeout, capture, or observer failure.
    pub fn run(
        &self,
        scenario: &ScenarioDefinition,
        seed: u64,
    ) -> Result<ScenarioRun, RunnerFailure> {
        scenario.validate().map_err(|_| {
            RunnerFailure::new(
                scenario.name.as_str(),
                seed,
                None,
                FailureClassification::InvalidScenario,
                "scenario contract validation failed",
            )
        })?;
        self.validate_policy(scenario, seed)?;
        preflight_resources(scenario, seed)?;
        let schedule = compile_schedule(scenario, seed)?;
        preflight_action_shapes(scenario, seed)?;
        preflight_parallel_mcp(scenario, seed, &schedule)?;
        preflight_assertion_schedule(scenario, seed, &schedule)?;

        let sandbox = Sandbox::create(&self.config, scenario, seed)?;
        let context = CommandContext {
            binary: self.config.sctx_binary.clone(),
            home: sandbox.home.clone(),
            root: sandbox.root.clone(),
            temporary: sandbox.temporary_path.clone(),
            workspace: sandbox.workspace.clone(),
            search_path: self.config.search_path.clone(),
            resource_roots: sandbox.resource_roots.clone(),
            resource_files: sandbox.resource_files.clone(),
        };
        let observer = ReadOnlyObserver::new(&self.config.git_binary);
        let mcp = scenario
            .actors
            .iter()
            .filter(|actor| matches!(actor.kind, ActorKind::Session))
            .map(|actor| {
                (
                    actor.id.as_str().to_owned(),
                    Arc::new(Mutex::new(McpManager::new(
                        context.clone(),
                        scenario.agent.vendor,
                        scenario.agent.framing,
                        self.config.step_timeout,
                    ))),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut state = ExecutionState {
            variables: BTreeMap::new(),
            records: Vec::new(),
            facts: BTreeMap::new(),
            canaries: BTreeMap::new(),
        };
        for batch in &schedule {
            execute_batch(
                scenario,
                seed,
                batch,
                &context,
                &observer,
                &mcp,
                self.config.step_timeout,
                &sandbox.root,
                &mut state,
            )?;
        }
        drop(mcp);
        let assertions = evaluate_assertions(scenario, seed, &state, &sandbox.root)?;
        Ok(ScenarioRun {
            _temporary: sandbox.temporary,
            home: sandbox.home,
            root: sandbox.root,
            workspace: sandbox.workspace,
            outcome: RunOutcome {
                scenario: scenario.name.as_str().to_owned(),
                seed,
                steps: state.records,
                variables: state.variables,
                assertions,
            },
        })
    }

    fn validate_policy(
        &self,
        scenario: &ScenarioDefinition,
        seed: u64,
    ) -> Result<(), RunnerFailure> {
        if !(MIN_STEP_TIMEOUT..=MAX_STEP_TIMEOUT).contains(&self.config.step_timeout) {
            return Err(policy_failure(
                scenario,
                seed,
                "step timeout is outside the runner policy",
            ));
        }
        if self.config.sctx_binary.as_os_str().is_empty()
            || self.config.git_binary.as_os_str().is_empty()
        {
            return Err(policy_failure(
                scenario,
                seed,
                "runner executables must be explicitly configured",
            ));
        }
        if binary_basename(&self.config.sctx_binary).is_some_and(|name| {
            matches!(
                name.trim_end_matches(".exe"),
                "codex" | "cursor" | "cursor-agent" | "claude" | "claude-code"
            )
        }) {
            return Err(policy_failure(
                scenario,
                seed,
                "model or Agent executables are forbidden by runner policy",
            ));
        }
        if let Some(parent) = &self.config.sandbox_parent
            && (!parent.is_dir()
                || fs::symlink_metadata(parent)
                    .map_or(true, |metadata| metadata.file_type().is_symlink()))
        {
            return Err(policy_failure(
                scenario,
                seed,
                "sandbox parent is unavailable or unsafe",
            ));
        }
        Ok(())
    }
}

struct Sandbox {
    temporary: TempDir,
    temporary_path: PathBuf,
    home: PathBuf,
    root: PathBuf,
    workspace: PathBuf,
    resource_roots: BTreeMap<String, PathBuf>,
    resource_files: BTreeMap<(String, String), PathBuf>,
}

impl Sandbox {
    fn create(
        config: &RunnerConfig,
        definition: &ScenarioDefinition,
        seed: u64,
    ) -> Result<Self, RunnerFailure> {
        let scenario = definition.name.as_str();
        let mut builder = TempDirBuilder::new();
        builder.prefix("sctx-scenario-");
        let temporary = config
            .sandbox_parent
            .as_ref()
            .map_or_else(|| builder.tempdir(), |parent| builder.tempdir_in(parent));
        let temporary = temporary.map_err(|_| {
            RunnerFailure::new(
                scenario,
                seed,
                None,
                FailureClassification::ProcessFailure,
                "isolated scenario directory could not be created",
            )
        })?;
        let temporary_path = temporary.path().to_path_buf();
        let home = temporary_path.join("home");
        let workspace = temporary_path.join("workspace");
        let temporary_files = temporary_path.join("tmp");
        for directory in [&home, &workspace, &temporary_files] {
            fs::create_dir(directory).map_err(|_| {
                RunnerFailure::new(
                    scenario,
                    seed,
                    None,
                    FailureClassification::ProcessFailure,
                    "isolated scenario layout could not be created",
                )
            })?;
        }
        let mut sandbox = Self {
            temporary,
            temporary_path: temporary_files,
            root: home.join(".shared-context"),
            home,
            workspace,
            resource_roots: BTreeMap::new(),
            resource_files: BTreeMap::new(),
        };
        initialize_sandbox_knowledge_store(config, definition, seed, &sandbox.root)?;
        sandbox.materialize_resources(config, definition, seed)?;
        Ok(sandbox)
    }

    fn materialize_resources(
        &mut self,
        config: &RunnerConfig,
        scenario: &ScenarioDefinition,
        seed: u64,
    ) -> Result<(), RunnerFailure> {
        let base = self.workspace.join("resources");
        if !scenario.resources.is_empty() {
            fs::create_dir(&base).map_err(|_| resource_failure(scenario, seed))?;
        }
        for resource in &scenario.resources {
            let root = base.join(resource.id.as_str());
            fs::create_dir(&root).map_err(|_| resource_failure(scenario, seed))?;
            for file in &resource.files {
                let path = root.join(file.path.as_str());
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|_| resource_failure(scenario, seed))?;
                }
                let mut output = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .map_err(|_| resource_failure(scenario, seed))?;
                output
                    .write_all(file.content.as_bytes())
                    .map_err(|_| resource_failure(scenario, seed))?;
                self.resource_files.insert(
                    (
                        resource.id.as_str().to_owned(),
                        file.path.as_str().to_owned(),
                    ),
                    fs::canonicalize(path).map_err(|_| resource_failure(scenario, seed))?,
                );
            }
            if resource.synthetic_git {
                initialize_resource_git(config, scenario, seed, &root)?;
            }
            self.resource_roots.insert(
                resource.id.as_str().to_owned(),
                fs::canonicalize(root).map_err(|_| resource_failure(scenario, seed))?,
            );
        }
        Ok(())
    }
}

fn initialize_sandbox_knowledge_store(
    config: &RunnerConfig,
    scenario: &ScenarioDefinition,
    seed: u64,
    root: &Path,
) -> Result<(), RunnerFailure> {
    let store = UserConfigStore::initialize(root).map_err(|_| {
        RunnerFailure::new(
            scenario.name.as_str(),
            seed,
            None,
            FailureClassification::ProcessFailure,
            "isolated Knowledge Store config could not be initialized",
        )
    })?;
    fs::create_dir_all(root.join("state/pending")).map_err(|_| resource_failure(scenario, seed))?;
    let repository = store.repository();
    fs::create_dir_all(repository).map_err(|_| resource_failure(scenario, seed))?;
    for arguments in [
        &[
            "init",
            "--quiet",
            "--initial-branch=main",
            repository
                .to_str()
                .ok_or_else(|| resource_failure(scenario, seed))?,
        ][..],
        &[
            "-C",
            repository
                .to_str()
                .ok_or_else(|| resource_failure(scenario, seed))?,
            "config",
            "user.name",
            "Shared Context Writer",
        ][..],
        &[
            "-C",
            repository
                .to_str()
                .ok_or_else(|| resource_failure(scenario, seed))?,
            "config",
            "user.email",
            "shared-context@localhost",
        ][..],
        &[
            "-C",
            repository
                .to_str()
                .ok_or_else(|| resource_failure(scenario, seed))?,
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Initialize Shared Context repository",
        ][..],
    ] {
        let status = Command::new(&config.git_binary)
            .args(arguments)
            .env_clear()
            .env("PATH", &config.search_path)
            .env("HOME", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| resource_failure(scenario, seed))?;
        if !status.success() {
            return Err(resource_failure(scenario, seed));
        }
    }
    Ok(())
}

fn preflight_resources(scenario: &ScenarioDefinition, seed: u64) -> Result<(), RunnerFailure> {
    let scanner = PrivacyScanner::default();
    for resource in &scenario.resources {
        for file in &resource.files {
            let lowercase = file.content.to_ascii_lowercase();
            if [
                "/users/",
                "\\users\\",
                "transcript_path",
                ".codex/sessions",
                ".cursor/",
            ]
            .iter()
            .any(|marker| lowercase.contains(marker))
            {
                return Err(RunnerFailure::new(
                    scenario.name.as_str(),
                    seed,
                    None,
                    FailureClassification::PolicyViolation,
                    "sandbox resource resembles local session material",
                ));
            }
            let scan = scanner.scan(&file.content).map_err(|_| {
                RunnerFailure::new(
                    scenario.name.as_str(),
                    seed,
                    None,
                    FailureClassification::PolicyViolation,
                    "sandbox resource privacy scan failed",
                )
            })?;
            if !scan.is_clean() {
                return Err(RunnerFailure::new(
                    scenario.name.as_str(),
                    seed,
                    None,
                    FailureClassification::PolicyViolation,
                    "sandbox resource contains forbidden private or secret material",
                ));
            }
        }
    }
    Ok(())
}

fn initialize_resource_git(
    config: &RunnerConfig,
    scenario: &ScenarioDefinition,
    seed: u64,
    root: &Path,
) -> Result<(), RunnerFailure> {
    for arguments in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["add", "--all"][..],
        &[
            "-c",
            "user.name=Scenario Resource",
            "-c",
            "user.email=scenario-resource@localhost",
            "commit",
            "--quiet",
            "-m",
            "synthetic scenario resource",
        ][..],
    ] {
        let status = Command::new(&config.git_binary)
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env_clear()
            .env("PATH", &config.search_path)
            .env("HOME", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| resource_failure(scenario, seed))?;
        if !status.success() {
            return Err(resource_failure(scenario, seed));
        }
    }
    if !resource_git_output(config, scenario, seed, root, &["remote"])?.is_empty() {
        return Err(resource_failure(scenario, seed));
    }
    let tracked = resource_git_output(config, scenario, seed, root, &["ls-files", "-z"])?;
    let tracked_count = tracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .count();
    let expected_count = scenario
        .resources
        .iter()
        .find(|resource| resource.synthetic_git && root.ends_with(resource.id.as_str()))
        .map_or(0, |resource| resource.files.len());
    if tracked_count != expected_count {
        return Err(resource_failure(scenario, seed));
    }
    Ok(())
}

fn resource_git_output(
    config: &RunnerConfig,
    scenario: &ScenarioDefinition,
    seed: u64,
    root: &Path,
    arguments: &[&str],
) -> Result<Vec<u8>, RunnerFailure> {
    let output = Command::new(&config.git_binary)
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env_clear()
        .env("PATH", &config.search_path)
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| resource_failure(scenario, seed))?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Err(resource_failure(scenario, seed));
    }
    Ok(output.stdout)
}

fn resource_failure(scenario: &ScenarioDefinition, seed: u64) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        None,
        FailureClassification::ProcessFailure,
        "sandbox resource materialization failed",
    )
}

struct ExecutionState {
    variables: BTreeMap<String, CapturedValue>,
    records: Vec<StepRecord>,
    facts: BTreeMap<String, SafeStepFacts>,
    canaries: BTreeMap<String, BTreeMap<RawContentKind, Vec<PrivacyCanary>>>,
}

#[derive(Clone)]
struct PrivacyCanary {
    bytes: Vec<u8>,
    digest: [u8; 32],
}

#[derive(Default)]
struct SafeStepFacts {
    domain_ids: BTreeSet<String>,
    string_hashes: BTreeSet<[u8; 32]>,
    has_engineering_graph: bool,
    has_working_intent_hint: bool,
    confirmation: Option<SafeConfirmationFacts>,
    expected_failure: Option<(String, String)>,
    observation: Option<ProvenObservation>,
}

struct SafeConfirmationFacts {
    confirmation_id: String,
    event_count: u64,
    envelope_valid: bool,
}

enum ActionExecution {
    Success {
        value: Value,
        observation: Option<ProvenObservation>,
    },
    TypedFailure {
        code: String,
        kind: String,
    },
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn execute_batch(
    scenario: &ScenarioDefinition,
    seed: u64,
    batch: &ScheduledBatch,
    context: &CommandContext,
    observer: &ReadOnlyObserver,
    mcp: &BTreeMap<String, Arc<Mutex<McpManager>>>,
    timeout: Duration,
    root: &Path,
    state: &mut ExecutionState,
) -> Result<(), RunnerFailure> {
    let mut prepared = Vec::new();
    for planned in &batch.steps {
        let action = &scenario.actions[planned.index];
        match planned.disposition {
            PlannedDisposition::Drop => {
                state
                    .records
                    .push(record(action, planned, StepStatus::Dropped));
            }
            PlannedDisposition::Barrier => {
                state
                    .records
                    .push(record(action, planned, StepStatus::Barrier));
            }
            PlannedDisposition::Execute if planned.crash == Some(CrashTiming::Before) => {
                crash_action_process(scenario, seed, action, mcp)?;
                state
                    .records
                    .push(record(action, planned, StepStatus::CrashedBefore));
            }
            PlannedDisposition::Execute => {
                let prepared_action =
                    prepare_action(scenario, seed, action, context, &state.variables)?;
                if let PreparedAction::Hook { canaries, .. } = &prepared_action
                    && !canaries.is_empty()
                {
                    state
                        .canaries
                        .insert(action.id.as_str().to_owned(), canaries.clone());
                }
                prepared.push((*planned, prepared_action));
            }
        }
    }

    let can_parallelize = batch.parallel
        && prepared.len() > 1
        && prepared.iter().all(|(planned, action)| {
            planned.crash.is_none()
                && matches!(
                    action,
                    PreparedAction::Mcp { .. }
                        | PreparedAction::Cli { .. }
                        | PreparedAction::Hook { .. }
                )
        });
    let mut outputs = if can_parallelize {
        thread::scope(|scope| {
            let handles = prepared
                .into_iter()
                .map(|(planned, action)| {
                    scope.spawn(move || {
                        (
                            planned,
                            execute_prepared(action, context, observer, mcp, timeout, root),
                        )
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    let (planned, result) = handle.join().map_err(|_| {
                        RunnerFailure::new(
                            scenario.name.as_str(),
                            seed,
                            None,
                            FailureClassification::ProcessFailure,
                            "concurrent scenario worker failed",
                        )
                    })?;
                    result
                        .map(|output| (planned, output))
                        .map_err(|error| map_process_error(scenario, seed, planned.index, error))
                })
                .collect::<Result<Vec<_>, RunnerFailure>>()
        })?
    } else {
        let mut outputs = Vec::with_capacity(prepared.len());
        for (planned, action) in prepared {
            let output = execute_prepared(action, context, observer, mcp, timeout, root)
                .map_err(|error| map_process_error(scenario, seed, planned.index, error))?;
            outputs.push((planned, output));
        }
        outputs
    };
    outputs.sort_by_key(|(planned, _)| (planned.index, planned.occurrence));
    for (planned, execution) in outputs {
        let action = &scenario.actions[planned.index];
        let (output, base_status, facts) =
            match_action_expectation(scenario, seed, action, execution)?;
        if base_status == StepStatus::Completed {
            capture_step_variables(scenario, seed, action, &output, &mut state.variables)?;
        }
        let status = if planned.crash == Some(CrashTiming::After) {
            crash_action_process(scenario, seed, action, mcp)?;
            StepStatus::CompletedThenCrashed
        } else if matches!(action.action, ActionKind::Restart) {
            StepStatus::Restarted
        } else {
            base_status
        };
        let mut step_record = record(action, &planned, status);
        if let Some((code, kind)) = &facts.expected_failure {
            step_record.error_code = Some(code.clone());
            step_record.error_kind = Some(kind.clone());
        }
        state.facts.insert(action.id.as_str().to_owned(), facts);
        state.records.push(step_record);
    }
    Ok(())
}

fn match_action_expectation(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    execution: ActionExecution,
) -> Result<(Value, StepStatus, SafeStepFacts), RunnerFailure> {
    match (&action.expectation, execution) {
        (ActionExpectation::Success, ActionExecution::Success { value, observation }) => {
            let facts = safe_step_facts(&value, observation);
            Ok((value, StepStatus::Completed, facts))
        }
        (
            ActionExpectation::TypedFailure {
                code: expected_code,
                kind: expected_kind,
            },
            ActionExecution::TypedFailure { code, kind },
        ) if expected_code.as_str() == code && expected_failure_kind(*expected_kind) == kind => {
            let facts = SafeStepFacts {
                expected_failure: Some((code.clone(), kind.clone())),
                ..SafeStepFacts::default()
            };
            Ok((
                json!({"error_code": code, "error_kind": kind}),
                StepStatus::ExpectedFailure,
                facts,
            ))
        }
        (_, ActionExecution::TypedFailure { code, kind }) => {
            Err(RunnerFailure::typed_failure_mismatch(
                scenario.name.as_str(),
                seed,
                action.id.as_str(),
                &code,
                &kind,
            ))
        }
        (ActionExpectation::TypedFailure { .. }, ActionExecution::Success { .. }) => {
            Err(process_failure(
                scenario,
                seed,
                action,
                "action did not match its typed failure expectation",
            ))
        }
    }
}

fn expected_failure_kind(kind: ExpectedFailureKind) -> &'static str {
    match kind {
        ExpectedFailureKind::InvalidInput => "invalid_input",
        ExpectedFailureKind::StaleState => "stale_state",
        ExpectedFailureKind::Conflict => "conflict",
        ExpectedFailureKind::PrivacyRejected => "privacy_rejected",
        ExpectedFailureKind::Unsupported => "unsupported",
        ExpectedFailureKind::RepositoryNotConfigured => "repository_not_configured",
        ExpectedFailureKind::IdempotencyKeyConflict => "idempotency_key_conflict",
    }
}

#[allow(clippy::too_many_lines)]
fn evaluate_assertions(
    scenario: &ScenarioDefinition,
    seed: u64,
    state: &ExecutionState,
    root: &Path,
) -> Result<Vec<AssertionRecord>, RunnerFailure> {
    let mut records = Vec::with_capacity(scenario.assertions.len());
    for assertion in &scenario.assertions {
        let (passed, diagnostic) = match &assertion.invariant {
            InvariantKind::ActiveTaskPerSession {
                observation, task, ..
            } => {
                let task = variable_string(state, task);
                let observed = observation_summary(state, observation.as_str());
                (
                    task.is_some()
                        && observed.is_some_and(|summary| {
                            summary.count == 1 && summary.identity.as_deref() == task
                        }),
                    "active_task_mismatch",
                )
            }
            InvariantKind::CanonicalContinueKeepsRevision {
                first_revision,
                retry_revision,
            } => (
                variable_value(state, first_revision) == variable_value(state, retry_revision),
                "revision_changed_on_semantic_retry",
            ),
            InvariantKind::StaleCasZeroWrites {
                attempt,
                before_observation,
                after_observation,
            } => {
                let expected_failure = state
                    .facts
                    .get(attempt.as_str())
                    .and_then(|facts| facts.expected_failure.as_ref())
                    .is_some_and(|(_, kind)| kind == "stale_state");
                let before = observation_proof(state, before_observation.as_str());
                let after = observation_proof(state, after_observation.as_str());
                (
                    expected_failure
                        && before
                            .zip(after)
                            .is_some_and(|(before, after)| before.summary == after.summary),
                    "stale_attempt_changed_semantic_state",
                )
            }
            InvariantKind::OpenEpisodeHasNoCandidate {
                episode,
                episode_observation,
                candidate_observation,
            } => {
                let episode = variable_string(state, episode);
                let observed_episode = observation_summary(state, episode_observation.as_str());
                let observed_candidates =
                    observation_summary(state, candidate_observation.as_str());
                (
                    episode.is_some()
                        && observed_episode.is_some_and(|summary| {
                            summary.count == 1
                                && summary.identity.as_deref() == episode
                                && summary.status.as_deref() == Some("open")
                        })
                        && observed_candidates.is_some_and(|summary| summary.count == 0),
                    "open_episode_candidate_or_status_mismatch",
                )
            }
            InvariantKind::TurnStopIsIdempotent {
                first_stop,
                repeated_stop,
                first_candidate,
                repeated_candidate,
            } => (
                state.facts.contains_key(first_stop.as_str())
                    && state.facts.contains_key(repeated_stop.as_str())
                    && variable_value(state, first_candidate)
                        == variable_value(state, repeated_candidate),
                "turn_stop_changed_candidate",
            ),
            InvariantKind::CandidateSourceEpisode {
                response,
                candidate,
                episode,
            } => {
                let facts = state.facts.get(response.as_str());
                let candidate = variable_string(state, candidate);
                let episode = variable_string(state, episode);
                (
                    facts.is_some_and(|facts| {
                        candidate.is_some_and(|candidate| facts.domain_ids.contains(candidate))
                            && episode.is_some_and(|episode| facts.domain_ids.contains(episode))
                    }),
                    "candidate_source_episode_mismatch",
                )
            }
            InvariantKind::CandidateIsNotAutoInjected {
                candidate,
                response,
            } => {
                let candidate = variable_string(state, candidate);
                (
                    candidate.is_some()
                        && state.facts.get(response.as_str()).is_some_and(|facts| {
                            candidate.is_some_and(|candidate| !facts.domain_ids.contains(candidate))
                        }),
                    "candidate_was_injected",
                )
            }
            InvariantKind::ConfirmationIsAtomic {
                confirmation,
                response,
                event_count,
            } => {
                let expected = match event_count {
                    ConfirmationEventCount::Four => 4,
                    ConfirmationEventCount::Five => 5,
                };
                let confirmation = variable_string(state, confirmation);
                (
                    confirmation.is_some()
                        && state.facts.get(response.as_str()).is_some_and(|facts| {
                            facts.confirmation.as_ref().is_some_and(|envelope| {
                                envelope.envelope_valid
                                    && envelope.event_count == expected
                                    && confirmation == Some(envelope.confirmation_id.as_str())
                            })
                        }),
                    "confirmation_event_closure_mismatch",
                )
            }
            InvariantKind::SessionEndDoesNotCloseEpisode {
                session_end,
                observation,
                episode,
            } => {
                let episode = variable_string(state, episode);
                (
                    state.facts.contains_key(session_end.as_str())
                        && episode.is_some()
                        && observation_summary(state, observation.as_str()).is_some_and(
                            |summary| {
                                summary.count == 1
                                    && summary.identity.as_deref() == episode
                                    && summary.status.as_deref() == Some("open")
                            },
                        ),
                    "session_end_closed_episode",
                )
            }
            InvariantKind::WorkingIntentHintHasNoGraphPath { response } => (
                state.facts.get(response.as_str()).is_some_and(|facts| {
                    facts.has_working_intent_hint && !facts.has_engineering_graph
                }),
                "working_intent_hint_graph_path_mismatch",
            ),
            InvariantKind::ArtifactFocusIsRequestScoped {
                focused_response,
                ordinary_response,
                path,
                ..
            } => {
                let path_hash: [u8; 32] = Sha256::digest(path.as_str().as_bytes()).into();
                (
                    state
                        .facts
                        .get(focused_response.as_str())
                        .is_some_and(|facts| {
                            facts.has_engineering_graph && facts.string_hashes.contains(&path_hash)
                        })
                        && state
                            .facts
                            .get(ordinary_response.as_str())
                            .is_some_and(|facts| !facts.has_engineering_graph),
                    "artifact_focus_scope_mismatch",
                )
            }
            InvariantKind::RawContentIsAbsent { probes } => {
                let mut clean = true;
                for probe in probes {
                    let Some(canaries) = state
                        .canaries
                        .get(probe.hook.as_str())
                        .and_then(|fields| fields.get(&probe.kind))
                    else {
                        clean = false;
                        continue;
                    };
                    for canary in canaries {
                        if Sha256::digest(&canary.bytes).as_slice() != canary.digest
                            || count_canary_matches(root, &canary.bytes, scenario, seed)? != 0
                        {
                            clean = false;
                        }
                    }
                }
                (clean, "raw_content_persisted")
            }
        };
        let passed_diagnostic = if passed
            && let InvariantKind::StaleCasZeroWrites {
                before_observation,
                after_observation,
                ..
            } = &assertion.invariant
            && observation_proof(state, before_observation.as_str())
                .zip(observation_proof(state, after_observation.as_str()))
                .is_some_and(|(before, after)| before.after != after.before)
        {
            "infrastructure_bytes_changed"
        } else {
            "passed"
        };
        records.push(AssertionRecord {
            id: assertion.id.as_str().to_owned(),
            passed,
            diagnostic_code: if passed {
                passed_diagnostic
            } else {
                diagnostic
            }
            .to_owned(),
        });
    }
    Ok(records)
}

fn variable_value<'a>(state: &'a ExecutionState, name: &VariableName) -> Option<&'a Value> {
    state.variables.get(name.as_str()).map(|value| &value.value)
}

fn variable_string<'a>(state: &'a ExecutionState, name: &VariableName) -> Option<&'a str> {
    variable_value(state, name).and_then(Value::as_str)
}

fn observation_proof<'a>(state: &'a ExecutionState, step: &str) -> Option<&'a ProvenObservation> {
    state
        .facts
        .get(step)
        .and_then(|facts| facts.observation.as_ref())
}

fn observation_summary<'a>(
    state: &'a ExecutionState,
    step: &str,
) -> Option<&'a ObservationSummary> {
    observation_proof(state, step).map(|proof| &proof.summary)
}

fn count_canary_matches(
    root: &Path,
    needle: &[u8],
    scenario: &ScenarioDefinition,
    seed: u64,
) -> Result<u64, RunnerFailure> {
    const MAX_FILES: usize = 20_000;
    const MAX_BYTES: u64 = 512 * 1024 * 1024;
    if !root.exists() {
        return Ok(0);
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    let mut matches = 0_u64;
    while let Some(path) = pending.pop() {
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| privacy_scan_failure(scenario, seed))?;
        if metadata.file_type().is_symlink() {
            return Err(privacy_scan_failure(scenario, seed));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|_| privacy_scan_failure(scenario, seed))? {
                pending.push(
                    entry
                        .map_err(|_| privacy_scan_failure(scenario, seed))?
                        .path(),
                );
            }
        } else if metadata.is_file() {
            files = files.saturating_add(1);
            bytes = bytes.saturating_add(metadata.len());
            if files > MAX_FILES || bytes > MAX_BYTES {
                return Err(privacy_scan_failure(scenario, seed));
            }
            matches = matches.saturating_add(count_in_file(&path, needle, scenario, seed)?);
        } else {
            return Err(privacy_scan_failure(scenario, seed));
        }
    }
    Ok(matches)
}

fn count_in_file(
    path: &Path,
    needle: &[u8],
    scenario: &ScenarioDefinition,
    seed: u64,
) -> Result<u64, RunnerFailure> {
    let mut file = fs::File::open(path).map_err(|_| privacy_scan_failure(scenario, seed))?;
    let mut buffer = [0_u8; 16 * 1024];
    let mut tail = Vec::new();
    let mut matches = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| privacy_scan_failure(scenario, seed))?;
        if read == 0 {
            break;
        }
        let mut chunk = Vec::with_capacity(tail.len() + read);
        chunk.extend_from_slice(&tail);
        chunk.extend_from_slice(&buffer[..read]);
        matches = matches.saturating_add(
            chunk
                .windows(needle.len())
                .filter(|window| *window == needle)
                .count() as u64,
        );
        let keep = needle.len().saturating_sub(1).min(chunk.len());
        tail.clear();
        tail.extend_from_slice(&chunk[chunk.len() - keep..]);
    }
    Ok(matches)
}

fn privacy_scan_failure(scenario: &ScenarioDefinition, seed: u64) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        None,
        FailureClassification::ObserverFailure,
        "persistent privacy scan failed",
    )
}

fn safe_step_facts(value: &Value, observation: Option<ProvenObservation>) -> SafeStepFacts {
    let mut facts = SafeStepFacts {
        observation,
        ..SafeStepFacts::default()
    };
    collect_domain_ids(value, &mut facts.domain_ids);
    collect_string_hashes(value, &mut facts.string_hashes);
    facts.has_engineering_graph = contains_engineering_graph(value);
    facts.has_working_intent_hint = contains_exact_string(value, "working_intent_hint_text");
    facts.confirmation = confirmation_facts(value);
    facts
}

fn confirmation_facts(value: &Value) -> Option<SafeConfirmationFacts> {
    let object = value.as_object()?;
    let confirmation_id = object.get("confirmation_id")?.as_str()?;
    let event_ids = object.get("event_ids")?.as_array()?;
    let batch_id = object.get("batch_id")?.as_str()?;
    let commit_oid = object.get("commit_oid")?.as_str()?;
    let event_count = u64::try_from(event_ids.len()).ok()?;
    let mut unique_events = BTreeSet::new();
    let events_valid = matches!(event_count, 4 | 5)
        && event_ids.iter().all(|event| {
            event
                .as_str()
                .is_some_and(|event| is_prefixed_uuid(event, "evt_") && unique_events.insert(event))
        });
    let lifecycle_valid = match (object.get("status"), object.get("created")) {
        (None, None) => true,
        (Some(Value::String(status)), Some(Value::Bool(created))) => matches!(
            (status.as_str(), *created),
            ("confirmed", true) | ("already_confirmed", false)
        ),
        _ => false,
    };
    Some(SafeConfirmationFacts {
        confirmation_id: confirmation_id.to_owned(),
        event_count,
        envelope_valid: is_prefixed_uuid(confirmation_id, "cfm_")
            && events_valid
            && is_prefixed_uuid(batch_id, "bat_")
            && is_commit_oid(commit_oid)
            && lifecycle_valid,
    })
}

fn is_commit_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn collect_string_hashes(value: &Value, hashes: &mut BTreeSet<[u8; 32]>) {
    match value {
        Value::String(value) => {
            hashes.insert(Sha256::digest(value.as_bytes()).into());
        }
        Value::Array(values) => {
            for value in values {
                collect_string_hashes(value, hashes);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_string_hashes(value, hashes);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn contains_exact_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values
            .iter()
            .any(|value| contains_exact_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| contains_exact_string(value, expected)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn collect_domain_ids(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::String(value) if is_domain_id(value) => {
            ids.insert(value.clone());
        }
        Value::Array(values) => {
            for value in values {
                collect_domain_ids(value, ids);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_domain_ids(value, ids);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn contains_engineering_graph(value: &Value) -> bool {
    match value {
        Value::String(value) => value == "engineering_graph",
        Value::Array(values) => values.iter().any(contains_engineering_graph),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key == "engineering_graph" || contains_engineering_graph(value)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn crash_action_process(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    managers: &BTreeMap<String, Arc<Mutex<McpManager>>>,
) -> Result<(), RunnerFailure> {
    if matches!(action.action, ActionKind::McpRequest { .. }) {
        mcp_for_action(scenario, seed, action, managers)?
            .lock()
            .map_err(|_| process_failure(scenario, seed, action, "MCP state lock failed"))?
            .crash();
    }
    Ok(())
}

fn mcp_for_action<'a>(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    managers: &'a BTreeMap<String, Arc<Mutex<McpManager>>>,
) -> Result<&'a Arc<Mutex<McpManager>>, RunnerFailure> {
    let session = session_actor(scenario, seed, action)?;
    managers.get(session).ok_or_else(|| {
        RunnerFailure::new(
            scenario.name.as_str(),
            seed,
            Some(action.id.as_str()),
            FailureClassification::InvalidScenario,
            "action Session transport is unavailable",
        )
    })
}

fn session_actor<'a>(
    scenario: &'a ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
) -> Result<&'a str, RunnerFailure> {
    let actor = scenario
        .actors
        .iter()
        .find(|actor| actor.id == action.actor)
        .ok_or_else(|| {
            RunnerFailure::new(
                scenario.name.as_str(),
                seed,
                Some(action.id.as_str()),
                FailureClassification::InvalidScenario,
                "action actor is unavailable",
            )
        })?;
    Ok(match &actor.kind {
        ActorKind::Session => actor.id.as_str(),
        ActorKind::Task { session } => session.as_str(),
    })
}

fn record(action: &ScenarioAction, planned: &PlannedStep, status: StepStatus) -> StepRecord {
    StepRecord {
        step: action.id.as_str().to_owned(),
        occurrence: planned.occurrence,
        status,
        error_code: None,
        error_kind: None,
    }
}

enum PreparedAction {
    Mcp {
        session: String,
        method: String,
        params: Value,
    },
    Cli {
        arguments: Vec<String>,
    },
    Hook {
        arguments: Vec<String>,
        payload: Vec<u8>,
        canaries: BTreeMap<RawContentKind, Vec<PrivacyCanary>>,
    },
    Observe {
        source: sctx_scenario_contract::ObservationSource,
        selector: Value,
    },
    Restart {
        session: String,
    },
}

fn prepare_action(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    context: &CommandContext,
    variables: &BTreeMap<String, CapturedValue>,
) -> Result<PreparedAction, RunnerFailure> {
    match &action.action {
        ActionKind::McpRequest { method, params } => Ok(PreparedAction::Mcp {
            session: session_actor(scenario, seed, action)?.to_owned(),
            method: method.as_str().to_owned(),
            params: render_template(scenario, seed, action, params, variables, context)?,
        }),
        ActionKind::CliJson { arguments } => Ok(PreparedAction::Cli {
            arguments: arguments
                .iter()
                .map(|argument| {
                    render_cli_argument(scenario, seed, action, argument, variables, context)
                })
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ActionKind::HookEvent { event, payload } => {
            let payload = render_template(scenario, seed, action, payload, variables, context)?;
            let product_action = scenario
                .events
                .iter()
                .find(|classification| classification.event == *event)
                .and_then(|classification| classification.product_action)
                .ok_or_else(|| {
                    invalid_step(
                        scenario,
                        seed,
                        action,
                        "Hook event classification is missing",
                    )
                })?;
            let mut payload = hook_payload(
                &scenario.agent,
                event.as_str(),
                product_action,
                payload,
                context,
                action.actor.as_str(),
                action.id.as_str(),
            )
            .ok_or_else(|| {
                invalid_step(
                    scenario,
                    seed,
                    action,
                    "Hook template must render to an object",
                )
            })?;
            let canaries = inject_privacy_canaries(
                scenario,
                seed,
                action,
                product_action,
                &scenario.agent,
                context,
                &mut payload,
            )?;
            let agent = match scenario.agent.vendor {
                AgentVendor::Cursor => "cursor",
                AgentVendor::Codex => "codex",
            };
            let mut arguments = vec!["hook".to_owned(), "--agent".to_owned(), agent.to_owned()];
            if scenario.agent.vendor == AgentVendor::Codex {
                arguments.push("--agent-version".to_owned());
                arguments.push(scenario.agent.version.to_string());
            }
            Ok(PreparedAction::Hook {
                arguments,
                payload: serde_json::to_vec(&payload).map_err(|_| {
                    invalid_step(scenario, seed, action, "Hook payload serialization failed")
                })?,
                canaries,
            })
        }
        ActionKind::Observe { source, selector } => Ok(PreparedAction::Observe {
            source: *source,
            selector: render_template(scenario, seed, action, selector, variables, context)?,
        }),
        ActionKind::Restart => Ok(PreparedAction::Restart {
            session: session_actor(scenario, seed, action)?.to_owned(),
        }),
        ActionKind::Barrier { .. } => Err(invalid_step(
            scenario,
            seed,
            action,
            "barrier reached executable preparation",
        )),
    }
}

fn execute_prepared(
    action: PreparedAction,
    context: &CommandContext,
    observer: &ReadOnlyObserver,
    mcp: &BTreeMap<String, Arc<Mutex<McpManager>>>,
    timeout: Duration,
    root: &Path,
) -> Result<ActionExecution, ProcessError> {
    match action {
        PreparedAction::Mcp {
            session,
            method,
            params,
        } => {
            let outcome = mcp[&session]
                .lock()
                .map_err(|_| ProcessError {
                    kind: ProcessErrorKind::Crashed,
                    message: "MCP state lock failed",
                })?
                .call(&method, &params)?;
            Ok(match outcome {
                McpToolOutcome::Success(value) => ActionExecution::Success {
                    value,
                    observation: None,
                },
                McpToolOutcome::TypedFailure { code, kind } => {
                    ActionExecution::TypedFailure { code, kind }
                }
            })
        }
        PreparedAction::Cli { arguments } => {
            let mut command = vec!["--json".to_owned()];
            command.extend(arguments);
            let output = run_one_shot(context, &command, None, timeout)?;
            let value = parse_json_output(&output)?;
            Ok(ActionExecution::Success {
                value: value.get("data").cloned().unwrap_or(value),
                observation: None,
            })
        }
        PreparedAction::Hook {
            arguments, payload, ..
        } => {
            let output = run_one_shot(context, &arguments, Some(&payload), timeout)?;
            parse_json_output(&output).map(|value| ActionExecution::Success {
                value,
                observation: None,
            })
        }
        PreparedAction::Observe { source, selector } => {
            let proof = observer
                .observe(root, source, &selector)
                .map_err(|_| ProcessError {
                    kind: ProcessErrorKind::Observer,
                    message: "read-only observer failed",
                })?;
            let value = serde_json::to_value(&proof.summary).map_err(|_| ProcessError {
                kind: ProcessErrorKind::Observer,
                message: "observer summary serialization failed",
            })?;
            Ok(ActionExecution::Success {
                value,
                observation: Some(proof),
            })
        }
        PreparedAction::Restart { session } => {
            mcp[&session]
                .lock()
                .map_err(|_| ProcessError {
                    kind: ProcessErrorKind::Crashed,
                    message: "MCP state lock failed",
                })?
                .restart()?;
            Ok(ActionExecution::Success {
                value: Value::Null,
                observation: None,
            })
        }
    }
}

fn render_template(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    template: &TemplateValue,
    variables: &BTreeMap<String, CapturedValue>,
    context: &CommandContext,
) -> Result<Value, RunnerFailure> {
    match template {
        TemplateValue::Null => Ok(Value::Null),
        TemplateValue::Boolean { value } => Ok(Value::Bool(*value)),
        TemplateValue::Integer { value } => Ok((*value).into()),
        TemplateValue::Unsigned { value } => Ok((*value).into()),
        TemplateValue::String { value } => Ok(Value::String(value.clone())),
        TemplateValue::Array { items } => items
            .iter()
            .map(|item| render_template(scenario, seed, action, item, variables, context))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        TemplateValue::Object { fields } => fields
            .iter()
            .map(|(key, value)| {
                render_template(scenario, seed, action, value, variables, context)
                    .map(|value| (key.clone(), value))
            })
            .collect::<Result<Map<_, _>, _>>()
            .map(Value::Object),
        TemplateValue::Variable { name } => variables
            .get(name.as_str())
            .map(|captured| captured.value.clone())
            .ok_or_else(|| {
                invalid_step(scenario, seed, action, "runtime variable is not available")
            }),
        TemplateValue::Builtin { builtin } => {
            render_builtin(scenario, seed, action, builtin, context).map(Value::String)
        }
    }
}

fn render_builtin(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    builtin: &SandboxBuiltin,
    context: &CommandContext,
) -> Result<String, RunnerFailure> {
    let path = match builtin {
        SandboxBuiltin::Home => Some(&context.home),
        SandboxBuiltin::Root => Some(&context.root),
        SandboxBuiltin::Workspace => Some(&context.workspace),
        SandboxBuiltin::Temp => Some(&context.temporary),
        SandboxBuiltin::ResourceRoot { resource } => context.resource_roots.get(resource.as_str()),
        SandboxBuiltin::ResourceFile { resource, path } => context
            .resource_files
            .get(&(resource.as_str().to_owned(), path.as_str().to_owned())),
        SandboxBuiltin::ActorSessionKey { actor } => {
            return Ok(actor_session_key(actor.as_str()));
        }
    };
    path.and_then(|path| path.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            invalid_step(
                scenario,
                seed,
                action,
                "sandbox builtin could not be rendered safely",
            )
        })
}

fn actor_session_key(actor: &str) -> String {
    format!("scenario-{actor}")
}

fn render_cli_argument(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    template: &TemplateValue,
    variables: &BTreeMap<String, CapturedValue>,
    context: &CommandContext,
) -> Result<String, RunnerFailure> {
    match render_template(scenario, seed, action, template, variables, context)? {
        Value::String(value) => Ok(value),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => Err(invalid_step(
            scenario,
            seed,
            action,
            "CLI arguments must render to scalar values",
        )),
    }
}

fn capture_step_variables(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    output: &Value,
    variables: &mut BTreeMap<String, CapturedValue>,
) -> Result<(), RunnerFailure> {
    for variable in scenario
        .variables
        .iter()
        .filter(|variable| variable.capture.step == action.id)
    {
        let value = output
            .pointer(variable.capture.pointer.as_str())
            .ok_or_else(|| capture_failure(scenario, seed, action, "capture pointer is absent"))?
            .clone();
        validate_captured_value(variable.value_type, &value).map_err(|()| {
            capture_failure(scenario, seed, action, "captured value has the wrong type")
        })?;
        let captured = CapturedValue {
            value_type: variable.value_type,
            value,
        };
        if let Some(existing) = variables.get(variable.name.as_str())
            && existing != &captured
        {
            return Err(capture_failure(
                scenario,
                seed,
                action,
                "repeated capture changed its typed value",
            ));
        }
        variables.insert(variable.name.as_str().to_owned(), captured);
    }
    Ok(())
}

fn validate_captured_value(kind: VariableKind, value: &Value) -> Result<(), ()> {
    let prefix = match kind {
        VariableKind::SpaceId => Some("spc_"),
        VariableKind::RepositoryId => {
            return value
                .as_str()
                .ok_or(())?
                .parse::<sctx_domain::RepositoryId>()
                .map(|_| ())
                .map_err(|_| ());
        }
        VariableKind::ReferenceId => Some("ref_"),
        VariableKind::TaskId => Some("tsk_"),
        VariableKind::TaskSessionId => Some("tss_"),
        VariableKind::ExternalSessionId => Some("xss_"),
        VariableKind::IntentRevisionId => Some("tir_"),
        VariableKind::SignalId => Some("sig_"),
        VariableKind::CaptureId => Some("cap_"),
        VariableKind::WorkEpisodeId => Some("wep_"),
        VariableKind::WorkObservationId => Some("wob_"),
        VariableKind::CheckpointId => Some("ckp_"),
        VariableKind::ClaimId => Some("clm_"),
        VariableKind::CandidateBuildId => Some("bld_"),
        VariableKind::SpaceRecommendationId => Some("rec_"),
        VariableKind::CandidateId => Some("cnd_"),
        VariableKind::SubmissionId => Some("sub_"),
        VariableKind::ConfirmationId => Some("cfm_"),
        VariableKind::SpaceAssociationId => Some("asc_"),
        VariableKind::ContextId => Some("ctx_"),
        VariableKind::RevisionId => Some("rev_"),
        VariableKind::EventId => Some("evt_"),
        VariableKind::PublicationId => Some("pub_"),
        VariableKind::EvidenceId => Some("evd_"),
        VariableKind::ReviewId => Some("rvw_"),
        VariableKind::ConflictId => Some("cnf_"),
        VariableKind::ResolutionId => Some("rsl_"),
        VariableKind::Generation
        | VariableKind::Status
        | VariableKind::Count
        | VariableKind::Boolean
        | VariableKind::Text => None,
    };
    if let Some(prefix) = prefix {
        return validate_domain_id(value, prefix);
    }
    match kind {
        VariableKind::Generation => match value {
            Value::Number(number) if number.as_u64().is_some() => Ok(()),
            Value::String(value) if is_safe_generation(value) => Ok(()),
            _ => Err(()),
        },
        VariableKind::Status => match value {
            Value::String(value)
                if !value.is_empty()
                    && value.len() <= 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)) =>
            {
                Ok(())
            }
            _ => Err(()),
        },
        VariableKind::Count if value.as_u64().is_some() => Ok(()),
        VariableKind::Boolean if value.is_boolean() => Ok(()),
        VariableKind::Text => match value {
            Value::String(value) if value.len() <= MAX_CAPTURED_TEXT_BYTES => Ok(()),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

fn is_safe_generation(value: &str) -> bool {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return value.bytes().all(|byte| !byte.is_ascii_uppercase());
    }
    if let Some(digest) = value.strip_prefix("sha256:") {
        return digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    }
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b":_-".contains(&byte))
}

fn validate_domain_id(value: &Value, prefix: &str) -> Result<(), ()> {
    let value = value.as_str().ok_or(())?;
    let raw = value.strip_prefix(prefix).ok_or(())?;
    let uuid = uuid::Uuid::parse_str(raw).map_err(|_| ())?;
    if uuid.get_version() == Some(Version::Random)
        && uuid.get_variant() == Variant::RFC4122
        && uuid.hyphenated().to_string() == raw
    {
        Ok(())
    } else {
        Err(())
    }
}

fn is_domain_id(value: &str) -> bool {
    const PREFIXES: [&str; 28] = [
        "spc_", "rpo_", "rpg_", "ref_", "tsk_", "tss_", "xss_", "tir_", "sig_", "cap_", "wep_",
        "wob_", "ckp_", "clm_", "bld_", "rec_", "cnd_", "sub_", "cfm_", "asc_", "ctx_", "rev_",
        "evt_", "pub_", "evd_", "rvw_", "cnf_", "rsl_",
    ];
    PREFIXES
        .iter()
        .any(|prefix| is_prefixed_uuid(value, prefix))
}

fn is_prefixed_uuid(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok().map(|uuid| (raw, uuid)))
        .is_some_and(|(raw, uuid)| {
            uuid.get_version() == Some(Version::Random)
                && uuid.get_variant() == Variant::RFC4122
                && uuid.hyphenated().to_string() == raw
        })
}

fn preflight_action_shapes(scenario: &ScenarioDefinition, seed: u64) -> Result<(), RunnerFailure> {
    for variable in &scenario.variables {
        let source = scenario
            .actions
            .iter()
            .find(|action| action.id == variable.capture.step)
            .ok_or_else(|| {
                RunnerFailure::new(
                    scenario.name.as_str(),
                    seed,
                    Some(variable.capture.step.as_str()),
                    FailureClassification::InvalidScenario,
                    "capture source action is unavailable",
                )
            })?;
        if variable.value_type == VariableKind::Text
            && !matches!(source.action, ActionKind::Observe { .. })
        {
            return Err(RunnerFailure::new(
                scenario.name.as_str(),
                seed,
                Some(variable.capture.step.as_str()),
                FailureClassification::PolicyViolation,
                "Text captures are restricted to read-only Observer summaries",
            ));
        }
        let pointer = variable.capture.pointer.as_str().to_ascii_lowercase();
        if [
            "prompt",
            "transcript",
            "tool_output",
            "tool_response",
            "last_assistant_message",
        ]
        .iter()
        .any(|field| pointer.split('/').any(|token| token == *field))
        {
            return Err(RunnerFailure::new(
                scenario.name.as_str(),
                seed,
                Some(variable.capture.step.as_str()),
                FailureClassification::PolicyViolation,
                "raw Agent content cannot be captured by the runner",
            ));
        }
    }
    for action in &scenario.actions {
        if matches!(action.expectation, ActionExpectation::TypedFailure { .. })
            && !matches!(action.action, ActionKind::McpRequest { .. })
        {
            return Err(invalid_step(
                scenario,
                seed,
                action,
                "this runner supports typed expected failures only for MCP",
            ));
        }
        match &action.action {
            ActionKind::McpRequest { params, .. }
            | ActionKind::HookEvent {
                payload: params, ..
            } if !matches!(params, TemplateValue::Object { .. }) => {
                return Err(invalid_step(
                    scenario,
                    seed,
                    action,
                    "MCP and Hook templates must be objects",
                ));
            }
            ActionKind::CliJson { arguments } => {
                for argument in arguments {
                    if !template_is_scalar(argument) {
                        return Err(invalid_step(
                            scenario,
                            seed,
                            action,
                            "CLI arguments must be scalar templates",
                        ));
                    }
                }
            }
            ActionKind::Observe { selector, .. }
                if !matches!(selector, TemplateValue::Object { .. }) =>
            {
                return Err(invalid_step(
                    scenario,
                    seed,
                    action,
                    "observer selector must be an object template",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn preflight_parallel_mcp(
    scenario: &ScenarioDefinition,
    seed: u64,
    schedule: &[ScheduledBatch],
) -> Result<(), RunnerFailure> {
    for batch in schedule.iter().filter(|batch| batch.parallel) {
        let mut sessions = BTreeSet::new();
        for planned in batch
            .steps
            .iter()
            .filter(|planned| planned.disposition == PlannedDisposition::Execute)
        {
            let action = &scenario.actions[planned.index];
            if !matches!(action.action, ActionKind::McpRequest { .. }) {
                continue;
            }
            let session = session_actor(scenario, seed, action)?;
            if !sessions.insert(session) {
                return Err(RunnerFailure::new(
                    scenario.name.as_str(),
                    seed,
                    Some(action.id.as_str()),
                    FailureClassification::InvalidScenario,
                    "parallel MCP actions require independent Session actors",
                ));
            }
        }
    }
    Ok(())
}

fn preflight_assertion_schedule(
    scenario: &ScenarioDefinition,
    seed: u64,
    schedule: &[ScheduledBatch],
) -> Result<(), RunnerFailure> {
    let require_once = |step: &sctx_scenario_contract::StepId| {
        let index = scenario
            .actions
            .iter()
            .position(|action| action.id == *step)
            .ok_or_else(|| RunnerFailure::invalid(seed, "assertion step is unavailable"))?;
        let occurrences = schedule
            .iter()
            .flat_map(|batch| &batch.steps)
            .filter(|planned| planned.index == index)
            .collect::<Vec<_>>();
        if occurrences.len() == 1
            && occurrences[0].disposition == PlannedDisposition::Execute
            && occurrences[0].crash.is_none()
        {
            Ok(())
        } else {
            Err(RunnerFailure::new(
                scenario.name.as_str(),
                seed,
                Some(step.as_str()),
                FailureClassification::InvalidScenario,
                "assertion evidence must execute exactly once without a delivery fault",
            ))
        }
    };
    for variable in &scenario.variables {
        require_once(&variable.capture.step)?;
    }
    for assertion in &scenario.assertions {
        match &assertion.invariant {
            InvariantKind::ActiveTaskPerSession { observation, .. } => {
                require_once(observation)?;
            }
            InvariantKind::CanonicalContinueKeepsRevision { .. } => {}
            InvariantKind::StaleCasZeroWrites {
                attempt,
                before_observation,
                after_observation,
            } => {
                require_once(before_observation)?;
                require_once(attempt)?;
                require_once(after_observation)?;
            }
            InvariantKind::OpenEpisodeHasNoCandidate {
                episode_observation,
                candidate_observation,
                ..
            } => {
                require_once(episode_observation)?;
                require_once(candidate_observation)?;
            }
            InvariantKind::TurnStopIsIdempotent {
                first_stop,
                repeated_stop,
                ..
            } => {
                require_once(first_stop)?;
                require_once(repeated_stop)?;
            }
            InvariantKind::CandidateSourceEpisode { response, .. }
            | InvariantKind::CandidateIsNotAutoInjected { response, .. }
            | InvariantKind::ConfirmationIsAtomic { response, .. }
            | InvariantKind::WorkingIntentHintHasNoGraphPath { response } => {
                require_once(response)?;
            }
            InvariantKind::SessionEndDoesNotCloseEpisode {
                session_end,
                observation,
                ..
            } => {
                require_once(session_end)?;
                require_once(observation)?;
            }
            InvariantKind::ArtifactFocusIsRequestScoped {
                focused_response,
                ordinary_response,
                ..
            } => {
                require_once(focused_response)?;
                require_once(ordinary_response)?;
            }
            InvariantKind::RawContentIsAbsent { probes } => {
                for probe in probes {
                    require_once(&probe.hook)?;
                }
            }
        }
    }
    Ok(())
}

fn template_is_scalar(template: &TemplateValue) -> bool {
    matches!(
        template,
        TemplateValue::Boolean { .. }
            | TemplateValue::Integer { .. }
            | TemplateValue::Unsigned { .. }
            | TemplateValue::String { .. }
            | TemplateValue::Variable { .. }
            | TemplateValue::Builtin { .. }
    )
}

fn hook_payload(
    profile: &AgentProfile,
    event: &str,
    product_action: ProductAction,
    payload: Value,
    context: &CommandContext,
    session_actor: &str,
    step: &str,
) -> Option<Value> {
    let Value::Object(mut payload) = payload else {
        return None;
    };
    let workspace = context.workspace.to_string_lossy().into_owned();
    let session = actor_session_key(session_actor);
    match profile.vendor {
        AgentVendor::Codex => {
            insert_defaults(
                &mut payload,
                match product_action {
                    ProductAction::SessionStart => json!({"source": "startup"}),
                    ProductAction::PromptSubmit => {
                        json!({"turn_id": step, "prompt": "synthetic scenario input"})
                    }
                    ProductAction::PostToolUse => json!({
                        "turn_id": step, "tool_name": "scenario_tool", "tool_use_id": step,
                        "tool_input": {}, "tool_response": {"success": true}
                    }),
                    ProductAction::PreCompact => json!({"turn_id": step, "trigger": "manual"}),
                    ProductAction::TurnStop => json!({
                        "turn_id": step, "stop_hook_active": false,
                        "last_assistant_message": null
                    }),
                    ProductAction::SessionEnd => json!({"reason": "other"}),
                },
            );
            payload.insert("session_id".to_owned(), Value::String(session));
            payload
                .entry("cwd".to_owned())
                .or_insert_with(|| Value::String(workspace));
            payload.insert(
                "hook_event_name".to_owned(),
                Value::String(event.to_owned()),
            );
            payload.insert(
                "model".to_owned(),
                Value::String("scenario-model-disabled".to_owned()),
            );
            payload.insert(
                "permission_mode".to_owned(),
                Value::String("default".to_owned()),
            );
        }
        AgentVendor::Cursor => {
            insert_defaults(
                &mut payload,
                match product_action {
                    ProductAction::SessionStart => json!({
                        "session_id": session, "is_background_agent": false,
                        "composer_mode": "agent"
                    }),
                    ProductAction::PromptSubmit => {
                        json!({"prompt": "synthetic scenario input", "attachments": []})
                    }
                    ProductAction::PostToolUse => json!({
                        "tool_name": "scenario_tool", "tool_input": {}, "tool_output": "redacted",
                        "tool_use_id": step, "cwd": workspace, "duration": 1
                    }),
                    ProductAction::PreCompact => json!({
                        "trigger": "manual", "context_usage_percent": 50.0,
                        "context_tokens": 10, "context_window_size": 20,
                        "message_count": 1, "messages_to_compact": 1,
                        "is_first_compaction": true
                    }),
                    ProductAction::TurnStop => json!({"status": "completed", "loop_count": 1}),
                    ProductAction::SessionEnd => json!({
                        "session_id": session, "reason": "completed", "duration_ms": 1,
                        "is_background_agent": false, "final_status": "completed"
                    }),
                },
            );
            payload.insert("conversation_id".to_owned(), Value::String(session.clone()));
            payload.insert(
                "generation_id".to_owned(),
                Value::String(format!("generation-{step}")),
            );
            payload.insert(
                "model".to_owned(),
                Value::String("scenario-model-disabled".to_owned()),
            );
            payload.insert(
                "hook_event_name".to_owned(),
                Value::String(event.to_owned()),
            );
            payload.insert(
                "cursor_version".to_owned(),
                Value::String(profile.version.to_string()),
            );
            payload
                .entry("workspace_roots".to_owned())
                .or_insert_with(|| Value::Array(vec![Value::String(workspace)]));
        }
    }
    Some(Value::Object(payload))
}

#[allow(clippy::too_many_arguments)]
fn inject_privacy_canaries(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    _product_action: ProductAction,
    profile: &AgentProfile,
    context: &CommandContext,
    payload: &mut Value,
) -> Result<BTreeMap<RawContentKind, Vec<PrivacyCanary>>, RunnerFailure> {
    let Value::Object(payload) = payload else {
        return Err(invalid_step(
            scenario,
            seed,
            action,
            "Hook payload cannot receive a privacy canary",
        ));
    };
    let mut canaries = BTreeMap::<RawContentKind, Vec<PrivacyCanary>>::new();
    for probe in scenario.assertions.iter().flat_map(|assertion| {
        if let InvariantKind::RawContentIsAbsent { probes } = &assertion.invariant {
            probes.as_slice()
        } else {
            &[]
        }
    }) {
        if probe.hook != action.id || canaries.contains_key(&probe.kind) {
            continue;
        }
        let digest = Sha256::digest(format!(
            "{}|{seed}|{}|{:?}",
            scenario.name.as_str(),
            action.id.as_str(),
            probe.kind
        ));
        let text = format!("sctx-canary-{digest:x}");
        let canary = PrivacyCanary {
            bytes: text.as_bytes().to_vec(),
            digest: Sha256::digest(text.as_bytes()).into(),
        };
        match probe.kind {
            RawContentKind::Prompt => {
                payload.insert("prompt".to_owned(), Value::String(text));
            }
            RawContentKind::Transcript => {
                let path = context.temporary.join(format!("{text}.jsonl"));
                payload.insert(
                    "transcript_path".to_owned(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
            }
            RawContentKind::ToolOutput => match profile.vendor {
                AgentVendor::Cursor => {
                    payload.insert("tool_output".to_owned(), Value::String(text));
                }
                AgentVendor::Codex => {
                    payload.insert("tool_response".to_owned(), json!({"scenario_canary": text}));
                }
            },
        }
        canaries.entry(probe.kind).or_default().push(canary);
    }
    Ok(canaries)
}

fn insert_defaults(target: &mut Map<String, Value>, defaults: Value) {
    if let Value::Object(defaults) = defaults {
        for (key, value) in defaults {
            target.entry(key).or_insert(value);
        }
    }
}

fn policy_failure(
    scenario: &ScenarioDefinition,
    seed: u64,
    message: &'static str,
) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        None,
        FailureClassification::PolicyViolation,
        message,
    )
}

fn invalid_step(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    message: &'static str,
) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        Some(action.id.as_str()),
        FailureClassification::InvalidScenario,
        message,
    )
}

fn capture_failure(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    message: &'static str,
) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        Some(action.id.as_str()),
        FailureClassification::CaptureFailure,
        message,
    )
}

fn process_failure(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    message: &'static str,
) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        Some(action.id.as_str()),
        FailureClassification::ProcessFailure,
        message,
    )
}

fn map_process_error(
    scenario: &ScenarioDefinition,
    seed: u64,
    step: usize,
    error: ProcessError,
) -> RunnerFailure {
    RunnerFailure::new(
        scenario.name.as_str(),
        seed,
        Some(scenario.actions[step].id.as_str()),
        match error.kind {
            ProcessErrorKind::Timeout => FailureClassification::Timeout,
            ProcessErrorKind::Observer => FailureClassification::ObserverFailure,
            ProcessErrorKind::Start
            | ProcessErrorKind::Write
            | ProcessErrorKind::Exit
            | ProcessErrorKind::Output
            | ProcessErrorKind::Protocol
            | ProcessErrorKind::Crashed => FailureClassification::ProcessFailure,
        },
        error.message,
    )
}

#[cfg(test)]
mod tests {
    use sctx_scenario_contract::VariableKind;
    use serde_json::json;

    use super::{is_domain_id, validate_captured_value};

    #[test]
    fn captured_repository_identity_uses_the_readable_domain_contract() {
        for value in ["Android", "iOS", "FE", "rpo_legacy"] {
            assert!(
                validate_captured_value(VariableKind::RepositoryId, &json!(value)).is_ok(),
                "{value}"
            );
        }
        for value in ["", "FE/mobile", "安卓"] {
            assert!(
                validate_captured_value(VariableKind::RepositoryId, &json!(value)).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn repository_group_identity_is_recognized_without_flagging_business_text() {
        assert!(is_domain_id("rpg_123e4567-e89b-42d3-a456-426614174000"));
        for value in [
            "rpg_feature",
            "rpg_not-a-uuid",
            "rpg_123e4567-e89b-12d3-a456-426614174000",
        ] {
            assert!(!is_domain_id(value), "{value}");
        }
    }
}
