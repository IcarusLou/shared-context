# Session replay audit tools

Scripts for turning a coding-agent session (an original real one, and
optionally a replayed one produced by `replay.py`) plus sctx's own database
rows into a compact, human- and agent-readable audit bundle.

Two hosts are supported — **Codex** (`codex`, rollout `.jsonl` under
`~/.codex/sessions`) and **Cursor Agent** (`cursor-agent`, transcript `.jsonl`
under `~/.cursor/projects/*/agent-transcripts`). Everything downstream of the
host parser is shared: one `Session` model, one Markdown digest, one
`facts.json` schema, one bundle layout, one manifest schema. `/session-review`
does not branch on host. What it *does* have to read is the digest's
`## fidelity` block, because a Cursor transcript is structurally much thinner
than a Codex rollout (no tool results, no injections, no token usage) and an
absence there is a missing record, not a finding.

## bundle

`bundle.py` builds the bundle:

```
<audit-root>/<replay-id-or-orig-id>/
  original.md    digest of the original session
  replay.md       digest of the replayed session (only if a manifest with replayed_thread_id exists)
  facts.json      hard facts computed per side ("original", "replay")
  sctx/           raw JSON exports of the sctx rows used, per side
```

Default audit root: `~/.shared-context-audit`.

Bundle a real session that was never replayed (writes only `original.md`,
`facts.json`, `sctx/` under `<audit-root>/orig-<short id>/`):

```sh
python3 tests/scripts/session_replay/bundle.py --session <thread_id>
```

`--agent auto|codex|cursor` (default `auto`) picks the host. `auto` resolves
the id against `~/.codex` first and `~/.cursor` second and names both misses if
neither owns it; with `--replay-id` the manifest's own `agent` field wins.
`facts.json` records the resolved host as a top-level `agent` key and per side
as `original.host` / `replay.host`.

Bundle a session alongside its replay (reads
`<audit-root>/<replay-id>/manifest.json` written by `replay.py`, and writes
into that same directory):

```sh
python3 tests/scripts/session_replay/bundle.py \
  --session <original_thread_id> --replay-id <replay-id> \
  [--audit-root ~/.shared-context-audit] [--home <HOME whose .shared-context to read, default real>]
```

Modules:
- `session_model.py` — host-agnostic `Session`/`Turn`/`SctxCall` dataclasses
  shared by every host parser and by `bundle.py`, split (2026-09-10) into two
  distinct channels: `HookInjection` (the hook `additionalContext`
  marker/reminder channel — small, <=506 bytes observed on every September
  rollout, and it never carries the Context Pack) and `PackDelivery` (the
  Context Pack itself, delivered as the RESULT of an sctx MCP tool call —
  see `hosts/codex.py`). An earlier version of this model folded both into
  one `Injection` type, which mislabeled the pack's own channel-cap
  truncation as "hook truncation" — see `hosts/codex.py`'s module docstring
  for the correction.
- `hosts/cursor.py` — parses a Cursor Agent transcript `.jsonl` into a
  `Session`, and rebuilds the pack-delivery/hook timeline from sctx's own
  state because the transcript carries neither. See its module docstring for
  the verified line shapes, the human-prompt rule, the dynamic-tool wrapper
  sctx MCP calls arrive in, and the turn-windowing used for reconstruction.
  See "### cursor" under bundle below for the fidelity caveats.
- `hosts/codex.py` — parses a Codex rollout `.jsonl` into a `Session`. See
  its module docstring for the verified rollout line shapes, the human-
  prompt / hook-injection detection rules, and — the important part — where
  the Context Pack actually travels: as the MCP tool RESULT of
  `task_intent_update`/`task_context`, never over the hook channel. It is
  wrapped in an `exec` code-mode script in every session observed so far
  (`channel="code_mode_script"`), which is what can truncate it (Codex's own
  ~10,000-token/~40,000-byte script-output cap — see
  `CODE_MODE_SCRIPT_CAP_BYTES`), independent of the model discarding the
  pack itself via a `.then(...)` wrapper before ever rendering it (see
  `discard_wrapper` below) — two different failure modes, kept separate in
  `facts.json`.
- `sctx_facts.py` — queries `<HOME>/.shared-context/state/runtime.sqlite`
  read-only for one external session's rows across every sctx table it can
  link (task_session, task_injection, context_usage, candidate_review,
  hook_event, ...), exports them to `sctx/<table>.json`, and returns a
  summary dict for `facts.json`'s `sctx_db` field. Also has
  `get_lease_facts()`: whether the session's `AuthorizedSessionScope`
  activation-lease file still exists under
  `<HOME>/.shared-context/state/authorized-session-scopes/` — if it does,
  `SessionEnd` never cleaned it up. See its module docstring for the
  verified join keys, the lease file's digest formula, and a couple of
  schema surprises (multiple `task_session` rows per external session;
  `hook_event`/`auto_confirm_rejection.external_session_id` is the raw host
  thread id, not the `xss_...` id; `hook_event` itself is effectively dead
  on current installs — see `sctx_logs.py`).
