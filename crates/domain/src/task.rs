use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{ContextId, Error, ErrorKind, Result, SpaceId, TaskId};

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
    use serde_json::json;

    use super::{TaskIntent, TaskSignal, TaskSignalKind, TaskSpaceAssociation};
    use crate::{ContextId, ErrorKind, SpaceId, TaskId};

    fn intent() -> TaskIntent {
        TaskIntent {
            task_id: TaskId::new(),
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
