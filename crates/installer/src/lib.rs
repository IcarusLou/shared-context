//! Transactional macOS setup, diagnosis, upgrade, and uninstall support.
//!
//! Knowledge in `repository/` is deliberately outside the rollback and normal uninstall sets.
//! Runtime and Agent configuration mutations are journalled before every write and can therefore
//! be restored byte-for-byte (including Unix permissions) after either an injected failure or an
//! interrupted earlier invocation.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{BufReader, Cursor, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;
use sctx_adapter_codex::TrustState;
pub use sctx_domain::{Error, ErrorKind, Result};
use sctx_engineering_graph::{EngineeringProjectionStore, RepositoryRegistry};
use sctx_git_store::GitStore;
use sctx_index::ProjectionIndex;
use sctx_local_state::{
    AuthorizedSessionScopeStore, CaptureStore, CatalogCheckoutStatus, MaintenanceLock,
    UserConfigStore,
};
use sctx_mcp::{ClientKind, McpServer};
use sctx_search::{SearchEngine, SearchFilters, SearchRequest};
use sctx_task_runtime::TaskRuntime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, value};
use uuid::Uuid;

const JOURNAL_VERSION: u32 = 1;
const RESET_JOURNAL_VERSION: u32 = 1;
const MANIFEST_VERSION: u32 = 1;
const MINIMUM_FREE_SPACE_BYTES: u64 = 64 * 1024 * 1024;
const AGENT_VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const PRODUCT_KEY: &str = "shared-context";
const GLOBAL_SKILL_DIRECTORY: &str = ".agents/skills/shared-context";
const SKILL_ASSETS: [(&str, &[u8]); 3] = [
    (
        "SKILL.md",
        include_bytes!("../../../skills/shared-context/SKILL.md"),
    ),
    (
        "references/workflow.md",
        include_bytes!("../../../skills/shared-context/references/workflow.md"),
    ),
    (
        "agents/openai.yaml",
        include_bytes!("../../../skills/shared-context/agents/openai.yaml"),
    ),
];
const HOOK_EVENTS_CURSOR: [&str; 6] = [
    "sessionStart",
    "beforeSubmitPrompt",
    "postToolUse",
    "preCompact",
    "stop",
    "sessionEnd",
];
const HOOK_EVENTS_CODEX: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "PreCompact",
    "Stop",
    "SessionEnd",
];

/// Supported installer CPU architectures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    Arm64,
    X86_64,
}

impl Architecture {
    fn directory(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X86_64 => "x64",
        }
    }
}

/// A write boundary exposed for deterministic rollback testing.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStage {
    RuntimeInstalled,
    CurrentSwitched,
    RepositoryInitialized,
    IndexInitialized,
    CursorMcpWritten,
    CursorHooksWritten,
    CodexMcpWritten,
    CodexHooksWritten,
    GlobalSkillGateWritten,
    GlobalSkillWorkflowWritten,
    GlobalSkillMetadataWritten,
    GlobalSkillWritten,
    ManifestWritten,
    SmokeTested,
}

/// Host-dependent preflight operations. Tests provide a deterministic implementation.
pub trait Host: Send + Sync {
    fn platform(&self) -> &str;
    fn architecture(&self) -> Option<Architecture>;
    /// Returns the detected Git version.
    ///
    /// # Errors
    ///
    /// Returns an external error when Git cannot be executed successfully.
    fn git_version(&self) -> Result<String>;
    /// Verifies the packaged runtime signature.
    ///
    /// # Errors
    ///
    /// Returns an invariant error when the signature is absent or invalid.
    fn verify_signature(&self, executable: &Path) -> Result<()>;
    /// Returns available bytes for the filesystem containing `path`.
    ///
    /// # Errors
    ///
    /// Returns an external or parse error when free space cannot be determined.
    fn available_space(&self, path: &Path) -> Result<u64>;
    fn agent_version(&self, agent: Agent) -> Option<String>;
}

/// Production host implementation using argv-based macOS commands.
#[derive(Debug, Default)]
pub struct SystemHost;

impl Host for SystemHost {
    fn platform(&self) -> &str {
        env::consts::OS
    }

    fn architecture(&self) -> Option<Architecture> {
        match env::consts::ARCH {
            "aarch64" => Some(Architecture::Arm64),
            "x86_64" => Some(Architecture::X86_64),
            _ => None,
        }
    }

    fn git_version(&self) -> Result<String> {
        command_stdout(Command::new("git").arg("--version"), "git --version")
    }

    fn verify_signature(&self, executable: &Path) -> Result<()> {
        let output = Command::new("codesign")
            .args(["--verify", "--strict", "--verbose=2"])
            .arg(executable)
            .output()
            .map_err(external_error("execute codesign"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::InvariantViolation,
                format!(
                    "runtime signature verification failed for {}: {}",
                    executable.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ))
        }
    }

    fn available_space(&self, path: &Path) -> Result<u64> {
        let existing = nearest_existing_ancestor(path)?;
        let output = Command::new("df")
            .args(["-Pk"])
            .arg(existing)
            .output()
            .map_err(external_error("execute df"))?;
        if !output.status.success() {
            return Err(Error::new(
                ErrorKind::External,
                format!(
                    "df failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|error| invalid(format!("df output was not UTF-8: {error}")))?;
        let line = stdout
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .ok_or_else(|| invalid("df returned no filesystem row"))?;
        let blocks = line
            .split_ascii_whitespace()
            .nth(3)
            .ok_or_else(|| invalid("df output did not contain available blocks"))?
            .parse::<u64>()
            .map_err(|error| invalid(format!("invalid df available blocks: {error}")))?;
        Ok(blocks.saturating_mul(1024))
    }

    fn agent_version(&self, agent: Agent) -> Option<String> {
        let executable = match agent {
            Agent::Cursor => "cursor",
            Agent::Codex => "codex",
        };
        command_stdout_with_timeout(
            Command::new(executable).arg("--version"),
            AGENT_VERSION_TIMEOUT,
        )
    }
}

fn command_stdout_with_timeout(command: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                let output = child.wait_with_output().ok()?;
                return status
                    .success()
                    .then(|| String::from_utf8(output.stdout).ok())
                    .flatten()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty());
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Agent targets managed by setup.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Cursor,
    Codex,
}

/// Fully injectable installation context.
#[derive(Clone, Debug)]
pub struct InstallContext {
    pub home: PathBuf,
    pub root: PathBuf,
    pub runtime_source: PathBuf,
    pub version: String,
    pub minimum_free_space_bytes: u64,
}

impl InstallContext {
    /// Builds the production context from `HOME`, the current executable, and package version.
    ///
    /// # Errors
    ///
    /// Returns an error when `HOME` is absent or the current executable cannot be resolved.
    pub fn current_process() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| invalid("HOME is not set"))?;
        let root = home.join(".shared-context");
        let runtime_source = env::current_exe().map_err(io_error("resolve current executable"))?;
        Ok(Self {
            home,
            root,
            runtime_source,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            minimum_free_space_bytes: MINIMUM_FREE_SPACE_BYTES,
        })
    }

    /// Builds a test/package-launcher context without consulting process globals.
    #[must_use]
    pub fn injected(
        home: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        runtime_source: impl Into<PathBuf>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            home: home.into(),
            root: root.into(),
            runtime_source: runtime_source.into(),
            version: version.into(),
            minimum_free_space_bytes: MINIMUM_FREE_SPACE_BYTES,
        }
    }
}

/// Setup selection. Both supported Agents are selected by default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupOptions {
    pub agents: BTreeSet<Agent>,
    pub knowledge_store_url: Option<KnowledgeStoreUrl>,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            agents: [Agent::Cursor, Agent::Codex].into_iter().collect(),
            knowledge_store_url: None,
        }
    }
}

/// Validated Git URL used only during remote Knowledge Store setup.
#[derive(Clone, Eq, PartialEq)]
pub struct KnowledgeStoreUrl(String);

impl KnowledgeStoreUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn remote_type(&self) -> KnowledgeRemoteType {
        classify_remote_type(&self.0)
    }

    fn digest(&self) -> String {
        sha256(self.0.as_bytes())
    }
}

impl std::fmt::Debug for KnowledgeStoreUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("KnowledgeStoreUrl(<redacted>)")
    }
}

impl FromStr for KnowledgeStoreUrl {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        validate_knowledge_store_url(value)?;
        Ok(Self(value.to_owned()))
    }
}

/// Transport class retained without persisting the remote URL itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeRemoteType {
    Local,
    Http,
    Https,
    Ssh,
    Git,
}

/// Safe Knowledge Store identity returned by setup without exposing credentials or URLs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct KnowledgeStoreReport {
    pub installation_id: String,
    pub source: String,
    pub remote_type: Option<KnowledgeRemoteType>,
    pub default_branch: String,
    pub work_branch: Option<String>,
    pub url_digest: Option<String>,
}

/// One preflight observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreflightReport {
    pub platform: String,
    pub architecture: Architecture,
    pub git_version: String,
    pub signature_verified: bool,
    pub available_space_bytes: u64,
    pub required_space_bytes: u64,
}

/// Completed setup/upgrade result.
#[derive(Clone, Debug, Serialize)]
pub struct SetupReport {
    pub operation: String,
    pub root: PathBuf,
    pub repository: PathBuf,
    pub runtime: PathBuf,
    pub journal: PathBuf,
    pub changed: bool,
    pub knowledge_store: KnowledgeStoreReport,
    pub preflight: PreflightReport,
    pub capabilities: Vec<sctx_agent_adapter::AgentCapabilities>,
    pub skill: SkillReport,
    pub notices: Vec<String>,
}

/// Result of installing the shared user-level Agent Skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillReport {
    pub path: PathBuf,
    pub status: SkillStatus,
}

/// User-level Agent Skill state observed by setup or upgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatus {
    Installed,
    Current,
    Modified,
    Conflict,
    Missing,
}

/// Doctor severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warning,
    ActionRequired,
    Error,
}

/// One doctor check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

/// Read-only diagnosis result (unless `doctor --fix` explicitly invokes setup repairs).
#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub root: PathBuf,
    pub checks: Vec<DoctorCheck>,
    pub capabilities: Vec<sctx_agent_adapter::AgentCapabilities>,
}

/// Normal uninstall result. `repository` is always retained.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UninstallReport {
    pub repository: PathBuf,
    pub repository_retained: bool,
    pub removed: Vec<PathBuf>,
    pub preserved: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// One deterministic reset crash seam exposed only for recovery testing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetStage {
    Staged,
    FirstOriginalMoved,
    FirstReplacementInstalled,
    Swapped,
    SmokeTested,
}

/// Destructive reset selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DataResetOptions {
    pub confirmed: bool,
    pub dry_run: bool,
}

/// Completed reset or read-only reset plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DataResetReport {
    pub root: PathBuf,
    pub dry_run: bool,
    pub reset_id: Option<String>,
    pub backup: Option<PathBuf>,
    pub repository_count_cleared: usize,
    pub repository_group_count_cleared: usize,
    pub cleared_targets: Vec<PathBuf>,
    pub preserved: Vec<PathBuf>,
    pub remote_detached: bool,
    pub remote_mutated: bool,
}

/// Installer entry point.
pub struct Installer {
    context: InstallContext,
    host: Arc<dyn Host>,
    fail_after: Option<SetupStage>,
    reset_crash_after: Option<ResetStage>,
    codex_trust: TrustState,
}

impl Installer {
    /// Creates a production installer.
    ///
    /// # Errors
    ///
    /// Returns an error when production paths cannot be resolved.
    pub fn system() -> Result<Self> {
        Ok(Self::new(
            InstallContext::current_process()?,
            Arc::new(SystemHost),
        ))
    }

    /// Creates an installer with injected paths and host checks.
    #[must_use]
    pub fn new(context: InstallContext, host: Arc<dyn Host>) -> Self {
        Self {
            context,
            host,
            fail_after: None,
            reset_crash_after: None,
            codex_trust: TrustState::Unconfirmed,
        }
    }

    /// Injects one deterministic failure after a completed write stage.
    #[must_use]
    pub const fn with_failure_after(mut self, stage: SetupStage) -> Self {
        self.fail_after = Some(stage);
        self
    }

    /// Simulates a process crash after one reset stage, leaving recovery journal state.
    #[must_use]
    pub const fn with_reset_crash_after(mut self, stage: ResetStage) -> Self {
        self.reset_crash_after = Some(stage);
        self
    }

    /// Injects the externally observed Codex trust state for tests/managed callers.
    #[must_use]
    pub const fn with_codex_trust(mut self, trust: TrustState) -> Self {
        self.codex_trust = trust;
        self
    }

    /// Runs idempotent setup.
    ///
    /// # Errors
    ///
    /// Returns a typed preflight, filesystem, Git, index, config, or rollback error.
    pub fn setup(&self, options: &SetupOptions) -> Result<SetupReport> {
        self.install(Operation::Setup, options)
    }

    /// Installs a new version and atomically switches `bin/current`.
    ///
    /// # Errors
    ///
    /// Returns a typed preflight, filesystem, Git, index, config, or rollback error.
    pub fn upgrade(&self, options: &SetupOptions) -> Result<SetupReport> {
        self.install(Operation::Upgrade, options)
    }

    fn install(&self, operation: Operation, options: &SetupOptions) -> Result<SetupReport> {
        validate_context(&self.context)?;
        let preflight = self.preflight()?;
        ensure_private_directory(&self.context.root)?;
        for directory in ["bin", "state", "backups", "logs"] {
            ensure_private_directory(&self.context.root.join(directory))?;
        }
        let _maintenance = MaintenanceLock::initialize(&self.context.root)?.try_exclusive()?;
        let lock = open_lock(&self.context.root.join("state/setup.lock"))?;
        lock.lock_exclusive()
            .map_err(io_error("lock setup transaction"))?;
        recover_incomplete_data_reset(&self.context.root)?;
        recover_incomplete_journals(&self.context.root)?;
        if operation == Operation::Upgrade {
            require_existing_installation(&self.context.root)?;
        }

        let mut transaction = Transaction::begin(&self.context.root, operation)?;
        let result = self.install_locked(operation, options, preflight, &mut transaction);
        let result = match result {
            Ok(report) => {
                transaction.complete()?;
                Ok(report)
            }
            Err(error) => match transaction.rollback() {
                Ok(()) => Err(error),
                Err(rollback) => Err(Error::new(
                    ErrorKind::InvariantViolation,
                    format!("{error}; rollback also failed: {rollback}"),
                )),
            },
        };
        FileExt::unlock(&lock).map_err(io_error("unlock setup transaction"))?;
        result
    }

