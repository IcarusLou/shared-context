use std::{
    collections::BTreeMap,
    fmt,
    path::{Component, Path},
};

use semver::Version;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::error::invalid_schema;

/// Maximum bytes in a protocol identifier such as a step or variable name.
pub const MAX_IDENTIFIER_BYTES: usize = 64;
/// Maximum bytes in a vendor event or operation name.
pub const MAX_WIRE_NAME_BYTES: usize = 128;
/// Maximum bytes in a JSON Pointer used for a scalar capture.
pub const MAX_JSON_POINTER_BYTES: usize = 256;
/// Maximum bytes in one sandbox-relative resource path.
pub const MAX_RESOURCE_PATH_BYTES: usize = 256;

macro_rules! protocol_identifier {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Creates a bounded protocol identifier.
            ///
            /// # Errors
            ///
            /// Rejects empty, overlong, or non-ASCII identifier text. Identifiers begin with a
            /// lowercase letter and then use lowercase letters, digits, `-`, or `_`.
            pub fn new(value: impl Into<String>) -> Result<Self, crate::ContractError> {
                let value = value.into();
                validate_identifier(stringify!($name), &value)?;
                Ok(Self(value))
            }

            /// Returns the stable wire text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

protocol_identifier!(
    ActorId,
    "Scenario-local identity of a Session or Task actor."
);
protocol_identifier!(
    StepId,
    "Scenario-local identity of one ordered action step."
);
protocol_identifier!(
    VariableName,
    "Scenario-local name of one captured runtime value."
);
protocol_identifier!(
    FaultId,
    "Scenario-local identity of one deterministic fault plan."
);
protocol_identifier!(
    AssertionId,
    "Scenario-local identity of one typed invariant assertion."
);
protocol_identifier!(BarrierId, "Scenario-local identity of a scheduler barrier.");
protocol_identifier!(
    ResourceId,
    "Scenario-local identity of one sandbox resource root."
);

fn validate_identifier(name: &str, value: &str) -> Result<(), crate::ContractError> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(invalid_schema(format!(
            "{name} must contain 1..={MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    let mut bytes = value.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
    {
        return Err(invalid_schema(format!(
            "{name} must be a lowercase ASCII protocol identifier"
        )));
    }
    Ok(())
}

/// A bounded vendor wire event or operation name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WireName(String);

impl WireName {
    /// Creates a non-empty printable ASCII wire name.
    ///
    /// # Errors
    ///
    /// Rejects whitespace, control characters, path separators, and overlong values.
    pub fn new(value: impl Into<String>) -> Result<Self, crate::ContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_WIRE_NAME_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(invalid_schema(format!(
                "wire name must contain 1..={MAX_WIRE_NAME_BYTES} safe ASCII bytes"
            )));
        }
        Ok(Self(value))
    }

    /// Returns the exact vendor wire name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded machine-readable product failure code used by an expected typed failure.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExpectedFailureCode(String);

impl ExpectedFailureCode {
    /// Creates a safe lowercase failure code.
    ///
    /// # Errors
    ///
    /// Rejects empty, overlong, or non-lowercase identifier text.
    pub fn new(value: impl Into<String>) -> Result<Self, crate::ContractError> {
        let value = value.into();
        let mut bytes = value.bytes();
        if value.is_empty()
            || value.len() > MAX_IDENTIFIER_BYTES
            || !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid_schema(
                "expected failure code must be a bounded lowercase identifier",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ExpectedFailureCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExpectedFailureCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl fmt::Display for WireName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for WireName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WireName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A non-empty RFC 6901 JSON Pointer selecting one captured response value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonPointer(String);

impl JsonPointer {
    /// Creates a bounded non-root JSON Pointer.
    ///
    /// # Errors
    ///
    /// Rejects an empty/root capture, invalid `~` escaping, or an overlong pointer.
    pub fn new(value: impl Into<String>) -> Result<Self, crate::ContractError> {
        let value = value.into();
        if value.len() > MAX_JSON_POINTER_BYTES || !value.starts_with('/') {
            return Err(invalid_schema(format!(
                "capture pointer must be a non-root RFC 6901 pointer of at most {MAX_JSON_POINTER_BYTES} bytes"
            )));
        }
        let bytes = value.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'~' {
                let Some(next) = bytes.get(index + 1) else {
                    return Err(invalid_schema("capture pointer has an incomplete escape"));
                };
                if !matches!(next, b'0' | b'1') {
                    return Err(invalid_schema("capture pointer has an invalid escape"));
                }
                index += 1;
            }
            index += 1;
        }
        Ok(Self(value))
    }

    /// Returns the exact JSON Pointer.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for JsonPointer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for JsonPointer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A bounded safe path below a declared sandbox resource root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourcePath(String);

impl ResourcePath {
    /// Creates a normalized sandbox-relative regular-file path.
    ///
    /// # Errors
    ///
    /// Rejects absolute paths, traversal, empty or hidden Git-control components, backslashes,
    /// non-ASCII text, and values over the protocol bound.
    pub fn new(value: impl Into<String>) -> Result<Self, crate::ContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_RESOURCE_PATH_BYTES
            || value.contains('\\')
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains("//")
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
        {
            return Err(invalid_schema(
                "resource path is not a bounded safe relative path",
            ));
        }
        let path = Path::new(&value);
        if path.is_absolute()
            || path.components().any(|component| match component {
                Component::Normal(value) => value
                    .to_str()
                    .is_none_or(|value| value.to_ascii_lowercase().starts_with(".git")),
                Component::CurDir
                | Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_) => true,
            })
        {
            return Err(invalid_schema(
                "resource path cannot escape or control its synthetic repository",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResourcePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ResourcePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ResourcePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Contract family marker. This test protocol is not a product-domain schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScenarioSchema {
    #[serde(rename = "shared-context.dynamic-scenario")]
    DynamicScenario,
}

/// Supported dynamic scenario protocol versions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScenarioContractVersion {
    #[serde(rename = "1")]
    V1,
}

/// Supported Agent vendors in the phase-one replay protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentVendor {
    Cursor,
    Codex,
}

/// A parsed semantic Agent version rather than an opaque label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentVersion(Version);

impl AgentVersion {
    /// Creates a semantic version.
    ///
    /// # Errors
    ///
    /// Rejects invalid or overlong semantic versions.
    pub fn new(value: &str) -> Result<Self, crate::ContractError> {
        if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
            return Err(invalid_schema("agent version is empty or over capacity"));
        }
        Version::parse(value)
            .map(Self)
            .map_err(|_| invalid_schema("agent version must be valid SemVer"))
    }

    /// Returns the normalized semantic version.
    #[must_use]
    pub fn as_version(&self) -> &Version {
        &self.0
    }
}

impl fmt::Display for AgentVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for AgentVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Wire framing used by the Agent-facing scenario stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFraming {
    NewlineDelimitedJson,
    ContentLength,
    CliJson,
}

/// Vendor/version/framing tuple under test.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub vendor: AgentVendor,
    pub version: AgentVersion,
    pub framing: AgentFraming,
}

/// Logical actor kind. These are scenario handles, never product IDs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActorKind {
    Session,
    Task { session: ActorId },
}

/// One scenario-local Session or Task actor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioActor {
    pub id: ActorId,
    pub kind: ActorKind,
}

/// Typed runtime value captured from a completed step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableKind {
    SpaceId,
    RepositoryId,
    ReferenceId,
    TaskId,
    TaskSessionId,
    ExternalSessionId,
    IntentRevisionId,
    SignalId,
    WorkEpisodeId,
    WorkObservationId,
    CheckpointId,
    ClaimId,
    CandidateBuildId,
    SpaceRecommendationId,
    CandidateId,
    SubmissionId,
    ConfirmationId,
    SpaceAssociationId,
    ContextId,
    RevisionId,
    EventId,
    PublicationId,
    EvidenceId,
    ReviewId,
    ConflictId,
    ResolutionId,
    Generation,
    Status,
    Count,
    Boolean,
    Text,
}

/// Response field from which the runner must capture a typed scalar.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSource {
    pub step: StepId,
    pub pointer: JsonPointer,
}

