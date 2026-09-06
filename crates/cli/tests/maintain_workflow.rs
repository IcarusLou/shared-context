//! `sctx maintain` from the outside: a real installation, the real binary, both output modes.
//!
//! The orchestration itself is covered against injected clocks and locks in the installer's own
//! matrix. What only an end-to-end run can show is that the command is reachable, that the digest
//! it writes is the one `doctor` reads back, and that a person running it without `--json` gets
//! something written for a person.

use std::{fs, path::PathBuf, process::Command};

use serde_json::Value;

fn sctx(binary: &PathBuf, home: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(binary)
        .args(args)
        .env("SCTX_SKIP_LAUNCHCTL", "1")
        .env("HOME", home)
        .output()
        .unwrap()
}

#[test]
fn maintain_run_and_status_are_reachable_and_feed_doctor() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("maintain cli home");
    fs::create_dir_all(&home).unwrap();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_sctx"));

    let setup = sctx(&binary, &home, &["--json", "setup"]);
    assert!(
        setup.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let root = home.join(".shared-context");

    let run = sctx(&binary, &home, &["--json", "maintain", "run"]);
    assert!(
        run.status.success(),
        "maintain run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let digest: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(digest["schema_version"], 1);
    assert_eq!(digest["mode"], "scheduled");
    assert_eq!(
        digest["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "association_rebuild",
            "candidate_survey",
            "provisional_space_survey",
            "knowledge_sync",
            "logs_sync",
        ]
    );
    // A default `sctx setup` bootstraps the Knowledge Store locally, so there is no remote to sync.
    assert_eq!(digest["steps"][3]["outcome"], "skipped");
    assert_eq!(digest["steps"][4]["outcome"], "skipped");
    assert!(root.join("state/maintain-digest.json").is_file());
    assert!(root.join("state/maintain-last-run").is_file());

    let status = sctx(&binary, &home, &["maintain", "status"]);
    assert!(status.status.success());
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(text.contains("last run: Unix second"), "{text}");
    assert!(text.contains("association_rebuild"), "{text}");
    assert!(text.contains("Candidate Reviews pending"), "{text}");
    assert!(text.contains("provisional Spaces"), "{text}");

    let opportunistic = sctx(
        &binary,
        &home,
        &["--json", "maintain", "run", "--opportunistic"],
    );
    assert!(opportunistic.status.success());
    let digest: Value = serde_json::from_slice(&opportunistic.stdout).unwrap();
    assert_eq!(digest["mode"], "opportunistic");

    let doctor = sctx(&binary, &home, &["--json", "doctor"]);
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "maintain")
        .expect("doctor reports the maintenance check");
    assert_eq!(check["status"], "ok");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("completed every step"),
        "{check}"
    );

    // An unrecognized subcommand is a typed rejection carrying the usage, never a panic.
    let bogus = sctx(&binary, &home, &["--json", "maintain", "sweep"]);
    assert!(!bogus.status.success());
    let error: Value = serde_json::from_slice(&bogus.stderr).unwrap();
    assert_eq!(error["error"]["code"], "invalid_input");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--opportunistic"),
        "{error}"
    );
}