- `sctx_logs.py` — reads `<HOME>/.shared-context-logs/state/` (where hook
  decisions actually land on current installs):
  `hook-diagnostics.json`'s `recent_events` filtered down to this session by
  a sha256 digest of `(agent_kind, external_session_id)` (a different digest
  formula than the lease file's — see the module docstring),
  `collector-status.json`, `upload-status.json` (surfaces a `logs_sync`
  failure), and the `spool/ready/` batch count. Feeds `facts.json`'s
  `sctx_logs` block.
- `codex_trust.py` — reimplements Codex's hook **trust hash** so `replay.py
  --sctx-bin` can write a `hooks.json` of its own and still have Codex run it.
  See "Replaying a dev build" under replay for the algorithm, the pinned
  openai/codex commit it was read at, and the verifier
  (`python3 codex_trust.py --config ~/.codex/config.toml`) that recomputes every
  real `[hooks.state]` entry as proof.
- `tests/test_codex_trust.py` — the hash's normalization rules (absent matcher,
  matcher-dropping events, timeout defaults and the SessionEnd clamp,
  `additionalContextLimit`, positional handler indices, skipped handler kinds)
  and `replay.py`'s retargeting glue, all against synthetic hooks.json /
  config.toml files built inline — no real hook command, path or hash.
- `tests/test_codex_parser.py` — unittest against a synthetic minimal
  rollout built inline in the test file (no real transcript text), plus the
  discard-wrapper regex tests.
- `tests/test_cursor_parser.py` — same discipline for the Cursor parser: a
  synthetic transcript, a synthetic three-table `runtime.sqlite` and a
  synthetic `hook-diagnostics.json` built inline, covering human-prompt
  detection, the dynamic-tool unwrapping, the truncated-arguments case, and
  the turn-windowed reconstruction (including the degrade-to-session-level
  path for an undated transcript).

Run them all with
`python3 -m unittest discover -s tests/scripts/session_replay/tests`.

`facts.json` per side also carries: `discard_wrapper` (turns where an exec
call's FULL, untruncated args text matches a `.then(r=>({key:r.key}))` /
`.then(r=>{...return {isError:...}})` shape, meaning sctx's response likely
never reached the model's own context — shown in the digest under that
turn's `### tools` section too) and `versions` (the session's own Codex CLI
version, the sctx hook's `--agent-version` string from `hooks.json`, the
`bin/current` symlink target, and `sctx --version` output — noted as
reflecting the install at *bundle* time, not necessarily at *session* time).

Validated against a real 2-turn session
(`01a08017-95a0-7841-8ab9-09b582c62cb1`): turn 1's `pack_deliveries[0]` had
16 items (`bytes=31,727`, the compact pack sctx computed) delivered over
`channel="code_mode_script"` with `channel_cap_bytes=40,000`, and it WAS
truncated by that channel's own cap — `delivered_bytes=40,154`,
`truncated=true`, `original_token_count=22949` parsed straight off the
"Warning: truncated output (original token count: 22949)" marker on the
paired `custom_tool_call_output`. This is Codex's script-output cap, not a
hook cap — `hook_injections` in the same turn total 959 bytes across 3
entries, nowhere near a cap of any kind. Turn 2's pack had 15 items
(`bytes=32,094`) but the model's own `.then(r=>({isError:r.isError}))`
wrapper discarded it before ever calling `text()` on it, so
`delivered_bytes=438` while `truncated=false` — a different failure mode
from turn 1's, kept distinguishable in `facts.json.pack_deliveries.per_turn`
and separately caught by `discard_wrapper`; do not read turn 2's
`truncated=false` as "the pack arrived intact". `checkpoint_reminder_count`
was 2; the assistant text cited zero `ctx_` ids. `sctx_logs` shows this
session's 27 session-digest-matched hook events ending in `turn_stop` with
no `session_end` among them, and `lease.lease_file_exists=true` two days
later — together making the human review's "SessionEnd payload
undecodable, lease not cleaned" finding decidable from the bundle rather
than only observable by reading the raw session. All of the above are
reproduced by `facts.json`.

### cursor

```sh
python3 tests/scripts/session_replay/bundle.py \
  --session <conversation_id> --agent cursor
```

The conversation id is the directory name under
`~/.cursor/projects/<cwd-slug>/agent-transcripts/`, and is simultaneously the
hook payload's `conversation_id`/`session_id` and sctx's
`external_session_key` with `agent_kind='cursor'` — one id, no mapping table.
`sctx_facts.py` and `sctx_logs.py` needed no change to serve it: both already
take `agent_kind` as a parameter, and `telemetry_session_digest` is gated on
`{"cursor","codex"}` in the Rust source it mirrors.

**Fidelity caveats — read these before treating a Cursor absence as a
finding.** They are printed at the top of `original.md` as a `## fidelity`
block and repeated in `facts.json` as `original.fidelity_notes`.

- **Tool outputs do not exist in a Cursor transcript.** There are no
  `tool_result` blocks at all — not for shell, not for reads, not for MCP. So
  every `### sctx calls` entry shows `status=unknown` and
  `result: (not recorded by this host)`, and `facts.json` reports
  `sctx_calls.results_unavailable`. Whether a `task_checkpoint` was accepted
  or rejected is only decidable from `sctx/original/agent_checkpoint.json`
  and friends, never from the digest's call list. (Large outputs are spilled
  to `projects/<slug>/agent-tools/<uuid>.txt`, but `tool_use` blocks carry no
  id and those files are shared across every conversation in the project, so
  they cannot be attributed back to a call and are deliberately not read.)
- **Pack deliveries are reconstructed, not observed** (`hook_injections` is
  simply always empty — see below). Cursor never writes a hook's
  `additionalContext` into the transcript, and it never records an MCP tool
  result either, so there is no wire evidence for the Context Pack at all.
  Every `PackDelivery` on the Cursor side is rebuilt from `runtime.sqlite`'s
  `task_injection` rows and carries `reconstructed_from=sctx:task_injection`
  and `channel="unknown"` on its digest line, with the provenance in the
  digest's `## reconstruction` block, in
  `facts.json.original.reconstruction`, and raw in
  `sctx/original/reconstruction.json`. Consequences:
  - `ctx_ids` and the delivery *time* are real; the delivered **text** is
    recoverable from nowhere, so `bytes` is **0 and `delivered_bytes`/
    `channel_cap_bytes` are `None`/`"unknown"` as sentinels, not as a
    measurement**. Never compare a Cursor `bytes`/`delivered_bytes` against
    a Codex one.
  - Rows sharing a `(source, injected_at_unix_seconds)` pair are one
    delivery event, so `pack_deliveries.count` counts pushes (comparable
    across hosts), not context ids (`pack_deliveries.ctx_ids_total` is
    that).
  - `tool` is the sctx `source` name prefixed with `sctx:`
    (`sctx:intent_update`, `sctx:task_context`, `sctx:artifact_focus`), not
    a real MCP tool-call name, because there is no call-level record to read
    one from.
  - Attribution to a turn uses the `<timestamp>` tag Cursor prepends to each
    user message. It is **minute-granular**, so a row within ~60s of a turn
    boundary can land on either side. Turn 1's lower bound is left open so the
    session-start hook lands in turn 1. A transcript with no timestamp tags
    degrades to session-level: nothing is guessed into a turn, and the rows
    appear under `reconstruction.unwindowed` instead.
