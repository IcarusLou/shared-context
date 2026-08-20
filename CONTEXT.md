# Shared Context

Shared Context organizes durable engineering knowledge around the intent behind work while allowing an Agent's current task to discover that knowledge without prior routing.

## Language

**ContextSpace**:
A stable container that organizes the intent and durable Context of one requirement or long-running objective. It is a knowledge-governance boundary, not a retrieval partition or property of a Workspace.
_Avoid_: Active Space, Workspace Space, search partition

**TaskIntent**:
The current structured understanding of a Task's goal, desired change, scope, constraints, and unknowns. It is independent of every ContextSpace and may evolve as the Task proceeds.
_Avoid_: Prompt, Space selection

**TaskSignal**:
An observable input that informs TaskIntent or knowledge retrieval, such as a Prompt, Workspace, Repository, File, Symbol, Diff, API, Schema, or Test observation. A signal is evidence about the current Task, not a declaration of Space membership.
_Avoid_: Space binding, routing key

**TaskSpaceAssociation**:
A derived, explainable relevance between one Task and one ContextSpace. A Task may have no associations or multiple associations, and their ordering may change as new signals arrive.
_Avoid_: Active Space, default Space

**Workspace**:
A location containing code or a repository that supplies TaskSignals about the current engineering scene. It does not identify a requirement, select a ContextSpace, or carry durable knowledge ownership.
_Avoid_: Requirement, Space binding
