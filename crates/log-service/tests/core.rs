use std::{
    fs,
    path::Path,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sctx_log_service::{
    Collector, CollectorOptions, ErrorCode, InitOptions, SealRequestOutcome, disable,
    encode_email_path, init, list_ready_batches, load_config, load_hook_diagnostics,
    load_ready_batch, report, request_seal, status, trace,
};
use sctx_telemetry::{EntryPoint, Event, EventKind, Outcome, emit_to};
use tempfile::TempDir;

fn options(email: &str, remote: &Path) -> InitOptions {
    InitOptions {
        email: Some(email.to_owned()),
        remote: Some(remote.display().to_string()),
        installation_id: Some("installation-1".to_owned()),
        enabled: true,
    }
}

fn event(invocation: &str) -> Event {
    let mut event = Event::finished(
        EntryPoint::Hook,
        EventKind::HookDecision,
        invocation,
        "session_start",
        Outcome::Success,
    );
    event.reason = Some("enabled".to_owned());
    event.duration_ms = Some(7);
    event.result_count = Some(0);
    event
}

fn remove_runtime(root: &Path) {
    let endpoint = sctx_telemetry::default_endpoint(root);
    let _ = fs::remove_file(&endpoint);
    if let Some(parent) = endpoint.parent() {
        let _ = fs::remove_dir(parent);
    }
}

#[test]
fn collector_publishes_verified_atomic_batch_and_offline_queries_it() {
    let root = TempDir::new().expect("root");
    let remote = root.path().join("remote.git");
    init(root.path(), options("Alice@Example.COM", &remote)).expect("init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    let sent = event("invocation-1");
    assert_eq!(
        emit_to(root.path(), &sent),
        sctx_telemetry::EmitOutcome::Sent
    );
    assert_eq!(collector.collect_once().expect("collect"), 1);
    let ready = collector.seal().expect("seal").expect("ready batch");
    assert_eq!(ready.manifest.event_count, 1);
    assert_eq!(
        ready.manifest.stream.email.as_deref(),
        Some("Alice@example.com")
    );
    assert_eq!(
        load_ready_batch(&ready.directory).expect("verify").manifest,
        ready.manifest
    );
    let report = report(&ready.directory).expect("report");
    assert_eq!(report.events_scanned, 1);
    assert_eq!(report.rows[0].empty_results, 1);
    let trace = trace(&ready.directory, "invocation-1").expect("trace");
    assert_eq!(trace.events, vec![sent.normalized()]);
    assert_eq!(
        load_hook_diagnostics(root.path())
            .expect("diagnostics")
            .total,
        1
    );
    drop(collector);
    remove_runtime(root.path());
}

#[test]
fn tampering_is_detected_and_identity_rotation_does_not_reassign_ready_batch() {
    let root = TempDir::new().expect("root");
    let first_remote = root.path().join("first.git");
    let first = init(root.path(), options("a@example.com", &first_remote)).expect("first init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    assert_eq!(
        emit_to(root.path(), &event("old-stream")),
        sctx_telemetry::EmitOutcome::Sent
    );
    collector.collect_once().expect("collect");
    let batch = collector.seal().expect("seal").expect("batch");
    drop(collector);

    let second_remote = root.path().join("second.git");
    let rotated = init(root.path(), options("b@example.com", &second_remote)).expect("rotate");
    assert!(rotated.rotated_stream);
    assert_ne!(rotated.config.stream_id, first.config.stream_id);
    let unchanged = load_ready_batch(&batch.directory).expect("old batch remains valid");
    assert_eq!(unchanged.manifest.stream.stream_id, first.config.stream_id);
    assert_eq!(
        unchanged.manifest.stream.remote.as_deref(),
        Some(first_remote.to_str().expect("utf8"))
    );

    fs::OpenOptions::new()
        .append(true)
        .open(&batch.events_path)
        .expect("open")
        .write_all(b"{}\n")
        .expect("tamper");
    assert_eq!(
        load_ready_batch(&batch.directory)
            .expect_err("tamper rejected")
            .code(),
        ErrorCode::CorruptBatch
    );
    let damaged_status = status(root.path()).expect("status remains available");
    assert!(!damaged_status.spool_observed);
    assert_eq!(damaged_status.spool_error, Some(ErrorCode::CorruptBatch));
    remove_runtime(root.path());
}

#[test]
fn active_recovery_uses_manifest_if_crash_happened_after_publication_cleanup() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .expect("init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    assert_eq!(
        emit_to(root.path(), &event("recover")),
        sctx_telemetry::EmitOutcome::Sent
    );
    collector.collect_once().expect("collect");
    let ready = collector.seal().expect("seal").expect("ready");
    drop(collector);
    for entry in fs::read_dir(root.path().join("spool/active")).expect("active") {
        fs::remove_dir_all(entry.expect("entry").path()).expect("remove empty current active");
    }
    let crashed = root
        .path()
        .join("spool/active")
        .join(format!("{}.building", ready.manifest.batch_id));
    fs::rename(&ready.directory, &crashed).expect("simulate crash before ready rename");
    assert!(!crashed.join("binding.json").exists());
    let mut recovered =
        Collector::open(root.path(), CollectorOptions::default()).expect("recover collector");
    let republished = recovered.seal().expect("reseal").expect("republish");
    assert_eq!(republished.manifest.batch_id, ready.manifest.batch_id);
    assert_eq!(
        republished.manifest.content_sha256,
        ready.manifest.content_sha256
    );
    drop(recovered);
    remove_runtime(root.path());
}

#[test]
fn corrupt_config_is_never_overwritten_and_concurrent_init_converges() {
    let root = TempDir::new().expect("root");
    let remote = root.path().join("remote.git");
    init(root.path(), options("a@example.com", &remote)).expect("init");
    let config_path = root.path().join("config.toml");
    fs::write(&config_path, b"not = [valid").expect("corrupt config");
    let before = fs::read(&config_path).expect("before");
    let error =
        init(root.path(), options("a@example.com", &remote)).expect_err("must reject corruption");
    assert_eq!(error.code(), ErrorCode::InvalidConfig);
    assert_eq!(fs::read(&config_path).expect("after"), before);

    let concurrent = TempDir::new().expect("concurrent root");
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let barrier = Arc::clone(&barrier);
        let path = concurrent.path().to_path_buf();
        workers.push(thread::spawn(move || {
            barrier.wait();
            init(
                &path,
                InitOptions {
                    email: None,
                    remote: None,
                    installation_id: None,
                    enabled: true,
                },
            )
            .expect("concurrent init")
            .config
            .stream_id
        }));
    }
    let ids = workers
        .into_iter()
        .map(|worker| worker.join().expect("join"))
        .collect::<Vec<_>>();
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(
        load_config(concurrent.path())
            .expect("valid config")
            .stream_id,
        ids[0]
    );
}