- **`hook_injections` is always empty and `### hook injections` never
  appears** — Cursor's transcript shows no hook output at all (not even the
  small marker/reminder text Codex's hook writes), so there is nothing to
  read or reconstruct it from. This is a structural absence, not evidence
  the hook never ran; see the next bullet for what Cursor *can* show about
  hook activity.
- **Hook activity is a separate, reconstructed block.** Per-turn
  `### hook events` come from `~/.shared-context-logs/state/hook-diagnostics.json`
  filtered by this session's digest. They say a hook *ran*; they do not say
  context arrived, which is why they are not folded into `pack_deliveries`.
- **No token usage, model, git commit, cwd or compaction markers.** `### usage`
  is all zeros on the Cursor side and means "not recorded". The `cwd` in the
  header is *recovered* — from sctx's activation lease `startup_cwd`, else by
  slugifying each registered checkout path and matching the project directory
  name — and the header's fidelity notes say which. Cursor's project slug is
  lossy (`/` and `_` both become `-`) and cannot be inverted.
- **Arguments can be truncated by the host.** `CallDynamicTool.input.arguments`
  is normally an object but is sometimes a JSON *string*, and in the validation
  fixture one such string was cut off mid-object. That call is preserved
  verbatim under `__arguments_unparsed__`, counted as
  `sctx_calls.arguments_unparsed`, and excluded from both the claims tally and
  the `external_session_id_verbatim` check, which it would otherwise fail for
  the wrong reason.
- **Cursor's own follow-up prompts are not turns.** A `role: "user"` message
  whose `<user_query>` opens with "Briefly inform the user about the task
  result…" or "The beginning of the above subagent result is already visible…",
  or which carries no `<user_query>` at all, is the client talking. It folds
  into the preceding human turn and is listed under `## other injections`, so
  `turns == human_prompts` still holds.

Validated against `7d8cbab1-0d90-4155-962b-7654f6f5bfcd` (2026-09-08, Cursor
2026.08.31, the FE monorepo): 4 human turns (1 host follow-up prompt skipped),
138 tool calls of which 12 are sctx MCP calls — `task_intent_update`×3,
`task_checkpoint`×4, `candidate_list`×4, `candidate_discard`×1 — carrying 7
claims across the 3 checkpoint calls whose arguments survived intact, and 1
whose arguments the host truncated. 20 `task_injection` rows group into 3
pack-delivery events (20 distinct context ids), all landing in turn 3, and 328
hook-diagnostics events match the session digest. `pack_usage` shows 0 of
those 20 ids cited back in assistant text or in later sctx call arguments,
while `sctx_db.context_usage` records all 20 as `ignored`. Note the
cross-check the two sides make possible: the transcript contains **no**
`candidate_confirm` call, yet `candidate_review.json` holds 7 rows, 4
`confirmed` and 3 `discarded`, all `decision_source: human` — a gap that is
only visible because the bundle carries both halves.

## replay

`replay.py` re-runs the *human* half of a real Codex session — its prompts, in
order, verbatim — against a fresh headless Codex session, so that `bundle.py`
has a second transcript to compare the original against. Fidelity is the bar:
the replay reuses the original's commit, sandbox policy, model, Codex config,
hooks and Shared Context Repository identity, and everything it could not
mirror is written into the manifest under `deviations` and `warnings`.

```sh
python3 tests/scripts/session_replay/replay.py --session <thread_id> \
  [--turns N|all] [--audit-root ~/.shared-context-audit] \
  [--checkout worktree|inplace] [--codex-home isolated|real] [--sctx-bin PATH] \
  [--driver app-server|exec-resume] [--on-question stop|next-prompt] [--dry-run]
```

`--dry-run` resolves the original and prints the plan (commit, branch, model,
sandbox, approval policy, prompt count and per-prompt length) without running
anything or creating any directory. Use it before committing minutes of agent
time to a large repository.

Output, under `<audit-root>/<replay-id>/` where `<replay-id>` is
`<UTC timestamp>-<first block of the original thread id>`:

```
manifest.json          everything bundle.py needs, plus the isolation evidence
home/                  the replay's HOME (its own .shared-context snapshot)
codex-home/            the replay's CODEX_HOME, including sessions/<the new rollout>
host/turn-<n>.jsonl    the raw `codex exec --json` stream for each turn
host/turn-<n>.stderr
```

and one detached worktree at `<audit-root>/worktrees/<replay-id>`.

### Driver: one app-server process, not one `codex exec` per turn

`--driver app-server` (the default) drives the whole replay through a single
`codex app-server` process over JSON-RPC on stdio: `initialize`, then
`thread/start` once, then `turn/start` per prompt, watching the `turn/completed`,
`item/completed`, `hook/started` and `hook/completed` notifications.

The reason is not tidiness. `codex exec resume` starts a *new process* for every
turn, and every start replays the SessionStart hook — so a two-turn replay
carried the Shared Context activation marker twice where the interactive
original carried it once (`injections.with_marker: 2` against the original's
`1`). Injected context is precisely what this audit measures, so an artifact of
the driver landing in that number makes the comparison worthless. Verified with
app-server on a real replay: exactly **one** activation-marker developer message
in the rollout, and hook operations `session_start x1, prompt_submit,
post_tool_use x N, turn_stop, session_end` — the interactive shape.

`--driver exec-resume` keeps the per-turn-process behaviour as a fallback if the
app-server protocol changes. It writes `session_start_per_turn: true` into the
manifest's `deviations`, so nobody compares marker counts across drivers by
accident.

