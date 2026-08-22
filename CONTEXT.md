# Shared Context

Shared Context organizes durable engineering knowledge around the intent behind work while allowing an Agent's current task to discover that knowledge without prior routing.

## Language

**ContextSpace**:
A stable container that organizes the intent and durable Context of one requirement or long-running objective. It is a knowledge-governance boundary, not a retrieval partition or property of a Workspace.
_Avoid_: Active Space, Workspace Space, search partition

**TaskIntent**:
The current structured understanding of a Task's goal, desired change, scope, constraints, and unknowns. It is independent of every ContextSpace and may evolve as the Task proceeds.
_Avoid_: Prompt, Space selection

**TaskSession**:
The local runtime boundary for one explicit Agent Task. An ExternalSession may retain multiple historical TaskSessions while selecting exactly one as active.
_Avoid_: External Session, Active Space

**ExternalSession**:
A local container corresponding to one external Agent session, with exactly one ActiveTask and zero or more historical Tasks. It preserves task history but does not infer when one Task should end and another should begin.
_Avoid_: Task, Workspace, knowledge owner

**ActiveTask**:
The one TaskSession currently selected for new Intent revisions, non-locating TaskSignals, and retrieval within an ExternalSession. Changing it is an explicit task-boundary decision, not a Workspace-derived guess.
_Avoid_: Active Space, latest Prompt

**TaskIntentRevision**:
One immutable version of a TaskIntent in a TaskSession's parent chain. It records how task understanding evolves without selecting or owning a ContextSpace.
_Avoid_: Space revision, Prompt history

**ExternalSessionLocator**:
An Agent kind and its external session key used only to find the corresponding local ExternalSession. It is neither Task identity nor durable knowledge identity.
_Avoid_: TaskSessionId, knowledge identifier

**TaskSignal**:
An observable, non-locating input that informs TaskIntent or knowledge retrieval, such as a Prompt, Workspace, Diff, or TestOutcome. A signal is evidence about the current Task, not Artifact identity or a declaration of Space membership.
_Avoid_: Space binding, routing key

**ArtifactFocusQuery**:
A one-request question asking for historical Context around one Artifact. It is transient retrieval input, not Task state, Evidence, or an engineering fact.
_Avoid_: Active Focus, Focus Signal, saved Focus

**ResolvedFocus**:
The server’s transient RepositoryIdentity plus complete ArtifactLocator interpretation of one ArtifactFocusQuery. It exists only for that query and is never restored, superseded, or reused implicitly.
_Avoid_: Focus ID, Artifact evidence, persistent query state

**TestOutcome**:
A non-locating observation about a test tool execution, such as success or failure. It cannot identify or match a qualified Test Artifact; a request that needs that lookup supplies a separate ArtifactFocusQuery.
_Avoid_: Test Artifact, qualified Test locator

**TaskSignalLifecycle**:
The current relevance of one identified non-locating TaskSignal within its Task: Active signals participate in retrieval, while Superseded signals remain historical evidence but do not participate.
_Avoid_: deletion, global signal state

**CaptureRecord**:
A redacted, TTL-bounded local Breadcrumb identified by CaptureId and carrying its ExternalSessionLocator plus any exactly resolved ActiveTask owner. It is an ingestion source, not yet a WorkObservation or durable knowledge.
_Avoid_: transcript, tool log, ownerless Task evidence

**CaptureClaim**:
An idempotent reservation of one CaptureRecord for its exact Task owner. It neither deletes the CaptureRecord nor proves that Runtime ingestion committed.
_Avoid_: Observation commit, Capture deletion, cross-Task handoff

**WorkEpisode**:
A server-identified, TaskSession- and Task-owned interval that aggregates normalized engineering observations across an explicit range of TaskIntentRevisions. It remains Open while work is accumulating and becomes Closed at a final AgentCheckpoint; it never embeds raw Prompt, transcript, tool output, or Space routing.
_Avoid_: chat transcript, tool log, Workspace-to-Space binding

**WorkObservation**:
A server-identified, normalized statement about engineering work under one WorkEpisode, owned by an exact TaskIntentRevision and grounded by typed source references or self-contained inline Validation evidence. It preserves extracted meaning rather than raw source payloads.
_Avoid_: raw Breadcrumb, terminal output, untyped artifact string

**AgentCheckpoint**:
An Agent-authored, server-identified snapshot of Claims and Unknowns owned by one WorkEpisode, TaskSession, Task, TaskIntentRevision, and exact parent Episode version. It may continue or close the Episode without selecting a ContextSpace; an Unknown-only Checkpoint records uncertainty without asserting knowledge.
_Avoid_: conversation summary, Candidate approval, Space selection

**CheckpointClaim**:
A structured engineering assertion containing its statement, rationale, applicability, assumptions, recheck conditions, typed Evidence references, Artifact associations, and related Context revisions. An Artifact association is not Evidence by itself.
_Avoid_: unsupported conclusion, free-form note

**CandidateBuilderProvenance**:
The typed ownership and exact Checkpoint and WorkObservation inputs of one automatic Candidate build. It makes the source WorkEpisode verifiable without retaining raw Agent payloads.
_Avoid_: opaque source_episode_id, transcript pointer

**CandidateBuilder**:
A deterministic converter that turns each sufficiently evidenced Claim in one Closed WorkEpisode into an unowned Context draft. It preserves explicit Claim classification when present, uses a conservative Discovery fallback when absent, and never performs semantic deduplication, conflict analysis, or Space selection.
_Avoid_: recommendation engine, keyword classifier, Candidate confirmer

**SubmissionId**:
The stable identity of one Candidate creation operation, reused only when retrying that operation. Equal Candidate content under different SubmissionIds represents distinct creation operations and must not converge.
_Avoid_: content hash, CandidateId, semantic deduplication key

