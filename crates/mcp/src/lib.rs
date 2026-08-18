//! Stdio Model Context Protocol server for the five Shared Context V1 tools.
//!
//! The transport accepts the newline-delimited framing used by current MCP
//! clients and the `Content-Length` framing used by older fixtures. Read tools
//! are pinned to one projection snapshot or one explicitly named Git tree;
//! the sole write tool delegates ID generation and append-only enforcement to
//! the domain event constructor and [`sctx_git_store::GitStore`].

use std::{
    collections::BTreeMap,
    fmt,
    io::{self, BufRead, Write},
    path::Path,
    str::FromStr,
};

use sctx_domain::{
    Applicability, ContextId, ContextKind, ContextRevisionDraft, Error, ErrorKind,
    EvidenceSnapshotDraft, EvidenceType, Result, RevisionId, SpaceId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendRequest, GitStore};
use sctx_index::{DomainSnapshot, ProjectionIndex};
use sctx_search::{
    ConflictView, ContextPackMode, ContextPackRequest, ContextStatus, ScopeFilter, SearchEngine,
    SearchFilters, SearchRequest,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Protocol version advertised when a client does not provide one.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
}

impl Runtime {
    fn open(root: &Path) -> Result<Self> {
        let store = GitStore::initialize(root)?;
        let index = ProjectionIndex::for_store(&store);
        Ok(Self { store, index })
    }

    fn snapshot(&self) -> Result<DomainSnapshot> {
        self.index.domain_snapshot()
    }
}

/// Stateful MCP request dispatcher for one stdio session.
pub struct McpServer {
    runtime: Runtime,
    client: ClientKind,
    initialized: bool,
}

impl McpServer {
    /// Opens the unique store and its rebuildable projection.
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
            "context_for_task" => self.context_for_task(call.arguments),
            "context_search" => self.context_search(call.arguments),
            "context_get" => self.context_get(call.arguments),
            "context_propose" => self.context_propose(call.arguments),
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

    fn context_for_task(&self, arguments: Value) -> ToolResult {
        let input: TaskInput = decode_arguments(arguments)?;
        let request = input.into_request()?;
        let response = SearchEngine::new(self.runtime.index.clone()).context_pack(&request)?;
        let conflicts = response
            .items
            .iter()
            .flat_map(|item| item.conflicts.iter().cloned())
            .collect::<Vec<_>>();
        let mut data = serde_json::to_value(response).map_err(serialization_failure)?;
        insert_fields(
            &mut data,
            [
                (
                    "conflicts",
                    serde_json::to_value(conflicts).map_err(serialization_failure)?,
                ),
                ("match_reason", json!("task_text_and_structured_hints")),
            ],
        )?;
        Ok(data)
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

    fn context_propose(&mut self, arguments: Value) -> ToolResult {
        let input: ProposeInput = decode_arguments(arguments)?;
        let space_id = parse_id::<SpaceId>(&input.space_id, "space_id")?;
        let before = self.runtime.snapshot()?;
        if !before.projection.spaces.contains_key(&space_id) {
            return Err(ToolFailure::from(invalid(format!(
                "space does not exist: {space_id}"
            ))));
        }
        let event = Event::context_proposed(space_id, input.into_draft(), None)?;
        let (context_id, revision_id) = match event.payload() {
            EventPayload::ContextRevisionAdded {
                context_id,
                revision,
                ..
            } => (*context_id, revision.revision_id),
            _ => unreachable!(),
        };
        let event_id = event.event_id();
        let append = self
            .runtime
            .store
            .append_event(AppendRequest::event(event))
            .map_err(ToolFailure::writer_rejected)?;
        let snapshot = self
            .runtime
            .snapshot()
            .map_err(ToolFailure::index_sync_failed)?;
        Ok(json!({
            "indexed_tree_oid": snapshot.metadata.indexed_tree_oid,
            "projection_generation": snapshot.metadata.projection_generation,
            "space_id": space_id,
            "context_id": context_id,
            "revision_id": revision_id,
            "event_id": event_id,
            "status": "candidate",
            "batch_id": append.batch_id,
            "commit_oid": append.commit_oid,
            "conflicts": context_conflicts(&snapshot, context_id),
            "match_reason": "new_candidate_created",
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
    preferred_space_id: Option<String>,
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
            preferred_space_id: self
                .preferred_space_id
                .as_deref()
                .map(|value| parse_id(value, "preferred_space_id"))
                .transpose()?,
            page_size: self.page_size,
            cursor: self.cursor,
        })
    }
}

type ToolResultSearch = std::result::Result<SearchRequest, ToolFailure>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInput {
    task: String,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    conditions: Vec<String>,
    #[serde(default)]
    kinds: Vec<ContextKind>,
    #[serde(default = "default_token_budget")]
    token_budget: usize,
    #[serde(default = "default_candidate_limit")]
    candidate_limit: usize,
}

impl TaskInput {
    fn into_request(self) -> std::result::Result<ContextPackRequest, ToolFailure> {
        if self.task.trim().is_empty() {
            return Err(invalid("task must not be empty").into());
        }
        let preferred_space_id = self
            .space_id
            .as_deref()
            .map(|value| parse_id(value, "space_id"))
            .transpose()?;
        Ok(ContextPackRequest {
            search: SearchRequest {
                query: self.task,
                filters: SearchFilters {
                    scope: ScopeFilter {
                        domains: self.domains,
                        platforms: self.platforms,
                        conditions: self.conditions,
                    },
                    kinds: self.kinds,
                    ..SearchFilters::default()
                },
                preferred_space_id,
                page_size: self.candidate_limit,
                ..SearchRequest::default()
            },
            token_budget: self.token_budget,
            candidate_limit: self.candidate_limit,
            mode: ContextPackMode::AutomaticInjection,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeInput {
    space_id: String,
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

impl ProposeInput {
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
            "context_for_task",
            "Build an automatic-injection Context Pack for a task from one projection snapshot.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["task"],
                "properties": {
                    "task": {"type": "string", "minLength": 1},
                    "space_id": id_schema("spc_"),
                    "domains": string_array_schema(),
                    "platforms": string_array_schema(),
                    "conditions": string_array_schema(),
                    "kinds": kind_array_schema(),
                    "token_budget": {"type": "integer", "minimum": 1, "default": 2000},
                    "candidate_limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 100}
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
            "context_propose",
            "Create one new Candidate Context through the append-only Writer. IDs, paths, and Publication are generated or governed internally.",
            propose_schema()
        ),
        tool_schema(
            "space_list",
            "List available ContextSpaces from a deterministic Git tree.",
            json!({"type": "object", "additionalProperties": false, "properties": {}})
        )
    ]})
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
            "preferred_space_id": id_schema("spc_"),
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

fn propose_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["space_id", "kind", "statement", "rationale", "evidence"],
        "properties": {
            "space_id": id_schema("spc_"),
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

const fn default_candidate_limit() -> usize {
    100
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
