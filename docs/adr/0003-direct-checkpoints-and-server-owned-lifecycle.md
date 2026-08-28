---
status: accepted
---

# Use direct Checkpoints with a server-owned lifecycle

`task_checkpoint` accepts only `agent_kind`, `external_session_id`, complete direct `claims`, and `unknowns`. The existing Session-scope guard remains unchanged and trusts the Agent-provided locator without a new MCP transport-binding handshake; the server resolves the current Task and Intent, closes the Work Episode, derives a content-addressed operation identity, persists the durable receipt and Candidate Build outbox atomically, and returns a queued ACK. Every resulting Candidate remains untrusted until explicit human confirmation.

## Considered options

- A Hook-captured record list was rejected because mechanical tool records add a selection step without deciding which engineering conclusions are valuable.
- A caller-provided transport request key was rejected because retry identity is derived from the scoped semantic content.
- A `task_checkpoint_v2` compatibility surface was rejected because the product is not released and can replace its local runtime state directly.

Known local Runtime schemas 11 and 12 are discarded and rebuilt as schema 13 during setup or upgrade; unknown or future schemas fail closed. Hooks may still add non-factual TaskSignals and lifecycle guidance, but they never author Claim Evidence.
