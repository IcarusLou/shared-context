use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
};

use sctx_installer::{
    Agent, Architecture, CheckStatus, Host, InstallContext, Installer, SetupOptions, SetupStage,
    SkillStatus,
};
use sha2::{Digest, Sha256};
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

    fn skill_root(&self) -> PathBuf {
        self.home.join(".agents/skills/shared-context")
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

fn replace_owned_skill_bytes(path: &Path, bytes: &[u8], manifest_path: &Path) {
    fs::write(path, bytes).unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    let owned = manifest["skills"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|owned| owned["path"] == path.to_string_lossy().as_ref())
        .unwrap();
    owned["sha256"] = format!("{:x}", Sha256::digest(bytes)).into();
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
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
    assert_eq!(first.skill.status, SkillStatus::Installed);
    assert_eq!(second.skill.status, SkillStatus::Current);
    assert_eq!(third.skill.status, SkillStatus::Current);
    assert_eq!(first.skill.path, harness.skill_root());
    assert_eq!(
        fs::read(harness.skill_root().join("SKILL.md")).unwrap(),
        include_bytes!("../../../skills/shared-context/SKILL.md")
    );
    assert_eq!(
        fs::read(harness.skill_root().join("agents/openai.yaml")).unwrap(),
        include_bytes!("../../../skills/shared-context/agents/openai.yaml")
    );

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
fn cursor_and_codex_share_one_global_skill_installation() {
    let harness = Harness::new();
    let cursor = SetupOptions {
        agents: [Agent::Cursor].into_iter().collect(),
    };
    let codex = SetupOptions {
        agents: [Agent::Codex].into_iter().collect(),
    };
    let first = harness.installer("1.0.0").setup(&cursor).unwrap();
    let second = harness.installer("1.0.0").setup(&codex).unwrap();

    assert_eq!(first.skill.status, SkillStatus::Installed);
    assert_eq!(second.skill.status, SkillStatus::Current);
    assert_eq!(first.skill.path, second.skill.path);
    assert!(harness.skill_root().join("SKILL.md").is_file());
    assert!(harness.skill_root().join("agents/openai.yaml").is_file());
}

#[test]
fn setup_preserves_and_does_not_claim_an_external_same_name_skill() {
    let harness = Harness::new();
    let skill = harness.skill_root();
    fs::create_dir_all(skill.join("agents")).unwrap();
    fs::write(skill.join("SKILL.md"), b"user skill\n").unwrap();
    fs::write(skill.join("agents/openai.yaml"), b"user: metadata\n").unwrap();

    let installer = harness.installer("1.0.0");
    let report = installer.setup(&SetupOptions::default()).unwrap();
    assert_eq!(report.skill.status, SkillStatus::Conflict);
    assert!(report.notices.iter().any(|notice| {
        notice.contains("user-owned global Agent Skill") && notice.contains("did not overwrite")
    }));
    assert_eq!(fs::read(skill.join("SKILL.md")).unwrap(), b"user skill\n");
    assert_eq!(
        fs::read(skill.join("agents/openai.yaml")).unwrap(),
        b"user: metadata\n"
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(harness.root.join("state/install-manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["skills"].as_array().unwrap().len(), 0);

    let uninstall = installer.uninstall().unwrap();
    assert!(skill.join("SKILL.md").is_file());
    assert!(skill.join("agents/openai.yaml").is_file());
    assert!(!uninstall.removed.contains(&skill));
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
        SetupStage::GlobalSkillWritten,
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
        assert!(!harness.skill_root().exists(), "skill root at {stage:?}");
        assert!(
            !harness.home.join(".agents").exists(),
            "new global Skill parents at {stage:?}"
        );
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
fn upgrade_and_uninstall_preserve_only_the_user_modified_skill_file() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let skill_md = harness.skill_root().join("SKILL.md");
    let openai_yaml = harness.skill_root().join("agents/openai.yaml");
    fs::write(&skill_md, b"user modified skill\n").unwrap();
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

    let installer = harness.installer("2.0.0");
    let upgrade = installer.upgrade(&SetupOptions::default()).unwrap();
    assert_eq!(upgrade.skill.status, SkillStatus::Modified);
    assert_eq!(fs::read(&skill_md).unwrap(), b"user modified skill\n");
    assert!(openai_yaml.is_file());
    assert!(
        upgrade
            .notices
            .iter()
            .any(|notice| notice.contains("user-modified global Agent Skill file"))
    );

    let uninstall = installer.uninstall().unwrap();
    assert!(skill_md.is_file());
    assert!(!openai_yaml.exists());
    assert!(harness.skill_root().is_dir());
    assert!(uninstall.preserved.contains(&skill_md));
    assert!(uninstall.removed.contains(&openai_yaml));
    assert!(
        uninstall
            .warnings
            .iter()
            .any(|warning| warning.contains("user-modified global Agent Skill file"))
    );
}

#[test]
fn uninstall_never_follows_a_replaced_skill_parent_symlink() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    let skill = harness.skill_root();
    let displaced = harness.home.join("displaced-managed-skill");
    let outside = harness.home.join("outside-symlink-target");
    fs::rename(&skill, &displaced).unwrap();
    fs::create_dir_all(outside.join("agents")).unwrap();
    fs::write(
        outside.join("SKILL.md"),
        include_bytes!("../../../skills/shared-context/SKILL.md"),
    )
    .unwrap();
    fs::write(
        outside.join("agents/openai.yaml"),
        include_bytes!("../../../skills/shared-context/agents/openai.yaml"),
    )
    .unwrap();
    symlink(&outside, &skill).unwrap();

    let report = installer.uninstall().unwrap();
    assert!(skill.is_symlink());
    assert!(outside.join("SKILL.md").is_file());
    assert!(outside.join("agents/openai.yaml").is_file());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("parent is not a non-symlink directory"))
    );
}

#[test]
fn upgrade_replaces_an_unchanged_managed_skill_from_an_older_build() {
    let harness = Harness::new();
    harness
        .installer("1.0.0")
        .setup(&SetupOptions::default())
        .unwrap();
    let skill_md = harness.skill_root().join("SKILL.md");
    let manifest = harness.root.join("state/install-manifest.json");
    replace_owned_skill_bytes(&skill_md, b"older managed skill\n", &manifest);
    fs::set_permissions(&skill_md, fs::Permissions::from_mode(0o640)).unwrap();
    fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

    let report = harness
        .installer("2.0.0")
        .upgrade(&SetupOptions::default())
        .unwrap();
    assert_eq!(report.skill.status, SkillStatus::Installed);
    assert_eq!(
        fs::read(&skill_md).unwrap(),
        include_bytes!("../../../skills/shared-context/SKILL.md")
    );
    assert_eq!(
        fs::metadata(&skill_md).unwrap().permissions().mode() & 0o7777,
        0o640
    );
}

#[test]
fn failed_upgrade_restores_managed_skill_bytes_and_permissions() {
    for stage in [
        SetupStage::GlobalSkillWritten,
        SetupStage::ManifestWritten,
        SetupStage::SmokeTested,
    ] {
        let harness = Harness::new();
        harness
            .installer("1.0.0")
            .setup(&SetupOptions::default())
            .unwrap();
        let manifest = harness.root.join("state/install-manifest.json");
        let originals = [
            (
                harness.skill_root().join("SKILL.md"),
                b"old skill\n".as_slice(),
                0o640,
            ),
            (
                harness.skill_root().join("agents/openai.yaml"),
                b"old: metadata\n".as_slice(),
                0o604,
            ),
        ];
        for (path, bytes, mode) in &originals {
            replace_owned_skill_bytes(path, bytes, &manifest);
            fs::set_permissions(path, fs::Permissions::from_mode(*mode)).unwrap();
        }
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o640)).unwrap();
        let original_manifest = fs::read(&manifest).unwrap();
        fs::write(&harness.runtime, b"signed-runtime-v2").unwrap();

        let result = harness
            .installer("2.0.0")
            .with_failure_after(stage)
            .upgrade(&SetupOptions::default());
        assert!(result.is_err());
        for (path, bytes, mode) in &originals {
            assert_eq!(fs::read(path).unwrap(), *bytes, "bytes at {stage:?}");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                *mode,
                "mode at {stage:?}"
            );
        }
        assert_eq!(fs::read(&manifest).unwrap(), original_manifest);
        assert_eq!(
            fs::metadata(&manifest).unwrap().permissions().mode() & 0o7777,
            0o640
        );
    }
}

