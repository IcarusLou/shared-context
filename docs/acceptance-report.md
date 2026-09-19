# Shared Context Acceptance Report

Date: 2026-08-28

Implementation baseline: `8ea7136` plus the Mew #229 acceptance changes in this commit

Scope: Mew #226–#229 direct Checkpoint final truth plus the unchanged Task-first, Engineering Graph, Repository activation, Candidate governance, and team Knowledge Store baselines.

## Verdict

The public Checkpoint path is now a flat, model-owned submission contract backed by a server-owned lifecycle and durable recovery queue. The former Hook Capture selection pipeline, public list tool, client lifecycle CAS, caller transport key, and versioned `task_checkpoint_v2` alternative are explicitly superseded by [ADR-0003](./adr/0003-direct-checkpoints-and-server-owned-lifecycle.md).

The product exposes exactly 16 MCP tools. `task_checkpoint` accepts exactly `agent_kind`, `external_session_id`, `claims`, and `unknowns`; the server resolves the current Task and Intent, closes the Work Episode, derives a scoped content-addressed operation identity, and atomically persists the Checkpoint receipt and Candidate Build outbox. Direct Agent Evidence remains untrusted until a human confirms the resulting Candidate.

## Superseded historical claims

Earlier versions of this report described `task_capture_list`, `CaptureId`, Capture ingestion, caller-provided Task/Intent/Episode versions, `continue|close` boundaries, Hook-driven Episode closure, and Checkpoint relation/reference proposal fields as current product behavior. Mew #226–#228 removed those surfaces and state. Historical tests and fixture chains remain valuable only where they have been rewritten for the direct Evidence contract; historical filenames have been renamed to `direct_evidence_workflow` and `direct_relation_workflow`.

The following older milestones remain valid and are not re-proven here: Task-first retrieval without Workspace-to-Space routing, strict request-scoped Artifact Focus, sparse Engineering Graph snapshots, explicit Repository Catalog identities, Direct/Group/Disabled Session admission, untrusted Candidate Review, atomic human Confirmation, append-only Git facts, explicit team synchronization through InstallationWorkBranch, and transactional reset/uninstall. Their old Capture-specific evidence is not retained as present-tense proof.

The Task-first baseline still uses `task_intent_update` to create or revise a non-factual `WorkingIntentSnapshot` as an immutable `TaskIntentRevision`; this retrieval state is not Checkpoint Evidence.

### Historical Dynamic Replay Phase One

**NON-BLOCKING EVIDENCE.** The Phase One report's original conclusion that “production acceptance through Mew #170/#164 is unchanged” remains a clearly superseded historical statement, not evidence for the current direct Checkpoint or Codex model loop. Its sanitized fixture replay is still useful for deterministic regression diagnosis under the separate Phase One policy.

## Public Checkpoint contract

```json
{
  "agent_kind": "codex",
  "external_session_id": "external-session",
  "claims": [
    {
      "context_kind": "validation",
      "statement": "A focused engineering conclusion",
      "rationale": "Why the evidence supports it",
      "conditions": ["The condition under which it applies"],
      "evidence": [
        {
          "evidence_type": "experiment_record",
          "summary": "The directly observed result",
          "limitations": ["What was not established"]
        }
      ]
    }
  ],
  "unknowns": [
    {"statement": "One unresolved question", "blocking": false}
  ]
}
```

Required and only fields:

| Object | Exact fields |
|---|---|
| Checkpoint | `agent_kind`, `external_session_id`, `claims`, `unknowns` |
| Claim | `context_kind`, `statement`, `rationale`, `conditions`, `evidence` |
| Evidence | `evidence_type`, `summary`, `limitations` |
| Unknown | `statement`, `blocking` |

Evidence types are `source_snapshot`, `experiment_record`, and `artifact_snapshot`. Every Claim requires at least one Evidence item. Empty Claims and Unknowns are a successful mutation-free `no_op`. Every object uses `additionalProperties=false`; no Task ID, Intent revision, Episode version, boundary, Capture reference, relation/reference proposal, request key, or caller-generated identity is accepted.

## Retry, ACK, and recovery truth

The Server canonicalizes Claims/Unknowns and derives the Checkpoint operation key from exact TaskSession, Task, current Intent revision, and semantic content. Same scoped content therefore replays one stable operation, Checkpoint, Episode, Build, and set of Submission identities. A Task or Intent change creates a new scope; the protocol does not semantically deduplicate conclusions across Tasks.

The write layers are deliberately distinct:

| Boundary | Durable result | Git effect | Trust state |
|---|---|---|---|
| `task_checkpoint` first ACK | Closed Episode, Checkpoint receipt, queued Build outbox | Zero Git writes | Agent attestation only |
| Same scoped content replay | Same durable receipt and identities, `replayed=true` | Zero Git writes | Agent attestation only |
| `candidate_list` / `candidate_get` recovery | Bounded/fair or target-aware outbox progress and Pending Review | May append only untrusted Candidate proposal facts needed for review | Untrusted Candidate |
| `candidate_confirm` | Atomic final revision, association, publication, confirmation and selected Space closure | Appends accepted knowledge facts | Human-confirmed Context |

