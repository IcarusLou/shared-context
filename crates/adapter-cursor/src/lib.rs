//! Strict Cursor hook payload/output adapter.
//!
//! Supports Cursor 3.13.0 and newer. Missing, malformed, or older versions retain MCP + CLI while
//! all Hook actions are disabled. `beforeSubmitPrompt` is translated for completeness but never
//! drives Context Pack injection.

use std::path::PathBuf;

pub use sctx_agent_adapter::{
    AgentCapabilities, CanonicalAgentEvent, CanonicalAgentEventKind, ResolvedAgentAction,
};
use sctx_agent_adapter::{
    AgentEventContext, AgentKind, ToolOutcome, TrustState, evaluate_capabilities,
    path_hints_from_tool_input,
};
pub use sctx_domain::{Error, ErrorKind, Result};
use serde::Deserialize;
use serde_json::{Value, json};

/// Minimum supported version exercised by checked-in fixtures and the local Cursor build.
pub const VERIFIED_VERSION_REQUIREMENT: &str = ">=3.13.0";

#[must_use]
pub fn capabilities(version: Option<&str>, hook_available: bool) -> AgentCapabilities {
    evaluate_capabilities(
        AgentKind::Cursor,
        version,
        VERIFIED_VERSION_REQUIREMENT,
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
            ("generation_id", self.generation_id.as_str()),
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
    #[allow(dead_code)]
    tool_output: String,
    tool_use_id: String,
    cwd: String,
    #[allow(dead_code)]
    duration: u64,
}

#[derive(Deserialize)]
struct PreCompactInput {
    #[serde(flatten)]
    common: Common,
    trigger: String,
    #[allow(dead_code)]
    context_usage_percent: f64,
    #[allow(dead_code)]
    context_tokens: u64,
    #[allow(dead_code)]
    context_window_size: u64,
    #[allow(dead_code)]
    message_count: u64,
    #[allow(dead_code)]
    messages_to_compact: u64,
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    duration_ms: u64,
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
/// Rejects malformed JSON, unsupported Hook names, wrong field types, empty required identity
/// fields, and undocumented enum values used by policy.
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
            require_nonempty("cwd", &input.cwd)?;
            CanonicalAgentEvent::PostToolUse {
                context: input.common.context(None, Some(&input.cwd)),
                tool_name: input.tool_name,
                tool_use_id: input.tool_use_id,
                path_hints: path_hints_from_tool_input(&input.tool_input),
                outcome: ToolOutcome::Succeeded,
            }
        }
        "preCompact" => {
            let input: PreCompactInput = decode(value, "preCompact")?;
            input.common.validate("preCompact")?;
            require_one_of(
                "Cursor preCompact trigger",
                &input.trigger,
                &["auto", "manual"],
            )?;
            CanonicalAgentEvent::PreCompact {
                context: input.common.context(None, None),
                trigger: input.trigger,
            }
        }
        "stop" => {
            let input: StopInput = decode(value, "stop")?;
            input.common.validate("stop")?;
            require_one_of(
                "Cursor stop status",
                &input.status,
                &["completed", "aborted", "error"],
            )?;
            let _ = input.loop_count;
            CanonicalAgentEvent::TurnStop {
                context: input.common.context(None, None),
                status: input.status,
            }
        }
        "sessionEnd" => {
            let input: SessionEndInput = decode(value, "sessionEnd")?;
            input.common.validate("sessionEnd")?;
            require_one_of(
                "Cursor sessionEnd reason",
                &input.reason,
                &[
                    "completed",
                    "aborted",
                    "error",
                    "window_close",
                    "user_close",
                ],
            )?;
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
        CanonicalAgentEventKind::PreCompact => action
            .system_message
            .as_ref()
            .map_or_else(|| json!({}), |message| json!({"user_message": message})),
        CanonicalAgentEventKind::PromptSubmit
        | CanonicalAgentEventKind::TurnStop
        | CanonicalAgentEventKind::SessionEnd => json!({}),
    };
    serde_json::to_vec(&value)
        .map_err(|error| Error::new(ErrorKind::Io, format!("encode Cursor hook output: {error}")))
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
