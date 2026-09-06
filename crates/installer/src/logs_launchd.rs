//! Independent lifecycle for the telemetry collector `LaunchAgent`.
//!
//! This deliberately lives outside the main install manifest and transaction. Callers invoke it
//! only after setup/upgrade has returned (and therefore after the business lease and setup lock
//! have been released). Every failure is represented as a notice so a logging fault can never
//! roll back or fail an otherwise working installation.

use std::{
    fs,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    time::Duration,
};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::SKIP_LAUNCHCTL_ENV;

pub const LOGS_LAUNCH_AGENT_LABEL: &str = "com.shared-context.logs";
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PLIST_BYTES: u64 = 64 * 1024;
const MAX_OWNERSHIP_BYTES: u64 = 64 * 1024;
const MAX_TRANSACTION_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LogServiceLifecycleReport {
    pub configured: bool,
    pub changed: bool,
    pub plist: Option<PathBuf>,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceOwnership {
    schema_version: u32,
    label: String,
    plist: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceTransaction {
    schema_version: u32,
    plist: PathBuf,
    desired_sha256: String,
    previous_plist: Option<Vec<u8>>,
    previous_ownership: Option<ServiceOwnership>,
}

enum ManagedRead<T> {
    Missing,
    Valid(T),
    Invalid(String),
}

enum LifecycleLock {
    Acquired(fs::File),
    Busy,
    Invalid(String),
}

/// Installs or refreshes the independent collector service when logging was explicitly initialized.
///
/// The function is intentionally infallible. A malformed logging configuration still counts as
/// configured here: `sctx logs doctor` owns its diagnosis, while setup remains successful.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn reconcile_log_service(
    home: &Path,
    logs_root: &Path,
    executable: &Path,
) -> LogServiceLifecycleReport {
    let mut report = LogServiceLifecycleReport::default();
    let config = match sctx_log_service::load_config(logs_root) {
        Ok(config) => {
            report.configured = true;
            config
        }
        Err(error) if error.code() == sctx_log_service::ErrorCode::NotConfigured => return report,
        Err(error) => {
            report.configured = logs_root.join("config.toml").symlink_metadata().is_ok();
            report.notices.push(format!(
                "logging is configured but its independent config is invalid ({error}); run `sctx logs doctor --probe`"
            ));
            return report;
        }
    };
    let _lock = match acquire_lifecycle_lock(home) {
        LifecycleLock::Acquired(file) => file,
        LifecycleLock::Busy => {
            report.notices.push(
                "logging service lifecycle is already active for this user; preserved the current service state"
                    .to_owned(),
            );
            return report;
        }
        LifecycleLock::Invalid(error) => {
            report.notices.push(format!(
                "logging service lifecycle lock is unavailable; preserved the current service state ({error})"
            ));
            return report;
        }
    };
    if !recover_service_transaction(home, logs_root, &mut report.notices) {
        return report;
    }
    if !config.enabled {
        return uninstall_log_service_locked(home, logs_root, report);
    }
    if std::env::consts::OS != "macos" {
        report.notices.push(
            "logging is configured, but the collector LaunchAgent is only available on macOS; run `sctx logs collect` from your own service manager"
                .to_owned(),
        );
        return report;
    }

    let plist = launch_agent_path(home);
    report.plist = Some(plist.clone());
    let desired = match render_logs_plist(executable, logs_root) {
        Ok(value) => value,
        Err(reason) => {
            report.notices.push(reason);
            return report;
        }
    };
    let desired_hash = sha256(desired.as_bytes());
    let ownership_path = ownership_path(logs_root);
    let mut prior = match read_ownership(&ownership_path, home) {
        ManagedRead::Valid(owned) => Some(owned),
        ManagedRead::Missing => None,
        ManagedRead::Invalid(error) => {
            report.notices.push(format!(
                "preserved logging service files because ownership is invalid at {}: {error}",
                ownership_path.display()
            ));
            return report;
        }
    };
    let ownership = ServiceOwnership {
        schema_version: 1,
        label: LOGS_LAUNCH_AGENT_LABEL.to_owned(),
        plist: plist.clone(),
        sha256: desired_hash.clone(),
    };

    match fs::symlink_metadata(&plist) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let current = match read_managed_bytes(&plist, MAX_PLIST_BYTES) {
                ManagedRead::Valid(bytes) => bytes,
                ManagedRead::Missing => {
                    report.notices.push(format!(
                        "logging LaunchAgent disappeared during lifecycle inspection: {}",
                        plist.display()
                    ));
                    return report;
                }
                ManagedRead::Invalid(error) => {
                    report.notices.push(format!(
                        "could not read the logging LaunchAgent at {}: {error}",
                        plist.display()
                    ));
                    return report;
                }
            };
            let current_hash = sha256(&current);
            if prior.as_ref().is_none_or(|owned| {
                owned.label != LOGS_LAUNCH_AGENT_LABEL
                    || owned.plist != plist
                    || owned.sha256 != current_hash
            }) {
                report.notices.push(format!(
                    "preserved an unowned or user-modified logging LaunchAgent at {}",
                    plist.display()
                ));
                return report;
            }
            if current_hash != desired_hash {
                if let Err(error) = commit_service_transaction(
                    logs_root,
                    &plist,
                    desired.as_bytes(),
                    &ownership,
                    Some(current),
                    prior.clone(),
                ) {
                    report.notices.push(format!(
                        "could not update the logging LaunchAgent transaction at {}: {error}",
                        plist.display()
                    ));
                    return report;
                }
                report.changed = true;
            }
        }
        Ok(_) => {
            report.notices.push(format!(
                "preserved the logging LaunchAgent path because it is not a regular file: {}",
                plist.display()
            ));
            return report;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if prior.is_some() {
                // The valid ownership named exactly this now-missing plist. Retire that stale
                // record before beginning a fresh paired transaction; a crash between these two
                // operations leaves no service state to misinterpret and the next run starts new.
                if let Err(error) = remove_file_and_sync_parent(&ownership_path) {
                    report.notices.push(format!(
                        "could not retire stale logging service ownership before reinstall: {error}"
                    ));
                    return report;
                }
                prior = None;
            }
            if let Err(error) = commit_service_transaction(
                logs_root,
                &plist,
                desired.as_bytes(),
                &ownership,
                None,
                prior.clone(),
            ) {
                report.notices.push(format!(
                    "could not install the logging LaunchAgent transaction at {}: {error}",
                    plist.display()
                ));
                return report;
            }
            report.changed = true;
        }
        Err(error) => {
            report.notices.push(format!(
                "could not inspect the logging LaunchAgent at {}: {error}",
                plist.display()
            ));
            return report;
        }
    }

    // Re-register even when bytes are current: enable and upgrade must recover a job that launchd
    // previously failed to load, and an upgrade should restart a still-running older collector.
    if launchctl_enabled() {
        if let Err(error) = replace_launch_agent(home, &plist) {
            report.notices.push(format!(
                "the logging LaunchAgent was written, but launchd could not activate it within the bounded service timeout ({error}); it can be retried with `sctx logs enable`"
            ));
        }
    }
    report
}

