//! Strict Codex hook payload/output adapter.
//!
//! Codex host versions are never gated; the reported version is an informational label and the
//! strict payload decoder below is what keeps the contract safe. Codex Hook Trust is explicit: an
//! unconfirmed state is reported as `ACTION REQUIRED` and disables all Hook capabilities while
//! MCP + CLI remain usable.
//!
//! SessionEnd follows the live #34 fingerprint: `model` may be absent and `reason` is any
//! nonempty string. The other five supported events still require a nonempty `model`; policy
//! enums such as SessionStart source and PreCompact trigger remain closed.
//!
//! The documented payload carries exactly one identity field, `session_id`, and Codex reports the
//! same `session_id` to a thread spawned from a parent conversation. The spawned thread's own
//! identity and its parent link exist only in the local rollout record, never in a Hook payload or
//! an MCP argument, and the `external_session_id` an Agent supplies is model-authored. Concurrent
//! Agents inside one Codex conversation therefore cannot be separated here; the Runtime separates
//! their Working Intent lineages instead (see `TaskRuntime::continue_working_intent`).

use std::path::PathBuf;

pub use sctx_agent_adapter::{
    AgentCapabilities, CanonicalAgentEvent, CanonicalAgentEventKind, ResolvedAgentAction,
    TrustState,
};
use sctx_agent_adapter::{
    AgentEventContext, AgentKind, ToolOutcome, evaluate_capabilities, normalize_tool_use,
};
pub use sctx_domain::{Error, ErrorKind, Result};
use serde::Deserialize;
use serde_json::{Value, json};

/// Checked-in fixture profile this payload contract was authored against. Informational only:
/// it never gates capabilities.
pub const FIXTURE_PROFILE_VERSION: &str = "0.147.0";