Token usage comes from `thread/tokenUsage/updated`, not from `turn/completed`
(which carries no usage in this protocol version).

### When the agent asks a question

A replayed agent that calls `request_user_input` — or simply ends its turn with a
question — is waiting for a human who is not there, and everything after that
point diverges from the original by construction. `--on-question` decides what
happens:

- `stop` (default): the turn is finished and recorded, the question is written to
  `manifest.stopped_on_question` and to that turn's `question` field, and the
  replay ends there rather than pretending the remaining prompts still line up.
- `next-prompt`: the next original prompt is fed back as the answer (usually what
  the human typed next anyway), and the turn is marked
  `answered_with_next_prompt`.

Blocking `item/tool/requestUserInput` server requests are always answered so the
turn can complete — with the next prompt under `next-prompt`, with an empty
string otherwise. A turn is never left deadlocked against an unanswerable
question.

### Isolation model

Four things are separated from the operator's real installation, because a
replay that writes into it would corrupt the very state the audit measures.

- **`HOME`** points at `<replay-id>/home`, built as an **overlay**: every
  top-level entry of the real `~` is symlinked into it except the four in
  `HOME_OVERLAY_EXCLUSIONS`, which get isolated copies. `sctx` resolves its
  installation root as `$HOME/.shared-context` with no environment override, so
  redirecting `HOME` is the only way to move it — but a *bare* redirected HOME is
  a different machine, and the agent notices. The first end-to-end replay
  diverged on turn 1 for exactly that reason: the original had read a document
  through the corporate `lark-cli`, and the replay, whose HOME held only
  `.shared-context`, answered that this machine has no `lark-cli` configured. The
  overlay gives the agent the same machine.
  - A tool that writes *through* one of those symlinks writes into the real HOME.
    That is accepted: `.codex`, `.cursor` and the two `.shared-context*`
    directories are the only state whose contamination would corrupt the audit,
    and those are the ones excluded. `.cursor` is symlinked for Codex replays and
    isolated for Cursor ones.
  - The audit root itself is never symlinked in, even when it lives under `~`:
    the replay HOME is inside it, and the link would make every tree walk from
    the replay HOME infinite.
  - Inside the isolated `.shared-context`, `state/` and `repository/` are
    snapshotted (databases through SQLite's backup API, so a live writer cannot
    hand over a torn copy), `config.toml` is copied with its absolute paths
    rewritten, and the 2.3G `embedding/` plus `bin/` are symlinked back to the
    originals.
  - `<home>/.shared-context-logs/` is **configured**, not left empty. Hook
    decisions are not written to `runtime.sqlite`'s `hook_event` table any more;
    the log collector aggregates them into `state/hook-diagnostics.json`, and with
    no config there is no collector and no file — which is why the first replay
    reported `sctx_logs.diagnostics_file_found: false` for its own side.
    `replay.py` runs `sctx logs init --logs-root <home>/.shared-context-logs`
    **with `HOME` unset**, which yields a config with no `remote` (nothing to
    upload to) and skips the launchd reconciliation, so the operator's real
    `com.shared-context.logs` / `logs-sync` services are untouched; `on_maintain`
    is then flipped to `false`. A `sctx logs collect` child process runs for the
    lifetime of the replay and is stopped afterwards.
  - The snapshot directories are created `0700` on purpose. `sctx` refuses to
    open an installation root or state directory that is not private, and it
    fails *closed*: hooks return `{}` and Shared Context silently never
    activates. A world-readable snapshot produces a replay that looks like it
    ran with the feature switched off.
- **`CODEX_HOME`** points at `<replay-id>/codex-home` (`--codex-home isolated`,
  the default), so the new rollout, history and thread index land inside the
  audit directory instead of beside the operator's own sessions. `config.toml`,
  `hooks.json` and `auth.json` are copied; `plugins/`, `skills/`, `rules/` and
  the other read-only caches are symlinked.
  - Codex 0.153.4 will not run a hook unless `config.toml` carries a matching
    `[hooks.state."<absolute hooks.json path>:<event>:0:0"] trusted_hash`, and
    that key is *keyed by the path*. A verbatim copy into a new `CODEX_HOME`
    therefore trusts nothing and the hooks never fire. Without `--sctx-bin`,
    `replay.py` rewrites only the path prefix inside those keys — the hashes
    cover the hook entries, which are copied byte for byte — which restores trust
    without needing `--dangerously-bypass-hook-trust`. This is verified working:
    the replayed rollout contains the `hooks.additional_context` developer
    message carrying
    `<shared-context-active external_session_id="<new thread id>">`. With
    `--sctx-bin` the hook entries *have* to change, so the hash is recomputed
    instead — see "Replaying a dev build" below.
  - `--codex-home real` is the fallback if a future Codex changes that scheme.
    `HOME` still points at the replay snapshot; only the rollout moves back to
    `~/.codex/sessions`.
- **The checkout** is a `git worktree` at the original commit on a local branch
  named `replay/<original branch>` (`--checkout worktree`, the default). A
  *detached* worktree reports `git_branch` as `-`, which propagates into the
  replayed session's metadata and into every hook that reads it, so the replay
  stops being comparable on a field the audit reads; if the branch name is taken
  the replay id is appended, and if the branch cannot be created at all the
  worktree falls back to detached with a warning. The repository under replay is
  never written to — only `git worktree add`/`remove` touch it. If the commit is not
  present locally the run fails and names the commit and branch to fetch.
  `--checkout inplace` runs in the original directory instead and prints a loud
  warning; it does not stash, reset or restore anything.
- **The Repository registration.** Shared Context authorizes a session by
  longest-prefix match of its startup directory against the registered checkout
  paths, so a worktree at a brand-new path would activate nothing. `replay.py`
  finds the Repository identity that owns the original `cwd` and registers the
  worktree as a second checkout of that same identity *inside the snapshot*, which
  keeps the knowledge base identical. If the original `cwd` belongs to no
  registered Repository, the replay runs with Shared Context disabled — which is
  what the original session did too — and says so in `warnings`.

