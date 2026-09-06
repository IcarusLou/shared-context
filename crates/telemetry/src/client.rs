use std::{
    env,
    fs::OpenOptions,
    io::{ErrorKind, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use crate::{Event, wire::encode_frame};
use sha2::{Digest, Sha256};

const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitOutcome {
    Sent,
    RootMissing,
    EndpointMissing,
    EndpointInvalid,
    WouldBlock,
    FrameTooLarge,
    Rejected,
    Io,
}

#[must_use]
pub fn default_logs_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("SCTX_LOGS_ROOT").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(root));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".shared-context-logs"))
}

#[must_use]
pub fn default_endpoint(logs_root: &Path) -> PathBuf {
    endpoint_for_uid(logs_root, nix::unistd::geteuid().as_raw())
}

fn endpoint_for_uid(logs_root: &Path, effective_uid: u32) -> PathBuf {
    let digest = Sha256::digest(logs_root.as_os_str().as_bytes());
    let mut identity = String::with_capacity(24);
    for byte in &digest[..12] {
        identity.push(char::from(HEX[usize::from(byte >> 4)]));
        identity.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    ipc_parent()
        .join(format!("shared-context-logs-{effective_uid}"))
        .join(format!("ipc-{identity}"))
        .join("collector.fifo")
}

#[cfg(target_os = "macos")]
fn ipc_parent() -> &'static Path {
    Path::new("/private/tmp")
}

#[cfg(not(target_os = "macos"))]
fn ipc_parent() -> &'static Path {
    Path::new("/tmp")
}

