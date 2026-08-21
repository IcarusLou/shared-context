//! Stdio Model Context Protocol server for the five Shared Context V1 tools.
//!
//! The transport accepts the newline-delimited framing used by current MCP
//! clients and the `Content-Length` framing used by older fixtures. Read tools
//! are pinned to one projection snapshot or one explicitly named Git tree. The
//! durable write tool delegates ID generation and append-only enforcement to
//! the domain event constructor and [`sctx_git_store::GitStore`]; `task_context`
//! writes only disposable local Task Runtime state.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    io::{self, BufRead, Write},
    path::Path,
    str::FromStr,
};

use sctx_domain::{
    Applicability, ContextId, ContextKind, ContextRevisionDraft, Error, ErrorKind,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, Result, RevisionId, SignalId,
    SpaceId, TaskId, TaskIntentDraft, TaskIntentRevisionId, TaskSessionId, TaskSessionSnapshot,
    TaskSignal, TaskSignalLifecycle, TaskSignalRecord, TaskSpaceAssociation, WorkEpisodeId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::{DomainSnapshot, ProjectionIndex};
use sctx_search::{
    ConflictView, ContextPackOmitted, ContextStatus, DEFAULT_TASK_MAX_SPACES, MAX_TASK_MAX_SPACES,
    MIN_TASK_CONTEXT_TOKEN_BUDGET, ScopeFilter, SearchEngine, SearchFilters, SearchRequest,
    TaskContextItem, TaskContextRequest, TaskRetrievalPath,
};
use sctx_task_runtime::TaskRuntime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Protocol version advertised when a client does not provide one.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoundary {
    Continue,
    New,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentMaturity {
    Provisional,
    Grounded,
}

/// Required nullable CAS field. Unlike `Option<T>`, an omitted field fails deserialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExpectedRevisionId {
    Revision(String),
    Null(()),
}

impl ExpectedRevisionId {
    fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Revision(value) => Some(value),
            Self::Null(()) => None,
        }
    }
}

/// Complete authoritative Task Intent update. Every field is required by transport.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIntentUpdateInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub task_boundary: TaskBoundary,
    pub expected_revision_id: ExpectedRevisionId,
    pub maturity: IntentMaturity,
    pub intent: TaskIntentDraft,
    pub evidence_refs: Vec<String>,
}

/// Stable-ID Signal lifecycle update guarded by active Task and Intent revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSignalSupersedeInput {
    pub agent_kind: String,
    pub external_session_id: String,
    pub task_id: String,
    pub expected_revision_id: String,
    pub signal_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskIntentUpdateResponse {
    #[serde(flatten)]
    pub context: TaskContextResponse,
    pub maturity: IntentMaturity,
    pub evidence_refs: Vec<String>,
    pub active_signals: Vec<TaskSignalRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskSignalSupersedeResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub superseded_signal_ids: Vec<SignalId>,
    pub active_signals: Vec<TaskSignalRecord>,
}

/// Client fixture selected by the stable CLI entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientKind {
    Cursor,
    Codex,
}

impl FromStr for ClientKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "cursor" => Ok(Self::Cursor),
            "codex" => Ok(Self::Codex),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("unsupported MCP client {value:?}; expected cursor or codex"),
            )),
        }
    }
}

/// Why the observable stdio session ended normally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    CleanEof,
}

/// Terminal state returned by a completed server loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServeOutcome {
    pub disconnect: DisconnectReason,
    pub requests_handled: u64,
}

/// Read-only locator and output bounds accepted by `task_context`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskContextReadInput {
    pub agent_kind: String,
    pub external_session_id: String,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default = "default_max_spaces")]
    pub max_spaces: usize,
}

impl TaskContextReadInput {
    fn locator(&self) -> Result<ExternalSessionLocator> {
        ExternalSessionLocator::new(&self.agent_kind, &self.external_session_id)
    }

    fn validate(&self) -> Result<()> {
        let _locator = self.locator()?;
        if self.token_budget < MIN_TASK_CONTEXT_TOKEN_BUDGET {
            return Err(invalid(format!(
                "task_context token_budget must be at least {MIN_TASK_CONTEXT_TOKEN_BUDGET}"
            )));
        }
        if self.max_spaces == 0 || self.max_spaces > MAX_TASK_MAX_SPACES {
            return Err(invalid(format!(
                "task_context max_spaces must be between 1 and {MAX_TASK_MAX_SPACES}"
            )));
        }
        Ok(())
    }
}

/// Flattened explanation index for one returned Task Context item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskContextRetrievalPaths {
    pub association_space_id: SpaceId,
    pub context_id: ContextId,
    pub paths: Vec<TaskRetrievalPath>,
}

/// Session-aware Task Context result shared by MCP and the CLI test entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContextResponse {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub intent_revision_id: TaskIntentRevisionId,
    pub candidate_spaces: Vec<TaskSpaceAssociation>,
    pub items: Vec<TaskContextItem>,
    pub retrieval_paths: Vec<TaskContextRetrievalPaths>,
    pub task_fingerprint: String,
    pub tree: String,
    pub generation: u64,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub omitted: Vec<ContextPackOmitted>,
}

/// Typed failures for transport state that cannot be represented as a JSON-RPC response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorKind {
    Io,
    InvalidFrame,
    UnexpectedEof,
    FrameTooLarge,
}

