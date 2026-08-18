//! V1 JSON event envelope, generation, parsing, and semantic hashing.
//!
//! Generation methods accept drafts without IDs and assign every new identity
//! internally. Parsing is intentionally a separate capability: supported V1
//! input is validated strictly, while unknown schema versions are preserved as
//! diagnostic input and never returned as a projectable [`Event`].

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use sctx_domain::{
    Applicability, AutoInjectionBlocker, AutoInjectionEligibility, ConflictId, ConflictParticipant,
    ConflictResolution, ConflictResolutionDraft, ConflictResolutionResult, ContextGovernanceStatus,
    ContextId, ContextKind, ContextProjection, ContextRevision, ContextRevisionDraft,
    ContextSpaceProjection, DomainProjection, Error, ErrorKind, EventId, EvidenceId,
    EvidenceSnapshot, EvidenceSnapshotDraft, EvidenceType, IdParseError, IntentProjection,
    IntentRevision, IntentSnapshot, Publication, PublicationAction, PublicationDraft,
    PublicationId, ReducerDiagnostic, ReducerDiagnosticCode, ReducerEvent, ReducerPayload,
    ResolutionId, ResolutionOutcome, Result, Review, ReviewDraft, ReviewId, ReviewSummary,
    ReviewVerdict, RevisionId, RevisionLifecycle, RevisionProjection, SemanticConflict,
    SemanticConflictCandidate, SemanticConflictDraft, SemanticConflictOpenReason,
    SemanticConflictProjection, SemanticConflictStatus, SpaceId, reduce,
};

/// Immutable identifier for the bundled V1 JSON Schema.
pub const V1_SCHEMA_ID: &str = "https://shared-context.local/schemas/event-v1.schema.json";

/// Bundled JSON Schema text. Schema evolution adds a new file and constant.
pub const V1_JSON_SCHEMA: &str = include_str!("../../../schemas/event-v1.schema.json");

/// Schema versions that this parser can project.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SchemaVersion {
    #[serde(rename = "1")]
    V1,
}

impl SchemaVersion {
    /// Serialized schema-version value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "1",
        }
    }
}

/// Closed set of V1 event kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "space.created")]
    SpaceCreated,
    #[serde(rename = "space.intent_revision_added")]
    SpaceIntentRevisionAdded,
    #[serde(rename = "context.revision_added")]
    ContextRevisionAdded,
    #[serde(rename = "context.reviewed")]
    ContextReviewed,
    #[serde(rename = "context.publication_changed")]
    ContextPublicationChanged,
    #[serde(rename = "semantic_conflict.opened")]
    SemanticConflictOpened,
    #[serde(rename = "semantic_conflict.resolution_added")]
    SemanticConflictResolutionAdded,
}

impl EventType {
    /// Stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SpaceCreated => "space.created",
            Self::SpaceIntentRevisionAdded => "space.intent_revision_added",
            Self::ContextRevisionAdded => "context.revision_added",
            Self::ContextReviewed => "context.reviewed",
            Self::ContextPublicationChanged => "context.publication_changed",
            Self::SemanticConflictOpened => "semantic_conflict.opened",
            Self::SemanticConflictResolutionAdded => "semantic_conflict.resolution_added",
        }
    }
}

/// Extensible, explicitly non-authoritative development-location hints.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OriginHint {
    #[serde(flatten)]
    pub values: BTreeMap<String, Value>,
}

/// Extensible metadata that never participates in domain semantics or hashing.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Annotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_hint: Option<OriginHint>,
    #[serde(flatten)]
    pub additional: BTreeMap<String, Value>,
}