/// Stops and removes the exactly-owned collector `LaunchAgent` while retaining all logging data.
#[must_use]
pub fn uninstall_log_service(home: &Path, logs_root: &Path) -> LogServiceLifecycleReport {
    let mut report = LogServiceLifecycleReport {
        configured: logs_root.join("config.toml").symlink_metadata().is_ok(),
        ..LogServiceLifecycleReport::default()
    };
    let _lock = match acquire_lifecycle_lock(home) {
        LifecycleLock::Acquired(file) => file,
        LifecycleLock::Busy => {
            report.notices.push(
                "logging service lifecycle is already active for this user; preserved the current service state"
                    .to_owned(),
            );
            return report;
        }
        LifecycleLock::Invalid(error) => {
            report.notices.push(format!(
                "logging service lifecycle lock is unavailable; preserved the current service state ({error})"
            ));
            return report;
        }
    };
    if !recover_service_transaction(home, logs_root, &mut report.notices) {
        return report;
    }
    uninstall_log_service_locked(home, logs_root, report)
}

fn uninstall_log_service_locked(
    home: &Path,
    logs_root: &Path,
    mut report: LogServiceLifecycleReport,
) -> LogServiceLifecycleReport {
    let ownership_file = ownership_path(logs_root);
    let owned = match read_ownership(&ownership_file, home) {
        ManagedRead::Valid(owned) => owned,
        ManagedRead::Missing => return report,
        ManagedRead::Invalid(error) => {
            report.notices.push(format!(
                "preserved logging service files because ownership is invalid at {}: {error}",
                ownership_file.display()
            ));
            return report;
        }
    };
    report.plist = Some(owned.plist.clone());
    if owned.label != LOGS_LAUNCH_AGENT_LABEL || owned.plist != launch_agent_path(home) {
        report.notices.push(
            "preserved logging service files because their ownership record names an unexpected target"
                .to_owned(),
        );
        return report;
    }

    match fs::symlink_metadata(&owned.plist) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            report.notices.push(format!(
                "preserved logging service registration because its plist path is not a regular file: {}",
                owned.plist.display()
            ));
            return report;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            report.notices.push(format!(
                "could not inspect the owned logging LaunchAgent at {}: {error}",
                owned.plist.display()
            ));
            return report;
        }
    }
    match read_managed_bytes(&owned.plist, MAX_PLIST_BYTES) {
        ManagedRead::Valid(bytes) if sha256(&bytes) == owned.sha256 => {}
        ManagedRead::Valid(_) => {
            report.notices.push(format!(
                "preserved user-modified logging LaunchAgent: {}",
                owned.plist.display()
            ));
            return report;
        }
        ManagedRead::Missing => {}
        ManagedRead::Invalid(error) => {
            report.notices.push(format!(
                "could not inspect the owned logging LaunchAgent at {}: {error}",
                owned.plist.display()
            ));
            return report;
        }
    }
    if launchctl_enabled()
        && let Err(error) = unload_launch_agent(home)
    {
        report.notices.push(format!(
            "could not stop the logging collector within the bounded service timeout ({error}); it will stop at the next login"
        ));
    }
    match fs::remove_file(&owned.plist) {
        Ok(()) => report.changed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            report.notices.push(format!(
                "could not remove the owned logging LaunchAgent at {}: {error}",
                owned.plist.display()
            ));
            // Keep ownership so a later disable/uninstall can recover.
            return report;
        }
    }
    if let Err(error) = fs::remove_file(&ownership_file) {
        if error.kind() != std::io::ErrorKind::NotFound {
            report.notices.push(format!(
                "could not remove logging service ownership at {}: {error}",
                ownership_file.display()
            ));
        }
    }
    report
}