    #[allow(clippy::too_many_lines)]
    fn install_locked(
        &self,
        operation: Operation,
        options: &SetupOptions,
        preflight: PreflightReport,
        transaction: &mut Transaction,
    ) -> Result<SetupReport> {
        let architecture = preflight.architecture;
        let runtime_dir = self
            .context
            .root
            .join("bin")
            .join(&self.context.version)
            .join(architecture.directory());
        ensure_private_directory(&runtime_dir)?;
        let runtime = runtime_dir.join("sctx");
        let changed_runtime = install_runtime(transaction, &self.context.runtime_source, &runtime)?;
        self.fail(SetupStage::RuntimeInstalled)?;

        let current = self.context.root.join("bin/current");
        let relative_target = PathBuf::from(&self.context.version).join(architecture.directory());
        let changed_current = switch_current(transaction, &current, &relative_target)?;
        self.fail(SetupStage::CurrentSwitched)?;

        let prior_manifest = read_manifest(&self.context.root)?;
        let installation_id = installation_id(prior_manifest.as_ref())?;
        let KnowledgeStoreInstall {
            store,
            source: knowledge_store_source,
            changed: knowledge_store_changed,
        } = install_knowledge_store(
            &self.context.root,
            options.knowledge_store_url.as_ref(),
            prior_manifest.as_ref(),
            &installation_id,
            transaction,
        )?;
        self.fail(SetupStage::RepositoryInitialized)?;
        let index = ProjectionIndex::for_store(&store);
        index.synchronize()?;
        sctx_mcp::sync_repository_catalog_at_root(&self.context.root)?;
        self.fail(SetupStage::IndexInitialized)?;

        let stable_binary = current.join("sctx");
        let mut ownership = prior_manifest
            .as_ref()
            .map_or_else(Vec::new, |manifest| manifest.configs.clone());
        let prior_skill_ownership = prior_manifest
            .as_ref()
            .map_or_else(Vec::new, |manifest| manifest.skills.clone());
        let mut notices = Vec::new();
        let mut config_changed = false;

        if options.agents.contains(&Agent::Cursor) {
            config_changed |= merge_cursor_mcp(
                transaction,
                &self.context.home.join(".cursor/mcp.json"),
                &stable_binary,
                &mut ownership,
                &mut notices,
            )?;
            self.fail(SetupStage::CursorMcpWritten)?;
            config_changed |= merge_json_hooks(
                transaction,
                &self.context.home.join(".cursor/hooks.json"),
                Agent::Cursor,
                &stable_binary,
                &mut ownership,
                &mut notices,
            )?;
            self.fail(SetupStage::CursorHooksWritten)?;
        }
        if options.agents.contains(&Agent::Codex) {
            config_changed |= merge_codex_mcp(
                transaction,
                &self.context.home.join(".codex/config.toml"),
                &stable_binary,
                &mut ownership,
                &mut notices,
            )?;
            self.fail(SetupStage::CodexMcpWritten)?;
            config_changed |= merge_json_hooks(
                transaction,
                &self.context.home.join(".codex/hooks.json"),
                Agent::Codex,
                &stable_binary,
                &mut ownership,
                &mut notices,
            )?;
            self.fail(SetupStage::CodexHooksWritten)?;
        }

        finalize_ownership(transaction, &mut ownership)?;

        let skill_install = install_global_skill(
            transaction,
            &self.context.home,
            &prior_skill_ownership,
            &mut notices,
            self.fail_after,
        )?;
        self.fail(SetupStage::GlobalSkillWritten)?;

        let manifest = InstallManifest {
            version: MANIFEST_VERSION,
            installed_version: self.context.version.clone(),
            architecture,
            installation_id: installation_id.clone(),
            knowledge_store: knowledge_store_source.clone(),
            configs: ownership,
            skills: skill_install.ownership,
        };
        let manifest_changed = write_manifest(transaction, &self.context.root, &manifest)?;
        self.fail(SetupStage::ManifestWritten)?;
        mcp_smoke(&self.context.root)?;
        self.fail(SetupStage::SmokeTested)?;

        let capabilities = self.capabilities(options);
        notices.extend(
            capabilities
                .iter()
                .filter(|capability| capability.diagnostic.starts_with("ACTION REQUIRED:"))
                .map(|capability| capability.diagnostic.clone()),
        );
        Ok(SetupReport {
            operation: operation.as_str().to_owned(),
            root: self.context.root.clone(),
            repository: store.repository().to_path_buf(),
            runtime: stable_binary,
            journal: transaction.journal_path.clone(),
            changed: changed_runtime
                || changed_current
                || knowledge_store_changed
                || config_changed
                || skill_install.changed
                || manifest_changed,
            knowledge_store: knowledge_store_source.report(&installation_id),
            preflight,
            capabilities,
            skill: SkillReport {
                path: global_skill_root(&self.context.home),
                status: skill_install.status,
            },
            notices,
        })
    }

    /// Checks the installed layout, Git/index, config syntax/ownership, MCP, and Agent capability.
    #[must_use]
    pub fn doctor(&self) -> DoctorReport {
        let root = &self.context.root;
        let _maintenance = match MaintenanceLock::open_or_create(root)
            .and_then(|maintenance| maintenance.try_shared())
        {
            Ok(guard) => Some(guard),
            Err(error) if error.kind() == ErrorKind::MaintenanceBusy => {
                return DoctorReport {
                    healthy: false,
                    root: root.clone(),
                    checks: vec![failed(
                        "maintenance",
                        "installation maintenance is currently active",
                    )],
                    capabilities: self.capabilities(&SetupOptions::default()),
                };
            }
            Err(_) => None,
        };
        let mut checks = Vec::new();
        check_directory(root, &mut checks);
        check_runtime(root, &mut checks);
        check_repository(root, &mut checks);
        check_index(root, &mut checks);
        check_repository_catalog(root, &mut checks);
        check_configs(root, &self.context.home, &mut checks);
        check_global_skill(root, &self.context.home, &mut checks);
        if root.join("repository/.git").is_dir() && root.join("bin/current/sctx").is_file() {
            match mcp_smoke(root) {
                Ok(()) => checks.push(ok(
                    "mcp",
                    "initialize and tools/list succeeded for Cursor and Codex",
                )),
                Err(error) => checks.push(failed("mcp", error.to_string())),
            }
        } else {
            checks.push(failed(
                "mcp",
                "smoke skipped because repository or current runtime is missing",
            ));
        }
        let options = SetupOptions::default();
        let capabilities = self.capabilities(&options);
        for capability in &capabilities {
            let (status, name) = if capability.diagnostic.starts_with("ACTION REQUIRED:") {
                (CheckStatus::ActionRequired, "codex_hook_trust")
            } else if capability.hooks_verified() {
                (CheckStatus::Ok, "adapter_capability")
            } else {
                (CheckStatus::Warning, "adapter_capability")
            };
            checks.push(DoctorCheck {
                name: format!("{name}.{:?}", capability.agent).to_lowercase(),
                status,
                message: capability.diagnostic.clone(),
            });
        }
        let healthy = !checks
            .iter()
            .any(|check| check.status == CheckStatus::Error);
        DoctorReport {
            healthy,
            root: root.clone(),
            checks,
            capabilities,
        }
    }

    /// Re-runs the safe, reversible registration/index setup and then diagnoses it.
    ///
    /// # Errors
    ///
    /// Returns an error when a reversible repair cannot be completed.
    pub fn doctor_fix(&self, options: &SetupOptions) -> Result<DoctorReport> {
        require_existing_installation(&self.context.root)?;
        let manifest = read_manifest(&self.context.root)?
            .ok_or_else(|| invalid("doctor --fix requires an install manifest"))?;
        let mut context = self.context.clone();
        context.version = manifest.installed_version;
        let fixer = Self {
            context,
            host: Arc::clone(&self.host),
            fail_after: None,
            reset_crash_after: None,
            codex_trust: self.codex_trust,
        };
        fixer.setup(options)?;
        Ok(self.doctor())
    }

    /// Removes only exactly-owned registrations and rebuildable/runtime data.
    ///
    /// # Errors
    ///
    /// Returns an error when config ownership cannot be parsed or an exact removal cannot be
    /// persisted safely.
    pub fn uninstall(&self) -> Result<UninstallReport> {
        let root = &self.context.root;
        let repository = root.join("repository");
        let mut report = UninstallReport {
            repository: repository.clone(),
            repository_retained: repository.exists(),
            removed: Vec::new(),
            preserved: Vec::new(),
            warnings: Vec::new(),
        };
        if !root.exists() {
            return Ok(report);
        }
        let _maintenance = MaintenanceLock::open_or_create(root)?.try_exclusive()?;
        let lock = open_lock(&root.join("state/setup.lock"))?;
        lock.lock_exclusive().map_err(io_error("lock uninstall"))?;
        recover_incomplete_data_reset(root)?;
        recover_incomplete_journals(root)?;
        if let Some(manifest) = read_manifest(root)? {
            for config in &manifest.configs {
                if config.path != expected_config_path(&self.context.home, config.kind) {
                    report.warnings.push(format!(
                        "preserved unexpected manifest config path: {}",
                        config.path.display()
                    ));
                    report.preserved.push(config.path.clone());
                    continue;
                }
                uninstall_config(config, &mut report)?;
                let config_lock = config_lock_path(&config.path)?;
                remove_path_if_exists(&config_lock, &mut report.removed)?;
            }
            let mut had_expected_skill_ownership = false;
            for skill in &manifest.skills {
                if is_expected_skill_path(&self.context.home, &skill.path) {
                    had_expected_skill_ownership = true;
                    uninstall_owned_skill(&self.context.home, skill, &mut report)?;
                } else {
                    report.warnings.push(format!(
                        "preserved unexpected manifest global Agent Skill path: {}",
                        skill.path.display()
                    ));
                    report.preserved.push(skill.path.clone());
                }
            }
            if had_expected_skill_ownership {
                for directory in [
                    global_skill_root(&self.context.home).join("references"),
                    global_skill_root(&self.context.home).join("agents"),
                    global_skill_root(&self.context.home),
                ] {
                    if !global_skill_directory_is_safe(&self.context.home, &directory)? {
                        if fs::symlink_metadata(&directory).is_ok() {
                            report.preserved.push(directory.clone());
                            report.warnings.push(format!(
                                "preserved global Agent Skill directory because it or a parent is not a non-symlink directory: {}",
                                directory.display()
                            ));
                        }
                    } else if remove_empty_directory(&directory)? {
                        report.removed.push(directory);
                    }
                }
            }
        } else {
            report.warnings.push(
                "installation manifest is missing; Agent configuration and global Agent Skill were preserved"
                    .to_owned(),
            );
        }
        for path in [
            root.join("bin"),
            root.join("logs"),
            root.join("backups"),
            root.join("state/capture"),
        ] {
            remove_path_if_exists(&path, &mut report.removed)?;
        }
        for suffix in ["", "-wal", "-shm"] {
            remove_path_if_exists(
                &root.join(format!("state/index.sqlite{suffix}")),
                &mut report.removed,
            )?;
        }
        let manifest_path = manifest_path(root);
        remove_path_if_exists(&manifest_path, &mut report.removed)?;
        if repository.exists() {
            report.preserved.push(repository.clone());
        }
        FileExt::unlock(&lock).map_err(io_error("unlock uninstall"))?;
        Ok(report)
    }

    /// Deletes the knowledge repository only after two explicit confirmations.
    ///
    /// # Errors
    ///
    /// Returns an error when either confirmation differs or repository deletion fails.
    pub fn delete_knowledge(&self, confirmed_path: &Path, confirmation: &str) -> Result<PathBuf> {
        let repository = self.context.root.join("repository");
        let expected = absolute(&repository)?;
        let supplied = absolute(confirmed_path)?;
        if supplied != expected {
            return Err(invalid(format!(
                "knowledge path confirmation mismatch; expected {}",
                expected.display()
            )));
        }
        if confirmation != "DELETE-SHARED-CONTEXT-KNOWLEDGE" {
            return Err(invalid(
                "second confirmation must be DELETE-SHARED-CONTEXT-KNOWLEDGE",
            ));
        }
        let _maintenance = MaintenanceLock::open_or_create(&self.context.root)?.try_exclusive()?;
        let setup_lock = open_lock(&self.context.root.join("state/setup.lock"))?;
        setup_lock
            .lock_exclusive()
            .map_err(io_error("lock knowledge deletion"))?;
        recover_incomplete_data_reset(&self.context.root)?;
        if repository.exists() {
            fs::remove_dir_all(&repository).map_err(io_error("delete knowledge repository"))?;
            sync_directory(
                repository
                    .parent()
                    .ok_or_else(|| invalid("repository has no parent"))?,
            )?;
        }
        FileExt::unlock(&setup_lock).map_err(io_error("unlock knowledge deletion"))?;
        Ok(repository)
    }

