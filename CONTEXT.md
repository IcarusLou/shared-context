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
A budgeted, automatically safe retrieval result for one TaskIntent revision and one exact knowledge-projection snapshot. It contains zero or more TaskSpaceAssociations and links every returned Context to one of those associations through typed RetrievalPaths.
_Avoid_: Space-scoped search result, manually routed Context Pack

**RetrievalPath**:
A typed explanation of how TaskIntent or a TaskSignal made one Context relevant. M2 paths describe Intent FTS, Context FTS, exact Applicability, or exact textual engineering hints; they must not claim an Engineering Graph edge that has not been resolved.
_Avoid_: opaque relevance score, inferred code relation

**RepositoryIdentity**:
A stable identity for one logical source repository across local checkouts and machines. A Workspace path, branch, or Commit is never Repository identity.
_Avoid_: checkout path, repository URL as identity

**EngineeringReference**:
A persistent, non-authoritative observation that Context relates to an engineering object through locator or fingerprint hints. It survives resolution failure and never claims that a current Artifact was found.
_Avoid_: resolved Artifact, file identity

**EngineeringArtifact**:
A currently discovered Repository, Module, File, Symbol, API, Schema, or Test in an available engineering snapshot. It is derived state identified by a deterministic ArtifactKey and can be rebuilt.
_Avoid_: Context fact, path identity

**ArtifactResolution**:
The current resolved, ambiguous, stale, unavailable, or unresolved interpretation of one EngineeringReference. It is derived state and may change without changing the Reference.
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
