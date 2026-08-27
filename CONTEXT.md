# Shared Context

Shared Context organizes durable engineering knowledge around the intent behind work while allowing an Agent's current task to discover that knowledge without prior routing.

## Language

**ContextSpace**:
A stable container that organizes the intent and durable Context of one requirement or long-running objective. It is a knowledge-governance boundary, not a retrieval partition or property of a Workspace.
_Avoid_: Active Space, Workspace Space, search partition

**WorkingIntentSnapshot**:
A lightweight, revisable snapshot of what the Agent currently understands it is doing: a required goal plus any naturally known direction, scope, constraints, acceptance conditions, retrieval Hints, and open questions. Its Hints may match text but never establish an Artifact, Interface, Evidence, Graph path, Candidate, Space selection, or investigation checklist.
_Avoid_: grounded Intent, task specification, Evidence binding

**TaskSession**:
The local runtime boundary for one explicit Agent Task. An ExternalSession may retain multiple historical TaskSessions while selecting exactly one as active.
_Avoid_: External Session, Active Space

**ExternalSession**:
A local container corresponding to one external Agent session, with exactly one ActiveTask and zero or more historical Tasks. It preserves task history but does not infer when one Task should end and another should begin.
_Avoid_: Task, Workspace, knowledge owner

**ActiveTask**:
The one TaskSession currently selected for new TaskIntentRevisions, non-locating TaskSignals, and retrieval within an ExternalSession. Changing it is an explicit task-boundary decision, not a Workspace-derived guess.
_Avoid_: Active Space, latest Prompt

**IntentBootstrapReminder**:
A bounded, one-shot advisory for an authorized Agent session that reaches substantive tool work before an ActiveTask exists. It asks the Agent to call task_intent_update explicitly, without interpreting Prompt text or creating a Task.
_Avoid_: automatic Task, Prompt-derived Intent, recurring reminder

**TaskIntentRevision**:
One immutable version of a WorkingIntentSnapshot in a TaskSession's parent chain. It records how task understanding evolves without selecting or owning a ContextSpace.
_Avoid_: Space revision, Prompt history

**ExternalSessionLocator**:
An Agent kind and its external session key used only to find the corresponding local ExternalSession. It is neither Task identity nor durable knowledge identity.
_Avoid_: TaskSessionId, knowledge identifier

**TaskSignal**:
A non-factual, non-locating work clue such as a Prompt, Workspace, Diff, or TestOutcome. It may influence a WorkingIntentSnapshot or retrieval, but it is not engineering Evidence, Artifact identity, or a declaration of Space membership.
_Avoid_: Evidence, Space binding, routing key

**ToolCategory**:
A vendor-neutral structural class emitted by an Agent adapter for one completed tool call: FileOperation, TestRunner, Shell, SharedContext, or Other. Shell classification may recognize only bounded simple test-runner commands; normalized Runtime and Capture meaning retain neither command text nor vendor tool names, and SharedContext calls are excluded from capture.
_Avoid_: substring guess, raw command, vendor tool log, tool-output classification

**ArtifactFocusQuery**:
A one-request question asking for historical Context around one Artifact. It is transient retrieval input, not Task state, Evidence, or an engineering fact.
_Avoid_: Active Focus, Focus Signal, saved Focus

**ResolvedFocus**:
The server’s transient RepositoryIdentity plus complete ArtifactLocator interpretation of one ArtifactFocusQuery. It exists only for that query and is never restored, superseded, or reused implicitly.
_Avoid_: Focus ID, Artifact evidence, persistent query state

**ResolvedFocusTextFallback**:
A strict, request-only text RetrievalPath requiring the complete RepositoryId plus canonical ArtifactLocator key when no EngineeringGraphSnapshot is available. It is not Graph evidence, never uses basename/fuzzy matching, and is disabled whenever a Graph snapshot exists, including missing or ambiguous resolution.
_Avoid_: inferred Artifact edge, fuzzy Focus, saved fallback

**TestOutcome**:
A non-locating observation about a test tool execution, such as success or failure. It cannot identify or match a qualified Test Artifact; a request that needs that lookup supplies a separate ArtifactFocusQuery.
_Avoid_: Test Artifact, qualified Test locator

**TaskSignalLifecycle**:
The current relevance of one identified non-locating TaskSignal within its Task: Active signals participate in retrieval, while Superseded signals remain historical work records but do not participate.
_Avoid_: deletion, global signal state

