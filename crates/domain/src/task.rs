use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{
    ContextId, Error, ErrorKind, Result, SpaceId, TaskId, TaskIntentRevisionId, TaskSessionId,
};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn require_text(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    Ok(())
}

fn require_text_items(values: &[String], field: &str) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        require_text(value, &format!("{field}[{index}]"))?;
    }
    Ok(())
}

fn require_unique<T>(values: &[T], field: &str) -> Result<()>
where
    T: Eq + std::hash::Hash,
{
    let mut seen = HashSet::with_capacity(values.len());
    if values.iter().any(|value| !seen.insert(value)) {
        return Err(invalid(format!("{field} must not contain duplicates")));
    }
    Ok(())
}

fn validate_text_set(values: &[String], field: &str) -> Result<()> {
    require_text_items(values, field)?;
    require_unique(values, field)
}

/// A structured, revisable understanding of the current engineering task.
///
/// A Task Intent deliberately has no Context Space identity. Space relevance is
/// represented separately by zero or more [`TaskSpaceAssociation`] values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIntent {
    pub task_id: TaskId,
    pub goal: String,
    pub desired_change: String,
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub domains: Vec<String>,
    pub platforms: Vec<String>,
    pub constraints: Vec<String>,
    pub acceptance_conditions: Vec<String>,
    pub artifacts: Vec<String>,
    pub interfaces: Vec<String>,
    pub unknowns: Vec<String>,
}

impl TaskIntent {
    /// Validates that the Task has a meaningful target and unambiguous lists.
    ///
    /// Empty lists are valid because an early, provisional Task Intent may know
    /// only its target. Entries that are present must be non-empty and unique.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required target text is empty or
    /// a structured list contains an empty or duplicate item.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.goal, "task_intent.goal")?;
        require_text(&self.desired_change, "task_intent.desired_change")?;
        validate_text_set(&self.in_scope, "task_intent.in_scope")?;
        validate_text_set(&self.out_of_scope, "task_intent.out_of_scope")?;
        validate_text_set(&self.domains, "task_intent.domains")?;
        validate_text_set(&self.platforms, "task_intent.platforms")?;
        validate_text_set(&self.constraints, "task_intent.constraints")?;
        validate_text_set(
            &self.acceptance_conditions,
            "task_intent.acceptance_conditions",
        )?;
        validate_text_set(&self.artifacts, "task_intent.artifacts")?;
        validate_text_set(&self.interfaces, "task_intent.interfaces")?;
        validate_text_set(&self.unknowns, "task_intent.unknowns")
    }
}

/// External Agent coordinates used only to locate a local Task Session.
///
/// The locator is deliberately not a domain identifier. Reusing the same
/// external session key under a different Agent kind denotes a different
/// locator, and neither component establishes durable knowledge identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSessionLocator {
    pub agent_kind: String,
    pub external_session_id: String,
}

impl ExternalSessionLocator {
    /// Creates a validated locator from external Agent coordinates.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when either coordinate is empty.
    pub fn new(
        agent_kind: impl Into<String>,
        external_session_id: impl Into<String>,
    ) -> Result<Self> {
        let locator = Self {
            agent_kind: agent_kind.into(),
            external_session_id: external_session_id.into(),
        };
        locator.validate()?;
        Ok(locator)
    }

    /// Validates the external coordinates without treating them as Task identity.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when either coordinate is empty.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.agent_kind, "external_session_locator.agent_kind")?;
        require_text(
            &self.external_session_id,
            "external_session_locator.external_session_id",
        )
    }
}

/// One immutable version of a Task's structured intent.
///
/// Task Intent revisions form a single local parent chain. The initial revision
/// has no parent; every successor names the immediately preceding revision.
/// Space and Workspace routing are intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIntentRevision {
    pub revision_id: TaskIntentRevisionId,
    pub parent_revision_id: Option<TaskIntentRevisionId>,
    pub intent: TaskIntent,
}