/// User-presentable typed stdio transport error.
#[derive(Debug)]
pub struct TransportError {
    kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> TransportErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameStyle {
    Newline,
    ContentLength,
}

struct Frame {
    body: Vec<u8>,
    style: FrameStyle,
}

struct Runtime {
    store: GitStore,
    index: ProjectionIndex,
    tasks: TaskRuntime,
}

impl Runtime {
    fn open(root: &Path) -> Result<Self> {
        let store = GitStore::initialize(root)?;
        let index = ProjectionIndex::for_store(&store);
        let tasks = TaskRuntime::initialize(root)?;
        Ok(Self {
            store,
            index,
            tasks,
        })
    }

    fn snapshot(&self) -> Result<DomainSnapshot> {
        self.index.domain_snapshot()
    }

    fn task_context_readonly(&self, input: &TaskContextReadInput) -> Result<TaskContextResponse> {
        input.validate()?;
        let locator = input.locator()?;
        let snapshot = self
            .tasks
            .read_snapshot_by_locator(&locator)?
            .ok_or_else(|| {
                invalid("task_context requires task_intent_update to establish an ActiveTask")
            })?;
        build_task_context_response(&self.index, &snapshot, input.token_budget, input.max_spaces)
    }

    fn task_intent_update(
        &self,
        input: &TaskIntentUpdateInput,
    ) -> Result<TaskIntentUpdateResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let active = self.tasks.read_snapshot_by_locator(&locator)?;
        let snapshot = match input.task_boundary {
            TaskBoundary::Continue => {
                let active = active.ok_or_else(|| {
                    invalid("task_boundary=continue requires an existing ActiveTask")
                })?;
                require_expected_revision(&active, input.expected_revision_id.as_deref())?;
                validate_intent_update(input, &active.task_signals)?;
                let parent = active
                    .current_intent_revision()
                    .ok_or_else(|| invariant("ActiveTask has no Intent Head"))?
                    .revision_id;
                self.tasks.append_intent_revision(
                    active.task_session_id,
                    parent,
                    input.intent.bind(active.task_id),
                )?;
                self.tasks
                    .read_snapshot(active.task_session_id)?
                    .ok_or_else(|| invariant("updated ActiveTask disappeared"))?
            }
            TaskBoundary::New => {
                if let Some(active) = active {
                    require_expected_revision(&active, input.expected_revision_id.as_deref())?;
                    validate_intent_update(input, &[])?;
                    self.tasks
                        .start_new_task(&locator, active.task_id, &input.intent, Vec::new())?
                        .snapshot
                } else {
                    if input.expected_revision_id.as_deref().is_some() {
                        return Err(invalid(
                            "expected_revision_id must be null when no ExternalSession exists",
                        ));
                    }
                    validate_intent_update(input, &[])?;
                    let task_id = TaskId::new();
                    self.tasks
                        .open_or_create(locator, input.intent.bind(task_id), Vec::new())?
                        .snapshot
                }
            }
        };
        let context = build_task_context_response(
            &self.index,
            &snapshot,
            default_token_budget(),
            default_max_spaces(),
        )?;
        Ok(TaskIntentUpdateResponse {
            active_signals: active_signal_records(&self.tasks, snapshot.task_session_id)?,
            context,
            maturity: input.maturity,
            evidence_refs: input.evidence_refs.clone(),
        })
    }