**CaptureRecord**:
A redacted, TTL-bounded local Breadcrumb identified by CaptureId and carrying its ExternalSessionLocator plus any exactly resolved ActiveTask owner. It is an ingestion source, not yet a WorkObservation or durable knowledge.
_Avoid_: transcript, tool log, ownerless Task attribution

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

**AutomatedEpisodeBoundary**:
A fail-open lifecycle transition that may close one Open WorkEpisode only at its latest persisted AgentCheckpoint for the current TaskIntentRevision, then ask the shared CandidateBuilder to process that exact Closed Episode. PreCompact and TurnStop may trigger it; it never authors Claims or Unknowns, and repeated or concurrent triggers reuse the same Episode and Candidate identities. SessionEnd is retention cleanup, not an AutomatedEpisodeBoundary.
_Avoid_: automatic Checkpoint, Hook-authored Claim, SessionEnd Candidate build

**CheckpointClaim**:
A structured engineering assertion containing its statement, rationale, applicability, assumptions, recheck conditions, typed Evidence references, Artifact associations, proposed ContextRelations, proposed EngineeringReferences, and analysis-only related Context revisions. Artifact associations, EngineeringReference proposals, and related Context revisions are not Evidence or durable facts by themselves.
_Avoid_: unsupported conclusion, free-form note

**CandidateBuilderProvenance**:
The typed ownership, exact Checkpoint and WorkObservation inputs, and proposed EngineeringReferences of one automatic Candidate build. It makes the source WorkEpisode and Reference proposals verifiable without retaining raw Agent payloads.
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

**ProposedSpaceGroup**:
The stable review grouping shared by all Candidates produced from one exact TaskIntentRevision. Its first confirmed new Space becomes the existing-Space recommendation for remaining group members without merging their Claims or Candidates.
_Avoid_: semantic Candidate deduplication, Active Space, global Space default

**CandidateReview**:
A Task-scoped, untrusted presentation of one finalized AutomaticContextCandidate, preserving its complete draft, provenance, analysis, recommendations, confidence, and unknowns for a human decision.
_Avoid_: Context fact, Candidate approval, Git Candidate browser

**CandidateReviewStatus**:
The human disposition of a CandidateReview: Pending remains available for a decision, Discarded records an explicit decision not to retain it, Expired is a terminal local-retention tombstone, and Confirmed records the exact accepted CandidateConfirmation and resulting Context.
_Avoid_: analysis status, readiness status, publication status

**CandidateConfirmation**:
An immutable fact that one unconflicted Candidate produced one exact published Context revision, its Primary and Related Space organization, and the causal facts that created them. It records the final edited content without turning the Candidate itself into injectable knowledge.
_Avoid_: CandidateReview decision, Candidate activation, Candidate mutation

**CandidateConfirmationOperation**:
One recoverable human confirmation decision bound to an exact Pending CandidateReview version, analysis generation, Space selection, Related Spaces, and optional field edits.
_Avoid_: content deduplication, automatic confirmation, Candidate mutation

**CandidateConfirmationPlan**:
The complete server-owned fact closure reserved for one CandidateConfirmationOperation, including stable identities and causal references for every knowledge fact it will create.
_Avoid_: caller-authored Event batch, mutable execution plan, partial confirmation

**ContextSpaceAssociation**:
A causally revisable knowledge-organization snapshot linking one Context to exactly one Primary ContextSpace and zero or more Related ContextSpaces. It is independent of ContextRelation and does not by itself relocate the Context’s current nested owner.
_Avoid_: TaskSpaceAssociation, ContextRelation, Workspace binding

**TaskSpaceAssociation**:
A derived, explainable relevance between one Task and one ContextSpace. A Task may have no associations or multiple associations, and their ordering may change as new signals arrive.
_Avoid_: Active Space, default Space

**TaskContextPack**:
A budgeted retrieval result for one WorkingIntentSnapshot revision, one current Context projection, and at most one historical EngineeringGraphSnapshot. It keeps their identities distinct and links every returned Context through typed RetrievalPaths and a safety source.
_Avoid_: Space-scoped search result, manually routed Context Pack

**RetrievalPath**:
A typed explanation of how Working Intent text, ContextSpaceAssociation role, or this request’s ResolvedFocus made one Context relevant. A Primary/Related Space path and ResolvedFocusTextFallback remain distinct from ContextRelation and Engineering Graph paths; only an exact ResolvedFocus reachable in that Graph Snapshot can claim an engineering edge.
_Avoid_: opaque relevance score, inferred code relation

