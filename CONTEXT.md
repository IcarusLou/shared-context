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
The one TaskSession currently selected for new Intent revisions, TaskSignals, and retrieval within an ExternalSession. Changing it is an explicit task-boundary decision, not a Workspace-derived guess.
_Avoid_: Active Space, latest Prompt

**TaskIntentRevision**:
One immutable version of a TaskIntent in a TaskSession's parent chain. It records how task understanding evolves without selecting or owning a ContextSpace.
_Avoid_: Space revision, Prompt history

**ExternalSessionLocator**:
An Agent kind and its external session key used only to find the corresponding local ExternalSession. It is neither Task identity nor durable knowledge identity.
_Avoid_: TaskSessionId, knowledge identifier

**TaskSignal**:
An observable input that informs TaskIntent or knowledge retrieval, such as a Prompt, Workspace, Repository, File, Symbol, Diff, API, Schema, or Test observation. A signal is evidence about the current Task, not a declaration of Space membership.
_Avoid_: Space binding, routing key

**TaskSignalLifecycle**:
The current relevance of one identified TaskSignal within its Task: Active signals participate in retrieval, while Superseded signals remain historical evidence but do not participate.
_Avoid_: deletion, global signal state

**TaskSpaceAssociation**:
A derived, explainable relevance between one Task and one ContextSpace. A Task may have no associations or multiple associations, and their ordering may change as new signals arrive.
_Avoid_: Active Space, default Space

**TaskContextPack**:
A budgeted retrieval result for one TaskIntent revision, one current Context projection, and at most one historical EngineeringGraphSnapshot. It keeps their identities distinct and links every returned Context through typed RetrievalPaths and a safety source.
_Avoid_: Space-scoped search result, manually routed Context Pack

**RetrievalPath**:
A typed explanation of how TaskIntent or a TaskSignal made one Context relevant. Text and Applicability paths remain distinct from Engineering Graph paths; only a unique Artifact resolution captured by that Graph Snapshot can claim an engineering edge.
_Avoid_: opaque relevance score, inferred code relation

**EngineeringGraphSnapshot**:
An explicitly built, sparse historical knowledge view rooted at EngineeringReferences and their bounded ContextRelation closure. Its Context revisions and safety decisions remain immutable until another explicit Graph build, independently of later Context Store appends.
_Avoid_: current Context view, automatic index refresh

**GraphContextSnapshot**:
One immutable Context revision, its fixed relation targets, and its automatic-safety decision as included by an EngineeringGraphSnapshot. Its source Context Tree is provenance, not a current-validity condition.
_Avoid_: current Context head, live governance lookup

**EvidenceSource**:
A typed, resolvable provenance target that can ground a TaskIntent or engineering claim, such as an active TaskSignal, immutable Context Evidence, or a current unique Engineering Resolution. An opaque label or unavailable/ambiguous target is not an EvidenceSource.
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
A location containing code or a repository that supplies TaskSignals about the current engineering scene. It does not identify a requirement, select a ContextSpace, or carry durable knowledge ownership.
_Avoid_: Requirement, Space binding