`manifest.json.isolation` carries the proof, not just the intent: row-count
fingerprints of the real `~/.shared-context` taken before and after the run,
`real_shared_context_unchanged`, and the replayed thread's
`external_session`/`task_injection`/`context_usage` rows looked up in *both*
the replay state and the real state.

### Replaying a dev build (`--sctx-bin`)

By default a replay can only exercise the `sctx` that is *installed*, because the
isolated `CODEX_HOME` copies the real `hooks.json` whose commands hard-code
`/Users/<you>/.shared-context/bin/current/sctx`. `--sctx-bin
target/release/sctx` replays a dev build instead, without touching the real
installation:

```sh
cargo build --release -p sctx-cli
python3 tests/scripts/session_replay/replay.py --session <thread_id> --turns 1 \
  --sctx-bin target/release/sctx --turn-timeout 2400
```

Five things change, and only when the flag is given:

- **The binary is copied**, not symlinked, to
  `<replay home>/.shared-context/bin/current/sctx`. `sctx` resolves its
  installation root from `$HOME` with no override, so this is the only place it
  can go; a symlink back into `~/.shared-context/bin` would silently become the
  operator's binary again the moment the real install is upgraded mid-run.
  `embedding/` stays a symlink as before (2.3G nothing writes to).
- **`hooks.json` is written, not copied.** Every hook command whose path matches
  `…/.shared-context/bin/…/sctx` is retargeted at the dev binary; the event, the
  `--agent codex --agent-version '<x>'` arguments, the quoting and the status
  message are left byte-identical, because the argument string is part of what is
  being replayed. Hooks belonging to anything else (a corporate telemetry plugin,
  a project `.codex/hooks.json`) are untouched.
- **The hook trust hashes are recomputed** — see below.
- **`mcp_servers.shared-context.command`** in the isolated `config.toml` is
  pointed at the same binary. This matters specifically for the default
  `app-server` driver, which passes no `-c` overrides, so the copied
  `config.toml` is the only place the MCP command is stated.
- **The managed skill bundles come from the binary's own source tree.**
  `~/.agents/skills/{shared-context,sctx-review}` is what `sctx setup` writes out
  of bytes the binary carries (`SKILL_ASSETS` in `crates/installer`, which are
  `include_bytes!` off `skills/` at build time). Symlinking the real `~/.agents`
  through the HOME overlay would therefore run a dev binary against the
  *installed* skill text — a silent mismatch on the input that steers the agent
  hardest. So `.agents` is excluded from the overlay and rebuilt: the two managed
  bundles are copied from the `skills/` tree found above the binary, every other
  skill the operator has is symlinked through, and the manifest's `skill_bundle`
  records the source path plus a sha256 and byte count for every file, so "which
  skill bytes were in effect" is answerable from the bundle rather than guessed.

`manifest.sctx_bin` records the source path, the installed path, the sha256 and
the `--version` output. Read the **sha256**, not the version: a dev build and the
installed release print the same workspace version string, so only the digest
tells them apart.

Everything else is unchanged. Without `--sctx-bin` not one of the five happens,
and the manifest carries `sctx_bin: null` / `skill_bundle: null`.

`replay_cursor.py` takes the same flag and does the first, second, fourth and
fifth of those. Cursor needs no third: it registers hooks by plain command with
no hashes anywhere, and `build_cursor_home` already *generates* its `hooks.json`
and `mcp.json` against `<replay home>/.shared-context/bin/current/sctx`, so
placing the dev binary there is the whole job.

#### The trust hash, reproduced

Retargeting a hook command changes the hook entry, which changes the hash Codex
recomputes at discovery time, which makes the operator's stored `trusted_hash`
read as `Modified` — and a `Modified` hook does not run. So `codex_trust.py`
reimplements Codex's own hash, `replay.py` drops the operator's `[hooks.state]`
tables for `~/.codex/hooks.json` and appends freshly computed ones for the
isolated file. No `--dangerously-bypass-hook-trust`.