    fn task_signal_supersede(
        &self,
        input: &TaskSignalSupersedeInput,
    ) -> Result<TaskSignalSupersedeResponse> {
        let locator = ExternalSessionLocator::new(&input.agent_kind, &input.external_session_id)?;
        let active = self
            .tasks
            .read_snapshot_by_locator(&locator)?
            .ok_or_else(|| invalid("ExternalSession has no ActiveTask"))?;
        let task_id = input
            .task_id
            .parse::<TaskId>()
            .map_err(|error| invalid(format!("invalid task_id: {error}")))?;
        if task_id != active.task_id {
            return Err(invalid("task_id does not identify the ActiveTask"));
        }
        require_expected_revision(&active, Some(&input.expected_revision_id))?;
        let signal_ids = input
            .signal_ids
            .iter()
            .map(|value| {
                value
                    .parse::<SignalId>()
                    .map_err(|error| invalid(format!("invalid signal_id: {error}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let outcome =
            self.tasks
                .supersede_signals(active.task_session_id, active.task_id, signal_ids)?;
        let revision_id = outcome
            .snapshot
            .current_intent_revision()
            .ok_or_else(|| invariant("ActiveTask has no Intent Head"))?
            .revision_id;
        Ok(TaskSignalSupersedeResponse {
            task_session_id: active.task_session_id,
            task_id: active.task_id,
            intent_revision_id: revision_id,
            superseded_signal_ids: outcome.superseded_signal_ids,
            active_signals: active_signal_records(&self.tasks, active.task_session_id)?,
        })
    }
}

fn require_expected_revision(
    snapshot: &TaskSessionSnapshot,
    expected: Option<&str>,
) -> Result<TaskIntentRevisionId> {
    let expected = expected.ok_or_else(|| {
        invalid("expected_revision_id must be non-null when an ActiveTask exists")
    })?;
    let expected = expected
        .parse::<TaskIntentRevisionId>()
        .map_err(|error| invalid(format!("invalid expected_revision_id: {error}")))?;
    let actual = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("ActiveTask has no Intent Head"))?
        .revision_id;
    if expected != actual {
        return Err(invalid(format!(
            "expected_revision_id is stale; current Intent Head is {actual}"
        )));
    }
    Ok(actual)
}

fn validate_intent_update(
    input: &TaskIntentUpdateInput,
    active_signals: &[TaskSignal],
) -> Result<()> {
    input.intent.validate()?;
    if normalize_semantic(&input.intent.goal) == normalize_semantic(&input.intent.desired_change) {
        return Err(invalid(
            "intent.goal and intent.desired_change must be semantically distinct",
        ));
    }
    let in_scope = normalized_values(&input.intent.in_scope);
    let out_of_scope = normalized_values(&input.intent.out_of_scope);
    if let Some(overlap) = in_scope.intersection(&out_of_scope).next() {
        return Err(invalid(format!(
            "intent.in_scope and intent.out_of_scope overlap: {overlap}"
        )));
    }
    let evidence = validate_evidence_refs(&input.evidence_refs)?;
    if input.maturity == IntentMaturity::Grounded && evidence.is_empty() {
        return Err(invalid(
            "maturity=grounded requires at least one evidence_ref",
        ));
    }
    let signal_support = active_signals
        .iter()
        .map(|signal| normalize_semantic(&signal.content))
        .collect::<HashSet<_>>();
    for (field, values) in [
        ("intent.artifacts", &input.intent.artifacts),
        ("intent.interfaces", &input.intent.interfaces),
    ] {
        for value in values {
            let normalized = normalize_semantic(value);
            if !signal_support.contains(&normalized) && !evidence.contains(&normalized) {
                return Err(invalid(format!(
                    "{field} item lacks active TaskSignal or evidence_ref support: {value}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_evidence_refs(values: &[String]) -> Result<HashSet<String>> {
    let mut normalized = HashSet::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let value = normalize_semantic(value);
        if value.is_empty() {
            return Err(invalid(format!("evidence_refs[{index}] must not be empty")));
        }
        if !normalized.insert(value) {
            return Err(invalid("evidence_refs must not contain duplicates"));
        }
    }
    Ok(normalized)
}

fn normalized_values(values: &[String]) -> HashSet<String> {
    values
        .iter()
        .map(|value| normalize_semantic(value))
        .collect()
}

fn normalize_semantic(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn active_signal_records(
    runtime: &TaskRuntime,
    task_session_id: TaskSessionId,
) -> Result<Vec<TaskSignalRecord>> {
    Ok(runtime
        .read_signal_history(task_session_id)?
        .into_iter()
        .filter(|record| record.lifecycle == TaskSignalLifecycle::Active)
        .collect())
}

/// Reads a Context Pack for an already-authoritative `ActiveTask` without mutation.
///
/// # Errors
///
/// Returns an input error when no strict Task Intent update established the Task.
pub fn task_context_readonly_at_root(
    root: impl AsRef<Path>,
    input: &TaskContextReadInput,
) -> Result<TaskContextResponse> {
    Runtime::open(root.as_ref())?.task_context_readonly(input)
}

/// Applies an authoritative Task Intent CAS update and returns its new Context Pack.
///
/// # Errors
///
/// Returns typed validation, CAS, runtime, or Search errors.
pub fn task_intent_update_at_root(
    root: impl AsRef<Path>,
    input: &TaskIntentUpdateInput,
) -> Result<TaskIntentUpdateResponse> {
    Runtime::open(root.as_ref())?.task_intent_update(input)
}

/// Supersedes active Signal IDs under exact Task and Intent CAS guards.
///
/// # Errors
///
/// Returns typed validation, CAS, or runtime errors.
pub fn task_signal_supersede_at_root(
    root: impl AsRef<Path>,
    input: &TaskSignalSupersedeInput,
) -> Result<TaskSignalSupersedeResponse> {
    Runtime::open(root.as_ref())?.task_signal_supersede(input)
}

fn build_task_context_response(
    index: &ProjectionIndex,
    snapshot: &TaskSessionSnapshot,
    token_budget: usize,
    max_spaces: usize,
) -> Result<TaskContextResponse> {
    let current = snapshot
        .current_intent_revision()
        .ok_or_else(|| invariant("Task Session has no current Intent revision"))?;
    let mut request = TaskContextRequest::automatic(
        current.intent.clone(),
        snapshot.task_signals.clone(),
        token_budget,
    );
    request.max_spaces = max_spaces;
    let pack = SearchEngine::new(index.clone()).task_context_pack(&request)?;
    let retrieval_paths = pack
        .items
        .iter()
        .map(|item| TaskContextRetrievalPaths {
            association_space_id: item.association_space_id,
            context_id: item.context.context_id,
            paths: item.retrieval_paths.clone(),
        })
        .collect();
    Ok(TaskContextResponse {
        task_session_id: snapshot.task_session_id,
        task_id: snapshot.task_id,
        intent_revision_id: current.revision_id,
        candidate_spaces: pack.associations,
        items: pack.items,
        retrieval_paths,
        task_fingerprint: pack.task_fingerprint,
        tree: pack.indexed_tree_oid,
        generation: pack.projection_generation,
        token_budget: pack.token_budget,
        estimated_tokens: pack.estimated_tokens,
        omitted: pack.omitted,
    })
}

/// Stateful MCP request dispatcher for one stdio session.
pub struct McpServer {
    runtime: Runtime,
    client: ClientKind,
    initialized: bool,
}

impl McpServer {
    /// Opens the unique store, rebuildable projection, and local Task Runtime.
    ///
    /// # Errors
    ///
    /// Returns storage/configuration errors from the existing runtime boundaries.
    pub fn new(root: impl AsRef<Path>, client: ClientKind) -> Result<Self> {
        Ok(Self {
            runtime: Runtime::open(root.as_ref())?,
            client,
            initialized: false,
        })
    }

    /// Runs requests until stdin reaches a clean EOF.
    ///
    /// # Errors
    ///
    /// Returns a typed transport failure for invalid framing, truncated frames,
    /// or an I/O failure. JSON and JSON-RPC failures are written to the peer and
    /// do not terminate the session.
    pub fn serve<R: BufRead, W: Write>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
    ) -> std::result::Result<ServeOutcome, TransportError> {
        let mut requests_handled = 0_u64;
        loop {
            let Some(frame) = read_frame(reader)? else {
                return Ok(ServeOutcome {
                    disconnect: DisconnectReason::CleanEof,
                    requests_handled,
                });
            };
            requests_handled = requests_handled.saturating_add(1);
            let response = match serde_json::from_slice::<Value>(&frame.body) {
                Ok(request) => self.dispatch(request),
                Err(error) => Some(rpc_error(
                    Value::Null,
                    -32_700,
                    "Parse error",
                    "parse_error",
                    Some(error.to_string()),
                )),
            };
            if let Some(response) = response {
                write_frame(writer, &response, frame.style)?;
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dispatch(&mut self, request: Value) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(rpc_error(
                Value::Null,
                -32_600,
                "Invalid Request",
                "invalid_request",
                Some("request must be a JSON object".to_owned()),
            ));
        };
        let id = object.get("id").cloned();
        if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
            return Some(rpc_error(
                id.unwrap_or(Value::Null),
                -32_600,
                "Invalid Request",
                "invalid_request",
                Some("jsonrpc must be \"2.0\"".to_owned()),
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(rpc_error(
                id.unwrap_or(Value::Null),
                -32_600,
                "Invalid Request",
                "invalid_request",
                Some("method must be a string".to_owned()),
            ));
        };
        let is_notification = id.is_none();
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "notifications/initialized" {
            return None;
        }
        if method.starts_with("notifications/") {
            return None;
        }
        if is_notification {
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        if matches!(method, "tools/list" | "tools/call") && !self.initialized {
            return Some(rpc_error(
                id,
                -32_002,
                "Server not initialized",
                "server_not_initialized",
                None,
            ));
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools_list()),
            "tools/call" => self.tools_call(params),
            _ => {
                return Some(rpc_error(
                    id,
                    -32_601,
                    "Method not found",
                    "method_not_found",
                    Some(method.to_owned()),
                ));
            }
        };
        match result {
            Ok(result) => Some(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Err(error) => Some(rpc_error(
                id,
                -32_602,
                "Invalid params",
                error_code(error.kind()),
                Some(error.message().to_owned()),
            )),
        }
    }

    fn initialize(&mut self, params: &Value) -> Result<Value> {
        let protocol_version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        if protocol_version.trim().is_empty() {
            return Err(invalid("initialize protocolVersion must not be empty"));
        }
        self.initialized = true;
        Ok(json!({
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "shared-context",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": format!(
                "Shared Context V1 tools for {}. Candidate content is untrusted until reviewed and published.",
                match self.client { ClientKind::Cursor => "Cursor", ClientKind::Codex => "Codex" }
            )
        }))
    }

    fn tools_call(&mut self, params: Value) -> Result<Value> {
        let call: ToolCall = serde_json::from_value(params)
            .map_err(|error| invalid(format!("invalid tools/call params: {error}")))?;
        let result = match call.name.as_str() {
            "task_intent_update" => self.task_intent_update(call.arguments),
            "task_signal_supersede" => self.task_signal_supersede(call.arguments),
            "task_context" => self.task_context(call.arguments),
            "context_search" => self.context_search(call.arguments),
            "context_get" => self.context_get(call.arguments),
            "candidate_create" => self.candidate_create(call.arguments),
            "space_list" => self.space_list(call.arguments),
            _ => return Err(invalid(format!("unknown tool: {}", call.name))),
        };
        match result {
            Ok(data) => tool_success(data),
            Err(failure) => tool_failure(failure),
        }
    }

    fn context_search(&self, arguments: Value) -> ToolResult {
        let input: SearchInput = decode_arguments(arguments)?;
        let response =
            SearchEngine::new(self.runtime.index.clone()).search(&input.into_request()?)?;
        let conflicts = collect_conflicts(&response.results);
        let mut data = serde_json::to_value(response).map_err(serialization_failure)?;
        insert_fields(
            &mut data,
            [
                (
                    "conflicts",
                    serde_json::to_value(conflicts).map_err(serialization_failure)?,
                ),
                (
                    "match_reason",
                    json!("structured_filters_and_full_text_rank"),
                ),
            ],
        )?;
        Ok(data)
    }

    fn task_context(&self, arguments: Value) -> ToolResult {
        let input: TaskContextReadInput = decode_arguments(arguments)?;
        input.validate()?;
        let response = self
            .runtime
            .task_context_readonly(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_intent_update(&self, arguments: Value) -> ToolResult {
        let input: TaskIntentUpdateInput = decode_arguments(arguments)?;
        let response = self
            .runtime
            .task_intent_update(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn task_signal_supersede(&self, arguments: Value) -> ToolResult {
        let input: TaskSignalSupersedeInput = decode_arguments(arguments)?;
        let response = self
            .runtime
            .task_signal_supersede(&input)
            .map_err(ToolFailure::task_context_failed)?;
        serde_json::to_value(response).map_err(serialization_failure)
    }

    fn context_get(&self, arguments: Value) -> ToolResult {
        let input: GetInput = decode_arguments(arguments)?;
        let context_id = parse_id::<ContextId>(&input.context_id, "context_id")?;
        let revision_id = input
            .revision_id
            .as_deref()
            .map(|value| parse_id::<RevisionId>(value, "revision_id"))
            .transpose()?;
        let space_id = input
            .space_id
            .as_deref()
            .map(|value| parse_id::<SpaceId>(value, "space_id"))
            .transpose()?;
        let snapshot = self.runtime.snapshot()?;
        let (found_space_id, context) = find_context(&snapshot, space_id, context_id)?;
        let value = if let Some(revision_id) = revision_id {
            let revision = context.revisions.get(&revision_id).ok_or_else(|| {
                invalid(format!(
                    "revision {revision_id} does not belong to Context {context_id}"
                ))
            })?;
            serde_json::to_value(revision).map_err(serialization_failure)?
        } else {
            serde_json::to_value(context).map_err(serialization_failure)?
        };
        Ok(json!({
            "indexed_tree_oid": snapshot.metadata.indexed_tree_oid,
            "projection_generation": snapshot.metadata.projection_generation,
            "space_id": found_space_id,
            "context_id": context_id,
            "context": value,
            "conflicts": context_conflicts(&snapshot, context_id),
            "match_reason": {"kind": "exact_context_id", "context_id": context_id},
        }))
    }

    fn space_list(&self, arguments: Value) -> ToolResult {
        let _: EmptyInput = decode_arguments(arguments)?;
        let snapshot = self.runtime.snapshot()?;
        let spaces = snapshot
            .projection
            .spaces
            .values()
            .map(|space| {
                let titles = space
                    .intent
                    .heads
                    .iter()
                    .filter_map(|id| space.intent.revisions.get(id))
                    .map(|revision| revision.intent.title.clone())
                    .collect::<Vec<_>>();
                json!({
                    "space_id": space.space_id,
                    "intent_heads": space.intent.heads,
                    "titles": titles,
                    "context_count": space.contexts.len(),
                    "conflicts": if space.intent.heads.len() > 1 {
                        vec![json!({"kind": "intent", "status": "open", "heads": space.intent.heads})]
                    } else { Vec::new() },
                    "match_reason": "available_space",
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "indexed_tree_oid": snapshot.metadata.indexed_tree_oid,
            "projection_generation": snapshot.metadata.projection_generation,
            "spaces": spaces,
            "conflicts": snapshot.projection.spaces.values().filter(|space| space.intent.heads.len() > 1).count(),
            "match_reason": "all_available_spaces",
        }))
    }

    fn candidate_create(&mut self, arguments: Value) -> ToolResult {
        let input: CandidateCreateInput = decode_arguments(arguments)?;
        let source_episode_id =
            parse_id::<WorkEpisodeId>(&input.source_episode_id, "source_episode_id")?;
        let event = Event::context_candidate_created(source_episode_id, input.into_draft(), None)?;
        let outcome = self
            .runtime
            .store
            .append_candidate_once(AppendRequest::event(event))
            .map_err(ToolFailure::writer_rejected)?;
        let (candidate_id, source_episode_id) = match outcome.event.payload() {
            EventPayload::ContextCandidateCreated { candidate } => {
                (candidate.candidate_id, candidate.source_episode_id)
            }
            _ => unreachable!(),
        };
        let event_id = outcome.event.event_id();
        let snapshot = self
            .runtime
            .snapshot()
            .map_err(ToolFailure::index_sync_failed)?;
        Ok(json!({
            "indexed_tree_oid": snapshot.metadata.indexed_tree_oid,
            "projection_generation": snapshot.metadata.projection_generation,
            "candidate_id": candidate_id,
            "source_episode_id": source_episode_id,
            "event_id": event_id,
            "status": "candidate",
            "created": outcome.created,
            "batch_id": outcome.append.batch_id,
            "commit_oid": outcome.append.commit_oid,
            "conflicts": [],
            "match_reason": if outcome.created { "new_candidate_created" } else { "identical_candidate_reused" },
        }))
    }
}

/// Runs a server using process stdio.
///
/// # Errors
///
/// Returns initialization errors or typed transport errors mapped to the shared
/// external-error category for the CLI boundary.
pub fn serve_stdio(root: impl AsRef<Path>, client: ClientKind) -> Result<ServeOutcome> {
    let mut server = McpServer::new(root, client)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    server
        .serve(&mut stdin.lock(), &mut stdout.lock())
        .map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("MCP transport {:?}: {}", error.kind(), error.message()),
            )
        })
}

type ToolResult = std::result::Result<Value, ToolFailure>;

struct ToolFailure {
    code: &'static str,
    error: Error,
}

impl ToolFailure {
    fn writer_rejected(error: Error) -> Self {
        Self {
            code: "writer_rejected",
            error,
        }
    }

    fn index_sync_failed(error: Error) -> Self {
        Self {
            code: "index_sync_failed",
            error,
        }
    }

    fn task_context_failed(error: Error) -> Self {
        let code = match error.kind() {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::InvariantViolation => "task_context_invariant",
            ErrorKind::Io => "task_context_storage_failed",
            ErrorKind::External => "task_runtime_conflict",
            ErrorKind::Unsupported => "task_context_unsupported",
            _ => "task_context_failed",
        };
        Self { code, error }
    }
}

impl From<Error> for ToolFailure {
    fn from(error: Error) -> Self {
        Self {
            code: error_code(error.kind()),
            error,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
struct GetInput {
    context_id: String,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    revision_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    #[serde(default)]
    query: String,
    #[serde(default)]
    space_ids: Vec<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    conditions: Vec<String>,
    #[serde(default)]
    kinds: Vec<ContextKind>,
    #[serde(default)]
    statuses: Vec<ContextStatus>,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    cursor: Option<String>,
}

impl SearchInput {
    fn into_request(self) -> ToolResultSearch {
        Ok(SearchRequest {
            query: self.query,
            filters: SearchFilters {
                space_ids: self
                    .space_ids
                    .iter()
                    .map(|value| parse_id(value, "space_ids"))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                scope: ScopeFilter {
                    domains: self.domains,
                    platforms: self.platforms,
                    conditions: self.conditions,
                },
                kinds: self.kinds,
                statuses: self.statuses,
            },
            page_size: self.page_size,
            cursor: self.cursor,
        })
    }
}

type ToolResultSearch = std::result::Result<SearchRequest, ToolFailure>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCreateInput {
    source_episode_id: String,
    kind: ContextKind,
    #[serde(default)]
    topic_key: Option<String>,
    statement: String,
    rationale: String,
    #[serde(default)]
    applicability: Applicability,
    #[serde(default)]
    assumptions: Vec<String>,
    #[serde(default)]
    recheck_when: Vec<String>,
    evidence: Vec<EvidenceInput>,
}

impl CandidateCreateInput {
    fn into_draft(self) -> ContextRevisionDraft {
        ContextRevisionDraft {
            kind: self.kind,
            topic_key: self.topic_key,
            statement: self.statement,
            rationale: self.rationale,
            applicability: self.applicability,
            assumptions: self.assumptions,
            recheck_when: self.recheck_when,
            evidence: self.evidence.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceInput {
    kind: EvidenceType,
    supports: String,
    content: Value,
    interpretation: String,
    #[serde(default)]
    limitations: Vec<String>,
}

impl From<EvidenceInput> for EvidenceSnapshotDraft {
    fn from(input: EvidenceInput) -> Self {
        Self {
            kind: input.kind,
            supports: input.supports,
            content: input.content,
            interpretation: input.interpretation,
            limitations: input.limitations,
        }
    }
}

fn tools_list() -> Value {
    json!({"tools": [
        tool_schema(
            "task_intent_update",
            "CAS-update a complete Task Intent, optionally start a new explicit Task, and return its TaskContextPack.",
            task_intent_update_schema()
        ),
        tool_schema(
            "task_signal_supersede",
            "Supersede stable active Signal IDs under exact Task and Intent CAS guards.",
            task_signal_supersede_schema()
        ),
        tool_schema(
            "task_context",
            "Read the Context Pack for an existing authoritative ActiveTask without changing runtime state.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["agent_kind", "external_session_id"],
                "properties": {
                    "agent_kind": {"type": "string", "minLength": 1},
                    "external_session_id": {"type": "string", "minLength": 1},
                    "token_budget": {"type": "integer", "minimum": MIN_TASK_CONTEXT_TOKEN_BUDGET, "default": 2000},
                    "max_spaces": {"type": "integer", "minimum": 1, "maximum": MAX_TASK_MAX_SPACES, "default": DEFAULT_TASK_MAX_SPACES}
                }
            })
        ),
        tool_schema(
            "context_search",
            "Search Context revisions with stable filters, pagination, conflicts, and match reasons.",
            search_schema()
        ),
        tool_schema(
            "context_get",
            "Get one Context or immutable revision from a deterministic Git tree.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["context_id"],
                "properties": {
                    "context_id": id_schema("ctx_"),
                    "space_id": id_schema("spc_"),
                    "revision_id": id_schema("rev_")
                }
            })
        ),
        tool_schema(
            "candidate_create",
            "Create one unassigned Context Candidate from a source Work Episode. Complete authoritative retries are idempotent; IDs and paths remain server-owned.",
            candidate_create_schema()
        ),
        tool_schema(
            "space_list",
            "List available ContextSpaces from a deterministic Git tree.",
            json!({"type": "object", "additionalProperties": false, "properties": {}})
        )
    ]})
}

fn task_intent_update_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "task_boundary", "expected_revision_id", "maturity", "intent", "evidence_refs"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "task_boundary": {"type": "string", "enum": ["continue", "new"]},
            "expected_revision_id": {
                "anyOf": [id_schema("tir_"), {"type": "null"}]
            },
            "maturity": {"type": "string", "enum": ["provisional", "grounded"]},
            "intent": {
                "type": "object",
                "additionalProperties": false,
                "required": ["goal", "desired_change", "in_scope", "out_of_scope", "domains", "platforms", "constraints", "acceptance_conditions", "artifacts", "interfaces", "unknowns"],
                "properties": {
                    "goal": {"type": "string", "minLength": 1},
                    "desired_change": {"type": "string", "minLength": 1},
                    "in_scope": string_array_schema(),
                    "out_of_scope": string_array_schema(),
                    "domains": string_array_schema(),
                    "platforms": string_array_schema(),
                    "constraints": string_array_schema(),
                    "acceptance_conditions": string_array_schema(),
                    "artifacts": string_array_schema(),
                    "interfaces": string_array_schema(),
                    "unknowns": string_array_schema()
                }
            },
            "evidence_refs": string_array_schema()
        }
    })
}

fn task_signal_supersede_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["agent_kind", "external_session_id", "task_id", "expected_revision_id", "signal_ids"],
        "properties": {
            "agent_kind": {"type": "string", "minLength": 1},
            "external_session_id": {"type": "string", "minLength": 1},
            "task_id": id_schema("tsk_"),
            "expected_revision_id": id_schema("tir_"),
            "signal_ids": {"type": "array", "minItems": 1, "items": id_schema("sig_")}
        }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn tool_schema(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {"type": "string", "default": ""},
            "space_ids": {"type": "array", "items": id_schema("spc_")},
            "domains": string_array_schema(),
            "platforms": string_array_schema(),
            "conditions": string_array_schema(),
            "kinds": kind_array_schema(),
            "statuses": {"type": "array", "items": {"type": "string", "enum": ["candidate", "accepted", "deprecated", "superseded", "governance_conflict"]}},
            "page_size": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20},
            "cursor": {"type": "string"}
        }
    })
}

fn candidate_create_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["source_episode_id", "kind", "statement", "rationale", "evidence"],
        "properties": {
            "source_episode_id": id_schema("wep_"),
            "kind": kind_schema(),
            "topic_key": {"type": "string", "minLength": 1},
            "statement": {"type": "string", "minLength": 1},
            "rationale": {"type": "string", "minLength": 1},
            "applicability": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "domains": string_array_schema(),
                    "platforms": string_array_schema(),
                    "conditions": string_array_schema()
                }
            },
            "assumptions": string_array_schema(),
            "recheck_when": string_array_schema(),
            "evidence": {
                "type": "array", "minItems": 1,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["kind", "supports", "content", "interpretation"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["source_snapshot", "experiment_record", "artifact_snapshot"]},
                        "supports": {"type": "string", "minLength": 1},
                        "content": {"type": "object", "minProperties": 1},
                        "interpretation": {"type": "string", "minLength": 1},
                        "limitations": string_array_schema()
                    }
                }
            }
        },
        "allOf": [{
            "if": {"properties": {"kind": {"enum": ["decision", "contract"]}}, "required": ["kind"]},
            "then": {"required": ["topic_key"]}
        }]
    })
}