#[test]
fn old_manifest_without_skill_ownership_is_accepted_without_claiming_existing_files() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    let manifest_path = harness.root.join("state/install-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("skills");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let report = installer.setup(&SetupOptions::default()).unwrap();
    assert_eq!(report.skill.status, SkillStatus::Conflict);
    let rewritten: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(rewritten["skills"].as_array().unwrap().len(), 0);

    installer.uninstall().unwrap();
    assert!(harness.skill_root().join("SKILL.md").is_file());
    assert!(harness.skill_root().join("agents/openai.yaml").is_file());
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
    assert!(
        report.checks.iter().any(|check| {
            check.name == "global_skill.skill_md" && check.status == CheckStatus::Ok
        })
    );
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.openai_yaml" && check.status == CheckStatus::Ok
    }));
}

#[test]
fn doctor_distinguishes_modified_and_missing_managed_skill_files() {
    let harness = Harness::new();
    let installer = harness.installer("1.0.0");
    installer.setup(&SetupOptions::default()).unwrap();
    fs::write(harness.skill_root().join("SKILL.md"), b"modified\n").unwrap();
    fs::remove_file(harness.skill_root().join("agents/openai.yaml")).unwrap();

    let report = installer.doctor();
    assert!(!report.healthy);
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.skill_md" && check.status == CheckStatus::ActionRequired
    }));
    assert!(report.checks.iter().any(|check| {
        check.name == "global_skill.openai_yaml" && check.status == CheckStatus::Error
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
    assert!(!harness.skill_root().exists());
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
