#![allow(dead_code)]
//! Shared harness for the association probe acceptance tests (WP-T, WP-L7).
//!
//! Stores a fixed synthetic knowledge set through the public chain
//! (`task_intent_update` -> `task_checkpoint` -> `candidate_list`/`candidate_get`
//! -> `candidate_confirm`) and then asks the same knowledge back with probes
//! phrased differently from the stored text, through both retrieval entry
//! points: explicit `sctx search` and automatic `task_intent_update`.
//!
//! The corpus, the probes and the expected top-1 Context are all data, so a
//! second probe set (for example the Chinese-first `probe-zh-v1`) only needs a
//! fixture file and its own thresholds.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub mod semantic;

use sctx_agent_adapter::{AgentKind, shared_context_activation_marker_with_policy};
use sctx_git_store::GitStore;
use sctx_local_state::Policy;
use serde_json::{Value, json};

const CODEX_FIXTURE: &str = include_str!("../../../../fixtures/agents/codex-0.147.json");

pub fn run_json_cli(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_hook(home: &Path, payload: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["hook", "--agent", "codex", "--agent-version", "0.147.0"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&serde_json::to_vec(payload).unwrap())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

pub fn mcp_tool(home: &Path, session: &str, name: &str, arguments: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["mcp", "serve", "--client", "codex"])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05"}
    });
    let mut arguments = arguments;
    let object = arguments.as_object_mut().unwrap();
    object
        .entry("agent_kind".to_owned())
        .or_insert_with(|| json!("codex"));
    object
        .entry("external_session_id".to_owned())
        .or_insert_with(|| json!(session));
    let call = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    });
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{initialize}").unwrap();
    writeln!(stdin, "{call}").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "MCP {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2, "{responses:#?}");
    assert_eq!(
        responses[1]["result"]["isError"], false,
        "session={session} tool={name} responses={responses:#?}"
    );
    responses[1]["result"]["structuredContent"].clone()
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_repository(path: &Path, files: &Value) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    for (relative, contents) in files.as_object().unwrap() {
        let target = path.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents.as_str().unwrap()).unwrap();
    }
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Association Probe Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "Add association probe fixture",
        ],
    );
    fs::canonicalize(path).unwrap()
}

/// Discards every recorded injection outcome in this installation.
///
/// Storing the corpus injects each Context into the Tasks that store the ones after it, and none
/// of those Tasks builds on what it was handed, so a finished corpus already carries enough
/// `ignored` rows to move the usage prior. The probes then measure ranking under a history that
/// only the fixture's own storage order produced. Every probe therefore starts from no history at
/// all: the usage prior is a real product signal, but it is not what these probes measure, and at
/// basis-point margins it is loud enough to flip a top-1.
pub fn reset_context_usage(home: &Path) {
    let database = home
        .join(".shared-context")
        .join("state")
        .join("runtime.sqlite");
    if !database.is_file() {
        return;
    }
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute("DELETE FROM context_usage", [])
        .unwrap();
}

fn session_start(session: &str, checkout: &Path) -> Value {
    let mut payload = serde_json::from_str::<Vec<Value>>(CODEX_FIXTURE)
        .unwrap()
        .remove(0);
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(checkout);
    payload["transcript_path"] = Value::Null;
    payload
}

/// The stored Context corpus plus the temporary installation it lives in.
pub struct Harness {
    _temporary: tempfile::TempDir,
    pub home: PathBuf,
    pub session: String,
    /// Fixture Context index (1-based) keyed by the accepted `ContextId`.
    index_by_context: BTreeMap<String, u64>,
    /// Latest Working Intent revision, threaded through every probe call.
    intent_revision_id: String,
}

fn claim_payload(context: &Value) -> Value {
    json!({
        "context_kind": context["context_kind"],
        "statement": context["statement"],
        "rationale": context["rationale"],
        "conditions": context["conditions"],
        "evidence": context["evidence"]
    })
}