    /// Restores active Shared Context data to the empty post-install state.
    ///
    /// A read-only dry run needs no confirmation. An actual reset requires
    /// `confirmed=true`, retains one recoverable backup, and never invokes a
    /// remote Git mutation.
    ///
    /// # Errors
    ///
    /// Returns a typed validation, maintenance, staging, journal, component
    /// smoke, or rollback error. Injected crash seams deliberately leave the
    /// journal for the next reset/setup recovery path.
    pub fn reset_data(&self, options: DataResetOptions) -> Result<DataResetReport> {
        validate_context(&self.context)?;
        if !options.dry_run && !options.confirmed {
            return Err(invalid(
                "data reset is destructive; rerun with --yes or use --dry-run",
            ));
        }
        let maintenance = MaintenanceLock::open_or_create(&self.context.root)?;
        if options.dry_run {
            let _guard = maintenance.try_shared()?;
            require_existing_installation(&self.context.root)?;
            validate_reset_active_targets(&self.context.root)?;
            let inspection =
                UserConfigStore::open_existing(&self.context.root)?.inspect_repository_catalog()?;
            let cleared_targets = existing_reset_targets(&self.context.root);
            return Ok(reset_report(
                &self.context.root,
                DataResetReportMaterial {
                    dry_run: true,
                    reset_id: None,
                    backup: None,
                    had_remote: knowledge_has_remote(&self.context.root.join("repository"))?,
                    repository_count_cleared: inspection.catalog.repositories.len(),
                    repository_group_count_cleared: inspection.catalog.repository_groups.len(),
                    cleared_targets,
                },
            ));
        }

        let _guard = maintenance.try_exclusive()?;
        let setup_lock = open_lock(&self.context.root.join("state/setup.lock"))?;
        setup_lock
            .lock_exclusive()
            .map_err(io_error("lock data reset transaction"))?;
        recover_incomplete_data_reset(&self.context.root)?;
        recover_incomplete_journals(&self.context.root)?;
        require_existing_installation(&self.context.root)?;
        let result = self.reset_data_locked();
        let result = match result {
            Ok(report) => Ok(report),
            Err(error) if is_injected_reset_crash(&error) => Err(error),
            Err(error) => match recover_incomplete_data_reset(&self.context.root) {
                Ok(_) => Err(error),
                Err(rollback) => Err(Error::new(
                    ErrorKind::InvariantViolation,
                    format!("{error}; reset rollback also failed: {rollback}"),
                )),
            },
        };
        FileExt::unlock(&setup_lock).map_err(io_error("unlock data reset transaction"))?;
        result
    }

    fn reset_data_locked(&self) -> Result<DataResetReport> {
        validate_reset_active_targets(&self.context.root)?;
        let remote_detached = knowledge_has_remote(&self.context.root.join("repository"))?;
        let inspection =
            UserConfigStore::open_existing(&self.context.root)?.inspect_repository_catalog()?;
        let repository_count = inspection.catalog.repositories.len();
        let repository_group_count = inspection.catalog.repository_groups.len();
        let id = format!("reset-{}", Uuid::new_v4());
        let backup_dir = self.context.root.join("backups").join(&id);
        ensure_private_directory(&backup_dir)?;
        let staging_root = backup_dir.join("new");
        build_pristine_reset_root(&self.context.root, &staging_root)?;
        let entries = build_reset_entries(&self.context.root, &backup_dir, &staging_root)?;
        let mut journal = DataResetJournal {
            version: RESET_JOURNAL_VERSION,
            id: id.clone(),
            root: self.context.root.clone(),
            backup_dir: backup_dir.clone(),
            staging_root,
            phase: "prepared".to_owned(),
            complete: false,
            applied_entries: 0,
            entries,
        };
        let cleared_targets = journal
            .entries
            .iter()
            .filter(|entry| entry.original_present)
            .map(|entry| entry.active.clone())
            .collect();
        persist_reset_journal(&journal, true)?;
        self.fail_reset(ResetStage::Staged)?;

        for index in 0..journal.entries.len() {
            let entry = &journal.entries[index];
            if entry.original_present {
                ensure_private_directory(
                    entry
                        .backup
                        .parent()
                        .ok_or_else(|| invalid("reset backup target has no parent"))?,
                )?;
                fs::rename(&entry.active, &entry.backup)
                    .map_err(io_error("move active reset target to backup"))?;
                sync_parent(&entry.active)?;
                sync_parent(&entry.backup)?;
            }
            if index == 0 {
                "first_original_moved".clone_into(&mut journal.phase);
                persist_reset_journal(&journal, true)?;
                self.fail_reset(ResetStage::FirstOriginalMoved)?;
            }
            if entry.staged_present {
                let staged = entry
                    .staged
                    .as_ref()
                    .ok_or_else(|| invariant("staged reset target is missing from journal"))?;
                if let Some(parent) = entry.active.parent() {
                    ensure_private_directory(parent)?;
                }
                fs::rename(staged, &entry.active)
                    .map_err(io_error("install staged reset target"))?;
                sync_parent(staged)?;
                sync_parent(&entry.active)?;
            }
            if index == 0 {
                "first_replacement_installed".clone_into(&mut journal.phase);
                persist_reset_journal(&journal, true)?;
                self.fail_reset(ResetStage::FirstReplacementInstalled)?;
            }
            journal.applied_entries = index + 1;
            journal.phase = format!("applied_{}", journal.applied_entries);
            persist_reset_journal(&journal, true)?;
        }
        "swapped".clone_into(&mut journal.phase);
        persist_reset_journal(&journal, true)?;
        self.fail_reset(ResetStage::Swapped)?;
        smoke_pristine_reset_root(&self.context.root)?;
        "smoke_tested".clone_into(&mut journal.phase);
        persist_reset_journal(&journal, true)?;
        self.fail_reset(ResetStage::SmokeTested)?;

        let report = reset_report(
            &self.context.root,
            DataResetReportMaterial {
                dry_run: false,
                reset_id: Some(id),
                backup: Some(backup_dir),
                had_remote: remote_detached,
                repository_count_cleared: repository_count,
                repository_group_count_cleared: repository_group_count,
                cleared_targets,
            },
        );
        "complete".clone_into(&mut journal.phase);
        journal.complete = true;
        persist_reset_journal(&journal, false)?;
        remove_reset_marker(&self.context.root)?;
        Ok(report)
    }

    fn fail_reset(&self, stage: ResetStage) -> Result<()> {
        if self.reset_crash_after == Some(stage) {
            Err(Error::new(
                ErrorKind::External,
                format!("injected reset crash after {stage:?}"),
            ))
        } else {
            Ok(())
        }
    }

