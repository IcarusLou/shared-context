use std::collections::{BTreeMap, BTreeSet};

use sctx_domain::{DomainProjection, EventId, ReducerDiagnostic, ReducerEvent, reduce};
use sctx_event_schema::{ParsedEvent, parse_event};
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
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build(blobs: &[TreeBlob]) -> BuildInput {
    let mut source_files = Vec::with_capacity(blobs.len());
    let mut reducer_events = Vec::<ReducerEvent>::new();
    let mut event_paths = BTreeMap::<EventId, Vec<String>>::new();
    let mut diagnostics = BTreeSet::new();

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
                    source_files.push(SourceFile {
                        path: blob.path.clone(),
                        blob_oid: blob.oid.clone(),
                        parse_status: "known".to_owned(),
                        event_id: Some(event_id.to_string()),
                        diagnostic_code: None,
                        diagnostic_message: None,
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
    }
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