fn kind_schema() -> Value {
    json!({"type": "string", "enum": ["decision", "contract", "issue", "risk", "validation", "discovery", "progress"]})
}

fn kind_array_schema() -> Value {
    json!({"type": "array", "items": kind_schema()})
}

fn string_array_schema() -> Value {
    json!({"type": "array", "items": {"type": "string", "minLength": 1}})
}

fn id_schema(prefix: &str) -> Value {
    json!({"type": "string", "pattern": format!("^{prefix}[0-9a-fA-F-]+$")})
}

fn read_frame<R: BufRead>(reader: &mut R) -> std::result::Result<Option<Frame>, TransportError> {
    let mut first = String::new();
    loop {
        first.clear();
        let bytes = reader
            .read_line(&mut first)
            .map_err(transport_io("read MCP frame"))?;
        if bytes == 0 {
            return Ok(None);
        }
        trim_line_ending(&mut first);
        if !first.is_empty() {
            break;
        }
    }
    if first.to_ascii_lowercase().starts_with("content-length:") {
        let length = first
            .split_once(':')
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .ok_or_else(|| {
                TransportError::new(TransportErrorKind::InvalidFrame, "invalid Content-Length")
            })?;
        if length > MAX_FRAME_BYTES {
            return Err(TransportError::new(
                TransportErrorKind::FrameTooLarge,
                format!("MCP frame is {length} bytes; maximum is {MAX_FRAME_BYTES}"),
            ));
        }
        loop {
            let mut header = String::new();
            let bytes = reader
                .read_line(&mut header)
                .map_err(transport_io("read MCP header"))?;
            if bytes == 0 {
                return Err(TransportError::new(
                    TransportErrorKind::UnexpectedEof,
                    "MCP peer disconnected inside headers",
                ));
            }
            trim_line_ending(&mut header);
            if header.is_empty() {
                break;
            }
            if !header.contains(':') {
                return Err(TransportError::new(
                    TransportErrorKind::InvalidFrame,
                    format!("invalid MCP header: {header}"),
                ));
            }
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                TransportError::new(
                    TransportErrorKind::UnexpectedEof,
                    "MCP peer disconnected inside a message body",
                )
            } else {
                TransportError::new(TransportErrorKind::Io, format!("read MCP body: {error}"))
            }
        })?;
        Ok(Some(Frame {
            body,
            style: FrameStyle::ContentLength,
        }))
    } else {
        if first.len() > MAX_FRAME_BYTES {
            return Err(TransportError::new(
                TransportErrorKind::FrameTooLarge,
                format!("MCP frame exceeds {MAX_FRAME_BYTES} bytes"),
            ));
        }
        Ok(Some(Frame {
            body: first.into_bytes(),
            style: FrameStyle::Newline,
        }))
    }
}

