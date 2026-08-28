use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;
use uuid::{Variant, Version};

use crate::{
    ActionExpectation, ActionKind, ActorKind, AgentFraming, AgentVendor, ContractError,
    ContractErrorKind, EventSupport, ExpectedFailureKind, FaultKind, InvariantKind, ProductAction,
    RawContentKind, SandboxBuiltin, ScenarioDefinition, StepId, TemplateValue, VariableKind,
    VariableName, error::invalid_schema,
};

/// Maximum accepted serialized scenario size.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Maximum actors in one scenario.
pub const MAX_ACTORS: usize = 16;
/// Maximum action steps in one scenario.
pub const MAX_ACTIONS: usize = 128;
/// Maximum runtime variables in one scenario.
pub const MAX_VARIABLES: usize = 128;
/// Maximum fault plans in one scenario.
pub const MAX_FAULTS: usize = 64;
/// Maximum invariant assertions in one scenario.
pub const MAX_ASSERTIONS: usize = 128;
/// Maximum classified vendor events in one scenario.
pub const MAX_EVENTS: usize = 128;
/// Maximum sandbox resource roots in one scenario.
pub const MAX_RESOURCES: usize = 16;
/// Maximum regular files across one sandbox resource root.
pub const MAX_RESOURCE_FILES: usize = 64;
/// Maximum UTF-8 bytes in one handwritten sandbox file.
pub const MAX_RESOURCE_CONTENT_BYTES: usize = 4 * 1024;
/// Maximum direct causal dependencies declared by one action.
pub const MAX_DEPENDENCIES_PER_ACTION: usize = 32;
/// Maximum nesting depth of any JSON or typed action template.
pub const MAX_TEMPLATE_DEPTH: usize = 32;
/// Maximum total JSON/template nodes in one scenario.
pub const MAX_TEMPLATE_NODES: usize = 4_096;

const MAX_ARRAY_ITEMS: usize = 256;
const MAX_OBJECT_FIELDS: usize = 128;
const MAX_STRING_BYTES: usize = 4_096;
const MAX_CLI_ARGUMENTS: usize = 64;
const MAX_CONCURRENT_TARGETS: usize = 16;
const MAX_REPEAT_COUNT: u8 = 20;
const SCHEMA_NAME: &str = "shared-context.dynamic-scenario";
const VERSION_V1: &str = "1";
const FORBIDDEN_EXPECTED_FIELDS: [&str; 6] = [
    "expected",
    "expected_json",
    "expected_output",
    "final_state",
    "model_output",
    "model_text",
];
const SUPPORTED_ACTION_TYPES: [&str; 6] = [
    "mcp_request",
    "cli_json",
    "hook_event",
    "observe",
    "restart",
    "barrier",
];

/// Parse and fully validate one V1 scenario document without side effects.
///
/// # Errors
///
/// Returns a typed [`ContractError`] for malformed, unsupported, over-capacity, non-causal, or
/// unsafe scenario input.
pub fn parse_scenario(bytes: &[u8]) -> Result<ScenarioDefinition, ContractError> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(ContractError::new(
            ContractErrorKind::DocumentTooLarge,
            format!("scenario exceeds {MAX_DOCUMENT_BYTES} bytes"),
        ));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        ContractError::new(
            ContractErrorKind::InvalidJson,
            format!(
                "malformed JSON at line {}, column {}",
                error.line(),
                error.column()
            ),
        )
    })?;
    validate_raw_capacity(&value)?;
    preflight_version(&value)?;
    reject_forged_expected(&value)?;
    preflight_actions(&value)?;
    reject_hardcoded_domain_ids(&value)?;

    let scenario: ScenarioDefinition = serde_json::from_value(value)
        .map_err(|_| invalid_schema("document does not match the V1 scenario shape"))?;
    scenario.validate()?;
    Ok(scenario)
}

/// Serialize a validated scenario to stable compact JSON.
///
/// Object templates use `BTreeMap`, while all semantic sequences retain declared order.
///
/// # Errors
///
/// Rejects an invalid in-memory contract or a serialization invariant failure.
pub fn to_canonical_json(scenario: &ScenarioDefinition) -> Result<Vec<u8>, ContractError> {
    scenario.validate()?;
    serde_json::to_vec(scenario)
        .map_err(|_| invalid_schema("validated scenario could not be serialized"))
}

impl ScenarioDefinition {
    /// Validate an in-memory scenario using the same semantic rules as [`parse_scenario`].
    ///
    /// # Errors
    ///
    /// Returns a typed error when identity, capacity, causality, capture, fault, event, or
    /// invariant constraints are not satisfied.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_collection_capacity("actors", self.actors.len(), 1, MAX_ACTORS)?;
        validate_collection_capacity("actions", self.actions.len(), 1, MAX_ACTIONS)?;
        validate_collection_capacity("variables", self.variables.len(), 0, MAX_VARIABLES)?;
        validate_collection_capacity("faults", self.faults.len(), 0, MAX_FAULTS)?;
        validate_collection_capacity("assertions", self.assertions.len(), 1, MAX_ASSERTIONS)?;
        validate_collection_capacity("events", self.events.len(), 1, MAX_EVENTS)?;
        validate_collection_capacity("resources", self.resources.len(), 0, MAX_RESOURCES)?;

        validate_agent_framing(self)?;
        let resources = validate_resources(self)?;
        let actors = validate_actors(self)?;
        let graph = validate_actions(self, &actors)?;
        let variables = validate_variables(self, &graph)?;
        validate_action_variables(self, &actors, &resources, &graph, &variables)?;
        validate_faults(self, &actors, &graph)?;
        validate_events(self)?;
        validate_assertions(self, &actors, &resources, &graph, &variables)?;

        let value = serde_json::to_value(self)
            .map_err(|_| invalid_schema("scenario could not be represented as JSON"))?;
        validate_raw_capacity(&value)?;
        if serde_json::to_vec(&value)
            .map_err(|_| invalid_schema("scenario could not be sized as JSON"))?
            .len()
            > MAX_DOCUMENT_BYTES
        {
            return Err(ContractError::new(
                ContractErrorKind::DocumentTooLarge,
                format!("scenario exceeds {MAX_DOCUMENT_BYTES} bytes"),
            ));
        }
        reject_hardcoded_domain_ids(&value)
    }
}

