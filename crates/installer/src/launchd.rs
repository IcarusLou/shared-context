//! The scheduled half of maintenance: one user `LaunchAgent` that runs `sctx maintain run` daily.
//!
//! [`maintain`](crate::maintain) is the work; this module is the only thing that makes it happen
//! on an installation nobody thought about. The opportunistic half lives in the `SessionStart`
//! Hook and covers a laptop that was asleep at the scheduled hour; the two are independent on
//! purpose, because each one alone leaves a real installation uncovered.
//!
//! ## Why the file and the registration are separated
//!
//! Writing the plist is a filesystem change setup can roll back like any other; `launchctl` is not.
//! Bootstrapping a job into the caller's GUI domain touches state that belongs to launchd, that a
//! rollback cannot honestly undo, and that fails for reasons having nothing to do with this
//! installation -- an SSH session with no GUI domain, a managed machine, a user who has not logged
//! in yet. So the plist is written *inside* the setup transaction, where it is exactly as
//! recoverable as the Agent configuration beside it, and `launchctl` runs *after* the transaction
//! commits, where its failure is a notice telling the operator to log in again rather than a
//! rolled-back installation.
//!
//! ## Ownership
//!
//! Setup never overwrites a `com.shared-context.maintain.plist` it did not write. The manifest
//! records the exact bytes installed, and a file whose digest no longer matches is a file the
//! operator edited: preserved, reported, and left registered exactly as they left it. This is the
//! same rule the managed Agent configuration and the global Agent Skill follow.

use std::path::{Path, PathBuf};

use sctx_local_state::MaintenanceSettings;
use serde::{Deserialize, Serialize};

use crate::{
    Result, Transaction, absent_parent_directories, atomic_write, invalid, io_error, path_text,
    sha256,
};

/// launchd job label, and the plist basename derived from it. Stable forever: it is the name
/// `launchctl bootout` needs to find a job an older version installed.
pub const MAINTAIN_LAUNCH_AGENT_LABEL: &str = "com.shared-context.maintain";

/// Where a user `LaunchAgent` lives on macOS.
#[must_use]
pub fn launch_agents_directory(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
}

/// Absolute path of the maintenance `LaunchAgent` plist for one home directory.
#[must_use]
pub fn launch_agent_path(home: &Path) -> PathBuf {
    launch_agents_directory(home).join(format!("{MAINTAIN_LAUNCH_AGENT_LABEL}.plist"))
}

/// The single log both streams of a scheduled run are appended to.
///
/// One file rather than two: a run's stderr diagnosis is only readable next to the `--json` digest
/// line it belongs to, and launchd appends rather than truncating, so interleaving them keeps the
/// order the run produced.
#[must_use]
pub fn launch_agent_log_path(root: &Path) -> PathBuf {
    root.join("logs/maintain-launchd.log")
}

/// Renders the complete plist for one installation root and schedule.
///
/// The program is `<root>/bin/current/sctx` -- the version-stable symlink, never the versioned
/// path underneath it, so an upgrade that moves `bin/current` needs no plist rewrite at all.
///
/// # Errors
///
/// Returns [`crate::ErrorKind::InvalidInput`] for a non-UTF-8 path or an out-of-range time.
pub fn render_maintain_plist(root: &Path, hour: u32, minute: u32) -> Result<String> {
    if hour > 23 || minute > 59 {
        return Err(invalid(format!(
            "maintenance schedule {hour:02}:{minute:02} is not a time of day"
        )));
    }
    let program = escape_xml(&path_text(&root.join("bin/current/sctx"))?);
    let log = escape_xml(&path_text(&launch_agent_log_path(root))?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{MAINTAIN_LAUNCH_AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
		<string>maintain</string>
		<string>run</string>
		<string>--json</string>
	</array>
	<key>RunAtLoad</key>
	<false/>
	<key>StartCalendarInterval</key>
	<dict>
		<key>Hour</key>
		<integer>{hour}</integer>
		<key>Minute</key>
		<integer>{minute}</integer>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>ProcessType</key>
	<string>Background</string>
</dict>
</plist>
"#
    ))
}

/// Escapes the five XML metacharacters. Home directories contain ampersands more often than
/// anyone expects, and a plist launchd cannot parse is a job that silently never runs.
fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// The maintenance `LaunchAgent` this installation wrote, recorded in the install manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct OwnedLaunchAgent {
    pub(crate) path: PathBuf,
    pub(crate) label: String,
    pub(crate) sha256: String,
}

