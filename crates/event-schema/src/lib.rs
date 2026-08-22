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
    Applicability, ArtifactKind, ArtifactLocator, AutoInjectionBlocker, AutoInjectionEligibility,
    CandidateConfirmation, CandidateConfirmationCausalRefs, CandidateConfirmationDraft,
    CandidateId, CandidateProjection, ConfirmationId, ConflictId, ConflictParticipant,
    ConflictResolution, ConflictResolutionDraft, ConflictResolutionResult, ContextCandidate,
    ContextGovernanceStatus, ContextId, ContextKind, ContextProjection, ContextRelation,
    ContextRelationKind, ContextRevision, ContextRevisionDraft, ContextSpaceAssociation,
    ContextSpaceAssociationDraft, ContextSpaceAssociationOrigin, ContextSpaceProjection,
    DomainProjection, EngineeringReference, EngineeringReferenceDraft, Error, ErrorKind, EventId,
    EvidenceId, EvidenceSnapshot, EvidenceSnapshotDraft, EvidenceType, IdParseError,
    IntentProjection, IntentRevision, IntentSnapshot, OptionalCandidateEdits, Publication,
    PublicationAction, PublicationDraft, PublicationId, ReducerDiagnostic, ReducerDiagnosticCode,
    ReducerEvent, ReducerPayload, ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId,
    ResolutionId, ResolutionOutcome, Result, Review, ReviewDraft, ReviewId, ReviewSummary,
    ReviewVerdict, RevisionId, RevisionLifecycle, RevisionProjection, SemanticConflict,
    SemanticConflictCandidate, SemanticConflictDraft, SemanticConflictOpenReason,
    SemanticConflictProjection, SemanticConflictStatus, SpaceAssociationId, SpaceId, SubmissionId,
    TaskId, TaskSessionId, TopicKeyEdit, WorkEpisodeId, WorkEpisodeRef,
    context_revision_content_hash, reduce,
};

/// Immutable identifier for the bundled V1 JSON Schema.
pub const V1_SCHEMA_ID: &str = "https://shared-context.local/schemas/event-v1.schema.json";

/// Bundled JSON Schema text. Schema evolution adds a new file and constant.
pub const V1_JSON_SCHEMA: &str = include_str!("../../../schemas/event-v1.schema.json");

/// Maximum malformed Event size inspected for a Candidate submission hint.
pub const MAX_CANDIDATE_SUBMISSION_HINT_BYTES: usize = 256 * 1024;

/// Safely extracted identity hints from a known-V1 Candidate Event envelope.
///
/// A hint is diagnostic-only: it never makes a malformed Event projectable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateSubmissionHint {
    pub submission_id: SubmissionId,
    pub event_id: Option<EventId>,
    pub candidate_id: Option<CandidateId>,
}

