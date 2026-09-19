//! Strict Cursor hook payload/output adapter.
//!
//! Cursor host versions are never gated. The host reports incompatible shapes across surfaces
//! (`cursor-agent --version` is date-like, e.g. `2026.08.25-3e8eec8`, while the desktop shim
//! reports semver), so the version is carried through as an informational label only; the strict
//! payload decoder below is what keeps the contract safe. Hook actions are disabled only when the
//! host provides no Hook. `beforeSubmitPrompt` is translated for completeness but never drives
//! Context Pack injection.
//!
//! The decoder accepts both documented host shapes: the cursor-agent CLI (fixture profile
//! `3.13.0`) and the desktop IDE (observed `3.17.21`), which sends an empty `generation_id`
//! on session-level events, a floating-point `postToolUse` `duration`, an empty or missing
//! `postToolUse` `cwd`, and undocumented lifecycle enum values. Identity fields the runtime
//! actually relies on (`conversation_id`, `workspace_roots`) stay strictly validated.

use std::path::PathBuf;

pub use sctx_agent_adapter::{
    AgentCapabilities, CanonicalAgentEvent, CanonicalAgentEventKind, ResolvedAgentAction,
};
use sctx_agent_adapter::{
    AgentEventContext, AgentKind, ToolOutcome, TrustState, evaluate_capabilities,
    normalize_tool_use,
};
pub use sctx_domain::{Error, ErrorKind, Result};
use serde::Deserialize;
use serde_json::{Value, json};

/// Checked-in fixture profile this payload contract was authored against. Informational only:
/// it never gates capabilities.
pub const FIXTURE_PROFILE_VERSION: &str = "3.13.0";

#[must_use]
pub fn capabilities(version: Option<&str>, hook_available: bool) -> AgentCapabilities {
    evaluate_capabilities(
        AgentKind::Cursor,
        version,
        FIXTURE_PROFILE_VERSION,
        hook_available,
        TrustState::NotRequired,
        true,
    )
}

#[derive(Deserialize)]
struct Envelope {
    hook_event_name: String,
    cursor_version: String,
}

#[derive(Deserialize)]
struct Common {
    conversation_id: String,
    /// Informational only: the desktop IDE sends an empty string on session-level events.
    #[allow(dead_code)]
    generation_id: String,
    model: String,
    #[allow(dead_code)]
    #[serde(default)]
    model_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    model_params: Vec<Value>,
    hook_event_name: String,
    cursor_version: String,
    workspace_roots: Vec<PathBuf>,
    #[allow(dead_code)]
    user_email: Option<String>,
    #[allow(dead_code)]
    transcript_path: Option<PathBuf>,
}

impl Common {
    fn validate(&self, expected_event: &str) -> Result<()> {
        for (name, value) in [
            ("conversation_id", self.conversation_id.as_str()),
            ("model", self.model.as_str()),
            ("cursor_version", self.cursor_version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(invalid(format!("Cursor {name} must not be empty")));
            }
        }
        if self.hook_event_name != expected_event {
            return Err(invalid(format!(
                "Cursor payload event mismatch: expected {expected_event}, got {}",
                self.hook_event_name
            )));
        }
        if self.workspace_roots.is_empty() {
            return Err(invalid("Cursor workspace_roots must not be empty"));
        }
        Ok(())
    }

    fn context(&self, session_id: Option<&str>, cwd: Option<&str>) -> AgentEventContext {
        AgentEventContext {
            session_id: session_id.unwrap_or(&self.conversation_id).to_owned(),
            cwd: cwd.map_or_else(|| self.workspace_roots[0].clone(), PathBuf::from),
            workspace_roots: self.workspace_roots.clone(),
        }
    }
}

#[derive(Deserialize)]
struct SessionStartInput {
    #[serde(flatten)]
    common: Common,
    session_id: String,
    is_background_agent: bool,
    #[serde(default)]
    composer_mode: Option<String>,
}

#[derive(Deserialize)]
struct PromptInput {
    #[serde(flatten)]
    common: Common,
    prompt: String,
    #[allow(dead_code)]
    #[serde(default)]
    attachments: Vec<Value>,
}

#[derive(Deserialize)]
struct PostToolInput {
    #[serde(flatten)]
    common: Common,
    tool_name: String,
    tool_input: Value,
    /// Read only for a structured success/failure marker; never retained.
    tool_output: String,
    tool_use_id: String,
    /// The desktop IDE sends an empty string or omits the field entirely.
    #[serde(default)]
    cwd: Option<String>,
    /// The desktop IDE sends fractional milliseconds; serde's `f64` also accepts integers.
    #[allow(dead_code)]
    duration: f64,
}