fn validate_agent_framing(scenario: &ScenarioDefinition) -> Result<(), ContractError> {
    let valid = matches!(
        (scenario.agent.vendor, scenario.agent.framing),
        (AgentVendor::Cursor, AgentFraming::NewlineDelimitedJson)
            | (
                AgentVendor::Cursor | AgentVendor::Codex,
                AgentFraming::CliJson
            )
            | (AgentVendor::Codex, AgentFraming::ContentLength)
    );
    if valid {
        Ok(())
    } else {
        Err(invalid_schema(
            "agent vendor and framing are not a supported phase-one pair",
        ))
    }
}

fn validate_collection_capacity(
    name: &str,
    len: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), ContractError> {
    if (minimum..=maximum).contains(&len) {
        Ok(())
    } else {
        Err(ContractError::new(
            ContractErrorKind::CapacityExceeded,
            format!("{name} must contain {minimum}..={maximum} entries"),
        ))
    }
}

struct ResourceDefinition<'a> {
    files: HashSet<&'a str>,
}

fn validate_resources(
    scenario: &ScenarioDefinition,
) -> Result<HashMap<&str, ResourceDefinition<'_>>, ContractError> {
    let mut resources = HashMap::with_capacity(scenario.resources.len());
    for resource in &scenario.resources {
        validate_collection_capacity(
            "resource files",
            resource.files.len(),
            1,
            MAX_RESOURCE_FILES,
        )?;
        let mut files = HashSet::with_capacity(resource.files.len());
        let mut folded_files = HashSet::with_capacity(resource.files.len());
        for file in &resource.files {
            if file.content.len() > MAX_RESOURCE_CONTENT_BYTES {
                return Err(ContractError::new(
                    ContractErrorKind::CapacityExceeded,
                    format!(
                        "resource {} contains a file over {MAX_RESOURCE_CONTENT_BYTES} bytes",
                        resource.id
                    ),
                ));
            }
            if !files.insert(file.path.as_str()) {
                return Err(ContractError::new(
                    ContractErrorKind::InvalidResource,
                    format!("resource {} repeats a file path", resource.id),
                ));
            }
            if !folded_files.insert(file.path.as_str().to_ascii_lowercase()) {
                return Err(ContractError::new(
                    ContractErrorKind::InvalidResource,
                    format!("resource {} repeats a case-folded file path", resource.id),
                ));
            }
        }
        let paths = resource
            .files
            .iter()
            .map(|file| file.path.as_str().to_ascii_lowercase())
            .collect::<Vec<_>>();
        for (offset, first) in paths.iter().enumerate() {
            for second in &paths[offset + 1..] {
                if first
                    .strip_prefix(second)
                    .or_else(|| second.strip_prefix(first))
                    .is_some_and(|tail| tail.starts_with('/'))
                {
                    return Err(ContractError::new(
                        ContractErrorKind::InvalidResource,
                        format!(
                            "resource {} has a file/directory path conflict",
                            resource.id
                        ),
                    ));
                }
            }
        }
        if resources
            .insert(resource.id.as_str(), ResourceDefinition { files })
            .is_some()
        {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateResource,
                format!("resource {} is declared more than once", resource.id),
            ));
        }
    }
    Ok(resources)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorRole {
    Session,
    Task,
}

fn validate_actors(
    scenario: &ScenarioDefinition,
) -> Result<HashMap<&str, ActorRole>, ContractError> {
    let mut actors = HashMap::with_capacity(scenario.actors.len());
    for actor in &scenario.actors {
        let role = match actor.kind {
            ActorKind::Session => ActorRole::Session,
            ActorKind::Task { .. } => ActorRole::Task,
        };
        if actors.insert(actor.id.as_str(), role).is_some() {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateActor,
                format!("actor {} is declared more than once", actor.id),
            ));
        }
    }
    if !actors.values().any(|role| *role == ActorRole::Session)
        || !actors.values().any(|role| *role == ActorRole::Task)
    {
        return Err(invalid_schema(
            "a scenario must declare at least one Session and one Task actor",
        ));
    }
    for actor in &scenario.actors {
        if let ActorKind::Task { session } = &actor.kind {
            match actors.get(session.as_str()) {
                Some(ActorRole::Session) => {}
                Some(ActorRole::Task) => {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        format!("task actor {} must reference a Session actor", actor.id),
                    ));
                }
                None => {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        format!("task actor {} references an unknown Session", actor.id),
                    ));
                }
            }
        }
    }
    Ok(actors)
}

struct ActionGraph<'a> {
    indices: HashMap<&'a str, usize>,
    dependencies: Vec<Vec<usize>>,
}

impl ActionGraph<'_> {
    fn depends_on(&self, step: usize, possible_ancestor: usize) -> bool {
        let mut pending = self.dependencies[step].clone();
        let mut visited = HashSet::new();
        while let Some(current) = pending.pop() {
            if current == possible_ancestor {
                return true;
            }
            if visited.insert(current) {
                pending.extend(self.dependencies[current].iter().copied());
            }
        }
        false
    }
}

