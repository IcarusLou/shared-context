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
mod schedule;

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
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
    CommandContext, McpManager, ProcessError, ProcessErrorKind, binary_basename, parse_json_output,
    run_one_shot,
};
use schedule::{PlannedDisposition, PlannedStep, ScheduledBatch, compile_schedule};
use sctx_scenario_contract::{
    ActionKind, ActorKind, AgentProfile, AgentVendor, CrashTiming, ProductAction, ScenarioAction,
    ScenarioDefinition, TemplateValue, VariableKind, parse_scenario,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
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
}

/// Deterministic, raw-output-free run result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOutcome {
    pub scenario: String,
    pub seed: u64,
    pub steps: Vec<StepRecord>,
    pub variables: BTreeMap<String, CapturedValue>,
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
        let schedule = compile_schedule(scenario, seed)?;
        preflight_action_shapes(scenario, seed)?;
        preflight_parallel_mcp(scenario, seed, &schedule)?;

        let sandbox = Sandbox::create(&self.config, scenario.name.as_str(), seed)?;
        let context = CommandContext {
            binary: self.config.sctx_binary.clone(),
            home: sandbox.home.clone(),
            temporary: sandbox.temporary_path.clone(),
            workspace: sandbox.workspace.clone(),
            search_path: self.config.search_path.clone(),
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
}

impl Sandbox {
    fn create(config: &RunnerConfig, scenario: &str, seed: u64) -> Result<Self, RunnerFailure> {
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
        Ok(Self {
            temporary,
            temporary_path: temporary_files,
            root: home.join(".shared-context"),
            home,
            workspace,
        })
    }
}

struct ExecutionState {
    variables: BTreeMap<String, CapturedValue>,
    records: Vec<StepRecord>,
}

#[allow(clippy::too_many_arguments)]
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
                prepared.push((
                    *planned,
                    prepare_action(scenario, seed, action, context, &state.variables)?,
                ));
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
    for (planned, output) in outputs {
        let action = &scenario.actions[planned.index];
        capture_step_variables(scenario, seed, action, &output, &mut state.variables)?;
        let status = if planned.crash == Some(CrashTiming::After) {
            crash_action_process(scenario, seed, action, mcp)?;
            StepStatus::CompletedThenCrashed
        } else if matches!(action.action, ActionKind::Restart) {
            StepStatus::Restarted
        } else {
            StepStatus::Completed
        };
        state.records.push(record(action, &planned, status));
    }
    Ok(())
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
            params: render_template(scenario, seed, action, params, variables)?,
        }),
        ActionKind::CliJson { arguments } => Ok(PreparedAction::Cli {
            arguments: arguments
                .iter()
                .map(|argument| render_cli_argument(scenario, seed, action, argument, variables))
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ActionKind::HookEvent { event, payload } => {
            let payload = render_template(scenario, seed, action, payload, variables)?;
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
            let payload = hook_payload(
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
            })
        }
        ActionKind::Observe { source, selector } => Ok(PreparedAction::Observe {
            source: *source,
            selector: render_template(scenario, seed, action, selector, variables)?,
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
) -> Result<Value, ProcessError> {
    match action {
        PreparedAction::Mcp {
            session,
            method,
            params,
        } => mcp[&session]
            .lock()
            .map_err(|_| ProcessError {
                kind: ProcessErrorKind::Crashed,
                message: "MCP state lock failed",
            })?
            .call(&method, &params),
        PreparedAction::Cli { arguments } => {
            let mut command = vec!["--json".to_owned()];
            command.extend(arguments);
            let output = run_one_shot(context, &command, None, timeout)?;
            let value = parse_json_output(&output)?;
            Ok(value.get("data").cloned().unwrap_or(value))
        }
        PreparedAction::Hook { arguments, payload } => {
            let output = run_one_shot(context, &arguments, Some(&payload), timeout)?;
            parse_json_output(&output)
        }
        PreparedAction::Observe { source, selector } => observer
            .observe(root, source, &selector)
            .map_err(|_| ProcessError {
                kind: ProcessErrorKind::Observer,
                message: "read-only observer failed",
            })
            .and_then(|proof| {
                serde_json::to_value(proof.summary).map_err(|_| ProcessError {
                    kind: ProcessErrorKind::Observer,
                    message: "observer summary serialization failed",
                })
            }),
        PreparedAction::Restart { session } => {
            mcp[&session]
                .lock()
                .map_err(|_| ProcessError {
                    kind: ProcessErrorKind::Crashed,
                    message: "MCP state lock failed",
                })?
                .restart()?;
            Ok(Value::Null)
        }
    }
}

fn render_template(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    template: &TemplateValue,
    variables: &BTreeMap<String, CapturedValue>,
) -> Result<Value, RunnerFailure> {
    match template {
        TemplateValue::Null => Ok(Value::Null),
        TemplateValue::Boolean { value } => Ok(Value::Bool(*value)),
        TemplateValue::Integer { value } => Ok((*value).into()),
        TemplateValue::Unsigned { value } => Ok((*value).into()),
        TemplateValue::String { value } => Ok(Value::String(value.clone())),
        TemplateValue::Array { items } => items
            .iter()
            .map(|item| render_template(scenario, seed, action, item, variables))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        TemplateValue::Object { fields } => fields
            .iter()
            .map(|(key, value)| {
                render_template(scenario, seed, action, value, variables)
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
    }
}

fn render_cli_argument(
    scenario: &ScenarioDefinition,
    seed: u64,
    action: &ScenarioAction,
    template: &TemplateValue,
    variables: &BTreeMap<String, CapturedValue>,
) -> Result<String, RunnerFailure> {
    match render_template(scenario, seed, action, template, variables)? {
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
        VariableKind::RepositoryId => Some("rpo_"),
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
            Value::String(value)
                if !value.is_empty()
                    && value.len() <= 128
                    && value.bytes().all(|byte| byte.is_ascii_graphic()) =>
            {
                Ok(())
            }
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

fn template_is_scalar(template: &TemplateValue) -> bool {
    matches!(
        template,
        TemplateValue::Boolean { .. }
            | TemplateValue::Integer { .. }
            | TemplateValue::Unsigned { .. }
            | TemplateValue::String { .. }
            | TemplateValue::Variable { .. }
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
    let session = format!("scenario-{session_actor}");
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
            payload.insert("cwd".to_owned(), Value::String(workspace));
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
            payload.insert(
                "workspace_roots".to_owned(),
                Value::Array(vec![Value::String(workspace)]),
            );
        }
    }
    Some(Value::Object(payload))
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
