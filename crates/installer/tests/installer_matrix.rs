use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use sctx_installer::{
    Agent, Architecture, Host, InstallContext, Installer, SetupOptions, SetupStage,
};
use tempfile::{TempDir, tempdir};

#[derive(Clone)]
struct FakeHost {
    platform: &'static str,
    architecture: Option<Architecture>,
    git_ok: bool,
    signature_ok: bool,
    space: u64,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            platform: "macos",
            architecture: Some(Architecture::Arm64),
            git_ok: true,
            signature_ok: true,
            space: u64::MAX,
        }
    }
}

impl Host for FakeHost {
    fn platform(&self) -> &str {
        self.platform
    }

    fn architecture(&self) -> Option<Architecture> {
        self.architecture
    }

    fn git_version(&self) -> sctx_installer::Result<String> {
        if self.git_ok {
            Ok("git version fixture".to_owned())
        } else {
            Err(sctx_installer::Error::new(
                sctx_installer::ErrorKind::External,
                "Git unavailable",
            ))
        }
    }

    fn verify_signature(&self, _executable: &Path) -> sctx_installer::Result<()> {
        if self.signature_ok {
            Ok(())
        } else {
            Err(sctx_installer::Error::new(
                sctx_installer::ErrorKind::InvariantViolation,
                "invalid signature",
            ))
        }
    }

    fn available_space(&self, _path: &Path) -> sctx_installer::Result<u64> {
        Ok(self.space)
    }

    fn agent_version(&self, agent: Agent) -> Option<String> {
        Some(
            match agent {
                Agent::Cursor => "3.13.10",
                Agent::Codex => "0.147.0",
            }
            .to_owned(),
        )
    }
}

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
    root: PathBuf,
    runtime: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("用户 Home 空格");
        let root = temporary.path().join("安装 根目录 中文");
        let runtime = temporary.path().join("runtime source/sctx fixture");
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(&runtime, b"signed-runtime-v1").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temporary: temporary,
            home,
            root,
            runtime,
        }
    }

    fn context(&self, version: &str) -> InstallContext {
        InstallContext::injected(&self.home, &self.root, &self.runtime, version)
    }

    fn installer(&self, version: &str) -> Installer {
        Installer::new(self.context(version), Arc::new(FakeHost::default()))
    }

    fn seed_configs(&self) -> Vec<(PathBuf, Vec<u8>, u32)> {
        let fixtures = [
            (
                self.home.join(".cursor/mcp.json"),
                r#"{
  "unknownTop": {"中文": true},
  "mcpServers": {"existing": {"command": "keep me"}}
}
"#
                .as_bytes()
                .to_vec(),
                0o640,
            ),
            (
                self.home.join(".cursor/hooks.json"),
                br#"{
  "version": 1,
  "unknown": "keep",
  "hooks": {"stop": [{"command": "user-stop", "future": 7}]}
}
"#
                .to_vec(),
                0o604,
            ),
            (
                self.home.join(".codex/config.toml"),
                b"# keep this leading comment\nmodel = \"fixture\" # keep inline\n\n[mcp_servers.existing]\ncommand = \"keep me\"\nunknown = true\n"
                    .to_vec(),
                0o600,
            ),
            (
                self.home.join(".codex/hooks.json"),
                br#"{
  "description": "keep unknown metadata",
  "hooks": {"Stop": [{"matcher": "custom", "hooks": [{"type": "command", "command": "keep"}]}]}
}
"#
                .to_vec(),
                0o620,
            ),
        ];
        fixtures
            .into_iter()
            .map(|(path, bytes, mode)| {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, &bytes).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                (path, bytes, mode)
            })
            .collect()
    }
}

fn hook_count(path: &Path, events: &[&str]) -> usize {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    events
        .iter()
        .map(|event| value["hooks"][event].as_array().map_or(0, Vec::len))
        .sum()
}