fn primary_selection(review: &Value) -> Value {
    let recommendations = review["space_recommendations"].as_array().unwrap();
    if let Some(proposed) = recommendations
        .iter()
        .find(|recommendation| recommendation["kind"] == "proposed_new_space_intent")
    {
        return json!({"new_space_recommendation_id": proposed["recommendation_id"]});
    }
    let existing = recommendations
        .iter()
        .find(|recommendation| recommendation["kind"] == "existing")
        .unwrap_or_else(|| panic!("no usable Space recommendation: {review:#}"));
    json!({"existing_space_id": existing["space_id"]})
}

fn relation_edits(fixture: &Value, index: u64, accepted: &BTreeMap<u64, String>) -> Value {
    let relations = fixture["relations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relation| relation["from"].as_u64() == Some(index))
        .filter_map(|relation| {
            let target = accepted.get(&relation["to"].as_u64().unwrap())?;
            Some(json!({
                "target_context_id": target,
                "kind": relation["kind"],
                "rationale": relation["rationale"],
                "supports": relation["supports"]
            }))
        })
        .collect::<Vec<_>>();
    if relations.is_empty() {
        json!({})
    } else {
        json!({"relations": relations})
    }
}

#[allow(clippy::too_many_lines)]
pub fn build_harness(fixture: &Value) -> Harness {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("association probe home");
    fs::create_dir_all(&home).unwrap();
    GitStore::bootstrap_local(home.join(".shared-context")).unwrap();
    let checkout = initialize_repository(&home.join("checkout"), &fixture["repository_files"]);
    run_json_cli(
        &home,
        &[
            "repository",
            "add",
            "--repository-id",
            fixture["repository_id"].as_str().unwrap(),
            "--path",
            checkout.to_str().unwrap(),
        ],
    );
    let session = "association-probe-session".to_owned();
    assert_eq!(
        run_hook(&home, &session_start(&session, &checkout)),
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": shared_context_activation_marker_with_policy(
                AgentKind::Codex,
                &session,
                Policy::compiled_default().session(),
            )
        }})
    );

    let mut accepted: BTreeMap<u64, String> = BTreeMap::new();
    let mut intent_revision_id: Option<String> = None;
    for context in fixture["contexts"].as_array().unwrap() {
        let index = context["index"].as_u64().unwrap();
        let task = mcp_tool(
            &home,
            &session,
            "task_intent_update",
            json!({
                "task_boundary": "new",
                "expected_revision_id": intent_revision_id,
                "intent": {
                    "goal": context["goal"],
                    "acceptance_conditions": context["acceptance_conditions"]
                }
            }),
        );
        intent_revision_id = Some(task["intent_revision_id"].as_str().unwrap().to_owned());
        let checkpoint = mcp_tool(
            &home,
            &session,
            "task_checkpoint",
            json!({"claims": [claim_payload(context)], "unknowns": []}),
        );
        assert_eq!(
            checkpoint["candidate_build"]["status"], "pending",
            "Context {index} did not produce a pending Candidate: {checkpoint:#}"
        );
        let pending = mcp_tool(
            &home,
            &session,
            "candidate_list",
            json!({"status": "pending", "limit": 10, "token_budget": 32768}),
        );
        let candidate_ids = pending["reviews"]
            .as_array()
            .unwrap()
            .iter()
            .map(|review| review["candidate_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            candidate_ids.len(),
            1,
            "Context {index} expected exactly one pending Candidate: {pending:#}"
        );
        let review = mcp_tool(
            &home,
            &session,
            "candidate_get",
            json!({"candidate_id": candidate_ids[0]}),
        );
        let confirmed = mcp_tool(
            &home,
            &session,
            "candidate_confirm",
            json!({
                "expected_task_id": task["task_id"],
                "expected_intent_revision_id": task["intent_revision_id"],
                "candidate_id": candidate_ids[0],
                "expected_review_version": review["review_version"],
                "primary": primary_selection(&review),
                "related_space_ids": [],
                "edits": relation_edits(fixture, index, &accepted)
            }),
        );
        assert_eq!(
            confirmed["status"], "confirmed",
            "Context {index} was not confirmed: {confirmed:#}"
        );
        accepted.insert(index, confirmed["context_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        accepted.len(),
        fixture["contexts"].as_array().unwrap().len()
    );

    reset_context_usage(&home);
    Harness {
        _temporary: temporary,
        home,
        session,
        index_by_context: accepted
            .iter()
            .map(|(index, context_id)| (context_id.clone(), *index))
            .collect(),
        intent_revision_id: intent_revision_id.unwrap(),
    }
}

/// One probe outcome on both retrieval entry points.
pub struct ProbeOutcome {
    pub id: String,
    pub category: String,
    pub query: String,
    pub expected: Vec<u64>,
    pub search_top1: Option<u64>,
    pub search_results: usize,
    pub search_hit: bool,
    pub intent_top1: Option<u64>,
    pub intent_items: usize,
    pub intent_hit: bool,
    /// Automatic coverage inputs, read back from the explainable `task_context` payload of the
    /// same Working Intent. They are diagnostics only; no assertion depends on them.
    pub automatic: AutomaticProbeDiagnostics,
}

/// What the automatic channel measured for one probe: how many query tokens it selected, how many
/// of those this corpus can answer at all, and how many the leading item actually matched.
#[derive(Default)]
pub struct AutomaticProbeDiagnostics {
    pub selected_tokens: usize,
    pub answerable_tokens: usize,
    pub matched_tokens: usize,
    pub coverage_basis_points: u64,
    /// Fused association score, in basis points, of the Space that produced the leading item.
    ///
    /// This is what [`AUTOMATIC_RELEVANCE_FLOOR_BASIS_POINTS`] is compared against, so it is the
    /// only number the floor sweep needs: a floor at or below the lowest score behind a hit
    /// cannot take that hit away.
    pub leading_fused_score_basis_points: Option<u64>,
    /// Omission reasons the Pack reported, deduplicated and counted.
    pub omitted: BTreeMap<String, usize>,
}

/// Reads `fused_score_basis_points` back out of an Association's serialized fusion explanation.
fn fused_score_basis_points(association: &Value) -> Option<u64> {
    association["reasons"]
        .as_array()?
        .iter()
        .filter_map(|reason| serde_json::from_str::<Value>(reason.as_str()?).ok())
        .find_map(|reason| reason["fused_score_basis_points"].as_u64())
}

fn count(value: &Value) -> usize {
    usize::try_from(value.as_u64().unwrap_or(0)).unwrap_or(usize::MAX)
}

fn automatic_diagnostics(harness: &Harness) -> AutomaticProbeDiagnostics {
    let pack = mcp_tool(
        &harness.home,
        &harness.session,
        "task_context",
        json!({"detail_level": "full", "token_budget": 32768}),
    );
    let explanation = &pack["query_token_explanation"];
    let leading = pack["items"].as_array().and_then(|items| items.first());
    let mut omitted = BTreeMap::new();
    for entry in pack["omitted"].as_array().into_iter().flatten() {
        let reason = entry["reason"].as_str().unwrap_or("unknown").to_owned();
        *omitted.entry(reason).or_insert(0) +=
            usize::try_from(entry["count"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX);
    }
    let leading_space = leading.and_then(|item| item["association_space_id"].as_str());
    AutomaticProbeDiagnostics {
        leading_fused_score_basis_points: leading_space.and_then(|space_id| {
            pack["candidate_spaces"]
                .as_array()?
                .iter()
                .find(|association| association["space_id"].as_str() == Some(space_id))
                .and_then(fused_score_basis_points)
        }),
        selected_tokens: count(&explanation["selected_token_count"]),
        answerable_tokens: count(&explanation["answerable_token_count"]),
        matched_tokens: leading
            .and_then(|item| item["context"]["match_reason"]["matched_tokens"].as_array())
            .map_or(0, Vec::len),
        coverage_basis_points: leading
            .and_then(|item| item["context"]["match_reason"]["coverage_basis_points"].as_u64())
            .unwrap_or(0),
        omitted,
    }
}

pub fn top_index(harness: &Harness, context_id: Option<&str>) -> Option<u64> {
    context_id.and_then(|id| harness.index_by_context.get(id).copied())
}

pub fn run_probes(harness: &mut Harness, fixture: &Value) -> Vec<ProbeOutcome> {
    let mut outcomes = Vec::new();
    for probe in fixture["probes"].as_array().unwrap() {
        reset_context_usage(&harness.home);
        let query = probe["query"].as_str().unwrap().to_owned();
        let expected = probe["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap())
            .collect::<Vec<_>>();

        let search = run_json_cli(
            &harness.home,
            &[
                "search",
                "--query",
                &query,
                "--status",
                "accepted",
                "--page-size",
                "5",
            ],
        );
        let search_results = search["data"]["results"].as_array().unwrap();
        let search_top1 = top_index(
            harness,
            search_results
                .first()
                .and_then(|result| result["context_id"].as_str()),
        );

        let intent = mcp_tool(
            &harness.home,
            &harness.session,
            "task_intent_update",
            json!({
                "task_boundary": "new",
                "expected_revision_id": harness.intent_revision_id,
                "intent": {"goal": query}
            }),
        );
        intent["intent_revision_id"]
            .as_str()
            .unwrap()
            .clone_into(&mut harness.intent_revision_id);
        let intent_items = intent["items"].as_array().unwrap();
        let intent_top1 = top_index(
            harness,
            intent_items
                .first()
                // `task_intent_update` returns the compact injection payload by default, which
                // carries the Context identity at the top level of each item.
                .and_then(|item| item["context_id"].as_str()),
        );

        let hit = |top: Option<u64>, count: usize| {
            if expected.is_empty() {
                count == 0
            } else {
                top.is_some_and(|index| expected.contains(&index))
            }
        };
        let automatic = automatic_diagnostics(harness);
        outcomes.push(ProbeOutcome {
            automatic,
            id: probe["id"].as_str().unwrap().to_owned(),
            category: probe["category"].as_str().unwrap().to_owned(),
            query,
            search_hit: hit(search_top1, search_results.len()),
            intent_hit: hit(intent_top1, intent_items.len()),
            expected,
            search_top1,
            search_results: search_results.len(),
            intent_top1,
            intent_items: intent_items.len(),
        });
    }
    outcomes
}

fn cell(index: Option<u64>) -> String {
    index.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn mark(hit: bool) -> &'static str {
    if hit { "yes" } else { "no" }
}

fn report_path(file_name: &str) -> PathBuf {
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target");
    fs::create_dir_all(&target).unwrap();
    fs::canonicalize(&target).unwrap().join(file_name)
}

/// Prints the per-probe table, writes the machine-readable report and returns
/// the two hit counts (explicit search, automatic `task_intent_update`).
#[allow(clippy::too_many_lines)]
pub fn emit(
    report_file_name: &str,
    mode: &str,
    fixture: &Value,
    outcomes: &[ProbeOutcome],
) -> (usize, usize) {
    let search_hits = outcomes.iter().filter(|outcome| outcome.search_hit).count();
    let intent_hits = outcomes.iter().filter(|outcome| outcome.intent_hit).count();
    println!(
        "\n=== association probe report ({mode}, fixture {}) ===",
        fixture["fixture_version"].as_str().unwrap()
    );
    println!(
        "{:<10} {:<26} {:<10} {:>7} {:>6} {:>7} {:>6}  query",
        "probe", "category", "expected", "search", "ok", "intent", "ok"
    );
    for outcome in outcomes {
        let expected = if outcome.expected.is_empty() {
            "none".to_owned()
        } else {
            outcome
                .expected
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("|")
        };
        println!(
            "{:<10} {:<26} {:<10} {:>7} {:>6} {:>7} {:>6}  {}",
            outcome.id,
            outcome.category,
            expected,
            cell(outcome.search_top1),
            mark(outcome.search_hit),
            cell(outcome.intent_top1),
            mark(outcome.intent_hit),
            outcome.query
        );
    }
    println!(
        "search {search_hits}/{total}, task_intent_update {intent_hits}/{total}",
        total = outcomes.len()
    );

    println!("\n--- automatic coverage inputs ({mode}) ---");
    println!(
        "{:<10} {:>9} {:>11} {:>8} {:>9}  omitted",
        "probe", "selected", "answerable", "matched", "coverage"
    );
    for outcome in outcomes {
        let omitted = outcome
            .automatic
            .omitted
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{:<10} {:>9} {:>11} {:>8} {:>9}  {}",
            outcome.id,
            outcome.automatic.selected_tokens,
            outcome.automatic.answerable_tokens,
            outcome.automatic.matched_tokens,
            outcome.automatic.coverage_basis_points,
            omitted
        );
    }

    emit_relevance_floor_sweep(mode, outcomes);

    let rows = outcomes
        .iter()
        .map(|outcome| {
            json!({
                "id": outcome.id,
                "category": outcome.category,
                "query": outcome.query,
                "expected_context_indexes": outcome.expected,
                "search": {
                    "top1_context_index": outcome.search_top1,
                    "result_count": outcome.search_results,
                    "hit": outcome.search_hit
                },
                "task_intent_update": {
                    "top1_context_index": outcome.intent_top1,
                    "item_count": outcome.intent_items,
                    "hit": outcome.intent_hit
                },
                "automatic_coverage": {
                    "selected_tokens": outcome.automatic.selected_tokens,
                    "answerable_tokens": outcome.automatic.answerable_tokens,
                    "matched_tokens": outcome.automatic.matched_tokens,
                    "coverage_basis_points": outcome.automatic.coverage_basis_points,
                    "leading_fused_score_basis_points":
                        outcome.automatic.leading_fused_score_basis_points,
                    "omitted": outcome.automatic.omitted
                }
            })
        })
        .collect::<Vec<_>>();
    let noise = |select: fn(&ProbeOutcome) -> Option<u64>| {
        outcomes
            .iter()
            .filter(|outcome| outcome.expected.is_empty() && select(outcome).is_some())
            .count()
    };
    let report = json!({
        "fixture_version": fixture["fixture_version"],
        "mode": mode,
        "probe_count": outcomes.len(),
        "summary": {
            "search": {"hits": search_hits, "noise_hits": noise(|outcome| outcome.search_top1)},
            "task_intent_update": {
                "hits": intent_hits,
                "noise_hits": noise(|outcome| outcome.intent_top1)
            }
        },
        "probes": rows
    });
    let path = report_path(report_file_name);
    // Both probe tests may run concurrently; stage per mode so their renames never race.
    let staging = path.with_extension(format!("{mode}.json.partial"));
    fs::write(&staging, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    fs::rename(&staging, &path).unwrap();
    println!("report written to {}", path.display());
    (search_hits, intent_hits)
}

/// Prints where an automatic relevance floor would start costing this probe set a hit.
///
/// The floor drops a whole Space before its items are packed, so a floor at or below the lowest
/// fused score behind a currently-hit top-1 cannot take that hit away, and the first value above
/// it is the first one that can. The table walks exactly those boundaries -- one row per distinct
/// score behind a hit -- so the separation surface is read straight off one unfloored run and no
/// candidate threshold needs a build of its own.
fn emit_relevance_floor_sweep(mode: &str, outcomes: &[ProbeOutcome]) {
    println!("\n--- automatic relevance floor sweep ({mode}) ---");
    let mut scored = outcomes
        .iter()
        .filter(|outcome| outcome.intent_hit && !outcome.expected.is_empty())
        .filter_map(|outcome| {
            outcome
                .automatic
                .leading_fused_score_basis_points
                .map(|score| (score, outcome.id.clone()))
        })
        .collect::<Vec<_>>();
    scored.sort_unstable();
    let hits = outcomes.iter().filter(|outcome| outcome.intent_hit).count();
    let total = outcomes.len();
    println!("{:<12} {:>8}  first probes lost", "floor (bp)", "intent");
    println!("{:<12} {:>8}  -", 0, format!("{hits}/{total}"));
    let mut boundaries = scored.iter().map(|(score, _)| *score).collect::<Vec<_>>();
    boundaries.dedup();
    for boundary in boundaries {
        let floor = boundary + 1;
        let lost = scored
            .iter()
            .filter(|(score, _)| *score < floor)
            .map(|(score, id)| format!("{id}({score})"))
            .collect::<Vec<_>>();
        println!(
            "{:<12} {:>8}  {}",
            floor,
            format!("{}/{}", hits - lost.len(), total),
            lost.join(" ")
        );
    }
    println!("\n--- leading fused score per probe ({mode}) ---");
    println!("{:<10} {:>8} {:>18}", "probe", "hit", "fused score (bp)");
    for outcome in outcomes {
        println!(
            "{:<10} {:>8} {:>18}",
            outcome.id,
            mark(outcome.intent_hit),
            outcome
                .automatic
                .leading_fused_score_basis_points
                .map_or_else(|| "-".to_owned(), |score| score.to_string())
        );
    }
}

/// Every noise probe must return nothing on both entry points.
pub fn assert_no_noise(outcomes: &[ProbeOutcome]) {
    for outcome in outcomes {
        if outcome.category == "noise" {
            assert_eq!(
                outcome.search_results, 0,
                "noise probe {} matched {} explicit-search results",
                outcome.id, outcome.search_results
            );
            assert_eq!(
                outcome.intent_items, 0,
                "noise probe {} matched {} automatic items",
                outcome.id, outcome.intent_items
            );
        }
    }
}

/// Not one automatic item across the whole fixture, and the reason stated rather than assumed.
///
/// ADR-0007 retired intent-text matching from automatic injection, and these three fixtures drive
/// `task_intent_update` with a Working Intent goal and nothing else: no Workspace or Diff Signal,
/// no `artifact_hints` that read as a path, no `task_artifact_focus`. Lane A therefore has no
/// anchor to start from, and with no seed there is no second hop either, so every automatic Pack in
/// these suites is empty *by construction*.
///
/// That makes the old `intent_hits >= N` floor unmeasurable rather than merely lower: it counted
/// hits on a channel that no longer exists. What is still worth asserting is the half these
/// fixtures can still speak to -- that an empty Pack is an empty Pack and not a Pack of noise, and
/// that it says so in one line instead of falling silent. Whoever extends a fixture with the file
/// footprint of a real Session should replace this with a lane hit-rate floor, and until then the
/// honest reading is zero of both lanes with zero noise.
pub fn assert_every_automatic_pack_is_empty(outcomes: &[ProbeOutcome]) {
    for outcome in outcomes {
        assert_eq!(
            outcome.intent_items, 0,
            "probe {} ({}) returned {} automatic item(s) from a fixture that supplies no lane \
             input at all -- no touched file, no resolved Focus, no path-shaped hint. Either the \
             fixture grew a footprint, in which case this assertion should become a lane hit-rate \
             floor, or a route into automatic injection exists that ADR-0007 does not describe.",
            outcome.id, outcome.query, outcome.intent_items
        );
    }
}