#[must_use]
pub fn capabilities(
    version: Option<&str>,
    hook_available: bool,
    trust: TrustState,
) -> AgentCapabilities {
    evaluate_capabilities(
        AgentKind::Codex,
        version,
        FIXTURE_PROFILE_VERSION,
        hook_available,
        trust,
        false,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookDecodeErrorClass {
    InvalidJson,
    MissingField,
    Type,
    Enum,
    UnknownEvent,
}

impl HookDecodeErrorClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::MissingField => "missing_field",
            Self::Type => "type",
            Self::Enum => "enum",
            Self::UnknownEvent => "unknown_event",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookHostSchema {
    InvalidJson,
    NonObject,
    MissingEventName,
    EventNameType,
    SupportedEvent,
    UnknownEvent,
}

impl HookHostSchema {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::NonObject => "non_object",
            Self::MissingEventName => "missing_event_name",
            Self::EventNameType => "event_name_type",
            Self::SupportedEvent => "supported_event",
            Self::UnknownEvent => "unknown_event",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookDecodeField {
    SessionId,
    TranscriptPath,
    Cwd,
    HookEventName,
    Model,
    PermissionMode,
    Source,
    TurnId,
    Prompt,
    ToolName,
    ToolUseId,
    ToolInput,
    ToolResponse,
    Trigger,
    StopHookActive,
    LastAssistantMessage,
    Reason,
}

impl HookDecodeField {
    #[must_use]
    pub const fn telemetry_family(self) -> &'static str {
        match self {
            Self::SessionId => "field.session_id",
            Self::TranscriptPath => "field.transcript_path",
            Self::Cwd => "field.cwd",
            Self::HookEventName => "field.hook_event_name",
            Self::Model => "field.model",
            Self::PermissionMode => "field.permission_mode",
            Self::Source => "field.source",
            Self::TurnId => "field.turn_id",
            Self::Prompt => "field.prompt",
            Self::ToolName => "field.tool_name",
            Self::ToolUseId => "field.tool_use_id",
            Self::ToolInput => "field.tool_input",
            Self::ToolResponse => "field.tool_response",
            Self::Trigger => "field.trigger",
            Self::StopHookActive => "field.stop_hook_active",
            Self::LastAssistantMessage => "field.last_assistant_message",
            Self::Reason => "field.reason",
        }
    }
}

/// Closed, privacy-safe metadata for one rejected host payload.
///
/// The optional Session id is read only from the documented top-level field and is bounded before
/// it leaves the adapter. Callers may hash it, but must never persist it directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookDecodeDiagnostic {
    pub error_class: HookDecodeErrorClass,
    pub event_kind: Option<CanonicalAgentEventKind>,
    pub host_schema: HookHostSchema,
    pub field: Option<HookDecodeField>,
    pub session_id: Option<String>,
}

#[derive(Debug)]
pub struct HookDecodeFailure {
    error: Error,
    diagnostic: HookDecodeDiagnostic,
}

impl HookDecodeFailure {
    #[must_use]
    pub const fn diagnostic(&self) -> &HookDecodeDiagnostic {
        &self.diagnostic
    }

    #[must_use]
    pub fn error(&self) -> &Error {
        &self.error
    }

    #[must_use]
    pub fn into_error(self) -> Error {
        self.error
    }
}

#[derive(Deserialize)]
struct Common {
    session_id: String,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
    hook_event_name: String,
    model: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
}

impl Common {
    fn validate(&self, expected_event: &str) -> Result<()> {
        require_nonempty("session_id", &self.session_id)?;
        if self.cwd.as_os_str().is_empty() {
            return Err(invalid("Codex cwd must not be empty"));
        }
        if self.hook_event_name != expected_event {
            return Err(invalid(format!(
                "Codex payload event mismatch: expected {expected_event}, got {}",
                self.hook_event_name
            )));
        }
        let _ = (&self.transcript_path, &self.permission_mode);
        Ok(())
    }

    fn validate_with_model(&self, expected_event: &str) -> Result<()> {
        self.validate(expected_event)?;
        require_nonempty("model", self.model.as_deref().unwrap_or_default())
    }

    fn context(&self) -> AgentEventContext {
        AgentEventContext {
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            workspace_roots: vec![self.cwd.clone()],
        }
    }
}

#[derive(Deserialize)]
struct SessionStartInput {
    #[serde(flatten)]
    common: Common,
    source: String,
}

#[derive(Deserialize)]
struct PromptInput {
    #[serde(flatten)]
    common: Common,
    turn_id: String,
    prompt: String,
}

#[derive(Deserialize)]
struct PostToolInput {
    #[serde(flatten)]
    common: Common,
    turn_id: String,
    tool_name: String,
    tool_use_id: String,
    tool_input: Value,
    tool_response: Value,
}

#[derive(Deserialize)]
struct PreCompactInput {
    #[serde(flatten)]
    common: Common,
    turn_id: String,
    trigger: String,
}

#[derive(Deserialize)]
struct StopInput {
    #[serde(flatten)]
    common: Common,
    turn_id: String,
    stop_hook_active: bool,
    last_assistant_message: Option<String>,
}

#[derive(Deserialize)]
struct SessionEndInput {
    #[serde(flatten)]
    common: Common,
    reason: String,
}

/// Strictly translate one documented Codex JSON object into a canonical event.
///
/// # Errors
///
/// Rejects malformed JSON, unsupported Hook names, wrong field types, empty required identity
/// fields, and undocumented enum values used by policy. SessionEnd accepts a missing/null model
/// and any nonempty reason, matching the live #34 fingerprint; a supplied model must be nonempty.
pub fn decode_hook_input(bytes: &[u8]) -> Result<CanonicalAgentEvent> {
    decode_hook_input_with_diagnostic(bytes).map_err(HookDecodeFailure::into_error)
}

/// Decode a Codex payload while retaining only closed, bounded failure metadata.
///
/// # Errors
///
/// Returns the same strict adapter error as [`decode_hook_input`] plus a diagnostic which never
/// contains Prompt text, tool values, paths, arbitrary keys, arbitrary enum values, or raw errors.
#[allow(clippy::too_many_lines)]
pub fn decode_hook_input_with_diagnostic(
    bytes: &[u8],
) -> std::result::Result<CanonicalAgentEvent, HookDecodeFailure> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| HookDecodeFailure {
        error: invalid(format!("invalid Codex hook JSON: {error}")),
        diagnostic: HookDecodeDiagnostic {
            error_class: HookDecodeErrorClass::InvalidJson,
            event_kind: None,
            host_schema: HookHostSchema::InvalidJson,
            field: None,
            session_id: None,
        },
    })?;
    let Some(object) = value.as_object() else {
        return Err(decode_failure(
            HookDecodeDiagnostic {
                error_class: HookDecodeErrorClass::Type,
                event_kind: None,
                host_schema: HookHostSchema::NonObject,
                field: None,
                session_id: None,
            },
            invalid("Codex hook payload must be an object"),
        ));
    };
    let session_id = object
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty() && session.len() <= 256)
        .map(str::to_owned);
    let Some(event_name) = object.get("hook_event_name") else {
        return Err(decode_failure(
            HookDecodeDiagnostic {
                error_class: HookDecodeErrorClass::MissingField,
                event_kind: None,
                host_schema: HookHostSchema::MissingEventName,
                field: Some(HookDecodeField::HookEventName),
                session_id,
            },
            invalid("invalid Codex hook envelope: missing hook_event_name"),
        ));
    };
    let Some(event_name) = event_name.as_str() else {
        return Err(decode_failure(
            HookDecodeDiagnostic {
                error_class: HookDecodeErrorClass::Type,
                event_kind: None,
                host_schema: HookHostSchema::EventNameType,
                field: Some(HookDecodeField::HookEventName),
                session_id,
            },
            invalid("invalid Codex hook envelope: hook_event_name must be a string"),
        ));
    };
    let event_kind = codex_event_kind(event_name);
    let diagnostic = HookDecodeDiagnostic {
        error_class: HookDecodeErrorClass::Type,
        event_kind,
        host_schema: if event_kind.is_some() {
            HookHostSchema::SupportedEvent
        } else {
            HookHostSchema::UnknownEvent
        },
        field: None,
        session_id,
    };
    validate_supported_shape(object, event_name, &diagnostic)?;
    match event_name {
        "SessionStart" => {
            let input: SessionStartInput = decode(value, "SessionStart", &diagnostic)?;
            input
                .common
                .validate_with_model("SessionStart")
                .map_err(|error| {
                    classified_failure(&diagnostic, HookDecodeErrorClass::MissingField, error)
                })?;
            require_one_of(
                "Codex SessionStart source",
                &input.source,
                &["startup", "resume", "clear", "compact"],
            )
            .map_err(|error| {
                classified_field_failure(
                    &diagnostic,
                    HookDecodeErrorClass::Enum,
                    HookDecodeField::Source,
                    error,
                )
            })?;
            Ok(CanonicalAgentEvent::SessionStart {
                context: input.common.context(),
            })
        }
        "UserPromptSubmit" => {
            let input: PromptInput = decode(value, "UserPromptSubmit", &diagnostic)?;
            validate_common_and_required(
                &diagnostic,
                input.common.validate_with_model("UserPromptSubmit"),
            )?;
            validate_required(&diagnostic, require_nonempty("turn_id", &input.turn_id))?;
            validate_required(&diagnostic, require_nonempty("prompt", &input.prompt))?;
            Ok(CanonicalAgentEvent::PromptSubmit {
                context: input.common.context(),
                prompt: input.prompt,
            })
        }
        "PostToolUse" => {
            let input: PostToolInput = decode(value, "PostToolUse", &diagnostic)?;
            validate_common_and_required(
                &diagnostic,
                input.common.validate_with_model("PostToolUse"),
            )?;
            validate_required(&diagnostic, require_nonempty("turn_id", &input.turn_id))?;
            validate_required(&diagnostic, require_nonempty("tool_name", &input.tool_name))?;
            validate_required(
                &diagnostic,
                require_nonempty("tool_use_id", &input.tool_use_id),
            )?;
            let outcome = tool_outcome(&input.tool_response);
            let normalized = normalize_tool_use(&input.tool_name, &input.tool_input);
            Ok(CanonicalAgentEvent::PostToolUse {
                context: input.common.context(),
                tool_category: normalized.category,
                tool_use_id: input.tool_use_id,
                path_hints: normalized.path_hints,
                file_access: normalized.file_access,
                outcome,
            })
        }
        "PreCompact" => {
            let input: PreCompactInput = decode(value, "PreCompact", &diagnostic)?;
            validate_common_and_required(
                &diagnostic,
                input.common.validate_with_model("PreCompact"),
            )?;
            validate_required(&diagnostic, require_nonempty("turn_id", &input.turn_id))?;
            require_one_of(
                "Codex PreCompact trigger",
                &input.trigger,
                &["manual", "auto"],
            )
            .map_err(|error| {
                classified_field_failure(
                    &diagnostic,
                    HookDecodeErrorClass::Enum,
                    HookDecodeField::Trigger,
                    error,
                )
            })?;
            Ok(CanonicalAgentEvent::PreCompact {
                context: input.common.context(),
                trigger: input.trigger,
            })
        }
        "Stop" => {
            let input: StopInput = decode(value, "Stop", &diagnostic)?;
            validate_common_and_required(&diagnostic, input.common.validate_with_model("Stop"))?;
            validate_required(&diagnostic, require_nonempty("turn_id", &input.turn_id))?;
            let _ = input.last_assistant_message;
            Ok(CanonicalAgentEvent::TurnStop {
                context: input.common.context(),
                status: if input.stop_hook_active {
                    "continued"
                } else {
                    "completed"
                }
                .to_owned(),
            })
        }
        "SessionEnd" => {
            let input: SessionEndInput = decode(value, "SessionEnd", &diagnostic)?;
            validate_common_and_required(&diagnostic, input.common.validate("SessionEnd"))?;
            // The canonical event only carries this descriptive host value through, and Session
            // cleanup never branches on it. Retain the non-empty boundary instead of maintaining
            // a guessed enum whitelist which can suppress cleanup for new host reasons.
            validate_required(
                &diagnostic,
                require_nonempty("SessionEnd reason", &input.reason),
            )?;
            Ok(CanonicalAgentEvent::SessionEnd {
                context: input.common.context(),
                reason: input.reason,
            })
        }
        other => Err(classified_failure(
            &diagnostic,
            HookDecodeErrorClass::UnknownEvent,
            invalid(format!("unsupported Codex Hook event {other:?}")),
        )),
    }
}

