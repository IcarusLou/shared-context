//! `sctx policy` and the operator-visible half of runtime policy delivery.
//!
//! Delivery itself is fail-open and silent by design, which is exactly why the operator needs a
//! command that answers "what is actually being delivered, and from where". These tests hold that
//! answer to the real binary: the shipped default as `sctx setup` seeds it, an edited file, a
//! broken file, and what `sctx doctor` says about each.

use std::{fs, path::Path, process::Command};

use sctx_local_state::{DEFAULT_POLICY_MARKDOWN, POLICY_FILE_NAME, Policy, STOP_SECTION_MAX_BYTES};
use serde_json::Value;

fn sctx(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(args)
        .env("SCTX_SKIP_LAUNCHCTL", "1")
        .env("HOME", home)
        .output()
        .unwrap()
}

fn installed_home(temporary: &Path) -> std::path::PathBuf {
    let home = temporary.join("policy home");
    fs::create_dir_all(&home).unwrap();
    let setup = sctx(&home, &["--json", "setup"]);
    assert!(
        setup.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    home
}

#[test]
fn setup_seeds_the_default_policy_once_and_never_overwrites_an_edited_one() {
    let temporary = tempfile::tempdir().unwrap();
    let home = installed_home(temporary.path());
    let path = home.join(".shared-context").join(POLICY_FILE_NAME);
    assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_POLICY_MARKDOWN);

    let edited = "## stop\nour own line\n";
    fs::write(&path, edited).unwrap();
    let again = sctx(&home, &["--json", "setup"]);
    assert!(
        again.status.success(),
        "second setup failed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        edited,
        "policy.md is the operator's document from the moment it exists"
    );
}

#[test]
fn policy_show_prints_the_effective_policy_and_its_source() {
    let temporary = tempfile::tempdir().unwrap();
    let home = installed_home(temporary.path());
    let root = home.join(".shared-context");

    let shown = sctx(&home, &["policy", "show"]);
    assert!(shown.status.success());
    let text = String::from_utf8(shown.stdout).unwrap();
    let default = Policy::compiled_default();
    let expected = format!(
        "path: {}\nstatus: policy loaded from {}\n\n## session\n{}\n\n## checkpoint\n{}\n\n## \
         stop\n{}\n\n## triage\n{}\n",
        root.join(POLICY_FILE_NAME).display(),
        root.join(POLICY_FILE_NAME).display(),
        default.session(),
        default.checkpoint(),
        default.stop(),
        default.triage(),
    );
    assert_eq!(text, expected);

    let json = sctx(&home, &["--json", "policy", "show"]);
    assert!(json.status.success());
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["status"], "policy_loaded");
    assert_eq!(value["sections"]["stop"], default.stop());
    assert!(value["oversize_sections"].as_array().unwrap().is_empty());
}

#[test]
fn policy_show_names_an_absent_file_without_failing() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("bare home");
    fs::create_dir_all(&home).unwrap();
    let shown = sctx(&home, &["--json", "policy", "show"]);
    assert!(shown.status.success());
    let value: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(value["status"], "policy_default");
    assert_eq!(
        value["sections"]["triage"],
        Policy::compiled_default().triage()
    );
}

#[test]
fn policy_reset_backs_up_the_existing_file_before_rewriting_the_default() {
    let temporary = tempfile::tempdir().unwrap();
    let home = installed_home(temporary.path());
    let root = home.join(".shared-context");
    let path = root.join(POLICY_FILE_NAME);
    fs::write(&path, "## stop\nmine\n").unwrap();

    let reset = sctx(&home, &["--json", "policy", "reset"]);
    assert!(
        reset.status.success(),
        "reset failed: {}",
        String::from_utf8_lossy(&reset.stderr)
    );
    let value: Value = serde_json::from_slice(&reset.stdout).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_POLICY_MARKDOWN);
    let backup = value["backup_path"].as_str().unwrap();
    assert!(
        backup.contains(&format!("{POLICY_FILE_NAME}.")) && backup.contains(".bak"),
        "{backup}"
    );
    assert_eq!(fs::read_to_string(backup).unwrap(), "## stop\nmine\n");
}

#[test]
fn doctor_reports_an_oversize_section_that_fell_back_to_the_default() {
    let temporary = tempfile::tempdir().unwrap();
    let home = installed_home(temporary.path());
    let path = home.join(".shared-context").join(POLICY_FILE_NAME);

    let healthy = sctx(&home, &["--json", "doctor"]);
    let report: Value = serde_json::from_slice(&healthy.stdout).unwrap();
    let check = policy_check(&report);
    assert_eq!(check["status"], "ok", "{check}");

    fs::write(
        &path,
        format!(
            "## stop\n{}\n\n## session\nkept\n",
            "x".repeat(STOP_SECTION_MAX_BYTES + 1)
        ),
    )
    .unwrap();
    let degraded = sctx(&home, &["--json", "doctor"]);
    let report: Value = serde_json::from_slice(&degraded.stdout).unwrap();
    let check = policy_check(&report);
    assert_eq!(check["status"], "warning", "{check}");
    let message = check["message"].as_str().unwrap();
    assert!(message.contains("stop"), "{message}");
    assert!(message.contains("byte ceiling"), "{message}");
    // A degraded policy is a warning about wording, never an unhealthy installation.
    assert_eq!(report["healthy"], true);
}

fn policy_check(report: &Value) -> Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "policy")
        .unwrap_or_else(|| panic!("no policy check in {report}"))
        .clone()
}
