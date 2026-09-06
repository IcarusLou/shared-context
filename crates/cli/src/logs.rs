use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use sctx_domain::{Error, ErrorKind, Result};
use sctx_log_service::{Collector, CollectorOptions, InitOptions};
use serde::Serialize;

use crate::args::Options;

const HELP: &str = r"Usage:
  sctx logs init [--remote <GIT_URL>] [--email <EMAIL>] [--logs-root <PATH>]
  sctx logs collect [--logs-root <PATH>]
  sctx logs sync [--logs-root <PATH>]
  sctx logs status [--logs-root <PATH>]
  sctx logs prune --cache [--logs-root <PATH>]
  sctx logs doctor --probe [--logs-root <PATH>]
  sctx logs enable|disable [--logs-root <PATH>]
  sctx logs report --input <JSONL_DIRECTORY> [--logs-root <PATH>]
  sctx logs trace --input <JSONL_DIRECTORY> --invocation <ID> [--logs-root <PATH>]

All logging state is independent from the Shared Context business installation.
";

pub(crate) fn run(args: &[String], json_output: bool) -> Result<()> {
    if matches!(args, [arg] if arg == "-h" || arg == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let [command, rest @ ..] = args else {
        return Err(invalid(HELP));
    };
    match command.as_str() {
        "init" => run_init(rest, json_output),
        "collect" => run_collect(rest),
        "sync" => run_sync(rest, json_output),
        "status" => run_status(rest, json_output),
        "prune" => run_prune(rest, json_output),
        "doctor" => run_doctor(rest, json_output),
        "enable" => run_enable(rest, json_output),
        "disable" => run_disable(rest, json_output),
        "report" => run_report(rest, json_output),
        "trace" => run_trace(rest, json_output),
        _ => Err(invalid(HELP)),
    }
}

fn run_init(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--remote", "--email", "--logs-root"], &[])?;
    let root = logs_root(&options)?;
    let remote = options.optional("--remote")?.map(str::to_owned);
    let email = match options.optional("--email")? {
        Some(email) => Some(email.to_owned()),
        None if remote.is_some() => git_user_email()?.ok_or_else(|| {
            invalid(
                "logs init with --remote needs --email because `git config --global user.email` is not set",
            )
        }).map(Some)?,
        None => None,
    };
    let report = sctx_log_service::init(
        &root,
        InitOptions {
            email,
            remote,
            installation_id: installed_id(),
            enabled: true,
        },
    )
    .map_err(service_error)?;

    // Config durability is the boundary: service installation happens afterwards and is best
    // effort. It never turns a valid independent logging configuration into a failed init.
    let mut notices = Vec::new();
    if let Some(home) = home_directory() {
        if let Some(executable) = collector_executable() {
            notices =
                sctx_installer::logs_launchd::reconcile_log_service(&home, &root, &executable)
                    .notices;
        } else {
            notices.push(
                "collector service was not installed because the stable Shared Context runtime is missing; run `sctx setup`, or run `sctx logs collect` manually"
                    .to_owned(),
            );
        }
    }
    emit_json(
        &serde_json::json!({"init": report, "service_notices": notices}),
        json_output,
    )
}

fn run_collect(args: &[String]) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--logs-root"], &[])?;
    let root = logs_root(&options)?;
    let mut collector = Collector::open(
        &root,
        CollectorOptions {
            idle_sleep: Duration::from_millis(25),
            read_buffer_bytes: 16 * 1024,
        },
    )
    .map_err(service_error)?;
    collector
        .run(&Arc::new(AtomicBool::new(false)))
        .map_err(service_error)
}

fn run_sync(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--logs-root"], &[])?;
    let root = logs_root(&options)?;
    let config = sctx_log_service::load_config(&root).map_err(service_error)?;
    let report = sctx_log_sync::sync(&root, &sctx_log_sync::SyncOptions::from_config(&config))
        .map_err(sync_error)?;
    emit_json(&report, json_output)
}

fn run_status(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--logs-root"], &[])?;
    let report = sctx_log_service::status(&logs_root(&options)?).map_err(service_error)?;
    emit_json(&report, json_output)
}

fn run_prune(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--cache"])?;
    options.allow_only(&["--logs-root"], &["--cache"])?;
    if !options.has("--cache") {
        return Err(invalid("logs prune currently requires --cache"));
    }
    let report = sctx_log_sync::prune_cache(&logs_root(&options)?).map_err(sync_error)?;
    emit_json(&report, json_output)
}

fn run_doctor(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--probe"])?;
    options.allow_only(&["--logs-root"], &["--probe"])?;
    if !options.has("--probe") {
        return Err(invalid("logs doctor requires --probe"));
    }
    let report = sctx_log_service::doctor_probe(&logs_root(&options)?).map_err(service_error)?;
    emit_json(&report, json_output)
}

