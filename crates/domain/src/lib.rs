//! Domain primitives shared by every Shared Context component.
//!
//! The workspace skeleton intentionally defines no domain events or reducers yet.

use std::fmt;

/// Broad categories used to route recoverable errors across crate boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Input failed validation before any state change was attempted.
    InvalidInput,
    /// A core product invariant would be violated.
    InvariantViolation,
    /// A local filesystem or process operation failed.
    Io,
    /// An external command or protocol peer failed.
    External,
    /// The requested behavior is outside the current implementation stage.
    Unsupported,
}

/// The shared, user-presentable error type for workspace crates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Creates an error with a stable category and actionable message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the user-presentable error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Workspace-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};

    #[test]
    fn shared_error_keeps_kind_and_message() {
        let error = Error::new(ErrorKind::InvariantViolation, "append-only contract failed");

        assert_eq!(error.kind(), ErrorKind::InvariantViolation);
        assert_eq!(error.message(), "append-only contract failed");
        assert_eq!(error.to_string(), "append-only contract failed");
    }
}