The algorithm, read from openai/codex at commit
`c7c824dce4da186e5142af5d9a1587ae553efe46` (the exact files are cited in
`codex_trust.py`'s module docstring): build
`{event_name: "<snake_case label>", matcher?: <event-adjusted matcher>, hooks: [<normalized handler>]}`,
serialize it to TOML, convert to JSON, sort every object's keys recursively,
serialize compactly, sha256 it, and prefix `sha256:`. The parts that are easy to
get wrong and are each covered by a unit test:

- a `None` matcher (or `commandWindows`, or `statusMessage`) contributes **no
  key at all**, because `toml::Value::try_from` drops None-valued entries —
  a JSON `null` would give a different digest;
- `UserPromptSubmit`, `Stop` and `Interrupt` drop their matcher even when the
  group states one;
- the timeout is normalized *before* hashing, so an entry that states none
  hashes as `600` — except `SessionEnd`/`Interrupt`, which default to `1` and
  clamp to `3`;
- an `additionalContextLimit` equal to the 2,500-token default, or on an event
  that cannot emit `additionalContext`, is dropped;
- handler indices are positional, so a `prompt`/`agent`/empty-command handler
  Codex skips still consumes its index.

**The proof is a check against the real installation, not a fixture.** Run

```sh
python3 tests/scripts/session_replay/codex_trust.py --config ~/.codex/config.toml
```

and it recomputes, from the hooks files themselves, every `[hooks.state]` entry
whose key source is a path that exists on disk. On the validation machine that is
**24 of 24 matched, 0 mismatched**, across three different `hooks.json` files —
6 entries for `~/.codex/hooks.json` (the Shared Context hooks), 8 for a project
file that uses matchers and explicit timeouts including a clamped `SessionEnd`,
and 10 for another — covering 10 of the 12 event kinds. Three further entries are
reported as `missing_from_hooks_file`: stale state for events the current
`hooks.json` no longer declares, which is a leftover rather than a disagreement.
Eleven plugin-provided sources (`<plugin>@<pack>:hooks/hooks.json:…`) name no
readable path and are reported as unresolved rather than counted either way.

`replay.py` runs that same comparison for `~/.codex/hooks.json` at the start of
every `--sctx-bin` run and writes the result into
`manifest.codex_home.trust.operator_entries_reproduced` (e.g. `"6/6"`). If a
future Codex changes the algorithm the count drops and the manifest says so,
instead of the replay quietly running with its hooks disabled.

**Validated end to end** on 2026-09-10, replaying turn 1 of `01a06646-…` (an
Android monorepo session originally run on 2026-09-03) against
`target/release/sctx` built from this checkout:

- `manifest.codex_home.trust` — `mode: "recomputed"`,
  `operator_entries_reproduced: "6/6"`, 9 stale state keys dropped, 6 written,
  6 hook commands retargeted; `sctx_bin_retargeted` and
  `mcp_command_retargeted` both true.
- **The hooks fired against the dev binary.** The replayed rollout carries the
  `hooks.additional_context` developer message naming the *new* thread id, and
  the replay side's `hook-diagnostics.json` (`facts.replay.sctx_logs`) holds 106
  events for the session — `session_start` 1, `prompt_submit` 21,
  `post_tool_use` 83, `session_end` 1, every one `success` or `degraded`, none
  refused.
- **The marker proves *which* binary.** The original's marker is one sentence;
  the replay's carries the dev build's `## session` policy lines on top of it —
  "Stored text is Chinese, with identifiers, paths, commands, and error codes in
  their original spelling…" and "Record one `progress` summary per task
  boundary, never per turn." Neither line exists in the original.
- `manifest.sctx_bin` — sha256 `01c0b933…`, 24,174,304 bytes, `sctx
  0.2.0-dev.9`, copied from `target/release/sctx`. `facts.json`'s `versions`
  block now carries `sctx_bin_sha256` for exactly this reason: both sides print
  `sctx 0.2.0-dev.9`, and only the digest separates the replay's `01c0b933…`
  from the original's installed `f7ba9342…`. `sctx_bin_current_target` being
  `null` on the replay side (a real directory holding a copy, not the
  installation's symlink) is the second tell.
- `manifest.skill_bundle` — six files from
  `/Users/bytedance/workspace/shared-context/skills`, including
  `shared-context/references/workflow.md` at **15,159 bytes**
  (`f792fb0e…`), which is the shrink-to-protocol revision; the installed bundle
  at the time was a different 19,644-byte file. 34 other skills symlinked
  through.
- **The real installation is untouched.** `~/.codex/hooks.json`,
  `~/.shared-context/config.toml`, `~/.shared-context/bin/current` and every
  file under `~/.agents/skills/shared-context` are byte-identical before and
  after, `~/.codex/config.toml` still holds its own 38 `[hooks.state]` entries
  with no `shared-context-audit` path anywhere in it, and
  `manifest.isolation.replayed_thread_in_real_state` shows
  `external_session_id: null` for the replayed thread. Note that
  `real_shared_context_unchanged` came back **false** on this run and that is
  *not* contamination: other real Codex sessions were running on the machine
  during the 40 minutes and wrote their own rows (the newest real
  `external_session` belongs to an unrelated thread). The per-thread lookup, not
  the whole-database fingerprint, is the load-bearing check when the machine is
  busy.

### Known deviations from an interactive session

- **Approvals.** The original ran `approval_policy = "on-request"` with a human
  (or the auto reviewer) answering. `codex exec` cannot prompt, so turn 1 uses
  `--approve-for-me` and every turn sets `approvals_reviewer = "auto_review"`.
  Note `--approve-for-me` hard-codes the workspace-write sandbox and refuses to
  sit beside `--sandbox`; when the original used a different sandbox the flag is
  dropped and `approvals_reviewer` alone carries approvals, because mirroring the
  sandbox matters more.
- **No human latency.** Turns follow each other as fast as the model finishes.
  Anything that depends on wall-clock gaps — maintenance windows, staleness
  timers, a human reading the answer before the next prompt — will not reproduce.
- **Sandbox roots.** The sandbox `type` and `network_access` are mirrored, but
  the original's per-session writable roots (Codex adds a `visualizations/<id>`
  directory of its own) are not.
- **The working tree.** A worktree is the original *commit*, not the original
  *working tree*: uncommitted edits the human had in flight are absent.
- **Turn-stop barrier.** The `turn_stop` row `replay.py` waits for between turns
  lives in `runtime.sqlite`'s `hook_event` table, which on current installations
  no hook writes any more (hook decisions go to the log service's
  `hook-diagnostics.json` aggregate instead — see the `.shared-context-logs` note
  above). Both drivers observe the turn's end directly, so the barrier is
  attempted once, recorded as `waited_for_turn_stop: false`, and skipped
  thereafter with a warning.
- **Depth of work.** Even with everything above mirrored, the replayed agent does
  not necessarily do the *same amount* of work: on the validated Android replay
  the original made 105 tool calls and the replay 31 for the same single prompt,
  reaching a comparable end state by a shorter route. Compare kinds and outcomes,
  not counts.
- **Host stderr.** Whatever Codex wrote to stderr — unloadable skills, MCP servers
  that refused to start, config keys this build does not know — is folded into
  `manifest.warnings`, de-duplicated with an `(xN)` count, because those are
  exactly the differences that quietly make a replay incomparable.
- **Plugin hooks.** The copied `config.toml` keeps the operator's other trusted
  hooks — including any corporate telemetry plugin — so a replay reports itself
  the same way the original did. Drop those `[hooks.state]` entries from the
  copied `codex-home/config.toml` before running if that is not wanted.
- **Prompt selection.** A user message that starts with `# AGENTS.md instructions`
  or with `<` is the host talking (AGENTS.md injection, `<recommended_plugins>`,
  environment blocks) and is skipped. Everything else is replayed byte for byte,
  URLs, pasted JSON and all.

### cursor

```sh
python3 tests/scripts/session_replay/replay.py --agent cursor \
  --session <conversation_id> [--turns N|all] \
  [--checkout worktree|inplace] [--cursor-home isolated|real] [--sctx-bin PATH] \
  [--cwd PATH] [--commit SHA] [--model MODEL] [--dry-run]
```

`--agent cursor` forwards every remaining argument to `replay_cursor.py`,
which is also runnable directly. It reuses `replay.py`'s snapshot machinery
verbatim — the same 0700 `HOME` snapshot (`state/` and `repository/` through
SQLite's backup API, `config.toml` path-rewritten, `embedding/` and `bin/`
symlinked), the same worktree, the same Repository re-registration inside the
snapshot, the same before/after fingerprints — and writes the same
`manifest.json` schema with `agent: "cursor"`, a `cursor_home` block where the
Codex path writes `codex_home`, and `replayed_transcript_path` alongside
`replayed_rollout_path` (same value) so `bundle.py --replay-id <id>` reads it
unchanged.

Output adds `host/hook-input.jsonl` and `host/hook-output.jsonl` to the Codex
layout's `host/turn-<n>.jsonl` / `.stderr`.

**Hook trust: there is none to repair.** Codex refuses to run a hook unless
`config.toml` carries a path-keyed `trusted_hash`, which is why `replay.py`
rewrites those keys (and, under `--sctx-bin`, recomputes them). Cursor has no equivalent: `~/.cursor/hooks.json` is
`{"hooks": {<event>: [{"command": ...}]}, "version": 1}` with no hashes, and
nothing else under `~/.cursor` gates hook execution. A freshly written
`hooks.json` in a fresh `CURSOR_CONFIG_DIR` is simply run. `build_cursor_home`
therefore *generates* the file rather than copying it, taking only the event
list from the operator's own (so a future Cursor that adds an event is picked
up), and warns if `hooks.json` ever grows a key beyond `hooks`/`version` —
which would be the first sign a trust mechanism had appeared.