/// A named typed runtime value. Its source is an earlier step response, never fixture state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioVariable {
    pub name: VariableName,
    pub value_type: VariableKind,
    pub capture: CaptureSource,
}

/// One handwritten regular file materialized only below a runner-owned resource root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxFile {
    pub path: ResourcePath,
    pub content: String,
}

/// Bounded resource root optionally initialized as a synthetic local Git repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxResource {
    pub id: ResourceId,
    #[serde(default)]
    pub synthetic_git: bool,
    pub files: Vec<SandboxFile>,
}

/// Runner-owned values that a fixture may reference without embedding machine-local paths or
/// external Session identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxBuiltin {
    Home,
    Root,
    Workspace,
    Temp,
    ResourceRoot {
        resource: ResourceId,
    },
    ResourceFile {
        resource: ResourceId,
        path: ResourcePath,
    },
    ActorSessionKey {
        actor: ActorId,
    },
}

/// Strongly typed JSON template. Runtime variables are explicit leaves, not string interpolation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TemplateValue {
    Null,
    Boolean { value: bool },
    Integer { value: i64 },
    Unsigned { value: u64 },
    String { value: String },
    Array { items: Vec<Self> },
    Object { fields: BTreeMap<String, Self> },
    Variable { name: VariableName },
    Builtin { builtin: SandboxBuiltin },
}

/// Safe cross-interface error classes accepted by typed expected-failure actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedFailureKind {
    InvalidInput,
    StaleState,
    Conflict,
    PrivacyRejected,
    Unsupported,
    RepositoryNotConfigured,
    IdempotencyKeyConflict,
}

/// Expected action disposition. This is never an expected response document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionExpectation {
    #[default]
    Success,
    TypedFailure {
        code: ExpectedFailureCode,
        kind: ExpectedFailureKind,
    },
}

/// Read-only state surface available to the future black-box observer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    Runtime,
    Git,
    Index,
    Graph,
}

