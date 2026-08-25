//! Stable identifiers, authoritative V1 snapshots, and the pure in-memory reducer.
//!
//! The reducer deliberately has no storage, Git, clock, or `SQLite` dependency.

use std::fmt;

mod confirmation;
mod engineering;
mod episode;
mod ids;
mod model;
mod reducer;
mod task;
mod working_intent;

pub use confirmation::{
    CandidateConfirmation, CandidateConfirmationCausalRefs, CandidateConfirmationDraft,
    CandidateConfirmationNewSpace, CandidateConfirmationOperation, CandidateConfirmationPlan,
    CandidateConfirmationPlanEventIds, CandidateConfirmationPrimaryReference,
    CandidateConfirmationRequest, CandidatePrimarySelection, ContextSpaceAssociation,
    ContextSpaceAssociationDraft, ContextSpaceAssociationOrigin, OptionalCandidateEdits,
    TopicKeyEdit, context_revision_as_draft, context_revision_content_hash,
};
pub use engineering::{
    ArtifactAssociationKind, ArtifactKey, ArtifactKind, ArtifactLocator, ArtifactRelationKind,
    ArtifactResolution, ContextArtifactAssociation, ContextRelation, ContextRelationKind,
    EngineeringArtifact, EngineeringGraphEdge, EngineeringReference, EngineeringReferenceDraft,
    ReferenceRelation, RepoRelativePath, RepositoryIdentity, ResolutionStatus, ResolvedFocus,
};
pub use episode::{
    AgentCheckpoint, ArtifactAction, ArtifactRef, AutomaticCandidateStatus,
    AutomaticContextCandidate, CandidateAnalysis, CandidateAnalysisStatus, CandidateAssessmentPath,
    CandidateAssessmentRelation, CandidateBuilderProvenance, CandidateConfidence,
    CandidateRelationAssessment, CandidateReviewDiagnostic, CandidateReviewStatus,
    CandidateReviewSummary, CandidateReviewView, CandidateSpaceRecommendation,
    CandidateSpaceRecommendationPath, CaptureEvidenceRef, CaptureSourceRef, CaptureUnknown,
    CheckpointClaim, ContextCandidate, ContextRevisionRef, ContextUseDisposition,
    IntentRevisionRange, NonLocatingSignalRef, NormalizedBreadcrumbKind, NormalizedWorkObservation,
    RecommendedSpaceRole, TestOutcomeStatus, WorkEpisode, WorkEpisodeRef, WorkEpisodeStatus,
    WorkObservation, WorkSourceRef, candidate_submission_content_hash,
};
pub use ids::{
    AgentCheckpointId, CandidateBuildId, CandidateId, CaptureId, CheckpointClaimId, ConfirmationId,
    ConflictId, ContextId, EventId, EvidenceId, ExternalSessionId, IdParseError, PublicationId,
    REPOSITORY_ID_MAX_BYTES, REPOSITORY_ID_PATTERN, ReferenceId, RepositoryGroupId, RepositoryId,
    ResolutionId, ReviewId, RevisionId, SignalId, SpaceAssociationId, SpaceId,
    SpaceRecommendationId, SubmissionId, TaskId, TaskIntentRevisionId, TaskSessionId,
    WorkEpisodeId, WorkObservationId,
};
pub use model::{
    Applicability, ConflictParticipant, ConflictResolution, ConflictResolutionDraft,
    ConflictResolutionResult, ContextKind, ContextRevision, ContextRevisionDraft, EvidenceSnapshot,
    EvidenceSnapshotDraft, EvidenceType, IntentRevision, IntentSnapshot, Publication,
    PublicationAction, PublicationDraft, ResolutionOutcome, Review, ReviewDraft, ReviewVerdict,
    SemanticConflict, SemanticConflictDraft,
};
pub use reducer::{
    AutoInjectionBlocker, AutoInjectionEligibility, CandidateConfirmationConflict,
    CandidateConfirmationProjection, CandidateProjection, CandidateSubmissionConflict,
    CandidateSubmissionProjection, ContextGovernanceStatus, ContextProjection,
    ContextSpaceAssociationConflict, ContextSpaceAssociationProjection, ContextSpaceProjection,
    DomainProjection, EngineeringReferenceProjection, IntentProjection, ReducerDiagnostic,
    ReducerDiagnosticCode, ReducerEvent, ReducerPayload, ReviewSummary, RevisionLifecycle,
    RevisionProjection, SemanticConflictCandidate, SemanticConflictOpenReason,
    SemanticConflictProjection, SemanticConflictStatus, reduce,
};
pub use task::{
    ExternalSessionLocator, ExternalSessionSnapshot, TaskIntentRevision, TaskSessionSnapshot,
    TaskSignal, TaskSignalKind, TaskSignalLifecycle, TaskSignalRecord, TaskSpaceAssociation,
};
pub use working_intent::{
    MAX_WORKING_INTENT_ITEM_BYTES, MAX_WORKING_INTENT_ITEMS_PER_FIELD,
    MAX_WORKING_INTENT_TEXT_BYTES, MAX_WORKING_INTENT_TOTAL_BYTES, WorkingIntentSnapshot,
};

/// Broad categories used to route recoverable errors across crate boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Input failed validation before any state change was attempted.
    InvalidInput,
    /// A core product invariant would be violated.
    InvariantViolation,
    /// A local filesystem or process operation failed.
    Io,
    /// An external command or protocol peer failed.
    External,
    /// A semantic idempotency key already names different authoritative content.
    Conflict,
    /// A caller-supplied compare-and-swap version no longer names the current state.
    StaleState,
    /// Untrusted content contains a Secret/PII signature and was rejected before storage.
    PrivacyRejected,
    /// The requested behavior is outside the current implementation stage.
    Unsupported,
    /// An absolute engineering path is outside the explicit local Repository Catalog.
    RepositoryNotConfigured,
    /// A stable idempotency key was reused with different authoritative content.
    IdempotencyKeyConflict,
    /// Another process currently owns the installation-wide maintenance boundary.
    MaintenanceBusy,
}

/// The shared, user-presentable error type for workspace crates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Creates an error with a stable category and actionable message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the user-presentable error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Workspace-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};

    #[test]
    fn shared_error_keeps_kind_and_message() {
        let error = Error::new(ErrorKind::InvariantViolation, "append-only contract failed");

        assert_eq!(error.kind(), ErrorKind::InvariantViolation);
        assert_eq!(error.message(), "append-only contract failed");
        assert_eq!(error.to_string(), "append-only contract failed");
    }
}