#[test]
fn setup_three_times_is_idempotent_and_preserves_existing_configuration() {
    let harness = Harness::new();
    harness.seed_configs();
    let installer = harness.installer("1.2.3");
    let first = installer.setup(&SetupOptions::default()).unwrap();
    assert!(first.changed);
    let second = installer.setup(&SetupOptions::default()).unwrap();
    let third = installer.setup(&SetupOptions::default()).unwrap();
    assert!(!second.changed);
    assert!(!third.changed);

    let cursor_mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.home.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(cursor_mcp["unknownTop"]["中文"], true);
    assert_eq!(cursor_mcp["mcpServers"].as_object().unwrap().len(), 2);
    assert_eq!(
        hook_count(
            &harness.home.join(".cursor/hooks.json"),
            &[
                "sessionStart",
                "beforeSubmitPrompt",
                "postToolUse",
                "preCompact",
                "stop",
                "sessionEnd"
            ]
        ),
        7
    );
    assert_eq!(
        hook_count(
            &harness.home.join(".codex/hooks.json"),
            &[
                "SessionStart",
                "UserPromptSubmit",
                "PostToolUse",
                "PreCompact",
                "Stop",
                "SessionEnd"
            ]
        ),
        7
    );
    let toml = fs::read_to_string(harness.home.join(".codex/config.toml")).unwrap();
    assert!(toml.contains("# keep this leading comment"));
    assert!(toml.contains("# keep inline"));
    assert!(toml.contains("[mcp_servers.existing]"));
    assert!(toml.contains("[mcp_servers.shared-context]"));
    assert!(harness.root.join("repository/.git").is_dir());
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("1.2.3/arm64")
    );
}

#[test]
fn every_setup_write_seam_restores_exact_agent_bytes_and_permissions() {
    let stages = [
        SetupStage::RuntimeInstalled,
        SetupStage::CurrentSwitched,
        SetupStage::RepositoryInitialized,
        SetupStage::IndexInitialized,
        SetupStage::CursorMcpWritten,
        SetupStage::CursorHooksWritten,
        SetupStage::CodexMcpWritten,
        SetupStage::CodexHooksWritten,
        SetupStage::ManifestWritten,
        SetupStage::SmokeTested,
    ];
    for stage in stages {
        let harness = Harness::new();
        let originals = harness.seed_configs();
        let parent_modes =
            [harness.home.join(".cursor"), harness.home.join(".codex")].map(|path| {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
                (path, mode)
            });
        let installer = harness.installer("1.2.3").with_failure_after(stage);
        let error = installer.setup(&SetupOptions::default()).unwrap_err();
        assert!(error.to_string().contains("injected setup failure"));
        for (path, bytes, mode) in &originals {
            assert_eq!(fs::read(path).unwrap(), *bytes, "bytes at {stage:?}");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                *mode,
                "mode at {stage:?}"
            );
        }
        for (path, mode) in parent_modes {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                mode,
                "parent mode at {stage:?}"
            );
        }
        assert!(
            !harness.root.join("bin/current").exists(),
            "current at {stage:?}"
        );
        assert!(
            !harness.root.join("bin/1.2.3/arm64/sctx").exists(),
            "runtime at {stage:?}"
        );
        assert!(!harness.root.join("state/install-manifest.json").exists());
        if stage >= SetupStage::RepositoryInitialized {
            assert!(harness.root.join("repository/.git").is_dir());
        }
    }
}

#[test]
fn preflight_rejects_platform_arch_git_signature_and_space_independently() {
    let harness = Harness::new();
    let cases = [
        FakeHost {
            platform: "linux",
            ..FakeHost::default()
        },
        FakeHost {
            architecture: None,
            ..FakeHost::default()
        },
        FakeHost {
            git_ok: false,
            ..FakeHost::default()
        },
        FakeHost {
            signature_ok: false,
            ..FakeHost::default()
        },
        FakeHost {
            space: 1,
            ..FakeHost::default()
        },
    ];
    for host in cases {
        let installer = Installer::new(harness.context("1.0.0"), Arc::new(host));
        assert!(installer.setup(&SetupOptions::default()).is_err());
        assert!(!harness.root.join("repository").exists());
    }
}

#[test]
fn x86_64_uses_the_distinct_x64_runtime_directory() {
    let harness = Harness::new();
    let host = FakeHost {
        architecture: Some(Architecture::X86_64),
        ..FakeHost::default()
    };
    Installer::new(harness.context("1.0.0"), Arc::new(host))
        .setup(&SetupOptions::default())
        .unwrap();
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("1.0.0/x64")
    );
    assert!(harness.root.join("bin/1.0.0/x64/sctx").is_file());
}

#[test]
fn upgrade_switches_atomically_and_failed_upgrade_restores_previous_runtime() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();
    let report = harness
        .installer("2.0.0")
        .upgrade(&SetupOptions::default())
        .unwrap();
    assert!(report.changed);
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("2.0.0/arm64")
    );
    assert_eq!(
        fs::read(harness.root.join("bin/current/sctx")).unwrap(),
        b"signed-runtime-v2"
    );
    assert!(harness.root.join("bin/1.0.0/arm64/sctx").is_file());

    fs::write(&harness.runtime, b"signed-runtime-v3").unwrap();
    let failed = harness
        .installer("3.0.0")
        .with_failure_after(SetupStage::CurrentSwitched)
        .upgrade(&SetupOptions::default());
    assert!(failed.is_err());
    assert_eq!(
        fs::read_link(harness.root.join("bin/current")).unwrap(),
        PathBuf::from("2.0.0/arm64")
    );
    assert!(!harness.root.join("bin/3.0.0/arm64/sctx").exists());
}

