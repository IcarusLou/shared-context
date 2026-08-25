---
status: accepted
---

# Isolate remote Knowledge Store writes by installation branch

An installation may bootstrap its KnowledgeStore from a non-empty team Git remote, but it treats the remote default branch as a read-only integration base and immediately checks out `shared-context/<installation-id>` without pushing. The install manifest retains the transport type, default branch, stable installation ID, work branch, and a URL digest rather than the raw URL; this enables idempotent setup and prepares protected-branch synchronization without confusing the KnowledgeStore remote with a business source RepositoryId.