#[must_use]
pub fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LOGS_LAUNCH_AGENT_LABEL}.plist"))
}

#[must_use]
pub fn ownership_path(logs_root: &Path) -> PathBuf {
    logs_root.join("state/service-ownership.json")
}

fn render_logs_plist(executable: &Path, logs_root: &Path) -> Result<String, String> {
    let executable = executable
        .to_str()
        .ok_or_else(|| "logging service executable path is not UTF-8".to_owned())?;
    let logs_root = logs_root
        .to_str()
        .ok_or_else(|| "logging root path is not UTF-8".to_owned())?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{}</string>
	<key>ProgramArguments</key>
	<array><string>{}</string><string>logs</string><string>collect</string><string>--logs-root</string><string>{}</string></array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><true/>
	<key>ThrottleInterval</key><integer>10</integer>
	<key>ProcessType</key><string>Background</string>
	<key>StandardOutPath</key><string>/dev/null</string>
	<key>StandardErrorPath</key><string>/dev/null</string>
</dict>
</plist>
"#,
        LOGS_LAUNCH_AGENT_LABEL,
        escape_xml(executable),
        escape_xml(logs_root)
    ))
}

fn replace_launch_agent(home: &Path, plist: &Path) -> Result<(), String> {
    let domain = launchctl_domain(home)?;
    let _ = run_launchctl(&["bootout", &format!("{domain}/{LOGS_LAUNCH_AGENT_LABEL}")]);
    run_launchctl(&[
        "bootstrap",
        &domain,
        plist
            .to_str()
            .ok_or_else(|| "logging LaunchAgent path is not UTF-8".to_owned())?,
    ])
}

fn unload_launch_agent(home: &Path) -> Result<(), String> {
    let domain = launchctl_domain(home)?;
    match run_launchctl(&["bootout", &format!("{domain}/{LOGS_LAUNCH_AGENT_LABEL}")]) {
        Ok(()) => Ok(()),
        Err(error) if error.starts_with("exit 3") => Ok(()),
        Err(error) => Err(error),
    }
}

fn launchctl_domain(home: &Path) -> Result<String, String> {
    let uid = fs::metadata(home)
        .map_err(|error| format!("inspect home directory owner: {error}"))?
        .uid();
    Ok(format!("gui/{uid}"))
}