impl TaskIntentRevision {
    /// Creates the first validated revision for a Task.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the Task Intent is invalid.
    pub fn initial(intent: TaskIntent) -> Result<Self> {
        intent.validate()?;
        Ok(Self {
            revision_id: TaskIntentRevisionId::new(),
            parent_revision_id: None,
            intent,
        })
    }

    /// Creates a validated successor to an existing Task Intent revision.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the new Intent is invalid or
    /// belongs to a different Task.
    pub fn successor(parent: &Self, intent: TaskIntent) -> Result<Self> {
        parent.validate()?;
        intent.validate()?;
        if intent.task_id != parent.task_id() {
            return Err(invalid(
                "task_intent_revision successor must belong to the parent task",
            ));
        }
        Ok(Self {
            revision_id: TaskIntentRevisionId::new(),
            parent_revision_id: Some(parent.revision_id),
            intent,
        })
    }

    /// Returns the Task whose understanding this revision records.
    #[must_use]
    pub const fn task_id(&self) -> TaskId {
        self.intent.task_id
    }

    /// Validates this revision without consulting a Session history.
    ///
    /// Parent existence and chain position are Session-level invariants checked
    /// by [`TaskSessionSnapshot::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an invalid Intent or self-parent.
    pub fn validate(&self) -> Result<()> {
        self.intent.validate()?;
        if self.parent_revision_id == Some(self.revision_id) {
            return Err(invalid("task_intent_revision cannot name itself as parent"));
        }
        Ok(())
    }
}

/// Complete local runtime snapshot for one Agent Task Session.
///
/// A Task Session owns exactly one Task and a non-empty, linear Task Intent
/// revision history. Task Signals describe the engineering scene but cannot
/// route the Session or its Intent to a Context Space.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSessionSnapshot {
    pub task_session_id: TaskSessionId,
    pub task_id: TaskId,
    pub external_session_locator: ExternalSessionLocator,
    pub intent_revisions: Vec<TaskIntentRevision>,
    pub task_signals: Vec<TaskSignal>,
}

impl TaskSessionSnapshot {
    /// Starts a validated local Task Session with its initial Intent revision.
    ///
    /// Empty Task Signal collections are valid. A Workspace, when present, is
    /// represented only as a Task Signal and never as a Session route.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the locator, Intent, or signals
    /// are invalid.
    pub fn from_initial(
        external_session_locator: ExternalSessionLocator,
        intent: TaskIntent,
        task_signals: Vec<TaskSignal>,
    ) -> Result<Self> {
        external_session_locator.validate()?;
        TaskSignal::validate_collection(&task_signals)?;
        let task_id = intent.task_id;
        let initial_revision = TaskIntentRevision::initial(intent)?;
        let snapshot = Self {
            task_session_id: TaskSessionId::new(),
            task_id,
            external_session_locator,
            intent_revisions: vec![initial_revision],
            task_signals,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Returns the current Task Intent revision.
    ///
    /// A valid snapshot always has a current revision. `None` is possible only
    /// for an unvalidated value obtained through direct construction.
    #[must_use]
    pub fn current_intent_revision(&self) -> Option<&TaskIntentRevision> {
        self.intent_revisions.last()
    }

    /// Appends a validated successor as the Session's new current Intent.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the existing Session is invalid,
    /// the Intent is invalid, or the Intent belongs to another Task.
    pub fn append_intent(&mut self, intent: TaskIntent) -> Result<TaskIntentRevisionId> {
        self.validate()?;
        let parent = self.current_intent_revision().ok_or_else(|| {
            invalid("task_session.intent_revisions must contain an initial revision")
        })?;
        let revision = TaskIntentRevision::successor(parent, intent)?;
        let revision_id = revision.revision_id;
        self.intent_revisions.push(revision);
        Ok(revision_id)
    }

    /// Validates the Session boundary and complete Task Intent parent chain.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the locator or signals are
    /// invalid, the chain is empty or disconnected, a revision is repeated, or
    /// any revision belongs to another Task.
    pub fn validate(&self) -> Result<()> {
        self.external_session_locator.validate()?;
        TaskSignal::validate_collection(&self.task_signals)?;
        if self.intent_revisions.is_empty() {
            return Err(invalid(
                "task_session.intent_revisions must contain an initial revision",
            ));
        }

        let mut revision_ids = HashSet::with_capacity(self.intent_revisions.len());
        let mut expected_parent = None;
        for (index, revision) in self.intent_revisions.iter().enumerate() {
            revision.validate()?;
            if revision.task_id() != self.task_id {
                return Err(invalid(format!(
                    "task_session.intent_revisions[{index}] must belong to the session task"
                )));
            }
            if !revision_ids.insert(revision.revision_id) {
                return Err(invalid(
                    "task_session.intent_revisions must not repeat a revision",
                ));
            }
            if revision.parent_revision_id != expected_parent {
                return Err(invalid(format!(
                    "task_session.intent_revisions[{index}] must continue the parent chain"
                )));
            }
            expected_parent = Some(revision.revision_id);
        }
        Ok(())
    }
}

/// Kinds of observable input that can inform an engineering Task.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSignalKind {
    Prompt,
    Workspace,
    Repository,
    File,
    Symbol,
    Diff,
    Api,
    Schema,
    Test,
}

/// One observable input about the current Task.
///
/// A Workspace signal describes a code location or repository scene only; it
/// cannot identify or select a Context Space.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSignal {
    pub kind: TaskSignalKind,
    pub content: String,
}