#[test]
fn doctor_reports_codex_trust_as_action_required() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let report = harness.installer("1.0.0").doctor();
    assert!(report.healthy);
    let codex = report
        .capabilities
        .iter()
        .find(|capability| format!("{:?}", capability.agent) == "Codex")
        .unwrap();
    assert!(codex.diagnostic.starts_with("ACTION REQUIRED:"));
    assert!(report.checks.iter().any(|check| {
        check.name == "codex_hook_trust.codex" && format!("{:?}", check.status) == "ActionRequired"
    }));
}

#[test]
fn uninstall_removes_only_exact_owned_entries_and_retains_repository() {
    let harness = Harness::new();
    let originals = harness.seed_configs();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();

    let cursor_hooks_path = harness.home.join(".cursor/hooks.json");
    let mut cursor_hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&cursor_hooks_path).unwrap()).unwrap();
    cursor_hooks["hooks"]["sessionStart"][0]["changed_by_user"] = true.into();
    cursor_hooks["hooks"]["userFuture"] = serde_json::json!([{"command": "keep user future hook"}]);
    fs::write(
        &cursor_hooks_path,
        serde_json::to_vec_pretty(&cursor_hooks).unwrap(),
    )
    .unwrap();

    let report = installer.uninstall().unwrap();
    assert!(report.repository_retained);
    assert!(harness.root.join("repository/.git").is_dir());
    assert!(!harness.root.join("bin").exists());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("modified after setup"))
    );

    let cursor_hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&cursor_hooks_path).unwrap()).unwrap();
    assert_eq!(
        cursor_hooks["hooks"]["sessionStart"][0]["changed_by_user"],
        true
    );
    assert_eq!(
        cursor_hooks["hooks"]["userFuture"][0]["command"],
        "keep user future hook"
    );
    assert!(cursor_hooks["hooks"].get("preCompact").is_none());

    let cursor_mcp: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.home.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert!(cursor_mcp["mcpServers"].get("shared-context").is_none());
    assert_eq!(cursor_mcp["mcpServers"]["existing"]["command"], "keep me");

    let codex = fs::read_to_string(harness.home.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("# keep this leading comment"));
    assert!(codex.contains("[mcp_servers.existing]"));
    assert!(!codex.contains("mcp_servers.shared-context"));
    for (path, bytes, mode) in originals
        .iter()
        .filter(|(path, _, _)| path != &cursor_hooks_path)
    {
        assert_eq!(
            fs::read(path).unwrap(),
            *bytes,
            "baseline bytes for {}",
            path.display()
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            *mode,
            "baseline mode for {}",
            path.display()
        );
    }
}

#[test]
fn knowledge_deletion_requires_path_and_phrase_as_two_confirmations() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    let repository = harness.root.join("repository");
    assert!(
        installer
            .delete_knowledge(
                &harness.root.join("wrong"),
                "DELETE-SHARED-CONTEXT-KNOWLEDGE"
            )
            .is_err()
    );
    assert!(installer.delete_knowledge(&repository, "DELETE").is_err());
    assert!(repository.exists());
    assert_eq!(
        installer
            .delete_knowledge(&repository, "DELETE-SHARED-CONTEXT-KNOWLEDGE")
            .unwrap(),
        repository
    );
    assert!(!repository.exists());
}

#[test]
fn next_setup_recovers_an_incomplete_durable_journal_before_reapplying() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    let report = installer.setup(&SetupOptions::default()).unwrap();
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&report.journal).unwrap()).unwrap();
    journal["complete"] = false.into();
    journal["phase"] = "simulated_process_exit".into();
    fs::write(
        &report.journal,
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let recovered = installer.setup(&SetupOptions::default()).unwrap();
    assert!(recovered.changed);
    assert!(harness.root.join("repository/.git").is_dir());
    assert!(harness.root.join("bin/current/sctx").is_file());
    let recovered_journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&report.journal).unwrap()).unwrap();
    assert_eq!(recovered_journal["phase"], "recovered_rollback");
    assert_eq!(recovered_journal["complete"], true);
}
