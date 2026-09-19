use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

static INVOCATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Creates a process-local invocation identity without random sources or I/O.
/// Invocation plus the event sequence is the ordering boundary; this is not a durable event ID.
#[must_use]
pub fn new_invocation_id() -> String {
    let sequence = INVOCATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        now_unix_ms(),
        sequence
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryPoint {
    Cli,
    Hook,
    Mcp,
    Maintenance,
    Collector,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    OperationStarted,
    OperationFinished,
    HookDecision,
    ToolStarted,
    ToolFinished,
    ProtocolFailure,
    MaintenanceStepFinished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Started,
    Success,
    Failure,
    Disabled,
    FailOpen,
    Degraded,
    Dropped,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authorization {
    NotApplicable,
    Authorized,
    Unauthorized,
    Unverified,
}

/// A deliberately closed telemetry schema. There is no arbitrary attribute map,
/// so callers cannot accidentally attach arguments, prompts, output, or environment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub occurred_at_unix_ms: i64,
    pub duration_ms: Option<u32>,
    pub sequence: u32,
    pub entry_point: EntryPoint,
    pub kind: EventKind,
    pub outcome: Outcome,
    pub authorization: Authorization,
    pub invocation_id: String,
    pub program_version: String,
    pub operation: Option<String>,
    pub error_code: Option<String>,
    pub error_family: Option<String>,
    pub reason: Option<String>,
    pub summary: Option<String>,
    pub session_digest: Option<String>,
    pub task_id: Option<String>,
    pub task_session_id: Option<String>,
    pub episode_id: Option<String>,
    pub checkpoint_id: Option<String>,
    pub operation_id: Option<String>,
    pub result_count: Option<u32>,
}

impl Event {
    #[must_use]
    pub fn started(
        entry_point: EntryPoint,
        kind: EventKind,
        invocation_id: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        Self::base(
            entry_point,
            kind,
            invocation_id,
            operation,
            Outcome::Started,
        )
    }

    #[must_use]
    pub fn finished(
        entry_point: EntryPoint,
        kind: EventKind,
        invocation_id: impl Into<String>,
        operation: impl Into<String>,
        outcome: Outcome,
    ) -> Self {
        Self::base(entry_point, kind, invocation_id, operation, outcome)
    }

    fn base(
        entry_point: EntryPoint,
        kind: EventKind,
        invocation_id: impl Into<String>,
        operation: impl Into<String>,
        outcome: Outcome,
    ) -> Self {
        Self {
            occurred_at_unix_ms: now_unix_ms(),
            duration_ms: None,
            sequence: 0,
            entry_point,
            kind,
            outcome,
            authorization: Authorization::NotApplicable,
            invocation_id: invocation_id.into(),
            // The same string `sctx --version` prints, so a telemetry row and a support
            // conversation identify one build the same way. [`normalized`](Self::normalized)
            // bounds it to a token; see [`crate::VERSION`] for why it survives that intact.
            program_version: crate::VERSION.to_owned(),
            operation: Some(operation.into()),
            error_code: None,
            error_family: None,
            reason: None,
            summary: None,
            session_digest: None,
            task_id: None,
            task_session_id: None,
            episode_id: None,
            checkpoint_id: None,
            operation_id: None,
            result_count: None,
        }
    }

    /// Bounds every field and removes summaries which look like paths or secrets.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self::bounded_from(&self)
    }

    pub(crate) fn bounded_from(source: &Self) -> Self {
        let mut bounded = Self {
            occurred_at_unix_ms: source.occurred_at_unix_ms,
            duration_ms: source.duration_ms,
            sequence: source.sequence,
            entry_point: source.entry_point,
            kind: source.kind,
            outcome: source.outcome,
            authorization: source.authorization,
            invocation_id: bounded_identifier(&source.invocation_id, 64),
            program_version: bounded_token(&source.program_version, 32),
            operation: bounded_option(source.operation.as_deref(), 48),
            error_code: bounded_option(source.error_code.as_deref(), 48),
            error_family: bounded_option(source.error_family.as_deref(), 32),
            reason: bounded_option(source.reason.as_deref(), 64),
            summary: None,
            session_digest: bounded_option(source.session_digest.as_deref(), 64),
            task_id: bounded_option(source.task_id.as_deref(), 64),
            task_session_id: bounded_option(source.task_session_id.as_deref(), 64),
            episode_id: bounded_option(source.episode_id.as_deref(), 64),
            checkpoint_id: bounded_option(source.checkpoint_id.as_deref(), 64),
            // Durable operation identities include the `sha256:` prefix plus 64 hex digits.
            // Validate the whole token instead of truncating it: a prefix of a content hash is
            // not the same business identity and must never be reported as though it were.
            operation_id: validated_identifier_option(source.operation_id.as_deref(), 96),
            result_count: source.result_count,
        };
        // V1 deliberately discards free text. Typed codes and closed operation/reason
        // enums carry the useful signal without trying to implement a partial secret scanner.
        bounded.summary = None;
        bounded
    }
}

pub(crate) fn now_unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn bounded_option(value: Option<&str>, max: usize) -> Option<String> {
    let bounded = bounded_token(value?, max);
    (!bounded.is_empty()).then_some(bounded)
}

fn validated_identifier_option(value: Option<&str>, max: usize) -> Option<String> {
    let value = value?;
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn bounded_identifier(value: &str, max: usize) -> String {
    bounded_with(value, max, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
    })
}

fn bounded_token(value: &str, max: usize) -> String {
    bounded_with(value, max, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b' ')
    })
}

fn bounded_with(value: &str, max: usize, allow: impl Fn(u8) -> bool) -> String {
    let filtered: String = value
        .bytes()
        .take(max.saturating_mul(4))
        .filter(|byte| allow(*byte))
        .take(max)
        .map(char::from)
        .collect();
    filtered
}

#[cfg(test)]
mod tests {
    use super::{EntryPoint, Event, EventKind};

    /// The build fingerprint has to survive normalization, or telemetry keeps reporting the one
    /// datum -- the version number -- that never distinguished two builds in the first place.
    #[test]
    fn the_build_fingerprint_survives_the_program_version_bound() {
        let normalized = Event::started(
            EntryPoint::Cli,
            EventKind::OperationFinished,
            "invocation",
            "version",
        )
        .normalized();

        assert!(
            normalized.program_version.len() <= 32,
            "{}",
            normalized.program_version
        );
        let mut parts = normalized.program_version.split(' ');
        assert_eq!(parts.next(), Some(env!("CARGO_PKG_VERSION")));
        // `(`, `,` and `)` are not token characters, so the human form collapses to
        // `<version> <commit> <state>` rather than losing its tail to the 32-byte bound.
        let commit = parts.next().expect("a commit token");
        assert!(
            commit == "unknown" || commit.len() == 7,
            "{}",
            normalized.program_version
        );
    }
}