#[derive(Deserialize)]
struct PreCompactInput {
    #[serde(flatten)]
    common: Common,
    trigger: String,
    #[allow(dead_code)]
    #[serde(default)]
    context_usage_percent: f64,
    #[allow(dead_code)]
    #[serde(default)]
    context_tokens: u64,
    #[allow(dead_code)]
    #[serde(default)]
    context_window_size: u64,
    #[allow(dead_code)]
    #[serde(default)]
    message_count: u64,
    #[allow(dead_code)]
    #[serde(default)]
    messages_to_compact: u64,
    #[allow(dead_code)]
    #[serde(default)]
    is_first_compaction: bool,
}

#[derive(Deserialize)]
struct StopInput {
    #[serde(flatten)]
    common: Common,
    status: String,
    loop_count: u64,
}

#[derive(Deserialize)]
struct SessionEndInput {
    #[serde(flatten)]
    common: Common,
    session_id: String,
    reason: String,
    /// The desktop IDE may send fractional milliseconds; serde's `f64` also accepts integers.
    #[allow(dead_code)]
    duration_ms: f64,
    #[allow(dead_code)]
    is_background_agent: bool,
    #[allow(dead_code)]
    final_status: String,
    #[allow(dead_code)]
    #[serde(default)]
    error_message: Option<String>,
}

/// Strictly translate one documented Cursor JSON object into a canonical event.
///
/// # Errors
///
/// Rejects malformed JSON, unsupported Hook names, wrong field types, and empty required
/// identity fields. Lifecycle enum values (`trigger`, `status`, `reason`) pass through
/// verbatim: no downstream policy branches on them, so an undocumented value must not
/// block the host session.
pub fn decode_hook_input(bytes: &[u8]) -> Result<(CanonicalAgentEvent, String)> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("invalid Cursor hook JSON: {error}")))?;
    let envelope: Envelope = serde_json::from_value(value.clone())
        .map_err(|error| invalid(format!("invalid Cursor hook envelope: {error}")))?;
    let version = envelope.cursor_version.clone();
    let event = match envelope.hook_event_name.as_str() {
        "sessionStart" => {
            let input: SessionStartInput = decode(value, "sessionStart")?;
            input.common.validate("sessionStart")?;
            if input.session_id != input.common.conversation_id {
                return Err(invalid(
                    "Cursor sessionStart session_id must equal conversation_id",
                ));
            }
            let _ = (input.is_background_agent, input.composer_mode);
            CanonicalAgentEvent::SessionStart {
                context: input.common.context(Some(&input.session_id), None),
            }
        }
        "beforeSubmitPrompt" => {
            let input: PromptInput = decode(value, "beforeSubmitPrompt")?;
            input.common.validate("beforeSubmitPrompt")?;
            if input.prompt.trim().is_empty() {
                return Err(invalid("Cursor prompt must not be empty"));
            }
            CanonicalAgentEvent::PromptSubmit {
                context: input.common.context(None, None),
                prompt: input.prompt,
            }
        }
        "postToolUse" => {
            let input: PostToolInput = decode(value, "postToolUse")?;
            input.common.validate("postToolUse")?;
            require_nonempty("tool_name", &input.tool_name)?;
            require_nonempty("tool_use_id", &input.tool_use_id)?;
            let cwd = input
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty());
            let normalized = normalize_tool_use(&input.tool_name, &input.tool_input);
            CanonicalAgentEvent::PostToolUse {
                context: input.common.context(None, cwd),
                tool_category: normalized.category,
                tool_use_id: input.tool_use_id,
                path_hints: normalized.path_hints,
                file_access: normalized.file_access,
                outcome: tool_outcome(&input.tool_output),
            }
        }
        "preCompact" => {
            let input: PreCompactInput = decode(value, "preCompact")?;
            input.common.validate("preCompact")?;
            require_nonempty("trigger", &input.trigger)?;
            CanonicalAgentEvent::PreCompact {
                context: input.common.context(None, None),
                trigger: input.trigger,
            }
        }
        "stop" => {
            let input: StopInput = decode(value, "stop")?;
            input.common.validate("stop")?;
            require_nonempty("status", &input.status)?;
            let _ = input.loop_count;
            CanonicalAgentEvent::TurnStop {
                context: input.common.context(None, None),
                status: input.status,
            }
        }
        "sessionEnd" => {
            let input: SessionEndInput = decode(value, "sessionEnd")?;
            input.common.validate("sessionEnd")?;
            require_nonempty("reason", &input.reason)?;
            CanonicalAgentEvent::SessionEnd {
                context: input.common.context(Some(&input.session_id), None),
                reason: input.reason,
            }
        }
        other => return Err(invalid(format!("unsupported Cursor Hook event {other:?}"))),
    };
    Ok((event, version))
}