    fn preflight(&self) -> Result<PreflightReport> {
        if self.host.platform() != "macos" {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "Shared Context setup supports macOS only, got {}",
                    self.host.platform()
                ),
            ));
        }
        let architecture = self.host.architecture().ok_or_else(|| {
            Error::new(
                ErrorKind::Unsupported,
                "unsupported CPU architecture; expected arm64 or x86_64",
            )
        })?;
        let source = fs::metadata(&self.context.runtime_source)
            .map_err(io_error("read runtime source metadata"))?;
        if !source.is_file() {
            return Err(invalid(format!(
                "runtime source is not a regular file: {}",
                self.context.runtime_source.display()
            )));
        }
        let git_version = self.host.git_version()?;
        if git_version.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::External,
                "Git version probe returned empty output",
            ));
        }
        self.host.verify_signature(&self.context.runtime_source)?;
        let required_space_bytes = self
            .context
            .minimum_free_space_bytes
            .saturating_add(source.len());
        let available_space_bytes = self.host.available_space(&self.context.root)?;
        if available_space_bytes < required_space_bytes {
            return Err(Error::new(
                ErrorKind::InvariantViolation,
                format!(
                    "insufficient disk space: need {required_space_bytes} bytes, have {available_space_bytes}"
                ),
            ));
        }
        Ok(PreflightReport {
            platform: "macos".to_owned(),
            architecture,
            git_version,
            signature_verified: true,
            available_space_bytes,
            required_space_bytes,
        })
    }

    fn capabilities(&self, options: &SetupOptions) -> Vec<sctx_agent_adapter::AgentCapabilities> {
        let mut capabilities = Vec::new();
        if options.agents.contains(&Agent::Cursor) {
            let version = self.host.agent_version(Agent::Cursor);
            capabilities.push(sctx_adapter_cursor::capabilities(
                version.as_deref(),
                hooks_available(&self.context.home, Agent::Cursor),
            ));
        }
        if options.agents.contains(&Agent::Codex) {
            let version = self.host.agent_version(Agent::Codex);
            capabilities.push(sctx_adapter_codex::capabilities(
                version.as_deref(),
                hooks_available(&self.context.home, Agent::Codex),
                self.codex_trust,
            ));
        }
        capabilities
    }

    fn fail(&self, stage: SetupStage) -> Result<()> {
        if self.fail_after == Some(stage) {
            Err(Error::new(
                ErrorKind::Io,
                format!("injected setup failure after {stage:?}"),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Setup,
    Upgrade,
}

impl Operation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Upgrade => "upgrade",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SetupJournal {
    version: u32,
    id: String,
    operation: Operation,
    phase: String,
    complete: bool,
    entries: Vec<Snapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DataResetJournal {
    version: u32,
    id: String,
    root: PathBuf,
    backup_dir: PathBuf,
    staging_root: PathBuf,
    phase: String,
    complete: bool,
    applied_entries: usize,
    entries: Vec<DataResetEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DataResetEntry {
    active: PathBuf,
    backup: PathBuf,
    staged: Option<PathBuf>,
    original_present: bool,
    staged_present: bool,
}

struct DataResetReportMaterial {
    dry_run: bool,
    reset_id: Option<String>,
    backup: Option<PathBuf>,
    had_remote: bool,
    repository_count_cleared: usize,
    repository_group_count_cleared: usize,
    cleared_targets: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Snapshot {
    path: PathBuf,
    original: Original,
    #[serde(default)]
    cleanup_empty_dirs: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Original {
    Absent,
    File {
        backup: PathBuf,
        sha256: String,
        mode: u32,
    },
    Symlink {
        target: PathBuf,
    },
}

struct Transaction {
    journal_path: PathBuf,
    backup_dir: PathBuf,
    journal: SetupJournal,
}

impl Transaction {
    fn begin(root: &Path, operation: Operation) -> Result<Self> {
        let id = format!("setup-{}", Uuid::new_v4());
        let backup_dir = root.join("backups").join(&id);
        ensure_private_directory(&backup_dir)?;
        let journal_path = backup_dir.join("journal.json");
        let mut transaction = Self {
            journal_path,
            backup_dir,
            journal: SetupJournal {
                version: JOURNAL_VERSION,
                id,
                operation,
                phase: "started".to_owned(),
                complete: false,
                entries: Vec::new(),
            },
        };
        transaction.persist()?;
        Ok(transaction)
    }

    fn record(&mut self, path: &Path) -> Result<()> {
        self.record_with_cleanup(path, Vec::new())
    }

    fn record_with_cleanup(&mut self, path: &Path, cleanup_empty_dirs: Vec<PathBuf>) -> Result<()> {
        if self.journal.entries.iter().any(|entry| entry.path == path) {
            return Ok(());
        }
        let original = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Original::Symlink {
                target: fs::read_link(path).map_err(io_error("read original symlink"))?,
            },
            Ok(metadata) if metadata.is_file() => {
                let bytes = fs::read(path).map_err(io_error("back up original file"))?;
                let backup = self
                    .backup_dir
                    .join(format!("file-{}.bin", self.journal.entries.len()));
                atomic_write(&backup, &bytes, 0o600)?;
                Original::File {
                    backup,
                    sha256: sha256(&bytes),
                    mode: metadata.permissions().mode() & 0o7777,
                }
            }
            Ok(_) => {
                return Err(invalid(format!(
                    "transaction target is neither file nor symlink: {}",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Original::Absent,
            Err(error) => return Err(io_error("inspect transaction target")(error)),
        };
        self.journal.entries.push(Snapshot {
            path: path.to_path_buf(),
            original,
            cleanup_empty_dirs,
        });
        self.persist()
    }

    fn phase(&mut self, phase: impl Into<String>) -> Result<()> {
        self.journal.phase = phase.into();
        self.persist()
    }

    fn original(&self, path: &Path) -> Option<&Original> {
        self.journal
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| &entry.original)
    }

    fn complete(&mut self) -> Result<()> {
        "complete".clone_into(&mut self.journal.phase);
        self.journal.complete = true;
        self.persist()
    }

    fn rollback(&mut self) -> Result<()> {
        restore_journal(&self.journal)?;
        "rolled_back".clone_into(&mut self.journal.phase);
        self.journal.complete = true;
        self.persist()
    }

    fn persist(&mut self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.journal)
            .map_err(|error| io_value("serialize setup journal", error))?;
        atomic_write(&self.journal_path, &bytes, 0o600)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstallManifest {
    version: u32,
    installed_version: String,
    architecture: Architecture,
    #[serde(default)]
    installation_id: String,
    #[serde(default)]
    knowledge_store: KnowledgeStoreSource,
    configs: Vec<OwnedConfig>,
    #[serde(default)]
    skills: Vec<OwnedSkill>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum KnowledgeStoreSource {
    #[default]
    Local,
    Remote {
        remote_type: KnowledgeRemoteType,
        default_branch: String,
        work_branch: String,
        url_digest: String,
    },
}

impl KnowledgeStoreSource {
    fn report(&self, installation_id: &str) -> KnowledgeStoreReport {
        match self {
            Self::Local => KnowledgeStoreReport {
                installation_id: installation_id.to_owned(),
                source: "local".to_owned(),
                remote_type: None,
                default_branch: "main".to_owned(),
                work_branch: None,
                url_digest: None,
            },
            Self::Remote {
                remote_type,
                default_branch,
                work_branch,
                url_digest,
            } => KnowledgeStoreReport {
                installation_id: installation_id.to_owned(),
                source: "remote".to_owned(),
                remote_type: Some(*remote_type),
                default_branch: default_branch.clone(),
                work_branch: Some(work_branch.clone()),
                url_digest: Some(url_digest.clone()),
            },
        }
    }
}

struct KnowledgeStoreInstall {
    store: GitStore,
    source: KnowledgeStoreSource,
    changed: bool,
}

fn installation_id(prior: Option<&InstallManifest>) -> Result<String> {
    let Some(existing) = prior
        .map(|manifest| manifest.installation_id.as_str())
        .filter(|value| !value.is_empty())
    else {
        return Ok(Uuid::new_v4().to_string());
    };
    Uuid::parse_str(existing)
        .map_err(|_| invalid("install manifest contains an invalid installation ID"))?;
    Ok(existing.to_owned())
}

fn install_knowledge_store(
    root: &Path,
    requested_url: Option<&KnowledgeStoreUrl>,
    prior: Option<&InstallManifest>,
    installation_id: &str,
    transaction: &mut Transaction,
) -> Result<KnowledgeStoreInstall> {
    let repository = root.join("repository");
    let prior_source = prior.map(|manifest| &manifest.knowledge_store);
    match prior_source {
        Some(KnowledgeStoreSource::Remote {
            remote_type,
            default_branch,
            work_branch,
            url_digest,
        }) => {
            if let Some(url) = requested_url
                && url.digest() != *url_digest
            {
                return Err(invalid(
                    "Knowledge Store URL does not match the installed remote",
                ));
            }
            let source = KnowledgeStoreSource::Remote {
                remote_type: *remote_type,
                default_branch: default_branch.clone(),
                work_branch: work_branch.clone(),
                url_digest: url_digest.clone(),
            };
            if fs::symlink_metadata(&repository).is_ok() {
                let store = GitStore::open_existing(root)?;
                verify_remote_store(&store, default_branch, work_branch, url_digest)?;
                return Ok(KnowledgeStoreInstall {
                    store,
                    source,
                    changed: false,
                });
            }
            let url = requested_url.ok_or_else(|| {
                invalid(
                    "remote Knowledge Store is missing; rerun setup with the original --knowledge-store-url",
                )
            })?;
            clone_remote_store(root, url, installation_id, transaction)
        }
        Some(KnowledgeStoreSource::Local) => {
            if requested_url.is_some() {
                return Err(invalid(
                    "cannot replace an installed local Knowledge Store with a remote URL",
                ));
            }
            let existed = repository.exists();
            Ok(KnowledgeStoreInstall {
                store: GitStore::bootstrap_local(root)?,
                source: KnowledgeStoreSource::Local,
                changed: !existed,
            })
        }
        None => {
            if let Some(url) = requested_url {
                if fs::symlink_metadata(&repository).is_ok() {
                    return Err(invalid(
                        "cannot replace an existing Knowledge Store with a remote URL",
                    ));
                }
                clone_remote_store(root, url, installation_id, transaction)
            } else {
                let existed = repository.exists();
                Ok(KnowledgeStoreInstall {
                    store: GitStore::bootstrap_local(root)?,
                    source: KnowledgeStoreSource::Local,
                    changed: !existed,
                })
            }
        }
    }
}

fn clone_remote_store(
    root: &Path,
    url: &KnowledgeStoreUrl,
    installation_id: &str,
    transaction: &mut Transaction,
) -> Result<KnowledgeStoreInstall> {
    UserConfigStore::initialize(root)?;
    let work_branch = format!("shared-context/{installation_id}");
    let staging_root = transaction.backup_dir.join("remote-bootstrap");
    let (staged_store, bootstrap) =
        GitStore::bootstrap_remote(&staging_root, url.as_str(), &work_branch)?;
    staged_store.validate_committed_objects()?;
    staged_store.validate_committed_events()?;
    let snapshot = ProjectionIndex::for_store(&staged_store).domain_snapshot()?;
    if !snapshot.projection.quarantined_event_ids.is_empty() {
        return Err(Error::new(
            ErrorKind::InvariantViolation,
            "remote Knowledge Store contains quarantined Events",
        ));
    }

    ensure_private_directory(&root.join("state/pending"))?;

    let repository = root.join("repository");
    if fs::symlink_metadata(&repository).is_ok() {
        return Err(invalid(
            "cannot replace an existing Knowledge Store with a remote URL",
        ));
    }
    transaction.record(&repository)?;
    fs::rename(staged_store.repository(), &repository)
        .map_err(io_error("atomically install remote Knowledge Store"))?;
    sync_directory(root)?;
    transaction.phase("remote_repository_installed")?;

    let source = KnowledgeStoreSource::Remote {
        remote_type: url.remote_type(),
        default_branch: bootstrap.default_branch,
        work_branch: bootstrap.work_branch,
        url_digest: url.digest(),
    };
    let store = GitStore::open_existing(root)?;
    if let KnowledgeStoreSource::Remote {
        default_branch,
        work_branch,
        url_digest,
        ..
    } = &source
    {
        verify_remote_store(&store, default_branch, work_branch, url_digest)?;
    }
    Ok(KnowledgeStoreInstall {
        store,
        source,
        changed: true,
    })
}

fn verify_remote_store(
    store: &GitStore,
    default_branch: &str,
    work_branch: &str,
    expected_url_digest: &str,
) -> Result<()> {
    let repository = store.repository();
    let current_branch = git_output(
        repository,
        &["symbolic-ref", "--short", "HEAD"],
        "inspect Knowledge Store branch",
    )?;
    if current_branch != work_branch {
        return Err(invalid(
            "remote Knowledge Store is not on its installation work branch",
        ));
    }
    let remote_url = git_output(
        repository,
        &["config", "--get", "remote.origin.url"],
        "inspect Knowledge Store origin",
    )?;
    if sha256(remote_url.as_bytes()) != expected_url_digest {
        return Err(invalid(
            "remote Knowledge Store origin does not match the install manifest",
        ));
    }
    let default_ref = format!("refs/remotes/origin/{default_branch}");
    git_output(
        repository,
        &["rev-parse", "--verify", &default_ref],
        "verify remote default branch",
    )?;
    let status = git_output(
        repository,
        &["status", "--porcelain", "--untracked-files=all"],
        "inspect Knowledge Store status",
    )?;
    if !status.is_empty() {
        return Err(invalid("remote Knowledge Store worktree is not clean"));
    }
    Ok(())
}

fn git_output(repository: &Path, args: &[&str], context: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(|error| Error::new(ErrorKind::External, format!("{context}: {error}")))?;
    if !output.status.success() {
        return Err(Error::new(
            ErrorKind::External,
            format!("{context} failed with {}", output.status),
        ));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| {
            Error::new(
                ErrorKind::External,
                format!("{context} returned non-UTF-8 output"),
            )
        })
}

fn validate_knowledge_store_url(value: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value {
        return Err(invalid(
            "Knowledge Store URL must be non-empty and have no surrounding whitespace",
        ));
    }
    if value.starts_with('-') || value.chars().any(char::is_control) {
        return Err(invalid("Knowledge Store URL contains unsafe characters"));
    }
    if value.contains(['?', '#']) {
        return Err(invalid(
            "Knowledge Store URL must not contain query parameters or fragments",
        ));
    }
    if let Some((scheme, remainder)) = value.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "file" | "http" | "https" | "ssh" | "git"
        ) {
            return Err(invalid("unsupported Knowledge Store URL scheme"));
        }
        let authority = remainder.split('/').next().unwrap_or_default();
        if authority.is_empty() && !scheme.eq_ignore_ascii_case("file") {
            return Err(invalid("Knowledge Store URL has no remote host"));
        }
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            let ssh_username_only = scheme.eq_ignore_ascii_case("ssh")
                && !userinfo.is_empty()
                && !userinfo.contains(':');
            if !ssh_username_only {
                return Err(invalid(
                    "Knowledge Store URL must not contain embedded credentials",
                ));
            }
        }
    } else if let Some((userinfo, tail)) = value.split_once('@')
        && tail.contains(':')
        && (userinfo.is_empty() || userinfo.contains(':'))
    {
        return Err(invalid(
            "Knowledge Store URL must not contain embedded credentials",
        ));
    }
    Ok(())
}

fn classify_remote_type(value: &str) -> KnowledgeRemoteType {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("https://") {
        KnowledgeRemoteType::Https
    } else if lower.starts_with("http://") {
        KnowledgeRemoteType::Http
    } else if lower.starts_with("ssh://")
        || value
            .split_once('@')
            .is_some_and(|(_, tail)| tail.contains(':'))
    {
        KnowledgeRemoteType::Ssh
    } else if lower.starts_with("git://") {
        KnowledgeRemoteType::Git
    } else {
        KnowledgeRemoteType::Local
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnedSkill {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigKind {
    CursorMcp,
    CursorHooks,
    CodexMcp,
    CodexHooks,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnedConfig {
    path: PathBuf,
    kind: ConfigKind,
    originally_absent: bool,
    entries: Vec<OwnedEntry>,
    #[serde(default)]
    baseline: Option<Original>,
    #[serde(default)]
    installed_file_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnedEntry {
    selector: String,
    hash: String,
    original_index: Option<usize>,
}

struct SkillInstall {
    changed: bool,
    status: SkillStatus,
    ownership: Vec<OwnedSkill>,
}

#[derive(Default)]
struct SkillAssetInstall {
    changed: bool,
}

fn global_skill_root(home: &Path) -> PathBuf {
    home.join(GLOBAL_SKILL_DIRECTORY)
}

fn global_skill_assets(home: &Path) -> Vec<(PathBuf, &'static [u8])> {
    let root = global_skill_root(home);
    SKILL_ASSETS
        .iter()
        .map(|(relative, bytes)| (root.join(relative), *bytes))
        .collect()
}

fn absent_parent_directories(path: &Path, stop_at: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == stop_at {
            break;
        }
        match fs::symlink_metadata(directory) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                directories.push(directory.to_path_buf());
                current = directory.parent();
            }
            Err(error) => {
                return Err(io_error("inspect global Agent Skill parent for rollback")(
                    error,
                ));
            }
        }
    }
    Ok(directories)
}

fn install_global_skill(
    transaction: &mut Transaction,
    home: &Path,
    prior_ownership: &[OwnedSkill],
    notices: &mut Vec<String>,
    fail_after: Option<SetupStage>,
) -> Result<SkillInstall> {
    let root = global_skill_root(home);
    let assets = global_skill_assets(home);
    let expected_paths = assets.iter().map(|(path, _)| path).collect::<BTreeSet<_>>();
    let mut ownership = prior_ownership.to_vec();

    for owned in prior_ownership {
        if !expected_paths.contains(&owned.path) {
            notices.push(format!(
                "preserved unexpected global Agent Skill ownership path: {}",
                owned.path.display()
            ));
        }
    }

    let owns_expected_file = prior_ownership
        .iter()
        .any(|owned| expected_paths.contains(&owned.path));
    if let Some(status) = global_skill_location_conflict(home, &root, owns_expected_file, notices)?
    {
        return Ok(SkillInstall {
            changed: false,
            status,
            ownership,
        });
    }
    if let Some(status) = global_skill_bundle_conflict(&assets, prior_ownership, notices)? {
        return Ok(SkillInstall {
            changed: false,
            status,
            ownership,
        });
    }

    let mut aggregate = SkillAssetInstall::default();
    for (path, desired) in assets {
        let prior = prior_ownership.iter().find(|owned| owned.path == path);
        let stage = global_skill_asset_stage(&path)?;
        let result =
            install_global_skill_asset(transaction, home, path, desired, prior, &mut ownership)?;
        aggregate.changed |= result.changed;
        if fail_after == Some(stage) {
            return Err(Error::new(
                ErrorKind::Io,
                format!("injected setup failure after {stage:?}"),
            ));
        }
    }

    let status = if aggregate.changed {
        SkillStatus::Installed
    } else {
        SkillStatus::Current
    };
    Ok(SkillInstall {
        changed: aggregate.changed,
        status,
        ownership,
    })
}

fn global_skill_asset_stage(path: &Path) -> Result<SetupStage> {
    if path.ends_with("SKILL.md") {
        Ok(SetupStage::GlobalSkillGateWritten)
    } else if path.ends_with("references/workflow.md") {
        Ok(SetupStage::GlobalSkillWorkflowWritten)
    } else if path.ends_with("agents/openai.yaml") {
        Ok(SetupStage::GlobalSkillMetadataWritten)
    } else {
        Err(Error::new(
            ErrorKind::InvariantViolation,
            format!("unexpected global Agent Skill asset: {}", path.display()),
        ))
    }
}

fn global_skill_bundle_conflict(
    assets: &[(PathBuf, &'static [u8])],
    prior_ownership: &[OwnedSkill],
    notices: &mut Vec<String>,
) -> Result<Option<SkillStatus>> {
    let mut modified = false;
    let mut conflict = false;
    for (path, _) in assets {
        let prior = prior_ownership.iter().find(|owned| owned.path == *path);
        if let Some(parent) = path.parent() {
            match fs::symlink_metadata(parent) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => {
                    notices.push(format!(
                        "preserved global Agent Skill bundle because an asset parent is not a non-symlink directory: {}",
                        parent.display()
                    ));
                    modified |= prior.is_some();
                    conflict |= prior.is_none();
                    continue;
                }
                Err(error) => {
                    return Err(io_error("inspect global Agent Skill bundle parent")(error));
                }
            }
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let current =
                    fs::read(path).map_err(io_error("read global Agent Skill bundle asset"))?;
                match prior {
                    Some(owned) if sha256(&current) == owned.sha256 => {}
                    Some(_) => {
                        notices.push(format!(
                            "preserved user-modified global Agent Skill file: {}",
                            path.display()
                        ));
                        modified = true;
                    }
                    None => {
                        notices.push(format!(
                            "preserved user-owned global Agent Skill file: {}; Shared Context did not overwrite or claim it",
                            path.display()
                        ));
                        conflict = true;
                    }
                }
            }
            Ok(_) => {
                notices.push(format!(
                    "preserved global Agent Skill path because it is not a regular file: {}",
                    path.display()
                ));
                modified |= prior.is_some();
                conflict |= prior.is_none();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect global Agent Skill bundle asset")(error)),
        }
    }
    if modified {
        Ok(Some(SkillStatus::Modified))
    } else if conflict {
        Ok(Some(SkillStatus::Conflict))
    } else {
        Ok(None)
    }
}

fn global_skill_location_conflict(
    home: &Path,
    root: &Path,
    owns_expected_file: bool,
    notices: &mut Vec<String>,
) -> Result<Option<SkillStatus>> {
    match fs::symlink_metadata(root) {
        Ok(_) if !owns_expected_file => {
            notices.push(format!(
                "preserved user-owned global Agent Skill at {}; Shared Context did not overwrite or claim it",
                root.display()
            ));
            return Ok(Some(SkillStatus::Conflict));
        }
        Ok(metadata)
            if owns_expected_file && (!metadata.is_dir() || metadata.file_type().is_symlink()) =>
        {
            notices.push(format!(
                "preserved managed global Agent Skill because its directory is no longer a non-symlink directory: {}",
                root.display()
            ));
            return Ok(Some(SkillStatus::Modified));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspect global Agent Skill")(error)),
        _ => {}
    }
    for ancestor in [home.join(".agents"), home.join(".agents/skills")] {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                notices.push(format!(
                    "preserved global Agent Skill because its parent is not a non-symlink directory: {}",
                    ancestor.display()
                ));
                return Ok(Some(SkillStatus::Conflict));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect global Agent Skill parent")(error)),
        }
    }
    Ok(None)
}

fn install_global_skill_asset(
    transaction: &mut Transaction,
    home: &Path,
    path: PathBuf,
    desired: &'static [u8],
    prior: Option<&OwnedSkill>,
    ownership: &mut Vec<OwnedSkill>,
) -> Result<SkillAssetInstall> {
    if let Some(parent) = path.parent() {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(Error::new(
                    ErrorKind::InvariantViolation,
                    format!(
                        "global Agent Skill bundle parent changed after preflight: {}",
                        parent.display()
                    ),
                ));
            }
            Err(error) => return Err(io_error("inspect global Agent Skill file parent")(error)),
        }
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            install_existing_skill_asset(transaction, path, desired, prior, ownership)
        }
        Ok(_) => Err(Error::new(
            ErrorKind::InvariantViolation,
            format!(
                "global Agent Skill bundle asset changed after preflight: {}",
                path.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let cleanup_empty_dirs = absent_parent_directories(&path, home)?;
            transaction.record_with_cleanup(&path, cleanup_empty_dirs)?;
            atomic_write(&path, desired, 0o644)?;
            transaction.phase("global_skill_installed")?;
            upsert_owned_skill(ownership, path, sha256(desired));
            Ok(SkillAssetInstall { changed: true })
        }
        Err(error) => Err(io_error("inspect global Agent Skill file")(error)),
    }
}

fn install_existing_skill_asset(
    transaction: &mut Transaction,
    path: PathBuf,
    desired: &'static [u8],
    prior: Option<&OwnedSkill>,
    ownership: &mut Vec<OwnedSkill>,
) -> Result<SkillAssetInstall> {
    let current = fs::read(&path).map_err(io_error("read global Agent Skill file"))?;
    match prior {
        Some(owned) if sha256(&current) == owned.sha256 => {
            let changed = current != desired;
            if changed {
                transaction.record(&path)?;
                atomic_write(&path, desired, existing_mode(&path, 0o644)?)?;
                transaction.phase("global_skill_updated")?;
            }
            upsert_owned_skill(ownership, path, sha256(desired));
            Ok(SkillAssetInstall { changed })
        }
        Some(_) | None => Err(Error::new(
            ErrorKind::InvariantViolation,
            format!(
                "global Agent Skill bundle asset changed after preflight: {}",
                path.display()
            ),
        )),
    }
}

fn upsert_owned_skill(ownership: &mut Vec<OwnedSkill>, path: PathBuf, hash: String) {
    if let Some(owned) = ownership.iter_mut().find(|owned| owned.path == path) {
        owned.sha256 = hash;
    } else {
        ownership.push(OwnedSkill { path, sha256: hash });
    }
    ownership.sort_by(|left, right| left.path.cmp(&right.path));
}

fn install_runtime(transaction: &mut Transaction, source: &Path, target: &Path) -> Result<bool> {
    let source_bytes = fs::read(source).map_err(io_error("read runtime source"))?;
    if target.exists() {
        let target_bytes = fs::read(target).map_err(io_error("read installed runtime"))?;
        if target_bytes != source_bytes {
            return Err(Error::new(
                ErrorKind::InvariantViolation,
                format!(
                    "runtime version directory already contains different bytes: {}",
                    target.display()
                ),
            ));
        }
        if existing_mode(target, 0o755)? != 0o755 {
            transaction.record(target)?;
            set_mode(target, 0o755)?;
            transaction.phase("runtime_permissions_repaired")?;
            return Ok(true);
        }
        return Ok(false);
    }
    transaction.record(target)?;
    atomic_write(target, &source_bytes, 0o755)?;
    transaction.phase("runtime_installed")?;
    Ok(true)
}

fn switch_current(transaction: &mut Transaction, current: &Path, target: &Path) -> Result<bool> {
    if fs::read_link(current).ok().as_deref() == Some(target) {
        return Ok(false);
    }
    transaction.record(current)?;
    let parent = current
        .parent()
        .ok_or_else(|| invalid("current link has no parent"))?;
    let temporary = parent.join(format!(".current-{}.tmp", Uuid::new_v4()));
    symlink(target, &temporary).map_err(io_error("create current runtime symlink"))?;
    fs::rename(&temporary, current).map_err(io_error("atomically switch current runtime"))?;
    sync_directory(parent)?;
    transaction.phase("current_switched")?;
    Ok(true)
}

fn merge_cursor_mcp(
    transaction: &mut Transaction,
    path: &Path,
    binary: &Path,
    ownership: &mut Vec<OwnedConfig>,
    notices: &mut Vec<String>,
) -> Result<bool> {
    let desired = json!({
        "type": "stdio",
        "command": path_text(binary)?,
        "args": ["mcp", "serve", "--client", "cursor"]
    });
    merge_json_named_entry(
        transaction,
        path,
        ConfigKind::CursorMcp,
        "mcpServers",
        PRODUCT_KEY,
        &desired,
        ownership,
        notices,
    )
}

#[allow(clippy::too_many_arguments)]
fn merge_json_named_entry(
    transaction: &mut Transaction,
    path: &Path,
    kind: ConfigKind,
    container: &str,
    key: &str,
    desired: &Value,
    ownership: &mut Vec<OwnedConfig>,
    notices: &mut Vec<String>,
) -> Result<bool> {
    let _lock = ConfigLock::acquire(path)?;
    let absent = !path.exists();
    let original = read_json_object(path)?;
    let mut document = original.clone();
    let table = object_field_mut(&mut document, container)?;
    let desired_hash = hash_value(desired)?;
    let prior = find_owned(ownership, kind, key).cloned();
    let changed = match table.get(key) {
        None => {
            table.insert(key.to_owned(), desired.clone());
            true
        }
        Some(current) if current == desired => false,
        Some(current)
            if prior
                .as_ref()
                .is_some_and(|entry| hash_value(current).ok().as_deref() == Some(&entry.hash)) =>
        {
            table.insert(key.to_owned(), desired.clone());
            true
        }
        Some(_) => {
            notices.push(format!(
                "preserved user-owned {key} entry in {}; Shared Context did not overwrite it",
                path.display()
            ));
            upsert_owned_config(ownership, path, kind, absent, Vec::new());
            return Ok(false);
        }
    };
    if changed {
        transaction.record(path)?;
        write_json(path, &document)?;
        transaction.phase(format!("wrote_{}", config_kind_name(kind)))?;
    }
    upsert_owned_config(
        ownership,
        path,
        kind,
        prior_originally_absent(ownership, kind, absent),
        vec![OwnedEntry {
            selector: key.to_owned(),
            hash: desired_hash,
            original_index: None,
        }],
    );
    Ok(changed)
}

fn merge_json_hooks(
    transaction: &mut Transaction,
    path: &Path,
    agent: Agent,
    binary: &Path,
    ownership: &mut Vec<OwnedConfig>,
    notices: &mut Vec<String>,
) -> Result<bool> {
    let kind = match agent {
        Agent::Cursor => ConfigKind::CursorHooks,
        Agent::Codex => ConfigKind::CodexHooks,
    };
    let events = match agent {
        Agent::Cursor => &HOOK_EVENTS_CURSOR[..],
        Agent::Codex => &HOOK_EVENTS_CODEX[..],
    };
    let _lock = ConfigLock::acquire(path)?;
    let absent = !path.exists();
    let original = read_json_object(path)?;
    let mut document = original.clone();
    if agent == Agent::Cursor && document.get("version").is_none() {
        document.insert("version".to_owned(), Value::from(1));
    }
    let mut changed = document != original;
    let hooks = object_field_mut(&mut document, "hooks")?;
    let command = format!(
        "{} hook --agent {}",
        shell_quote(binary)?,
        agent_name(agent)
    );
    let desired = match agent {
        Agent::Cursor => json!({"command": command}),
        Agent::Codex => json!({
            "hooks": [{"type": "command", "command": command, "statusMessage": "Shared Context"}]
        }),
    };
    let desired_hash = hash_value(&desired)?;
    let prior_entries = ownership
        .iter()
        .find(|config| config.kind == kind)
        .map_or_else(Vec::new, |config| config.entries.clone());
    let mut entries = Vec::new();
    for event in events {
        let item = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let array = item
            .as_array_mut()
            .ok_or_else(|| invalid(format!("{}.hooks.{event} must be an array", path.display())))?;
        if let Some(index) = array
            .iter()
            .position(|entry| hash_value(entry).ok().as_deref() == Some(&desired_hash))
        {
            entries.push(OwnedEntry {
                selector: (*event).to_owned(),
                hash: desired_hash.clone(),
                original_index: Some(index),
            });
            continue;
        }
        let prior = prior_entries.iter().find(|entry| entry.selector == *event);
        if let Some(index) = prior.and_then(|entry| entry.original_index) {
            if index < array.len() {
                notices.push(format!(
                    "preserved user-modified {event} hook at index {index} in {}",
                    path.display()
                ));
                entries.push(prior.cloned().expect("prior entry exists"));
                continue;
            }
        }
        let index = array.len();
        array.push(desired.clone());
        changed = true;
        entries.push(OwnedEntry {
            selector: (*event).to_owned(),
            hash: desired_hash.clone(),
            original_index: Some(index),
        });
    }
    if changed {
        transaction.record(path)?;
        write_json(path, &document)?;
        transaction.phase(format!("wrote_{}", config_kind_name(kind)))?;
    }
    let originally_absent = prior_originally_absent(ownership, kind, absent);
    upsert_owned_config(ownership, path, kind, originally_absent, entries);
    Ok(changed)
}

fn merge_codex_mcp(
    transaction: &mut Transaction,
    path: &Path,
    binary: &Path,
    ownership: &mut Vec<OwnedConfig>,
    notices: &mut Vec<String>,
) -> Result<bool> {
    let _lock = ConfigLock::acquire(path)?;
    let absent = !path.exists();
    let text = read_utf8_or_empty(path)?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| invalid(format!("invalid Codex TOML in {}: {error}", path.display())))?;
    if document.get("mcp_servers").is_none() {
        document.insert("mcp_servers", Item::Table(Table::new()));
    }
    let servers = document["mcp_servers"]
        .as_table_mut()
        .ok_or_else(|| invalid("Codex mcp_servers must be a table"))?;
    let mut desired_table = Table::new();
    desired_table["command"] = value(path_text(binary)?);
    let mut args = Array::new();
    args.extend(["mcp", "serve", "--client", "codex"]);
    desired_table["args"] = value(args);
    let desired = Item::Table(desired_table);
    let desired_hash = sha256(desired.to_string().as_bytes());
    let prior = find_owned(ownership, ConfigKind::CodexMcp, PRODUCT_KEY).cloned();
    let changed = match servers.get(PRODUCT_KEY) {
        None => {
            servers.insert(PRODUCT_KEY, desired.clone());
            true
        }
        Some(current) if current.to_string() == desired.to_string() => false,
        Some(current)
            if prior
                .as_ref()
                .is_some_and(|entry| sha256(current.to_string().as_bytes()) == entry.hash) =>
        {
            servers.insert(PRODUCT_KEY, desired.clone());
            true
        }
        Some(_) => {
            notices.push(format!(
                "preserved user-owned [mcp_servers.{PRODUCT_KEY}] in {}",
                path.display()
            ));
            upsert_owned_config(ownership, path, ConfigKind::CodexMcp, absent, Vec::new());
            return Ok(false);
        }
    };
    if changed {
        transaction.record(path)?;
        atomic_write(
            path,
            document.to_string().as_bytes(),
            existing_mode(path, 0o600)?,
        )?;
        parse_toml_file(path)?;
        transaction.phase("wrote_codex_mcp")?;
    }
    let originally_absent = prior_originally_absent(ownership, ConfigKind::CodexMcp, absent);
    upsert_owned_config(
        ownership,
        path,
        ConfigKind::CodexMcp,
        originally_absent,
        vec![OwnedEntry {
            selector: PRODUCT_KEY.to_owned(),
            hash: desired_hash,
            original_index: None,
        }],
    );
    Ok(changed)
}

fn uninstall_config(config: &OwnedConfig, report: &mut UninstallReport) -> Result<()> {
    if !config.path.exists() {
        return Ok(());
    }
    let _lock = ConfigLock::acquire(&config.path)?;
    if restore_unchanged_config(config, report)? {
        return Ok(());
    }
    match config.kind {
        ConfigKind::CursorMcp => uninstall_json_named(config, "mcpServers", report),
        ConfigKind::CursorHooks | ConfigKind::CodexHooks => uninstall_json_hooks(config, report),
        ConfigKind::CodexMcp => uninstall_codex_mcp(config, report),
    }
}

fn uninstall_json_named(
    config: &OwnedConfig,
    container: &str,
    report: &mut UninstallReport,
) -> Result<()> {
    let mut document = read_json_object(&config.path)?;
    let Some(table) = document.get_mut(container).and_then(Value::as_object_mut) else {
        preserve_modified(config, report, "owned JSON container is missing");
        return Ok(());
    };
    let Some(owned) = config.entries.first() else {
        preserve_modified(config, report, "entry was not installer-owned");
        return Ok(());
    };
    match table.get(&owned.selector) {
        Some(current) if hash_value(current)? == owned.hash => {
            table.remove(&owned.selector);
        }
        Some(_) => {
            preserve_modified(config, report, "owned entry was modified after setup");
            return Ok(());
        }
        None => return Ok(()),
    }
    if table.is_empty() {
        document.remove(container);
    }
    finish_json_uninstall(config, &document, report)
}

fn uninstall_json_hooks(config: &OwnedConfig, report: &mut UninstallReport) -> Result<()> {
    let mut document = read_json_object(&config.path)?;
    let Some(hooks) = document.get_mut("hooks").and_then(Value::as_object_mut) else {
        preserve_modified(config, report, "hooks object is missing");
        return Ok(());
    };
    let mut modified = false;
    for owned in &config.entries {
        let Some(array) = hooks.get_mut(&owned.selector).and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(index) = array
            .iter()
            .position(|entry| hash_value(entry).ok().as_deref() == Some(&owned.hash))
        {
            array.remove(index);
        } else if owned
            .original_index
            .is_some_and(|index| index < array.len())
        {
            modified = true;
        }
        if array.is_empty() {
            hooks.remove(&owned.selector);
        }
    }
    if hooks.is_empty() {
        document.remove("hooks");
    }
    if modified {
        preserve_modified(
            config,
            report,
            "one or more hook entries were modified after setup",
        );
    }
    finish_json_uninstall(config, &document, report)
}

fn finish_json_uninstall(
    config: &OwnedConfig,
    document: &Map<String, Value>,
    report: &mut UninstallReport,
) -> Result<()> {
    let only_cursor_version = config.kind == ConfigKind::CursorHooks
        && document.len() == 1
        && document.get("version") == Some(&Value::from(1));
    if config.originally_absent && (document.is_empty() || only_cursor_version) {
        fs::remove_file(&config.path).map_err(io_error("remove installer-created JSON config"))?;
        report.removed.push(config.path.clone());
    } else {
        write_json(&config.path, document)?;
        report.preserved.push(config.path.clone());
    }
    Ok(())
}

fn uninstall_codex_mcp(config: &OwnedConfig, report: &mut UninstallReport) -> Result<()> {
    let text = read_utf8_or_empty(&config.path)?;
    let mut document = text.parse::<DocumentMut>().map_err(|error| {
        invalid(format!(
            "invalid Codex TOML in {}: {error}",
            config.path.display()
        ))
    })?;
    let Some(servers) = document.get_mut("mcp_servers").and_then(Item::as_table_mut) else {
        preserve_modified(config, report, "mcp_servers table is missing");
        return Ok(());
    };
    let Some(owned) = config.entries.first() else {
        preserve_modified(config, report, "MCP entry was not installer-owned");
        return Ok(());
    };
    match servers.get(PRODUCT_KEY) {
        Some(item) if sha256(item.to_string().as_bytes()) == owned.hash => {
            servers.remove(PRODUCT_KEY);
        }
        Some(_) => {
            preserve_modified(config, report, "Codex MCP entry was modified after setup");
            return Ok(());
        }
        None => return Ok(()),
    }
    if servers.is_empty() {
        document.remove("mcp_servers");
    }
    if config.originally_absent && document.as_table().is_empty() {
        fs::remove_file(&config.path).map_err(io_error("remove installer-created Codex config"))?;
        report.removed.push(config.path.clone());
    } else {
        atomic_write(
            &config.path,
            document.to_string().as_bytes(),
            existing_mode(&config.path, 0o600)?,
        )?;
        report.preserved.push(config.path.clone());
    }
    Ok(())
}

fn preserve_modified(config: &OwnedConfig, report: &mut UninstallReport, reason: &str) {
    report.preserved.push(config.path.clone());
    report
        .warnings
        .push(format!("preserved {}: {reason}", config.path.display()));
}

fn is_expected_skill_path(home: &Path, path: &Path) -> bool {
    global_skill_assets(home)
        .iter()
        .any(|(expected, _)| expected == path)
}

fn skill_parent_directories_are_safe(home: &Path, path: &Path) -> Result<bool> {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == home {
            return Ok(true);
        }
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect global Agent Skill parent")(error)),
        }
        current = directory.parent();
    }
    Ok(false)
}

fn global_skill_directory_is_safe(home: &Path, directory: &Path) -> Result<bool> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            skill_parent_directories_are_safe(home, &directory.join(".sctx-prune-check"))
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(io_error("inspect global Agent Skill directory")(error)),
    }
}

fn uninstall_owned_skill(
    home: &Path,
    skill: &OwnedSkill,
    report: &mut UninstallReport,
) -> Result<()> {
    if !skill_parent_directories_are_safe(home, &skill.path)? {
        report.preserved.push(skill.path.clone());
        report.warnings.push(format!(
            "preserved global Agent Skill file because a parent is not a non-symlink directory: {}",
            skill.path.display()
        ));
        return Ok(());
    }
    match fs::symlink_metadata(&skill.path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let bytes = fs::read(&skill.path).map_err(io_error("read global Agent Skill file"))?;
            if sha256(&bytes) == skill.sha256 {
                fs::remove_file(&skill.path)
                    .map_err(io_error("remove installer-owned global Agent Skill file"))?;
                report.removed.push(skill.path.clone());
            } else {
                report.preserved.push(skill.path.clone());
                report.warnings.push(format!(
                    "preserved user-modified global Agent Skill file: {}",
                    skill.path.display()
                ));
            }
        }
        Ok(_) => {
            report.preserved.push(skill.path.clone());
            report.warnings.push(format!(
                "preserved global Agent Skill path because it is not a regular file: {}",
                skill.path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io_error("inspect global Agent Skill file for uninstall")(
                error,
            ));
        }
    }
    Ok(())
}

fn write_manifest(
    transaction: &mut Transaction,
    root: &Path,
    manifest: &InstallManifest,
) -> Result<bool> {
    let path = manifest_path(root);
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| io_value("serialize install manifest", error))?;
    if fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(false);
    }
    transaction.record(&path)?;
    atomic_write(&path, &bytes, 0o600)?;
    transaction.phase("manifest_written")?;
    Ok(true)
}

fn read_manifest(root: &Path) -> Result<Option<InstallManifest>> {
    let path = manifest_path(root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read install manifest")(error)),
    };
    let manifest: InstallManifest = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid install manifest: {error}")))?;
    if manifest.version != MANIFEST_VERSION {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("unsupported install manifest version {}", manifest.version),
        ));
    }
    Ok(Some(manifest))
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("state/install-manifest.json")
}

fn upsert_owned_config(
    configs: &mut Vec<OwnedConfig>,
    path: &Path,
    kind: ConfigKind,
    originally_absent: bool,
    entries: Vec<OwnedEntry>,
) {
    if let Some(config) = configs.iter_mut().find(|config| config.kind == kind) {
        config.path = path.to_path_buf();
        config.entries = entries;
    } else {
        configs.push(OwnedConfig {
            path: path.to_path_buf(),
            kind,
            originally_absent,
            entries,
            baseline: None,
            installed_file_hash: None,
        });
    }
    configs.sort_by_key(|config| config_kind_name(config.kind));
}

fn finalize_ownership(transaction: &mut Transaction, configs: &mut [OwnedConfig]) -> Result<()> {
    for config in configs {
        let prior_installed_hash = config.installed_file_hash.clone();
        if config.baseline.is_none() {
            transaction.record(&config.path)?;
            config.baseline = transaction.original(&config.path).cloned();
        }
        let current = fs::read(&config.path).map_err(io_error("read installed Agent config"))?;
        let current_hash = sha256(&current);
        let safe_to_refresh = match (&prior_installed_hash, transaction.original(&config.path)) {
            (None, _) => true,
            (Some(prior), None) => current_hash == *prior,
            (Some(prior), Some(Original::File { sha256, .. })) => sha256 == prior,
            (Some(_), Some(Original::Absent | Original::Symlink { .. })) => false,
        };
        if safe_to_refresh {
            config.installed_file_hash = Some(current_hash);
        }
    }
    Ok(())
}

fn restore_unchanged_config(config: &OwnedConfig, report: &mut UninstallReport) -> Result<bool> {
    let Some(installed_hash) = &config.installed_file_hash else {
        return Ok(false);
    };
    let current = fs::read(&config.path).map_err(io_error("read Agent config for uninstall"))?;
    if sha256(&current) != *installed_hash {
        return Ok(false);
    }
    match &config.baseline {
        Some(Original::Absent) => {
            fs::remove_file(&config.path)
                .map_err(io_error("remove installer-created Agent config"))?;
            report.removed.push(config.path.clone());
        }
        Some(Original::File {
            backup,
            sha256: expected,
            mode,
        }) => {
            let bytes = fs::read(backup).map_err(io_error("read Agent config baseline"))?;
            if sha256(&bytes) != *expected {
                return Err(Error::new(
                    ErrorKind::InvariantViolation,
                    format!("Agent config baseline hash mismatch: {}", backup.display()),
                ));
            }
            atomic_write(&config.path, &bytes, *mode)?;
            report.preserved.push(config.path.clone());
        }
        Some(Original::Symlink { .. }) => {
            return Err(Error::new(
                ErrorKind::InvariantViolation,
                format!(
                    "Agent config baseline cannot be a symlink: {}",
                    config.path.display()
                ),
            ));
        }
        None => return Ok(false),
    }
    Ok(true)
}

fn prior_originally_absent(
    configs: &[OwnedConfig],
    kind: ConfigKind,
    current_absent: bool,
) -> bool {
    configs
        .iter()
        .find(|config| config.kind == kind)
        .map_or(current_absent, |config| config.originally_absent)
}

fn find_owned<'a>(
    configs: &'a [OwnedConfig],
    kind: ConfigKind,
    selector: &str,
) -> Option<&'a OwnedEntry> {
    configs
        .iter()
        .find(|config| config.kind == kind)
        .and_then(|config| {
            config
                .entries
                .iter()
                .find(|entry| entry.selector == selector)
        })
}