fn validate_actions<'a>(
    scenario: &'a ScenarioDefinition,
    actors: &HashMap<&str, ActorRole>,
) -> Result<ActionGraph<'a>, ContractError> {
    let mut indices = HashMap::with_capacity(scenario.actions.len());
    for (index, step) in scenario.actions.iter().enumerate() {
        if indices.insert(step.id.as_str(), index).is_some() {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateStep,
                format!("step {} is declared more than once", step.id),
            ));
        }
        let Some(role) = actors.get(step.actor.as_str()) else {
            return Err(ContractError::new(
                ContractErrorKind::DanglingReference,
                format!("step {} references an unknown actor", step.id),
            ));
        };
        if matches!(
            step.action,
            ActionKind::HookEvent { .. } | ActionKind::Restart
        ) && *role != ActorRole::Session
        {
            return Err(invalid_schema(format!(
                "step {} requires a Session actor",
                step.id
            )));
        }
        if let ActionKind::CliJson { arguments } = &step.action {
            validate_collection_capacity("CLI arguments", arguments.len(), 1, MAX_CLI_ARGUMENTS)?;
        }
        if matches!(step.expectation, ActionExpectation::TypedFailure { .. })
            && !matches!(
                step.action,
                ActionKind::McpRequest { .. } | ActionKind::CliJson { .. }
            )
        {
            return Err(ContractError::new(
                ContractErrorKind::InvalidExpectation,
                format!(
                    "step {} can expect a typed failure only from MCP or CLI",
                    step.id
                ),
            ));
        }
        validate_collection_capacity(
            "action dependencies",
            step.after.len(),
            0,
            MAX_DEPENDENCIES_PER_ACTION,
        )?;
    }

    let mut dependencies = Vec::with_capacity(scenario.actions.len());
    for step in &scenario.actions {
        let mut seen = HashSet::with_capacity(step.after.len());
        let mut resolved = Vec::with_capacity(step.after.len());
        for dependency in &step.after {
            if !seen.insert(dependency.as_str()) {
                return Err(ContractError::new(
                    ContractErrorKind::InvalidDependencyOrder,
                    format!("step {} repeats a dependency", step.id),
                ));
            }
            let Some(index) = indices.get(dependency.as_str()).copied() else {
                return Err(ContractError::new(
                    ContractErrorKind::DanglingReference,
                    format!("step {} has an unknown dependency", step.id),
                ));
            };
            resolved.push(index);
        }
        dependencies.push(resolved);
    }

    reject_dependency_cycles(&dependencies)?;
    for (step_index, step_dependencies) in dependencies.iter().enumerate() {
        if step_dependencies
            .iter()
            .any(|dependency| *dependency >= step_index)
        {
            return Err(ContractError::new(
                ContractErrorKind::ForwardReference,
                format!(
                    "step {} depends on itself or a later step",
                    scenario.actions[step_index].id
                ),
            ));
        }
        if !step_dependencies.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(ContractError::new(
                ContractErrorKind::InvalidDependencyOrder,
                format!(
                    "dependencies of step {} must follow declared action order",
                    scenario.actions[step_index].id
                ),
            ));
        }
    }
    Ok(ActionGraph {
        indices,
        dependencies,
    })
}

fn reject_dependency_cycles(dependencies: &[Vec<usize>]) -> Result<(), ContractError> {
    fn visit(
        node: usize,
        dependencies: &[Vec<usize>],
        states: &mut [u8],
    ) -> Result<(), ContractError> {
        if states[node] == 1 {
            return Err(ContractError::new(
                ContractErrorKind::DependencyCycle,
                "action dependencies contain a cycle",
            ));
        }
        if states[node] == 2 {
            return Ok(());
        }
        states[node] = 1;
        for dependency in &dependencies[node] {
            visit(*dependency, dependencies, states)?;
        }
        states[node] = 2;
        Ok(())
    }

    let mut states = vec![0_u8; dependencies.len()];
    for node in 0..dependencies.len() {
        visit(node, dependencies, &mut states)?;
    }
    Ok(())
}

struct VariableDefinition {
    kind: VariableKind,
    source_step: usize,
}

fn validate_variables<'a>(
    scenario: &'a ScenarioDefinition,
    graph: &ActionGraph<'_>,
) -> Result<HashMap<&'a str, VariableDefinition>, ContractError> {
    let mut variables = HashMap::with_capacity(scenario.variables.len());
    for variable in &scenario.variables {
        let Some(source_step) = graph.indices.get(variable.capture.step.as_str()).copied() else {
            return Err(ContractError::new(
                ContractErrorKind::DanglingReference,
                format!("variable {} captures an unknown step", variable.name),
            ));
        };
        if !scenario.actions[source_step].action.produces_output() {
            return Err(ContractError::new(
                ContractErrorKind::InvalidCapture,
                format!("variable {} captures a step without output", variable.name),
            ));
        }
        if matches!(
            scenario.actions[source_step].expectation,
            ActionExpectation::TypedFailure { .. }
        ) {
            return Err(ContractError::new(
                ContractErrorKind::InvalidCapture,
                format!("variable {} cannot capture a typed failure", variable.name),
            ));
        }
        if variables
            .insert(
                variable.name.as_str(),
                VariableDefinition {
                    kind: variable.value_type,
                    source_step,
                },
            )
            .is_some()
        {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateVariable,
                format!("variable {} is declared more than once", variable.name),
            ));
        }
    }
    Ok(variables)
}

fn validate_action_variables(
    scenario: &ScenarioDefinition,
    actors: &HashMap<&str, ActorRole>,
    resources: &HashMap<&str, ResourceDefinition<'_>>,
    graph: &ActionGraph<'_>,
    variables: &HashMap<&str, VariableDefinition>,
) -> Result<(), ContractError> {
    for (step_index, step) in scenario.actions.iter().enumerate() {
        let mut references = Vec::new();
        match &step.action {
            ActionKind::McpRequest { params, .. }
            | ActionKind::HookEvent {
                payload: params, ..
            }
            | ActionKind::Observe {
                selector: params, ..
            } => {
                validate_template_builtins(params, actors, resources)?;
                collect_template_variables(params, &mut references);
            }
            ActionKind::CliJson { arguments } => {
                for argument in arguments {
                    validate_template_builtins(argument, actors, resources)?;
                    collect_template_variables(argument, &mut references);
                }
            }
            ActionKind::Restart | ActionKind::Barrier { .. } => {}
        }
        for reference in references {
            let Some(variable) = variables.get(reference.as_str()) else {
                return Err(ContractError::new(
                    ContractErrorKind::DanglingReference,
                    format!("step {} references an unknown variable", step.id),
                ));
            };
            if variable.source_step >= step_index {
                return Err(ContractError::new(
                    ContractErrorKind::ForwardReference,
                    format!("step {} references a variable from a later step", step.id),
                ));
            }
            if !graph.depends_on(step_index, variable.source_step) {
                return Err(ContractError::new(
                    ContractErrorKind::VariableNotReady,
                    format!(
                        "step {} does not causally depend on its variable source",
                        step.id
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn collect_template_variables<'a>(value: &'a TemplateValue, output: &mut Vec<&'a VariableName>) {
    match value {
        TemplateValue::Array { items } => {
            for item in items {
                collect_template_variables(item, output);
            }
        }
        TemplateValue::Object { fields } => {
            for item in fields.values() {
                collect_template_variables(item, output);
            }
        }
        TemplateValue::Variable { name } => output.push(name),
        TemplateValue::Builtin { .. }
        | TemplateValue::Null
        | TemplateValue::Boolean { .. }
        | TemplateValue::Integer { .. }
        | TemplateValue::Unsigned { .. }
        | TemplateValue::String { .. } => {}
    }
}

fn validate_template_builtins(
    value: &TemplateValue,
    actors: &HashMap<&str, ActorRole>,
    resources: &HashMap<&str, ResourceDefinition<'_>>,
) -> Result<(), ContractError> {
    match value {
        TemplateValue::Array { items } => {
            for item in items {
                validate_template_builtins(item, actors, resources)?;
            }
        }
        TemplateValue::Object { fields } => {
            for item in fields.values() {
                validate_template_builtins(item, actors, resources)?;
            }
        }
        TemplateValue::Builtin { builtin } => match builtin {
            SandboxBuiltin::Home
            | SandboxBuiltin::Root
            | SandboxBuiltin::Workspace
            | SandboxBuiltin::Temp => {}
            SandboxBuiltin::ResourceRoot { resource } => {
                if !resources.contains_key(resource.as_str()) {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        "sandbox builtin references an unknown resource",
                    ));
                }
            }
            SandboxBuiltin::ResourceFile { resource, path } => {
                let Some(resource) = resources.get(resource.as_str()) else {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        "sandbox builtin references an unknown resource",
                    ));
                };
                if !resource.files.contains(path.as_str()) {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        "sandbox builtin references an undeclared resource file",
                    ));
                }
            }
            SandboxBuiltin::ActorSessionKey { actor } => {
                if actors.get(actor.as_str()) != Some(&ActorRole::Session) {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        "sandbox Session-key builtin requires a Session actor",
                    ));
                }
            }
        },
        TemplateValue::Null
        | TemplateValue::Boolean { .. }
        | TemplateValue::Integer { .. }
        | TemplateValue::Unsigned { .. }
        | TemplateValue::String { .. }
        | TemplateValue::Variable { .. } => {}
    }
    Ok(())
}

