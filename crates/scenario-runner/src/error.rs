use std::{error, fmt};

use serde::{Deserialize, Serialize};

const MAX_FAILURE_MESSAGE_BYTES: usize = 256;

/// Phase-one safe failure buckets. Mew #176 owns the final reporting taxonomy and policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClassification {
    InvalidScenario,
    PolicyViolation,
    ProcessFailure,
    Timeout,
    CaptureFailure,
    ObserverFailure,
}

/// Reproducible, sanitized failure report. Child stderr, action templates, and response bodies are
/// never retained in this type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerFailure {
    pub scenario: String,
    pub seed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub classification: FailureClassification,
    pub message: String,
}

impl RunnerFailure {
    pub(crate) fn new(
        scenario: impl Into<String>,
        seed: u64,
        step: Option<&str>,
        classification: FailureClassification,
        message: &'static str,
    ) -> Self {
        let mut message = sanitize_message(message);
        if message.len() > MAX_FAILURE_MESSAGE_BYTES {
            message.truncate(MAX_FAILURE_MESSAGE_BYTES);
        }
        Self {
            scenario: scenario.into(),
            seed,
            step: step.map(str::to_owned),
            classification,
            message,
        }
    }

    pub(crate) fn invalid(seed: u64, message: &'static str) -> Self {
        Self::new(
            "invalid-scenario",
            seed,
            None,
            FailureClassification::InvalidScenario,
            message,
        )
    }
}

impl fmt::Display for RunnerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "scenario {} seed {} {:?}: {}",
            self.scenario, self.seed, self.classification, self.message
        )
    }
}

impl error::Error for RunnerFailure {}

fn sanitize_message(message: &str) -> String {
    let lowercase = message.to_ascii_lowercase();
    if [
        "prompt",
        "transcript",
        "tool output",
        "tool_output",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
    {
        "sensitive failure detail redacted".to_owned()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_message;

    #[test]
    fn sensitive_failure_text_is_never_retained() {
        assert_eq!(
            sanitize_message("raw prompt contained SECRET_VALUE"),
            "sensitive failure detail redacted"
        );
        assert_eq!(sanitize_message("child exited"), "child exited");
    }
}