const fn config_kind_name(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::CursorMcp => "cursor_mcp",
        ConfigKind::CursorHooks => "cursor_hooks",
        ConfigKind::CodexMcp => "codex_mcp",
        ConfigKind::CodexHooks => "codex_hooks",
    }
}

fn expected_config_path(home: &Path, kind: ConfigKind) -> PathBuf {
    match kind {
        ConfigKind::CursorMcp => home.join(".cursor/mcp.json"),
        ConfigKind::CursorHooks => home.join(".cursor/hooks.json"),
        ConfigKind::CodexMcp => home.join(".codex/config.toml"),
        ConfigKind::CodexHooks => home.join(".codex/hooks.json"),
    }
}

struct ConfigLock(File);

impl ConfigLock {
    fn acquire(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("config path has no parent"))?;
        ensure_directory_preserving_mode(parent)?;
        let lock = open_lock(&config_lock_path(path)?)?;
        lock.lock_exclusive()
            .map_err(io_error("lock Agent config"))?;
        Ok(Self(lock))
    }
}

fn config_lock_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("config path has no parent"))?;
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| invalid("config filename is not UTF-8"))?;
    Ok(parent.join(format!(".{name}.sctx.lock")))
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn recover_incomplete_journals(root: &Path) -> Result<()> {
    let backups = root.join("backups");
    let mut journals = Vec::new();
    if !backups.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&backups).map_err(io_error("read backups directory"))? {
        let entry = entry.map_err(io_error("read backup entry"))?;
        if !entry.file_name().to_string_lossy().starts_with("setup-") {
            continue;
        }
        let path = entry.path().join("journal.json");
        if path.is_file() {
            journals.push(path);
        }
    }
    journals.sort();
    for path in journals {
        let bytes = fs::read(&path).map_err(io_error("read setup journal"))?;
        let mut journal: SetupJournal = serde_json::from_slice(&bytes).map_err(|error| {
            invalid(format!("invalid setup journal {}: {error}", path.display()))
        })?;
        if journal.version != JOURNAL_VERSION || journal.complete {
            continue;
        }
        restore_journal(&journal)?;
        journal.complete = true;
        "recovered_rollback".clone_into(&mut journal.phase);
        let bytes = serde_json::to_vec_pretty(&journal)
            .map_err(|error| io_value("serialize recovered journal", error))?;
        atomic_write(&path, &bytes, 0o600)?;
    }
    Ok(())
}

