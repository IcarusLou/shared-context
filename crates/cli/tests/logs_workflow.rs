use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct CollectorProcess(Child);

impl Drop for CollectorProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(home: &Path, logs_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sctx"));
    // Logging lifecycle uses a real, fixed launchd label. Every subprocess in this suite must
    // disable registration even though HOME points at a temporary directory.
    command
        .env("HOME", home)
        .env("SCTX_LOGS_ROOT", logs_root)
        .env("SCTX_SKIP_LAUNCHCTL", "1");
    command
}

fn run_json(home: &Path, logs_root: &Path, args: &[&str]) -> Value {
    let output = command(home, logs_root)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sctx {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn start_collector(home: &Path, logs_root: &Path) -> CollectorProcess {
    let child = command(home, logs_root)
        .args(["logs", "collect", "--logs-root"])
        .arg(logs_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let endpoint = sctx_telemetry::default_endpoint(logs_root);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !endpoint.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(endpoint.exists(), "collector endpoint was not created");
    CollectorProcess(child)
}

fn hook_raw(home: &Path, logs_root: &Path, input: &[u8]) -> std::process::Output {
    let mut child = command(home, logs_root)
        .args(["hook", "--agent", "cursor"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

#[test]
fn independent_logs_commands_work_when_business_state_is_corrupt() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let logs_root = temporary.path().join("independent logs");
    fs::create_dir_all(home.join(".shared-context")).unwrap();
    fs::write(
        home.join(".shared-context/config.toml"),
        b"this is not business config",
    )
    .unwrap();

    let initialized = run_json(
        &home,
        &logs_root,
        &["logs", "init", "--logs-root", logs_root.to_str().unwrap()],
    );
    assert_eq!(initialized["init"]["config"]["enabled"], json!(true));
    assert_eq!(initialized["init"]["config"]["remote"], Value::Null);

    let status = run_json(
        &home,
        &logs_root,
        &["logs", "status", "--logs-root", logs_root.to_str().unwrap()],
    );
    assert_eq!(status["configured"], json!(true));
    assert_eq!(status["enabled"], json!(true));

    let probe = run_json(
        &home,
        &logs_root,
        &[
            "logs",
            "doctor",
            "--probe",
            "--logs-root",
            logs_root.to_str().unwrap(),
        ],
    );
    assert_eq!(probe["logs_root"], json!("ok"));
}

#[test]
fn relative_logs_root_is_rejected_before_it_can_enter_a_launch_agent() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let output = command(&home, Path::new("relative-logs"))
        .args(["logs", "init", "--logs-root", "relative-logs"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be absolute"));
}

#[test]
fn telemetry_endpoint_fault_does_not_change_business_output_or_exit_status() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let logs_root = temporary.path().join("logs");
    fs::create_dir_all(&home).unwrap();
    let baseline = command(&home, &logs_root)
        .args(["setup", "--not-a-real-option", "value"])
        .output()
        .unwrap();

    let endpoint = sctx_telemetry::default_endpoint(&logs_root);
    fs::create_dir_all(endpoint.parent().unwrap()).unwrap();
    fs::write(&endpoint, b"not a fifo").unwrap();
    let faulted = command(&home, &logs_root)
        .args(["setup", "--not-a-real-option", "value"])
        .output()
        .unwrap();
    assert_eq!(faulted.status, baseline.status);
    assert_eq!(faulted.stdout, baseline.stdout);
    assert_eq!(faulted.stderr, baseline.stderr);
    let _ = fs::remove_file(&endpoint);
    let _ = fs::remove_dir(endpoint.parent().unwrap());
}

#[test]
fn logs_init_ignores_fifo_and_oversized_business_manifests_without_blocking() {
    for (name, oversized) in [("fifo manifest", false), ("oversized manifest", true)] {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let logs_root = temporary.path().join(name);
        let manifest = home.join(".shared-context/state/install-manifest.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        if oversized {
            fs::write(&manifest, vec![b'x'; 64 * 1024 + 1]).unwrap();
        } else {
            let status = Command::new("mkfifo")
                .args(["-m", "600"])
                .arg(&manifest)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let started = Instant::now();
        let initialized = run_json(
            &home,
            &logs_root,
            &["logs", "init", "--logs-root", logs_root.to_str().unwrap()],
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            initialized["init"]["config"]["installation_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }
}

#[test]
fn ordinary_doctor_reports_logging_as_a_warning_without_marking_business_unhealthy() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let logs_root = temporary.path().join("logs with spaces");
    fs::create_dir_all(&home).unwrap();
    run_json(
        &home,
        &logs_root,
        &["logs", "init", "--logs-root", logs_root.to_str().unwrap()],
    );
    run_json(&home, &logs_root, &["setup"]);

    let doctor = run_json(&home, &logs_root, &["doctor"]);
    let logging = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == json!("logging"))
        .expect("ordinary doctor logging check");
    assert_eq!(logging["status"], json!("warning"));
    assert_eq!(doctor["healthy"], json!(true));
}

#[test]
fn business_lifecycle_manages_only_the_owned_service_and_preserves_logging_data() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let logs_root = temporary.path().join("logs with spaces");
    fs::create_dir_all(&home).unwrap();
    run_json(
        &home,
        &logs_root,
        &["logs", "init", "--logs-root", logs_root.to_str().unwrap()],
    );
    run_json(&home, &logs_root, &["setup"]);

    let plist = home
        .join("Library/LaunchAgents")
        .join("com.shared-context.logs.plist");
    let plist_text = fs::read_to_string(&plist).unwrap();
    assert!(
        plist_text.contains(".shared-context/bin/current/sctx"),
        "{plist_text}"
    );
    assert!(logs_root.join("state/service-ownership.json").is_file());

    let uninstall = run_json(&home, &logs_root, &["uninstall"]);
    assert!(!plist.exists());
    assert!(logs_root.join("config.toml").is_file());
    assert!(
        uninstall["preserved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == &json!(logs_root))
    );
}

#[test]
fn hook_diagnostics_use_the_collector_and_never_create_runtime_sqlite() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let logs_root = temporary.path().join("logs");
    fs::create_dir_all(&home).unwrap();
    run_json(
        &home,
        &logs_root,
        &["logs", "init", "--logs-root", logs_root.to_str().unwrap()],
    );
    let _collector = start_collector(&home, &logs_root);

    let hook = hook_raw(&home, &logs_root, b"NOT_JSON_AT_ALL");
    assert!(hook.status.success());
    assert_eq!(String::from_utf8_lossy(&hook.stdout).trim(), "{}");

    let deadline = Instant::now() + Duration::from_secs(3);
    let report = loop {
        let report = run_json(&home, &logs_root, &["doctor", "--hooks"]);
        if report["counts"].as_array().is_some_and(|counts| {
            counts.iter().any(|entry| {
                entry["decision"] == json!("fail_open")
                    && entry["reason"] == json!("payload_decode_failed")
            })
        }) {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "collector did not publish Hook diagnostics"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(report["source"], json!("telemetry"));
    assert!(!home.join(".shared-context/state/runtime.sqlite").exists());
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains(temporary.path().to_str().unwrap()));
}