It is incorrect to claim blanket zero Git writes through Candidate reads. The precise invariant is: before explicit `candidate_confirm`, there are zero accepted Context revision, association, publication, or confirmation facts.

Lost or timed-out Checkpoint ACKs must be retried with the same Session locator and the same Claims/Unknowns field values and list ordering. Invalid shapes are corrected once rather than retried mechanically. No `transport_request_key`, `task_checkpoint_v2`, or caller lifecycle field exists. Pending/incomplete Candidate recovery is retried through list/get or the operator-only `candidate build-closed-episode` command, not by altering the Checkpoint.

## Hook and Evidence boundary

Hook adapters may merge bounded, non-factual TaskSignals from structural tool category/outcome information. They may also provide activation, bootstrap, and Checkpoint guidance. They do not select evidence, author Claims, store raw command/output/transcript content, or establish Artifact identity.

Evidence in a direct Checkpoint is an Agent attestation. `evidence_type`, `summary`, and `limitations` make it self-contained enough for Review, but do not make it a trusted team fact. Candidate Analysis and Space recommendations are also review data. Only explicit human `candidate_confirm` crosses the trust boundary.

## Acceptance matrix

| Requirement | Current evidence | Result |
|---|---|---|
| Flat exact Checkpoint fields | Rust structs, MCP JSON Schema, strict decoding, Codex declaration golden and MCP contract negative-field matrix | PROVEN |
| Session identity remains simple | Existing AuthorizedSessionScope guard accepts the Agent-provided `agent_kind + external_session_id` locator; no new MCP transport-binding handshake exists | PROVEN |
| Server-owned Task/Intent/lifecycle | Runtime locates exact ActiveTask from the submitted Session locator and creates/closes the Episode without caller lifecycle fields | PROVEN |
| Content-addressed same-scope replay | Runtime operation receipt includes TaskSession/Task/Intent plus canonical content; timeout, delayed retry and concurrency tests reuse every durable identity | PROVEN |
| Durable ACK excludes Candidate Git work | Checkpoint transaction persists receipt + outbox; ACK performance test compares committed Event state before/after 100 submissions | PROVEN |
| Recovery is bounded and fair | Candidate list limits recoverable Episodes, advances attempt generation, exposes pending/incomplete counts and is concurrency tested | PROVEN |
| Target-aware recovery | Candidate get checks exact Task ownership before recovering one Episode | PROVEN |
| Trust layering | Recovery may append Candidate proposal Event(s); confirmation tests assert accepted revision/association/publication/confirmation appear only after explicit Confirm | PROVEN |
| Direct Evidence is untrusted | Candidate list/get label Reviews untrusted; no Hook/TaskSignal evidence path exists; confirmation is explicit and CAS-guarded | PROVEN |
| No Capture product surface | No Capture domain/store/ingestion table/public tool; tool list rejects `task_capture_list`; installed flows assert no runtime Capture state | PROVEN |
| Exactly 16 MCP tools | MCP contract, installer smoke, CLI tools-list and installed client initialization | PROVEN |
| Codex first-submission legality | Exact-shape ordinary-MCP probe with CLI 0.149.1 / gpt-5.6-luna produced 99/100 accepted host Sessions and 99/99 legal submitted argument objects, with no retry or other tool call | PROVEN |
| Runtime schema policy | Setup/Upgrade discards known schema 11/12 DB + sidecars and initializes schema 13; unknown/future schemas fail closed | PROVEN |
| Transactional compatibility handling | Installer matrix verifies later failure restores schema 12 DB/sidecars bytes and permissions; schema 13 is retained | PROVEN |
| Reset/uninstall residue | Installer matrix removes runtime sidecars and cleanup-only legacy Capture paths | PROVEN |
| Candidate governance | Pending Review remains non-injectable; discard is explicit; confirm atomically produces existing/new Space fact closure | PROVEN |
| Task-first retrieval and Graph boundaries | Existing M1–M3 fixed oracles, strict Focus fallback, sparse Graph and retrieval-quality workflows remain unchanged | PROVEN |
| Repository activation and authorization | Sanitized Codex Direct and Cursor explicit-Group public Hook/MCP wire contracts plus Disabled zero-business-residue matrix | PROVEN AS CONTRACT |
| Team Knowledge Store | Two-installation hand-authored oracle retains exact RepositoryId across different checkouts and explicit work-branch synchronization | PROVEN |

## Codex-measured evidence

The final #229 review recorded these local gates:

- Installer passed 2 unit tests and all 45 `installer_matrix` tests with the installed 9098-byte workflow asset.
- The complete MCP package passed 1 unit test, the dedicated creation-ACK gate, 5 engineering workflows, and all 40 contract tests. A quiet dedicated ACK run measured p95 `234.574ms` and p99 `310.217ms`, below the 250ms/500ms gates; an earlier aggregate run after several heavy packages measured p95 `270.926ms` and was not used as passing evidence.
- Domain, Runtime, Local State, Engineering Graph, Event, Git, Index, Scenario Contract/Runner, Search, all remaining CLI binaries, and NPM packaging passed in explicit package/test runs. NPM reported 15 passed, one chartered skip, and zero failures.
- `cargo test --workspace --locked` was run twice from a quiet machine and both runs stopped at the same `retrieval_quality_workflow` `context_fts` eligibility assertion. The exact test passed immediately after each failure, and every test binary skipped by the early stop was run explicitly. No threshold or retrieval path was changed; this report does not claim that the monolithic workspace command was green.
- Workspace clippy with `-D warnings`, rustfmt, Skill validation, Markdown JSON parsing, privacy/residue checks, and `git diff --check` passed.

Manual validation Session `01a0433a-9996-7db3-b00a-591903d1faf9` exposed the old wrong-field rejection and repeated-retry behavior that motivated this redesign. It is diagnostic input, not post-change proof.

A failed interim prompt that described fields without showing the exact shape produced 89 valid Checkpoint calls out of 100; the 11 failures were 9 invalid nested-Evidence shapes and 2 no-call results. This is retained as evidence that field descriptions alone do not meet the acceptance threshold. A later safe auto-approval trial that omitted explicit authorization wording reached only 98/100 and was also rejected as final evidence.

The final exact-shape probe used Codex CLI `0.149.1`, `gpt-5.6-luna`, 8 parallel workers, ordinary MCP tool calling, 16 tools, and Checkpoint schema hash `fe13a5df7adc7e891236fedb519e58d52ec3c34ca4a1ea35bcbdf8cabe4eaa5a`. Codex `--approve-for-me` provided automatic approval in its workspace-write sandbox; the probe used no output schema, constrained decoding, or retry. It produced 99 accepted host Sessions out of 100, meeting the 99% charter threshold. All 99 actual first submissions used legal fields, returned `accepted` with `candidate_build.status=pending`, and produced 99 distinct operation IDs; forbidden field/validation errors, duplicate Checkpoint calls, and other tool calls were zero. Trial 023 reported the MCP tool unavailable and made zero calls, leaving zero residue. Knowledge Git was unchanged, and Capture paths/tables remained zero across the run.

**POST-CHANGE CODEX MODEL-LOOP RESULT: PASSED — 99/100 host Sessions accepted and 99/99 submitted first calls were legal with the exact JSON-shape workflow guidance.** Raw probe artifacts remain under ignored `target/` output and are not committed.

## Cursor contract evidence

Cursor evidence is currently deterministic contract/public-process evidence, not a model-loop measurement. Cursor adapter payload tests, sanitized lifecycle fixtures, repository-scoped public Hook/MCP wire tests, installer-generated configuration, and the installed-host process workflow validate decoding, Session admission, flat Checkpoint submission, queued recovery, Candidate Review, and SessionEnd isolation.

**POST-CHANGE CURSOR MODEL-LOOP RESULT: PENDING — no live Cursor model inference result is claimed.** Fixture profile/version fields are payload compatibility inputs, not evidence that the named model executed the workflow.

## Reproduction commands

```bash
cargo test --locked -p sctx-domain
cargo test --locked -p sctx-task-runtime --test work_episode_store
cargo test --locked -p sctx-mcp --test mcp_contract
cargo test --locked -p sctx-mcp --test checkpoint_ack_performance
cargo test --locked -p sctx-cli --test direct_evidence_workflow
cargo test --locked -p sctx-cli --test direct_relation_workflow
cargo test --locked -p sctx-cli --test episode_lifecycle_hooks
cargo test --locked -p sctx-cli --test installed_live_host_workflow
cargo test --locked -p sctx-cli --test repository_scoped_activation_acceptance
cargo test --locked -p sctx-cli --test repository_scoped_context_acceptance
cargo test --locked -p sctx-cli --test retrieval_quality_workflow
cargo test --locked -p sctx-installer
cargo test --locked -p sctx-installer --test installer_matrix
python3 -B tests/scripts/codex_checkpoint_model_probe.py --trials 100 --parallelism 8

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

## Residue gate

The expected product result is zero Capture types, IDs, stores, ingestion tables, task tools, workflow steps, runtime directories, or metadata files. Allowed textual residue is limited to:

- ADR-0003 and this report identifying the rejected/superseded design;
- installer cleanup-only handling of legacy `state/capture`, `capture.lock`, and `capture-metadata.json`;
- negative assertions proving those surfaces and files remain absent.

No result in this report proves background synchronization, automatic Pull Requests, token billing reduction, physical MCP process removal, untracked-file scanning, or a Codex/Cursor model loop unless explicitly identified as such.