fn reset_marker_path(root: &Path) -> PathBuf {
    root.join("state/reset-journal.json")
}

fn reset_relative_targets() -> Vec<PathBuf> {
    let mut targets = vec![PathBuf::from("repository"), PathBuf::from("config.toml")];
    for database in [
        "index.sqlite",
        "runtime.sqlite",
        "engineering.sqlite",
        "repository-registry.sqlite",
    ] {
        for suffix in ["", "-wal", "-shm"] {
            targets.push(PathBuf::from(format!("state/{database}{suffix}")));
        }
    }
    targets.extend([
        PathBuf::from("state/pending"),
        PathBuf::from("state/pending-aside"),
        PathBuf::from("state/capture"),
        PathBuf::from("state/authorized-session-scopes"),
    ]);
    targets
}

fn validate_reset_active_targets(root: &Path) -> Result<()> {
    for relative in reset_relative_targets() {
        let active = root.join(&relative);
        match fs::symlink_metadata(&active) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "data reset target must not be a symlink: {}",
                    active.display()
                )));
            }
            Ok(metadata) if relative == Path::new("repository") && !metadata.is_dir() => {
                return Err(invalid("data reset repository target must be a directory"));
            }
            Ok(metadata) if relative == Path::new("config.toml") && !metadata.is_file() => {
                return Err(invalid("data reset config target must be a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect data reset target")(error)),
        }
    }
    Ok(())
}

fn build_pristine_reset_root(final_root: &Path, staging_root: &Path) -> Result<()> {
    ensure_private_directory(staging_root)?;
    let store = GitStore::bootstrap_local(staging_root)?;
    ProjectionIndex::for_store(&store).synchronize()?;
    let _runtime = TaskRuntime::initialize(staging_root.to_path_buf())?;
    let registry = RepositoryRegistry::initialize(staging_root.to_path_buf())?;
    registry.sync_catalog(&[])?;
    let _engineering = EngineeringProjectionStore::initialize(staging_root.to_path_buf())?;
    let _capture = CaptureStore::initialize(staging_root)?;
    let _scopes = AuthorizedSessionScopeStore::initialize(staging_root)?;
    ensure_private_directory(&staging_root.join("state/pending-aside"))?;
    let empty_config = UserConfigStore::empty_document(final_root)?;
    atomic_write(
        &staging_root.join("config.toml"),
        empty_config.as_bytes(),
        0o600,
    )?;
    Ok(())
}