impl TaskSignal {
    /// Validates one Task Signal.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the signal content is empty.
    pub fn validate(&self) -> Result<()> {
        require_text(&self.content, "task_signal.content")
    }

    /// Validates a Task's signal collection, rejecting duplicate observations.
    ///
    /// The empty collection is valid. Signal order and multiplicity do not carry
    /// domain meaning, so exact duplicates are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for invalid or duplicate signals.
    pub fn validate_collection(signals: &[Self]) -> Result<()> {
        for signal in signals {
            signal.validate()?;
        }
        require_unique(signals, "task_signals")
    }
}

/// Derived, explainable relevance between a Task and one Context Space.
///
/// Associations are task-local query results rather than durable knowledge
/// facts. A Task can have zero or many associations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpaceAssociation {
    pub task_id: TaskId,
    pub space_id: SpaceId,
    pub score: f64,
    pub matched_intent_fields: Vec<String>,
    pub matched_artifacts: Vec<String>,
    pub matched_contexts: Vec<ContextId>,
    pub relation_paths: Vec<Vec<String>>,
    pub reasons: Vec<String>,
}

impl TaskSpaceAssociation {
    /// Validates confidence and the match evidence that explains it.
    ///
    /// A zero score is not an association; callers represent that outcome by
    /// omitting the Space from the Task's association collection.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the score is not finite and in
    /// `(0, 1]`, an explanation is absent, or match evidence is malformed.
    pub fn validate(&self) -> Result<()> {
        if !self.score.is_finite() || self.score <= 0.0 || self.score > 1.0 {
            return Err(invalid(
                "task_space_association.score must be finite and in (0, 1]",
            ));
        }
        validate_text_set(
            &self.matched_intent_fields,
            "task_space_association.matched_intent_fields",
        )?;
        validate_text_set(
            &self.matched_artifacts,
            "task_space_association.matched_artifacts",
        )?;
        require_unique(
            &self.matched_contexts,
            "task_space_association.matched_contexts",
        )?;
        for (index, path) in self.relation_paths.iter().enumerate() {
            if path.is_empty() {
                return Err(invalid(format!(
                    "task_space_association.relation_paths[{index}] must not be empty"
                )));
            }
            validate_text_set(
                path,
                &format!("task_space_association.relation_paths[{index}]"),
            )?;
        }
        require_unique(
            &self.relation_paths,
            "task_space_association.relation_paths",
        )?;
        if self.reasons.is_empty() {
            return Err(invalid(
                "task_space_association.reasons must contain at least one confidence basis",
            ));
        }
        validate_text_set(&self.reasons, "task_space_association.reasons")
    }

