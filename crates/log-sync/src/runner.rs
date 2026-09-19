use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{self, Read},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const RESOURCE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_GUARD_ENTRIES: usize = 100_000;

#[derive(Clone, Debug)]
pub struct DiskGuard {
    pub root: PathBuf,
    pub max_bytes: u64,
    pub min_free_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub clear_env: bool,
    pub deadline: Instant,
    pub output_limit: usize,
    pub disk_guard: Option<DiskGuard>,
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: BTreeMap::new(),
            clear_env: false,
            deadline: Instant::now() + timeout,
            output_limit: 64 * 1024,
            disk_guard: None,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub const fn clear_env(mut self, clear: bool) -> Self {
        self.clear_env = clear;
        self
    }

    #[must_use]
    pub const fn output_limit(mut self, limit: usize) -> Self {
        self.output_limit = limit;
        self
    }

    #[must_use]
    pub const fn deadline(mut self, deadline: Instant) -> Self {
        self.deadline = deadline;
        self
    }

    #[must_use]
    pub fn disk_guard(mut self, guard: DiskGuard) -> Self {
        self.disk_guard = Some(guard);
        self
    }
}

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug)]
pub enum RunnerError {
    Spawn(io::Error),
    Wait(io::Error),
    Output(io::Error),
    TimedOut { stdout: Vec<u8>, stderr: Vec<u8> },
    ResourceLimit { stdout: Vec<u8>, stderr: Vec<u8> },
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "process could not start: {error}"),
            Self::Wait(error) => write!(formatter, "process status failed: {error}"),
            Self::Output(error) => write!(formatter, "process output failed: {error}"),
            Self::TimedOut { .. } => formatter.write_str("process exceeded its deadline"),
            Self::ResourceLimit { .. } => {
                formatter.write_str("process exceeded its disk resource limit")
            }
        }
    }
}

impl std::error::Error for RunnerError {}