#[test]
fn collector_is_singleton_disable_is_observed_and_status_marks_unknown_drops() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .expect("init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    let second = Collector::open(root.path(), CollectorOptions::default());
    assert_eq!(
        second.err().expect("singleton error").code(),
        ErrorCode::Endpoint
    );
    disable(root.path()).expect("disable");
    thread::sleep(Duration::from_millis(1_050));
    assert_eq!(collector.collect_once().expect("disabled refresh"), 0);
    assert!(
        !status(root.path())
            .expect("status")
            .drop_observation_complete
    );
    drop(collector);
    remove_runtime(root.path());
}

#[test]
fn cross_process_style_seal_request_gets_matching_ack() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .expect("init");
    let collector = Collector::open(
        root.path(),
        CollectorOptions {
            idle_sleep: Duration::from_millis(5),
            read_buffer_bytes: 512,
        },
    )
    .expect("collector");
    for sequence in 0..3 {
        let mut queued = event("seal-request");
        queued.sequence = sequence;
        assert_eq!(
            emit_to(root.path(), &queued),
            sctx_telemetry::EmitOutcome::Sent
        );
    }
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        let mut collector = collector;
        collector.run(&worker_stop).expect("collector run");
    });
    let outcome =
        request_seal(root.path(), &(Instant::now() + Duration::from_secs(2))).expect("request");
    assert_eq!(outcome, SealRequestOutcome::Sealed);
    assert_eq!(
        load_hook_diagnostics(root.path())
            .expect("sealed diagnostics fence")
            .total,
        3
    );
    assert_eq!(
        list_ready_batches(root.path())
            .expect("sealed batch")
            .iter()
            .map(|batch| batch.manifest.event_count)
            .sum::<u64>(),
        3
    );
    stop.store(true, Ordering::Relaxed);
    worker.join().expect("join");
    assert!(!list_ready_batches(root.path()).expect("ready").is_empty());
    remove_runtime(root.path());
}

