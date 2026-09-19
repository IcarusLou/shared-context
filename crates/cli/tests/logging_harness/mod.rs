use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use sctx_log_service::{HookDiagnostics, SealRequestOutcome};

pub struct LoggingHarness {
    root: PathBuf,
    collector: Child,
}

impl LoggingHarness {
    pub fn start(home: &Path) -> Self {
        let root = home.join(".shared-context-logs-test");
        let output = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["--json", "logs", "init", "--logs-root"])
            .arg(&root)
            .env("HOME", home)
            .env("SCTX_LOGS_ROOT", &root)
            .env("SCTX_SKIP_LAUNCHCTL", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "logs init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let collector = Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(["logs", "collect", "--logs-root"])
            .arg(&root)
            .env("HOME", home)
            .env("SCTX_LOGS_ROOT", &root)
            .env("SCTX_SKIP_LAUNCHCTL", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let endpoint = sctx_telemetry::default_endpoint(&root);
        while !endpoint.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            endpoint.exists(),
            "collector FIFO was not created at {}",
            endpoint.display()
        );
        Self { root, collector }
    }

    pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env("SCTX_LOGS_ROOT", &self.root)
            .env("SCTX_SKIP_LAUNCHCTL", "1")
    }

    pub fn diagnostics(&self) -> HookDiagnostics {
        let view = self.diagnostics_or_empty();
        assert!(
            view.updated_at_unix_ms > 0,
            "collector observed no diagnostic events"
        );
        view
    }

    pub fn diagnostics_or_empty(&self) -> HookDiagnostics {
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(
            sctx_log_service::request_seal(&self.root, &deadline).unwrap(),
            SealRequestOutcome::Sealed
        );
        sctx_log_service::load_hook_diagnostics(&self.root).unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn persisted_text(&self) -> String {
        let _ = self.diagnostics();
        let mut bytes = Vec::new();
        collect_regular_files(&self.root, &mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for LoggingHarness {
    fn drop(&mut self) {
        let _ = self.collector.kill();
        let _ = self.collector.wait();
    }
}

#[allow(dead_code)]
fn collect_regular_files(directory: &Path, bytes: &mut Vec<u8>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_regular_files(&path, bytes);
        } else if metadata.is_file() && metadata.len() <= 16 * 1024 * 1024 {
            if let Ok(mut content) = fs::read(path) {
                bytes.append(&mut content);
            }
        }
    }
}
