//! Strict Codex hook payload/output adapter.
//!
//! Codex host versions are never gated; the reported version is an informational label and the
//! strict payload decoder below is what keeps the contract safe. Codex Hook Trust is explicit: an
//! unconfirmed state is reported as `ACTION REQUIRED` and disables all Hook capabilities while
//! MCP + CLI remain usable.
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

#[derive(Deserialize)]
struct Envelope {
    hook_event_name: String,
}

#[derive(Deserialize)]
struct Common {
    session_id: String,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
    hook_event_name: String,
    model: String,
    #[serde(default)]
    permission_mode: Option<String>,
}

impl Common {
    fn validate(&self, expected_event: &str) -> Result<()> {
        require_nonempty("session_id", &self.session_id)?;
        require_nonempty("model", &self.model)?;
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
/// fields, and undocumented enum values used by policy.
pub fn decode_hook_input(bytes: &[u8]) -> Result<CanonicalAgentEvent> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("invalid Codex hook JSON: {error}")))?;
    let envelope: Envelope = serde_json::from_value(value.clone())
        .map_err(|error| invalid(format!("invalid Codex hook envelope: {error}")))?;
    match envelope.hook_event_name.as_str() {
        "SessionStart" => {
            let input: SessionStartInput = decode(value, "SessionStart")?;
            input.common.validate("SessionStart")?;
            require_one_of(
                "Codex SessionStart source",
                &input.source,
                &["startup", "resume", "clear", "compact"],
            )?;
            Ok(CanonicalAgentEvent::SessionStart {
                context: input.common.context(),
            })
        }
        "UserPromptSubmit" => {
            let input: PromptInput = decode(value, "UserPromptSubmit")?;
            input.common.validate("UserPromptSubmit")?;
            require_nonempty("turn_id", &input.turn_id)?;
            require_nonempty("prompt", &input.prompt)?;
            Ok(CanonicalAgentEvent::PromptSubmit {
                context: input.common.context(),
                prompt: input.prompt,
            })
        }
        "PostToolUse" => {
            let input: PostToolInput = decode(value, "PostToolUse")?;
            input.common.validate("PostToolUse")?;
            require_nonempty("turn_id", &input.turn_id)?;
            require_nonempty("tool_name", &input.tool_name)?;
            require_nonempty("tool_use_id", &input.tool_use_id)?;
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
            let input: PreCompactInput = decode(value, "PreCompact")?;
            input.common.validate("PreCompact")?;
            require_nonempty("turn_id", &input.turn_id)?;
            require_one_of(
                "Codex PreCompact trigger",
                &input.trigger,
                &["manual", "auto"],
            )?;
            Ok(CanonicalAgentEvent::PreCompact {
                context: input.common.context(),
                trigger: input.trigger,
            })
        }
        "Stop" => {
            let input: StopInput = decode(value, "Stop")?;
            input.common.validate("Stop")?;
            require_nonempty("turn_id", &input.turn_id)?;
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
            let input: SessionEndInput = decode(value, "SessionEnd")?;
            input.common.validate("SessionEnd")?;
            require_one_of("Codex SessionEnd reason", &input.reason, &["other"])?;
            Ok(CanonicalAgentEvent::SessionEnd {
                context: input.common.context(),
                reason: input.reason,
            })
        }
        other => Err(invalid(format!("unsupported Codex Hook event {other:?}"))),
    }
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

fn decode<T: for<'de> Deserialize<'de>>(value: Value, event: &str) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|error| invalid(format!("invalid Codex {event} payload: {error}")))
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