const fn codex_event_kind(event_name: &str) -> Option<CanonicalAgentEventKind> {
    match event_name.as_bytes() {
        b"SessionStart" => Some(CanonicalAgentEventKind::SessionStart),
        b"UserPromptSubmit" => Some(CanonicalAgentEventKind::PromptSubmit),
        b"PostToolUse" => Some(CanonicalAgentEventKind::PostToolUse),
        b"PreCompact" => Some(CanonicalAgentEventKind::PreCompact),
        b"Stop" => Some(CanonicalAgentEventKind::TurnStop),
        b"SessionEnd" => Some(CanonicalAgentEventKind::SessionEnd),
        _ => None,
    }
}

fn decode_failure(diagnostic: HookDecodeDiagnostic, error: Error) -> HookDecodeFailure {
    HookDecodeFailure { error, diagnostic }
}

fn classified_failure(
    diagnostic: &HookDecodeDiagnostic,
    error_class: HookDecodeErrorClass,
    error: Error,
) -> HookDecodeFailure {
    let mut diagnostic = diagnostic.clone();
    diagnostic.error_class = error_class;
    decode_failure(diagnostic, error)
}

fn classified_field_failure(
    diagnostic: &HookDecodeDiagnostic,
    error_class: HookDecodeErrorClass,
    field: HookDecodeField,
    error: Error,
) -> HookDecodeFailure {
    let mut diagnostic = diagnostic.clone();
    diagnostic.error_class = error_class;
    diagnostic.field = Some(field);
    decode_failure(diagnostic, error)
}