**How the isolation is arranged.**

- `HOME` is the snapshot, exactly as on the Codex path. `CURSOR_CONFIG_DIR`
  and `CURSOR_DATA_DIR` both point at `<home>/.cursor` inside it, so chats,
  transcripts and per-project state land in the audit directory.
- `cli-config.json` is written fresh with only `version`, `authInfo`, `model`
  and `permissions` copied across; `$HOME/Library/Keychains` is **symlinked**,
  never copied, because Cursor resolves its stored credential through the macOS
  keychain. No token is read, printed or passed as an argument.
- `mcp.json` registers `shared-context` against the snapshot's
  `bin/current/sctx` with `HOME` pinned to the snapshot, so the MCP side and
  the hook side agree on which installation they are in. That indirection is
  also why `--sctx-bin` needs no Cursor-specific work beyond placing the binary:
  both the generated `hooks.json` wrapper and `mcp.json` already name that path.
  See "Replaying a dev build" above for the flag; on the Cursor side it also
  rebuilds `~/.agents/skills/` from the binary's source tree and records
  `sctx_bin` / `skill_bundle` in the manifest.
- Each hook is registered as a two-line `sh` recorder that appends the payload
  to `host/hook-input.jsonl`, runs the real snapshot binary with the same
  arguments and the same stdin, appends the reply to `host/hook-output.jsonl`,
  and prints that reply unchanged. This is the only way to see hook traffic:
  Cursor writes it into neither the transcript nor its own logs. The two files
  pair by line index; concurrent `postToolUse` hooks append small single
  writes, which is atomic enough in practice but is not a guarantee.

`manifest.json.isolation.hooks` is the proof the hooks fired *in the isolated
HOME*: the per-event payload counts, the conversation ids seen in those
payloads, how many replies carried a `<shared-context-active>` marker, and
`marker_names_replayed_conversation` — whether such a marker named the new
conversation id. Alongside it, `real_cursor_unchanged` /
`real_cursor_new_transcripts` compare the real `~/.cursor` transcript
inventory before and after the run, and
`replayed_thread_in_replay_state` / `replayed_thread_in_real_state` show the
new conversation's `external_session` / `task_injection` / `context_usage` rows
existing in the snapshot and absent from the operator's real installation.

**Known deviations beyond the Codex list.** All of these are written into
`manifest.deviations` at run time, with the specific reason string used.

- **The commit is a guess.** A Cursor transcript records no git metadata at
  all. The commit is `git -C <cwd> log -1 --before=<turn 1's `<timestamp>`
  tag>` on whatever branch the checkout is on *now* (falling back to the
  transcript's file mtime when the transcript predates the timestamp tag), and
  `manifest.original.commit_source` says so in words, prefixed `GUESS:`. Use
  `--commit` when you know better.
- **The cwd is recovered, not read** — from sctx's activation lease
  `startup_cwd`, else by slug-matching the registered checkouts;
  `manifest.original.cwd_source` records which. `--cwd` overrides. If neither
  works the run refuses rather than guessing.
- **The model is not recorded either.** The replay uses the operator's current
  `cli-config.json` default and marks `model_source` as a `GUESS:`. `--model`
  overrides.
- **Approvals.** `--auto-review --approve-mcps --trust`. `--force`/`--yolo` is
  deliberately *not* used: it would let the replay run commands the original's
  reviewer would have stopped.
- **Only human prompts are replayed.** Cursor's own follow-up prompts (see
  `hosts/cursor.py` `HOST_QUERY_PREFIXES`) are skipped, and
  `manifest.original.host_prompts_skipped` counts them.
- **Turn 2 onwards uses `--resume <conversation id>`**, the id taken from turn
  1's `{"type":"system","subtype":"init"}` stream event. If a later turn reports
  a *different* id the manifest warns that `--resume` did not continue the same
  session.