fn run_launchctl(arguments: &[&str]) -> Result<(), String> {
    let output = sctx_log_sync::runner::run(
        &sctx_log_sync::runner::CommandSpec::new("launchctl", LAUNCHCTL_TIMEOUT)
            .args(arguments.iter().copied())
            .output_limit(4096),
    )
    .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("exit {}", output.status.code().unwrap_or(-1)))
    }
}

fn launchctl_enabled() -> bool {
    if cfg!(test) {
        return false;
    }
    std::env::var_os(SKIP_LAUNCHCTL_ENV).is_none_or(|value| value.is_empty())
}

fn lifecycle_lock_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!(".{LOGS_LAUNCH_AGENT_LABEL}.lifecycle.lock"))
}

fn acquire_lifecycle_lock(home: &Path) -> LifecycleLock {
    let path = lifecycle_lock_path(home);
    let Some(parent) = path.parent() else {
        return LifecycleLock::Invalid("lifecycle lock has no parent".to_owned());
    };
    if let Err(error) = ensure_directory(parent, 0o755) {
        return LifecycleLock::Invalid(format!("prepare lifecycle lock directory: {error}"));
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            return LifecycleLock::Invalid(format!(
                "open lifecycle lock at {}: {error}",
                path.display()
            ));
        }
    };
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return LifecycleLock::Invalid(format!(
                "lifecycle lock is not a regular file: {}",
                path.display()
            ));
        }
        Err(error) => {
            return LifecycleLock::Invalid(format!("inspect lifecycle lock: {error}"));
        }
    }
    match file.try_lock_exclusive() {
        Ok(()) => LifecycleLock::Acquired(file),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => LifecycleLock::Busy,
        Err(error) => LifecycleLock::Invalid(format!("acquire lifecycle lock: {error}")),
    }
}

fn transaction_path(logs_root: &Path) -> PathBuf {
    logs_root.join("state/service-transaction.json")
}

fn commit_service_transaction(
    logs_root: &Path,
    plist: &Path,
    desired: &[u8],
    ownership: &ServiceOwnership,
    previous_plist: Option<Vec<u8>>,
    previous_ownership: Option<ServiceOwnership>,
) -> std::io::Result<()> {
    let state = logs_root.join("state");
    ensure_directory(&state, 0o700)?;
    ensure_directory(plist.parent().unwrap_or(logs_root), 0o755)?;
    let journal = ServiceTransaction {
        schema_version: 1,
        plist: plist.to_path_buf(),
        desired_sha256: ownership.sha256.clone(),
        previous_plist,
        previous_ownership,
    };
    validate_ownership(ownership, plist).map_err(std::io::Error::other)?;
    validate_transaction(&journal, plist).map_err(std::io::Error::other)?;
    if u64::try_from(desired.len()).unwrap_or(u64::MAX) > MAX_PLIST_BYTES
        || ownership.sha256 != sha256(desired)
    {
        return Err(std::io::Error::other(
            "desired logging LaunchAgent does not match its bounded ownership",
        ));
    }
    atomic_write(&transaction_path(logs_root), &json_bytes(&journal)?, 0o600)?;
    let result = atomic_write(plist, desired, 0o644)
        .and_then(|()| atomic_write(&ownership_path(logs_root), &json_bytes(ownership)?, 0o600));
    if let Err(error) = result {
        let mut notices = Vec::new();
        let _ = recover_service_transaction(
            plist
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .unwrap_or(logs_root),
            logs_root,
            &mut notices,
        );
        return Err(error);
    }
    fs::remove_file(transaction_path(logs_root))?;
    sync_directory(&state)
}