/// Runs one command with bounded output in an isolated process group.
///
/// # Errors
///
/// Returns an error when spawning, waiting, or reading output fails, or when the shared deadline
/// expires. A timeout terminates the entire process group before returning.
#[allow(clippy::too_many_lines)]
pub fn run(spec: &CommandSpec) -> Result<CommandOutput, RunnerError> {
    if Instant::now() >= spec.deadline {
        return Err(RunnerError::TimedOut {
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }
    if let Some(guard) = &spec.disk_guard {
        match check_guard(guard, spec.deadline) {
            GuardCheck::Within => {}
            GuardCheck::Exceeded => {
                return Err(RunnerError::ResourceLimit {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            GuardCheck::Deadline => {
                return Err(RunnerError::TimedOut {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
        }
    }
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(directory) = &spec.current_dir {
        command.current_dir(directory);
    }
    if spec.clear_env {
        command.env_clear();
    }
    command.envs(&spec.env);

    let mut child = command.spawn().map_err(RunnerError::Spawn)?;
    let process_group = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
    let Some(stdout) = child.stdout.take() else {
        terminate_group(process_group, &mut child);
        return Err(RunnerError::Output(io::Error::other(
            "piped stdout is unavailable",
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_group(process_group, &mut child);
        return Err(RunnerError::Output(io::Error::other(
            "piped stderr is unavailable",
        )));
    };
    let stdout_reader = spawn_reader(stdout, spec.output_limit);
    let stderr_reader = spawn_reader(stderr, spec.output_limit);
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let mut last_resource_check = Instant::now();
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => {
                    terminate_group(process_group, &mut child);
                    return Err(RunnerError::Wait(error));
                }
            }
        }
        if let Err(error) = poll_reader(&stdout_reader, &mut stdout)
            .and_then(|()| poll_reader(&stderr_reader, &mut stderr))
        {
            terminate_group(process_group, &mut child);
            return Err(error);
        }
        if let (Some(status), Some(stdout), Some(stderr)) =
            (status.as_ref(), stdout.as_ref(), stderr.as_ref())
        {
            return Ok(CommandOutput {
                status: *status,
                stdout: stdout.bytes.clone(),
                stderr: stderr.bytes.clone(),
                stdout_truncated: stdout.truncated,
                stderr_truncated: stderr.truncated,
            });
        }
        if last_resource_check.elapsed() >= RESOURCE_POLL_INTERVAL {
            last_resource_check = Instant::now();
            if let Some(guard) = &spec.disk_guard {
                let check = check_guard(guard, spec.deadline);
                if check != GuardCheck::Within {
                    terminate_group(process_group, &mut child);
                    let stdout = stdout.map_or_else(Vec::new, |output| output.bytes);
                    let stderr = stderr.map_or_else(Vec::new, |output| output.bytes);
                    return if check == GuardCheck::Deadline {
                        Err(RunnerError::TimedOut { stdout, stderr })
                    } else {
                        Err(RunnerError::ResourceLimit { stdout, stderr })
                    };
                }
            }
        }
        if Instant::now() >= spec.deadline {
            let _ = killpg(process_group, Signal::SIGTERM);
            let grace_deadline = Instant::now() + TERMINATION_GRACE;
            while Instant::now() < grace_deadline {
                if status.is_none() {
                    status = child.try_wait().unwrap_or(None);
                }
                let _ = poll_reader(&stdout_reader, &mut stdout);
                let _ = poll_reader(&stderr_reader, &mut stderr);
                if status.is_some() && stdout.is_some() && stderr.is_some() {
                    break;
                }
                thread::sleep(POLL_INTERVAL);
            }
            // Always kill the group when any part of the child pipeline outlives the deadline.
            // The direct child may already be reaped while a descendant still owns a pipe.
            let _ = killpg(process_group, Signal::SIGKILL);
            if status.is_none() {
                let _ = child.wait();
            }
            let stdout = stdout.map_or_else(Vec::new, |output| output.bytes);
            let stderr = stderr.map_or_else(Vec::new, |output| output.bytes);
            return Err(RunnerError::TimedOut { stdout, stderr });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardCheck {
    Within,
    Exceeded,
    Deadline,
}

fn check_guard(guard: &DiskGuard, deadline: Instant) -> GuardCheck {
    let used = match directory_size(&guard.root, deadline) {
        Ok(Some(used)) => used,
        Ok(None) if Instant::now() >= deadline => return GuardCheck::Deadline,
        Ok(None) | Err(_) => return GuardCheck::Exceeded,
    };
    if Instant::now() >= deadline {
        return GuardCheck::Deadline;
    }
    if used > guard.max_bytes
        || fs2::available_space(&guard.root).map_or(true, |free| free < guard.min_free_bytes)
    {
        GuardCheck::Exceeded
    } else {
        GuardCheck::Within
    }
}

fn directory_size(root: &std::path::Path, deadline: Instant) -> io::Result<Option<u64>> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    let mut entries_seen = 0_usize;
    while let Some(directory) = pending.pop() {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            if Instant::now() >= deadline || entries_seen >= MAX_GUARD_ENTRIES {
                return Ok(None);
            }
            let entry = entry?;
            entries_seen += 1;
            let metadata = match entry.path().symlink_metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(Some(total))
}

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn spawn_reader<R>(mut reader: R, limit: usize) -> Receiver<io::Result<BoundedOutput>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = (|| {
            let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
            let mut buffer = [0_u8; 16 * 1024];
            let mut truncated = false;
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    return Ok(BoundedOutput { bytes, truncated });
                }
                let remaining = limit.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                truncated |= count > remaining;
            }
        })();
        let _ = sender.send(result);
    });
    receiver
}

fn poll_reader(
    reader: &Receiver<io::Result<BoundedOutput>>,
    output: &mut Option<BoundedOutput>,
) -> Result<(), RunnerError> {
    if output.is_some() {
        return Ok(());
    }
    match reader.try_recv() {
        Ok(Ok(value)) => *output = Some(value),
        Ok(Err(error)) => return Err(RunnerError::Output(error)),
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            return Err(RunnerError::Output(io::Error::other(
                "output reader stopped unexpectedly",
            )));
        }
    }
    Ok(())
}

fn terminate_group(process_group: Pid, child: &mut std::process::Child) {
    let _ = killpg(process_group, Signal::SIGTERM);
    let grace_deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < grace_deadline {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let _ = killpg(process_group, Signal::SIGKILL);
    let _ = child.wait();
}

#[must_use]
pub fn git_environment() -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        (OsString::from("GCM_INTERACTIVE"), OsString::from("Never")),
        (OsString::from("GIT_ASKPASS"), OsString::from("/bin/false")),
        (OsString::from("SSH_ASKPASS"), OsString::from("/bin/false")),
    ])
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::*;

    #[test]
    fn output_is_drained_but_bounded() {
        let spec = CommandSpec::new("/bin/sh", Duration::from_secs(2))
            .args(["-c", "yes x | head -c 200000; yes y | head -c 200000 >&2"])
            .output_limit(1024);
        let output = run(&spec).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1024);
        assert_eq!(output.stderr.len(), 1024);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[test]
    fn timeout_terminates_descendant_process_group() {
        let spec =
            CommandSpec::new("/bin/sh", Duration::from_millis(100)).args(["-c", "sleep 30 & wait"]);
        assert!(matches!(run(&spec), Err(RunnerError::TimedOut { .. })));
    }

    #[test]
    fn exited_parent_with_pipe_holding_child_still_obeys_deadline() {
        let started = Instant::now();
        let spec = CommandSpec::new("/bin/sh", Duration::from_millis(100))
            .args(["-c", "sleep 30 & exit 0"]);
        assert!(matches!(run(&spec), Err(RunnerError::TimedOut { .. })));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn stdout_can_close_before_the_child_exits() {
        let spec = CommandSpec::new("/bin/sh", Duration::from_secs(2))
            .args(["-c", "exec 1>&-; sleep 0.1; echo done >&2"]);
        let output = run(&spec).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"done\n");
    }

    #[test]
    fn expired_deadline_does_not_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("marker");
        let spec = CommandSpec::new("/usr/bin/touch", Duration::from_secs(1))
            .arg(marker_path.as_os_str())
            .deadline(Instant::now());
        assert!(matches!(run(&spec), Err(RunnerError::TimedOut { .. })));
        assert!(!marker_path.exists());
    }

    #[test]
    fn disk_guard_stops_a_progressively_growing_process() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("growing");
        let command = format!(
            "while :; do dd if=/dev/zero bs=16384 count=1 2>/dev/null; sleep 0.02; done > '{}'",
            output.display()
        );
        let spec = CommandSpec::new("/bin/sh", Duration::from_secs(5))
            .args(["-c", &command])
            .disk_guard(DiskGuard {
                root: directory.path().to_path_buf(),
                max_bytes: 64 * 1024,
                min_free_bytes: 0,
            });
        assert!(matches!(run(&spec), Err(RunnerError::ResourceLimit { .. })));
        assert!(fs::metadata(output).unwrap().len() < 16 * 1024 * 1024);
    }

    #[test]
    fn large_guard_tree_cannot_overrun_a_short_deadline() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..4_000 {
            fs::write(directory.path().join(format!("entry-{index}")), b"").unwrap();
        }
        let started = Instant::now();
        let spec = CommandSpec::new("/bin/sleep", Duration::from_millis(1))
            .arg("30")
            .disk_guard(DiskGuard {
                root: directory.path().to_path_buf(),
                max_bytes: u64::MAX,
                min_free_bytes: 0,
            });
        assert!(matches!(run(&spec), Err(RunnerError::TimedOut { .. })));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