**CandidateSubmission**:
One SubmissionId paired with exact closed WorkEpisode ownership and a complete Context draft. Its authoritative content hash detects conflicting retries of that SubmissionId but never deduplicates different submissions.
_Avoid_: similarity match, content-addressed Candidate, Candidate analysis

**AutomaticContextCandidate**:
An unowned, non-injectable Context draft produced from one Closed WorkEpisode. It carries CandidateAnalysis, Space recommendations, confidence, Unknowns, and Builder provenance, but no selected ContextSpace.
_Avoid_: published Context, accepted Candidate, automatically injected knowledge

**CandidateAnalysis**:
A non-authoritative, generation-pinned review assessment comparing an AutomaticContextCandidate with existing immutable Context revisions. It distinguishes exact duplicate, support, revision, potential contradiction, unresolved relatedness, and novelty through typed evidence paths; retrieval similarity alone never becomes a knowledge fact.
_Avoid_: confirmed conflict, BM25 fact, automatic governance decision

**CandidateRelationAssessment**:
One target-scoped conclusion inside CandidateAnalysis, carrying a confidence basis, typed comparison paths, and human-readable reasons. Exact duplicate requires full canonical draft equality, while potential contradiction and unresolved relatedness explicitly remain review hypotheses.
_Avoid_: similarity label, authoritative ContextRelation

**CandidateSpaceRecommendation**:
A non-binding, path-explained assessment that an AutomaticContextCandidate may be Primary or Related to an existing ContextSpace, or that a complete system-suggested new Space Intent may be needed. Recommendations never create, select, or resolve a conflicted Space Intent.
_Avoid_: Candidate ownership, Active Space, automatic Space creation

**TaskSpaceAssociation**:
A derived, explainable relevance between one Task and one ContextSpace. A Task may have no associations or multiple associations, and their ordering may change as new signals arrive.
_Avoid_: Active Space, default Space

**TaskContextPack**:
A budgeted retrieval result for one TaskIntent revision, one current Context projection, and at most one historical EngineeringGraphSnapshot. It keeps their identities distinct and links every returned Context through typed RetrievalPaths and a safety source.
_Avoid_: Space-scoped search result, manually routed Context Pack

**RetrievalPath**:
A typed explanation of how TaskIntent or this request’s ResolvedFocus made one Context relevant. Text and Applicability paths remain distinct from Engineering Graph paths; only an exact ResolvedFocus reachable in that Graph Snapshot can claim an engineering edge.
_Avoid_: opaque relevance score, inferred code relation

**EngineeringGraphSnapshot**:
An explicitly built, sparse historical knowledge view rooted at EngineeringReferences and their bounded ContextRelation closure. Its Context revisions and safety decisions remain immutable until another explicit Graph build, independently of later Context Store appends.
_Avoid_: current Context view, automatic index refresh

**GraphContextSnapshot**:
One immutable Context revision, its fixed relation targets, and its automatic-safety decision as included by an EngineeringGraphSnapshot. Its source Context Tree is provenance, not a current-validity condition.
_Avoid_: current Context head, live governance lookup

**EvidenceSource**:
A typed, resolvable provenance target that can ground a TaskIntent or engineering claim, such as immutable Context Evidence or a current unique Engineering Resolution. ArtifactFocusQuery, ResolvedFocus, generic TestOutcome, and unavailable/ambiguous targets are not EvidenceSources.
_Avoid_: evidence string, unverified reference

**RepositoryCatalog**:
The local explicit record of logical Repository identities and their configured checkout or worktree locations. It is authoritative for RepositoryId assignment but never routes a Workspace or Task to a ContextSpace.
_Avoid_: repository discovery, Workspace binding, team registry

**RepositoryIdentity**:
A stable identity for one logical source repository across its configured checkouts and worktrees in one local installation. Paths, basenames, remotes, common parents, branches, and Commits never create or merge this identity.
_Avoid_: checkout path, repository URL as identity, inferred repository identity

**EngineeringReference**:
A persistent, non-authoritative observation that Context relates to an engineering object at one deterministic repository-relative ArtifactLocator. It survives resolution failure and never claims that a current Artifact was found.
_Avoid_: resolved Artifact, file identity

**ArtifactLocator**:
The canonical, kind-specific identity of an engineering object within one RepositoryIdentity: exact Path for File/Module, protocol coordinates for API, qualified coordinates for Schema/Symbol/Test. It never uses content similarity or version digests.
_Avoid_: Locator Hint, version digest, relocation candidate

**EngineeringArtifact**:
A currently discovered File, Module, API, Schema, Symbol, or Test in an available Repository snapshot. It is derived state identified by RepositoryIdentity and an exact ArtifactLocator and can be rebuilt.
_Avoid_: Context fact, path identity

**ArtifactResolution**:
The current resolved, missing, ambiguous, or unavailable interpretation of one EngineeringReference. Only a unique exact locator match is resolved; resolution never guesses after a move or rename.
_Avoid_: permanent link, knowledge fact

**ContextArtifactAssociation**:
A derived, explainable link between a Context revision and a resolved EngineeringArtifact. It can disappear or be recomputed without changing durable Context knowledge.
_Avoid_: Context ownership, immutable relation

**ContextRelation**:
A stable knowledge edge between two Context revisions, such as dependency, constraint, implementation, validation, conflict, or supersession. Unlike engineering associations, it remains meaningful without access to source repositories.
_Avoid_: inferred code edge, retrieval score

**Workspace**:
A location containing code or a repository that may supply non-locating TaskSignals and Breadcrumbs about the current engineering scene. It does not identify an Artifact, requirement, ContextSpace, or durable knowledge owner.
_Avoid_: Requirement, Space binding