/// What setup should do about the `LaunchAgent` on this host, decided before touching anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LaunchAgentPlan {
    Install {
        hour: u32,
        minute: u32,
    },
    /// `scheduled = false`: remove an agent this installation previously installed.
    Remove,
    /// Nothing to do here, with the sentence explaining why.
    Unsupported(String),
}

/// Pure planning: platform and configuration only, no filesystem, no launchd.
pub(crate) fn plan_launch_agent(platform: &str, settings: &MaintenanceSettings) -> LaunchAgentPlan {
    if platform != "macos" {
        return LaunchAgentPlan::Unsupported(format!(
            "scheduled maintenance was skipped because launchd user agents are a macOS facility \
             and this host reports {platform}; run `sctx maintain run` from your own scheduler."
        ));
    }
    if !settings.scheduled {
        return LaunchAgentPlan::Remove;
    }
    LaunchAgentPlan::Install {
        hour: settings.schedule_hour,
        minute: settings.schedule_minute,
    }
}

/// The registration step, which runs only after the setup transaction commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LaunchAgentActivation {
    Load { plist: PathBuf },
    Unload,
}

/// Result of the transactional half of the `LaunchAgent` step.
#[derive(Debug, Default)]
pub(crate) struct LaunchAgentInstall {
    pub(crate) changed: bool,
    /// Ownership to record in the manifest. `None` means this installation owns no agent, either
    /// because it removed its own or because it declined to claim someone else's file.
    pub(crate) ownership: Option<OwnedLaunchAgent>,
    pub(crate) activation: Option<LaunchAgentActivation>,
}

/// Writes, refreshes, or removes the maintenance `LaunchAgent` inside the setup transaction.
///
/// Every ending that is not "this installation's own file" preserves what is on disk and explains
/// itself in `notices`; none of them fails setup. A daily timer is a convenience, and refusing to
/// install a machine because a stale plist is in the way would be the wrong trade.
pub(crate) fn install_launch_agent(
    transaction: &mut Transaction,
    home: &Path,
    root: &Path,
    plan: &LaunchAgentPlan,
    prior: Option<&OwnedLaunchAgent>,
    notices: &mut Vec<String>,
) -> Result<LaunchAgentInstall> {
    let path = launch_agent_path(home);
    let (hour, minute) = match plan {
        LaunchAgentPlan::Unsupported(reason) => {
            notices.push(reason.clone());
            // Ownership is carried forward untouched: a plist written on a previous, supported run
            // is still that run's to remove, and this host simply cannot speak to launchd about it.
            return Ok(LaunchAgentInstall {
                changed: false,
                ownership: prior.cloned(),
                activation: None,
            });
        }
        LaunchAgentPlan::Remove => return remove_launch_agent(transaction, &path, prior, notices),
        LaunchAgentPlan::Install { hour, minute } => (*hour, *minute),
    };

    let desired = render_maintain_plist(root, hour, minute)?;
    let desired = desired.as_bytes();
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let current = std::fs::read(&path).map_err(io_error("read maintenance LaunchAgent"))?;
            let digest = sha256(&current);
            match prior {
                Some(owned) if owned.sha256 == digest => {}
                Some(_) => {
                    notices.push(format!(
                        "preserved user-modified scheduled maintenance LaunchAgent: {}. Delete it \
                         and run `sctx setup` again to restore the managed schedule.",
                        path.display()
                    ));
                    return Ok(LaunchAgentInstall {
                        changed: false,
                        ownership: prior.cloned(),
                        activation: None,
                    });
                }
                None => {
                    notices.push(format!(
                        "preserved user-owned launchd job at {}; Shared Context did not overwrite \
                         or claim it, so scheduled maintenance was not installed.",
                        path.display()
                    ));
                    return Ok(LaunchAgentInstall::default());
                }
            }
            if current == desired {
                // Byte-identical: nothing to write, and nothing to re-register either. An agent
                // already bootstrapped from these exact bytes is already running the right job.
                return Ok(LaunchAgentInstall {
                    changed: false,
                    ownership: prior.cloned(),
                    activation: None,
                });
            }
            transaction.record(&path)?;
            atomic_write(&path, desired, 0o644)?;
            transaction.phase("launch_agent_updated")?;
        }
        Ok(_) => {
            notices.push(format!(
                "preserved the scheduled maintenance LaunchAgent path because it is not a regular \
                 file: {}",
                path.display()
            ));
            return Ok(LaunchAgentInstall {
                changed: false,
                ownership: prior.cloned(),
                activation: None,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if prior.is_some() {
                notices.push(format!(
                    "reinstalled the scheduled maintenance LaunchAgent, which had been removed: {}",
                    path.display()
                ));
            }
            // `~/Library/LaunchAgents` may not exist yet, and a rollback owes the home directory
            // the same "leave nothing behind" the global Agent Skill install already honors.
            let cleanup_empty_dirs = absent_parent_directories(&path, home)?;
            transaction.record_with_cleanup(&path, cleanup_empty_dirs)?;
            atomic_write(&path, desired, 0o644)?;
            transaction.phase("launch_agent_installed")?;
        }
        Err(error) => return Err(io_error("inspect maintenance LaunchAgent")(error)),
    }

    Ok(LaunchAgentInstall {
        changed: true,
        ownership: Some(OwnedLaunchAgent {
            path: path.clone(),
            label: MAINTAIN_LAUNCH_AGENT_LABEL.to_owned(),
            sha256: sha256(desired),
        }),
        activation: Some(LaunchAgentActivation::Load { plist: path }),
    })
}

