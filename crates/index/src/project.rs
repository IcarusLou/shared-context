use std::collections::{BTreeMap, BTreeSet};

use sctx_domain::{DomainProjection, EventId, ReducerDiagnostic, ReducerEvent, reduce};
use sctx_event_schema::{Event, EventPayload, ParsedEvent, parse_event};
use sha2::{Digest, Sha256};

use crate::git_tree::TreeBlob;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceFile {
    pub(crate) path: String,
    pub(crate) blob_oid: String,
    pub(crate) parse_status: String,
    pub(crate) event_id: Option<String>,
    pub(crate) diagnostic_code: Option<String>,
    pub(crate) diagnostic_message: Option<String>,
    pub(crate) content: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventImpact {
    pub(crate) path: String,
    pub(crate) space_id: Option<String>,
    definitions: BTreeSet<String>,
    references: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ProjectionDiagnostic {
    pub(crate) key: String,
    pub(crate) source_path: Option<String>,
    pub(crate) code: String,
    pub(crate) entity_id: String,
    pub(crate) event_ids_json: String,
    pub(crate) message: String,
}

#[derive(Debug)]
pub(crate) struct BuildInput {
    pub(crate) source_files: Vec<SourceFile>,
    pub(crate) projection: DomainProjection,
    pub(crate) diagnostics: Vec<ProjectionDiagnostic>,
    pub(crate) impacts: Vec<EventImpact>,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build(blobs: &[TreeBlob]) -> BuildInput {
    let mut source_files = Vec::with_capacity(blobs.len());
    let mut reducer_events = Vec::<ReducerEvent>::new();
    let mut event_paths = BTreeMap::<EventId, Vec<String>>::new();
    let mut diagnostics = BTreeSet::new();
    let mut impacts = Vec::new();

    for blob in blobs {
        if blob.path.starts_with("events/") {
            match parse_event(&blob.bytes) {
                Ok(ParsedEvent::Known(event)) => {
                    let event_id = event.event_id();
                    event_paths
                        .entry(event_id)
                        .or_default()
                        .push(blob.path.clone());
                    if let Some(reducer_event) = event.reducer_event() {
                        reducer_events.push(reducer_event);
                    }
                    impacts.push(event_impact(&blob.path, &event));
                    source_files.push(SourceFile {
                        path: blob.path.clone(),
                        blob_oid: blob.oid.clone(),
                        parse_status: "known".to_owned(),
                        event_id: Some(event_id.to_string()),
                        diagnostic_code: None,
                        diagnostic_message: None,
                        content: blob.bytes.clone(),
                    });
                }
                Ok(ParsedEvent::UnknownSchema(event)) => {
                    let code = event.diagnostic().code.as_str().to_owned();
                    let message = event.diagnostic().message.clone();
                    diagnostics.insert(diagnostic(
                        Some(&blob.path),
                        &code,
                        event.event_id().unwrap_or(&blob.path),
                        "[]",
                        &message,
                    ));
                    source_files.push(SourceFile {
                        path: blob.path.clone(),
                        blob_oid: blob.oid.clone(),
                        parse_status: "unknown_schema".to_owned(),
                        event_id: event.event_id().map(ToOwned::to_owned),
                        diagnostic_code: Some(code),
                        diagnostic_message: Some(message),
                        content: blob.bytes.clone(),
                    });
                }
                Err(error) => {
                    let code = "EVENT_PARSE_ERROR".to_owned();
                    let message = error.to_string();
                    diagnostics.insert(diagnostic(
                        Some(&blob.path),
                        &code,
                        &blob.path,
                        "[]",
                        &message,
                    ));
                    source_files.push(SourceFile {
                        path: blob.path.clone(),
                        blob_oid: blob.oid.clone(),
                        parse_status: "invalid".to_owned(),
                        event_id: None,
                        diagnostic_code: Some(code),
                        diagnostic_message: Some(message),
                        content: blob.bytes.clone(),
                    });
                }
            }
        } else if blob.path.starts_with("objects/") {
            let digest = blob.path.rsplit('/').next().unwrap_or_default();
            let actual = hex_digest(&blob.bytes);
            let expected_path =
                (digest.len() >= 2).then(|| format!("objects/sha256/{}/{digest}", &digest[..2]));
            let valid = digest.len() == 64
                && digest == actual
                && expected_path.as_deref() == Some(blob.path.as_str());
            let (status, code, message) = if valid {
                ("object_verified", None, None)
            } else {
                let code = "OBJECT_DIGEST_MISMATCH".to_owned();
                let message = format!(
                    "object path digest {digest:?} does not match blob content digest {actual}"
                );
                diagnostics.insert(diagnostic(
                    Some(&blob.path),
                    &code,
                    &blob.path,
                    "[]",
                    &message,
                ));
                ("object_invalid", Some(code), Some(message))
            };
            source_files.push(SourceFile {
                path: blob.path.clone(),
                blob_oid: blob.oid.clone(),
                parse_status: status.to_owned(),
                event_id: None,
                diagnostic_code: code,
                diagnostic_message: message,
                content: blob.bytes.clone(),
            });
        }
    }

    let projection = reduce(&reducer_events);
    for reducer_diagnostic in &projection.diagnostics {
        diagnostics.insert(from_reducer(reducer_diagnostic, &event_paths));
    }
    for source in &mut source_files {
        let quarantined = source
            .event_id
            .as_deref()
            .and_then(|id| id.parse::<EventId>().ok())
            .is_some_and(|id| projection.quarantined_event_ids.contains(&id));
        if quarantined {
            "quarantined".clone_into(&mut source.parse_status);
            source.diagnostic_code = Some("REDUCER_QUARANTINED".to_owned());
            source.diagnostic_message =
                Some("event was quarantined by the domain reducer".to_owned());
        }
    }
    source_files.sort_by(|left, right| left.path.cmp(&right.path));

    BuildInput {
        source_files,
        projection,
        diagnostics: diagnostics.into_iter().collect(),
        impacts,
    }
}

/// Computes the reverse-reference impact closure for changed event paths. The closure expands in
/// both directions: definitions reach old dangling referrers, while references reach their target
/// definitions. Duplicate definitions therefore pull every owner aggregate into the result.
pub(crate) fn impact_closure(
    old: &BuildInput,
    new: &BuildInput,
    changed_paths: &BTreeSet<String>,
) -> BTreeSet<String> {
    let facts: Vec<_> = old.impacts.iter().chain(&new.impacts).collect();
    let mut selected = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for (index, fact) in facts.iter().enumerate() {
        if changed_paths.contains(&fact.path) {
            selected.insert(index);
            identities.extend(fact.definitions.iter().cloned());
            identities.extend(fact.references.iter().cloned());
        }
    }
    loop {
        let mut expanded = false;
        for (index, fact) in facts.iter().enumerate() {
            if selected.contains(&index)
                || (fact.definitions.is_disjoint(&identities)
                    && fact.references.is_disjoint(&identities))
            {
                continue;
            }
            selected.insert(index);
            identities.extend(fact.definitions.iter().cloned());
            identities.extend(fact.references.iter().cloned());
            expanded = true;
        }
        if !expanded {
            break;
        }
    }
    selected
        .into_iter()
        .filter_map(|index| facts[index].space_id.clone())
        .collect()
}

/// Returns every Space whose effective domain projection changed. The caller uses this as a
/// conservative proof check for the independently computed reverse-reference closure.
pub(crate) fn changed_projection_spaces(old: &BuildInput, new: &BuildInput) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    ids.extend(old.projection.spaces.keys().map(ToString::to_string));
    ids.extend(new.projection.spaces.keys().map(ToString::to_string));
    ids.into_iter()
        .filter(|space_id| projection_slice(old, space_id) != projection_slice(new, space_id))
        .collect()
}

fn projection_slice(input: &BuildInput, space_id: &str) -> serde_json::Value {
    let space = input
        .projection
        .spaces
        .iter()
        .find(|(id, _)| id.to_string() == space_id)
        .map(|(_, value)| value);
    let candidates: Vec<_> = input
        .projection
        .semantic_conflict_candidates
        .iter()
        .filter(|candidate| candidate.space_id.to_string() == space_id)
        .collect();
    let conflicts: Vec<_> = input
        .projection
        .semantic_conflicts
        .values()
        .filter(|conflict| conflict.space_id.to_string() == space_id)
        .collect();
    let engineering_references: Vec<_> = input
        .projection
        .engineering_references
        .values()
        .filter(|reference| reference.space_id.to_string() == space_id)
        .collect();
    serde_json::json!({
        "space": space,
        "engineering_references": engineering_references,
        "semantic_conflict_candidates": candidates,
        "semantic_conflicts": conflicts,
    })
}

#[allow(clippy::too_many_lines)]
fn event_impact(path: &str, event: &Event) -> EventImpact {
    let mut definitions = BTreeSet::from([identity("event", &event.event_id())]);
    let mut references = BTreeSet::new();
    let space_id = match event.payload() {
        EventPayload::ContextCandidateCreated { candidate } => {
            definitions.insert(identity("candidate", &candidate.candidate_id));
            references.insert(identity("work_episode", &candidate.source_episode_id));
            None
        }
        EventPayload::SpaceCreated {
            space_id,
            intent_revision,
        } => {
            definitions.insert(identity("space", space_id));
            definitions.insert(identity("revision", &intent_revision.revision_id));
            add_references(
                &mut references,
                "revision",
                &intent_revision.parent_revision_ids,
            );
            Some(*space_id)
        }
        EventPayload::SpaceIntentRevisionAdded {
            space_id,
            intent_revision,
        } => {
            references.insert(identity("space", space_id));
            definitions.insert(identity("revision", &intent_revision.revision_id));
            add_references(
                &mut references,
                "revision",
                &intent_revision.parent_revision_ids,
            );
            Some(*space_id)
        }
        EventPayload::ContextRevisionAdded {
            space_id,
            context_id,
            revision,
        } => {
            references.insert(identity("space", space_id));
            definitions.insert(identity("context", context_id));
            definitions.insert(identity("revision", &revision.revision_id));
            for evidence in &revision.evidence {
                definitions.insert(identity("evidence", &evidence.evidence_id));
            }
            add_references(&mut references, "revision", &revision.parent_revision_ids);
            Some(*space_id)
        }
        EventPayload::ContextReviewed {
            space_id,
            context_id,
            review,
        } => {
            references.insert(identity("space", space_id));
            definitions.insert(identity("context", context_id));
            definitions.insert(identity("review", &review.review_id));
            references.insert(identity("revision", &review.revision_id));
            Some(*space_id)
        }
        EventPayload::ContextPublicationChanged {
            space_id,
            context_id,
            publication,
        } => {
            references.insert(identity("space", space_id));
            definitions.insert(identity("context", context_id));
            definitions.insert(identity("publication", &publication.publication_id));
            references.insert(identity("revision", &publication.revision_id));
            add_references(
                &mut references,
                "publication",
                &publication.previous_publication_ids,
            );
            add_references(&mut references, "event", &publication.review_event_ids);
            Some(*space_id)
        }
        EventPayload::SemanticConflictOpened { space_id, conflict } => {
            references.insert(identity("space", space_id));
            definitions.insert(identity("conflict", &conflict.conflict_id));
            for participant in &conflict.participants {
                references.insert(identity("context", &participant.context_id));
                references.insert(identity("revision", &participant.revision_id));
                references.insert(identity("publication", &participant.publication_id));
            }
            Some(*space_id)
        }
        EventPayload::SemanticConflictResolutionAdded {
            space_id,
            conflict_id,
            resolution,
        } => {
            references.insert(identity("space", space_id));
            references.insert(identity("conflict", conflict_id));
            definitions.insert(identity("resolution", &resolution.resolution_id));
            add_references(
                &mut references,
                "resolution",
                &resolution.previous_resolution_ids,
            );
            add_references(
                &mut references,
                "publication",
                &resolution.related_publication_ids,
            );
            for result in &resolution.results {
                references.insert(identity("context", &result.context_id));
                references.insert(identity("revision", &result.revision_id));
            }
            Some(*space_id)
        }
        EventPayload::EngineeringReferenceRecorded {
            context_id,
            revision_id,
            reference,
        } => {
            definitions.insert(identity("reference", &reference.reference_id));
            references.insert(identity("context", context_id));
            references.insert(identity("revision", revision_id));
            references.insert(identity("repository", &reference.repository_id));
            None
        }
    };
    EventImpact {
        path: path.to_owned(),
        space_id: space_id.map(|space_id| space_id.to_string()),
        definitions,
        references,
    }
}

fn identity(kind: &str, value: &impl ToString) -> String {
    format!("{kind}:{}", value.to_string())
}

fn add_references<T: ToString>(target: &mut BTreeSet<String>, kind: &str, values: &[T]) {
    target.extend(
        values
            .iter()
            .map(|value| format!("{kind}:{}", value.to_string())),
    );
}

fn from_reducer(
    diagnostic_value: &ReducerDiagnostic,
    event_paths: &BTreeMap<EventId, Vec<String>>,
) -> ProjectionDiagnostic {
    let code = enum_name(diagnostic_value.code);
    let event_ids: Vec<_> = diagnostic_value
        .event_ids
        .iter()
        .map(ToString::to_string)
        .collect();
    let event_ids_json = serde_json::to_string(&event_ids).expect("serializing IDs cannot fail");
    let paths: BTreeSet<_> = diagnostic_value
        .event_ids
        .iter()
        .flat_map(|event_id| event_paths.get(event_id).into_iter().flatten().cloned())
        .collect();
    let source_path = (paths.len() == 1).then(|| paths.first().cloned()).flatten();
    diagnostic(
        source_path.as_deref(),
        &code,
        &diagnostic_value.entity_id,
        &event_ids_json,
        &diagnostic_value.message,
    )
}

fn diagnostic(
    source_path: Option<&str>,
    code: &str,
    entity_id: &str,
    event_ids_json: &str,
    message: &str,
) -> ProjectionDiagnostic {
    let identity = format!(
        "{}\0{}\0{}\0{}\0{}",
        source_path.unwrap_or_default(),
        code,
        entity_id,
        event_ids_json,
        message
    );
    ProjectionDiagnostic {
        key: hex_digest(identity.as_bytes()),
        source_path: source_path.map(ToOwned::to_owned),
        code: code.to_owned(),
        entity_id: entity_id.to_owned(),
        event_ids_json: event_ids_json.to_owned(),
        message: message.to_owned(),
    }
}

fn enum_name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("serializing reducer enum cannot fail")
        .as_str()
        .expect("reducer enum serializes as a string")
        .to_owned()
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use sctx_event_schema::{Event, IntentSnapshot};

    use super::build;
    use crate::git_tree::TreeBlob;

    fn intent(title: &str) -> IntentSnapshot {
        IntentSnapshot {
            title: title.to_owned(),
            problem: "problem".to_owned(),
            desired_outcome: "outcome".to_owned(),
            in_scope: vec!["scope".to_owned()],
            out_of_scope: Vec::new(),
            acceptance_conditions: vec!["accepted".to_owned()],
            domain_terms: Vec::new(),
        }
    }

    #[test]
    fn shuffled_discovery_order_has_identical_reducer_output() {
        let first = Event::space_created(intent("first"), None).unwrap();
        let second = Event::space_created(intent("second"), None).unwrap();
        let mut blobs = vec![
            TreeBlob {
                path: "events/z/second.json".to_owned(),
                oid: "2".repeat(40),
                bytes: serde_json::to_vec(&second).unwrap(),
            },
            TreeBlob {
                path: "events/a/first.json".to_owned(),
                oid: "1".repeat(40),
                bytes: serde_json::to_vec(&first).unwrap(),
            },
        ];
        let forward = build(&blobs);
        blobs.reverse();
        let reversed = build(&blobs);

        assert_eq!(
            serde_json::to_value(&forward.projection).unwrap(),
            serde_json::to_value(&reversed.projection).unwrap()
        );
        assert_eq!(forward.source_files, reversed.source_files);
        assert_eq!(forward.diagnostics, reversed.diagnostics);
    }
}