fn validate_supported_shape(
    object: &serde_json::Map<String, Value>,
    event_name: &str,
    diagnostic: &HookDecodeDiagnostic,
) -> std::result::Result<(), HookDecodeFailure> {
    if diagnostic.event_kind.is_none() {
        return Ok(());
    }
    for (name, field) in [
        ("session_id", HookDecodeField::SessionId),
        ("cwd", HookDecodeField::Cwd),
        ("hook_event_name", HookDecodeField::HookEventName),
    ] {
        require_shape_string(object, name, field, diagnostic)?;
    }
    if event_name != "SessionEnd" || object.get("model").is_some_and(|value| !value.is_null()) {
        require_shape_string(object, "model", HookDecodeField::Model, diagnostic)?;
    }
    for (name, field) in [
        ("transcript_path", HookDecodeField::TranscriptPath),
        ("permission_mode", HookDecodeField::PermissionMode),
    ] {
        require_optional_shape_string(object, name, field, diagnostic)?;
    }
    match event_name {
        "SessionStart" => {
            require_shape_string(object, "source", HookDecodeField::Source, diagnostic)?;
        }
        "UserPromptSubmit" => {
            require_shape_string(object, "turn_id", HookDecodeField::TurnId, diagnostic)?;
            require_shape_string(object, "prompt", HookDecodeField::Prompt, diagnostic)?;
        }
        "PostToolUse" => {
            require_shape_string(object, "turn_id", HookDecodeField::TurnId, diagnostic)?;
            require_shape_string(object, "tool_name", HookDecodeField::ToolName, diagnostic)?;
            require_shape_string(
                object,
                "tool_use_id",
                HookDecodeField::ToolUseId,
                diagnostic,
            )?;
            require_shape_value(object, "tool_input", HookDecodeField::ToolInput, diagnostic)?;
            require_shape_value(
                object,
                "tool_response",
                HookDecodeField::ToolResponse,
                diagnostic,
            )?;
        }
        "PreCompact" => {
            require_shape_string(object, "turn_id", HookDecodeField::TurnId, diagnostic)?;
            require_shape_string(object, "trigger", HookDecodeField::Trigger, diagnostic)?;
        }
        "Stop" => {
            require_shape_string(object, "turn_id", HookDecodeField::TurnId, diagnostic)?;
            require_shape_bool(
                object,
                "stop_hook_active",
                HookDecodeField::StopHookActive,
                diagnostic,
            )?;
            require_optional_shape_string(
                object,
                "last_assistant_message",
                HookDecodeField::LastAssistantMessage,
                diagnostic,
            )?;
        }
        "SessionEnd" => {
            require_shape_string(object, "reason", HookDecodeField::Reason, diagnostic)?;
        }
        _ => {}
    }
    Ok(())
}