    /// Validates all Space associations owned by one Task.
    ///
    /// Zero associations and multiple distinct Space associations are valid.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when an association belongs to a
    /// different Task, repeats a Space, or is itself invalid.
    pub fn validate_collection(task_id: TaskId, associations: &[Self]) -> Result<()> {
        let mut spaces = HashSet::with_capacity(associations.len());
        for association in associations {
            if association.task_id != task_id {
                return Err(invalid(
                    "task_space_associations must belong to the same task",
                ));
            }
            if !spaces.insert(association.space_id) {
                return Err(invalid("task_space_associations must not repeat a space"));
            }
            association.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        ExternalSessionLocator, TaskIntent, TaskIntentRevision, TaskSessionSnapshot, TaskSignal,
        TaskSignalKind, TaskSpaceAssociation,
    };
    use crate::{ContextId, ErrorKind, SpaceId, TaskId};

    fn intent() -> TaskIntent {
        intent_for(TaskId::new())
    }

    fn intent_for(task_id: TaskId) -> TaskIntent {
        TaskIntent {
            task_id,
            goal: "Make task-first retrieval possible".to_owned(),
            desired_change: "Infer relevant knowledge from the current task".to_owned(),
            in_scope: vec!["Task domain primitives".to_owned()],
            out_of_scope: vec!["Workspace routing".to_owned()],
            domains: vec!["context retrieval".to_owned()],
            platforms: vec!["fe".to_owned()],
            constraints: vec!["No Space prerequisite".to_owned()],
            acceptance_conditions: vec!["One Task can match many Spaces".to_owned()],
            artifacts: vec!["SearchResult".to_owned()],
            interfaces: vec!["search-v2".to_owned()],
            unknowns: vec!["Historical compatibility limits".to_owned()],
        }
    }

    fn locator(external_session_id: &str) -> ExternalSessionLocator {
        ExternalSessionLocator::new("codex", external_session_id).expect("valid locator")
    }

    fn assert_has_no_route_fields(value: &Value) {
        match value {
            Value::Object(fields) => {
                for (field, nested) in fields {
                    assert!(
                        !field.contains("space") && !field.contains("workspace"),
                        "serialized Task runtime field must not route through {field}"
                    );
                    assert_has_no_route_fields(nested);
                }
            }
            Value::Array(values) => {
                for value in values {
                    assert_has_no_route_fields(value);
                }
            }
            _ => {}
        }
    }

    fn association(task_id: TaskId, space_id: SpaceId) -> TaskSpaceAssociation {
        TaskSpaceAssociation {
            task_id,
            space_id,
            score: 0.85,
            matched_intent_fields: vec!["desired_outcome".to_owned()],
            matched_artifacts: vec!["symbol:SearchResult".to_owned()],
            matched_contexts: vec![ContextId::new()],
            relation_paths: vec![vec![
                "symbol:SearchResult".to_owned(),
                "api:search-v2".to_owned(),
            ]],
            reasons: vec!["The changed symbol consumes the matched API".to_owned()],
        }
    }

    #[test]
    fn task_intent_rejects_an_empty_target() {
        let mut value = intent();
        value.goal = "  ".to_owned();

        let error = value.validate().expect_err("empty goal must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.message().contains("task_intent.goal"));
    }

    #[test]
    fn task_intent_rejects_duplicate_structured_values() {
        let mut value = intent();
        value.domains.push(value.domains[0].clone());

        let error = value.validate().expect_err("duplicate domain must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.message().contains("task_intent.domains"));
    }

    #[test]
    fn task_session_starts_with_one_parentless_intent_revision() {
        let intent = intent();
        let task_id = intent.task_id;
        let session = TaskSessionSnapshot::from_initial(locator("session-a"), intent, vec![])
            .expect("valid initial Task Session");

        assert_eq!(session.task_id, task_id);
        assert_eq!(session.intent_revisions.len(), 1);
        assert_eq!(
            session
                .current_intent_revision()
                .expect("initial revision")
                .parent_revision_id,
            None
        );
        assert!(session.validate().is_ok());
    }

    #[test]
    fn task_intent_revisions_form_a_linear_parent_chain() {
        let task_id = TaskId::new();
        let mut session =
            TaskSessionSnapshot::from_initial(locator("session-a"), intent_for(task_id), vec![])
                .expect("valid initial Task Session");
        let initial_id = session.intent_revisions[0].revision_id;

        let mut second_intent = intent_for(task_id);
        second_intent.unknowns.clear();
        let second_id = session
            .append_intent(second_intent)
            .expect("valid successor");

        assert_eq!(session.intent_revisions[1].revision_id, second_id);
        assert_eq!(
            session.intent_revisions[1].parent_revision_id,
            Some(initial_id)
        );
        assert!(session.validate().is_ok());
    }

    #[test]
    fn task_session_rejects_mixed_tasks() {
        let mut session = TaskSessionSnapshot::from_initial(locator("session-a"), intent(), vec![])
            .expect("valid initial Task Session");
        let parent = session.intent_revisions[0].clone();

        let error = TaskIntentRevision::successor(&parent, intent())
            .expect_err("mixed Task successor must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.message().contains("parent task"));

        session.intent_revisions.push(TaskIntentRevision {
            revision_id: crate::TaskIntentRevisionId::new(),
            parent_revision_id: Some(parent.revision_id),
            intent: intent(),
        });
        let error = session
            .validate()
            .expect_err("mixed Task history must fail");
        assert!(error.message().contains("session task"));
    }

    #[test]
    fn task_session_rejects_duplicate_or_disconnected_revisions() {
        let mut duplicate =
            TaskSessionSnapshot::from_initial(locator("session-a"), intent(), vec![])
                .expect("valid initial Task Session");
        duplicate
            .intent_revisions
            .push(duplicate.intent_revisions[0].clone());
        let error = duplicate
            .validate()
            .expect_err("duplicate revision must fail");
        assert!(error.message().contains("must not repeat a revision"));

        let task_id = TaskId::new();
        let mut disconnected =
            TaskSessionSnapshot::from_initial(locator("session-b"), intent_for(task_id), vec![])
                .expect("valid initial Task Session");
        disconnected.intent_revisions.push(TaskIntentRevision {
            revision_id: crate::TaskIntentRevisionId::new(),
            parent_revision_id: None,
            intent: intent_for(task_id),
        });
        let error = disconnected
            .validate()
            .expect_err("disconnected revision must fail");
        assert!(error.message().contains("continue the parent chain"));
    }

    #[test]
    fn task_session_rejects_empty_intent_or_external_locator_boundaries() {
        let mut empty_intent = intent();
        empty_intent.desired_change = "  ".to_owned();
        let error = TaskSessionSnapshot::from_initial(locator("session-a"), empty_intent, vec![])
            .expect_err("empty Intent must fail");
        assert!(error.message().contains("task_intent.desired_change"));

        for (agent_kind, external_session_id, field) in [
            (" ", "session-a", "agent_kind"),
            ("codex", "\t", "external_session_id"),
        ] {
            let error = ExternalSessionLocator::new(agent_kind, external_session_id)
                .expect_err("empty locator coordinate must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.message().contains(field));
        }
    }

    #[test]
    fn one_workspace_can_supply_signals_to_independent_task_sessions() {
        let workspace_signal = TaskSignal {
            kind: TaskSignalKind::Workspace,
            content: "/work/shared-repository".to_owned(),
        };
        let first = TaskSessionSnapshot::from_initial(
            locator("session-a"),
            intent(),
            vec![workspace_signal.clone()],
        )
        .expect("first Task Session");
        let second = TaskSessionSnapshot::from_initial(
            locator("session-b"),
            intent(),
            vec![workspace_signal],
        )
        .expect("second Task Session");

        assert_ne!(first.task_session_id, second.task_session_id);
        assert_ne!(first.task_id, second.task_id);
        assert_ne!(
            first.external_session_locator,
            second.external_session_locator
        );
        assert_eq!(first.task_signals, second.task_signals);
    }

    #[test]
    fn serialized_task_intent_revision_and_session_have_no_route_fields() {
        let session = TaskSessionSnapshot::from_initial(
            locator("session-a"),
            intent(),
            vec![TaskSignal {
                kind: TaskSignalKind::File,
                content: "src/search.tsx".to_owned(),
            }],
        )
        .expect("valid Task Session");
        let revision = serde_json::to_value(&session.intent_revisions[0])
            .expect("serialize Task Intent revision");
        let session = serde_json::to_value(session).expect("serialize Task Session");

        assert_has_no_route_fields(&revision);
        assert_has_no_route_fields(&session);
    }

    #[test]
    fn serialized_task_intent_has_no_space_route() {
        let value = serde_json::to_value(intent()).expect("serialize Task Intent");

        assert!(
            value
                .as_object()
                .unwrap()
                .keys()
                .all(|field| !field.contains("space") && !field.contains("workspace"))
        );
    }

    #[test]
    fn task_signal_collection_rejects_duplicates_and_empty_content() {
        let signal = TaskSignal {
            kind: TaskSignalKind::File,
            content: "src/search.tsx".to_owned(),
        };
        let error = TaskSignal::validate_collection(&[signal.clone(), signal])
            .expect_err("duplicate signals must fail");
        assert!(error.message().contains("must not contain duplicates"));

        let error = TaskSignal {
            kind: TaskSignalKind::Prompt,
            content: " ".to_owned(),
        }
        .validate()
        .expect_err("empty signal content must fail");
        assert!(error.message().contains("task_signal.content"));
    }

    #[test]
    fn workspace_is_only_a_code_location_signal() {
        let signal = TaskSignal {
            kind: TaskSignalKind::Workspace,
            content: "/work/search".to_owned(),
        };

        assert!(signal.validate().is_ok());
        assert_eq!(
            serde_json::to_value(signal).expect("serialize Workspace signal"),
            json!({"kind": "workspace", "content": "/work/search"})
        );
    }

    #[test]
    fn task_space_association_rejects_invalid_confidence_or_basis() {
        for invalid_score in [f64::NAN, f64::INFINITY, -0.1, 0.0, 1.01] {
            let mut value = association(TaskId::new(), SpaceId::new());
            value.score = invalid_score;
            let error = value.validate().expect_err("invalid score must fail");
            assert!(error.message().contains("score"));
        }

        let mut value = association(TaskId::new(), SpaceId::new());
        value.reasons.clear();
        let error = value
            .validate()
            .expect_err("confidence without an explanation must fail");
        assert!(error.message().contains("confidence basis"));

        let mut value = association(TaskId::new(), SpaceId::new());
        value.relation_paths = vec![Vec::new()];
        let error = value
            .validate()
            .expect_err("an empty relation path must fail");
        assert!(error.message().contains("relation_paths[0]"));
    }

    #[test]
    fn one_task_accepts_zero_or_multiple_space_associations() {
        let task_id = TaskId::new();
        assert!(TaskSpaceAssociation::validate_collection(task_id, &[]).is_ok());

        let associations = vec![
            association(task_id, SpaceId::new()),
            association(task_id, SpaceId::new()),
        ];
        assert!(TaskSpaceAssociation::validate_collection(task_id, &associations).is_ok());
    }

    #[test]
    fn association_collection_rejects_duplicate_spaces_and_mixed_tasks() {
        let task_id = TaskId::new();
        let space_id = SpaceId::new();
        let duplicate_space = vec![
            association(task_id, space_id),
            association(task_id, space_id),
        ];
        assert!(
            TaskSpaceAssociation::validate_collection(task_id, &duplicate_space)
                .expect_err("duplicate Space must fail")
                .message()
                .contains("repeat a space")
        );

        let mixed_tasks = vec![association(TaskId::new(), SpaceId::new())];
        assert!(
            TaskSpaceAssociation::validate_collection(task_id, &mixed_tasks)
                .expect_err("mixed Task IDs must fail")
                .message()
                .contains("same task")
        );
    }
}