fn validate_faults(
    scenario: &ScenarioDefinition,
    actors: &HashMap<&str, ActorRole>,
    graph: &ActionGraph<'_>,
) -> Result<(), ContractError> {
    let mut fault_ids = HashSet::with_capacity(scenario.faults.len());
    for fault in &scenario.faults {
        if !fault_ids.insert(fault.id.as_str()) {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateFault,
                format!("fault {} is declared more than once", fault.id),
            ));
        }
        match &fault.fault {
            FaultKind::Drop { target } => {
                fault_target(scenario, graph, target)?;
            }
            FaultKind::Repeat { target, times } => {
                fault_target(scenario, graph, target)?;
                if !(2..=MAX_REPEAT_COUNT).contains(times) {
                    return Err(invalid_fault("repeat count must be between 2 and 20"));
                }
            }
            FaultKind::Reorder { first, second } => {
                let first = fault_target(scenario, graph, first)?;
                let second = fault_target(scenario, graph, second)?;
                if first == second
                    || graph.depends_on(first, second)
                    || graph.depends_on(second, first)
                {
                    return Err(invalid_fault(
                        "reorder targets must be distinct and causally independent",
                    ));
                }
            }
            FaultKind::Crash { target, actor, .. } => {
                let target = fault_target(scenario, graph, target)?;
                if !actors.contains_key(actor.as_str()) {
                    return Err(invalid_fault("crash references an unknown actor"));
                }
                if scenario.actions[target].actor != *actor {
                    return Err(invalid_fault("crash actor does not own the target step"));
                }
            }
            FaultKind::Concurrent { targets } => {
                validate_collection_capacity(
                    "concurrent fault targets",
                    targets.len(),
                    2,
                    MAX_CONCURRENT_TARGETS,
                )?;
                let mut indices = Vec::with_capacity(targets.len());
                let mut unique = HashSet::with_capacity(targets.len());
                for target in targets {
                    let index = fault_target(scenario, graph, target)?;
                    if !unique.insert(index) {
                        return Err(invalid_fault("concurrent targets must be unique"));
                    }
                    indices.push(index);
                }
                for (offset, first) in indices.iter().enumerate() {
                    for second in &indices[offset + 1..] {
                        if graph.depends_on(*first, *second) || graph.depends_on(*second, *first) {
                            return Err(invalid_fault(
                                "concurrent targets must be causally independent",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn fault_target(
    scenario: &ScenarioDefinition,
    graph: &ActionGraph<'_>,
    step: &StepId,
) -> Result<usize, ContractError> {
    let Some(index) = graph.indices.get(step.as_str()).copied() else {
        return Err(invalid_fault("fault references an unknown step"));
    };
    if !scenario.actions[index].action.accepts_faults() {
        return Err(invalid_fault(
            "fault target must be an MCP, CLI, or Hook product action",
        ));
    }
    if matches!(
        scenario.actions[index].expectation,
        ActionExpectation::TypedFailure { .. }
    ) {
        return Err(invalid_fault(
            "typed expected-failure steps cannot be fault targets",
        ));
    }
    Ok(index)
}

fn invalid_fault(message: impl Into<String>) -> ContractError {
    ContractError::new(ContractErrorKind::InvalidFaultTarget, message)
}

fn validate_events(scenario: &ScenarioDefinition) -> Result<(), ContractError> {
    let mut events = HashMap::with_capacity(scenario.events.len());
    for event in &scenario.events {
        if events.insert(event.event.as_str(), event).is_some() {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateEvent,
                format!("event {} is classified more than once", event.event),
            ));
        }
        match (event.classification, event.product_action) {
            (EventSupport::Supported, Some(_))
            | (EventSupport::ObservedOnly | EventSupport::Unsupported, None) => {}
            (EventSupport::Supported, None) => {
                return Err(ContractError::new(
                    ContractErrorKind::InvalidEventClassification,
                    "a supported event must map to a typed product action",
                ));
            }
            (EventSupport::ObservedOnly | EventSupport::Unsupported, Some(_)) => {
                return Err(ContractError::new(
                    ContractErrorKind::InvalidEventClassification,
                    "an observed-only or unsupported event cannot trigger a product action",
                ));
            }
        }
    }

    for step in &scenario.actions {
        let ActionKind::HookEvent { event, .. } = &step.action else {
            continue;
        };
        let Some(classification) = events.get(event.as_str()) else {
            return Err(ContractError::new(
                ContractErrorKind::InvalidEventClassification,
                format!("Hook action step {} uses an unclassified event", step.id),
            ));
        };
        if classification.classification != EventSupport::Supported
            || classification.product_action.is_none()
        {
            return Err(ContractError::new(
                ContractErrorKind::InvalidEventClassification,
                format!(
                    "Hook action step {} must use an event classified as supported",
                    step.id
                ),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_assertions(
    scenario: &ScenarioDefinition,
    actors: &HashMap<&str, ActorRole>,
    resources: &HashMap<&str, ResourceDefinition<'_>>,
    graph: &ActionGraph<'_>,
    variables: &HashMap<&str, VariableDefinition>,
) -> Result<(), ContractError> {
    let mut assertion_ids = HashSet::with_capacity(scenario.assertions.len());
    for assertion in &scenario.assertions {
        if !assertion_ids.insert(assertion.id.as_str()) {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateAssertion,
                format!("assertion {} is declared more than once", assertion.id),
            ));
        }
        match &assertion.invariant {
            InvariantKind::ActiveTaskPerSession {
                observation,
                session,
                task,
            } => {
                require_actor_role(actors, session.as_str(), ActorRole::Session)?;
                require_variable(variables, task, VariableKind::TaskId)?;
                let observed = require_observation_entity(
                    scenario,
                    graph,
                    observation,
                    crate::ObservationSource::Runtime,
                    "active_task",
                )?;
                require_observer_session_builtin(&scenario.actions[observed], session)?;
                require_after_variable_source(graph, variables, task, observed)?;
            }
            InvariantKind::CanonicalContinueKeepsRevision {
                first_revision,
                retry_revision,
            } => {
                if first_revision == retry_revision {
                    return Err(invalid_schema(
                        "semantic retry assertion requires two distinct captures",
                    ));
                }
                require_variable(variables, first_revision, VariableKind::IntentRevisionId)?;
                require_variable(variables, retry_revision, VariableKind::IntentRevisionId)?;
            }
            InvariantKind::StaleCasZeroWrites {
                attempt,
                before_observation,
                after_observation,
            } => {
                let attempt = require_step(graph, attempt)?;
                if !matches!(
                    scenario.actions[attempt].expectation,
                    ActionExpectation::TypedFailure {
                        kind: ExpectedFailureKind::StaleState,
                        ..
                    }
                ) {
                    return Err(ContractError::new(
                        ContractErrorKind::InvalidExpectation,
                        "stale-zero-write invariant requires a stale_state typed failure",
                    ));
                }
                let before = require_observation_entity(
                    scenario,
                    graph,
                    before_observation,
                    crate::ObservationSource::Runtime,
                    "task_semantic_state",
                )?;
                let after = require_observation_entity(
                    scenario,
                    graph,
                    after_observation,
                    crate::ObservationSource::Runtime,
                    "task_semantic_state",
                )?;
                require_any_observer_session_builtin(&scenario.actions[before])?;
                require_any_observer_session_builtin(&scenario.actions[after])?;
                if before >= attempt
                    || attempt >= after
                    || attempt != before + 1
                    || after != attempt + 1
                    || !graph.depends_on(attempt, before)
                    || !graph.depends_on(after, attempt)
                {
                    return Err(ContractError::new(
                        ContractErrorKind::ForwardReference,
                        "stale zero-write observations must causally surround the attempt",
                    ));
                }
                let ActionKind::Observe {
                    source: before_source,
                    selector: before_selector,
                } = &scenario.actions[before].action
                else {
                    unreachable!()
                };
                let ActionKind::Observe {
                    source: after_source,
                    selector: after_selector,
                } = &scenario.actions[after].action
                else {
                    unreachable!()
                };
                if before_source != after_source || before_selector != after_selector {
                    return Err(invalid_schema(
                        "stale zero-write observations must use the same fixed selector",
                    ));
                }
            }
            InvariantKind::OpenEpisodeHasNoCandidate {
                episode,
                episode_observation,
                candidate_observation,
            } => {
                require_variable(variables, episode, VariableKind::WorkEpisodeId)?;
                let episode_observation = require_observation_entity(
                    scenario,
                    graph,
                    episode_observation,
                    crate::ObservationSource::Runtime,
                    "work_episode",
                )?;
                require_observer_identity_variable(
                    &scenario.actions[episode_observation],
                    episode,
                )?;
                require_after_variable_source(graph, variables, episode, episode_observation)?;
                let candidate_observation = require_observation_entity(
                    scenario,
                    graph,
                    candidate_observation,
                    crate::ObservationSource::Index,
                    "index_candidate_for_episode",
                )?;
                require_observer_identity_variable(
                    &scenario.actions[candidate_observation],
                    episode,
                )?;
                require_after_variable_source(graph, variables, episode, candidate_observation)?;
            }
            InvariantKind::TurnStopIsIdempotent {
                first_stop,
                repeated_stop,
                first_candidate,
                repeated_candidate,
            } => {
                let first = require_hook_product_action(
                    scenario,
                    graph,
                    first_stop,
                    ProductAction::TurnStop,
                )?;
                let repeated = require_hook_product_action(
                    scenario,
                    graph,
                    repeated_stop,
                    ProductAction::TurnStop,
                )?;
                if first >= repeated || !graph.depends_on(repeated, first) {
                    return Err(ContractError::new(
                        ContractErrorKind::ForwardReference,
                        "repeated TurnStop must causally follow the first TurnStop",
                    ));
                }
                require_variable(variables, first_candidate, VariableKind::CandidateId)?;
                require_variable(variables, repeated_candidate, VariableKind::CandidateId)?;
                let first_candidate_source = variables[first_candidate.as_str()].source_step;
                let repeated_candidate_source = variables[repeated_candidate.as_str()].source_step;
                if !(first < first_candidate_source
                    && first_candidate_source < repeated
                    && repeated < repeated_candidate_source
                    && graph.depends_on(first_candidate_source, first)
                    && graph.depends_on(repeated, first_candidate_source)
                    && graph.depends_on(repeated_candidate_source, repeated))
                {
                    return Err(ContractError::new(
                        ContractErrorKind::ForwardReference,
                        "TurnStop Candidate captures must bracket the two Hook steps",
                    ));
                }
            }
            InvariantKind::CandidateSourceEpisode {
                response,
                candidate,
                episode,
            } => {
                require_variable(variables, candidate, VariableKind::CandidateId)?;
                require_variable(variables, episode, VariableKind::WorkEpisodeId)?;
                let response = require_mcp_method(scenario, graph, response, "candidate_get")?;
                if variables[candidate.as_str()].source_step != response
                    || variables[episode.as_str()].source_step != response
                {
                    return Err(invalid_schema(
                        "Candidate source assertion identities must come from its response",
                    ));
                }
            }
            InvariantKind::CandidateIsNotAutoInjected {
                candidate,
                response,
            } => {
                require_variable(variables, candidate, VariableKind::CandidateId)?;
                let context_step = require_mcp_method(scenario, graph, response, "task_context")?;
                let candidate_source = variables[candidate.as_str()].source_step;
                if context_step <= candidate_source
                    || !graph.depends_on(context_step, candidate_source)
                {
                    return Err(ContractError::new(
                        ContractErrorKind::ForwardReference,
                        "Candidate injection response must causally follow Candidate capture",
                    ));
                }
            }
            InvariantKind::ConfirmationIsAtomic {
                confirmation,
                response,
                ..
            } => {
                require_variable(variables, confirmation, VariableKind::ConfirmationId)?;
                let response = require_mcp_method(scenario, graph, response, "candidate_confirm")?;
                if variables[confirmation.as_str()].source_step != response {
                    return Err(invalid_schema(
                        "confirmation assertion must reference its capture response step",
                    ));
                }
            }
            InvariantKind::SessionEndDoesNotCloseEpisode {
                session_end,
                observation,
                episode,
            } => {
                require_variable(variables, episode, VariableKind::WorkEpisodeId)?;
                let session_end = require_hook_product_action(
                    scenario,
                    graph,
                    session_end,
                    ProductAction::SessionEnd,
                )?;
                let observation = require_observation_entity(
                    scenario,
                    graph,
                    observation,
                    crate::ObservationSource::Runtime,
                    "work_episode",
                )?;
                require_observer_identity_variable(&scenario.actions[observation], episode)?;
                if observation <= session_end || !graph.depends_on(observation, session_end) {
                    return Err(ContractError::new(
                        ContractErrorKind::ForwardReference,
                        "SessionEnd observation must causally follow SessionEnd",
                    ));
                }
            }
            InvariantKind::WorkingIntentHintHasNoGraphPath { response } => {
                let response = require_step(graph, response)?;
                if !matches!(
                    &scenario.actions[response].action,
                    ActionKind::McpRequest { method, .. }
                        if matches!(method.as_str(), "task_intent_update" | "task_context")
                ) {
                    return Err(invalid_schema(
                        "Working Intent Hint assertion requires task_intent_update or task_context",
                    ));
                }
            }
            InvariantKind::ArtifactFocusIsRequestScoped {
                focused_response,
                ordinary_response,
                resource,
                path,
            } => {
                let focus_index =
                    require_mcp_method(scenario, graph, focused_response, "task_artifact_focus")?;
                let context_index =
                    require_mcp_method(scenario, graph, ordinary_response, "task_context")?;
                if focus_index >= context_index || !graph.depends_on(context_index, focus_index) {
                    return Err(ContractError::new(
                        ContractErrorKind::ForwardReference,
                        "ordinary context observation must causally follow artifact focus",
                    ));
                }
                if !resources
                    .get(resource.as_str())
                    .is_some_and(|resource| resource.files.contains(path.as_str()))
                {
                    return Err(ContractError::new(
                        ContractErrorKind::DanglingReference,
                        "Artifact Focus assertion references an undeclared resource file",
                    ));
                }
            }
            InvariantKind::RawContentIsAbsent { probes } => {
                if probes.is_empty() || probes.len() > 3 {
                    return Err(invalid_schema(
                        "raw-content assertion must contain 1..=3 typed probes",
                    ));
                }
                let unique = probes
                    .iter()
                    .map(|probe| probe.kind)
                    .collect::<BTreeSet<RawContentKind>>();
                if unique.len() != probes.len() {
                    return Err(invalid_schema(
                        "raw-content assertion probe kinds must be unique",
                    ));
                }
                for probe in probes {
                    require_raw_probe(scenario, graph, probe)?;
                }
            }
        }
    }
    Ok(())
}

fn require_actor_role(
    actors: &HashMap<&str, ActorRole>,
    actor: &str,
    expected: ActorRole,
) -> Result<(), ContractError> {
    match actors.get(actor) {
        Some(actual) if *actual == expected => Ok(()),
        _ => Err(ContractError::new(
            ContractErrorKind::DanglingReference,
            "invariant references an unknown or incorrectly typed actor",
        )),
    }
}

fn require_after_variable_source(
    graph: &ActionGraph<'_>,
    variables: &HashMap<&str, VariableDefinition>,
    variable: &VariableName,
    step: usize,
) -> Result<(), ContractError> {
    let source = variables[variable.as_str()].source_step;
    if source < step && graph.depends_on(step, source) {
        Ok(())
    } else {
        Err(ContractError::new(
            ContractErrorKind::ForwardReference,
            "observation proof must causally follow its captured identity",
        ))
    }
}

fn require_observation_entity(
    scenario: &ScenarioDefinition,
    graph: &ActionGraph<'_>,
    step: &StepId,
    expected_source: crate::ObservationSource,
    expected_entity: &str,
) -> Result<usize, ContractError> {
    let index = require_observation_step(scenario, graph, step)?;
    let ActionKind::Observe { source, selector } = &scenario.actions[index].action else {
        unreachable!()
    };
    let TemplateValue::Object { fields } = selector else {
        return Err(invalid_schema(
            "typed observation selector must be an object template",
        ));
    };
    if *source != expected_source
        || !matches!(
            fields.get("entity"),
            Some(TemplateValue::String { value }) if value == expected_entity
        )
    {
        return Err(invalid_schema(
            "observation proof uses the wrong fixed source or entity",
        ));
    }
    Ok(index)
}

fn require_observer_session_builtin(
    action: &crate::ScenarioAction,
    session: &crate::ActorId,
) -> Result<(), ContractError> {
    let ActionKind::Observe {
        selector: TemplateValue::Object { fields },
        ..
    } = &action.action
    else {
        unreachable!()
    };
    if matches!(
        fields.get("session_key"),
        Some(TemplateValue::Builtin {
            builtin: SandboxBuiltin::ActorSessionKey { actor }
        }) if actor == session
    ) {
        Ok(())
    } else {
        Err(invalid_schema(
            "active-task observation requires the same Session-key builtin",
        ))
    }
}

fn require_any_observer_session_builtin(
    action: &crate::ScenarioAction,
) -> Result<(), ContractError> {
    let ActionKind::Observe {
        selector: TemplateValue::Object { fields },
        ..
    } = &action.action
    else {
        unreachable!()
    };
    if matches!(
        fields.get("session_key"),
        Some(TemplateValue::Builtin {
            builtin: SandboxBuiltin::ActorSessionKey { .. }
        })
    ) {
        Ok(())
    } else {
        Err(invalid_schema(
            "task semantic observation requires a Session-key builtin",
        ))
    }
}

fn require_observer_identity_variable(
    action: &crate::ScenarioAction,
    variable: &VariableName,
) -> Result<(), ContractError> {
    let ActionKind::Observe {
        selector: TemplateValue::Object { fields },
        ..
    } = &action.action
    else {
        unreachable!()
    };
    if matches!(
        fields.get("identity"),
        Some(TemplateValue::Variable { name }) if name == variable
    ) {
        Ok(())
    } else {
        Err(invalid_schema(
            "identity observation requires the matching runtime variable",
        ))
    }
}

fn require_hook_product_action(
    scenario: &ScenarioDefinition,
    graph: &ActionGraph<'_>,
    step: &StepId,
    expected: ProductAction,
) -> Result<usize, ContractError> {
    let index = require_step(graph, step)?;
    let ActionKind::HookEvent { event, .. } = &scenario.actions[index].action else {
        return Err(invalid_schema(
            "Hook invariant must reference a Hook event step",
        ));
    };
    if scenario.events.iter().any(|classification| {
        classification.event == *event
            && classification.classification == EventSupport::Supported
            && classification.product_action == Some(expected)
    }) {
        Ok(index)
    } else {
        Err(invalid_schema(
            "Hook invariant references the wrong classified product action",
        ))
    }
}

fn require_raw_probe(
    scenario: &ScenarioDefinition,
    graph: &ActionGraph<'_>,
    probe: &crate::RawContentProbe,
) -> Result<(), ContractError> {
    let index = require_step(graph, &probe.hook)?;
    let ActionKind::HookEvent { event, .. } = &scenario.actions[index].action else {
        return Err(invalid_schema(
            "raw-content probe must reference a Hook event step",
        ));
    };
    let product_action = scenario
        .events
        .iter()
        .find(|classification| classification.event == *event)
        .and_then(|classification| classification.product_action)
        .ok_or_else(|| invalid_schema("raw-content probe Hook is not supported"))?;
    let compatible = match probe.kind {
        RawContentKind::Prompt => product_action == ProductAction::PromptSubmit,
        RawContentKind::Transcript => true,
        RawContentKind::ToolOutput => product_action == ProductAction::PostToolUse,
    };
    if compatible {
        Ok(())
    } else {
        Err(invalid_schema(
            "raw-content probe lacks a compatible nonempty synthetic Hook field",
        ))
    }
}

fn require_variable(
    variables: &HashMap<&str, VariableDefinition>,
    variable: &VariableName,
    expected: VariableKind,
) -> Result<(), ContractError> {
    match variables.get(variable.as_str()) {
        Some(definition) if definition.kind == expected => Ok(()),
        Some(_) => Err(ContractError::new(
            ContractErrorKind::VariableTypeMismatch,
            format!("variable {variable} has the wrong type for its invariant"),
        )),
        None => Err(ContractError::new(
            ContractErrorKind::DanglingReference,
            format!("invariant references unknown variable {variable}"),
        )),
    }
}

fn require_step(graph: &ActionGraph<'_>, step: &StepId) -> Result<usize, ContractError> {
    graph.indices.get(step.as_str()).copied().ok_or_else(|| {
        ContractError::new(
            ContractErrorKind::DanglingReference,
            "invariant references an unknown step",
        )
    })
}

fn require_mcp_method(
    scenario: &ScenarioDefinition,
    graph: &ActionGraph<'_>,
    step: &StepId,
    expected_method: &str,
) -> Result<usize, ContractError> {
    let index = require_step(graph, step)?;
    if matches!(
        &scenario.actions[index].action,
        ActionKind::McpRequest { method, .. } if method.as_str() == expected_method
    ) && matches!(
        scenario.actions[index].expectation,
        ActionExpectation::Success
    ) {
        Ok(index)
    } else {
        Err(invalid_schema(
            "invariant response uses the wrong MCP operation or expectation",
        ))
    }
}

fn require_observation_step(
    scenario: &ScenarioDefinition,
    graph: &ActionGraph<'_>,
    step: &StepId,
) -> Result<usize, ContractError> {
    let index = require_step(graph, step)?;
    if matches!(scenario.actions[index].action, ActionKind::Observe { .. }) {
        Ok(index)
    } else {
        Err(invalid_schema(
            "observation proof must reference a read-only Observe step",
        ))
    }
}

fn preflight_version(value: &Value) -> Result<(), ContractError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_schema("scenario root must be an object"))?;
    match object.get("schema") {
        Some(Value::String(schema)) if schema == SCHEMA_NAME => {}
        Some(Value::String(_)) => {
            return Err(ContractError::new(
                ContractErrorKind::UnsupportedSchema,
                "unsupported scenario schema",
            ));
        }
        _ => return Err(invalid_schema("scenario schema must be a string")),
    }
    match object.get("version") {
        Some(Value::String(version)) if version == VERSION_V1 => Ok(()),
        Some(Value::String(_)) => Err(ContractError::new(
            ContractErrorKind::UnsupportedVersion,
            "unsupported scenario contract version",
        )),
        _ => Err(invalid_schema("scenario version must be a string")),
    }
}

fn reject_forged_expected(value: &Value) -> Result<(), ContractError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_schema("scenario root must be an object"))?;
    if FORBIDDEN_EXPECTED_FIELDS
        .iter()
        .any(|field| object.contains_key(*field))
    {
        Err(ContractError::new(
            ContractErrorKind::ForgedExpected,
            "scenario expected data must use typed invariant assertions",
        ))
    } else {
        Ok(())
    }
}

fn preflight_actions(value: &Value) -> Result<(), ContractError> {
    let Some(actions) = value.get("actions").and_then(Value::as_array) else {
        return Err(invalid_schema("scenario actions must be an array"));
    };
    for action in actions {
        let Some(action_type) = action
            .get("action")
            .and_then(|action| action.get("type"))
            .and_then(Value::as_str)
        else {
            return Err(invalid_schema(
                "each action must have a typed action object",
            ));
        };
        if !SUPPORTED_ACTION_TYPES.contains(&action_type) {
            return Err(ContractError::new(
                ContractErrorKind::UnsupportedAction,
                "scenario contains an unsupported action type",
            ));
        }
    }
    Ok(())
}

fn validate_raw_capacity(value: &Value) -> Result<(), ContractError> {
    fn visit(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), ContractError> {
        if depth > MAX_TEMPLATE_DEPTH {
            return Err(capacity("JSON nesting depth exceeds the contract limit"));
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_TEMPLATE_NODES {
            return Err(capacity("JSON node count exceeds the contract limit"));
        }
        match value {
            Value::String(value) => {
                if value.len() > MAX_STRING_BYTES {
                    return Err(capacity("a JSON string exceeds the contract limit"));
                }
            }
            Value::Array(values) => {
                if values.len() > MAX_ARRAY_ITEMS {
                    return Err(capacity("a JSON array exceeds the contract limit"));
                }
                for value in values {
                    visit(value, depth + 1, nodes)?;
                }
            }
            Value::Object(values) => {
                if values.len() > MAX_OBJECT_FIELDS {
                    return Err(capacity("a JSON object exceeds the contract limit"));
                }
                for (key, value) in values {
                    if key.len() > crate::MAX_WIRE_NAME_BYTES {
                        return Err(capacity("a JSON object key exceeds the contract limit"));
                    }
                    visit(value, depth + 1, nodes)?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
        Ok(())
    }

    let mut nodes = 0;
    visit(value, 0, &mut nodes)
}

fn capacity(message: impl Into<String>) -> ContractError {
    ContractError::new(ContractErrorKind::CapacityExceeded, message)
}

fn reject_hardcoded_domain_ids(value: &Value) -> Result<(), ContractError> {
    fn visit(value: &Value) -> Option<&'static str> {
        match value {
            Value::String(value) => find_domain_id_prefix(value),
            Value::Array(values) => values.iter().find_map(visit),
            Value::Object(values) => values
                .iter()
                .find_map(|(key, value)| find_domain_id_prefix(key).or_else(|| visit(value))),
            Value::Null | Value::Bool(_) | Value::Number(_) => None,
        }
    }

    if let Some(prefix) = visit(value) {
        Err(ContractError::new(
            ContractErrorKind::HardcodedDomainId,
            format!("scenario embeds a canonical {prefix} domain identity"),
        ))
    } else {
        Ok(())
    }
}

const DOMAIN_ID_PREFIXES: [&str; 28] = [
    "spc_", "rpo_", "rpg_", "ref_", "tsk_", "tss_", "xss_", "tir_", "sig_", "wep_", "wob_", "ckp_",
    "clm_", "bld_", "rec_", "psg_", "cnd_", "sub_", "cfm_", "asc_", "ctx_", "rev_", "evt_", "pub_",
    "evd_", "rvw_", "cnf_", "rsl_",
];

fn find_domain_id_prefix(value: &str) -> Option<&'static str> {
    for prefix in DOMAIN_ID_PREFIXES {
        for (start, _) in value.match_indices(prefix) {
            if start > 0
                && (value.as_bytes()[start - 1].is_ascii_alphanumeric()
                    || value.as_bytes()[start - 1] == b'_')
            {
                continue;
            }
            let uuid_start = start + prefix.len();
            let Some(uuid_end) = uuid_start.checked_add(36) else {
                continue;
            };
            let Some(raw_uuid) = value.get(uuid_start..uuid_end) else {
                continue;
            };
            if value
                .as_bytes()
                .get(uuid_end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                continue;
            }
            let Ok(uuid) = uuid::Uuid::parse_str(raw_uuid) else {
                continue;
            };
            if uuid.get_version() == Some(Version::Random)
                && uuid.get_variant() == Variant::RFC4122
                && uuid.hyphenated().to_string() == raw_uuid
            {
                return Some(prefix);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DOMAIN_ID_PREFIXES, find_domain_id_prefix, validate_raw_capacity};

    #[test]
    fn domain_id_prefix_table_covers_current_uuid_types_and_legacy_repository_ids() {
        let domain_ids = include_str!("../../domain/src/ids.rs");
        let declarations = domain_ids
            .split("#[cfg(test)]")
            .next()
            .expect("domain ID declarations precede tests");
        assert_eq!(
            declarations.match_indices("opaque_id!(").count() + 2,
            DOMAIN_ID_PREFIXES.len(),
            "the scenario denylist must cover current IDs plus legacy Repository UUIDs"
        );
        assert_eq!(DOMAIN_ID_PREFIXES.len(), 28);
        for prefix in DOMAIN_ID_PREFIXES {
            if prefix != "rpo_" {
                assert!(
                    declarations.contains(&format!("\"{prefix}\"")),
                    "missing current domain ID prefix {prefix}"
                );
            }
            let value = format!("{prefix}123e4567-e89b-42d3-a456-426614174000");
            assert_eq!(find_domain_id_prefix(&value), Some(prefix));
        }
    }

    #[test]
    fn domain_id_scan_does_not_flag_business_text() {
        for value in [
            "tsk_documentation",
            "rpg_feature",
            "rpg_not-a-uuid",
            "rev_parse",
            "ctx_menu",
            "a_tsk_123e4567-e89b-42d3-a456-426614174000",
            "tsk_123e4567-e89b-12d3-a456-426614174000",
            "TSK_123e4567-e89b-42d3-a456-426614174000",
        ] {
            assert_eq!(find_domain_id_prefix(value), None, "{value}");
        }
    }

    #[test]
    fn raw_capacity_counts_nested_nodes() {
        assert!(validate_raw_capacity(&json!({"safe": [1, 2, 3]})).is_ok());
    }
}