fn remove_launch_agent(
    transaction: &mut Transaction,
    path: &Path,
    prior: Option<&OwnedLaunchAgent>,
    notices: &mut Vec<String>,
) -> Result<LaunchAgentInstall> {
    let Some(owned) = prior else {
        // Never installed one, or already gave it up. `scheduled = false` on a fresh installation
        // is not an event worth a notice.
        return Ok(LaunchAgentInstall::default());
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let current = std::fs::read(path).map_err(io_error("read maintenance LaunchAgent"))?;
            if sha256(&current) != owned.sha256 {
                notices.push(format!(
                    "preserved user-modified scheduled maintenance LaunchAgent even though \
                     [maintenance] scheduled is false: {}. Remove it by hand to stop the timer.",
                    path.display()
                ));
                return Ok(LaunchAgentInstall {
                    changed: false,
                    ownership: prior.cloned(),
                    activation: None,
                });
            }
            transaction.record(path)?;
            std::fs::remove_file(path).map_err(io_error("remove maintenance LaunchAgent"))?;
            transaction.phase("launch_agent_removed")?;
            notices.push(format!(
                "removed the scheduled maintenance LaunchAgent because [maintenance] scheduled is \
                 false: {}",
                path.display()
            ));
            Ok(LaunchAgentInstall {
                changed: true,
                ownership: None,
                activation: Some(LaunchAgentActivation::Unload),
            })
        }
        Ok(_) => {
            notices.push(format!(
                "preserved the scheduled maintenance LaunchAgent path because it is not a regular \
                 file: {}",
                path.display()
            ));
            Ok(LaunchAgentInstall {
                changed: false,
                ownership: prior.cloned(),
                activation: None,
            })
        }
        // The file is already gone; the job may still be registered from a previous login, so the
        // bootout is still worth attempting.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LaunchAgentInstall {
            changed: false,
            ownership: None,
            activation: Some(LaunchAgentActivation::Unload),
        }),
        Err(error) => Err(io_error("inspect maintenance LaunchAgent")(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> MaintenanceSettings {
        MaintenanceSettings::default()
    }

    #[test]
    fn the_default_plan_installs_the_daily_agent_on_macos() {
        assert_eq!(
            plan_launch_agent("macos", &settings()),
            LaunchAgentPlan::Install { hour: 6, minute: 0 }
        );
    }

    #[test]
    fn a_disabled_schedule_plans_removal_and_a_foreign_platform_plans_nothing() {
        let disabled = MaintenanceSettings {
            scheduled: false,
            ..settings()
        };
        assert_eq!(
            plan_launch_agent("macos", &disabled),
            LaunchAgentPlan::Remove
        );
        // The platform decides before the switch does: a Linux host has no launchd job to remove
        // either, so `scheduled = false` there is still Unsupported rather than Remove.
        let LaunchAgentPlan::Unsupported(reason) = plan_launch_agent("linux", &disabled) else {
            panic!("a non-macOS host must not plan launchd work");
        };
        assert!(reason.contains("linux"), "{reason}");
        assert!(matches!(
            plan_launch_agent("linux", &settings()),
            LaunchAgentPlan::Unsupported(_)
        ));
    }

    #[test]
    fn a_rendered_plist_escapes_xml_and_refuses_an_impossible_time() {
        let root = Path::new("/tmp/a & b/.shared-context");
        let plist = render_maintain_plist(root, 6, 0).unwrap();
        assert!(plist.contains("<string>/tmp/a &amp; b/.shared-context/bin/current/sctx</string>"));
        assert!(!plist.contains("a & b"));
        assert!(render_maintain_plist(Path::new("/root"), 24, 0).is_err());
        assert!(render_maintain_plist(Path::new("/root"), 6, 60).is_err());
    }
}