/// Encode only fields supported by the concrete Cursor event. No output path can modify a prompt,
/// tool input, tool output, or request a follow-up turn.
///
/// # Errors
///
/// Serialization errors are returned as typed adapter errors.
pub fn encode_hook_output(
    event: CanonicalAgentEventKind,
    action: &ResolvedAgentAction,
) -> Result<Vec<u8>> {
    let value = match event {
        CanonicalAgentEventKind::SessionStart => action
            .additional_context
            .as_ref()
            .or(action.system_message.as_ref())
            .map_or_else(
                || json!({}),
                |context| json!({"additional_context": context}),
            ),
        CanonicalAgentEventKind::PostToolUse => action
            .additional_context
            .as_ref()
            .or(action.system_message.as_ref())
            .map_or_else(
                || json!({}),
                |context| json!({"additional_context": context}),
            ),
        // Cursor registers both `preCompact` and `stop`, and both accept `user_message`.
        // A `stop` that dropped the runtime's checkpoint line silently discarded the only
        // Episode boundary notice the host can render, so the two share one encoding. The
        // re-stated activation marker a `preCompact` carries follows the message on its own
        // line, because `user_message` is the single text field these events accept.
        CanonicalAgentEventKind::PreCompact | CanonicalAgentEventKind::TurnStop => {
            match (
                action.system_message.as_deref(),
                action.additional_context.as_deref(),
            ) {
                (Some(message), Some(marker)) => {
                    json!({"user_message": format!("{message}\n{marker}")})
                }
                (Some(text), None) | (None, Some(text)) => json!({"user_message": text}),
                (None, None) => json!({}),
            }
        }
        CanonicalAgentEventKind::PromptSubmit | CanonicalAgentEventKind::SessionEnd => json!({}),
    };
    serde_json::to_vec(&value)
        .map_err(|error| Error::new(ErrorKind::Io, format!("encode Cursor hook output: {error}")))
}

/// Whether text this adapter encodes for `event` can reach the model reading the Session.
///
/// The mirror of [`encode_hook_output`]'s match, stated as one predicate so a Runtime decision
/// that depends on model visibility — whether a one-shot delivery like the self-healed activation
/// marker is worth spending on this event — asks the adapter that owns the wire instead of
/// restating its rules. Cursor's `sessionStart` and `postToolUse` carry `additional_context`, and
/// its `preCompact` and `stop` carry `user_message`; a `beforeSubmitPrompt` and a `sessionEnd`
/// encode an empty object, so anything written for them is dropped. `payload_contract` holds the
/// test that keeps this in step with the encoder.
#[must_use]
pub const fn delivers_model_visible_context(event: CanonicalAgentEventKind) -> bool {
    matches!(
        event,
        CanonicalAgentEventKind::SessionStart
            | CanonicalAgentEventKind::PostToolUse
            | CanonicalAgentEventKind::PreCompact
            | CanonicalAgentEventKind::TurnStop
    )
}

/// Derives the one structured success/failure marker a Cursor `postToolUse` carries.
///
/// Cursor sends `tool_output` as a JSON *string* whose content is the tool's own JSON result
/// object — `{"exitCode":0,"stdout":"..."}` for shell tools, `{"contents":"..."}` for reads.
/// Only the small set of explicit failure fields below is read; the payload is otherwise
/// discarded and never crosses the adapter seam. Anything undecodable, non-object, or without
/// one of those fields carries no failure information and stays `Succeeded`, which is exactly
/// the value this adapter hard-coded before.
fn tool_outcome(tool_output: &str) -> ToolOutcome {
    let Ok(Value::Object(result)) = serde_json::from_str::<Value>(tool_output) else {
        return ToolOutcome::Succeeded;
    };
    let failed = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || result.get("success").and_then(Value::as_bool) == Some(false)
        || result.get("error").is_some_and(is_present_error)
        || ["exitCode", "exit_code"]
            .iter()
            .filter_map(|key| result.get(*key))
            .any(|code| code.as_i64().is_some_and(|code| code != 0));
    if failed {
        ToolOutcome::Failed
    } else {
        ToolOutcome::Succeeded
    }
}

/// An `error` field states a failure only when it actually carries one. Hosts routinely send
/// `null`, `""`, or `{}` on success, and none of those is evidence that the tool failed.
fn is_present_error(error: &Value) -> bool {
    match error {
        Value::Null => false,
        Value::String(message) => !message.trim().is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Bool(flag) => *flag,
        Value::Number(_) => true,
    }
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value, event: &str) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|error| invalid(format!("invalid Cursor {event} payload: {error}")))
}

fn require_nonempty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(invalid(format!("Cursor {name} must not be empty")))
    } else {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