fn run_enable(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--logs-root"], &[])?;
    let root = logs_root(&options)?;
    let config = sctx_log_service::enable(&root).map_err(service_error)?;
    let mut notices = Vec::new();
    if let Some(home) = home_directory() {
        if let Some(executable) = collector_executable() {
            notices =
                sctx_installer::logs_launchd::reconcile_log_service(&home, &root, &executable)
                    .notices;
        } else {
            notices.push(
                "collector service was not installed because the stable Shared Context runtime is missing; run `sctx setup`, or run `sctx logs collect` manually"
                    .to_owned(),
            );
        }
    }
    emit_json(
        &serde_json::json!({"config": config, "service_notices": notices}),
        json_output,
    )
}

fn run_disable(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--logs-root"], &[])?;
    let root = logs_root(&options)?;
    let config = sctx_log_service::disable(&root).map_err(service_error)?;
    let notices = home_directory().map_or_else(Vec::new, |home| {
        sctx_installer::logs_launchd::uninstall_log_service(&home, &root).notices
    });
    emit_json(
        &serde_json::json!({"config": config, "service_notices": notices}),
        json_output,
    )
}

fn run_report(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--input", "--logs-root"], &[])?;
    let report =
        sctx_log_service::report(Path::new(options.required("--input")?)).map_err(service_error)?;
    emit_json(&report, json_output)
}

fn run_trace(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--input", "--invocation", "--logs-root"], &[])?;
    let report = sctx_log_service::trace(
        Path::new(options.required("--input")?),
        options.required("--invocation")?,
    )
    .map_err(service_error)?;
    emit_json(&report, json_output)
}

fn logs_root(options: &Options) -> Result<PathBuf> {
    let root = options
        .optional("--logs-root")?
        .map(PathBuf::from)
        .or_else(sctx_telemetry::default_logs_root)
        .ok_or_else(|| invalid("HOME is not set and --logs-root was not provided"))?;
    if !root.is_absolute() {
        return Err(invalid(
            "--logs-root and SCTX_LOGS_ROOT must be absolute paths",
        ));
    }
    Ok(root)
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn collector_executable() -> Option<PathBuf> {
    home_directory()
        .map(|home| home.join(".shared-context/bin/current/sctx"))
        .filter(|path| path.is_file())
}

fn installed_id() -> Option<String> {
    let path = home_directory()?.join(".shared-context/state/install-manifest.json");
    let bytes = sctx_log_service::read_bounded_file(&path, 64 * 1024).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("installation_id")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn git_user_email() -> Result<Option<String>> {
    let spec = sctx_log_sync::runner::CommandSpec::new("git", Duration::from_secs(2))
        .args(["config", "--global", "--get", "user.email"])
        .output_limit(1024);
    match sctx_log_sync::runner::run(&spec) {
        Ok(output) if output.status.success() => Ok(String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())),
        Ok(_) => Ok(None),
        Err(error) => Err(Error::new(
            ErrorKind::External,
            format!("read global Git email: {error}"),
        )),
    }
}

fn emit_json(value: &impl Serialize, compact: bool) -> Result<()> {
    let encoded = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    }
    .map_err(|error| Error::new(ErrorKind::Io, format!("serialize logs response: {error}")))?;
    println!("{encoded}");
    Ok(())
}

fn service_error(error: sctx_log_service::Error) -> Error {
    let code = error.code();
    let message = error.to_string();
    drop(error);
    Error::new(
        match code {
            sctx_log_service::ErrorCode::InvalidConfig
            | sctx_log_service::ErrorCode::InvalidPath
            | sctx_log_service::ErrorCode::InvalidInput
            | sctx_log_service::ErrorCode::NotConfigured => ErrorKind::InvalidInput,
            sctx_log_service::ErrorCode::Permission
            | sctx_log_service::ErrorCode::Io
            | sctx_log_service::ErrorCode::Endpoint
            | sctx_log_service::ErrorCode::CorruptFrame
            | sctx_log_service::ErrorCode::CorruptBatch
            | sctx_log_service::ErrorCode::StoragePressure => ErrorKind::Io,
        },
        format!("logs {code:?}: {message}"),
    )
}

fn sync_error(error: sctx_log_sync::SyncError) -> Error {
    let code = error.code();
    let message = error.to_string();
    drop(error);
    Error::new(
        match code {
            sctx_log_sync::SyncErrorCode::InvalidRoot
            | sctx_log_sync::SyncErrorCode::InvalidBatch
            | sctx_log_sync::SyncErrorCode::InvalidRemote => ErrorKind::InvalidInput,
            sctx_log_sync::SyncErrorCode::Conflict
            | sctx_log_sync::SyncErrorCode::RemoteHistoryMissing
            | sctx_log_sync::SyncErrorCode::PendingJournal => ErrorKind::Conflict,
            sctx_log_sync::SyncErrorCode::Io
            | sctx_log_sync::SyncErrorCode::Git
            | sctx_log_sync::SyncErrorCode::Timeout
            | sctx_log_sync::SyncErrorCode::StorageBudget => ErrorKind::External,
        },
        format!("logs sync {code:?}: {message}"),
    )
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
