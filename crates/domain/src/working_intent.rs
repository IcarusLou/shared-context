use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, ErrorKind, Result};

/// Maximum UTF-8 bytes in the required goal or optional current direction.
pub const MAX_WORKING_INTENT_TEXT_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes in one structured list item.
pub const MAX_WORKING_INTENT_ITEM_BYTES: usize = 1024;
/// Maximum number of items in any one structured list field.
pub const MAX_WORKING_INTENT_ITEMS_PER_FIELD: usize = 64;
/// Maximum aggregate UTF-8 bytes across one complete Working Intent snapshot.
pub const MAX_WORKING_INTENT_TOTAL_BYTES: usize = 64 * 1024;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// The Agent's current, non-factual understanding of one Task.
///
/// Only `goal` is required on the wire. Every omitted structured field is an empty part of this
/// complete snapshot, not a request for the Agent to invent content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkingIntentSnapshot {
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_direction: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_conditions: Vec<String>,
    /// Non-authoritative retrieval clues; they do not prove that an Artifact exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_hints: Vec<String>,
    /// Non-authoritative retrieval clues; they do not prove an Interface contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interface_hints: Vec<String>,
    /// Questions the Agent naturally formed; absence never requests an investigation plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
}

impl WorkingIntentSnapshot {
    /// Creates a goal-only Working Intent snapshot.
    ///
    /// # Errors
    ///
    /// Rejects an empty or oversized goal.
    pub fn new(goal: impl Into<String>) -> Result<Self> {
        let snapshot = Self {
            goal: goal.into(),
            current_direction: None,
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            domains: Vec::new(),
            platforms: Vec::new(),
            constraints: Vec::new(),
            acceptance_conditions: Vec::new(),
            artifact_hints: Vec::new(),
            interface_hints: Vec::new(),
            open_questions: Vec::new(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Validates bounded, non-empty, unique structured content and explicit scope consistency.
    ///
    /// # Errors
    ///
    /// Rejects blank/oversized text, excessive list sizes, canonically duplicate entries,
    /// explicit in-scope/out-of-scope overlap, or an oversized complete snapshot.
    pub fn validate(&self) -> Result<()> {
        validate_text(
            &self.goal,
            "working_intent.goal",
            MAX_WORKING_INTENT_TEXT_BYTES,
        )?;
        if let Some(direction) = &self.current_direction {
            validate_text(
                direction,
                "working_intent.current_direction",
                MAX_WORKING_INTENT_TEXT_BYTES,
            )?;
        }
        for (field, values) in self.list_fields() {
            validate_list(values, field)?;
        }
        let in_scope = canonical_set(&self.in_scope);
        let out_of_scope = canonical_set(&self.out_of_scope);
        if let Some(overlap) = in_scope.intersection(&out_of_scope).next() {
            return Err(invalid(format!(
                "working_intent.in_scope and out_of_scope overlap: {overlap}"
            )));
        }
        if self.authoritative_text_bytes() > MAX_WORKING_INTENT_TOTAL_BYTES {
            return Err(invalid(format!(
                "Working Intent exceeds {MAX_WORKING_INTENT_TOTAL_BYTES} total UTF-8 bytes"
            )));
        }
        Ok(())
    }

    /// Returns a stable hash of bounded canonical semantics without changing authoritative text.
    ///
    /// Whitespace runs, letter case, and structured-list ordering are explicitly non-semantic.
    ///
    /// # Errors
    ///
    /// Returns validation errors before hashing malformed or unbounded input.
    pub fn canonical_semantic_hash(&self) -> Result<String> {
        self.validate()?;
        let bytes = serde_json::to_vec(&self.canonical())
            .map_err(|error| invalid(format!("serialize canonical Working Intent: {error}")))?;
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    /// Compares canonical semantics while preserving both authoritative snapshots verbatim.
    ///
    /// # Errors
    ///
    /// Returns validation errors from either snapshot.
    pub fn canonically_equals(&self, other: &Self) -> Result<bool> {
        self.validate()?;
        other.validate()?;
        Ok(self.canonical() == other.canonical())
    }

    fn list_fields(&self) -> [(&'static str, &[String]); 9] {
        [
            ("working_intent.in_scope", &self.in_scope),
            ("working_intent.out_of_scope", &self.out_of_scope),
            ("working_intent.domains", &self.domains),
            ("working_intent.platforms", &self.platforms),
            ("working_intent.constraints", &self.constraints),
            (
                "working_intent.acceptance_conditions",
                &self.acceptance_conditions,
            ),
            ("working_intent.artifact_hints", &self.artifact_hints),
            ("working_intent.interface_hints", &self.interface_hints),
            ("working_intent.open_questions", &self.open_questions),
        ]
    }

    fn authoritative_text_bytes(&self) -> usize {
        self.goal.len()
            + self.current_direction.as_ref().map_or(0, String::len)
            + self
                .list_fields()
                .into_iter()
                .flat_map(|(_, values)| values)
                .map(String::len)
                .sum::<usize>()
    }

    fn canonical(&self) -> CanonicalWorkingIntent {
        CanonicalWorkingIntent {
            goal: normalize(&self.goal),
            current_direction: self.current_direction.as_deref().map(normalize),
            in_scope: canonical_list(&self.in_scope),
            out_of_scope: canonical_list(&self.out_of_scope),
            domains: canonical_list(&self.domains),
            platforms: canonical_list(&self.platforms),
            constraints: canonical_list(&self.constraints),
            acceptance_conditions: canonical_list(&self.acceptance_conditions),
            artifact_hints: canonical_list(&self.artifact_hints),
            interface_hints: canonical_list(&self.interface_hints),
            open_questions: canonical_list(&self.open_questions),
        }
    }
}

#[derive(Eq, PartialEq, Serialize)]
struct CanonicalWorkingIntent {
    goal: String,
    current_direction: Option<String>,
    in_scope: Vec<String>,
    out_of_scope: Vec<String>,
    domains: Vec<String>,
    platforms: Vec<String>,
    constraints: Vec<String>,
    acceptance_conditions: Vec<String>,
    artifact_hints: Vec<String>,
    interface_hints: Vec<String>,
    open_questions: Vec<String>,
}

fn validate_text(value: &str, field: &str, max_bytes: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    if value.len() > max_bytes {
        return Err(invalid(format!("{field} exceeds {max_bytes} UTF-8 bytes")));
    }
    Ok(())
}

fn validate_list(values: &[String], field: &str) -> Result<()> {
    if values.len() > MAX_WORKING_INTENT_ITEMS_PER_FIELD {
        return Err(invalid(format!(
            "{field} exceeds {MAX_WORKING_INTENT_ITEMS_PER_FIELD} items"
        )));
    }
    let mut seen = HashSet::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        validate_text(
            value,
            &format!("{field}[{index}]"),
            MAX_WORKING_INTENT_ITEM_BYTES,
        )?;
        if !seen.insert(normalize(value)) {
            return Err(invalid(format!(
                "{field} must not contain canonically duplicate items"
            )));
        }
    }
    Ok(())
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn canonical_list(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| normalize(value))
        .collect::<Vec<_>>();
    values.sort_unstable();
    values
}

fn canonical_set(values: &[String]) -> HashSet<String> {
    values.iter().map(|value| normalize(value)).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn complete() -> WorkingIntentSnapshot {
        WorkingIntentSnapshot {
            goal: "Implement Search Results".to_owned(),
            current_direction: Some("Reuse the existing response contract".to_owned()),
            in_scope: vec!["FE rendering".to_owned(), "Compatibility".to_owned()],
            out_of_scope: vec!["Ranking model".to_owned()],
            domains: vec!["Search".to_owned()],
            platforms: vec!["FE".to_owned(), "iOS".to_owned()],
            constraints: vec!["No API break".to_owned()],
            acceptance_conditions: vec!["Old clients continue to work".to_owned()],
            artifact_hints: vec!["symbol:SearchResult".to_owned()],
            interface_hints: vec!["api:search-v2".to_owned()],
            open_questions: vec!["Does Android consume the fallback?".to_owned()],
        }
    }

    #[test]
    fn only_goal_is_required_and_optional_fields_default_to_empty() {
        let parsed: WorkingIntentSnapshot =
            serde_json::from_value(json!({"goal": "Fix search"})).unwrap();
        assert!(parsed.validate().is_ok());
        assert_eq!(parsed.current_direction, None);
        assert!(parsed.in_scope.is_empty());
        assert!(parsed.artifact_hints.is_empty());
        assert!(parsed.open_questions.is_empty());
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            json!({"goal": "Fix search"})
        );
    }

    #[test]
    fn validation_is_bounded_unique_and_scope_consistent() {
        assert!(WorkingIntentSnapshot::new("  ").is_err());
        let mut duplicate = complete();
        duplicate.domains = vec!["Search".to_owned(), " search ".to_owned()];
        assert!(duplicate.validate().is_err());
        let mut overlap = complete();
        overlap.out_of_scope.push(" fe   rendering ".to_owned());
        assert!(overlap.validate().is_err());
        let mut too_many = complete();
        too_many.constraints = (0..=MAX_WORKING_INTENT_ITEMS_PER_FIELD)
            .map(|index| format!("constraint {index}"))
            .collect();
        assert!(too_many.validate().is_err());
        let mut oversized = complete();
        oversized.artifact_hints = vec!["x".repeat(MAX_WORKING_INTENT_ITEM_BYTES + 1)];
        assert!(oversized.validate().is_err());
        let mut aggregate = complete();
        aggregate.artifact_hints = (0..MAX_WORKING_INTENT_ITEMS_PER_FIELD)
            .map(|index| format!("artifact-{index}-{}", "a".repeat(1000)))
            .collect();
        aggregate.interface_hints = (0..MAX_WORKING_INTENT_ITEMS_PER_FIELD)
            .map(|index| format!("interface-{index}-{}", "i".repeat(1000)))
            .collect();
        assert!(aggregate.validate().is_err());
    }

    #[test]
    fn canonical_hash_converges_nonsemantic_differences_without_rewriting_text() {
        let original = complete();
        let mut equivalent = original.clone();
        equivalent.goal = "  implement   SEARCH results  ".to_owned();
        equivalent.current_direction = Some("reuse THE existing response contract".to_owned());
        equivalent.in_scope.reverse();
        equivalent.platforms.reverse();
        equivalent.artifact_hints[0] = "SYMBOL:searchresult".to_owned();
        let authoritative_goal = equivalent.goal.clone();
        assert!(original.canonically_equals(&equivalent).unwrap());
        assert_eq!(
            original.canonical_semantic_hash().unwrap(),
            equivalent.canonical_semantic_hash().unwrap()
        );
        assert_eq!(equivalent.goal, authoritative_goal);

        equivalent
            .open_questions
            .push("Is telemetry affected?".to_owned());
        assert!(!original.canonically_equals(&equivalent).unwrap());
    }

    #[test]
    fn every_authoritative_dimension_changes_canonical_semantics() {
        let baseline = complete();
        let mut variants = Vec::new();

        let mut value = baseline.clone();
        value.goal.push_str(" now");
        variants.push(value);
        let mut value = baseline.clone();
        value.current_direction = Some("Choose a different implementation".to_owned());
        variants.push(value);
        for field in 0..9 {
            let mut value = baseline.clone();
            match field {
                0 => value.in_scope.push("Telemetry".to_owned()),
                1 => value.out_of_scope.push("Caching".to_owned()),
                2 => value.domains.push("Analytics".to_owned()),
                3 => value.platforms.push("Android".to_owned()),
                4 => value.constraints.push("Preserve latency".to_owned()),
                5 => value
                    .acceptance_conditions
                    .push("Telemetry remains stable".to_owned()),
                6 => value.artifact_hints.push("file:search.ts".to_owned()),
                7 => value.interface_hints.push("schema:search-v3".to_owned()),
                8 => value.open_questions.push("Is caching affected?".to_owned()),
                _ => unreachable!(),
            }
            variants.push(value);
        }
        let baseline_hash = baseline.canonical_semantic_hash().unwrap();
        assert!(variants.into_iter().all(|variant| {
            variant.canonical_semantic_hash().unwrap() != baseline_hash
                && !baseline.canonically_equals(&variant).unwrap()
        }));
    }

    #[test]
    fn serialized_hints_and_questions_never_claim_fact_evidence_or_routing() {
        let encoded = serde_json::to_value(complete()).unwrap();
        let object = encoded.as_object().unwrap();
        assert!(object.contains_key("artifact_hints"));
        assert!(object.contains_key("interface_hints"));
        assert!(object.contains_key("open_questions"));
        for forbidden in [
            "maturity",
            "evidence",
            "evidence_refs",
            "space_id",
            "workspace",
            "binding",
            "unknowns",
        ] {
            assert!(!object.contains_key(forbidden));
        }
        let encoded = serde_json::to_string(&Value::Object(object.clone())).unwrap();
        assert!(!encoded.contains("grounded"));
    }
}