fn write_frame<W: Write>(
    writer: &mut W,
    response: &Value,
    style: FrameStyle,
) -> std::result::Result<(), TransportError> {
    let body = serde_json::to_vec(response).map_err(|error| {
        TransportError::new(
            TransportErrorKind::Io,
            format!("serialize MCP response: {error}"),
        )
    })?;
    match style {
        FrameStyle::Newline => {
            writer
                .write_all(&body)
                .map_err(transport_io("write MCP response"))?;
            writer
                .write_all(b"\n")
                .map_err(transport_io("write MCP delimiter"))?;
        }
        FrameStyle::ContentLength => {
            writer
                .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                .map_err(transport_io("write MCP header"))?;
            writer
                .write_all(&body)
                .map_err(transport_io("write MCP response"))?;
        }
    }
    writer.flush().map_err(transport_io("flush MCP response"))
}

fn trim_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn transport_io(context: &'static str) -> impl FnOnce(io::Error) -> TransportError {
    move |error| TransportError::new(TransportErrorKind::Io, format!("{context}: {error}"))
}

#[allow(clippy::needless_pass_by_value)]
fn rpc_error(
    id: Value,
    code: i64,
    message: &str,
    typed_code: &str,
    detail: Option<String>,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": {"code": typed_code, "detail": detail}
        }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn tool_success(data: Value) -> Result<Value> {
    let text = serde_json::to_string_pretty(&data)
        .map_err(|error| Error::new(ErrorKind::Io, format!("serialize tool result: {error}")))?;
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": data,
        "isError": false,
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn tool_failure(failure: ToolFailure) -> Result<Value> {
    let data = json!({
        "error": {
            "code": failure.code,
            "kind": error_code(failure.error.kind()),
            "message": failure.error.message(),
        }
    });
    let text = serde_json::to_string_pretty(&data)
        .map_err(|error| Error::new(ErrorKind::Io, format!("serialize tool error: {error}")))?;
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": data,
        "isError": true,
    }))
}

