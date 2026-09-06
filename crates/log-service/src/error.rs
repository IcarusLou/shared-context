use std::{fmt, io};

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotConfigured,
    InvalidConfig,
    InvalidPath,
    Permission,
    Io,
    Endpoint,
    CorruptFrame,
    CorruptBatch,
    StoragePressure,
    InvalidInput,
}

#[derive(Debug)]
pub struct Error {
    code: ErrorCode,
    message: String,
    source: Option<io::Error>,
}

impl Error {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        let code = if source.kind() == io::ErrorKind::PermissionDenied {
            ErrorCode::Permission
        } else {
            ErrorCode::Io
        };
        Self {
            code,
            message: context.into(),
            source: Some(source),
        }
    }

    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }
}

impl ErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::InvalidConfig => "invalid_config",
            Self::InvalidPath => "invalid_path",
            Self::Permission => "permission",
            Self::Io => "io",
            Self::Endpoint => "endpoint",
            Self::CorruptFrame => "corrupt_frame",
            Self::CorruptBatch => "corrupt_batch",
            Self::StoragePressure => "storage_pressure",
            Self::InvalidInput => "invalid_input",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}
