//! Versioned, test-only protocol for dynamic Shared Context black-box scenarios.
//!
//! This crate defines a testing bounded context and deliberately does not extend the product
//! glossary. A [`ScenarioDefinition`] records ordered actions and causal dependencies, not a saved
//! final state. An [`AgentProfile`] selects a vendor, semantic version, and wire framing. A
//! [`ScenarioActor`] is a scenario-local Session or Task handle. A [`ScenarioAction`] is a typed
//! description that a separate runner may execute. A [`ScenarioVariable`] captures one typed value
//! from an already completed action. A [`FaultPlan`] describes a deterministic scheduling or
//! delivery fault. An [`InvariantAssertion`] selects one stable product invariant rather than an
//! expected JSON document or model-authored text. An [`EventClassification`] distinguishes
//! supported, observed-only, and unsupported vendor events; observed-only events cannot map to a
//! product action.
//!
//! Parsing and validation are side-effect free. This crate never starts a process, opens a network
//! connection, invokes a model, or reads local Agent session data.

mod error;
mod model;
mod validation;

pub use error::{ContractError, ContractErrorKind};
pub use model::{
    ActionKind, ActorId, ActorKind, AgentFraming, AgentProfile, AgentVendor, AgentVersion,
    AssertionId, BarrierId, CaptureSource, ConfirmationEventCount, CrashTiming,
    EventClassification, EventSupport, FaultId, FaultKind, FaultPlan, InvariantAssertion,
    InvariantKind, JsonPointer, MAX_IDENTIFIER_BYTES, MAX_JSON_POINTER_BYTES, MAX_WIRE_NAME_BYTES,
    ObservationSource, ProductAction, RawContentKind, ScenarioAction, ScenarioActor,
    ScenarioContractVersion, ScenarioDefinition, ScenarioSchema, ScenarioVariable, StepId,
    TemplateValue, VariableKind, VariableName, WireName,
};
pub use validation::{
    MAX_ACTIONS, MAX_ACTORS, MAX_ASSERTIONS, MAX_DEPENDENCIES_PER_ACTION, MAX_DOCUMENT_BYTES,
    MAX_EVENTS, MAX_FAULTS, MAX_TEMPLATE_DEPTH, MAX_TEMPLATE_NODES, MAX_VARIABLES, parse_scenario,
    to_canonical_json,
};
