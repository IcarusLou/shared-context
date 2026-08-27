use std::{collections::BTreeSet, fs, path::Path};

use serde_json::Value;

const SCENARIOS: [&str; 6] = [
    "codex_normal_candidate_confirm",
    "cursor_normal_candidate_confirm",
    "precompact_resume_new_episode",
    "missing_hook_empty_close_fallback",
    "turnstop_repeat_and_mcp_restart_recovery",
    "same_workspace_dual_session_isolation",
];
const CLASSIFICATIONS: [&str; 6] = [
    "invalid_scenario",
    "unsupported_version",
    "corrupt_data",
    "expected_fail_open",
    "infrastructure_flake",
    "product_invariant_violation",
];
const DOMAIN_ID_PREFIXES: [&str; 29] = [
    "spc_", "rpo_", "rpg_", "ref_", "tsk_", "tss_", "xss_", "tir_", "sig_", "cap_", "wep_", "wob_",
    "ckp_", "clm_", "bld_", "rec_", "psg_", "cnd_", "sub_", "cfm_", "asc_", "ctx_", "rev_", "evt_",
    "pub_", "evd_", "rvw_", "cnf_", "rsl_",
];

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn phase_one_summary_is_complete_aggregate_only_and_sanitized() {
    let root = workspace_root();
    let text =
        fs::read_to_string(root.join("tests/reports/dynamic-replay-phase-one-v1.json")).unwrap();
    let report: Value = serde_json::from_str(&text).unwrap();

    assert_eq!(
        report["schema"],
        "shared-context.dynamic-replay-phase-one-summary"
    );
    assert_eq!(report["version"], 1);
    assert_eq!(report["scenario_suite_commit"], "9c4f505");
    assert_eq!(report["mode"], "non_blocking");
    assert_eq!(report["source_policy"], "aggregate_pacing_only");
    assert_eq!(report["runs_per_scenario"], 20);

    let seeds = report["seeds"].as_array().unwrap();
    assert_eq!(seeds.len(), 20);
    assert!(
        seeds
            .iter()
            .enumerate()
            .all(|(seed, value)| value.as_u64() == u64::try_from(seed).ok())
    );

    let scenarios = report["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 6);
    assert_eq!(
        scenarios
            .iter()
            .map(|scenario| scenario["name"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        SCENARIOS.into_iter().collect()
    );
    assert!(scenarios.iter().all(|scenario| {
        scenario["runs"] == 20
            && scenario["passed"].as_u64().unwrap()
                + scenario["expected_fail_open"].as_u64().unwrap()
                == 20
    }));

    assert_eq!(report["totals"]["runs"], 120);
    assert_eq!(report["totals"]["completed"], 120);
    assert_eq!(report["totals"]["failures"], 0);
    assert_eq!(report["totals"]["passed"], 100);
    assert_eq!(report["totals"]["expected_fail_open"], 20);

    let classifications = report["classifications"].as_object().unwrap();
    assert_eq!(
        classifications
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        CLASSIFICATIONS.into_iter().collect()
    );
    assert_eq!(classifications["expected_fail_open"], 20);
    for classification in CLASSIFICATIONS {
        if classification != "expected_fail_open" {
            assert_eq!(classifications[classification], 0);
        }
    }

    let timing = &report["timing_ms"];
    let total = timing["total"].as_u64().unwrap();
    let minimum = timing["min"].as_u64().unwrap();
    let maximum = timing["max"].as_u64().unwrap();
    let rounding = timing["rounding"].as_u64().unwrap();
    assert!(minimum <= maximum && maximum <= total);
    assert_eq!(rounding, 100);
    assert!(
        [total, minimum, maximum]
            .into_iter()
            .all(|value| value % rounding == 0)
    );

    let digest = report["semantic_digest"].as_str().unwrap();
    assert_eq!(digest.len(), 71);
    assert!(digest.starts_with("sha256:"));
    assert!(
        digest[7..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );

    for forbidden in [
        "/Users/",
        "/private/",
        "variables",
        "prompt",
        "transcript",
        "tool_output",
        "stdout",
        "stderr",
        "hostname",
        "timestamp",
    ] {
        assert!(!text.to_ascii_lowercase().contains(forbidden));
    }
    for prefix in DOMAIN_ID_PREFIXES {
        assert!(
            !text.contains(prefix),
            "summary contains a domain identity prefix"
        );
    }
    assert!(DOMAIN_ID_PREFIXES.contains(&"rpg_"));
}

#[test]
fn phase_one_documents_keep_replay_non_blocking_and_product_gated() {
    let root = workspace_root();
    let development = fs::read_to_string(root.join("DEVELOPMENT.md")).unwrap();
    let acceptance = fs::read_to_string(root.join("docs/acceptance-report.md")).unwrap();
    let method = fs::read_to_string(root.join("docs/dynamic-replay-phase-one.md")).unwrap();

    for required in [
        "Dynamic Replay Phase One",
        "non-blocking",
        "It does not replay a real session",
        "aggregate pacing",
        "fixture profiles",
        "6×20",
        "product_invariant_violation",
        "explicitly ignored by default",
        "fresh human approval",
        "fixtures/m4/fixed-oracle.json",
        "hook_to_confirm_chain",
        "#150",
    ] {
        assert!(
            method.contains(required),
            "missing replay policy: {required}"
        );
    }
    for required in [
        "exact fixture profile",
        "reproduces stably",
        "named closed invariant",
        "human explicitly approves",
    ] {
        assert!(
            method.contains(required),
            "missing product-fix Gate: {required}"
        );
    }
    for deferred in [
        "actual Shared Context user session",
        "collection, export, or sanitization pipeline",
        "model-in-the-loop",
        "multi-version Agent matrix",
        "per-commit or merge-blocking gate",
    ] {
        assert!(
            method.contains(deferred),
            "missing deferred boundary: {deferred}"
        );
    }
    assert!(development.contains("Dynamic Replay Phase One（#171）"));
    assert!(development.contains("显式、非阻塞的附加证据"));
    assert!(acceptance.contains("NON-BLOCKING EVIDENCE"));
    assert!(acceptance.contains("production acceptance through Mew #170/#164 is unchanged"));
    assert!(!method.contains("/Users/"));
}

#[test]
fn acceptance_script_tracks_the_sixteen_node_tests() {
    let script =
        fs::read_to_string(workspace_root().join("tests/scripts/run-acceptance.sh")).unwrap();
    assert!(script.contains("package structure (16 Node tests)"));
}