fn decode_arguments<T: for<'de> Deserialize<'de>>(
    arguments: Value,
) -> std::result::Result<T, ToolFailure> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolFailure::from(invalid(format!("invalid tool arguments: {error}"))))
}

fn parse_id<T>(value: &str, field: &str) -> std::result::Result<T, ToolFailure>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse()
        .map_err(|error| invalid(format!("invalid {field}: {error}")).into())
}

fn find_context(
    snapshot: &DomainSnapshot,
    space_id: Option<SpaceId>,
    context_id: ContextId,
) -> std::result::Result<(SpaceId, &sctx_domain::ContextProjection), ToolFailure> {
    if let Some(space_id) = space_id {
        let context = snapshot
            .projection
            .spaces
            .get(&space_id)
            .and_then(|space| space.contexts.get(&context_id))
            .ok_or_else(|| {
                ToolFailure::from(invalid(format!(
                    "Context {context_id} does not belong to Space {space_id}"
                )))
            })?;
        return Ok((space_id, context));
    }
    snapshot
        .projection
        .spaces
        .iter()
        .find_map(|(space_id, space)| {
            space
                .contexts
                .get(&context_id)
                .map(|context| (*space_id, context))
        })
        .ok_or_else(|| ToolFailure::from(invalid(format!("Context does not exist: {context_id}"))))
}