fn require_shape_string(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
    field: HookDecodeField,
    diagnostic: &HookDecodeDiagnostic,
) -> std::result::Result<(), HookDecodeFailure> {
    let Some(value) = object.get(name) else {
        return Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::MissingField,
            field,
            invalid(format!("invalid Codex payload: missing {name}")),
        ));
    };
    let Some(value) = value.as_str() else {
        return Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::Type,
            field,
            invalid(format!("invalid Codex payload: {name} must be a string")),
        ));
    };
    if value.trim().is_empty() {
        return Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::MissingField,
            field,
            invalid(format!("invalid Codex payload: {name} must not be empty")),
        ));
    }
    Ok(())
}

fn require_optional_shape_string(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
    field: HookDecodeField,
    diagnostic: &HookDecodeDiagnostic,
) -> std::result::Result<(), HookDecodeFailure> {
    if object
        .get(name)
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::Type,
            field,
            invalid(format!(
                "invalid Codex payload: {name} must be a string or null"
            )),
        ));
    }
    Ok(())
}

fn require_shape_bool(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
    field: HookDecodeField,
    diagnostic: &HookDecodeDiagnostic,
) -> std::result::Result<(), HookDecodeFailure> {
    match object.get(name) {
        None => Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::MissingField,
            field,
            invalid(format!("invalid Codex payload: missing {name}")),
        )),
        Some(value) if !value.is_boolean() => Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::Type,
            field,
            invalid(format!("invalid Codex payload: {name} must be a boolean")),
        )),
        Some(_) => Ok(()),
    }
}

fn require_shape_value(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
    field: HookDecodeField,
    diagnostic: &HookDecodeDiagnostic,
) -> std::result::Result<(), HookDecodeFailure> {
    if object.contains_key(name) {
        Ok(())
    } else {
        Err(classified_field_failure(
            diagnostic,
            HookDecodeErrorClass::MissingField,
            field,
            invalid(format!("invalid Codex payload: missing {name}")),
        ))
    }
}

fn validate_common_and_required(
    diagnostic: &HookDecodeDiagnostic,
    result: Result<()>,
) -> std::result::Result<(), HookDecodeFailure> {
    validate_required(diagnostic, result)
}

fn validate_required(
    diagnostic: &HookDecodeDiagnostic,
    result: Result<()>,
) -> std::result::Result<(), HookDecodeFailure> {
    result
        .map_err(|error| classified_failure(diagnostic, HookDecodeErrorClass::MissingField, error))
}

/// Encode only documented Codex output fields. The adapter never returns a decision, continuation,
/// updated input, or any other control field.
///
/// # Errors
///
/// Serialization errors are returned as typed adapter errors.
pub fn encode_hook_output(
    event: CanonicalAgentEventKind,
    action: &ResolvedAgentAction,
) -> Result<Vec<u8>> {
    let mut object = serde_json::Map::new();
    if let Some(message) = &action.system_message {
        object.insert("systemMessage".to_owned(), Value::String(message.clone()));
    }
    // `systemMessage` is a user-visible line by Hook-protocol convention, so a reminder
    // delivered only there may never enter the model's context. A message that carries no
    // additional context of its own is therefore written to both fields — but only on the
    // events whose output object can hold the second one at all.
    if let Some(hook_event_name) = hook_specific_output_event_name(event) {
        if let Some(context) = action
            .additional_context
            .as_deref()
            .or(action.system_message.as_deref())
        {
            object.insert(
                "hookSpecificOutput".to_owned(),
                json!({
                    "hookEventName": hook_event_name,
                    "additionalContext": context,
                }),
            );
        }
    }
    serde_json::to_vec(&Value::Object(object))
        .map_err(|error| Error::new(ErrorKind::Io, format!("encode Codex hook output: {error}")))
}