fn build_reset_entries(
    root: &Path,
    backup_dir: &Path,
    staging_root: &Path,
) -> Result<Vec<DataResetEntry>> {
    reset_relative_targets()
        .into_iter()
        .map(|relative| {
            let active = root.join(&relative);
            let staged = staging_root.join(&relative);
            Ok(DataResetEntry {
                original_present: reset_path_present(&active)?,
                staged_present: reset_path_present(&staged)?,
                backup: backup_dir.join("old").join(&relative),
                active,
                staged: Some(staged),
            })
        })
        .collect()
}

fn persist_reset_journal(journal: &DataResetJournal, active_marker: bool) -> Result<()> {
    validate_reset_journal(journal, &journal.root)?;
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| io_value("serialize data reset journal", error))?;
    atomic_write(&journal.backup_dir.join("journal.json"), &bytes, 0o600)?;
    if active_marker {
        atomic_write(&reset_marker_path(&journal.root), &bytes, 0o600)?;
    }
    Ok(())
}

fn validate_reset_journal(journal: &DataResetJournal, root: &Path) -> Result<()> {
    if journal.version != RESET_JOURNAL_VERSION || journal.root != root {
        return Err(invalid("data reset journal version or root is invalid"));
    }
    let raw_id = journal
        .id
        .strip_prefix("reset-")
        .ok_or_else(|| invalid("data reset journal ID prefix is invalid"))?;
    Uuid::parse_str(raw_id).map_err(|_| invalid("data reset journal ID is invalid"))?;
    let expected_backup = root.join("backups").join(&journal.id);
    if journal.backup_dir != expected_backup
        || journal.staging_root != expected_backup.join("new")
        || journal.entries.len() != reset_relative_targets().len()
    {
        return Err(invalid("data reset journal paths are invalid"));
    }
    for (entry, relative) in journal.entries.iter().zip(reset_relative_targets()) {
        if entry.active != root.join(&relative)
            || entry.backup != expected_backup.join("old").join(&relative)
            || entry.staged.as_ref() != Some(&expected_backup.join("new").join(&relative))
        {
            return Err(invalid("data reset journal target is invalid"));
        }
    }
    if journal.applied_entries > journal.entries.len() {
        return Err(invalid("data reset journal applied count is invalid"));
    }
    Ok(())
}

fn read_active_reset_journal(root: &Path) -> Result<Option<DataResetJournal>> {
    let marker = reset_marker_path(root);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect data reset journal")(error)),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(invalid(
            "active data reset journal must be a private regular file",
        ));
    }
    let bytes = fs::read(&marker).map_err(io_error("read data reset journal"))?;
    let journal: DataResetJournal = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid data reset journal: {error}")))?;
    validate_reset_journal(&journal, root)?;
    Ok(Some(journal))
}

fn recover_incomplete_data_reset(root: &Path) -> Result<Option<PathBuf>> {
    let Some(mut journal) = read_active_reset_journal(root)? else {
        return Ok(None);
    };
    if journal.complete {
        remove_reset_marker(root)?;
        return Ok(Some(journal.backup_dir));
    }
    for entry in journal.entries.iter().rev() {
        if reset_path_present(&entry.backup)? {
            remove_any(&entry.active)?;
            if let Some(parent) = entry.active.parent() {
                ensure_private_directory(parent)?;
            }
            fs::rename(&entry.backup, &entry.active)
                .map_err(io_error("restore data reset backup"))?;
            sync_parent(&entry.backup)?;
            sync_parent(&entry.active)?;
        } else if !entry.original_present {
            remove_any(&entry.active)?;
        }
    }
    "recovered_rollback".clone_into(&mut journal.phase);
    journal.complete = true;
    persist_reset_journal(&journal, false)?;
    remove_reset_marker(root)?;
    Ok(Some(journal.backup_dir))
}

fn remove_reset_marker(root: &Path) -> Result<()> {
    let marker = reset_marker_path(root);
    match fs::remove_file(&marker) {
        Ok(()) => sync_directory(
            marker
                .parent()
                .ok_or_else(|| invalid("reset marker has no parent"))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("remove data reset journal")(error)),
    }
}

fn smoke_pristine_reset_root(root: &Path) -> Result<()> {
    let store = GitStore::bootstrap_local(root)?;
    let snapshot = ProjectionIndex::for_store(&store).domain_snapshot()?;
    if !snapshot.projection.spaces.is_empty()
        || !snapshot.projection.candidates.is_empty()
        || !snapshot.projection.engineering_references.is_empty()
    {
        return Err(invariant("reset Index is not empty"));
    }
    let catalog = UserConfigStore::open_existing(root)?.repository_catalog_wait()?;
    if !catalog.repositories.is_empty() || !catalog.repository_groups.is_empty() {
        return Err(invariant("reset Repository Catalog is not empty"));
    }
    let _runtime = TaskRuntime::initialize(root.to_path_buf())?;
    if !RepositoryRegistry::initialize(root.to_path_buf())?
        .list()?
        .is_empty()
    {
        return Err(invariant("reset Repository Registry is not empty"));
    }
    if EngineeringProjectionStore::initialize(root.to_path_buf())?
        .read_projection()?
        .is_some()
    {
        return Err(invariant("reset Engineering projection is not empty"));
    }
    if knowledge_has_remote(store.repository())? {
        return Err(invariant("reset Knowledge Store retained a Git remote"));
    }
    Ok(())
}

fn reset_report(root: &Path, material: DataResetReportMaterial) -> DataResetReport {
    DataResetReport {
        root: root.to_path_buf(),
        dry_run: material.dry_run,
        reset_id: material.reset_id,
        backup: material.backup,
        repository_count_cleared: material.repository_count_cleared,
        repository_group_count_cleared: material.repository_group_count_cleared,
        cleared_targets: material.cleared_targets,
        preserved: vec![
            root.join("bin"),
            root.join("state/install-manifest.json"),
            root.join("logs"),
            root.join("backups"),
        ],
        remote_detached: !material.dry_run && material.had_remote,
        remote_mutated: false,
    }
}

fn existing_reset_targets(root: &Path) -> Vec<PathBuf> {
    reset_relative_targets()
        .into_iter()
        .map(|relative| root.join(relative))
        .filter(|path| path.exists())
        .collect()
}

fn reset_path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid(format!(
            "data reset path must not be a symlink: {}",
            path.display()
        ))),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect data reset path")(error)),
    }
}

fn knowledge_has_remote(repository: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["remote"])
        .output()
        .map_err(external_error("inspect Knowledge Store remotes"))?;
    if !output.status.success() {
        return Err(Error::new(
            ErrorKind::External,
            "inspect Knowledge Store remotes failed",
        ));
    }
    Ok(!output.stdout.is_empty())
}

fn sync_parent(path: &Path) -> Result<()> {
    sync_directory(
        path.parent()
            .ok_or_else(|| invalid("reset target has no parent"))?,
    )
}

fn is_injected_reset_crash(error: &Error) -> bool {
    error.kind() == ErrorKind::External && error.message().starts_with("injected reset crash after")
}

fn restore_journal(journal: &SetupJournal) -> Result<()> {
    for snapshot in journal.entries.iter().rev() {
        match &snapshot.original {
            Original::Absent => remove_any(&snapshot.path)?,
            Original::Symlink { target } => {
                remove_any(&snapshot.path)?;
                if let Some(parent) = snapshot.path.parent() {
                    ensure_private_directory(parent)?;
                }
                symlink(target, &snapshot.path).map_err(io_error("restore original symlink"))?;
            }
            Original::File {
                backup,
                sha256: expected,
                mode,
            } => {
                let bytes = fs::read(backup).map_err(io_error("read rollback backup"))?;
                if sha256(&bytes) != *expected {
                    return Err(Error::new(
                        ErrorKind::InvariantViolation,
                        format!("rollback backup hash mismatch: {}", backup.display()),
                    ));
                }
                atomic_write(&snapshot.path, &bytes, *mode)?;
            }
        }
        if let Some(parent) = snapshot.path.parent() {
            sync_directory(parent)?;
        }
        for directory in &snapshot.cleanup_empty_dirs {
            remove_empty_directory(directory)?;
        }
    }
    Ok(())
}

fn mcp_smoke(root: &Path) -> Result<()> {
    for client in [ClientKind::Cursor, ClientKind::Codex] {
        let mut server = McpServer::new(root, client)?;
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"installer-doctor\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n"
        );
        let mut output = Vec::new();
        server
            .serve(
                &mut BufReader::new(Cursor::new(input.as_bytes())),
                &mut output,
            )
            .map_err(|error| Error::new(ErrorKind::Io, format!("MCP smoke transport: {error}")))?;
        let responses = String::from_utf8(output)
            .map_err(|error| invalid(format!("MCP smoke output was not UTF-8: {error}")))?;
        let values = responses
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| invalid(format!("invalid MCP smoke response: {error}")))?;
        let tools = values
            .get(1)
            .and_then(|value| value["result"]["tools"].as_array());
        if values.len() != 2
            || values.iter().any(|value| value.get("error").is_some())
            || tools.is_none_or(|tools| {
                tools.len() != 16
                    || [
                        "task_checkpoint",
                        "candidate_list",
                        "candidate_get",
                        "candidate_discard",
                        "candidate_confirm",
                    ]
                    .iter()
                    .any(|name| !tools.iter().any(|tool| tool["name"] == *name))
            })
        {
            return Err(Error::new(
                ErrorKind::InvariantViolation,
                format!("MCP initialize/tools-list smoke failed for {client:?}"),
            ));
        }
    }
    Ok(())
}

fn check_directory(root: &Path, checks: &mut Vec<DoctorCheck>) {
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {
            let mode = metadata.permissions().mode() & 0o777;
            if mode.trailing_zeros() >= 6 {
                checks.push(ok(
                    "layout",
                    format!("{} exists with private permissions", root.display()),
                ));
            } else {
                checks.push(warning(
                    "layout",
                    format!(
                        "{} permissions are {mode:o}; expected no group/other access",
                        root.display()
                    ),
                ));
            }
        }
        Ok(_) => checks.push(failed(
            "layout",
            format!("{} is not a directory", root.display()),
        )),
        Err(error) => checks.push(failed("layout", format!("{}: {error}", root.display()))),
    }
}

fn check_runtime(root: &Path, checks: &mut Vec<DoctorCheck>) {
    let current = root.join("bin/current");
    match (fs::read_link(&current), fs::metadata(current.join("sctx"))) {
        (Ok(target), Ok(metadata)) if metadata.is_file() => {
            checks.push(ok("runtime", format!("current -> {}", target.display())));
        }
        (_, _) => checks.push(failed("runtime", "bin/current/sctx is missing or invalid")),
    }
}