/// Authoritative V1 event payload. The enclosing [`Event`] owns envelope data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", deny_unknown_fields)]
pub enum EventPayload {
    #[serde(rename = "space.created")]
    SpaceCreated {
        space_id: SpaceId,
        intent_revision: IntentRevision,
    },
    #[serde(rename = "space.intent_revision_added")]
    SpaceIntentRevisionAdded {
        space_id: SpaceId,
        intent_revision: IntentRevision,
    },
    #[serde(rename = "context.revision_added")]
    ContextRevisionAdded {
        space_id: SpaceId,
        context_id: ContextId,
        revision: ContextRevision,
    },
    #[serde(rename = "context.reviewed")]
    ContextReviewed {
        space_id: SpaceId,
        context_id: ContextId,
        review: Review,
    },
    #[serde(rename = "context.publication_changed")]
    ContextPublicationChanged {
        space_id: SpaceId,
        context_id: ContextId,
        publication: Publication,
    },
    #[serde(rename = "semantic_conflict.opened")]
    SemanticConflictOpened {
        space_id: SpaceId,
        conflict: SemanticConflict,
    },
    #[serde(rename = "semantic_conflict.resolution_added")]
    SemanticConflictResolutionAdded {
        space_id: SpaceId,
        conflict_id: ConflictId,
        resolution: ConflictResolution,
    },
}

impl EventPayload {
    /// Event kind represented by this payload.
    #[must_use]
    pub const fn event_type(&self) -> EventType {
        match self {
            Self::SpaceCreated { .. } => EventType::SpaceCreated,
            Self::SpaceIntentRevisionAdded { .. } => EventType::SpaceIntentRevisionAdded,
            Self::ContextRevisionAdded { .. } => EventType::ContextRevisionAdded,
            Self::ContextReviewed { .. } => EventType::ContextReviewed,
            Self::ContextPublicationChanged { .. } => EventType::ContextPublicationChanged,
            Self::SemanticConflictOpened { .. } => EventType::SemanticConflictOpened,
            Self::SemanticConflictResolutionAdded { .. } => {
                EventType::SemanticConflictResolutionAdded
            }
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::SpaceCreated {
                intent_revision, ..
            } => {
                if !intent_revision.parent_revision_ids.is_empty() {
                    return Err(invalid(
                        "space.created intent_revision.parent_revision_ids must be empty",
                    ));
                }
                intent_revision.validate()
            }
            Self::SpaceIntentRevisionAdded {
                intent_revision, ..
            } => {
                if intent_revision.parent_revision_ids.is_empty() {
                    return Err(invalid(
                        "space.intent_revision_added must reference at least one parent revision",
                    ));
                }
                intent_revision.validate()
            }
            Self::ContextRevisionAdded { revision, .. } => revision.validate(),
            Self::ContextReviewed { review, .. } => review.validate(),
            Self::ContextPublicationChanged { publication, .. } => publication.validate(),
            Self::SemanticConflictOpened { conflict, .. } => conflict.validate(),
            Self::SemanticConflictResolutionAdded { resolution, .. } => resolution.validate(),
        }
    }
}

/// A supported, strictly validated immutable event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_field_names)]
pub struct Event {
    schema_version: SchemaVersion,
    event_id: EventId,
    #[serde(flatten)]
    payload: EventPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    annotations: Option<Annotations>,
}

#[derive(Deserialize)]
struct UnvalidatedEvent {
    schema_version: SchemaVersion,
    event_id: EventId,
    #[serde(flatten)]
    payload: EventPayload,
    #[serde(default)]
    annotations: Option<Annotations>,
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = UnvalidatedEvent::deserialize(deserializer)?;
        let event = Self {
            schema_version: wire.schema_version,
            event_id: wire.event_id,
            payload: wire.payload,
            annotations: wire.annotations,
        };
        event.validate().map_err(de::Error::custom)?;
        Ok(event)
    }
}

impl Event {
    fn generated(payload: EventPayload, annotations: Option<Annotations>) -> Result<Self> {
        let event = Self {
            schema_version: SchemaVersion::V1,
            event_id: EventId::new(),
            payload,
            annotations,
        };
        event.validate()?;
        Ok(event)
    }

    /// Creates a Space and its initial full Intent revision. All IDs are generated internally.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the Intent snapshot is incomplete.
    pub fn space_created(intent: IntentSnapshot, annotations: Option<Annotations>) -> Result<Self> {
        intent.validate()?;
        Self::generated(
            EventPayload::SpaceCreated {
                space_id: SpaceId::new(),
                intent_revision: IntentRevision {
                    revision_id: RevisionId::new(),
                    parent_revision_ids: Vec::new(),
                    intent,
                },
            },
            annotations,
        )
    }

