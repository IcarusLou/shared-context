use std::{error, fmt};

const MAX_ERROR_BYTES: usize = 512;

/// Stable failure categories produced by scenario parsing and validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ContractErrorKind {
    DocumentTooLarge,
    InvalidJson,
    InvalidSchema,
    UnsupportedSchema,
    UnsupportedVersion,
    UnsupportedAction,
    CapacityExceeded,
    DuplicateActor,
    DuplicateStep,
    DuplicateVariable,
    DuplicateFault,
    DuplicateAssertion,
    DuplicateEvent,
    DuplicateResource,
    DanglingReference,
    DependencyCycle,
    ForwardReference,
    InvalidDependencyOrder,
    VariableNotReady,
    VariableTypeMismatch,
    InvalidCapture,
    InvalidFaultTarget,
    InvalidEventClassification,
    InvalidResource,
    InvalidExpectation,
    HardcodedDomainId,
    ForgedExpected,
}

/// One bounded, user-safe contract error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractError {
    kind: ContractErrorKind,
    message: String,
}

impl ContractError {
    pub(crate) fn new(kind: ContractErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_BYTES {
            message.truncate(MAX_ERROR_BYTES);
        }
        Self { kind, message }
    }

    /// Machine-readable error category.
    #[must_use]
    pub const fn kind(&self) -> ContractErrorKind {
        self.kind
    }

    /// Bounded diagnostic without scenario payload contents.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "scenario contract {:?}: {}",
            self.kind, self.message
        )
    }
}

impl error::Error for ContractError {}

pub(crate) fn invalid_schema(message: impl Into<String>) -> ContractError {
    ContractError::new(ContractErrorKind::InvalidSchema, message)
}