**AutomaticTextEligibility**:
The request-local safety decision that lets text retrieval enter an automatic TaskContextPack only with a strong phrase, sufficient query coverage, independent corroborating channels, Context text plus exact scope, or an exact Graph/ContextRelation path. Explicit exploration remains available, while weak Space text cannot make unrelated sibling Context automatically eligible.
_Avoid_: any BM25 hit, Space-wide Context inheritance, explicit-search filter

**EngineeringGraphSnapshot**:
An explicitly built, sparse historical knowledge view rooted at EngineeringReferences and their bounded ContextRelation closure. Its Context revisions and safety decisions remain immutable until another explicit Graph build, independently of later Context Store appends.
_Avoid_: current Context view, automatic index refresh

**GraphContextSnapshot**:
One immutable Context revision, its fixed relation targets, and its automatic-safety decision as included by an EngineeringGraphSnapshot. Its source Context Tree is provenance, not a current-validity condition.
_Avoid_: current Context head, live governance lookup

**Evidence**:
A self-contained or typed, resolvable provenance record supporting a normalized WorkObservation or an engineering assertion in a CheckpointClaim, Candidate, ContextRevision, or EngineeringReference. A WorkingIntentSnapshot, TaskSignal, Hint, or ArtifactFocusQuery may guide work or retrieval but is not Evidence.
_Avoid_: Task clue, retrieval match, Artifact association alone

**EvidenceSource**:
A typed, resolvable provenance target for an engineering claim, such as immutable Context Evidence or a current unique Engineering Resolution. WorkingIntentSnapshot, its Hints, ArtifactFocusQuery, generic TestOutcome, and unavailable/ambiguous targets are not EvidenceSources.
_Avoid_: evidence string, unverified reference

**KnowledgeStore**:
The append-only fact store that carries durable Shared Context knowledge across installations. Integrated team facts and installation-local proposals remain distinct from every business source RepositoryIdentity.
_Avoid_: source repository, RepositoryCatalog, ContextSpace

**InstallationWorkBranch**:
The stable KnowledgeStore proposal boundary owned by one Shared Context installation. It carries that installation's knowledge changes while the remote default remains the integrated, read-only team baseline.
_Avoid_: main branch, source-code feature branch, RepositoryId

**RepositoryCatalog**:
The local explicit binding from team-stable RepositoryIds to this installation’s checkout or worktree locations. It is authoritative for local path ownership but never assigns identity or routes a Workspace or Task to a ContextSpace.
_Avoid_: repository discovery, Workspace binding, team registry

**RepositoryId**:
A user-visible, exact-case ASCII name shared by a team for one logical source repository, such as `Android`, `iOS`, or `FE`. Paths, remotes, and local checkout names never derive or normalize it.
_Avoid_: generated repository UUID, repository URL, local path alias

**RepositoryIdentity**:
A stable RepositoryId for one logical source repository across team members’ configured checkouts and worktrees. Paths, basenames, remotes, common parents, branches, and Commits never create or merge this identity.
_Avoid_: checkout path, repository URL as identity, inferred repository identity

**RepositoryGroup**:
A local, explicitly declared activation boundary containing a closed set of RepositoryIdentities that may participate together in one Agent session. It is neither repository discovery nor Workspace-to-ContextSpace routing or knowledge identity.
_Avoid_: inferred repository family, Workspace Space, monorepo identity

**ActivationScope**:
The local authorization decision for whether Shared Context may participate in an Agent session: one Direct RepositoryIdentity, one explicitly matched RepositoryGroup, or Disabled. It never selects a ContextSpace or owns durable knowledge.
_Avoid_: Workspace route, Active Space, repository discovery

**AuthorizedSessionScope**:
An ExternalSessionLocator-owned, TTL-bounded local activation lease holding one Disabled, Direct, or Group decision. It is neither a TaskSession, Workspace route, Context fact, nor durable knowledge.
_Avoid_: Task authorization, Workspace binding, durable session knowledge

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
A stable knowledge edge between two Context revisions expressing dependency, constraint, implementation, validation, contradiction, or general relatedness. Supersession belongs to Revision and governance causality, not ContextRelation.
_Avoid_: inferred code edge, retrieval score

**Workspace**:
A location containing code or a repository that may supply non-locating TaskSignals and Breadcrumbs about the current engineering scene. It does not identify an Artifact, requirement, ContextSpace, or durable knowledge owner.
_Avoid_: Requirement, Space binding