    /// Adds a full Intent snapshot to an existing Space and generates its Revision/Event IDs.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for missing parents or incomplete Intent content.
    pub fn intent_revision_added(
        space_id: SpaceId,
        parent_revision_ids: Vec<RevisionId>,
        intent: IntentSnapshot,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::SpaceIntentRevisionAdded {
                space_id,
                intent_revision: IntentRevision {
                    revision_id: RevisionId::new(),
                    parent_revision_ids,
                    intent,
                },
            },
            annotations,
        )
    }

    /// Proposes a new `ContextItem`. Context, Revision, Evidence, and Event IDs are generated.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required Context content is missing.
    pub fn context_proposed(
        space_id: SpaceId,
        draft: ContextRevisionDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::ContextRevisionAdded {
                space_id,
                context_id: ContextId::new(),
                revision: ContextRevision::from_draft(Vec::new(), draft)?,
            },
            annotations,
        )
    }

    /// Revises an existing `ContextItem`. The referenced Context/parents are caller input;
    /// every newly defined identity is generated internally.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for missing parents or incomplete content.
    pub fn context_revised(
        space_id: SpaceId,
        context_id: ContextId,
        parent_revision_ids: Vec<RevisionId>,
        draft: ContextRevisionDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        if parent_revision_ids.is_empty() {
            return Err(invalid(
                "a Context revision of an existing item must reference at least one parent",
            ));
        }
        Self::generated(
            EventPayload::ContextRevisionAdded {
                space_id,
                context_id,
                revision: ContextRevision::from_draft(parent_revision_ids, draft)?,
            },
            annotations,
        )
    }

    /// Records an immutable review and generates its Review/Event IDs.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the review reason is empty.
    pub fn context_reviewed(
        space_id: SpaceId,
        context_id: ContextId,
        draft: ReviewDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::ContextReviewed {
                space_id,
                context_id,
                review: Review::from_draft(draft)?,
            },
            annotations,
        )
    }

    /// Records a publication transition and generates its Publication/Event IDs.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when causal or review references are duplicated.
    pub fn publication_changed(
        space_id: SpaceId,
        context_id: ContextId,
        draft: PublicationDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::ContextPublicationChanged {
                space_id,
                context_id,
                publication: Publication::from_draft(draft)?,
            },
            annotations,
        )
    }

    /// Confirms a cross-Context semantic conflict and generates its Conflict/Event IDs.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for an incomplete or duplicate participant set.
    pub fn semantic_conflict_opened(
        space_id: SpaceId,
        draft: SemanticConflictDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::SemanticConflictOpened {
                space_id,
                conflict: SemanticConflict::from_draft(draft)?,
            },
            annotations,
        )
    }

    /// Adds a conflict-resolution DAG node and generates its Resolution/Event IDs.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for missing or duplicate resolution references.
    pub fn semantic_conflict_resolution_added(
        space_id: SpaceId,
        conflict_id: ConflictId,
        draft: ConflictResolutionDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::SemanticConflictResolutionAdded {
                space_id,
                conflict_id,
                resolution: ConflictResolution::from_draft(draft)?,
            },
            annotations,
        )
    }

    /// Supported schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Generated immutable Event ID.
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    /// Closed V1 event type.
    #[must_use]
    pub const fn event_type(&self) -> EventType {
        self.payload.event_type()
    }

    /// Authoritative event payload.
    #[must_use]
    pub const fn payload(&self) -> &EventPayload {
        &self.payload
    }

    /// Non-authoritative metadata, if present.
    #[must_use]
    pub const fn annotations(&self) -> Option<&Annotations> {
        self.annotations.as_ref()
    }

    /// Converts every supported event into owned pure-reducer input.
    #[must_use]
    pub fn reducer_event(&self) -> Option<ReducerEvent> {
        let payload = match &self.payload {
            EventPayload::SpaceCreated {
                space_id,
                intent_revision,
            } => ReducerPayload::SpaceCreated {
                space_id: *space_id,
                intent_revision: intent_revision.clone(),
            },
            EventPayload::SpaceIntentRevisionAdded {
                space_id,
                intent_revision,
            } => ReducerPayload::SpaceIntentRevisionAdded {
                space_id: *space_id,
                intent_revision: intent_revision.clone(),
            },
            EventPayload::ContextRevisionAdded {
                space_id,
                context_id,
                revision,
            } => ReducerPayload::ContextRevisionAdded {
                space_id: *space_id,
                context_id: *context_id,
                revision: revision.clone(),
            },
            EventPayload::ContextReviewed {
                space_id,
                context_id,
                review,
            } => ReducerPayload::ContextReviewed {
                space_id: *space_id,
                context_id: *context_id,
                review: review.clone(),
            },
            EventPayload::ContextPublicationChanged {
                space_id,
                context_id,
                publication,
            } => ReducerPayload::ContextPublicationChanged {
                space_id: *space_id,
                context_id: *context_id,
                publication: publication.clone(),
            },
            EventPayload::SemanticConflictOpened { space_id, conflict } => {
                ReducerPayload::SemanticConflictOpened {
                    space_id: *space_id,
                    conflict: conflict.clone(),
                }
            }
            EventPayload::SemanticConflictResolutionAdded {
                space_id,
                conflict_id,
                resolution,
            } => ReducerPayload::SemanticConflictResolutionAdded {
                space_id: *space_id,
                conflict_id: *conflict_id,
                resolution: resolution.clone(),
            },
        };
        Some(ReducerEvent {
            event_id: self.event_id,
            payload,
        })
    }

    /// Validates all V1 field and payload-local invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when the payload violates the V1 contract.
    pub fn validate(&self) -> Result<()> {
        self.payload.validate()
    }

    /// Stable SHA-256 of authoritative semantics.
    ///
    /// This excludes annotations/origin hints, Event IDs, and IDs generated for
    /// newly defined payload nodes. Existing causal references remain included.
    ///
    /// # Panics
    ///
    /// Panics only if a future [`Event`] field violates its internal JSON serialization
    /// contract; all current V1 fields serialize infallibly.
    #[must_use]
    pub fn semantic_hash(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut value = serde_json::to_value(self).expect("serializing Event cannot fail");
        let object = value
            .as_object_mut()
            .expect("serialized Event is always a JSON object");
        object.remove("schema_version");
        object.remove("event_id");
        object.remove("annotations");
        remove_defined_identity_fields(self.event_type(), object);

        let mut canonical = Vec::new();
        write_canonical_json(&value, &mut canonical);
        let digest = Sha256::digest(canonical);
        let mut rendered = String::with_capacity(digest.len() * 2);
        for byte in digest {
            rendered.push(char::from(HEX[usize::from(byte >> 4)]));
            rendered.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        rendered
    }
}

