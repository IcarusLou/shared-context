//! Append-only `Git` storage, batch journal, and crash-recovery boundary.
//!
//! The writer deliberately shells out to the system `git` executable with an
//! argv vector. It never constructs a shell command and never uses broad
//! pathspecs such as `git add .` or `git add -A`.

mod git;
mod journal;
mod store;

pub use journal::{BatchId, JournalPhase, PendingBatch, PendingFile, PendingFileKind};
pub use sctx_domain::{Error, ErrorKind, Result};
pub use store::{
    AppendBatchOutcome, AppendOutcome, AppendRequest, CandidateConfirmationIndex,
    CandidateConfirmationLookup, CandidateConfirmationOutcome, CandidateConfirmationRecord,
    CandidateConfirmationWriteStatus, CandidateSubmissionIndex, CandidateSubmissionLookup,
    CandidateSubmissionOutcome, CandidateSubmissionRecord, CandidateSubmissionRequest,
    CandidateSubmissionStatus, CommitObserver, CrashInjector, CrashSeam, GitStore,
    NoopCommitObserver, NoopCrashInjector, ObjectRef, RemoteBootstrap, StagedValidation,
    TextObject, UnavailableCandidateConfirmationIndex, UnavailableCandidateSubmissionIndex,
};

/// Stable marker used for the retryable case where an object exists outside
/// the current `HEAD` tree and therefore cannot safely be reused.
pub const OBJECT_PENDING: &str = "OBJECT_PENDING";