/// Best-effort extraction used only to isolate malformed Candidate submissions.
///
/// The input must be bounded, valid JSON, exact known schema V1, exact
/// `context_candidate.created`, and contain a strictly valid `SubmissionId`.
/// Every other parse error remains outside this hint channel.
#[must_use]
pub fn candidate_submission_hint(bytes: &[u8]) -> Option<CandidateSubmissionHint> {
    if bytes.len() > MAX_CANDIDATE_SUBMISSION_HINT_BYTES {
        return None;
    }
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    let envelope = value.as_object()?;
    if envelope.get("schema_version")?.as_str()? != SchemaVersion::V1.as_str()
        || envelope.get("event_type")?.as_str()? != EventType::ContextCandidateCreated.as_str()
    {
        return None;
    }
    let candidate = envelope.get("candidate")?.as_object()?;
    let submission_id = candidate.get("submission_id")?.as_str()?.parse().ok()?;
    Some(CandidateSubmissionHint {
        submission_id,
        event_id: envelope
            .get("event_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
        candidate_id: candidate
            .get("candidate_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok()),
    })
}

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
    #[serde(rename = "context_candidate.created")]
    ContextCandidateCreated,
    #[serde(rename = "candidate.confirmed")]
    CandidateConfirmed,
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
    #[serde(rename = "context.space_association_changed")]
    ContextSpaceAssociationChanged,
    #[serde(rename = "semantic_conflict.opened")]
    SemanticConflictOpened,
    #[serde(rename = "semantic_conflict.resolution_added")]
    SemanticConflictResolutionAdded,
    #[serde(rename = "engineering_reference.recorded")]
    EngineeringReferenceRecorded,
}

impl EventType {
    /// Stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextCandidateCreated => "context_candidate.created",
            Self::CandidateConfirmed => "candidate.confirmed",
            Self::SpaceCreated => "space.created",
            Self::SpaceIntentRevisionAdded => "space.intent_revision_added",
            Self::ContextRevisionAdded => "context.revision_added",
            Self::ContextReviewed => "context.reviewed",
            Self::ContextPublicationChanged => "context.publication_changed",
            Self::ContextSpaceAssociationChanged => "context.space_association_changed",
            Self::SemanticConflictOpened => "semantic_conflict.opened",
            Self::SemanticConflictResolutionAdded => "semantic_conflict.resolution_added",
            Self::EngineeringReferenceRecorded => "engineering_reference.recorded",
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
    #[serde(rename = "context_candidate.created")]
    ContextCandidateCreated { candidate: ContextCandidate },
    #[serde(rename = "candidate.confirmed")]
    CandidateConfirmed {
        confirmation: Box<CandidateConfirmation>,
    },
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
    #[serde(rename = "context.space_association_changed")]
    ContextSpaceAssociationChanged {
        association: ContextSpaceAssociation,
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
    #[serde(rename = "engineering_reference.recorded")]
    EngineeringReferenceRecorded {
        context_id: ContextId,
        revision_id: RevisionId,
        reference: EngineeringReference,
    },
}

impl EventPayload {
    /// Event kind represented by this payload.
    #[must_use]
    pub const fn event_type(&self) -> EventType {
        match self {
            Self::ContextCandidateCreated { .. } => EventType::ContextCandidateCreated,
            Self::CandidateConfirmed { .. } => EventType::CandidateConfirmed,
            Self::SpaceCreated { .. } => EventType::SpaceCreated,
            Self::SpaceIntentRevisionAdded { .. } => EventType::SpaceIntentRevisionAdded,
            Self::ContextRevisionAdded { .. } => EventType::ContextRevisionAdded,
            Self::ContextReviewed { .. } => EventType::ContextReviewed,
            Self::ContextPublicationChanged { .. } => EventType::ContextPublicationChanged,
            Self::ContextSpaceAssociationChanged { .. } => {
                EventType::ContextSpaceAssociationChanged
            }
            Self::SemanticConflictOpened { .. } => EventType::SemanticConflictOpened,
            Self::SemanticConflictResolutionAdded { .. } => {
                EventType::SemanticConflictResolutionAdded
            }
            Self::EngineeringReferenceRecorded { .. } => EventType::EngineeringReferenceRecorded,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::ContextCandidateCreated { candidate } => candidate.validate(),
            Self::CandidateConfirmed { confirmation } => confirmation.validate(),
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
            Self::ContextSpaceAssociationChanged { association } => association.validate(),
            Self::SemanticConflictOpened { conflict, .. } => conflict.validate(),
            Self::SemanticConflictResolutionAdded { resolution, .. } => resolution.validate(),
            Self::EngineeringReferenceRecorded { reference, .. } => reference.validate(),
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
        if event.requires_writer_batch()
            && event
                .writer_batch_id()
                .is_none_or(|value| !valid_batch_id(value))
        {
            return Err(de::Error::custom(
                "Candidate-origin Event requires valid writer_batch_id annotation",
            ));
        }
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

    /// Records one unassigned Context Candidate and generates its Candidate/Event IDs.
    ///
    /// The source Work Episode is causal provenance, not a Space route.
    /// This event does not create a Context item or any publication eligibility.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when governable Candidate content is incomplete.
    pub fn context_candidate_created(
        submission_id: SubmissionId,
        source_episode: WorkEpisodeRef,
        content: ContextRevisionDraft,
        writer_batch_id: &str,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        if !valid_batch_id(writer_batch_id) {
            return Err(invalid("writer batch ID must be a canonical bat_ UUIDv4"));
        }
        let mut annotations = annotations.unwrap_or_default();
        if annotations
            .additional
            .insert(
                "writer_batch_id".to_owned(),
                Value::String(writer_batch_id.to_owned()),
            )
            .is_some()
        {
            return Err(invalid("writer batch ID is already present"));
        }
        Self::generated(
            EventPayload::ContextCandidateCreated {
                candidate: ContextCandidate::from_verified_submission(
                    submission_id,
                    source_episode,
                    content,
                )?,
            },
            Some(annotations),
        )
    }

    /// Records an initial Candidate-origin Context-to-Space organization snapshot.
    ///
    /// # Errors
    ///
    /// Rejects correction origin, invalid membership/causality, or a non-canonical Writer batch.
    pub fn candidate_space_association_changed(
        draft: ContextSpaceAssociationDraft,
        writer_batch_id: &str,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        if !matches!(
            draft.origin,
            ContextSpaceAssociationOrigin::CandidateConfirmation { .. }
        ) {
            return Err(invalid(
                "candidate Space Association Event requires CandidateConfirmation origin",
            ));
        }
        let annotations = annotations_with_writer_batch(writer_batch_id, annotations)?;
        Self::generated(
            EventPayload::ContextSpaceAssociationChanged {
                association: ContextSpaceAssociation::from_draft(draft)?,
            },
            Some(annotations),
        )
    }

    /// Records a future causal correction to Context-to-Space organization.
    ///
    /// This constructor is domain-only; authorization is a later application boundary.
    ///
    /// # Errors
    ///
    /// Rejects Candidate origin or invalid membership/causality.
    pub fn context_space_association_changed(
        draft: ContextSpaceAssociationDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        if draft.origin != ContextSpaceAssociationOrigin::Correction {
            return Err(invalid(
                "ordinary Space Association change requires correction origin",
            ));
        }
        Self::generated(
            EventPayload::ContextSpaceAssociationChanged {
                association: ContextSpaceAssociation::from_draft(draft)?,
            },
            annotations,
        )
    }

    /// Records the immutable fact closure of one Candidate confirmation.
    ///
    /// # Errors
    ///
    /// Rejects invalid local Confirmation content or a non-canonical Writer batch.
    pub fn candidate_confirmed(
        draft: CandidateConfirmationDraft,
        writer_batch_id: &str,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        let annotations = annotations_with_writer_batch(writer_batch_id, annotations)?;
        Self::generated(
            EventPayload::CandidateConfirmed {
                confirmation: Box::new(CandidateConfirmation::from_draft(draft)?),
            },
            Some(annotations),
        )
    }

    /// Server-owned Writer batch provenance, if present and well-typed.
    #[must_use]
    pub fn writer_batch_id(&self) -> Option<&str> {
        self.annotations
            .as_ref()?
            .additional
            .get("writer_batch_id")?
            .as_str()
    }

    fn requires_writer_batch(&self) -> bool {
        matches!(
            self.payload,
            EventPayload::ContextCandidateCreated { .. } | EventPayload::CandidateConfirmed { .. }
        ) || matches!(
            &self.payload,
            EventPayload::ContextSpaceAssociationChanged { association }
                if matches!(association.origin, ContextSpaceAssociationOrigin::CandidateConfirmation { .. })
        )
    }

    /// Records one persistent, non-authoritative engineering observation for a Context revision.
    ///
    /// Reference and Event identities are generated internally. Target ownership is deliberately
    /// validated by the reducer against the complete Event set.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] for invalid locator, relation, support, or
    /// limitation content.
    pub fn engineering_reference_recorded(
        context_id: ContextId,
        revision_id: RevisionId,
        draft: EngineeringReferenceDraft,
        annotations: Option<Annotations>,
    ) -> Result<Self> {
        Self::generated(
            EventPayload::EngineeringReferenceRecorded {
                context_id,
                revision_id,
                reference: EngineeringReference::from_draft(draft)?,
            },
            annotations,
        )
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

    /// Adds the initial revision for a new confirmed `ContextItem`.
    /// Context, Revision, Evidence, and Event IDs are generated.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] when required Context content is missing.
    pub fn context_revision_added(
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
            EventPayload::ContextCandidateCreated { candidate } => {
                ReducerPayload::ContextCandidateCreated {
                    candidate: candidate.clone(),
                }
            }
            EventPayload::CandidateConfirmed { confirmation } => {
                ReducerPayload::CandidateConfirmed {
                    confirmation: Box::new(confirmation.as_ref().clone()),
                }
            }
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
            EventPayload::ContextSpaceAssociationChanged { association } => {
                ReducerPayload::ContextSpaceAssociationChanged {
                    association: association.clone(),
                }
            }
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
            EventPayload::EngineeringReferenceRecorded {
                context_id,
                revision_id,
                reference,
            } => ReducerPayload::EngineeringReferenceRecorded {
                context_id: *context_id,
                revision_id: *revision_id,
                reference: reference.clone(),
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
        EventType::ContextCandidateCreated => {
            if let Some(candidate) = object.get_mut("candidate").and_then(Value::as_object_mut) {
                candidate.remove("candidate_id");
            }
        }
        EventType::CandidateConfirmed => {
            if let Some(confirmation) = object
                .get_mut("confirmation")
                .and_then(Value::as_object_mut)
            {
                confirmation.remove("confirmation_id");
            }
        }
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
        EventType::ContextSpaceAssociationChanged => {
            if let Some(association) = object.get_mut("association").and_then(Value::as_object_mut)
            {
                association.remove("association_id");
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
        EventType::EngineeringReferenceRecorded => {
            if let Some(reference) = object.get_mut("reference").and_then(Value::as_object_mut) {
                reference.remove("reference_id");
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

fn annotations_with_writer_batch(
    writer_batch_id: &str,
    annotations: Option<Annotations>,
) -> Result<Annotations> {
    if !valid_batch_id(writer_batch_id) {
        return Err(invalid("writer batch ID must be a canonical bat_ UUIDv4"));
    }
    let mut annotations = annotations.unwrap_or_default();
    if annotations
        .additional
        .insert(
            "writer_batch_id".to_owned(),
            Value::String(writer_batch_id.to_owned()),
        )
        .is_some()
    {
        return Err(invalid("writer batch ID is already present"));
    }
    Ok(annotations)
}

fn valid_batch_id(value: &str) -> bool {
    let Some(uuid) = value.strip_prefix("bat_") else {
        return false;
    };
    let bytes = uuid.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && bytes.iter().enumerate().all(|(index, byte)| {
            [8, 13, 18, 23].contains(&index) || byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
        })
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