fn remove_defined_identity_fields(event_type: EventType, object: &mut Map<String, Value>) {
    match event_type {
        EventType::SpaceCreated => {
            object.remove("space_id");
            if let Some(revision) = object
                .get_mut("intent_revision")
                .and_then(Value::as_object_mut)
            {
                revision.remove("revision_id");
            }
        }
        EventType::SpaceIntentRevisionAdded => {
            if let Some(revision) = object
                .get_mut("intent_revision")
                .and_then(Value::as_object_mut)
            {
                revision.remove("revision_id");
            }
        }
        EventType::ContextRevisionAdded => {
            object.remove("space_id");
            object.remove("context_id");
            if let Some(revision) = object.get_mut("revision").and_then(Value::as_object_mut) {
                revision.remove("revision_id");
                revision.remove("parent_revision_ids");
                if let Some(evidence) = revision.get_mut("evidence").and_then(Value::as_array_mut) {
                    for snapshot in evidence {
                        if let Some(snapshot) = snapshot.as_object_mut() {
                            snapshot.remove("evidence_id");
                        }
                    }
                }
            }
        }
        EventType::ContextReviewed => {
            if let Some(review) = object.get_mut("review").and_then(Value::as_object_mut) {
                review.remove("review_id");
            }
        }
        EventType::ContextPublicationChanged => {
            if let Some(publication) = object.get_mut("publication").and_then(Value::as_object_mut)
            {
                publication.remove("publication_id");
            }
        }
        EventType::SemanticConflictOpened => {
            if let Some(conflict) = object.get_mut("conflict").and_then(Value::as_object_mut) {
                conflict.remove("conflict_id");
            }
        }
        EventType::SemanticConflictResolutionAdded => {
            if let Some(resolution) = object.get_mut("resolution").and_then(Value::as_object_mut) {
                resolution.remove("resolution_id");
            }
        }
    }
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => output.extend_from_slice(number.to_string().as_bytes()),
        Value::String(string) => output.extend_from_slice(
            serde_json::to_string(string)
                .expect("serializing a JSON string cannot fail")
                .as_bytes(),
        ),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output);
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .expect("serializing a JSON key cannot fail")
                        .as_bytes(),
                );
                output.push(b':');
                write_canonical_json(&values[*key], output);
            }
            output.push(b'}');
        }
    }
}