#[test]
fn report_uses_terminal_operations_and_result_count_denominators() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .expect("init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    let start_one = Event::started(EntryPoint::Mcp, EventKind::ToolStarted, "one", "query");
    let mut success = Event::finished(
        EntryPoint::Mcp,
        EventKind::ToolFinished,
        "one",
        "query",
        Outcome::Success,
    );
    success.result_count = None;
    let start_two = Event::started(EntryPoint::Mcp, EventKind::ToolStarted, "two", "query");
    let mut failure = Event::finished(
        EntryPoint::Mcp,
        EventKind::ToolFinished,
        "two",
        "query",
        Outcome::Failure,
    );
    failure.result_count = Some(0);
    failure.error_code = Some("timeout".to_owned());
    for event in [&start_one, &success, &start_two, &failure] {
        assert_eq!(
            emit_to(root.path(), event),
            sctx_telemetry::EmitOutcome::Sent
        );
    }
    assert_eq!(collector.collect_once().expect("collect"), 4);
    let ready = collector.seal().expect("seal").expect("ready");
    let row = report(&ready.directory)
        .expect("report")
        .rows
        .into_iter()
        .next()
        .expect("row");
    assert_eq!(row.count, 2);
    assert_eq!(row.failures, 1);
    assert!((row.failure_rate - 0.5).abs() < f64::EPSILON);
    assert_eq!(row.results_with_count, 1);
    assert!((row.empty_result_rate - 1.0).abs() < f64::EPSILON);
    assert_eq!(row.error_counts[0].error_code, "timeout");
    drop(collector);
    remove_runtime(root.path());
}

#[test]
fn configuration_rejects_inline_credentials_and_encodes_email_paths() {
    let root = TempDir::new().expect("root");
    let error = init(
        root.path(),
        InitOptions {
            email: Some("a@example.com".to_owned()),
            remote: Some("https://user:secret@example.com/logs.git".to_owned()),
            installation_id: None,
            enabled: true,
        },
    )
    .expect_err("credentials rejected");
    assert_eq!(error.code(), ErrorCode::InvalidConfig);
    assert_eq!(
        encode_email_path("first+tag@Example.COM").expect("path identity"),
        "first%2Btag@example.com"
    );
    assert_eq!(
        init(
            root.path(),
            options("a@example.com", &root.path().join("remote.git"))
        )
        .expect("absolute local remote")
        .config
        .installation_id,
        "installation-1"
    );
}

#[test]
fn storage_pressure_preserves_existing_spool_and_recovers_after_space_returns() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .expect("init");
    let pressure = root.path().join("spool/ready/pressure.bin");
    fs::File::create(&pressure)
        .expect("pressure file")
        .set_len(256 * 1024 * 1024)
        .expect("sparse pressure");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    assert_eq!(
        emit_to(root.path(), &event("dropped-under-pressure")),
        sctx_telemetry::EmitOutcome::Sent
    );
    collector.collect_once().expect("drain pressured event");
    assert!(
        status(root.path())
            .expect("pressure status")
            .storage_pressure
    );
    assert!(pressure.exists());

    fs::remove_file(&pressure).expect("space returns");
    thread::sleep(Duration::from_millis(1_050));
    assert_eq!(
        emit_to(root.path(), &event("accepted-after-pressure")),
        sctx_telemetry::EmitOutcome::Sent
    );
    collector.collect_once().expect("collect after pressure");
    assert!(collector.seal().expect("seal").is_some());
    drop(collector);
    remove_runtime(root.path());
}