fn recover_service_transaction(home: &Path, logs_root: &Path, notices: &mut Vec<String>) -> bool {
    let path = transaction_path(logs_root);
    let transaction = match read_transaction(&path, home) {
        ManagedRead::Missing => return true,
        ManagedRead::Valid(transaction) => transaction,
        ManagedRead::Invalid(error) => {
            notices.push(format!(
                "preserved logging service recovery journal because it is invalid at {}: {error}",
                path.display()
            ));
            return false;
        }
    };
    let current_hash = match read_managed_bytes(&transaction.plist, MAX_PLIST_BYTES) {
        ManagedRead::Missing => None,
        ManagedRead::Valid(bytes) => Some(sha256(&bytes)),
        ManagedRead::Invalid(error) => {
            notices.push(format!(
                "preserved logging service recovery state because the LaunchAgent cannot be safely read: {error}"
            ));
            return false;
        }
    };
    let installed_ownership = match read_ownership(&ownership_path(logs_root), home) {
        ManagedRead::Missing => None,
        ManagedRead::Valid(ownership) => Some(ownership),
        ManagedRead::Invalid(error) => {
            notices.push(format!(
                "preserved logging service recovery state because installed ownership is invalid: {error}"
            ));
            return false;
        }
    };
    let committed = current_hash.as_deref() == Some(transaction.desired_sha256.as_str())
        && installed_ownership
            .as_ref()
            .is_some_and(|owned| owned.sha256 == transaction.desired_sha256);
    let previous_hash = transaction
        .previous_plist
        .as_ref()
        .map(|bytes| sha256(bytes));
    let still_previous = current_hash == previous_hash && current_hash.is_some();
    if !committed
        && (current_hash.as_deref() == Some(transaction.desired_sha256.as_str())
            || still_previous
            || current_hash.is_none())
    {
        let plist_result = match transaction.previous_plist {
            Some(previous) => atomic_write(&transaction.plist, &previous, 0o644),
            None => match fs::remove_file(&transaction.plist) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        let ownership_result = match transaction.previous_ownership {
            Some(previous) => json_bytes(&previous)
                .and_then(|bytes| atomic_write(&ownership_path(logs_root), &bytes, 0o600)),
            None => match fs::remove_file(ownership_path(logs_root)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = plist_result.and(ownership_result) {
            notices.push(format!(
                "could not roll back interrupted logging service transaction: {error}"
            ));
            return false;
        }
    } else if !committed && current_hash.is_some() {
        notices.push(
            "preserved logging service recovery journal because the plist changed after the interrupted transaction"
                .to_owned(),
        );
        return false;
    }
    if let Err(error) = fs::remove_file(&path) {
        notices.push(format!(
            "could not clear recovered logging service transaction at {}: {error}",
            path.display()
        ));
        return false;
    }
    true
}

fn json_bytes(value: &impl Serialize) -> std::io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_ownership(path: &Path, home: &Path) -> ManagedRead<ServiceOwnership> {
    let bytes = match read_managed_bytes(path, MAX_OWNERSHIP_BYTES) {
        ManagedRead::Missing => return ManagedRead::Missing,
        ManagedRead::Valid(bytes) => bytes,
        ManagedRead::Invalid(error) => return ManagedRead::Invalid(error),
    };
    let ownership = match serde_json::from_slice::<ServiceOwnership>(&bytes) {
        Ok(ownership) => ownership,
        Err(error) => return ManagedRead::Invalid(format!("invalid ownership JSON: {error}")),
    };
    match validate_ownership(&ownership, &launch_agent_path(home)) {
        Ok(()) => ManagedRead::Valid(ownership),
        Err(error) => ManagedRead::Invalid(error),
    }
}

fn read_transaction(path: &Path, home: &Path) -> ManagedRead<ServiceTransaction> {
    let bytes = match read_managed_bytes(path, MAX_TRANSACTION_BYTES) {
        ManagedRead::Missing => return ManagedRead::Missing,
        ManagedRead::Valid(bytes) => bytes,
        ManagedRead::Invalid(error) => return ManagedRead::Invalid(error),
    };
    let transaction = match serde_json::from_slice::<ServiceTransaction>(&bytes) {
        Ok(transaction) => transaction,
        Err(error) => return ManagedRead::Invalid(format!("invalid transaction JSON: {error}")),
    };
    match validate_transaction(&transaction, &launch_agent_path(home)) {
        Ok(()) => ManagedRead::Valid(transaction),
        Err(error) => ManagedRead::Invalid(error),
    }
}

fn read_managed_bytes(path: &Path, limit: u64) -> ManagedRead<Vec<u8>> {
    match sctx_log_service::read_bounded_file(path, limit) {
        Ok(bytes) => ManagedRead::Valid(bytes),
        Err(_)
            if path
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            ManagedRead::Missing
        }
        Err(error) => ManagedRead::Invalid(error.to_string()),
    }
}

fn validate_ownership(ownership: &ServiceOwnership, expected_plist: &Path) -> Result<(), String> {
    if ownership.schema_version != 1 {
        return Err("unsupported ownership schema_version".to_owned());
    }
    if ownership.label != LOGS_LAUNCH_AGENT_LABEL || ownership.plist != expected_plist {
        return Err("ownership names an unexpected label or plist target".to_owned());
    }
    if !is_sha256(&ownership.sha256) {
        return Err("ownership SHA-256 is invalid".to_owned());
    }
    Ok(())
}

fn validate_transaction(
    transaction: &ServiceTransaction,
    expected_plist: &Path,
) -> Result<(), String> {
    if transaction.schema_version != 1 {
        return Err("unsupported transaction schema_version".to_owned());
    }
    if transaction.plist != expected_plist || !is_sha256(&transaction.desired_sha256) {
        return Err("transaction names an unexpected target or invalid desired SHA-256".to_owned());
    }
    if transaction.previous_plist.is_some() != transaction.previous_ownership.is_some() {
        return Err("transaction previous plist and ownership must be present together".to_owned());
    }
    if let (Some(previous_plist), Some(previous_ownership)) = (
        transaction.previous_plist.as_ref(),
        transaction.previous_ownership.as_ref(),
    ) {
        if u64::try_from(previous_plist.len()).unwrap_or(u64::MAX) > MAX_PLIST_BYTES {
            return Err("transaction previous plist exceeds its bounded size".to_owned());
        }
        validate_ownership(previous_ownership, expected_plist)?;
        if previous_ownership.sha256 != sha256(previous_plist) {
            return Err("transaction previous ownership hash does not match its plist".to_owned());
        }
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn ensure_directory(path: &Path, mode: u32) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "directory path is not a real directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn remove_file_and_sync_parent(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("managed file has no parent"))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sctx"),
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true).mode(mode);
    let result = (|| {
        use std::io::Write as _;
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(std::io::Error::other(
            "directory sync target is not a directory",
        ));
    }
    directory.sync_all()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        sync::{Arc, Barrier},
        thread,
        time::Instant,
    };

    use super::*;

    fn initialized_fixture(
        temporary: &tempfile::TempDir,
        name: &str,
    ) -> (PathBuf, PathBuf, PathBuf) {
        let home = temporary.path().join("home");
        let logs_root = temporary.path().join(name);
        let executable = home.join(".shared-context/bin/current/sctx");
        ensure_directory(&home, 0o700).unwrap();
        ensure_directory(executable.parent().unwrap(), 0o700).unwrap();
        fs::write(&executable, b"fixture runtime").unwrap();
        sctx_log_service::init(
            &logs_root,
            sctx_log_service::InitOptions {
                email: None,
                remote: None,
                installation_id: None,
                enabled: true,
            },
        )
        .unwrap();
        (home, logs_root, executable)
    }

    fn write_ownership(logs_root: &Path, ownership: &ServiceOwnership) {
        atomic_write(
            &ownership_path(logs_root),
            &json_bytes(ownership).unwrap(),
            0o600,
        )
        .unwrap();
    }

    #[test]
    fn interrupted_plist_write_rolls_back_before_service_activation() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let logs_root = temporary.path().join("logs");
        let plist = launch_agent_path(&home);
        ensure_directory(plist.parent().unwrap(), 0o755).unwrap();
        ensure_directory(&logs_root.join("state"), 0o700).unwrap();
        let old = b"old managed plist".to_vec();
        let desired = b"new managed plist".to_vec();
        fs::write(&plist, &old).unwrap();
        let prior = ServiceOwnership {
            schema_version: 1,
            label: LOGS_LAUNCH_AGENT_LABEL.to_owned(),
            plist: plist.clone(),
            sha256: sha256(&old),
        };
        atomic_write(
            &ownership_path(&logs_root),
            &json_bytes(&prior).unwrap(),
            0o600,
        )
        .unwrap();
        let transaction = ServiceTransaction {
            schema_version: 1,
            plist: plist.clone(),
            desired_sha256: sha256(&desired),
            previous_plist: Some(old.clone()),
            previous_ownership: Some(prior.clone()),
        };
        atomic_write(
            &transaction_path(&logs_root),
            &json_bytes(&transaction).unwrap(),
            0o600,
        )
        .unwrap();
        fs::write(&plist, desired).unwrap();

        let mut notices = Vec::new();
        assert!(recover_service_transaction(&home, &logs_root, &mut notices));
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(fs::read(&plist).unwrap(), old);
        assert_eq!(
            serde_json::from_slice::<ServiceOwnership>(
                &fs::read(ownership_path(&logs_root)).unwrap()
            )
            .unwrap(),
            prior
        );
        assert!(!transaction_path(&logs_root).exists());
    }

    #[test]
    fn modified_owned_plist_is_preserved_without_stopping_its_label() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let logs_root = temporary.path().join("logs");
        let plist = launch_agent_path(&home);
        ensure_directory(plist.parent().unwrap(), 0o755).unwrap();
        ensure_directory(&logs_root.join("state"), 0o700).unwrap();
        let installed = b"installer bytes";
        fs::write(&plist, b"user modified bytes").unwrap();
        let ownership = ServiceOwnership {
            schema_version: 1,
            label: LOGS_LAUNCH_AGENT_LABEL.to_owned(),
            plist: plist.clone(),
            sha256: sha256(installed),
        };
        atomic_write(
            &ownership_path(&logs_root),
            &json_bytes(&ownership).unwrap(),
            0o600,
        )
        .unwrap();

        let report = uninstall_log_service(&home, &logs_root);
        assert!(!report.changed);
        assert!(plist.exists());
        assert!(ownership_path(&logs_root).exists());
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("user-modified"))
        );
    }

    #[test]
    fn fifo_and_oversized_lifecycle_state_are_rejected_without_touching_the_plist() {
        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "fifo lock");
        let lock = lifecycle_lock_path(&home);
        ensure_directory(lock.parent().unwrap(), 0o755).unwrap();
        let status = Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&lock)
            .status()
            .unwrap();
        assert!(status.success());
        let started = Instant::now();
        let report = reconcile_log_service(&home, &logs_root, &executable);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!report.changed);
        assert!(report.notices.iter().any(|notice| notice.contains("lock")));

        for (name, target) in [
            ("fifo ownership", "ownership"),
            ("fifo transaction", "transaction"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let (home, logs_root, executable) = initialized_fixture(&temporary, name);
            let path = if target == "ownership" {
                ownership_path(&logs_root)
            } else {
                transaction_path(&logs_root)
            };
            let status = Command::new("mkfifo")
                .args(["-m", "600"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            let started = Instant::now();
            let report = reconcile_log_service(&home, &logs_root, &executable);
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(!report.changed);
            assert!(!launch_agent_path(&home).exists());
            assert!(
                report
                    .notices
                    .iter()
                    .any(|notice| notice.contains("invalid"))
            );
        }

        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "oversized ownership");
        fs::write(
            ownership_path(&logs_root),
            vec![b'x'; usize::try_from(MAX_OWNERSHIP_BYTES + 1).unwrap()],
        )
        .unwrap();
        let report = reconcile_log_service(&home, &logs_root, &executable);
        assert!(!report.changed);
        assert!(!launch_agent_path(&home).exists());

        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "oversized plist");
        let plist = launch_agent_path(&home);
        ensure_directory(plist.parent().unwrap(), 0o755).unwrap();
        let oversized = vec![b'p'; usize::try_from(MAX_PLIST_BYTES + 1).unwrap()];
        fs::write(&plist, &oversized).unwrap();
        write_ownership(
            &logs_root,
            &ServiceOwnership {
                schema_version: 1,
                label: LOGS_LAUNCH_AGENT_LABEL.to_owned(),
                plist: plist.clone(),
                sha256: sha256(&oversized),
            },
        );
        let report = reconcile_log_service(&home, &logs_root, &executable);
        assert!(!report.changed);
        assert_eq!(fs::metadata(&plist).unwrap().len(), MAX_PLIST_BYTES + 1);
    }

    #[test]
    fn unknown_ownership_schema_cannot_replace_or_remove_the_plist() {
        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "unknown ownership");
        let plist = launch_agent_path(&home);
        ensure_directory(plist.parent().unwrap(), 0o755).unwrap();
        let bytes = b"existing plist";
        fs::write(&plist, bytes).unwrap();
        write_ownership(
            &logs_root,
            &ServiceOwnership {
                schema_version: 99,
                label: LOGS_LAUNCH_AGENT_LABEL.to_owned(),
                plist: plist.clone(),
                sha256: sha256(bytes),
            },
        );

        let reconcile = reconcile_log_service(&home, &logs_root, &executable);
        assert!(!reconcile.changed);
        assert_eq!(fs::read(&plist).unwrap(), bytes);
        let uninstall = uninstall_log_service(&home, &logs_root);
        assert!(!uninstall.changed);
        assert_eq!(fs::read(&plist).unwrap(), bytes);
        assert!(ownership_path(&logs_root).exists());
    }

    #[test]
    fn invalid_nested_transaction_snapshot_stops_lifecycle_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "invalid transaction");
        let plist = launch_agent_path(&home);
        ensure_directory(plist.parent().unwrap(), 0o755).unwrap();
        let previous = b"previous plist".to_vec();
        fs::write(&plist, &previous).unwrap();
        let ownership = ServiceOwnership {
            schema_version: 1,
            label: LOGS_LAUNCH_AGENT_LABEL.to_owned(),
            plist: plist.clone(),
            sha256: sha256(&previous),
        };
        write_ownership(&logs_root, &ownership);
        let invalid = ServiceTransaction {
            schema_version: 1,
            plist: plist.clone(),
            desired_sha256: sha256(b"desired"),
            previous_plist: Some(previous.clone()),
            previous_ownership: Some(ServiceOwnership {
                sha256: sha256(b"different bytes"),
                ..ownership.clone()
            }),
        };
        atomic_write(
            &transaction_path(&logs_root),
            &json_bytes(&invalid).unwrap(),
            0o600,
        )
        .unwrap();

        let report = reconcile_log_service(&home, &logs_root, &executable);
        assert!(!report.changed);
        assert_eq!(fs::read(&plist).unwrap(), previous);
        assert!(transaction_path(&logs_root).exists());
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("invalid"))
        );
    }

    #[test]
    fn missing_owned_plist_is_reinstalled_as_a_fresh_recoverable_transaction() {
        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "missing plist");
        let first = reconcile_log_service(&home, &logs_root, &executable);
        assert!(first.changed);
        let plist = launch_agent_path(&home);
        fs::remove_file(&plist).unwrap();

        let second = reconcile_log_service(&home, &logs_root, &executable);
        assert!(second.changed, "{:?}", second.notices);
        assert!(plist.is_file());
        assert!(matches!(
            read_ownership(&ownership_path(&logs_root), &home),
            ManagedRead::Valid(_)
        ));
        assert!(!transaction_path(&logs_root).exists());

        // Crash after the fresh transaction installs the plist but before it publishes ownership:
        // recovery removes the unowned plist and keeps no ambiguous half-state.
        let desired = fs::read(&plist).unwrap();
        fs::remove_file(ownership_path(&logs_root)).unwrap();
        atomic_write(
            &transaction_path(&logs_root),
            &json_bytes(&ServiceTransaction {
                schema_version: 1,
                plist: plist.clone(),
                desired_sha256: sha256(&desired),
                previous_plist: None,
                previous_ownership: None,
            })
            .unwrap(),
            0o600,
        )
        .unwrap();
        let mut notices = Vec::new();
        assert!(recover_service_transaction(&home, &logs_root, &mut notices));
        assert!(!plist.exists());
        assert!(!transaction_path(&logs_root).exists());
    }

    #[test]
    fn two_logs_roots_cannot_both_claim_the_one_user_launch_agent() {
        let temporary = tempfile::tempdir().unwrap();
        let (home, first_root, executable) = initialized_fixture(&temporary, "logs one");
        let (_, second_root, _) = initialized_fixture(&temporary, "logs two");
        let barrier = Arc::new(Barrier::new(3));
        let handles = [first_root.clone(), second_root.clone()].map(|logs_root| {
            let home = home.clone();
            let executable = executable.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                reconcile_log_service(&home, &logs_root, &executable)
            })
        });
        barrier.wait();
        let reports = handles.map(|handle| handle.join().unwrap());

        let first_owned = matches!(
            read_ownership(&ownership_path(&first_root), &home),
            ManagedRead::Valid(_)
        );
        let second_owned = matches!(
            read_ownership(&ownership_path(&second_root), &home),
            ManagedRead::Valid(_)
        );
        assert_ne!(first_owned, second_owned);
        assert!(launch_agent_path(&home).is_file());
        assert_eq!(reports.iter().filter(|report| report.changed).count(), 1);
    }

    #[test]
    fn busy_user_label_lock_is_a_non_mutating_notice() {
        let temporary = tempfile::tempdir().unwrap();
        let (home, logs_root, executable) = initialized_fixture(&temporary, "busy lifecycle");
        let LifecycleLock::Acquired(_guard) = acquire_lifecycle_lock(&home) else {
            panic!("fixture lifecycle lock");
        };
        let report = reconcile_log_service(&home, &logs_root, &executable);
        assert!(!report.changed);
        assert!(!launch_agent_path(&home).exists());
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("already active"))
        );
    }
}