/// Stable diagnostic classification emitted for non-projectable input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticCode {
    UnknownSchemaVersion,
}

impl DiagnosticCode {
    /// Machine-readable code suitable for a projection diagnostic table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownSchemaVersion => "UNKNOWN_SCHEMA_VERSION",
        }
    }
}

/// Diagnostic attached to a preserved unknown-schema event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaDiagnostic {
    pub code: DiagnosticCode,
    pub message: String,
}

/// Raw unknown-schema input. It cannot be converted to a V1 [`Event`].
#[derive(Clone, Debug)]
pub struct UnknownSchemaEvent {
    raw_json: String,
    schema_version: String,
    event_id: Option<String>,
    event_type: Option<String>,
    diagnostic: SchemaDiagnostic,
}

impl UnknownSchemaEvent {
    /// Original JSON text, including formatting, preserved for diagnostics/storage.
    #[must_use]
    pub fn raw_json(&self) -> &str {
        &self.raw_json
    }

    /// Unsupported version reported by the envelope probe.
    #[must_use]
    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }

    /// Unvalidated Event ID text, when present.
    #[must_use]
    pub fn event_id(&self) -> Option<&str> {
        self.event_id.as_deref()
    }

    /// Unvalidated event-type text, when present.
    #[must_use]
    pub fn event_type(&self) -> Option<&str> {
        self.event_type.as_deref()
    }

    /// Diagnostic explaining why this input cannot be projected.
    #[must_use]
    pub const fn diagnostic(&self) -> &SchemaDiagnostic {
        &self.diagnostic
    }
}

/// Result of compatibility-aware event parsing.
#[derive(Clone, Debug)]
pub enum ParsedEvent {
    Known(Box<Event>),
    UnknownSchema(UnknownSchemaEvent),
}

impl ParsedEvent {
    /// Returns a projectable V1 event, or `None` for quarantined unknown schemas.
    #[must_use]
    pub fn known(&self) -> Option<&Event> {
        match self {
            Self::Known(event) => Some(event.as_ref()),
            Self::UnknownSchema(_) => None,
        }
    }
}

/// Parses one event while preserving, diagnosing, and quarantining unknown schemas.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidInput`] for malformed JSON, a malformed envelope, or an
/// invalid event that declares a supported schema version.
pub fn parse_event(input: &[u8]) -> Result<ParsedEvent> {
    let raw_json = std::str::from_utf8(input)
        .map_err(|error| invalid(format!("event is not valid UTF-8 JSON: {error}")))?;
    let value: Value = serde_json::from_slice(input)
        .map_err(|error| invalid(format!("event is not valid JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("event must be a JSON object"))?;
    let schema_version = object
        .get("schema_version")
        .ok_or_else(|| invalid("event.schema_version is required"))?
        .as_str()
        .ok_or_else(|| invalid("event.schema_version must be a string"))?;

    if schema_version != SchemaVersion::V1.as_str() {
        let event_id = optional_probe_string(object, "event_id");
        let event_type = optional_probe_string(object, "event_type");
        return Ok(ParsedEvent::UnknownSchema(UnknownSchemaEvent {
            raw_json: raw_json.to_owned(),
            schema_version: schema_version.to_owned(),
            event_id,
            event_type,
            diagnostic: SchemaDiagnostic {
                code: DiagnosticCode::UnknownSchemaVersion,
                message: format!(
                    "schema version {schema_version:?} is not supported and was quarantined"
                ),
            },
        }));
    }

    let event: Event = serde_json::from_value(value)
        .map_err(|error| invalid(format!("event does not match V1 schema: {error}")))?;
    event.validate()?;
    Ok(ParsedEvent::Known(Box::new(event)))
}

fn optional_probe_string(object: &Map<String, Value>, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