fn check_repository(root: &Path, checks: &mut Vec<DoctorCheck>) {
    let repository = root.join("repository");
    let fsck = Command::new("git")
        .args(["-C"])
        .arg(&repository)
        .args(["fsck", "--no-progress"])
        .output();
    match fsck {
        Ok(output) if output.status.success() => {
            checks.push(ok("git_fsck", "repository fsck passed"));
        }
        Ok(output) => checks.push(failed(
            "git_fsck",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )),
        Err(error) => checks.push(failed("git_fsck", error.to_string())),
    }
    let status = Command::new("git")
        .args(["-C"])
        .arg(&repository)
        .args(["status", "--porcelain=v1"])
        .output();
    match status {
        Ok(output) if output.status.success() && output.stdout.is_empty() => {
            checks.push(ok("git_worktree", "no managed M/D/R or pending A"));
        }
        Ok(output) if output.status.success() => checks.push(warning(
            "git_worktree",
            format!(
                "pending or modified paths:\n{}",
                String::from_utf8_lossy(&output.stdout).trim()
            ),
        )),
        Ok(output) => checks.push(failed(
            "git_worktree",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )),
        Err(error) => checks.push(failed("git_worktree", error.to_string())),
    }
    let pending = root.join("state/pending");
    match fs::read_dir(&pending) {
        Ok(entries) => {
            let count = entries.filter_map(std::result::Result::ok).count();
            if count == 0 {
                checks.push(ok("pending", "no setup-external pending batches"));
            } else {
                checks.push(warning(
                    "pending",
                    format!("{count} pending batch directories require writer or explicit pending handling"),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            checks.push(ok("pending", "no pending batch directory"));
        }
        Err(error) => checks.push(failed("pending", error.to_string())),
    }
}

fn check_index(root: &Path, checks: &mut Vec<DoctorCheck>) {
    let index = ProjectionIndex::new(root.join("repository"), root.join("state"));
    match index.quick_check() {
        Ok(check) if check.healthy => checks.push(ok("sqlite", "quick_check passed")),
        Ok(check) => checks.push(failed("sqlite", format!("quick_check: {}", check.detail))),
        Err(error) => checks.push(failed("sqlite", error.to_string())),
    }
    match index.metadata() {
        Ok(metadata) => checks.push(ok(
            "indexed_tree",
            format!(
                "tree {} generation {}",
                metadata.indexed_tree_oid, metadata.projection_generation
            ),
        )),
        Err(error) => checks.push(failed("indexed_tree", error.to_string())),
    }
    match index.domain_snapshot() {
        Ok(snapshot) if snapshot.diagnostics.is_empty() => {
            checks.push(ok(
                "event_schema",
                "event schema and causal projection have no diagnostics",
            ));
        }
        Ok(snapshot) => checks.push(warning(
            "event_schema",
            format!(
                "{} projection diagnostics are quarantined",
                snapshot.diagnostics.len()
            ),
        )),
        Err(error) => checks.push(failed("event_schema", error.to_string())),
    }
    let request = SearchRequest {
        query: String::new(),
        filters: SearchFilters::default(),
        page_size: 1,
        cursor: None,
    };
    match SearchEngine::new(index).search(&request) {
        Ok(_) => checks.push(ok("fts", "FTS query smoke passed")),
        Err(error) => checks.push(failed("fts", error.to_string())),
    }
}

fn check_repository_catalog(root: &Path, checks: &mut Vec<DoctorCheck>) {
    let config = match UserConfigStore::initialize(root) {
        Ok(config) => config,
        Err(error) => {
            checks.push(failed("repository_catalog", error.to_string()));
            return;
        }
    };
    let report = match config.doctor_repository_catalog() {
        Ok(report) => report,
        Err(error) => {
            checks.push(failed("repository_catalog", error.to_string()));
            return;
        }
    };
    for checkout in &report.checkouts {
        let message = format!(
            "{} {}",
            checkout.repository_id,
            checkout.checkout_path.display()
        );
        match checkout.status {
            CatalogCheckoutStatus::Available => checks.push(ok(
                format!("repository_catalog.{}", checkout.repository_id),
                message,
            )),
            CatalogCheckoutStatus::Missing => checks.push(warning(
                format!("repository_catalog.{}", checkout.repository_id),
                format!("{message} is unavailable"),
            )),
            CatalogCheckoutStatus::Symlink => checks.push(failed(
                format!("repository_catalog.{}", checkout.repository_id),
                format!("{message} is a symlink"),
            )),
            CatalogCheckoutStatus::NotDirectory => checks.push(failed(
                format!("repository_catalog.{}", checkout.repository_id),
                format!("{message} is not a directory"),
            )),
            CatalogCheckoutStatus::NotGitRoot => checks.push(failed(
                format!("repository_catalog.{}", checkout.repository_id),
                format!("{message} is not a Git worktree root"),
            )),
        }
    }
    if report.checkouts.is_empty() {
        checks.push(ok(
            "repository_catalog",
            format!(
                "{} configured Repository identities and no checkout paths",
                report.repository_count
            ),
        ));
    }
    let syncable = report.checkouts.iter().all(|checkout| {
        matches!(
            checkout.status,
            CatalogCheckoutStatus::Available | CatalogCheckoutStatus::Missing
        )
    });
    if syncable {
        match sctx_mcp::sync_repository_catalog_at_root(root) {
            Ok(sync) => checks.push(ok(
                "repository_registry",
                format!(
                    "synchronized {} identities and {} locators",
                    sync.repository_count, sync.locator_count
                ),
            )),
            Err(error) => checks.push(failed("repository_registry", error.to_string())),
        }
    } else {
        checks.push(failed(
            "repository_registry",
            "Catalog validation failed; Registry synchronization skipped",
        ));
    }
}

fn check_global_skill(root: &Path, home: &Path, checks: &mut Vec<DoctorCheck>) {
    let manifest = match read_manifest(root) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            checks.push(failed(
                "global_skill",
                "global Agent Skill ownership cannot be verified because the install manifest is missing",
            ));
            return;
        }
        Err(error) => {
            checks.push(failed("global_skill", error.to_string()));
            return;
        }
    };
    let assets = global_skill_assets(home);
    let expected_paths = assets.iter().map(|(path, _)| path).collect::<BTreeSet<_>>();
    for owned in &manifest.skills {
        if !expected_paths.contains(&owned.path) {
            checks.push(action_required(
                "global_skill.ownership",
                format!(
                    "manifest contains an unexpected global Agent Skill path; it will be preserved: {}",
                    owned.path.display()
                ),
            ));
        }
    }
    if manifest
        .skills
        .iter()
        .all(|owned| !expected_paths.contains(&owned.path))
    {
        let skill_root = global_skill_root(home);
        let message = if skill_root.exists() {
            format!(
                "{} exists but is not installer-owned; setup will preserve it",
                skill_root.display()
            )
        } else {
            format!("{} is missing", skill_root.display())
        };
        checks.push(if skill_root.exists() {
            action_required("global_skill", message)
        } else {
            failed("global_skill", message)
        });
        return;
    }

    for (path, desired) in assets {
        let name = if path.ends_with("SKILL.md") {
            "global_skill.skill_md"
        } else if path.ends_with("references/workflow.md") {
            "global_skill.workflow_reference"
        } else {
            "global_skill.openai_yaml"
        };
        let Some(owned) = manifest.skills.iter().find(|owned| owned.path == path) else {
            checks.push(action_required(
                name,
                format!(
                    "{} is not installer-owned and was preserved",
                    path.display()
                ),
            ));
            continue;
        };
        check_global_skill_asset(home, name, &path, desired, owned, checks);
    }
}

fn check_global_skill_asset(
    home: &Path,
    name: &str,
    path: &Path,
    desired: &[u8],
    owned: &OwnedSkill,
    checks: &mut Vec<DoctorCheck>,
) {
    match skill_parent_directories_are_safe(home, path) {
        Ok(true) => {}
        Ok(false) => {
            checks.push(action_required(
                name,
                format!(
                    "{} was preserved because a parent is not a non-symlink directory",
                    path.display()
                ),
            ));
            return;
        }
        Err(error) => {
            checks.push(failed(name, error.to_string()));
            return;
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            match fs::read(path) {
                Ok(bytes) if sha256(&bytes) != owned.sha256 => checks.push(action_required(
                    name,
                    format!("{} was modified after setup and was preserved", path.display()),
                )),
                Ok(bytes) if bytes.as_slice() != desired => checks.push(warning(
                    name,
                    format!(
                        "{} is an unchanged managed file from another version; run setup or upgrade",
                        path.display()
                    ),
                )),
                Ok(_) => checks.push(ok(
                    name,
                    format!("{} is current and installer-owned", path.display()),
                )),
                Err(error) => checks.push(failed(name, error.to_string())),
            }
        }
        Ok(_) => checks.push(action_required(
            name,
            format!("{} is no longer a regular managed file", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            checks.push(failed(name, format!("{} is missing", path.display())));
        }
        Err(error) => checks.push(failed(name, error.to_string())),
    }
}

fn check_configs(root: &Path, home: &Path, checks: &mut Vec<DoctorCheck>) {
    let stable = root.join("bin/current/sctx");
    for (name, path) in [
        ("cursor_mcp", home.join(".cursor/mcp.json")),
        ("cursor_hooks", home.join(".cursor/hooks.json")),
        ("codex_hooks", home.join(".codex/hooks.json")),
    ] {
        match read_json_object(&path) {
            Ok(document) => {
                let text = serde_json::to_string(&document).unwrap_or_default();
                if text.contains(&stable.to_string_lossy().to_string()) {
                    checks.push(ok(
                        name,
                        format!("{} parses and targets current runtime", path.display()),
                    ));
                } else {
                    checks.push(warning(
                        name,
                        format!("{} has no current runtime registration", path.display()),
                    ));
                }
            }
            Err(error) => checks.push(failed(name, error.to_string())),
        }
    }
    let codex = home.join(".codex/config.toml");
    match read_utf8_or_empty(&codex).and_then(|text| {
        text.parse::<DocumentMut>()
            .map_err(|error| invalid(format!("invalid Codex TOML: {error}")))
    }) {
        Ok(document)
            if document
                .to_string()
                .contains(&stable.to_string_lossy().to_string()) =>
        {
            checks.push(ok(
                "codex_mcp",
                format!("{} parses and targets current runtime", codex.display()),
            ));
        }
        Ok(_) => checks.push(warning(
            "codex_mcp",
            format!("{} has no current runtime registration", codex.display()),
        )),
        Err(error) => checks.push(failed("codex_mcp", error.to_string())),
    }
}

fn hooks_available(home: &Path, agent: Agent) -> bool {
    let hooks = match agent {
        Agent::Cursor => home.join(".cursor/hooks.json"),
        Agent::Codex => home.join(".codex/hooks.json"),
    };
    if !hooks.is_file() {
        return false;
    }
    if agent == Agent::Codex {
        let config = home.join(".codex/config.toml");
        if let Ok(document) = read_utf8_or_empty(&config).and_then(|text| {
            text.parse::<DocumentMut>()
                .map_err(|error| invalid(error.to_string()))
        }) {
            if document
                .get("features")
                .and_then(Item::as_table)
                .and_then(|features| features.get("hooks"))
                .and_then(Item::as_value)
                .and_then(toml_edit::Value::as_bool)
                == Some(false)
            {
                return false;
            }
        }
    }
    true
}

fn ok(name: impl Into<String>, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        status: CheckStatus::Ok,
        message: message.into(),
    }
}

fn warning(name: impl Into<String>, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        status: CheckStatus::Warning,
        message: message.into(),
    }
}

fn action_required(name: impl Into<String>, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        status: CheckStatus::ActionRequired,
        message: message.into(),
    }
}

fn failed(name: impl Into<String>, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        status: CheckStatus::Error,
        message: message.into(),
    }
}

fn validate_context(context: &InstallContext) -> Result<()> {
    if !context.home.is_absolute()
        || !context.root.is_absolute()
        || !context.runtime_source.is_absolute()
    {
        return Err(invalid(
            "HOME, installation root, and runtime source must be absolute paths",
        ));
    }
    if context.version.is_empty()
        || context.version == "."
        || context.version == ".."
        || context.version.contains('/')
        || context.version.contains('\\')
    {
        return Err(invalid("runtime version must be one safe path component"));
    }
    Ok(())
}

fn require_existing_installation(root: &Path) -> Result<()> {
    if !root.join("repository/.git").is_dir() || read_manifest(root)?.is_none() {
        Err(invalid(
            "upgrade requires an existing Shared Context installation",
        ))
    } else {
        Ok(())
    }
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| invalid(format!("invalid JSON in {}: {error}", path.display())))?
            .as_object()
            .cloned()
            .ok_or_else(|| invalid(format!("{} must contain a JSON object", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(io_error("read JSON config")(error)),
    }
}

fn object_field_mut<'a>(
    document: &'a mut Map<String, Value>,
    field: &str,
) -> Result<&'a mut Map<String, Value>> {
    document
        .entry(field.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid(format!("{field} must be a JSON object")))
}

fn write_json(path: &Path, document: &Map<String, Value>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| io_value("serialize JSON config", error))?;
    atomic_write(path, &bytes, existing_mode(path, 0o600)?)?;
    read_json_object(path).map(|_| ())
}

fn parse_toml_file(path: &Path) -> Result<()> {
    read_utf8_or_empty(path)?
        .parse::<DocumentMut>()
        .map_or_else(
            |error| {
                Err(invalid(format!(
                    "invalid TOML after write {}: {error}",
                    path.display()
                )))
            },
            |_| Ok(()),
        )
}

fn read_utf8_or_empty(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(io_error("read UTF-8 config")(error)),
    }
}

fn hash_value(value: &Value) -> Result<String> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| io_value("serialize ownership value", error))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn shell_quote(path: &Path) -> Result<String> {
    let text = path_text(path)?;
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

const fn agent_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Cursor => "cursor",
        Agent::Codex => "codex",
    }
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid(format!("path is not UTF-8: {}", path.display())))
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("file path has no parent"))?;
    ensure_directory_preserving_mode(parent)?;
    let temporary = parent.join(format!(".sctx-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(&temporary)
        .map_err(io_error("create atomic temporary file"))?;
    file.write_all(bytes)
        .map_err(io_error("write atomic temporary file"))?;
    file.sync_all()
        .map_err(io_error("fsync atomic temporary file"))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
        .map_err(io_error("set atomic file permissions"))?;
    fs::rename(&temporary, path).map_err(io_error("rename atomic file"))?;
    sync_directory(parent)
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(invalid(format!(
                "expected a non-symlink private directory: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(io_error("create private directory"))?;
        }
        Err(error) => return Err(io_error("inspect private directory")(error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("set private directory permissions"))
}

fn ensure_directory_preserving_mode(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(invalid(format!(
            "expected a non-symlink directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(io_error("create parent directory"))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(io_error("set new parent directory permissions"))
        }
        Err(error) => Err(io_error("inspect parent directory")(error)),
    }
}

fn existing_mode(path: &Path, default: u32) -> Result<u32> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.permissions().mode() & 0o7777),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(error) => Err(io_error("read file permissions")(error)),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_error("set file mode"))
}

fn open_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        ensure_directory_preserving_mode(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error("open lock file"))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("fsync directory"))
}

fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).map_err(io_error("remove rollback directory"))
        }
        Ok(_) => fs::remove_file(path).map_err(io_error("remove rollback path")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect rollback path")(error)),
    }
}

fn remove_path_if_exists(path: &Path, removed: &mut Vec<PathBuf>) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        remove_any(path)?;
        removed.push(path.to_path_buf());
    }
    Ok(())
}

fn remove_empty_directory(path: &Path) -> Result<bool> {
    match fs::remove_dir(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::DirectoryNotEmpty
                    | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(io_error("remove empty installer directory")(error)),
    }
}

fn nearest_existing_ancestor(path: &Path) -> Result<&Path> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return Ok(candidate);
        }
        current = candidate.parent();
    }
    Err(invalid(format!(
        "path has no existing ancestor: {}",
        path.display()
    )))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(io_error("resolve absolute path"))
    }
}

fn command_stdout(command: &mut Command, operation: &'static str) -> Result<String> {
    let output = command.output().map_err(external_error(operation))?;
    if !output.status.success() {
        return Err(Error::new(
            ErrorKind::External,
            format!(
                "{operation} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|error| invalid(format!("{operation} output was not UTF-8: {error}")))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("failed to {operation}: {error}"))
}

fn external_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |error| {
        Error::new(
            ErrorKind::External,
            format!("failed to {operation}: {error}"),
        )
    }
}

fn io_value(operation: &'static str, error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Io, format!("failed to {operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{process::Command, time::Duration};

    use super::command_stdout_with_timeout;

    #[test]
    fn agent_version_probe_has_a_hard_timeout() {
        let started = std::time::Instant::now();
        assert!(
            command_stdout_with_timeout(
                Command::new("/bin/sleep").arg("10"),
                Duration::from_millis(25),
            )
            .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