/// The Codex Hook name to tag a `hookSpecificOutput` block with, or `None` when this event's
/// output object cannot carry one.
///
/// Codex 0.153.4 deserializes `hookSpecificOutput` as an internally tagged enum with exactly six
/// variants — `PreToolUse`, `PostToolUse`, `PermissionRequest`, `SessionStart`, `SubagentStart`,
/// `UserPromptSubmit`. There is no `Stop`, `PreCompact`, or `SessionEnd` variant, so putting the
/// block on one of those events does not merely drop the extra context: the whole output object
/// fails to deserialize and every field goes with it, including the `systemMessage` the user was
/// meant to read. A real Codex session showed exactly that — `hook returned invalid stop hook JSON
/// output` on every turn, and the checkpoint reminder never arrived. The base output shape for
/// those three events does accept `systemMessage`, so `{"systemMessage": …}` alone is the whole
/// contract they can honor here.
///
/// Compaction therefore has no `PreCompact` channel into model context, and does not need one:
/// Codex re-sends `SessionStart` with `source: "compact"` after it compacts, and `SessionStart`
/// renders the activation marker unconditionally. That is the marker's post-compaction path.
///
/// A `Stop` has no such fallback. The only field Codex reads there that the model would see is
/// `decision: "block"` with a `reason`, and blocking a turn to make the Agent read a reminder is a
/// control decision this adapter does not make: it returns no decision, continuation, or updated
/// input on any event. The checkpoint request stays a user-visible `systemMessage`.
const fn hook_specific_output_event_name(event: CanonicalAgentEventKind) -> Option<&'static str> {
    match event {
        CanonicalAgentEventKind::SessionStart => Some("SessionStart"),
        CanonicalAgentEventKind::PromptSubmit => Some("UserPromptSubmit"),
        CanonicalAgentEventKind::PostToolUse => Some("PostToolUse"),
        CanonicalAgentEventKind::PreCompact
        | CanonicalAgentEventKind::TurnStop
        | CanonicalAgentEventKind::SessionEnd => None,
    }
}

/// Whether text this adapter encodes for `event` can reach the model reading the Session.
///
/// This is [`hook_specific_output_event_name`] read from the caller's side, and it exists so a
/// Runtime decision that depends on model visibility — whether a one-shot delivery like the
/// self-healed activation marker is worth spending on this event — asks the adapter that owns
/// the wire instead of restating its rules. It is derived rather than written out, so the two
/// cannot drift: an event Codex has no `hookSpecificOutput` variant for delivers nothing to the
/// model no matter what the caller puts in `additional_context`.
#[must_use]
pub const fn delivers_model_visible_context(event: CanonicalAgentEventKind) -> bool {
    hook_specific_output_event_name(event).is_some()
}

fn tool_outcome(response: &Value) -> ToolOutcome {
    if response
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || response.get("error").is_some_and(|error| !error.is_null())
    {
        ToolOutcome::Failed
    } else {
        ToolOutcome::Succeeded
    }
}

fn decode<T: for<'de> Deserialize<'de>>(
    value: Value,
    event: &str,
    diagnostic: &HookDecodeDiagnostic,
) -> std::result::Result<T, HookDecodeFailure> {
    serde_json::from_value(value).map_err(|error| {
        let error_class = if error.to_string().starts_with("missing field") {
            HookDecodeErrorClass::MissingField
        } else {
            HookDecodeErrorClass::Type
        };
        classified_failure(
            diagnostic,
            error_class,
            invalid(format!("invalid Codex {event} payload: {error}")),
        )
    })
}

fn require_nonempty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(invalid(format!("Codex {name} must not be empty")))
    } else {
        Ok(())
    }
}

fn require_one_of(name: &str, value: &str, accepted: &[&str]) -> Result<()> {
    if accepted.contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!("{name} has unsupported value {value:?}")))
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
