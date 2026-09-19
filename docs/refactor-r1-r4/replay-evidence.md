# Immutable R1 replay evidence

Replay workspace and reproducible commands: `/private/tmp/sctx-refactor-replay/README.md`. Input snapshots came from the user-supplied plan's original scratchpad, opened read-only and copied with SQLite backup. Each run uses a fresh independent root and an independent Knowledge Store clone at commit `f0684978e09ae4698f5b897495d793a11e16f266`, tree `810c67b947a2eff829576e15581ac5dfe2683c78` (matching the index snapshot). Minimal private config and empty config lock are test-environment adaptations. Installed database/configuration is unchanged. Raw private claim text is deliberately retained only in local temporary evidence.

The frozen binary's relevant source matches `f539f9c` (Search/MCP unchanged during the concurrent R1-1 commit). Its SHA256 is `84f39e632e6cdf7569c3f23e2e64748b969367543303009a02a0b93fc1960269`.

| Input | SHA256 |
|---|---|
| pristine/state/runtime.sqlite | `68ab4b7c9cc84805a3f12d025e6850dbffe4e7e6226e6b2431b45e577d58fe2e` |
| pristine/state/index.sqlite | `d586f9067477971d6757fb6a8c84c8b79508eec57488caa695510057d7c9bd26` |
| pristine/state/engineering.sqlite | `eeb13eaa49d6d213ae94b06fb5fb043d5ce26ade3fc3173f5e4b9c16cd40a321` |
| candidate-ids.json | `85dbfe5b4f7d931cf0d0b339065a2cddcda2089610a7c23882c02edc8bdcccd1` |
| intent-corpus.json | `040c624533395c34a8a93eba8208c45bb7bf059a0656eb52201359152a8c15ca` |

## Candidate baseline

Production `candidate_analyze_at_root` runs on all 34 recorded candidate IDs in sorted order, with current defaults: token budget 4096, top_k 16. Historical persisted analyses contained 207 assessments; that is not the comparison baseline because those were computed at different times. Fresh replay produces 328 assessments: exact_duplicate 17; unresolved_related 291; supports 11; revises 3; potential_contradiction 5; novel 1. All 34 API calls succeed; 33 analyses Complete, one remains Failed with pre-existing `analysis_invalid_input`, exactly as in the immutable source. Do not claim 34 complete analyses.

After R1-2 (`7f26461`): 346 assessments: exact_duplicate 17; unresolved_related 309; supports 11; revises 3; potential_contradiction 5; novel 1. All 34 calls and analyses complete. The previously Failed candidate now has 16 unresolved assessments and estimated_tokens 3975 within budget 4096. This is an observation, not evidence of stronger semantic relevance. Verified outputs: `/private/tmp/sctx-refactor-replay/after-r1-2-verified-candidates.jsonl` and `after-r1-2-verified-candidates-summary.json`. The executor discarded an initial run launched before copy preparation completed; the authoritative run waited for preparation and the main agent independently verified all three pristine DB hashes unchanged.

## Intent baseline

All 36 recorded Intent revisions are replayed via production `task_space_associations` and `task_context_pack`, AutomaticInjection with pack budget 6000. Eight requests fail ratio sufficiency (including the minimum-answerable condition), while actual `low_answerable_ratio` omission count is one. These are distinct metrics; the second measures the gate after production bypasses.

Limit: immutable as-of signal histories are unavailable, so signals are held empty and resolved_focus absent. Intent artifact/interface hints remain present. This is a fixed intent-only lexical/graph replay; semantic and usage-prior providers are absent. Candidate analyzer likewise does not use these providers; writes to copied usage rows do not feed the comparison. Separate semantic probe suite covers embedding fusion.

After R1-3 (70a816b): ratio-insufficient requests8→1, actual low_answerable_ratio omissions1→0;23 of36 intent summaries changed. Same immutable snapshots/harness assumptions. Frozen after evidence: `/private/tmp/sctx-refactor-replay/after-r1-3-evidence.json`; after outputs `after-intents.jsonl` and `after-intents-summary.json`.

## Pre-R1-3 probe baseline

At `369772b`, debug-profile ordinary probe: 2 tests passed, 20/22 explicit search and 20/22 automatic intent, no noise hits (15.46 seconds). F2LLM extended probe explicitly used `--ignored --nocapture`: 1 test passed, lexical 27/39 and fused 32/39, no noise hits (49.30 seconds). Model snapshot `8786315a8711c242ee03ec67c74dd9ad0a61e2cf`, local ONNX runtime. Debug profile uses available cache and suffices for recall counts; it is not release performance evidence.

Logs: `/private/tmp/sctx-r1-3-baseline-workflow.log`, `/private/tmp/sctx-r1-3-baseline-semantic.log`; metadata `/private/tmp/sctx-r1-3-baseline-metadata.json`.

Three post-change release repetitions completed before H-001 stopped implementation: ordinary search20/22 and intent20/22 each time; F2LLM lexical29/39 and fused32/39 each time, zero noise. Lexical long_intent improves1→3/3; fused total is unchanged. Runner summary `/private/tmp/sctx-r1-3-probe-runs.json`; per-run logs `/private/tmp/sctx-r1-3-association_probe_{workflow,ext_semantic_f2llm}-{1,2,3}.log`. These are executor observations; R1-3 remains uncommitted and unaccepted pending replay, ratchet disposition, clippy and main review after H-001 is resolved.


R1-3 post-resumption completion: the measured lexical long_intent gain is pinned at3/3, with one further real F2LLM release run passing the new assertion. Existing fused32/39 bound and independent binary-reference27 remain unchanged. Main independently reran all three direct token-selection regressions (2 +1 passed), reviewed exact commit70a816b, source invariants and D-004.