fn context_conflicts(snapshot: &DomainSnapshot, context_id: ContextId) -> Vec<Value> {
    let mut conflicts = Vec::new();
    for conflict in snapshot.projection.semantic_conflicts.values() {
        if conflict
            .conflict
            .participants
            .iter()
            .any(|participant| participant.context_id == context_id)
        {
            conflicts.push(json!({
                "kind": "semantic",
                "conflict_id": conflict.conflict.conflict_id,
                "status": conflict.status,
                "participants": conflict.conflict.participants,
                "reason": conflict.conflict.reason,
            }));
        }
    }
    if let Some(context) = snapshot
        .projection
        .spaces
        .values()
        .find_map(|space| space.contexts.get(&context_id))
        && context.publication_heads.len() > 1
    {
        conflicts.push(json!({
            "kind": "governance",
            "status": "open",
            "publication_heads": context.publication_heads,
            "reason": "multiple publication heads",
        }));
    }
    conflicts
}

fn collect_conflicts(results: &[sctx_search::SearchResult]) -> Vec<ConflictView> {
    let mut by_id = BTreeMap::new();
    for conflict in results.iter().flat_map(|result| &result.conflicts) {
        by_id
            .entry(conflict.conflict_id.clone())
            .or_insert_with(|| conflict.clone());
    }
    by_id.into_values().collect()
}

fn insert_fields<const N: usize>(
    data: &mut Value,
    fields: [(&str, Value); N],
) -> std::result::Result<(), ToolFailure> {
    let object = data
        .as_object_mut()
        .ok_or_else(|| ToolFailure::from(invariant("serialized response is not an object")))?;
    for (name, value) in fields {
        object.insert(name.to_owned(), value);
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn serialization_failure(error: serde_json::Error) -> ToolFailure {
    ToolFailure::from(Error::new(
        ErrorKind::Io,
        format!("serialize MCP response: {error}"),
    ))
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

const fn default_page_size() -> usize {
    20
}

const fn default_token_budget() -> usize {
    2_000
}

const fn default_max_spaces() -> usize {
    DEFAULT_TASK_MAX_SPACES
}

const fn error_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvariantViolation => "invariant_violation",
        ErrorKind::Io => "io_error",
        ErrorKind::External => "external_error",
        ErrorKind::Unsupported => "unsupported",
        _ => "unknown_error",
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}