/// One ordered, side-effect description. This crate only validates; it never executes actions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionKind {
    McpRequest {
        method: WireName,
        params: TemplateValue,
    },
    CliJson {
        arguments: Vec<TemplateValue>,
    },
    HookEvent {
        event: WireName,
        payload: TemplateValue,
    },
    Observe {
        source: ObservationSource,
        selector: TemplateValue,
    },
    Restart,
    Barrier {
        barrier: BarrierId,
    },
}

impl ActionKind {
    #[must_use]
    pub(crate) const fn produces_output(&self) -> bool {
        matches!(
            self,
            Self::McpRequest { .. }
                | Self::CliJson { .. }
                | Self::HookEvent { .. }
                | Self::Observe { .. }
        )
    }

    #[must_use]
    pub(crate) const fn accepts_faults(&self) -> bool {
        matches!(
            self,
            Self::McpRequest { .. } | Self::CliJson { .. } | Self::HookEvent { .. }
        )
    }
}

/// One action step and its explicit causal predecessors.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioAction {
    pub id: StepId,
    pub actor: ActorId,
    #[serde(default)]
    pub after: Vec<StepId>,
    #[serde(default)]
    pub expectation: ActionExpectation,
    pub action: ActionKind,
}

/// Whether a crash is injected before or after the target action is attempted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashTiming {
    Before,
    After,
}

/// Deterministic fault primitives consumed by the future runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FaultKind {
    Drop {
        target: StepId,
    },
    Repeat {
        target: StepId,
        times: u8,
    },
    Reorder {
        first: StepId,
        second: StepId,
    },
    Crash {
        target: StepId,
        actor: ActorId,
        timing: CrashTiming,
    },
    Concurrent {
        targets: Vec<StepId>,
    },
}

/// A named, bounded fault-injection plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultPlan {
    pub id: FaultId,
    pub fault: FaultKind,
}

/// Stable canonical product action to which a supported vendor event maps.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductAction {
    SessionStart,
    PromptSubmit,
    PostToolUse,
    PreCompact,
    TurnStop,
    SessionEnd,
}

/// Support posture for a vendor event observed in a session protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSupport {
    Supported,
    ObservedOnly,
    Unsupported,
}

/// Explicit classification of one vendor event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventClassification {
    pub event: WireName,
    pub classification: EventSupport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_action: Option<ProductAction>,
}

/// Allowed event counts for one atomic Candidate confirmation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationEventCount {
    Four,
    Five,
}

/// Raw content classes that must remain absent from persistence and reports.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawContentKind {
    Prompt,
    Transcript,
    ToolOutput,
}

/// One non-vacuous raw-content probe tied to the compatible Hook payload that carries it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawContentProbe {
    pub kind: RawContentKind,
    pub hook: StepId,
}

/// Closed set of stable product invariants. There is deliberately no generic equality or
/// expected-output variant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvariantKind {
    ActiveTaskPerSession {
        observation: StepId,
        session: ActorId,
        task: VariableName,
    },
    CanonicalContinueKeepsRevision {
        first_revision: VariableName,
        retry_revision: VariableName,
    },
    StaleCasZeroWrites {
        attempt: StepId,
        before_observation: StepId,
        after_observation: StepId,
    },
    OpenEpisodeHasNoCandidate {
        episode: VariableName,
        episode_observation: StepId,
        candidate_observation: StepId,
    },
    TurnStopIsIdempotent {
        first_stop: StepId,
        repeated_stop: StepId,
        first_candidate: VariableName,
        repeated_candidate: VariableName,
    },
    CandidateSourceEpisode {
        response: StepId,
        candidate: VariableName,
        episode: VariableName,
    },
    CandidateIsNotAutoInjected {
        candidate: VariableName,
        response: StepId,
    },
    ConfirmationIsAtomic {
        confirmation: VariableName,
        response: StepId,
        event_count: ConfirmationEventCount,
    },
    SessionEndDoesNotCloseEpisode {
        session_end: StepId,
        observation: StepId,
        episode: VariableName,
    },
    WorkingIntentHintHasNoGraphPath {
        response: StepId,
    },
    ArtifactFocusIsRequestScoped {
        focused_response: StepId,
        ordinary_response: StepId,
        resource: ResourceId,
        path: ResourcePath,
    },
    RawContentIsAbsent {
        probes: Vec<RawContentProbe>,
    },
}

/// One named typed invariant assertion evaluated after the action sequence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvariantAssertion {
    pub id: AssertionId,
    pub invariant: InvariantKind,
}

/// Versioned test-protocol description of actions and causality, never final product state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioDefinition {
    pub schema: ScenarioSchema,
    pub version: ScenarioContractVersion,
    pub name: WireName,
    pub agent: AgentProfile,
    pub actors: Vec<ScenarioActor>,
    pub actions: Vec<ScenarioAction>,
    #[serde(default)]
    pub resources: Vec<SandboxResource>,
    #[serde(default)]
    pub variables: Vec<ScenarioVariable>,
    #[serde(default)]
    pub faults: Vec<FaultPlan>,
    pub assertions: Vec<InvariantAssertion>,
    pub events: Vec<EventClassification>,
}