#[test]
fn first_assignment_binds_unassigned_ready_batches_once() {
    let root = TempDir::new().expect("root");
    let local = init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: Some("installation-1".to_owned()),
            enabled: true,
        },
    )
    .expect("local init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    assert_eq!(
        emit_to(root.path(), &event("unassigned")),
        sctx_telemetry::EmitOutcome::Sent
    );
    collector.collect_once().expect("collect");
    let batch = collector.seal().expect("seal").expect("batch");
    drop(collector);
    assert!(batch.manifest.stream.remote.is_none());

    let assigned = init(
        root.path(),
        options("owner@example.com", &root.path().join("remote.git")),
    )
    .expect("assign");
    assert_eq!(assigned.bound_unassigned_batches, 1);
    assert_ne!(assigned.config.stream_id, local.config.stream_id);
    let rebound = load_ready_batch(&batch.directory).expect("rebound batch");
    assert_eq!(rebound.manifest.stream.stream_id, assigned.config.stream_id);
    assert_eq!(
        rebound.manifest.stream.email.as_deref(),
        Some("owner@example.com")
    );
    remove_runtime(root.path());
}

#[test]
fn bounded_state_reads_reject_sparse_files_and_fifo_config_without_blocking() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: None,
            enabled: true,
        },
    )
    .expect("init");
    let config = root.path().join("config.toml");
    fs::File::create(&config)
        .expect("oversized config")
        .set_len(1024 * 1024)
        .expect("sparse config");
    let before = fs::metadata(&config).expect("metadata").len();
    assert_eq!(
        load_config(root.path()).expect_err("bounded reject").code(),
        ErrorCode::InvalidConfig
    );
    assert!(
        init(
            root.path(),
            InitOptions {
                email: None,
                remote: None,
                installation_id: None,
                enabled: true
            }
        )
        .is_err()
    );
    assert_eq!(fs::metadata(&config).expect("preserved").len(), before);

    fs::remove_file(&config).expect("remove sparse config");
    let mkfifo_status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(&config)
        .status()
        .expect("mkfifo config");
    assert!(mkfifo_status.success());
    let started = Instant::now();
    assert_eq!(
        load_config(root.path()).expect_err("FIFO rejected").code(),
        ErrorCode::InvalidConfig
    );
    assert!(started.elapsed() < Duration::from_millis(100));

    fs::remove_file(&config).expect("remove FIFO config");
    fs::File::create(root.path().join("state/collector-status.json"))
        .expect("status file")
        .set_len(128 * 1024)
        .expect("sparse status");
    let report = status(root.path()).expect("bounded status report");
    assert_eq!(report.collector_status_error, Some(ErrorCode::InvalidInput));
}

#[test]
fn live_collector_seals_and_binds_active_unassigned_data_on_first_assignment() {
    let root = TempDir::new().expect("root");
    init(
        root.path(),
        InitOptions {
            email: None,
            remote: None,
            installation_id: Some("installation-1".to_owned()),
            enabled: true,
        },
    )
    .expect("local init");
    let mut collector =
        Collector::open(root.path(), CollectorOptions::default()).expect("collector");
    assert_eq!(
        emit_to(root.path(), &event("active-unassigned")),
        sctx_telemetry::EmitOutcome::Sent
    );
    collector.collect_once().expect("collect active event");
    let assigned = init(
        root.path(),
        options("owner@example.com", &root.path().join("remote.git")),
    )
    .expect("assign while collector lives");
    thread::sleep(Duration::from_millis(1_050));
    collector.collect_once().expect("reload assignment");
    let batches = list_ready_batches(root.path()).expect("ready after assignment");
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0].manifest.stream.stream_id,
        assigned.config.stream_id
    );
    drop(collector);
    remove_runtime(root.path());
}

use std::io::Write;