- **`--model` is deliberately not passed** unless the operator supplies one.
  Cursor records no model in the transcript, so there is nothing faithful to
  pass, and the account default (which the copied `cli-config.json` carries) is
  what an interactive session would have used anyway. Passing the config's own
  `model.modelId` is actively worse: it is a display alias like `grok-4.6`,
  which a `--resume` turn rejects with `Cannot use this model` — a first attempt
  at this replay died exactly that way on turn 2 after turn 1 had accepted it.
- **`hook-diagnostics.json` is absent on the replay side.** That file is written
  by the sctx log *collector* service, and a replay HOME has an empty
  `.shared-context-logs/` with no collector in it. The reconstruction block says
  `hook-diagnostics: unavailable: ... — the hook-event counts below are ABSENT,
  not zero`, and the replay's real hook traffic lives in
  `host/hook-input.jsonl` / `host/hook-output.jsonl` and
  `manifest.isolation.hooks` instead.

**Validated end to end** on 2026-09-10, replaying the first 2 human prompts of
`7d8cbab1-…` into the FE monorepo at guessed commit `a3351d8d`:

- `manifest.isolation.hooks` — 69 hook payloads captured in the isolated HOME:
  `sessionStart` 1, `postToolUse` 66, `sessionEnd` 2; one conversation id
  observed (`230023e6-…`, the replay's own); 2 replies carried a
  `<shared-context-active>` marker and `marker_names_replayed_conversation` is
  `true`. So the hooks ran, sctx activated, and it activated *for this
  conversation* — with no trust handshake of any kind.
- `real_shared_context_unchanged: true` (identical row-count fingerprint before
  and after across all 9 tracked tables), `real_cursor_unchanged: true`
  (627 → 627 transcripts, `real_cursor_new_transcripts: []`).
- `replayed_thread_in_replay_state` shows the new conversation with
  `task_injection_rows: 17`, `context_usage_rows: 17`,
  `agent_checkpoint_rows: 2`; `replayed_thread_in_real_state` shows
  `external_session_id: null` — the rows exist in the snapshot and do not exist
  in the operator's installation.
- Both turns exited 0 with `turn_ended` `success`, and turn 2 reported the same
  conversation id, so `--resume` did continue the session.
- `bundle.py --replay-id <id>` then produced `original.md` (65 KB),
  `replay.md` (26 KB) and a two-sided `facts.json`: original 4 turns / 12 sctx
  calls / 7 claims / 3 injection events over 20 context ids, replay 2 turns /
  18 sctx calls / 5 claims / 2 injection events over 17 context ids.

One finding fell out of that run and is now handled in the parser: the sctx MCP
`namespace` on a dynamic-tool call is the server name **with whatever scope
prefix Cursor gives it**. The real session shows `user-shared-context`; the
replay's isolated `CURSOR_CONFIG_DIR` produced bare `shared-context`. Matching
the literal string would have reported zero sctx calls for a replay that made
eighteen.

### Cleaning up

Worktrees hold a real checkout and a `.git/worktrees` entry in the source
repository, so remove them through git rather than with `rm -rf`:

```sh
git -C <original cwd> worktree remove --force <audit-root>/worktrees/<replay-id>
git -C <original cwd> branch -D replay/<original branch>   # the branch it was on
git -C <original cwd> worktree prune          # if a directory was already deleted
rm -rf <audit-root>/<replay-id>               # home, host home, host streams, manifest
```

A Cursor replay directory additionally holds `home/.cursor/` — the isolated
`CURSOR_CONFIG_DIR`, including the replayed transcript — so deleting it deletes
that transcript too. Bundle first if it matters.

A replay directory is a few tens of megabytes (`home/.shared-context/state` is
the bulk of it; `embedding/` and `bin/` are symlinks, not copies). Deleting it
deletes the replayed rollout with it, so bundle first if the transcript matters.

## candidates

Replays are headless: the agent's questions get no answer. `candidates.py`
scans local real sessions (Codex `~/.codex/state_5.sqlite` `threads`,
`thread_source='user'`, via `hosts/codex.py`; Cursor
`~/.cursor/projects/*/agent-transcripts/*/*.jsonl`, via `hosts/cursor.py`)
and ranks them as replay candidates, so a session gets picked for `replay.py`
*before* it turns out to depend on an external doc or user corrections. (Note:
named `candidates.py`, not `select.py` — that name shadows the stdlib
`select` module that `subprocess`/`selectors` import, which breaks any script
run from this directory.)

Every metric is computed from the parsed `session_model.Session` alone: how
long and specific the first human prompt is (a path, a `.kt`/`.swift`/`.ts`/`.rs`
name, or a backticked identifier counts as specific; a bare URL or Lark link
does not), how much of the tool-call volume happened in turn 1 versus later,
how many short "continue"/"确认" follow-ups and agent-asked questions there
were, and how many tool calls touched something needing credentials or the
network (`lark`, `curl`, `gh `, `adb`, `xcodebuild`, a bare host). A session's
repo must also be registered in `~/.shared-context/config.toml`'s
`[[repositories]]` table (parsed directly with `tomllib`, matched by path
prefix) — an unregistered repo never receives a marker injection and is
useless as a replay candidate, so this is a **hard filter by default**
(`--include-unregistered` opts back in). The score formula (long specific
first prompt + high turn-1 tool share + few prompts + an active sctx marker,
minus questions/short-followups/external-deps/URL-first-prompt) is documented
in the file's module docstring alongside every constant.

```sh
python3 tests/scripts/session_replay/candidates.py \
  [--agent codex|cursor|all] [--since YYYY-MM-DD] [--min-t1-tools N] \
  [--top N] [--json] [--include-unregistered] [--explain THREAD_ID]
```

`--json` prints the same records the table shows, for a replay driver to
consume machine-readably; `--explain THREAD_ID` resolves and parses one
session (ignoring every other filter/the hard registration filter) and
prints its full raw metrics, including which `external_dep_keywords`
matched. `tests/test_candidates.py` covers the detection helpers, the score
formula, `config.toml` parsing/prefix matching, and the `--since` /
`thread_source` filtering, all against synthetic data built inline — no real
transcript text or thread ids.