/// Attempts one atomic FIFO write and returns immediately on every failure.
#[must_use]
pub fn emit_to(logs_root: &Path, event: &Event) -> EmitOutcome {
    let endpoint = default_endpoint(logs_root);
    let opened = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&endpoint);
    let mut fifo = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return EmitOutcome::EndpointMissing,
        Err(error) if error.kind() == ErrorKind::WouldBlock => return EmitOutcome::WouldBlock,
        Err(_) => return EmitOutcome::Io,
    };
    let Some(frame) = encode_frame(event) else {
        return EmitOutcome::FrameTooLarge;
    };
    match fifo.metadata() {
        Ok(metadata)
            if metadata.file_type().is_fifo()
                && metadata.uid() == nix::unistd::geteuid().as_raw()
                && metadata.mode().trailing_zeros() >= 6 => {}
        Ok(_) => return EmitOutcome::EndpointInvalid,
        Err(_) => return EmitOutcome::Io,
    }
    match fifo.write(&frame) {
        Ok(written) if written == frame.len() => EmitOutcome::Sent,
        Ok(_) => EmitOutcome::Rejected,
        Err(error) if error.kind() == ErrorKind::WouldBlock => EmitOutcome::WouldBlock,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::BrokenPipe | ErrorKind::NotConnected
            ) =>
        {
            EmitOutcome::Rejected
        }
        Err(_) => EmitOutcome::Io,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        process::Command,
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{EntryPoint, EventKind, FRAME_MAX_BYTES, FrameDecoder};

    fn prepare(root: &Path, fifo: bool) -> PathBuf {
        let endpoint = default_endpoint(root);
        fs::create_dir_all(endpoint.parent().expect("parent")).expect("runtime dir");
        fs::set_permissions(
            endpoint.parent().expect("parent"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("private runtime dir");
        if fifo {
            let status = Command::new("mkfifo")
                .arg("-m")
                .arg("600")
                .arg(&endpoint)
                .status()
                .expect("mkfifo");
            assert!(status.success());
        } else {
            fs::write(&endpoint, b"ordinary").expect("ordinary endpoint");
        }
        endpoint
    }

    fn cleanup(endpoint: &Path) {
        let _ = fs::remove_file(endpoint);
        if let Some(parent) = endpoint.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    #[test]
    fn sends_one_decodable_frame_without_touching_logs_root() {
        let root = TempDir::new().expect("root");
        let absent_root = root.path().join("does-not-exist");
        let endpoint = prepare(&absent_root, true);
        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&endpoint)
            .expect("reader");
        let event = Event::started(
            EntryPoint::Cli,
            EventKind::OperationStarted,
            "inv",
            "status",
        );
        assert_eq!(emit_to(&absent_root, &event), EmitOutcome::Sent);
        let mut bytes = [0; FRAME_MAX_BYTES];
        let read = reader.read(&mut bytes).expect("read frame");
        let decoded = FrameDecoder::new().push(&bytes[..read]);
        assert_eq!(decoded, vec![Ok(event.normalized())]);
        cleanup(&endpoint);
    }

    #[test]
    fn rejects_regular_endpoint_and_returns_quickly_without_reader() {
        let root = TempDir::new().expect("root");
        let ordinary = prepare(root.path(), false);
        let event = Event::started(EntryPoint::Hook, EventKind::HookDecision, "inv", "hook");
        assert_eq!(emit_to(root.path(), &event), EmitOutcome::EndpointInvalid);
        cleanup(&ordinary);

        let fifo = prepare(root.path(), true);
        let started = Instant::now();
        assert_ne!(emit_to(root.path(), &event), EmitOutcome::Sent);
        assert!(started.elapsed() < Duration::from_millis(100));
        cleanup(&fifo);
    }

    #[test]
    fn concurrent_atomic_writers_do_not_mix_frames() {
        let root = TempDir::new().expect("root");
        let endpoint = prepare(root.path(), true);
        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&endpoint)
            .expect("reader");
        let barrier = Arc::new(Barrier::new(32));
        let mut writers = Vec::new();
        for index in 0..32 {
            let root = root.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            writers.push(thread::spawn(move || {
                let event = Event::started(
                    EntryPoint::Mcp,
                    EventKind::ToolStarted,
                    format!("inv-{index}"),
                    "query",
                );
                barrier.wait();
                emit_to(&root, &event)
            }));
        }
        for writer in writers {
            assert_eq!(writer.join().expect("writer"), EmitOutcome::Sent);
        }
        let mut decoder = FrameDecoder::new();
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        while events.len() < 32 && Instant::now() < deadline {
            let mut bytes = [0; 4096];
            match reader.read(&mut bytes) {
                Ok(0) => thread::yield_now(),
                Ok(read) => events.extend(
                    decoder
                        .push(&bytes[..read])
                        .into_iter()
                        .map(|item| item.expect("valid frame")),
                ),
                Err(error) if error.kind() == ErrorKind::WouldBlock => thread::yield_now(),
                Err(error) => panic!("read: {error}"),
            }
        }
        events.sort_by(|left, right| left.invocation_id.cmp(&right.invocation_id));
        assert_eq!(events.len(), 32);
        events.dedup_by(|left, right| left.invocation_id == right.invocation_id);
        assert_eq!(events.len(), 32);
        cleanup(&endpoint);
    }

    #[test]
    fn a_full_fifo_is_a_bounded_drop() {
        let root = TempDir::new().expect("root");
        let endpoint = prepare(root.path(), true);
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&endpoint)
            .expect("reader");
        let mut filler = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&endpoint)
            .expect("writer");
        let block = [0u8; FRAME_MAX_BYTES];
        let mut full = false;
        for _ in 0..10_000 {
            match filler.write(&block) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    full = true;
                    break;
                }
                Err(error) => panic!("fill FIFO: {error}"),
            }
        }
        assert!(full);
        let event = Event::started(
            EntryPoint::Cli,
            EventKind::OperationStarted,
            "inv",
            "status",
        );
        let started = Instant::now();
        assert_eq!(emit_to(root.path(), &event), EmitOutcome::WouldBlock);
        assert!(started.elapsed() < Duration::from_millis(100));
        cleanup(&endpoint);
    }

    #[test]
    fn unsafe_fifo_permissions_are_rejected() {
        let root = TempDir::new().expect("root");
        let endpoint = prepare(root.path(), true);
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o644))
            .expect("unsafe permissions");
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&endpoint)
            .expect("reader");
        let event = Event::started(
            EntryPoint::Cli,
            EventKind::OperationStarted,
            "inv",
            "status",
        );
        assert_eq!(emit_to(root.path(), &event), EmitOutcome::EndpointInvalid);
        cleanup(&endpoint);
    }

    #[test]
    fn endpoint_uid_namespaces_are_distinct() {
        let root = Path::new("/private/tmp/logs-root");
        assert_ne!(endpoint_for_uid(root, 501), endpoint_for_uid(root, 502));
        assert!(
            endpoint_for_uid(root, 501)
                .to_string_lossy()
                .contains("shared-context-logs-501")
        );
    }

    #[test]
    fn endpoint_and_delivery_are_independent_of_tmpdir_in_subprocesses() {
        const CHILD_ENV: &str = "SCTX_ENDPOINT_TEST_CHILD";
        const ROOT_ENV: &str = "SCTX_ENDPOINT_TEST_ROOT";
        if let Some(invocation) = std::env::var_os(CHILD_ENV) {
            let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("child root"));
            let event = Event::started(
                EntryPoint::Cli,
                EventKind::OperationStarted,
                invocation.to_string_lossy(),
                "status",
            );
            assert_eq!(emit_to(&root, &event), EmitOutcome::Sent);
            println!("ENDPOINT={}", default_endpoint(&root).display());
            return;
        }

        let root = TempDir::new().expect("root");
        let endpoint = prepare(root.path(), true);
        let mut reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&endpoint)
            .expect("reader");
        let executable = std::env::current_exe().expect("test executable");
        let test_name =
            "client::tests::endpoint_and_delivery_are_independent_of_tmpdir_in_subprocesses";
        let run_child = |invocation: &str, tmpdir: Option<&str>| {
            let mut command = Command::new(&executable);
            command
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD_ENV, invocation)
                .env(ROOT_ENV, root.path());
            if let Some(tmpdir) = tmpdir {
                command.env("TMPDIR", tmpdir);
            } else {
                command.env_remove("TMPDIR");
            }
            let output = command.output().expect("child process");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).expect("child stdout");
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("ENDPOINT="))
                .expect("child endpoint")
                .to_owned()
        };
        let without_tmpdir = run_child("without-tmpdir", None);
        let with_tmpdir = run_child("with-tmpdir", Some("/var/empty/different-tmpdir"));
        assert_eq!(without_tmpdir, with_tmpdir);
        assert_eq!(without_tmpdir, endpoint.display().to_string());

        let mut decoder = FrameDecoder::new();
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        while events.len() < 2 && Instant::now() < deadline {
            let mut bytes = [0; 1024];
            match reader.read(&mut bytes) {
                Ok(0) => thread::yield_now(),
                Ok(read) => events.extend(
                    decoder
                        .push(&bytes[..read])
                        .into_iter()
                        .map(|result| result.expect("valid child frame")),
                ),
                Err(error) if error.kind() == ErrorKind::WouldBlock => thread::yield_now(),
                Err(error) => panic!("read child frames: {error}"),
            }
        }
        events.sort_by(|left, right| left.invocation_id.cmp(&right.invocation_id));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].invocation_id, "with-tmpdir");
        assert_eq!(events[1].invocation_id, "without-tmpdir");
        cleanup(&endpoint);
    }
}
