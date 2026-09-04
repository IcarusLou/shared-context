//! `sctx` command-line entry point.

mod args;

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    str::FromStr,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use args::Options;
use sctx_agent_adapter::{
    AgentCapabilities, AgentEventContext, ArtifactFocusReminderContext, CanonicalAgentAction,
    CanonicalAgentEvent, CanonicalAgentEventKind, EpisodeFinalizationTrigger, FileAccess,
    MAX_SHELL_COMMAND_PATH_CANDIDATES, PathHint, ResolvedActivationDecision, ResolvedAgentAction,
    TaskRuntimeOperation, ToolCategory, ToolOutcome, TrustState, artifact_focus_reminder_file,
    plan_action_for_activation, render_artifact_focus_reminder, shared_context_activation_marker,
};
use sctx_domain::{
    Applicability, CandidateReviewStatus, ConflictParticipant, ConflictResolutionDraft,
    ConflictResolutionResult, ContextGovernanceStatus, ContextId, ContextKind,
    ContextRevisionDraft, DecisionSource, DomainProjection, Error, ErrorKind,
    EvidenceSnapshotDraft, EvidenceType, ExternalSessionLocator, IntentSnapshot, PublicationAction,
    PublicationDraft, PublicationId, RepositoryId, ResolutionOutcome, Result, ReviewDraft,
    ReviewSummary, ReviewVerdict, RevisionId, SemanticConflictDraft, SpaceId, TaskSessionSnapshot,
    TaskSignal, TaskSignalKind, WorkEpisodeId, WorkEpisodeStatus,
};
use sctx_engineering_graph::{
    ARTIFACT_FOCUS_QUERY_BUDGET, ArtifactFocusOutcome, ArtifactFocusReader, MAX_ARTIFACT_FOCUS_HITS,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendOutcome, AppendRequest, BatchId, GitStore};
use sctx_index::{
    DomainSnapshot, IndexMetadata, ProjectionDiagnosticView, ProjectionIndex, RebuildOutcome,
};
use sctx_local_state::{
    ArtifactReminderKey, ArtifactReminderMark, ArtifactReminderStore, AuthorizedSessionScope,
    AuthorizedSessionScopeRead, AuthorizedSessionScopeStore, CatalogCheckoutStatus, HookSettings,
    MaintenanceLock, PrivacyScanner, RepositoryCatalogDiagnostic, RepositoryCatalogSnapshot,
    UserConfigStore,
};
use sctx_mcp::{
    ArtifactFocusQuery, AssociationExplainInput, AssociationRebuildInput, CandidateAnalyzeInput,
    CandidateConfirmBatchInput, CandidateConfirmInput, CandidateDiscardBatchInput,
    CandidateDiscardInput, CandidateGetInput, CandidateListInput, EngineeringReferenceRecordInput,
    RepositoryScanInput, TaskCheckpointInput, TaskContextReadInput, TaskIntentUpdateInput,
    TaskSignalSupersedeInput,
};
use sctx_search::{
    ContextPackDetailLevel, ContextStatus, ScopeFilter, SearchEngine, SearchFilters,
    SearchMatchMode, SearchRequest,
};
use sctx_task_runtime::{
    AutomatedEpisodeBoundary, CandidateBuildStatus, HookEventDecision, HookEventRecord,
    SignalRetentionRule, TaskRuntime,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const HOOK_TASK_UNAVAILABLE: &str = "Shared Context task retrieval is temporarily unavailable. Coding can continue; retry through MCP or CLI later.";
const INTENT_BOOTSTRAP_REMINDER: &str = "Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.";
const _: () = assert!(INTENT_BOOTSTRAP_REMINDER.len() <= 128);

const HELP: &str = r"Shared Context command-line interface

Usage: sctx [--json] <COMMAND>

Commands:
  setup [--demo] [--embedding] [--agents cursor,codex] [--knowledge-store-url GIT_URL]
      [--root PATH] [--runtime-source PATH]
  demo
  doctor [--fix] [--recheck] [--root PATH]
  upgrade [--agents cursor,codex] [--root PATH] [--runtime-source PATH]
  uninstall [--root PATH]
  data reset [--dry-run] [--yes]
  knowledge sync|delete
  embedding install|status|remove
  space create|intent revise|list|get
  candidate list|get|discard|confirm|stats|build-closed-episode|analyze
  context revise|review|publish|withdraw|get
  context withdraw --decision-source human|agent_policy [--external-session <ID>] [--dry-run]
  semantic conflict open|resolve
  task context|artifact-focus|checkpoint|intent update|signal supersede
  repository add|list|doctor|rename|scan
  engineering-reference record
  association explain|rebuild
  search
  index rebuild|status
  pending list|commit|move-aside
  validate --staged
  hook --agent cursor|codex
  hook --agent cursor|codex --capabilities [probe options]
  mcp serve --client cursor|codex

Global options:
  --json        Emit a stable JSON envelope
  -h, --help    Print help
  -V, --version Print version

Use `sctx <command> --help` for command-specific options.
";

const SPACE_CREATE_HELP: &str = r"Usage:
  sctx space create --input <INTENT.json>
  sctx space create --title <TEXT> --problem <TEXT> --desired-outcome <TEXT>
      --in-scope <TEXT>... --acceptance-condition <TEXT>...
      [--out-of-scope <TEXT>]... [--domain-term <TEXT>]...
";

const CONTEXT_WRITE_HELP: &str = r#"Context content options:
  --input <CONTEXT.json>
or:
  --kind decision|contract|issue|risk|validation|discovery|progress
  [--topic-key <TEXT>] --statement <TEXT> --rationale <TEXT>
  [--domain <TEXT>]... [--platform <TEXT>]... [--condition <TEXT>]...
  [--assumption <TEXT>]... [--recheck-when <TEXT>]...
  --evidence-json <JSON>...

Evidence JSON shape:
  {"kind":"source_snapshot","supports":"...","content":{"...":"..."},
   "interpretation":"...","limitations":["..."]}
"#;

fn main() -> ExitCode {
    let raw = env::args_os().skip(1).collect::<Vec<_>>();
    let json_output = raw.iter().any(|arg| arg == "--json");
    match utf8_args(raw).and_then(|args| run(&args, json_output)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            emit_error(&error, json_output);
            ExitCode::from(2)
        }
    }
}

fn utf8_args(args: Vec<OsString>) -> Result<Vec<String>> {
    args.into_iter()
        .filter(|arg| arg != "--json")
        .map(|arg| {
            arg.into_string()
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "arguments must be valid UTF-8"))
        })
        .collect()
}

fn run(args: &[String], json_output: bool) -> Result<()> {
    let _maintenance = if requires_shared_maintenance_guard(args) {
        Some(MaintenanceLock::initialize(installation_root()?)?.try_shared()?)
    } else {
        None
    };
    run_without_maintenance(args, json_output)
}

fn requires_shared_maintenance_guard(args: &[String]) -> bool {
    args.first().is_some_and(|command| {
        matches!(
            command.as_str(),
            "demo"
                | "space"
                | "candidate"
                | "context"
                | "semantic"
                | "task"
                | "repository"
                | "engineering-reference"
                | "association"
                | "search"
                | "index"
                | "pending"
                | "validate"
        )
    })
}

fn run_without_maintenance(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [] => {
            print!("{HELP}");
            Ok(())
        }
        [arg] if arg == "-h" || arg == "--help" => {
            print!("{HELP}");
            Ok(())
        }
        [arg] if arg == "-V" || arg == "--version" => {
            println!("sctx {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [command, rest @ ..] if command == "setup" => {
            run_install_lifecycle("setup", rest, json_output)
        }
        [command, rest @ ..] if command == "demo" => run_demo(rest, json_output),
        [command, rest @ ..] if command == "doctor" => run_doctor(rest, json_output),
        [command, rest @ ..] if command == "upgrade" => {
            run_install_lifecycle("upgrade", rest, json_output)
        }
        [command, rest @ ..] if command == "uninstall" => run_uninstall(rest, json_output),
        [group, rest @ ..] if group == "data" => run_data(rest, json_output),
        [group, rest @ ..] if group == "knowledge" => run_knowledge(rest, json_output),
        [group, rest @ ..] if group == "embedding" => run_embedding(rest, json_output),
        [group, rest @ ..] if group == "space" => run_space(rest, json_output),
        [group, rest @ ..] if group == "candidate" => run_candidate(rest, json_output),
        [group, rest @ ..] if group == "context" => run_context(rest, json_output),
        [group, rest @ ..] if group == "semantic" => run_semantic(rest, json_output),
        [group, rest @ ..] if group == "task" => run_task(rest, json_output),
        [group, rest @ ..] if group == "repository" => run_repository(rest, json_output),
        [group, rest @ ..] if group == "engineering-reference" => {
            run_engineering_reference(rest, json_output)
        }
        [group, rest @ ..] if group == "association" => run_association(rest, json_output),
        [command, rest @ ..] if command == "search" => run_search(rest, json_output),
        [group, rest @ ..] if group == "index" => run_index(rest, json_output),
        [group, rest @ ..] if group == "pending" => run_pending(rest, json_output),
        [command, rest @ ..] if command == "validate" => run_validate(rest, json_output),
        [command, rest @ ..] if command == "hook" => run_hook(rest),
        [group, rest @ ..] if group == "mcp" => run_mcp(rest),
        _ => Err(invalid(format!("unknown command\n\n{HELP}"))),
    }
}

fn run_install_lifecycle(command: &str, args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--yes", "--demo", "--embedding"])?;
    options.allow_only(
        &[
            "--agents",
            "--root",
            "--runtime-source",
            "--runtime-version",
            "--knowledge-store-url",
        ],
        &["--yes", "--demo", "--embedding"],
    )?;
    if command != "setup" && options.has("--demo") {
        return Err(invalid("--demo applies only to setup"));
    }
    // `upgrade` deliberately never provisions the channel. An upgrade is expected to be quick and
    // unattended; a 2.3 GB download is neither, and an installation that wanted the channel
    // already has it.
    if command != "setup" && options.has("--embedding") {
        return Err(invalid(
            "--embedding applies only to setup; run `sctx embedding install` to add the channel to an existing installation",
        ));
    }
    if command != "setup" && options.provided("--knowledge-store-url") {
        return Err(invalid("--knowledge-store-url applies only to setup"));
    }
    let installer = installer_from_options(&options)?;
    let setup = setup_options(&options)?;
    let mut report = if command == "setup" {
        installer.setup(&setup)?
    } else {
        installer.upgrade(&setup)?
    };
    if options.has("--embedding") {
        append_setup_embedding(&mut report, json_output);
    }
    if options.has("--demo") {
        let _maintenance = MaintenanceLock::open_or_create(&report.root)?.try_shared()?;
        let (demo, metadata) = complete_demo(&report.root)?;
        emit(
            "setup.demo",
            &metadata,
            json!({
                "setup": serde_json::to_value(report)
                    .map_err(json_error("serialize setup report"))?,
                "demo": demo,
            }),
            json_output,
        )
    } else {
        emit_lifecycle(&report, json_output)
    }
}

fn run_doctor(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--fix", "--recheck", "--hooks"])?;
    options.allow_only(
        &[
            "--root",
            "--runtime-source",
            "--runtime-version",
            "--agents",
        ],
        &["--fix", "--recheck", "--hooks"],
    )?;
    if options.has("--hooks") {
        if options.has("--fix") || options.has("--recheck") {
            return Err(invalid(
                "sctx doctor --hooks reports Hook diagnostics only; run --fix or --recheck separately",
            ));
        }
        return run_doctor_hooks(&options, json_output);
    }
    if options.has("--recheck") {
        if options.has("--fix") {
            return Err(invalid(
                "sctx doctor --recheck evaluates recheck_when only; run --fix separately",
            ));
        }
        return run_doctor_recheck(&options, json_output);
    }
    let installer = installer_from_options(&options)?;
    let report = if options.has("--fix") {
        installer.doctor_fix(&setup_options(&options)?)?
    } else {
        installer.doctor()
    };
    emit_lifecycle(&report, json_output)
}

/// Evaluates the structured `recheck_when` subset against the local Repository checkouts.
///
/// The outcome is local derived state written to this machine's projection: it never becomes an
/// Event, and a projection rebuild clears it, so this is the command to re-run afterwards.
fn run_doctor_recheck(options: &Options, json_output: bool) -> Result<()> {
    let root = options
        .optional("--root")?
        .map_or_else(installation_root, |value| Ok(PathBuf::from(value)))?;
    let response = sctx_mcp::context_recheck_at_root(root)?;
    let data =
        serde_json::to_value(&response).map_err(json_error("serialize recheck evaluation"))?;
    emit_raw(
        "doctor.recheck",
        &response.tree,
        response.generation,
        &data,
        json_output,
    )
}

const HOOK_DIAGNOSTIC_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
const HOOK_DIAGNOSTIC_RECENT_LIMIT: usize = 50;

/// Reports the Hook-path diagnostics `hook_event` recorded: a decision/reason count table over
/// the last 24h, the most recent rows, and — best-effort — the current activation lease count.
///
/// This opens `TaskRuntime` with the normal (non-Hook) busy window and schema check; it is never
/// on the Hook hot path.
fn run_doctor_hooks(options: &Options, json_output: bool) -> Result<()> {
    let root = options
        .optional("--root")?
        .map_or_else(installation_root, |value| Ok(PathBuf::from(value)))?;
    let runtime = TaskRuntime::initialize(&root)?;
    let since_unix_ms = unix_millis_now().saturating_sub(HOOK_DIAGNOSTIC_WINDOW_MS);
    let counts = runtime.hook_event_counts_since(since_unix_ms)?;
    let recent = runtime.recent_hook_events(HOOK_DIAGNOSTIC_RECENT_LIMIT)?;
    let active_leases = AuthorizedSessionScopeStore::initialize(&root)
        .and_then(|store| store.survey_stale_leases(Duration::ZERO))
        .map(|survey| survey.total_entries)
        .ok();
    let data = json!({
        "window_hours": HOOK_DIAGNOSTIC_WINDOW_MS / (60 * 60 * 1000),
        "counts": counts
            .iter()
            .map(|count| json!({
                "decision": count.decision,
                "reason": count.reason,
                "count": count.count,
            }))
            .collect::<Vec<_>>(),
        "recent_events": recent
            .iter()
            .map(|event| json!({
                "recorded_at_unix_ms": event.recorded_at_unix_ms,
                "agent_kind": event.agent_kind,
                "event_kind": event.event_kind,
                "decision": event.decision,
                "reason": event.reason,
                "duration_ms": event.duration_ms,
            }))
            .collect::<Vec<_>>(),
        "active_leases": active_leases,
    });
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&data).map_err(json_error("serialize doctor hooks output"))?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&data)
                .map_err(json_error("serialize doctor hooks output"))?
        );
    }
    Ok(())
}

fn run_uninstall(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--root", "--runtime-source", "--runtime-version"], &[])?;
    let report = installer_from_options(&options)?.uninstall()?;
    emit_lifecycle(&report, json_output)
}

fn run_data(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid("Usage: sctx data reset [--dry-run] [--yes]"));
    };
    if command != "reset" {
        return Err(invalid("data command must be reset"));
    }
    let options = Options::parse(rest, &["--dry-run", "--yes"])?;
    options.allow_only(
        &["--root", "--runtime-source", "--runtime-version"],
        &["--dry-run", "--yes"],
    )?;
    let report =
        installer_from_options(&options)?.reset_data(sctx_installer::DataResetOptions {
            confirmed: options.has("--yes"),
            dry_run: options.has("--dry-run"),
        })?;
    emit_lifecycle(&report, json_output)
}

fn run_knowledge(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid(
            "Usage: sctx knowledge sync | sctx knowledge delete --confirm-path <ABSOLUTE_PATH> --confirm DELETE-SHARED-CONTEXT-KNOWLEDGE",
        ));
    };
    if command == "sync" {
        let options = Options::parse(rest, &[])?;
        options.allow_only(&["--root", "--runtime-source", "--runtime-version"], &[])?;
        let report = installer_from_options(&options)?.sync_knowledge()?;
        return emit_lifecycle(&report, json_output);
    }
    if command != "delete" {
        return Err(invalid("knowledge command must be sync or delete"));
    }
    let options = Options::parse(rest, &[])?;
    options.allow_only(
        &[
            "--root",
            "--runtime-source",
            "--runtime-version",
            "--confirm-path",
            "--confirm",
        ],
        &[],
    )?;
    let confirmed_path = PathBuf::from(options.required("--confirm-path")?);
    let confirmation = options.required("--confirm")?;
    let deleted =
        installer_from_options(&options)?.delete_knowledge(&confirmed_path, confirmation)?;
    emit_lifecycle(
        &json!({"repository": deleted, "deleted": true}),
        json_output,
    )
}

const EMBEDDING_HELP: &str = r"Usage:
  sctx embedding install [--model-url <BASE_URL>] [--runtime-url <URL>]
      [--expected-sha256 <SHA256>] [--root PATH]
  sctx embedding status [--verify] [--root PATH]
  sctx embedding remove --yes [--root PATH]

`install` downloads a bge-m3 ONNX export and an ONNX Runtime library into
`~/.shared-context/embedding/`, proves the model loads, writes `[retrieval]`,
and fills the vector cache. It needs about 2.3 GB of disk and is safe to rerun:
verified files are not downloaded twice.

`--model-url` names a directory serving `model.onnx`, `model.onnx_data` and
`tokenizer.json` under those names -- an internal mirror, for example. The
built-in SHA-256 digests still apply, so a mirror serving different bytes is
rejected.
";

/// Provisions, inspects, or removes the optional embedding recall channel (ADR-0004).
///
/// Progress goes to stderr, and only when `--json` was not asked for. A half-hour download that
/// printed nothing until it finished would look indistinguishable from a hang, so a human gets a
/// running account; but stderr is also where this CLI puts its error envelope, so narrating over
/// it would leave a scripted caller parsing prose. `--json` means a machine is reading, and a
/// machine gets the two streams it was promised and nothing else.
fn run_embedding(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid(EMBEDDING_HELP));
    };
    if is_help(args) || is_help(rest) {
        print!("{EMBEDDING_HELP}");
        return Ok(());
    }
    let mut progress = embedding_progress(json_output);
    match command.as_str() {
        "install" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(
                &[
                    "--root",
                    "--runtime-source",
                    "--runtime-version",
                    "--model-url",
                    "--runtime-url",
                    "--expected-sha256",
                ],
                &[],
            )?;
            let root = embedding_root(&options)?;
            let report = sctx_installer::embedding::install(
                &root,
                &embedding_install_options(&options)?,
                &mut progress,
            )?;
            emit_lifecycle(&report, json_output)
        }
        "status" => {
            let options = Options::parse(rest, &["--verify"])?;
            options.allow_only(
                &["--root", "--runtime-source", "--runtime-version"],
                &["--verify"],
            )?;
            let report = sctx_installer::embedding::status(
                &embedding_root(&options)?,
                options.has("--verify"),
            )?;
            emit_lifecycle(&report, json_output)
        }
        "remove" => {
            let options = Options::parse(rest, &["--yes"])?;
            options.allow_only(
                &["--root", "--runtime-source", "--runtime-version"],
                &["--yes"],
            )?;
            let report = sctx_installer::embedding::remove(
                &embedding_root(&options)?,
                options.has("--yes"),
                &mut progress,
            )?;
            emit_lifecycle(&report, json_output)
        }
        _ => Err(invalid(format!(
            "embedding command must be install, status, or remove\n\n{EMBEDDING_HELP}"
        ))),
    }
}

/// Narrates a long provisioning run to stderr, unless a machine asked for JSON.
fn embedding_progress(json_output: bool) -> impl FnMut(&str) {
    move |line: &str| {
        if !json_output {
            eprintln!("sctx embedding: {line}");
        }
    }
}

fn embedding_root(options: &Options) -> Result<PathBuf> {
    options
        .optional("--root")?
        .map_or_else(installation_root, |value| Ok(PathBuf::from(value)))
}

fn embedding_install_options(
    options: &Options,
) -> Result<sctx_installer::embedding::InstallOptions> {
    Ok(sctx_installer::embedding::InstallOptions {
        model_url: options.optional("--model-url")?.map(str::to_owned),
        runtime_url: options.optional("--runtime-url")?.map(str::to_owned),
        expected_runtime_sha256: options.optional("--expected-sha256")?.map(str::to_owned),
    })
}

/// Runs the embedding provisioning that `setup --embedding` asked for, without letting it fail
/// setup.
///
/// The channel is an optional enhancement to retrieval, and the installation it enhances is
/// already complete and working by the time this runs. Failing the whole `setup` over a download
/// that timed out would trade a working installation for no installation, so a failure becomes a
/// notice on the report and an operator who can rerun `sctx embedding install` whenever they like.
///
/// It runs *after* `installer.setup()` returns rather than inside it, because setup holds the
/// exclusive maintenance lock for its whole duration and a 2.3 GB download does not belong inside
/// a lock that blocks every other `sctx` process on the machine.
fn append_setup_embedding(report: &mut sctx_installer::SetupReport, json_output: bool) {
    let mut progress = embedding_progress(json_output);
    match sctx_installer::embedding::install(
        &report.root,
        &sctx_installer::embedding::InstallOptions::default(),
        &mut progress,
    ) {
        Ok(embedding) => report.notices.push(format!(
            "Embedding recall channel enabled: model {}, runtime {}, {} Context revision(s) embedded.",
            embedding.model_path.display(),
            embedding.runtime_path.display(),
            embedding.embedded
        )),
        Err(error) => report.notices.push(format!(
            "Setup finished, but --embedding did not: {error} Retrieval stays lexical, which is \
             the default; rerun `sctx embedding install` to try again."
        )),
    }
}

fn installer_from_options(options: &Options) -> Result<sctx_installer::Installer> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| invalid("HOME is not set"))?;
    let root = options
        .optional("--root")?
        .map_or_else(|| home.join(".shared-context"), PathBuf::from);
    let runtime_source = options
        .optional("--runtime-source")?
        .map(PathBuf::from)
        .map_or_else(
            || {
                env::current_exe().map_err(|error| {
                    Error::new(
                        ErrorKind::Io,
                        format!("resolve current executable: {error}"),
                    )
                })
            },
            Ok,
        )?;
    let version = options
        .optional("--runtime-version")?
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    Ok(sctx_installer::Installer::new(
        sctx_installer::InstallContext::injected(home, root, runtime_source, version),
        std::sync::Arc::new(sctx_installer::SystemHost),
    ))
}

fn setup_options(options: &Options) -> Result<sctx_installer::SetupOptions> {
    let agents = if let Some(value) = options.optional("--agents")? {
        let mut agents = BTreeSet::new();
        for agent in value.split(',') {
            match agent.trim() {
                "cursor" => {
                    agents.insert(sctx_installer::Agent::Cursor);
                }
                "codex" => {
                    agents.insert(sctx_installer::Agent::Codex);
                }
                value => {
                    return Err(invalid(format!(
                        "unsupported setup Agent {value:?}; expected cursor,codex"
                    )));
                }
            }
        }
        if agents.is_empty() {
            return Err(invalid("--agents must select cursor and/or codex"));
        }
        agents
    } else {
        sctx_installer::SetupOptions::default().agents
    };
    let knowledge_store_url = options
        .optional("--knowledge-store-url")?
        .map(str::parse)
        .transpose()?;
    Ok(sctx_installer::SetupOptions {
        agents,
        knowledge_store_url,
    })
}

fn emit_lifecycle(value: &impl Serialize, json_output: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(value).map_err(json_error("serialize lifecycle output"))?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(value)
                .map_err(json_error("serialize lifecycle output"))?
        );
    }
    Ok(())
}

const DEMO_MARKER: &str = "sctx.demo.v1";
const DEMO_TITLE: &str = "Shared Context 三分钟 Demo";
const DEMO_TOPIC: &str = "shared-context/demo-v1";
const DEMO_STATEMENT: &str = "Published demo context is searchable through CLI and MCP.";
const DEMO_QUERY: &str = "searchable CLI MCP";

fn run_demo(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&[], &[])?;
    let root = installation_root()?;
    let (report, metadata) = complete_demo(&root)?;
    emit("demo", &metadata, report, json_output)
}

fn demo_intent() -> IntentSnapshot {
    IntentSnapshot {
        title: DEMO_TITLE.to_owned(),
        problem: "New contributors repeatedly rediscover already verified engineering context."
            .to_owned(),
        desired_outcome: "A published fact is retrievable from the CLI and both MCP clients."
            .to_owned(),
        in_scope: vec!["Space to publication and retrieval loop".to_owned()],
        out_of_scope: vec!["Remote synchronization".to_owned()],
        acceptance_conditions: vec![
            "The fixed demo statement is accepted and searchable.".to_owned(),
        ],
        domain_terms: vec![DEMO_MARKER.to_owned()],
    }
}

fn demo_context() -> ContextRevisionDraft {
    ContextRevisionDraft {
        // The fixed demo fixture has no derived problem framing or unresolved locator hints to
        // carry (WP-D's reference derivation only runs over real checkpoint claims).
        problem_view: None,
        hints: Vec::new(),
        kind: ContextKind::Validation,
        topic_key: Some(DEMO_TOPIC.to_owned()),
        statement: DEMO_STATEMENT.to_owned(),
        rationale: "The local append-only workflow completed without external services.".to_owned(),
        applicability: Applicability {
            domains: vec!["shared-context".to_owned()],
            platforms: vec!["macos".to_owned()],
            conditions: vec!["local-demo".to_owned()],
        },
        assumptions: vec!["The local Git executable remains available.".to_owned()],
        recheck_when: vec!["The demo fixture version changes.".to_owned()],
        relations: Vec::new(),
        evidence: vec![EvidenceSnapshotDraft {
            kind: EvidenceType::ExperimentRecord,
            supports: "The demo lifecycle is independently reproducible.".to_owned(),
            content: json!({
                "fixture": DEMO_MARKER,
                "expected_event_types": [
                    "space.created",
                    "context.revision_added",
                    "context.reviewed",
                    "context.publication_changed"
                ]
            }),
            interpretation: "A fixed black-box oracle can inspect the resulting Git Tree."
                .to_owned(),
            limitations: vec!["This demo does not prove remote distribution.".to_owned()],
        }],
    }
}

#[allow(clippy::too_many_lines)]
fn complete_demo(root: &Path) -> Result<(Value, IndexMetadata)> {
    let runtime = Runtime::open_at(root)?;
    let mut created_event_count = 0_usize;
    let mut snapshot = runtime.domain_snapshot()?;
    let matching_spaces = snapshot
        .projection
        .spaces
        .values()
        .filter(|space| {
            space.intent.revisions.values().any(|revision| {
                revision.intent.title == DEMO_TITLE
                    && revision
                        .intent
                        .domain_terms
                        .iter()
                        .any(|term| term == DEMO_MARKER)
            })
        })
        .map(|space| space.space_id)
        .collect::<Vec<_>>();
    let space_id = match matching_spaces.as_slice() {
        [] => {
            let event = Event::space_created(demo_intent(), None)?;
            let space_id = match event.payload() {
                EventPayload::SpaceCreated { space_id, .. } => *space_id,
                _ => unreachable!(),
            };
            runtime.append(event)?;
            created_event_count += 1;
            snapshot = runtime.domain_snapshot()?;
            space_id
        }
        [space_id] => *space_id,
        _ => {
            return Err(invariant(format!(
                "multiple Spaces carry the reserved demo marker {DEMO_MARKER}; remove the ambiguity before retrying"
            )));
        }
    };

    let space = snapshot
        .projection
        .spaces
        .get(&space_id)
        .ok_or_else(|| invariant("demo Space disappeared from the current Tree"))?;
    let matching_revisions = space
        .contexts
        .values()
        .flat_map(|context| {
            context.revisions.values().filter_map(move |revision| {
                let value = &revision.revision;
                (value.topic_key.as_deref() == Some(DEMO_TOPIC)
                    && value.statement == DEMO_STATEMENT)
                    .then_some((context.context_id, value.revision_id))
            })
        })
        .collect::<Vec<_>>();
    let (context_id, revision_id) = match matching_revisions.as_slice() {
        [] => {
            let event = Event::context_revision_added(space_id, demo_context(), None)?;
            let identities = context_identity(&event);
            runtime.append(event)?;
            created_event_count += 1;
            snapshot = runtime.domain_snapshot()?;
            identities
        }
        [identities] => *identities,
        _ => {
            return Err(invariant(format!(
                "multiple revisions carry the reserved demo topic {DEMO_TOPIC}; remove the ambiguity before retrying"
            )));
        }
    };

    let revision = require_revision(
        require_context(&snapshot.projection, space_id, context_id)?,
        context_id,
        revision_id,
    )?;
    match revision.review_summary {
        ReviewSummary::Unreviewed => {
            let event = Event::context_reviewed(
                space_id,
                context_id,
                ReviewDraft {
                    revision_id,
                    verdict: ReviewVerdict::Approve,
                    reason: "The fixed demo fixture matches its independent oracle.".to_owned(),
                },
                None,
            )?;
            runtime.append(event)?;
            created_event_count += 1;
            snapshot = runtime.domain_snapshot()?;
        }
        ReviewSummary::Approved => {}
        ReviewSummary::Rejected | ReviewSummary::Mixed => {
            return Err(invariant(
                "the reserved demo revision has a rejecting review; demo will not override governance",
            ));
        }
    }

    let context = require_context(&snapshot.projection, space_id, context_id)?;
    let revision = require_revision(context, context_id, revision_id)?;
    match &context.governance {
        ContextGovernanceStatus::Unpublished => {
            if !context.publication_heads.is_empty() {
                return Err(invariant(
                    "the reserved demo Context has unexpected Publication Heads",
                ));
            }
            let event = Event::publication_changed(
                space_id,
                context_id,
                PublicationDraft {
                    previous_publication_ids: Vec::new(),
                    action: PublicationAction::Publish,
                    revision_id,
                    review_event_ids: revision.review_event_ids.iter().copied().collect(),
                },
                None,
            )?;
            runtime.append(event)?;
            created_event_count += 1;
            snapshot = runtime.domain_snapshot()?;
        }
        ContextGovernanceStatus::Accepted {
            revision_id: accepted,
            ..
        } if *accepted == revision_id => {}
        state => {
            return Err(invariant(format!(
                "the reserved demo Context is not safely publishable: {state:?}"
            )));
        }
    }

    let response = SearchEngine::new(runtime.index.clone()).search(&SearchRequest {
        query: DEMO_QUERY.to_owned(),
        filters: SearchFilters {
            space_ids: vec![space_id],
            statuses: vec![ContextStatus::Accepted],
            ..SearchFilters::default()
        },
        page_size: 20,
        ..SearchRequest::default()
    })?;
    if !response
        .results
        .iter()
        .any(|result| result.context_id == context_id && result.revision_id == revision_id)
    {
        return Err(invariant(
            "published demo Context was not returned by the fixed CLI search",
        ));
    }
    for client in [sctx_mcp::ClientKind::Cursor, sctx_mcp::ClientKind::Codex] {
        verify_demo_mcp(runtime.store.root(), client, space_id, context_id)?;
    }

    let context = require_context(&snapshot.projection, space_id, context_id)?;
    let revision = require_revision(context, context_id, revision_id)?;
    Ok((
        json!({
            "repository": runtime.store.repository(),
            "space_id": space_id,
            "context_id": context_id,
            "revision_id": revision_id,
            "review_event_ids": revision.review_event_ids,
            "publication_head_ids": context.publication_heads,
            "query": DEMO_QUERY,
            "search_match_count": response.results.len(),
            "mcp_clients": ["cursor", "codex"],
            "created_event_count": created_event_count,
        }),
        snapshot.metadata,
    ))
}

fn verify_demo_mcp(
    root: &Path,
    client: sctx_mcp::ClientKind,
    space_id: SpaceId,
    context_id: ContextId,
) -> Result<()> {
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2024-11-05","capabilities":{},
            "clientInfo":{"name":format!("{client:?}"),"version":"demo"}
        }}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"context_search","arguments":{
                "query":DEMO_QUERY,"space_ids":[space_id],"statuses":["accepted"]
            }
        }}),
    ];
    let mut input = Vec::new();
    for request in requests {
        serde_json::to_writer(&mut input, &request).map_err(json_error("serialize demo MCP"))?;
        input.push(b'\n');
    }
    let mut output = Vec::new();
    let mut reader = io::BufReader::new(input.as_slice());
    sctx_mcp::McpServer::new(root, client)?
        .serve(&mut reader, &mut output)
        .map_err(|error| {
            Error::new(
                ErrorKind::External,
                format!("demo MCP transport {:?}: {}", error.kind(), error.message()),
            )
        })?;
    let responses = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<Value>(line)
                .map_err(|error| invariant(format!("invalid demo MCP response: {error}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let tools = responses
        .get(1)
        .and_then(|response| response.pointer("/result/tools"))
        .and_then(Value::as_array)
        .ok_or_else(|| invariant("demo MCP tools/list response is missing"))?;
    // Compare the whole emitted surface against the one shared name list rather than spot-checking
    // the Candidate Review tools: a hardcoded count plus five sampled names let `space_create` ship
    // in `tools/list` without the demo noticing.
    let emitted = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect::<BTreeSet<_>>();
    let expected = sctx_agent_adapter::shared_context_tool_names()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if tools.len() != expected.len() || emitted != expected {
        return Err(invariant(
            "demo MCP tools/list did not return the full tool surface, including Candidate Review",
        ));
    }
    let results = responses
        .get(2)
        .and_then(|response| response.pointer("/result/structuredContent/results"))
        .and_then(Value::as_array)
        .ok_or_else(|| invariant("demo MCP context_search response is missing"))?;
    if !results.iter().any(|result| {
        result.get("context_id").and_then(Value::as_str) == Some(&context_id.to_string())
    }) {
        return Err(invariant(
            "published demo Context was not returned by MCP search",
        ));
    }
    Ok(())
}

/// Best-effort diagnostic recorder for one `sctx hook` invocation.
///
/// It is created once at the top of [`run_hook`] and every fail-open, degraded, or normal
/// completion point along the Hook path calls [`Self::flush`] exactly once. It opens at most one
/// `TaskRuntime` per invocation, via the same short-timeout [`TaskRuntime::initialize_for_hook`]
/// the rest of the Hook path already uses, and reuses it for every flush in this process. When
/// the Runtime cannot be opened at all — `HOME` unset, a damaged installation, a schema that
/// predates `hook_event` — every flush degrades to exactly one stderr line instead of failing
/// the Hook or retrying.
struct HookEventRecorder {
    agent: String,
    /// `<root>/state/runtime.sqlite`, computed once with no I/O. `None` only when `HOME` is
    /// unset, matching every other Hook-path degrade-to-stderr case.
    database_path: Option<PathBuf>,
    event_kind: RefCell<Option<&'static str>>,
    session_id: RefCell<Option<String>>,
    /// Reason and detail the single Enabled completion row carries instead of a bare `ok`.
    completion: RefCell<Option<(&'static str, Option<String>)>>,
    started: Instant,
}

impl HookEventRecorder {
    fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_owned(),
            database_path: installation_root()
                .ok()
                .map(|root| root.join("state").join("runtime.sqlite")),
            event_kind: RefCell::new(None),
            session_id: RefCell::new(None),
            completion: RefCell::new(None),
            started: Instant::now(),
        }
    }

    /// Names what this Hook actually did, for the one Enabled completion row it will write.
    ///
    /// A normal Enabled Hook writes exactly one `hook_event` row, and that budget is the point:
    /// several Hook processes flush concurrently, so an extra insert per event lands directly on
    /// the contended part of the hot path. Ordinary outcomes therefore *replace* the `ok` reason
    /// rather than adding a row; only genuine faults flush one of their own.
    fn note_completion(&self, reason: &'static str, detail: Option<String>) {
        *self.completion.borrow_mut() = Some((reason, detail));
    }

    fn take_completion(&self) -> (&'static str, Option<String>) {
        self.completion.borrow_mut().take().unwrap_or(("ok", None))
    }

    /// Binds the decoded event kind and session id. Every `flush` after this call uses the
    /// bound values; a `flush` before it (only reachable from an undecodable payload) records
    /// `event_kind = "undecodable"` and no session id.
    fn bind(&self, event_kind: CanonicalAgentEventKind, session_id: &str) {
        *self.event_kind.borrow_mut() = Some(hook_event_kind_str(event_kind));
        *self.session_id.borrow_mut() = Some(session_id.to_owned());
    }

    /// Records one decision point. Never fails the Hook, and never adds stderr noise to a Hook
    /// run that is otherwise clean: only a completely unresolvable `runtime.sqlite` path (`HOME`
    /// unset) degrades to one stderr line. A write that fails once the path is resolved —
    /// including the ordinary case of a schema that predates `hook_event`, or an installation
    /// that has not run `sctx setup` yet — is silently dropped, exactly like every other
    /// Hook-path diagnostic gap this change did not create.
    ///
    /// This writes through [`TaskRuntime::record_hook_event_at`] directly against the resolved
    /// path rather than constructing a [`TaskRuntime`] first: many Hook processes may flush
    /// concurrently, and skipping the extra validating connection open keeps this off the
    /// contended part of the Hook hot path.
    fn flush(&self, decision: HookEventDecision, reason: &str, detail: Option<String>) {
        let event_kind = self.event_kind.borrow().unwrap_or("undecodable").to_owned();
        let external_session_id = self.session_id.borrow().clone();
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &self.database_path {
            Some(database_path) => {
                let record = HookEventRecord {
                    recorded_at_unix_ms: unix_millis_now(),
                    agent_kind: self.agent.clone(),
                    external_session_id,
                    event_kind,
                    decision,
                    reason: reason.to_owned(),
                    duration_ms,
                    detail,
                };
                let _ = TaskRuntime::record_hook_event_at(database_path, &record);
            }
            None => {
                eprintln!("shared-context hook: {reason}");
            }
        }
    }
}

const fn hook_event_kind_str(kind: CanonicalAgentEventKind) -> &'static str {
    match kind {
        CanonicalAgentEventKind::SessionStart => "session_start",
        CanonicalAgentEventKind::PromptSubmit => "prompt_submit",
        CanonicalAgentEventKind::PostToolUse => "post_tool_use",
        CanonicalAgentEventKind::PreCompact => "pre_compact",
        CanonicalAgentEventKind::TurnStop => "turn_stop",
        CanonicalAgentEventKind::SessionEnd => "session_end",
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Truncates a safe (non-prompt, non-tool-output) diagnostic string to the
/// `hook_event.detail` column's character ceiling.
fn truncate_hook_detail(text: &str) -> String {
    text.chars()
        .take(sctx_task_runtime::MAX_HOOK_EVENT_DETAIL_CHARS)
        .collect()
}

/// Longest single key name one payload fingerprint quotes.
const MAX_FINGERPRINT_KEY_CHARS: usize = 32;

/// Describes an undecodable Hook payload by its *shape* alone, so the next one is diagnosable.
///
/// Ten of these arrived from one real Codex build with an empty `detail`, which said only that
/// something failed and never which event. The shape is enough to identify it: the payload's
/// byte length and its top-level key names, which is what the decoder branches on. No value is
/// read — not a Prompt, not a tool input, not a path, not an identity — because the payload is
/// untrusted host text and this row is a diagnostic, not a capture. Key names are held to an
/// ASCII identifier alphabet and truncated, so a hostile payload cannot write arbitrary text
/// into the diagnostic column.
fn undecodable_payload_fingerprint(payload: &[u8]) -> String {
    let bytes = payload.len();
    let keys = match serde_json::from_slice::<Value>(payload) {
        Ok(Value::Object(object)) => object
            .keys()
            .map(|key| sanitize_fingerprint_key(key))
            .collect::<Vec<_>>()
            .join(","),
        Ok(_) => "<not-an-object>".to_owned(),
        Err(_) => "<invalid-json>".to_owned(),
    };
    truncate_hook_detail(&format!("bytes={bytes} keys=[{keys}]"))
}

fn sanitize_fingerprint_key(key: &str) -> String {
    let sanitized = key
        .chars()
        .take(MAX_FINGERPRINT_KEY_CHARS)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '?'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "?".to_owned()
    } else {
        sanitized
    }
}

/// Decodes one vendor payload, or fails open with a shape fingerprint.
///
/// `Ok(None)` means the payload is undecodable. It comes from the Agent host, not the user, so
/// an unrecognized shape (a new desktop build, say) is a neutral no-op rather than exit 2, which
/// hosts render as a blocked action. Exit 2 stays reserved for CLI usage errors.
fn decode_hook_payload(
    agent: &str,
    input: &[u8],
    installed_agent_version: Option<String>,
    recorder: &HookEventRecorder,
) -> Result<Option<(CanonicalAgentEvent, Option<String>)>> {
    let decoded = if agent == "cursor" {
        sctx_adapter_cursor::decode_hook_input(input)
            .map(|(event, payload_version)| (event, Some(payload_version)))
    } else {
        sctx_adapter_codex::decode_hook_input(input).map(|event| (event, installed_agent_version))
    };
    match decoded {
        Ok(decoded) => Ok(Some(decoded)),
        Err(error) if error.kind() == ErrorKind::InvalidInput => {
            eprintln!("sctx hook: ignoring undecodable {agent} payload: {error}");
            recorder.flush(
                HookEventDecision::FailOpen,
                "payload_decode_failed",
                Some(undecodable_payload_fingerprint(input)),
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn run_hook(args: &[String]) -> Result<()> {
    let options = Options::parse(args, &["--capabilities"])?;
    options.allow_only(
        &["--agent", "--agent-version", "--hook-available", "--trust"],
        &["--capabilities"],
    )?;
    let agent = options.required("--agent")?;
    if !matches!(agent, "cursor" | "codex") {
        return Err(invalid(format!(
            "unsupported hook agent {agent:?}; expected cursor or codex"
        )));
    }

    if options.has("--capabilities") {
        let version = options
            .optional("--agent-version")?
            .map(str::to_owned)
            .or_else(|| detect_agent_version(agent));
        let hook_available = options
            .optional("--hook-available")?
            .map(parse_bool)
            .transpose()?
            .unwrap_or_else(|| version.is_some());
        let trust = parse_trust(agent, options.optional("--trust")?, false)?;
        let report = agent_capabilities(agent, version.as_deref(), hook_available, trust);
        println!(
            "{}",
            serde_json::to_string(&report).map_err(json_error("serialize Agent capabilities"))?
        );
        return Ok(());
    }

    if options.optional("--hook-available")?.is_some() || options.optional("--trust")?.is_some() {
        return Err(invalid(
            "--hook-available and --trust are probe-only options used with --capabilities",
        ));
    }
    let recorder = HookEventRecorder::new(agent);
    let mut input = Vec::new();
    io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| Error::new(ErrorKind::Io, format!("read hook stdin: {error}")))?;
    if input.is_empty() {
        return Err(invalid("hook stdin must contain one JSON payload"));
    }

    let installed_agent_version = options.optional("--agent-version")?.map(str::to_owned);
    let Some((event, version)) =
        decode_hook_payload(agent, &input, installed_agent_version, &recorder)?
    else {
        println!("{{}}");
        return Ok(());
    };
    recorder.bind(event.kind(), &event.context().session_id);
    let trust = parse_trust(agent, None, true)?;
    let capabilities = agent_capabilities(agent, version.as_deref(), true, trust);
    let maintenance = installation_root()
        .and_then(MaintenanceLock::open_or_create)
        .and_then(|lock| lock.try_shared())
        .ok();
    if maintenance.is_none() {
        recorder.flush(HookEventDecision::FailOpen, "maintenance_lock_busy", None);
    }
    let authorization = if maintenance.is_some() {
        resolve_hook_authorization(agent, &event, &recorder)
    } else {
        HookAuthorization::disabled()
    };
    let activation = authorization.activation;
    let activated = activation == ResolvedActivationDecision::Enabled;
    let action = plan_hook_action(agent, &event, &capabilities, &authorization, &recorder);
    let resolved = resolve_hook_action(action, &recorder);
    if maintenance.is_some() && event.kind() == CanonicalAgentEventKind::SessionEnd {
        remove_hook_session_scope(agent, &event.context().session_id, &recorder);
    }
    let output = if agent == "cursor" {
        sctx_adapter_cursor::encode_hook_output(event.kind(), &resolved)?
    } else {
        sctx_adapter_codex::encode_hook_output(event.kind(), &resolved)?
    };
    println!(
        "{}",
        String::from_utf8(output).map_err(|error| {
            Error::new(ErrorKind::Io, format!("hook output is not UTF-8: {error}"))
        })?
    );
    if activated && !capabilities.hooks_verified() {
        eprintln!("{}", capabilities.diagnostic);
    }
    // A Disabled outcome that reached here without any fail-open/degraded flush along the way is
    // not a diagnostic event — it is the product's normal, by-design behavior for a Session
    // Shared Context was never authorized for, and that Session must leave exactly zero local
    // residue (verified by `repository_scoped_context_acceptance` and
    // `repository_scoped_activation_acceptance`). Only an Enabled completion is recorded here;
    // every genuine fault along a Disabled path already recorded its own row above.
    if activated {
        let (reason, detail) = recorder.take_completion();
        recorder.flush(HookEventDecision::Enabled, reason, detail);
    }
    Ok(())
}

/// Resolves the complete Hook policy for one event: pure activation policy,
/// then `PostToolUse` Catalog attribution, then a self-healed activation marker,
/// then the off-by-default P4.1 reminder.
fn plan_hook_action(
    agent: &str,
    event: &CanonicalAgentEvent,
    capabilities: &AgentCapabilities,
    authorization: &HookAuthorization,
    recorder: &HookEventRecorder,
) -> CanonicalAgentAction {
    let activation = authorization.activation;
    let action = plan_action_for_activation(event, capabilities, activation);
    // Only an event that actually planned a Signal merge has anything to attribute. Shared
    // Context's own MCP tool calls plan nothing on purpose, and running attribution over them
    // reported the by-design path as `attribution_failed` on every single call.
    let action = if matches!(
        action.task_operation,
        Some(TaskRuntimeOperation::MergeSignals { .. })
    ) && activation == ResolvedActivationDecision::Enabled
    {
        authorization
            .scope
            .as_ref()
            .zip(authorization.catalog.as_ref())
            .and_then(|(scope, catalog)| {
                match attribute_post_tool_action(event, action, scope, catalog) {
                    Ok(action) => Some(action),
                    Err(error) => {
                        recorder.flush(
                            HookEventDecision::Neutral,
                            "attribution_failed",
                            Some(truncate_hook_detail(error.message())),
                        );
                        None
                    }
                }
            })
            .unwrap_or_else(CanonicalAgentAction::neutral)
    } else {
        action
    };
    let action = add_self_healed_activation_marker(event, action, capabilities, authorization);
    if authorization.hooks.artifact_focus_reminder {
        add_artifact_focus_reminder(
            agent,
            event,
            action,
            activation,
            capabilities,
            authorization,
            recorder,
        )
    } else {
        action
    }
}

#[derive(Debug)]
struct HookAuthorization {
    activation: ResolvedActivationDecision,
    scope: Option<AuthorizedSessionScope>,
    catalog: Option<RepositoryCatalogSnapshot>,
    hooks: HookSettings,
    /// True when *this* event created the Session's missing lease and owes it the activation
    /// marker `SessionStart` never delivered.
    deliver_activation_marker: bool,
}

impl HookAuthorization {
    const fn disabled() -> Self {
        Self {
            activation: ResolvedActivationDecision::Disabled,
            scope: None,
            catalog: None,
            hooks: HookSettings {
                artifact_focus_reminder: false,
            },
            deliver_activation_marker: false,
        }
    }
}

/// Re-states the activation marker for a Session whose `SessionStart` never delivered one.
///
/// `SessionStart` is the only event that renders the marker, so a `SessionStart` that failed
/// open — a busy maintenance lock is enough — left its Session permanently without the one datum
/// the Agent cannot guess: the host Session id it must send back as `external_session_id`. The
/// lease self-heal already rebuilds authorization from a later event; this rebuilds the marker
/// with it, exactly once per lease.
///
/// The marker takes the `additional_context` field only when nothing else claimed it, which is
/// the same first-come rule the Artifact focus reminder follows. Order settles the collision:
/// this runs before the reminder, because a Session that cannot identify itself has nothing to
/// focus on. Events whose vendor output cannot carry model-visible text — a Cursor
/// `beforeSubmitPrompt` or `sessionEnd` encodes an empty object — are skipped rather than
/// spending the one-shot delivery on a field that is dropped.
fn add_self_healed_activation_marker(
    event: &CanonicalAgentEvent,
    mut action: CanonicalAgentAction,
    capabilities: &AgentCapabilities,
    authorization: &HookAuthorization,
) -> CanonicalAgentAction {
    if !authorization.deliver_activation_marker || action.additional_context.is_some() {
        return action;
    }
    action.additional_context = Some(shared_context_activation_marker(
        capabilities.agent,
        &event.context().session_id,
    ));
    action
}

/// Whether one lifecycle event's vendor output can carry model-visible text on both adapters.
const fn carries_model_visible_context(kind: CanonicalAgentEventKind) -> bool {
    matches!(
        kind,
        CanonicalAgentEventKind::PostToolUse
            | CanonicalAgentEventKind::PreCompact
            | CanonicalAgentEventKind::TurnStop
    )
}

fn resolve_hook_authorization(
    agent: &str,
    event: &CanonicalAgentEvent,
    recorder: &HookEventRecorder,
) -> HookAuthorization {
    match resolve_hook_authorization_inner(
        agent,
        event.kind(),
        &event.context().session_id,
        &event.context().cwd,
    ) {
        Ok(authorization) => authorization,
        Err(error) => {
            recorder.flush(
                HookEventDecision::FailOpen,
                "authorization_internal",
                Some(truncate_hook_detail(error.message())),
            );
            HookAuthorization::disabled()
        }
    }
}

fn resolve_hook_authorization_inner(
    agent: &str,
    event_kind: CanonicalAgentEventKind,
    session_id: &str,
    startup_cwd: &Path,
) -> Result<HookAuthorization> {
    let root = installation_root()?;
    let locator = ExternalSessionLocator::new(agent, session_id)?;
    let config = UserConfigStore::open_existing(&root)?;
    let (catalog, hooks) = config.repository_catalog_with_hooks()?;
    let store = AuthorizedSessionScopeStore::initialize(&root)?;

    // A lease is permanent, but its decision is not: the recorded canonical
    // `startup_cwd` is re-resolved against the Catalog this Hook just read, so a
    // `repository add` or removal reaches an already running Session on its next
    // event — including a Session started at a common parent, which simply gains or
    // loses one of the Repositories it records for. Re-resolution is pure — no stat, no Git, no scan — so this stays on
    // the Hook hot path.
    // Every event, not only `SessionStart`, may create the lease it is missing. A host
    // that installs the Hook mid-Session, starts it after the Session began, or stores a
    // record this Store must classify as `Missing` would otherwise leave that Session
    // permanently and silently unauthorized: the marker never appears, and every MCP call
    // is refused for its whole life. Authorization is identical either way — the event's
    // own cwd (the vendor adapters already fall back to the first workspace root) resolved
    // against this Catalog — so a Session outside every registered Repository still
    // records Disabled. The added hot-path cost is one `canonicalize`, and the write is
    // non-blocking.
    let mut deliver_activation_marker = false;
    let scope = match store.try_read_reconciled(&locator, &catalog)? {
        AuthorizedSessionScopeRead::Current(scope) => Some(scope),
        AuthorizedSessionScopeRead::Missing => {
            let canonical_startup_cwd = fs::canonicalize(startup_cwd).map_err(|error| {
                Error::new(ErrorKind::Io, format!("canonicalize startup cwd: {error}"))
            })?;
            let scope = store
                .try_authorize_missing(&locator, &catalog, &canonical_startup_cwd)?
                .scope;
            // `SessionStart` renders the marker unconditionally, so only a later event that had
            // to build the lease itself owes one. Recording the delivery in the lease is what
            // keeps the next event from repeating it — the same one-shot bookkeeping the Intent
            // bootstrap reminder uses — and a busy lock simply skips this event's delivery
            // rather than risking a duplicate.
            if event_kind != CanonicalAgentEventKind::SessionStart
                && carries_model_visible_context(event_kind)
                && scope.decision.is_enabled()
                && !scope.activation_marker_delivered
            {
                deliver_activation_marker = store
                    .try_mark_activation_marker_delivered(&locator)
                    .unwrap_or(false);
            }
            Some(scope)
        }
    };
    let activation = if scope
        .as_ref()
        .is_some_and(|scope| scope.decision.is_enabled())
    {
        ResolvedActivationDecision::Enabled
    } else {
        ResolvedActivationDecision::Disabled
    };
    Ok(HookAuthorization {
        activation,
        scope,
        catalog: Some(catalog),
        hooks,
        deliver_activation_marker,
    })
}

fn remove_hook_session_scope(agent: &str, session_id: &str, recorder: &HookEventRecorder) {
    let Some((root, locator)) = installation_root()
        .ok()
        .zip(ExternalSessionLocator::new(agent, session_id).ok())
    else {
        return;
    };
    if root.join("state").join("artifact-reminders").is_dir() {
        if let Ok(store) = ArtifactReminderStore::open(&root) {
            store.forget(&locator);
        }
    }
    let removed =
        AuthorizedSessionScopeStore::initialize(root).and_then(|store| store.try_remove(&locator));
    if let Err(error) = removed {
        recorder.flush(
            HookEventDecision::Neutral,
            "session_cleanup_failed",
            Some(truncate_hook_detail(error.message())),
        );
    }
}

/// P4.1 experiment (`[hooks] artifact_focus_reminder`, default off).
///
/// When explicitly enabled, one located `PostToolUse` file operation may add one
/// bounded Artifact focus reminder. The path stays read-only: it runs no Git, no
/// Repository scan, and no Graph rebuild, opens `state/engineering.sqlite`
/// read-only without waiting for a lock, and writes no fact. Every failure —
/// unresolved path, absent projection, query budget, unusable reminder state —
/// degrades to the disabled output.
fn add_artifact_focus_reminder(
    agent: &str,
    event: &CanonicalAgentEvent,
    mut action: CanonicalAgentAction,
    activation: ResolvedActivationDecision,
    capabilities: &AgentCapabilities,
    authorization: &HookAuthorization,
    recorder: &HookEventRecorder,
) -> CanonicalAgentAction {
    if action.additional_context.is_some() {
        return action;
    }
    if let Some(reminder) = resolve_artifact_focus_reminder(
        agent,
        event,
        activation,
        capabilities,
        authorization,
        recorder,
    ) {
        action.additional_context = Some(reminder);
    }
    action
}

fn resolve_artifact_focus_reminder(
    agent: &str,
    event: &CanonicalAgentEvent,
    activation: ResolvedActivationDecision,
    capabilities: &AgentCapabilities,
    authorization: &HookAuthorization,
    recorder: &HookEventRecorder,
) -> Option<String> {
    let file = artifact_focus_reminder_file(
        event,
        activation,
        capabilities,
        authorization.hooks.artifact_focus_reminder,
    )?;
    let catalog = authorization.catalog.as_ref()?;
    let authorized_repository_ids = authorization.scope.as_ref()?.decision.repository_ids();
    if authorized_repository_ids.is_empty() {
        return None;
    }
    // A Session started at a common parent records for several Repositories, so the file
    // itself decides which one this reminder is about: the Catalog places it, and the
    // placement must land inside this Session's own activation.
    let declared = catalog.resolve_declared_path(file).ok()?;
    let resolved = catalog
        .resolve_file_path(file, std::slice::from_ref(&declared.checkout_path))
        .ok()?;
    if !authorized_repository_ids.contains(&resolved.repository_id) {
        return None;
    }
    let root = installation_root().ok()?;
    let lookup = match ArtifactFocusReader::new(&root).accepted_contexts_for_file(
        &resolved.repository_id,
        &resolved.relative_path,
        MAX_ARTIFACT_FOCUS_HITS,
        ARTIFACT_FOCUS_QUERY_BUDGET,
    ) {
        Ok(lookup) => lookup,
        Err(error) => {
            let reason = if error.kind() == ErrorKind::MaintenanceBusy {
                "artifact_focus_db_busy"
            } else {
                "artifact_focus_error"
            };
            recorder.flush(
                HookEventDecision::FailOpen,
                reason,
                Some(truncate_hook_detail(error.message())),
            );
            return None;
        }
    };
    match lookup.outcome {
        ArtifactFocusOutcome::Completed => {}
        ArtifactFocusOutcome::BudgetExceeded => {
            recorder.flush(
                HookEventDecision::FailOpen,
                "artifact_focus_budget_exceeded",
                None,
            );
            return None;
        }
        ArtifactFocusOutcome::ProjectionAbsent => {
            recorder.flush(
                HookEventDecision::Neutral,
                "artifact_focus_projection_absent",
                None,
            );
            return None;
        }
    }
    if lookup.hits.is_empty() {
        return None;
    }
    let contexts = lookup
        .hits
        .into_iter()
        .map(|hit| ArtifactFocusReminderContext {
            context_id: hit.context_id,
            title: hit.statement,
        })
        .collect::<Vec<_>>();
    let reminder = render_artifact_focus_reminder(resolved.relative_path.as_str(), &contexts)?;
    let locator = ExternalSessionLocator::new(agent, &event.context().session_id).ok()?;
    let key = ArtifactReminderKey::new(
        &resolved.repository_id.to_string(),
        resolved.relative_path.as_str(),
    )
    .ok()?;
    match ArtifactReminderStore::open(&root)
        .ok()?
        .mark_reminded(&locator, &key)
    {
        ArtifactReminderMark::FirstReminder => Some(reminder),
        ArtifactReminderMark::AlreadyReminded => None,
    }
}

#[derive(Debug)]
enum HookEventAttribution {
    /// Every path resolved to a Repository this Session is authorized for. The Workspace it
    /// resolved through is a precondition, not a result: nothing downstream records a location,
    /// so only the attributed files travel on.
    Registered {
        file_hints: Vec<PathBuf>,
    },
    NonLocating,
}

#[derive(Debug)]
enum SafePathAttribution {
    Registered {
        repository_id: RepositoryId,
        checkout_path: PathBuf,
        file_hint: Option<PathBuf>,
    },
    Unregistered,
}

fn attribute_post_tool_action(
    event: &CanonicalAgentEvent,
    mut action: CanonicalAgentAction,
    scope: &AuthorizedSessionScope,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<CanonicalAgentAction> {
    let CanonicalAgentEvent::PostToolUse {
        context,
        path_hints,
        ..
    } = event
    else {
        return Err(invariant("PostToolUse attribution received another event"));
    };
    let attribution = resolve_post_tool_attribution(context, path_hints, scope, catalog)?;
    let Some(TaskRuntimeOperation::MergeSignals { file_hints, .. }) =
        action.task_operation.as_mut()
    else {
        return Err(invariant("enabled PostToolUse has no merge operation"));
    };
    match attribution {
        HookEventAttribution::Registered {
            file_hints: attributed_files,
        } => file_hints.clone_from(&attributed_files),
        HookEventAttribution::NonLocating => file_hints.clear(),
    }
    Ok(action)
}

fn resolve_post_tool_attribution(
    context: &AgentEventContext,
    path_hints: &[PathHint],
    scope: &AuthorizedSessionScope,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<HookEventAttribution> {
    if !scope.decision.is_enabled() {
        return Err(invalid("PostToolUse requires an enabled Session scope"));
    }
    let base = event_base_directory(context);
    let (candidates, structured): (Vec<&PathHint>, Vec<&PathHint>) = path_hints
        .iter()
        .partition(|hint| matches!(hint, PathHint::CommandCandidate(_)));

    let mut repository_ids = BTreeSet::new();
    let mut checkout_paths = BTreeSet::new();
    let mut file_hints = BTreeSet::new();
    let mut has_unregistered_path = false;
    if structured.is_empty() {
        collect_safe_path_attribution(
            resolve_safe_directory(&absolute_against(base, &context.cwd), catalog)?,
            &mut repository_ids,
            &mut checkout_paths,
            &mut file_hints,
            &mut has_unregistered_path,
        );
    } else {
        for hint in structured {
            collect_safe_path_attribution(
                resolve_structured_path_hint(hint, base, catalog)?,
                &mut repository_ids,
                &mut checkout_paths,
                &mut file_hints,
                &mut has_unregistered_path,
            );
        }
    }

    if has_unregistered_path {
        return Ok(HookEventAttribution::NonLocating);
    }
    if repository_ids.is_empty() || checkout_paths.is_empty() {
        return Err(invariant("PostToolUse attribution resolved no safe path"));
    }
    if resolve_registered_workspace(&checkout_paths, scope).is_none() {
        return Ok(HookEventAttribution::NonLocating);
    }
    extend_command_candidate_files(&candidates, base, catalog, &checkout_paths, &mut file_hints);
    Ok(HookEventAttribution::Registered {
        file_hints: file_hints.into_iter().collect(),
    })
}

/// The directory a relative path hint is resolved against.
///
/// Cursor's desktop build sends some tool events with relative paths, which the absolute-path
/// rule rejected outright and which therefore contributed no clue at all. The event states where
/// it ran: its own working directory, or — when the host omitted one — the first Workspace root
/// it declared. Neither is trusted as a location on its own; the joined path still has to pass
/// every existing safety check, including the canonical-form check that rejects a join through
/// a symlinked or non-normalized base.
fn event_base_directory(context: &AgentEventContext) -> Option<&Path> {
    if context.cwd.is_absolute() {
        return Some(context.cwd.as_path());
    }
    context
        .workspace_roots
        .iter()
        .find(|root| root.is_absolute())
        .map(PathBuf::as_path)
}

/// Joins a relative path onto the event's base directory, leaving an absolute path alone.
///
/// A relative path with no usable base stays relative, so [`validate_safe_existing_path`]
/// rejects it exactly as it did before.
fn absolute_against(base: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    base.map_or_else(|| path.to_path_buf(), |base| base.join(path))
}

/// Resolves the bounded command-derived candidates of one already attributed event.
///
/// Every rule here is a rejection rule, because a candidate is a guess: it must resolve to an
/// existing, non-symlink, canonical regular file inside a checkout this event *already*
/// attributed to. Landing in a different registered Repository is not enough — accepting one
/// could widen the event's Workspace and flip it non-locating, so a guess is never allowed to
/// change an outcome the structured hints decided. Nothing here fails the event, and at most
/// [`MAX_SHELL_COMMAND_PATH_CANDIDATES`] candidates are inspected.
fn extend_command_candidate_files(
    candidates: &[&PathHint],
    base: Option<&Path>,
    catalog: &RepositoryCatalogSnapshot,
    checkout_paths: &BTreeSet<PathBuf>,
    file_hints: &mut BTreeSet<PathBuf>,
) {
    for hint in candidates.iter().take(MAX_SHELL_COMMAND_PATH_CANDIDATES) {
        let PathHint::CommandCandidate(path) = hint else {
            continue;
        };
        let path = absolute_against(base, path);
        let Ok(metadata) = validate_safe_existing_path(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(Some((_, checkout_path))) = catalog.deepest_checkout_for(&path) else {
            continue;
        };
        if checkout_paths.contains(&checkout_path) {
            file_hints.insert(path);
        }
    }
}

fn resolve_structured_path_hint(
    hint: &PathHint,
    base: Option<&Path>,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<SafePathAttribution> {
    match hint {
        PathHint::File(path) => resolve_safe_file(&absolute_against(base, path), catalog),
        PathHint::Path(path) => {
            let path = absolute_against(base, path);
            let metadata = validate_safe_existing_path(&path)?;
            if metadata.is_file() {
                return resolve_safe_file(&path, catalog);
            }
            resolve_safe_directory(&path, catalog)
        }
        PathHint::WorkingDirectory(path) => {
            resolve_safe_directory(&absolute_against(base, path), catalog)
        }
        PathHint::CommandCandidate(_) => Err(invariant(
            "a command candidate is never resolved as a structured path hint",
        )),
        PathHint::Ambiguous => Err(invalid("PostToolUse contains an ambiguous path hint")),
    }
}

fn resolve_safe_file(
    path: &Path,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<SafePathAttribution> {
    let metadata = validate_safe_existing_path(path)?;
    if !metadata.is_file() {
        return Err(invalid(
            "PostToolUse file path must identify a regular file",
        ));
    }
    let declared = match catalog.resolve_declared_path(path) {
        Ok(declared) => declared,
        Err(error) if error.kind() == ErrorKind::RepositoryNotConfigured => {
            return Ok(SafePathAttribution::Unregistered);
        }
        Err(error) => return Err(error),
    };
    let resolved =
        catalog.resolve_file_path(path, std::slice::from_ref(&declared.checkout_path))?;
    Ok(SafePathAttribution::Registered {
        repository_id: resolved.repository_id,
        checkout_path: resolved.checkout_path,
        file_hint: Some(path.to_path_buf()),
    })
}

fn resolve_safe_directory(
    directory: &Path,
    catalog: &RepositoryCatalogSnapshot,
) -> Result<SafePathAttribution> {
    let metadata = validate_safe_existing_path(directory)?;
    if !metadata.is_dir() {
        return Err(invalid(
            "PostToolUse working directory must identify a directory",
        ));
    }
    // Only a directory *inside* a registered checkout attributes an event. A parent
    // directory that merely contains checkouts activates the Session but locates nothing,
    // so it stays unregistered here.
    match catalog.deepest_checkout_for(directory)? {
        Some((repository_id, checkout_path)) => Ok(SafePathAttribution::Registered {
            repository_id,
            checkout_path,
            file_hint: None,
        }),
        None => Ok(SafePathAttribution::Unregistered),
    }
}

fn validate_safe_existing_path(path: &Path) -> Result<fs::Metadata> {
    if !path.is_absolute() {
        return Err(invalid("PostToolUse path must be absolute"));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::new(ErrorKind::Io, format!("inspect PostToolUse path: {error}")))?;
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return Err(invalid(
            "PostToolUse path must identify a non-symlink regular file or directory",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("canonicalize PostToolUse path: {error}"),
        )
    })?;
    if canonical != path {
        return Err(invalid(
            "PostToolUse path must be canonical and contain no symlink components",
        ));
    }
    Ok(metadata)
}

fn collect_safe_path_attribution(
    attribution: SafePathAttribution,
    repository_ids: &mut BTreeSet<RepositoryId>,
    checkout_paths: &mut BTreeSet<PathBuf>,
    file_hints: &mut BTreeSet<PathBuf>,
    has_unregistered_path: &mut bool,
) {
    if let SafePathAttribution::Registered {
        repository_id,
        checkout_path,
        file_hint,
    } = attribution
    {
        repository_ids.insert(repository_id);
        checkout_paths.insert(checkout_path);
        file_hints.extend(file_hint);
    } else {
        *has_unregistered_path = true;
    }
}

/// Picks the one Workspace root that covers every checkout this event touched.
///
/// A single checkout is its own Workspace. Several checkouts only belong together when
/// the Session itself started at a directory that contains all of them — which is exactly
/// the common-parent activation the lease already recorded. Anything wider is not this
/// Session's Workspace, so the event stays non-locating.
fn resolve_registered_workspace(
    checkout_paths: &BTreeSet<PathBuf>,
    scope: &AuthorizedSessionScope,
) -> Option<PathBuf> {
    if checkout_paths.len() == 1 {
        return checkout_paths.first().cloned();
    }
    checkout_paths
        .iter()
        .all(|checkout| checkout.starts_with(&scope.startup_cwd))
        .then(|| scope.startup_cwd.clone())
}

fn agent_capabilities(
    agent: &str,
    version: Option<&str>,
    hook_available: bool,
    trust: TrustState,
) -> AgentCapabilities {
    if agent == "cursor" {
        sctx_adapter_cursor::capabilities(version, hook_available)
    } else {
        sctx_adapter_codex::capabilities(version, hook_available, trust)
    }
}

fn resolve_hook_action(
    action: CanonicalAgentAction,
    recorder: &HookEventRecorder,
) -> ResolvedAgentAction {
    let CanonicalAgentAction {
        task_operation,
        additional_context,
        system_message,
    } = action;
    let task_resolution = match task_operation
        .map(|operation| resolve_task_operation(operation, recorder))
        .transpose()
    {
        Ok(resolution) => resolution.unwrap_or_default(),
        Err(error) => {
            recorder.flush(
                HookEventDecision::FailOpen,
                "task_operation_failed",
                Some(truncate_hook_detail(error.message())),
            );
            return ResolvedAgentAction {
                additional_context: None,
                system_message: Some(HOOK_TASK_UNAVAILABLE.to_owned()),
            };
        }
    };
    ResolvedAgentAction {
        additional_context: task_resolution.additional_context.or(additional_context),
        system_message: task_resolution.system_message.or(system_message),
    }
}

#[derive(Default)]
struct ResolvedTaskOperation {
    additional_context: Option<String>,
    system_message: Option<String>,
}

fn resolve_task_operation(
    operation: TaskRuntimeOperation,
    recorder: &HookEventRecorder,
) -> Result<ResolvedTaskOperation> {
    match operation {
        TaskRuntimeOperation::MergeSignals {
            locator,
            file_hints,
            tool_category,
            file_access,
            outcome,
            ..
        } => {
            let root = installation_root()?;
            let runtime = TaskRuntime::initialize_for_hook(&root)?;
            let Some(active) = runtime.read_snapshot_by_locator(&locator)? else {
                let mark_result = AuthorizedSessionScopeStore::initialize(&root)
                    .and_then(|store| store.try_mark_intent_bootstrap_notified(&locator));
                let notify = mark_result.as_ref().copied().unwrap_or(false);
                if let Err(error) = &mark_result {
                    recorder.flush(
                        HookEventDecision::Neutral,
                        "bootstrap_mark_failed",
                        Some(truncate_hook_detail(error.message())),
                    );
                }
                return Ok(ResolvedTaskOperation {
                    additional_context: None,
                    system_message: notify.then(|| INTENT_BOOTSTRAP_REMINDER.to_owned()),
                });
            };
            let catalog = UserConfigStore::open_existing(&root)?.repository_catalog()?;
            let derived = normalized_tool_signals(
                &catalog,
                &file_hints,
                tool_category,
                file_access,
                outcome,
            )?;
            // Two different outcomes used to share one reason, and the louder one hid the
            // quieter: an event that produced no Signal at all was reported as an event whose
            // Signals were already known. `no_attributable_files` is the honest name for a tool
            // call that named nothing this Session could place, which is exactly what a Codex
            // Session full of `exec` calls looked like before command candidates existed.
            if derived.is_empty() {
                recorder.note_completion("no_attributable_files", None);
                return Ok(ResolvedTaskOperation::default());
            }
            let signals = unrecorded_signals(&active, derived);
            if signals.is_empty() {
                recorder.note_completion("signal_write_skipped_nothing_new", None);
                return Ok(ResolvedTaskOperation::default());
            }
            let merged = runtime.merge_hook_signals_by_locator(
                &locator,
                signals,
                &file_signal_retention(),
            )?;
            if let Some(merged) = merged {
                recorder.note_completion(
                    "file_signal_recorded",
                    Some(truncate_hook_detail(&format!(
                        "inserted={} retired={}",
                        merged.inserted, merged.retired
                    ))),
                );
            }
            Ok(ResolvedTaskOperation::default())
        }
        TaskRuntimeOperation::RecordPromptSignal { locator, prompt } => {
            record_prompt_signal(&locator, &prompt, recorder);
            Ok(ResolvedTaskOperation::default())
        }
        TaskRuntimeOperation::FinalizeCheckpointedEpisode { locator, trigger } => {
            finalize_checkpointed_episode(&locator, trigger)
        }
        TaskRuntimeOperation::CleanupSessionState { locator } => {
            let root = installation_root()?;
            let runtime = TaskRuntime::initialize_for_hook(&root)?;
            let _active = runtime.read_snapshot_by_locator(&locator)?;
            let _reviews = runtime.cleanup_expired_candidate_reviews()?;
            Ok(ResolvedTaskOperation::default())
        }
    }
}

fn finalize_checkpointed_episode(
    locator: &ExternalSessionLocator,
    trigger: EpisodeFinalizationTrigger,
) -> Result<ResolvedTaskOperation> {
    let root = installation_root()?;
    let runtime = TaskRuntime::initialize_for_hook(&root)?;
    let boundary = runtime.close_checkpointed_work_episode(locator)?;
    let trigger_name = match trigger {
        EpisodeFinalizationTrigger::PreCompact => "PreCompact",
        EpisodeFinalizationTrigger::TurnStop => "TurnStop",
    };
    let system_message = match boundary {
        AutomatedEpisodeBoundary::NoActiveTask => format!(
            "Shared Context {trigger_name}: no ActiveTask exists. Continue coding normally; use $shared-context and task_intent_update before checkpointing."
        ),
        AutomatedEpisodeBoundary::NoEpisode { task_id, .. } => format!(
            "Shared Context {trigger_name}: no Work Episode is open for Task {task_id}. Use $shared-context and call task_checkpoint with complete direct Claims/Unknowns; the server resolves the current Task, Intent, and lifecycle. Hook text is not Claim evidence."
        ),
        AutomatedEpisodeBoundary::CheckpointRequired { .. } => format!(
            "Shared Context {trigger_name}: current work has no Checkpoint. Before compaction or completion, call task_checkpoint with complete direct Claims/Unknowns; the server resolves the current Task, Intent, and lifecycle. Hook text is not Claim evidence."
        ),
        AutomatedEpisodeBoundary::Closed {
            episode,
            newly_closed,
        } => {
            let episode_id = episode.episode.episode_id;
            let WorkEpisodeStatus::Closed {
                final_checkpoint_id,
            } = episode.episode.status
            else {
                return Err(invariant("automated closed Episode lacks final Checkpoint"));
            };
            let existing = runtime.read_candidate_build(episode_id)?;
            let should_build = newly_closed
                || existing
                    .as_ref()
                    .is_none_or(|build| build.status == CandidateBuildStatus::Pending);
            let build = if should_build {
                Some(sctx_mcp::build_closed_episode_at_root(&root, episode_id)?)
            } else {
                None
            };
            let (build_status, item_count) = build.as_ref().map_or_else(
                || {
                    existing.as_ref().map_or_else(
                        || ("not_started", 0),
                        |build| {
                            (
                                match build.status {
                                    CandidateBuildStatus::Pending => "pending",
                                    CandidateBuildStatus::Complete => "complete",
                                    CandidateBuildStatus::Incomplete => "incomplete",
                                },
                                build.items.len(),
                            )
                        },
                    )
                },
                |build| {
                    (
                        match build.status {
                            sctx_mcp::CandidateBuildResponseStatus::Pending => "pending",
                            sctx_mcp::CandidateBuildResponseStatus::Complete => "complete",
                            sctx_mcp::CandidateBuildResponseStatus::Incomplete => "incomplete",
                        },
                        build.items.len(),
                    )
                },
            );
            format!(
                "Shared Context {trigger_name}: Work Episode {episode_id} is durably closed at Checkpoint {final_checkpoint_id}; Candidate Builder is {build_status} with {item_count} item(s). Candidate review remains explicit and untrusted.",
            )
        }
    };
    recover_one_pending_episode_build(&root, &runtime, locator)?;
    Ok(ResolvedTaskOperation {
        additional_context: None,
        system_message: Some(system_message),
    })
}

fn recover_one_pending_episode_build(
    root: &Path,
    runtime: &TaskRuntime,
    locator: &ExternalSessionLocator,
) -> Result<()> {
    let Some(active) = runtime.read_snapshot_by_locator(locator)? else {
        return Ok(());
    };
    let episodes = runtime.list_work_episodes(active.task_session_id, 256)?;
    for episode in episodes.into_iter().rev() {
        if !matches!(episode.episode.status, WorkEpisodeStatus::Closed { .. }) {
            continue;
        }
        let build = runtime.read_candidate_build(episode.episode.episode_id)?;
        if build
            .as_ref()
            .is_none_or(|build| build.status == CandidateBuildStatus::Pending)
        {
            let _build = sctx_mcp::build_closed_episode_at_root(root, episode.episode.episode_id)?;
            break;
        }
    }
    Ok(())
}

/// Maximum Active Prompt Signals one Task retains.
///
/// A Prompt Signal is an unreviewed clue, and a long Session submits many. Eight keeps the
/// recent shape of what the user asked for without letting one Task's association tokens
/// drift into a transcript.
const MAX_ACTIVE_PROMPT_SIGNALS: usize = 8;

/// Maximum Active file Signals (`Diff` plus `Workspace`) one Task retains.
const MAX_ACTIVE_FILE_SIGNALS: usize = 16;

/// Character ceiling for one stored Prompt Signal, applied after redaction.
const MAX_PROMPT_SIGNAL_CHARS: usize = 512;

fn prompt_signal_retention() -> Vec<SignalRetentionRule> {
    vec![SignalRetentionRule {
        kinds: vec![TaskSignalKind::Prompt],
        max_active: MAX_ACTIVE_PROMPT_SIGNALS,
    }]
}

/// `Diff` and `Workspace` share one budget: both describe files this Task touched, and a Task
/// that reads twenty files and edits twenty more should not keep forty locating clues alive.
fn file_signal_retention() -> Vec<SignalRetentionRule> {
    vec![SignalRetentionRule {
        kinds: vec![TaskSignalKind::Diff, TaskSignalKind::Workspace],
        max_active: MAX_ACTIVE_FILE_SIGNALS,
    }]
}

/// Records one Prompt Signal against an already existing `ActiveTask`.
///
/// Three properties are load-bearing and are why this never returns an error to the caller:
///
/// * It never creates anything. No `ActiveTask` means no Signal, no Task, no Intent revision,
///   and no change to the Intent bootstrap reminder — a Prompt is a clue about a Task the Agent
///   already declared, never a reason to invent one.
/// * It never stores text it did not scan. The Prompt is redacted first and truncated second, so
///   a secret cannot survive by sitting past the character ceiling; a Prompt too large for the
///   scanner is dropped rather than stored unscanned.
/// * It is invisible to the model. Both vendors encode `PromptSubmit` as an empty object, and a
///   failure here must not change that, so every fault is recorded to `hook_event` and swallowed
///   instead of becoming a `systemMessage`.
fn record_prompt_signal(
    locator: &ExternalSessionLocator,
    prompt: &str,
    recorder: &HookEventRecorder,
) {
    let outcome = || -> Result<Option<usize>> {
        let root = installation_root()?;
        // No Task Runtime database means no `ActiveTask` can exist yet, so there is nothing to
        // attach a clue to. Checking that before opening keeps a Prompt from being the event
        // that first creates `runtime.sqlite`: activation alone must never fabricate Task state,
        // and the common case — a Session whose user has not called `task_intent_update` yet —
        // then costs one `stat` instead of a database open on the Hook hot path.
        if !root.join("state").join("runtime.sqlite").is_file() {
            return Ok(None);
        }
        let Some(content) = redacted_prompt_signal_content(prompt)? else {
            return Ok(None);
        };
        let runtime = TaskRuntime::initialize_for_hook(&root)?;
        let Some(active) = runtime.read_snapshot_by_locator(locator)? else {
            return Ok(None);
        };
        let signals = unrecorded_signals(
            &active,
            vec![TaskSignal {
                kind: TaskSignalKind::Prompt,
                content,
            }],
        );
        if signals.is_empty() {
            return Ok(Some(0));
        }
        let merged =
            runtime.merge_hook_signals_by_locator(locator, signals, &prompt_signal_retention())?;
        Ok(merged.map(|merged| merged.retired))
    }();
    match outcome {
        Ok(Some(retired)) => recorder.note_completion(
            "prompt_signal_recorded",
            Some(truncate_hook_detail(&format!("retired={retired}"))),
        ),
        Ok(None) => recorder.note_completion("prompt_signal_skipped_no_task", None),
        Err(error) => recorder.flush(
            HookEventDecision::FailOpen,
            "signal_write_failed",
            Some(truncate_hook_detail(error.message())),
        ),
    }
}

/// Drops Signals this Task already carries as Active, so an unchanged observation never opens a
/// write transaction.
///
/// The merge itself is already idempotent, but idempotence inside a transaction still costs the
/// write lock, and a Session that reads the same file in a loop would serialize every Hook
/// process behind one. The snapshot this filters against was read on the way in, so the check is
/// free; a concurrent insert that races it is still deduplicated inside the transaction.
fn unrecorded_signals(active: &TaskSessionSnapshot, signals: Vec<TaskSignal>) -> Vec<TaskSignal> {
    signals
        .into_iter()
        .filter(|signal| !active.task_signals.contains(signal))
        .collect()
}

/// Redacts, then truncates, one Prompt into storable Signal content.
///
/// `None` means there is nothing safe and non-empty left to store. An oversized Prompt the
/// scanner refuses is an error, not a `None`: the caller must not silently treat unscanned text
/// as clean.
fn redacted_prompt_signal_content(prompt: &str) -> Result<Option<String>> {
    let redacted = PrivacyScanner::default().redact(prompt)?;
    let content = redacted
        .text
        .chars()
        .take(MAX_PROMPT_SIGNAL_CHARS)
        .collect::<String>();
    let content = content.trim().to_owned();
    Ok((!content.is_empty()).then_some(content))
}

/// Derives this event's Signals from the structured, already attributed observation.
///
/// Every path here has already been resolved against the Catalog by
/// [`attribute_post_tool_action`], so a file Signal names a registered Repository and a path
/// relative to its checkout — never an absolute path from the user's machine. No raw command,
/// tool input value, or tool output participates.
fn normalized_tool_signals(
    catalog: &RepositoryCatalogSnapshot,
    file_hints: &[PathBuf],
    tool_category: ToolCategory,
    file_access: Option<FileAccess>,
    outcome: ToolOutcome,
) -> Result<Vec<TaskSignal>> {
    let mut signals = Vec::new();
    if tool_category == ToolCategory::TestRunner {
        let test_outcome = format!(
            "test runner {}",
            match outcome {
                ToolOutcome::Succeeded => "succeeded",
                ToolOutcome::Failed => "failed",
            }
        );
        push_signal(&mut signals, TaskSignalKind::TestOutcome, &test_outcome);
    }
    // A file the Agent rewrote states far more about this Task than a file it read, so the two
    // become different kinds. Retrieval consumes `Diff` today and `Workspace` is carried for the
    // locating channel that will consume it; both stay clues, never Evidence.
    // A shell command names the files it mentioned without ever saying whether it read or
    // rewrote them, so it settles for the same weaker kind a read gets — never `Diff`.
    let kind = match (file_access, tool_category) {
        (Some(FileAccess::Modify), _) => TaskSignalKind::Diff,
        (Some(FileAccess::Read), _) | (None, ToolCategory::Shell) => TaskSignalKind::Workspace,
        (None, _) => return Ok(signals),
    };
    for file in file_hints {
        if let Some(content) = repository_relative_signal_content(catalog, file)? {
            push_signal(&mut signals, kind, &content);
        }
    }
    Ok(signals)
}

/// Renders one attributed file as `<RepositoryId>:<checkout-relative path>`.
fn repository_relative_signal_content(
    catalog: &RepositoryCatalogSnapshot,
    file: &Path,
) -> Result<Option<String>> {
    let Some((repository_id, checkout_path)) = catalog.deepest_checkout_for(file)? else {
        return Ok(None);
    };
    let Ok(relative) = file.strip_prefix(&checkout_path) else {
        return Ok(None);
    };
    Ok(relative
        .to_str()
        .filter(|relative| !relative.is_empty())
        .map(|relative| format!("{repository_id}:{relative}")))
}

fn push_signal(signals: &mut Vec<TaskSignal>, kind: TaskSignalKind, content: &str) {
    let signal = TaskSignal {
        kind,
        content: content.trim().to_owned(),
    };
    if !signal.content.is_empty() && !signals.contains(&signal) {
        signals.push(signal);
    }
}

/// Probes the Agent that actually runs the Hook.
///
/// Cursor ships two executables: the Hook-running CLI `cursor-agent` (date-like build ids such as
/// `2026.08.25-3e8eec8`) and the desktop shim `cursor` (semver). Probe `cursor-agent` first so the
/// reported version belongs to the Hook host; fall back to the shim only when it is absent. The
/// value is informational and never gates capabilities.
fn detect_agent_version(agent: &str) -> Option<String> {
    if agent == "cursor" {
        return agent_version_output("cursor-agent").or_else(|| agent_version_output("cursor"));
    }
    agent_version_output("codex")
}

fn agent_version_output(executable: &str) -> Option<String> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
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

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid(format!(
            "invalid boolean {value:?}; expected true or false"
        ))),
    }
}

fn parse_trust(agent: &str, value: Option<&str>, running_hook: bool) -> Result<TrustState> {
    if agent == "cursor" {
        if value.is_some() {
            return Err(invalid("--trust applies only to Codex"));
        }
        return Ok(TrustState::NotRequired);
    }
    match value {
        Some("confirmed") => Ok(TrustState::Confirmed),
        Some("unconfirmed") => Ok(TrustState::Unconfirmed),
        Some(value) => Err(invalid(format!(
            "invalid Codex trust state {value:?}; expected confirmed or unconfirmed"
        ))),
        None if running_hook => Ok(TrustState::Confirmed),
        None => Ok(TrustState::Unconfirmed),
    }
}

fn run_mcp(args: &[String]) -> Result<()> {
    let [command, option, client] = args else {
        return Err(invalid("Usage: sctx mcp serve --client cursor|codex"));
    };
    if command != "serve" || option != "--client" {
        return Err(invalid("Usage: sctx mcp serve --client cursor|codex"));
    }
    let client = sctx_mcp::ClientKind::from_str(client)?;
    let _outcome = sctx_mcp::serve_stdio(installation_root()?, client)?;
    Ok(())
}

struct Runtime {
    store: GitStore,
    index: ProjectionIndex,
}

impl Runtime {
    fn open() -> Result<Self> {
        Self::open_at(&installation_root()?)
    }

    fn open_at(root: &Path) -> Result<Self> {
        let base_store = GitStore::open_existing(root)?;
        let index = ProjectionIndex::for_store(&base_store);
        let store = base_store
            .with_candidate_submission_index(Arc::new(index.clone()))
            .with_candidate_confirmation_index(Arc::new(index.clone()));
        Ok(Self { store, index })
    }

    fn domain_snapshot(&self) -> Result<DomainSnapshot> {
        self.index.domain_snapshot()
    }

    fn append(&self, event: Event) -> Result<(AppendOutcome, IndexMetadata)> {
        let outcome = self.store.append_event(AppendRequest::event(event))?;
        let metadata = self.index.synchronize()?.metadata;
        Ok((outcome, metadata))
    }
}

#[allow(clippy::too_many_lines)]
fn run_space(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "create" => {
            if is_help(rest) {
                print!("{SPACE_CREATE_HELP}");
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            allow_intent_options(&options, &[])?;
            let intent = intent_from_options(&options)?;
            let event = Event::space_created(intent, None)?;
            let (space_id, revision_id) = match event.payload() {
                EventPayload::SpaceCreated {
                    space_id,
                    intent_revision,
                } => (*space_id, intent_revision.revision_id),
                _ => unreachable!(),
            };
            let runtime = Runtime::open()?;
            let event_id = event.event_id();
            let (append, metadata) = runtime.append(event)?;
            emit(
                "space.create",
                &metadata,
                json!({
                    "space_id": space_id,
                    "revision_id": revision_id,
                    "event_id": event_id,
                    "batch_id": append.batch_id,
                    "commit_oid": append.commit_oid,
                }),
                json_output,
            )
        }
        [intent, command, rest @ ..] if intent == "intent" && command == "revise" => {
            let options = Options::parse(rest, &[])?;
            allow_intent_options(&options, &["--space-id", "--parent-revision-id"])?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let parents = parse_many_ids(&options, "--parent-revision-id", "revision ID")?;
            if parents.is_empty() {
                return Err(invalid(
                    "space intent revise requires --parent-revision-id for every current Intent Head",
                ));
            }
            let intent = intent_from_options(&options)?;
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            let space = snapshot
                .projection
                .spaces
                .get(&space_id)
                .ok_or_else(|| invalid(format!("space does not exist: {space_id}")))?;
            require_exact_heads(
                "Intent",
                &space.intent.heads,
                &parents.iter().copied().collect(),
            )?;
            let event = Event::intent_revision_added(space_id, parents, intent, None)?;
            let revision_id = match event.payload() {
                EventPayload::SpaceIntentRevisionAdded {
                    intent_revision, ..
                } => intent_revision.revision_id,
                _ => unreachable!(),
            };
            let event_id = event.event_id();
            let (append, metadata) = runtime.append(event)?;
            emit(
                "space.intent.revise",
                &metadata,
                json!({"space_id": space_id, "revision_id": revision_id, "event_id": event_id,
                       "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
                json_output,
            )
        }
        [command] if command == "list" => {
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            let spaces = snapshot
                .projection
                .spaces
                .values()
                .map(|space| {
                    let titles = space
                        .intent
                        .heads
                        .iter()
                        .filter_map(|id| space.intent.revisions.get(id))
                        .map(|revision| revision.intent.title.clone())
                        .collect::<Vec<_>>();
                    // A conflicted Space has no winning head, so it never reports provisional.
                    let provisional = space.intent.heads.len() == 1
                        && space
                            .intent
                            .heads
                            .first()
                            .and_then(|revision_id| space.intent.revisions.get(revision_id))
                            .is_some_and(|revision| revision.provisional);
                    json!({"space_id": space.space_id, "intent_heads": space.intent.heads,
                           "titles": titles, "context_count": space.contexts.len(),
                           "provisional": provisional})
                })
                .collect::<Vec<_>>();
            emit(
                "space.list",
                &snapshot.metadata,
                json!({"spaces": spaces, "diagnostics": snapshot.diagnostics.len()}),
                json_output,
            )
        }
        [command, rest @ ..] if command == "get" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--space-id"], &[])?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            let space = snapshot
                .projection
                .spaces
                .get(&space_id)
                .ok_or_else(|| invalid(format!("space does not exist: {space_id}")))?;
            emit(
                "space.get",
                &snapshot.metadata,
                serde_json::to_value(space).map_err(json_error("serialize Space"))?,
                json_output,
            )
        }
        _ => Err(invalid(format!(
            "invalid space command\n\n{SPACE_CREATE_HELP}Usage: sctx space intent revise|list|get ..."
        ))),
    }
}

#[allow(clippy::too_many_lines)]
fn run_candidate(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "list" => {
            if is_help(rest) {
                println!(
                    "Usage: sctx candidate list --agent-kind <KIND> --external-session-id <ID> [--status pending|discarded|expired|confirmed] [--limit <N>] [--cursor <CURSOR>] [--token-budget <N>] [--compact]"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &["--compact"])?;
            options.allow_only(
                &[
                    "--agent-kind",
                    "--external-session-id",
                    "--status",
                    "--scope",
                    "--limit",
                    "--cursor",
                    "--token-budget",
                ],
                &["--compact"],
            )?;
            let input = CandidateListInput {
                agent_kind: options.required("--agent-kind")?.to_owned(),
                external_session_id: options.required("--external-session-id")?.to_owned(),
                status: parse_candidate_review_status(
                    options.optional("--status")?.unwrap_or("pending"),
                )?,
                scope: parse_candidate_review_scope(
                    options.optional("--scope")?.unwrap_or("task"),
                )?,
                limit: parse_usize(options.optional("--limit")?.unwrap_or("20"), "limit")?,
                cursor: options.optional("--cursor")?.map(str::to_owned),
                token_budget: parse_usize(
                    options.optional("--token-budget")?.unwrap_or("4096"),
                    "token budget",
                )?,
            };
            // `--compact` mirrors the MCP `detail_level: "compact"` selector: the Full Rust entry
            // point stays the default so existing behavior is unchanged when the flag is absent.
            let detail_level = if options.has("--compact") {
                ContextPackDetailLevel::Compact
            } else {
                ContextPackDetailLevel::Full
            };
            let response = sctx_mcp::candidate_list_with_detail_at_root(
                installation_root()?,
                &input,
                detail_level,
            )?;
            let metadata = Runtime::open()?.index.synchronize()?.metadata;
            let data = match detail_level {
                ContextPackDetailLevel::Compact => serde_json::to_value(response.compact())
                    .map_err(json_error("serialize compact Candidate Review list"))?,
                ContextPackDetailLevel::Full => serde_json::to_value(response)
                    .map_err(json_error("serialize Candidate Review list"))?,
            };
            emit("candidate.list", &metadata, data, json_output)
        }
        [command, rest @ ..] if command == "get" => {
            if is_help(rest) {
                println!(
                    "Usage: sctx candidate get --agent-kind <KIND> --external-session-id <ID> --candidate-id <ID>"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            options.allow_only(
                &["--agent-kind", "--external-session-id", "--candidate-id"],
                &[],
            )?;
            let input = CandidateGetInput {
                agent_kind: options.required("--agent-kind")?.to_owned(),
                external_session_id: options.required("--external-session-id")?.to_owned(),
                candidate_id: options.required("--candidate-id")?.to_owned(),
            };
            let response = sctx_mcp::candidate_get_at_root(installation_root()?, &input)?;
            let metadata = Runtime::open()?.index.synchronize()?.metadata;
            emit(
                "candidate.get",
                &metadata,
                serde_json::to_value(response).map_err(json_error("serialize Candidate Review"))?,
                json_output,
            )
        }
        [command, rest @ ..] if command == "discard" => {
            if is_help(rest) {
                println!(
                    "Usage: sctx candidate discard --agent-kind <KIND> --external-session-id <ID> --expected-task-id <ID> --expected-intent-revision-id <ID> --candidate-id <ID> [--candidate-id <ID> ...] --expected-review-version <N> --reason <TEXT>\n  (repeat --candidate-id to discard several owned Pending Candidates atomically)"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            options.allow_only(
                &[
                    "--agent-kind",
                    "--external-session-id",
                    "--expected-task-id",
                    "--expected-intent-revision-id",
                    "--candidate-id",
                    "--expected-review-version",
                    "--reason",
                ],
                &[],
            )?;
            let candidate_ids = strings(options.many("--candidate-id"));
            if candidate_ids.is_empty() {
                return Err(invalid("missing required option --candidate-id"));
            }
            let agent_kind = options.required("--agent-kind")?.to_owned();
            let external_session_id = options.required("--external-session-id")?.to_owned();
            let expected_task_id = options.required("--expected-task-id")?.to_owned();
            let expected_intent_revision_id = options
                .required("--expected-intent-revision-id")?
                .to_owned();
            let expected_review_version = parse_u64(
                options.required("--expected-review-version")?,
                "expected Review version",
            )?;
            let reason = options.required("--reason")?.to_owned();
            if let [candidate_id] = candidate_ids.as_slice() {
                let input = CandidateDiscardInput {
                    agent_kind,
                    external_session_id,
                    expected_task_id,
                    expected_intent_revision_id,
                    candidate_id: candidate_id.clone(),
                    expected_review_version,
                    reason,
                    decision_source: DecisionSource::Human,
                };
                let response = sctx_mcp::candidate_discard_at_root(installation_root()?, &input)?;
                let metadata = Runtime::open()?.index.synchronize()?.metadata;
                emit(
                    "candidate.discard",
                    &metadata,
                    serde_json::to_value(response)
                        .map_err(json_error("serialize Candidate discard response"))?,
                    json_output,
                )
            } else {
                let input = CandidateDiscardBatchInput {
                    agent_kind,
                    external_session_id,
                    expected_task_id,
                    expected_intent_revision_id,
                    candidate_ids,
                    expected_review_version,
                    reason,
                    decision_source: DecisionSource::Human,
                };
                let response =
                    sctx_mcp::candidate_discard_batch_at_root(installation_root()?, &input)?;
                let metadata = Runtime::open()?.index.synchronize()?.metadata;
                let mut data = serde_json::to_value(response)
                    .map_err(json_error("serialize Candidate discard batch response"))?;
                if let Value::Object(ref mut map) = data {
                    map.insert("batch".to_owned(), Value::Bool(true));
                }
                emit("candidate.discard", &metadata, data, json_output)
            }
        }
        [command, rest @ ..] if command == "confirm" => {
            if is_help(rest) {
                println!(
                    "Usage: sctx candidate confirm --input <JSON_FILE>\n  (JSON with candidate_ids confirms several owned Pending Candidates atomically; candidate_id confirms one)"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--input"], &[])?;
            let path = options.required("--input")?;
            let raw: Value = read_json(path, "Candidate Confirmation input")?;
            let has_single = raw.get("candidate_id").is_some();
            let has_batch = raw.get("candidate_ids").is_some();
            match (has_single, has_batch) {
                (true, false) => {
                    let input: CandidateConfirmInput =
                        serde_json::from_value(raw).map_err(|error| {
                            invalid(format!(
                                "invalid Candidate Confirmation input JSON in {path}: {error}"
                            ))
                        })?;
                    let response =
                        sctx_mcp::candidate_confirm_at_root(installation_root()?, &input)?;
                    let metadata = Runtime::open()?.index.synchronize()?.metadata;
                    emit(
                        "candidate.confirm",
                        &metadata,
                        serde_json::to_value(response)
                            .map_err(json_error("serialize Candidate Confirmation response"))?,
                        json_output,
                    )
                }
                (false, true) => {
                    let input: CandidateConfirmBatchInput =
                        serde_json::from_value(raw).map_err(|error| {
                            invalid(format!(
                                "invalid Candidate Confirmation batch input JSON in {path}: {error}"
                            ))
                        })?;
                    let response =
                        sctx_mcp::candidate_confirm_batch_at_root(installation_root()?, &input)?;
                    let metadata = Runtime::open()?.index.synchronize()?.metadata;
                    let mut data = serde_json::to_value(response).map_err(json_error(
                        "serialize Candidate Confirmation batch response",
                    ))?;
                    if let Value::Object(ref mut map) = data {
                        map.insert("batch".to_owned(), Value::Bool(true));
                    }
                    emit("candidate.confirm", &metadata, data, json_output)
                }
                _ => Err(invalid(
                    "Candidate Confirmation input must include exactly one of candidate_id or candidate_ids",
                )),
            }
        }
        [command, rest @ ..] if command == "analyze" => {
            if is_help(rest) {
                println!(
                    "Usage: sctx candidate analyze --candidate-id <ID> [--token-budget <N>] [--top-k <N>]"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--candidate-id", "--token-budget", "--top-k"], &[])?;
            let input = CandidateAnalyzeInput {
                candidate_id: options.required("--candidate-id")?.to_owned(),
                token_budget: parse_usize(
                    options.optional("--token-budget")?.unwrap_or("4096"),
                    "token budget",
                )?,
                top_k: parse_usize(options.optional("--top-k")?.unwrap_or("16"), "top k")?,
            };
            let response = sctx_mcp::candidate_analyze_at_root(installation_root()?, &input)?;
            let metadata = Runtime::open()?.index.synchronize()?.metadata;
            emit(
                "candidate.analyze",
                &metadata,
                serde_json::to_value(response)
                    .map_err(json_error("serialize Candidate analysis response"))?,
                json_output,
            )
        }
        [command, rest @ ..] if command == "build-closed-episode" => {
            if is_help(rest) {
                println!("Usage: sctx candidate build-closed-episode --episode-id <ID>");
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--episode-id"], &[])?;
            let episode_id =
                parse_id::<WorkEpisodeId>(options.required("--episode-id")?, "source episode ID")?;
            let response =
                sctx_mcp::build_closed_episode_at_root(installation_root()?, episode_id)?;
            let metadata = Runtime::open()?.index.synchronize()?.metadata;
            emit(
                "candidate.build-closed-episode",
                &metadata,
                serde_json::to_value(response)
                    .map_err(json_error("serialize Candidate Build response"))?,
                json_output,
            )
        }
        [command, rest @ ..] if command == "stats" => run_candidate_stats(rest, json_output),
        _ => Err(invalid(format!(
            "invalid candidate command; expected list|get|discard|confirm|stats|build-closed-episode|analyze\n\n{CONTEXT_WRITE_HELP}"
        ))),
    }
}

fn run_context(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "revise" => {
            let options = Options::parse(rest, &[])?;
            allow_context_options(
                &options,
                &["--space-id", "--context-id", "--parent-revision-id"],
            )?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let context_id = parse_id(options.required("--context-id")?, "context ID")?;
            let parents = parse_many_ids(&options, "--parent-revision-id", "revision ID")?;
            if parents.is_empty() {
                return Err(invalid(
                    "context revise requires --parent-revision-id for every current Revision Head",
                ));
            }
            let draft = context_draft(&options)?;
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            let context = require_context(&snapshot.projection, space_id, context_id)?;
            require_exact_heads(
                "Context revision",
                &context.revision_heads,
                &parents.iter().copied().collect(),
            )?;
            validate_context_relation_targets(&snapshot.projection, context_id, &draft)?;
            let event = Event::context_revised(space_id, context_id, parents, draft, None)?;
            let (_, revision_id) = context_identity(&event);
            let event_id = event.event_id();
            let (append, metadata) = runtime.append(event)?;
            emit(
                "context.revise",
                &metadata,
                json!({"space_id": space_id, "context_id": context_id,
                       "revision_id": revision_id, "event_id": event_id,
                       "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
                json_output,
            )
        }
        [command, rest @ ..] if command == "review" => run_review(rest, json_output),
        [command, rest @ ..] if command == "publish" => {
            run_publication(rest, PublicationAction::Publish, json_output)
        }
        [command, rest @ ..] if command == "withdraw" => {
            // One `withdraw` with two selectors. The single-Context form is unchanged; naming
            // `--decision-source` switches to the batch form, which reverses a whole class of
            // automatic acceptances at once — the reversal path ADR-0005 requires.
            if rest.iter().any(|argument| argument == "--decision-source") {
                run_withdraw_by_disposition(rest, json_output)
            } else {
                run_publication(rest, PublicationAction::Withdraw, json_output)
            }
        }
        [command, rest @ ..] if command == "get" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--space-id", "--context-id", "--revision-id"], &[])?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let context_id = parse_id(options.required("--context-id")?, "context ID")?;
            let revision_id = options
                .optional("--revision-id")?
                .map(|id| parse_id(id, "revision ID"))
                .transpose()?;
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            let context = require_context(&snapshot.projection, space_id, context_id)?;
            let data = if let Some(revision_id) = revision_id {
                let revision = context.revisions.get(&revision_id).ok_or_else(|| {
                    invalid(format!(
                        "revision {revision_id} does not belong to Context {context_id}"
                    ))
                })?;
                serde_json::to_value(revision).map_err(json_error("serialize revision"))?
            } else {
                serde_json::to_value(context).map_err(json_error("serialize Context"))?
            };
            emit("context.get", &snapshot.metadata, data, json_output)
        }
        _ => Err(invalid(format!(
            "invalid context command; expected revise|review|publish|withdraw|get\n\n{CONTEXT_WRITE_HELP}"
        ))),
    }
}

fn run_review(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(
        &[
            "--space-id",
            "--context-id",
            "--revision-id",
            "--verdict",
            "--reason",
        ],
        &[],
    )?;
    let space_id = parse_id(options.required("--space-id")?, "space ID")?;
    let context_id = parse_id(options.required("--context-id")?, "context ID")?;
    let revision_id = parse_id(options.required("--revision-id")?, "revision ID")?;
    let verdict = match options.required("--verdict")? {
        "approve" => ReviewVerdict::Approve,
        "reject" => ReviewVerdict::Reject,
        value => return Err(invalid(format!("invalid verdict: {value}"))),
    };
    let runtime = Runtime::open()?;
    let snapshot = runtime.domain_snapshot()?;
    let context = require_context(&snapshot.projection, space_id, context_id)?;
    require_revision(context, context_id, revision_id)?;
    let event = Event::context_reviewed(
        space_id,
        context_id,
        ReviewDraft {
            revision_id,
            verdict,
            reason: options.required("--reason")?.to_owned(),
        },
        None,
    )?;
    let review_id = match event.payload() {
        EventPayload::ContextReviewed { review, .. } => review.review_id,
        _ => unreachable!(),
    };
    let event_id = event.event_id();
    let (append, metadata) = runtime.append(event)?;
    emit(
        "context.review",
        &metadata,
        json!({"space_id": space_id, "context_id": context_id, "revision_id": revision_id,
               "review_id": review_id, "event_id": event_id,
               "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
        json_output,
    )
}

#[allow(clippy::too_many_lines)]
fn run_publication(args: &[String], action: PublicationAction, json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--expect-no-publication-head"])?;
    options.allow_only(
        &[
            "--space-id",
            "--context-id",
            "--revision-id",
            "--previous-publication-id",
            "--review-event-id",
        ],
        &["--expect-no-publication-head"],
    )?;
    let space_id = parse_id(options.required("--space-id")?, "space ID")?;
    let context_id = parse_id(options.required("--context-id")?, "context ID")?;
    let revision_id = parse_id(options.required("--revision-id")?, "revision ID")?;
    let previous = parse_many_ids(&options, "--previous-publication-id", "publication ID")?;
    let reviews = parse_many_ids(&options, "--review-event-id", "review event ID")?;
    let expect_none = options.has("--expect-no-publication-head");
    if expect_none != previous.is_empty() {
        return Err(invalid(
            "use exactly one of --expect-no-publication-head or --previous-publication-id",
        ));
    }
    let runtime = Runtime::open()?;
    let snapshot = runtime.domain_snapshot()?;
    let context = require_context(&snapshot.projection, space_id, context_id)?;
    let revision = require_revision(context, context_id, revision_id)?;
    require_exact_heads(
        "Publication",
        &context.publication_heads,
        &previous.iter().copied().collect(),
    )?;

    if action == PublicationAction::Publish {
        if reviews.is_empty() {
            return Err(invalid(
                "publish requires --review-event-id for every deterministic Review",
            ));
        }
        let supplied = reviews.iter().copied().collect::<BTreeSet<_>>();
        if supplied != revision.review_event_ids {
            return Err(invariant(format!(
                "Review references do not equal the deterministic Review set; expected: {}",
                display_ids(&revision.review_event_ids)
            )));
        }
        if revision.review_summary != ReviewSummary::Approved {
            return Err(invariant(format!(
                "revision {revision_id} is not deterministically approved ({:?}); add an explicit review or resolve mixed reviews",
                revision.review_summary
            )));
        }
    } else {
        if !reviews.is_empty() {
            return Err(invalid("withdraw does not accept --review-event-id"));
        }
        match &context.governance {
            ContextGovernanceStatus::Accepted {
                revision_id: current,
                ..
            }
            | ContextGovernanceStatus::Deprecated {
                revision_id: current,
                ..
            } if *current == revision_id => {}
            _ => {
                return Err(invariant(format!(
                    "withdraw requires the revision selected by one deterministic Publication Head; current governance is {:?}",
                    context.governance
                )));
            }
        }
    }

    let event = Event::publication_changed(
        space_id,
        context_id,
        PublicationDraft {
            previous_publication_ids: previous,
            action,
            revision_id,
            review_event_ids: reviews,
        },
        None,
    )?;
    let publication_id = match event.payload() {
        EventPayload::ContextPublicationChanged { publication, .. } => publication.publication_id,
        _ => unreachable!(),
    };
    let event_id = event.event_id();
    let (append, metadata) = runtime.append(event)?;
    let command = if action == PublicationAction::Publish {
        "context.publish"
    } else {
        "context.withdraw"
    };
    emit(
        command,
        &metadata,
        json!({"space_id": space_id, "context_id": context_id, "revision_id": revision_id,
               "publication_id": publication_id, "event_id": event_id,
               "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
        json_output,
    )
}

const WITHDRAW_BY_DISPOSITION_HELP: &str = r"Usage:
  sctx context withdraw --decision-source human|agent_policy
      [--external-session <XSS_ID>] [--dry-run]

Withdraws every accepted Context this installation confirmed under the given disposition.
Each Context is withdrawn through the ordinary Publication event path, one append at a time:
nothing already written is modified, and a Context whose current Publication Head no longer
selects an accepted revision is reported as skipped rather than forced.

The selector is answered from this machine's local runtime, which knows what this installation
decided. A Context confirmed on another machine is not in it and is never touched.
";

/// Reverses a whole class of Candidate dispositions, one ordinary withdrawal at a time.
///
/// The Contexts are selected from the local Runtime, because `decision_source` is provenance this
/// installation recorded about its own decisions. Each withdrawal is then an ordinary
/// `context.publication_changed` append: the batch is a selector over an existing operation, not a
/// new kind of write, so a partial failure leaves every already-withdrawn Context withdrawn and
/// names the one that stopped it.
fn run_withdraw_by_disposition(args: &[String], json_output: bool) -> Result<()> {
    if is_help(args) {
        print!("{WITHDRAW_BY_DISPOSITION_HELP}");
        return Ok(());
    }
    let options = Options::parse(args, &["--dry-run"])?;
    options.allow_only(&["--decision-source", "--external-session"], &["--dry-run"])?;
    let decision_source = DecisionSource::parse(options.required("--decision-source")?)?;
    let external_session_id = options
        .optional("--external-session")?
        .map(|value| parse_id(value, "ExternalSession ID"))
        .transpose()?;
    let dry_run = options.has("--dry-run");

    let root = installation_root()?;
    let tasks = TaskRuntime::initialize(&root)?;
    let selected = tasks.list_confirmed_dispositions(Some(decision_source), external_session_id)?;
    let runtime = Runtime::open_at(&root)?;
    let snapshot = runtime.domain_snapshot()?;

    let mut planned = Vec::new();
    let mut skipped = Vec::new();
    for disposition in &selected {
        match locate_withdrawable(&snapshot.projection, disposition.result_context_id) {
            Some(target) => planned.push(target),
            None => skipped.push(json!({
                "context_id": disposition.result_context_id,
                "candidate_id": disposition.candidate_id,
                "reason": "no accepted Publication Head selects a revision to withdraw; it is already withdrawn, unpublished, or in governance conflict",
            })),
        }
    }

    if dry_run {
        return emit(
            "context.withdraw.batch",
            &snapshot.metadata,
            json!({
                "decision_source": decision_source.as_str(),
                "dry_run": true,
                "selected": selected.len(),
                "planned": planned.iter().map(withdrawal_json).collect::<Vec<_>>(),
                "skipped": skipped,
            }),
            json_output,
        );
    }

    let mut withdrawn = Vec::new();
    let mut metadata = snapshot.metadata;
    for target in &planned {
        let event = Event::publication_changed(
            target.space_id,
            target.context_id,
            PublicationDraft {
                previous_publication_ids: target.previous_publication_ids.clone(),
                action: PublicationAction::Withdraw,
                revision_id: target.revision_id,
                review_event_ids: Vec::new(),
            },
            None,
        )?;
        let event_id = event.event_id();
        let (append, appended) = runtime.append(event)?;
        metadata = appended;
        let mut entry = withdrawal_json(target);
        entry["event_id"] = json!(event_id);
        entry["batch_id"] = json!(append.batch_id);
        entry["commit_oid"] = json!(append.commit_oid);
        withdrawn.push(entry);
    }
    emit(
        "context.withdraw.batch",
        &metadata,
        json!({
            "decision_source": decision_source.as_str(),
            "dry_run": false,
            "selected": selected.len(),
            "withdrawn": withdrawn,
            "skipped": skipped,
        }),
        json_output,
    )
}

/// One accepted Context resolved to everything an ordinary withdrawal needs.
struct WithdrawalTarget {
    space_id: SpaceId,
    context_id: ContextId,
    revision_id: RevisionId,
    previous_publication_ids: Vec<PublicationId>,
}

fn withdrawal_json(target: &WithdrawalTarget) -> Value {
    json!({
        "space_id": target.space_id,
        "context_id": target.context_id,
        "revision_id": target.revision_id,
    })
}

/// Finds the Space and revision one accepted Context would be withdrawn from.
///
/// Only an `Accepted` Context is a target. `Deprecated` is what a withdrawal already produced, so
/// selecting it would append a second withdrawal of something already withdrawn and make re-running
/// the selector grow history for no change; `Unpublished` and a governance conflict are exactly the
/// cases the single-Context `withdraw` refuses, and a batch must not do quietly what the explicit
/// command refuses to do at all. Anything skipped is reported, never silently dropped.
fn locate_withdrawable(
    projection: &DomainProjection,
    context_id: ContextId,
) -> Option<WithdrawalTarget> {
    projection.spaces.iter().find_map(|(space_id, space)| {
        let context = space.contexts.get(&context_id)?;
        let ContextGovernanceStatus::Accepted { revision_id, .. } = context.governance else {
            return None;
        };
        Some(WithdrawalTarget {
            space_id: *space_id,
            context_id,
            revision_id,
            previous_publication_ids: context.publication_heads.iter().copied().collect(),
        })
    })
}

/// Reports this installation's own disposition totals, grouped by who decided them.
fn run_candidate_stats(args: &[String], json_output: bool) -> Result<()> {
    if is_help(args) {
        println!("Usage: sctx candidate stats");
        return Ok(());
    }
    let options = Options::parse(args, &[])?;
    options.allow_only(&[], &[])?;
    let root = installation_root()?;
    let stats = TaskRuntime::initialize(&root)?.candidate_disposition_stats()?;
    let data = json!({
        "human": {
            "confirmed": stats.human.confirmed,
            "discarded": stats.human.discarded,
        },
        "agent_policy": {
            "confirmed": stats.agent_policy.confirmed,
            "discarded": stats.agent_policy.discarded,
        },
        "auto_confirm_not_permitted": stats.auto_confirm_not_permitted,
    });
    // The counts are Runtime-only, but the envelope stays the one every `candidate` command
    // shares, so a caller reads one shape across the group.
    let metadata = Runtime::open_at(&root)?.index.synchronize()?.metadata;
    emit("candidate.stats", &metadata, data, json_output)
}

fn run_semantic(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [conflict, command, rest @ ..] if conflict == "conflict" && command == "open" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(
                &[
                    "--space-id",
                    "--participant",
                    "--reason",
                    "--domain",
                    "--platform",
                    "--condition",
                ],
                &[],
            )?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let participants = options
                .many("--participant")
                .into_iter()
                .map(parse_participant)
                .collect::<Result<Vec<_>>>()?;
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            for participant in &participants {
                let context =
                    require_context(&snapshot.projection, space_id, participant.context_id)?;
                match &context.governance {
                    ContextGovernanceStatus::Accepted {
                        publication_id,
                        revision_id,
                    } if *publication_id == participant.publication_id
                        && *revision_id == participant.revision_id => {}
                    _ => {
                        return Err(invariant(format!(
                            "participant {}:{}:{} is not the current deterministic Accepted head; refresh with `sctx context get`",
                            participant.context_id,
                            participant.revision_id,
                            participant.publication_id
                        )));
                    }
                }
            }
            let event = Event::semantic_conflict_opened(
                space_id,
                SemanticConflictDraft {
                    participants,
                    reason: options.required("--reason")?.to_owned(),
                    applicability: applicability(&options),
                },
                None,
            )?;
            let conflict_id = match event.payload() {
                EventPayload::SemanticConflictOpened { conflict, .. } => conflict.conflict_id,
                _ => unreachable!(),
            };
            let event_id = event.event_id();
            let (append, metadata) = runtime.append(event)?;
            emit(
                "semantic.conflict.open",
                &metadata,
                json!({"space_id": space_id, "conflict_id": conflict_id, "event_id": event_id,
                       "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
                json_output,
            )
        }
        [conflict, command, rest @ ..] if conflict == "conflict" && command == "resolve" => {
            run_conflict_resolve(rest, json_output)
        }
        _ => Err(invalid(
            "invalid semantic command; expected `semantic conflict open|resolve`",
        )),
    }
}

#[allow(clippy::too_many_lines)]
fn run_conflict_resolve(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--expect-no-resolution-head"])?;
    options.allow_only(
        &[
            "--space-id",
            "--conflict-id",
            "--previous-resolution-id",
            "--related-publication-id",
            "--result",
            "--rationale",
        ],
        &["--expect-no-resolution-head"],
    )?;
    let space_id = parse_id(options.required("--space-id")?, "space ID")?;
    let conflict_id = parse_id(options.required("--conflict-id")?, "conflict ID")?;
    let previous = parse_many_ids(&options, "--previous-resolution-id", "resolution ID")?;
    let expect_none = options.has("--expect-no-resolution-head");
    if expect_none != previous.is_empty() {
        return Err(invalid(
            "use exactly one of --expect-no-resolution-head or --previous-resolution-id",
        ));
    }
    let related = parse_many_ids(&options, "--related-publication-id", "publication ID")?;
    let results = options
        .many("--result")
        .into_iter()
        .map(parse_resolution_result)
        .collect::<Result<Vec<_>>>()?;
    let runtime = Runtime::open()?;
    let snapshot = runtime.domain_snapshot()?;
    let conflict = snapshot
        .projection
        .semantic_conflicts
        .get(&conflict_id)
        .ok_or_else(|| invalid(format!("semantic conflict does not exist: {conflict_id}")))?;
    if conflict.space_id != space_id {
        return Err(invalid(format!(
            "semantic conflict {conflict_id} does not belong to Space {space_id}"
        )));
    }
    require_exact_heads(
        "Resolution",
        &conflict.resolution_heads,
        &previous.iter().copied().collect(),
    )?;
    let mut current_publications = BTreeSet::new();
    for participant in &conflict.conflict.participants {
        let context = require_context(&snapshot.projection, space_id, participant.context_id)?;
        match &context.governance {
            ContextGovernanceStatus::Accepted { publication_id, .. }
            | ContextGovernanceStatus::Deprecated { publication_id, .. } => {
                current_publications.insert(*publication_id);
            }
            _ => {
                return Err(invariant(format!(
                    "Context {} has no deterministic Publication Head; converge governance before resolving semantic conflict",
                    participant.context_id
                )));
            }
        }
    }
    let supplied = related.iter().copied().collect::<BTreeSet<_>>();
    if supplied != current_publications {
        return Err(invariant(format!(
            "related Publication references are stale or incomplete; expected: {}",
            display_ids(&current_publications)
        )));
    }
    let participant_contexts = conflict
        .conflict
        .participants
        .iter()
        .map(|participant| participant.context_id)
        .collect::<BTreeSet<_>>();
    let result_contexts = results
        .iter()
        .map(|result| result.context_id)
        .collect::<BTreeSet<_>>();
    if result_contexts.len() != results.len() || result_contexts != participant_contexts {
        return Err(invariant(format!(
            "resolution results must name every conflict participant exactly once; expected Contexts: {}, supplied Contexts: {}",
            display_ids(&participant_contexts),
            display_ids(&result_contexts)
        )));
    }
    for result in &results {
        let context = require_context(&snapshot.projection, space_id, result.context_id)?;
        require_revision(context, result.context_id, result.revision_id)?;
    }
    let event = Event::semantic_conflict_resolution_added(
        space_id,
        conflict_id,
        ConflictResolutionDraft {
            previous_resolution_ids: previous,
            related_publication_ids: related,
            results,
            rationale: options.required("--rationale")?.to_owned(),
        },
        None,
    )?;
    let resolution_id = match event.payload() {
        EventPayload::SemanticConflictResolutionAdded { resolution, .. } => {
            resolution.resolution_id
        }
        _ => unreachable!(),
    };
    let event_id = event.event_id();
    let (append, metadata) = runtime.append(event)?;
    emit(
        "semantic.conflict.resolve",
        &metadata,
        json!({"space_id": space_id, "conflict_id": conflict_id,
               "resolution_id": resolution_id, "event_id": event_id,
               "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
        json_output,
    )
}

fn run_search(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--exact"])?;
    allow_search_options(&options, &["--exact"])?;
    let request = search_request(&options)?;
    let runtime = Runtime::open()?;
    let response = SearchEngine::new(runtime.index).search(&request)?;
    let data = serde_json::to_value(&response).map_err(json_error("serialize search response"))?;
    emit_raw(
        "search",
        &response.indexed_tree_oid,
        response.projection_generation,
        &data,
        json_output,
    )
}

fn run_task(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "context" => run_task_context(rest, json_output),
        [command, rest @ ..] if command == "artifact-focus" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--input"], &[])?;
            let input: ArtifactFocusQuery =
                read_json(options.required("--input")?, "Task Artifact Focus")?;
            let response = sctx_mcp::task_artifact_focus_at_root(installation_root()?, &input)?;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Task Artifact Focus response"))?;
            emit_raw(
                "task.artifact-focus",
                &response.context.tree,
                response.context.generation,
                &data,
                json_output,
            )
        }
        [command, rest @ ..] if command == "checkpoint" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--input"], &[])?;
            let input: TaskCheckpointInput =
                read_json(options.required("--input")?, "Task Checkpoint")?;
            let response = sctx_mcp::task_checkpoint_at_root(installation_root()?, &input)?;
            let metadata = Runtime::open()?.index.synchronize()?.metadata;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Task Checkpoint response"))?;
            emit("task.checkpoint", &metadata, data, json_output)
        }
        [group, command, rest @ ..] if group == "intent" && command == "update" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--input"], &[])?;
            let input: TaskIntentUpdateInput =
                read_json(options.required("--input")?, "Working Intent update")?;
            let response = sctx_mcp::task_intent_update_at_root(installation_root()?, &input)?;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Working Intent update response"))?;
            emit_raw(
                "task.intent.update",
                &response.context.tree,
                response.context.generation,
                &data,
                json_output,
            )
        }
        [group, command, rest @ ..] if group == "signal" && command == "supersede" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--input"], &[])?;
            let input: TaskSignalSupersedeInput =
                read_json(options.required("--input")?, "Task Signal supersede")?;
            let response = sctx_mcp::task_signal_supersede_at_root(installation_root()?, &input)?;
            let active = sctx_task_runtime::TaskRuntime::initialize(installation_root()?)?
                .read_snapshot(response.task_session_id)?
                .ok_or_else(|| invariant("updated ActiveTask disappeared"))?;
            let metadata = Runtime::open()?.index.synchronize()?.metadata;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Task Signal supersede response"))?;
            debug_assert_eq!(active.task_id, response.task_id);
            emit("task.signal.supersede", &metadata, data, json_output)
        }
        _ => Err(invalid(
            "Usage: sctx task context|artifact-focus|checkpoint|intent update|signal supersede [OPTIONS]",
        )),
    }
}

fn run_task_context(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--compact"])?;
    options.allow_only(
        &[
            "--agent-kind",
            "--external-session-id",
            "--token-budget",
            "--max-spaces",
        ],
        &["--compact"],
    )?;
    let input = TaskContextReadInput {
        agent_kind: options.required("--agent-kind")?.to_owned(),
        external_session_id: options.required("--external-session-id")?.to_owned(),
        token_budget: parse_usize(
            options.optional("--token-budget")?.unwrap_or("2000"),
            "token budget",
        )?,
        max_spaces: parse_usize(
            options.optional("--max-spaces")?.unwrap_or("8"),
            "max spaces",
        )?,
    };
    // `--compact` mirrors the MCP `detail_level: "compact"` selector: the Full Rust entry point
    // stays the default so existing behavior is unchanged when the flag is absent.
    let detail_level = if options.has("--compact") {
        ContextPackDetailLevel::Compact
    } else {
        ContextPackDetailLevel::Full
    };
    let response = sctx_mcp::task_context_readonly_with_detail_at_root(
        installation_root()?,
        &input,
        detail_level,
    )?;
    let (tree, generation, data) = match detail_level {
        ContextPackDetailLevel::Compact => {
            let compact = response.compact();
            let data = serde_json::to_value(&compact)
                .map_err(json_error("serialize compact Task Context response"))?;
            (compact.tree.clone(), compact.generation, data)
        }
        ContextPackDetailLevel::Full => {
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Task Context response"))?;
            (response.tree.clone(), response.generation, data)
        }
    };
    emit_raw("task.context", &tree, generation, &data, json_output)
}

fn run_repository(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid(
            "Usage: sctx repository add|list|doctor|rename|scan [OPTIONS]",
        ));
    };
    let root = installation_root()?;
    match command.as_str() {
        "add" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--repository-id", "--path"], &[])?;
            let repository_id =
                parse_id::<RepositoryId>(options.required("--repository-id")?, "Repository ID")?;
            let paths = options
                .many("--path")
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let outcome =
                UserConfigStore::initialize(&root)?.add_repository(repository_id, &paths)?;
            let sync = sctx_mcp::sync_repository_catalog_at_root(&root)?;
            // A Repository nothing ever scanned is indistinguishable from a broken one: every
            // Reference naming it resolves against "not registered" until somebody happens to run
            // a rebuild. Registration is explicit and a person is watching it, so it pays for the
            // first read here rather than leaving the Graph dark and silent about why.
            let first_scan = sctx_mcp::repository_first_scan_at_root(&root)?;
            let metadata = repository_command_metadata(&root)?;
            emit(
                "repository.add",
                &metadata,
                json!({"catalog": outcome, "registry": sync, "first_scan": first_scan}),
                json_output,
            )
        }
        "list" => run_repository_list(rest, json_output, &root),
        "doctor" => run_repository_doctor(rest, json_output, &root),
        "rename" => run_repository_rename(rest, json_output, &root),
        "scan" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--checkout-path", "--path", "--max-artifacts"], &[])?;
            let input = RepositoryScanInput {
                checkout_path: options.required("--checkout-path")?.to_owned(),
                paths: options
                    .many("--path")
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                max_artifacts: parse_usize(
                    options.optional("--max-artifacts")?.unwrap_or("200"),
                    "max artifacts",
                )?,
            };
            let response = sctx_mcp::repository_scan_at_root(&root, &input)?;
            let data =
                serde_json::to_value(&response).map_err(json_error("serialize Repository scan"))?;
            emit_raw(
                "repository.scan",
                &response.tree,
                response.generation,
                &data,
                json_output,
            )
        }
        _ => Err(invalid(
            "repository command must be add, list, doctor, rename, or scan",
        )),
    }
}

fn run_repository_list(args: &[String], json_output: bool, root: &Path) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&[], &[])?;
    let catalog = UserConfigStore::open_existing(root)?.inspect_repository_catalog()?;
    let sync = sctx_mcp::sync_repository_catalog_at_root(root)?;
    let metadata = repository_repair_command_metadata(root)?;
    emit(
        "repository.list",
        &metadata,
        json!({
            "repositories": catalog.repositories,
            "activation": catalog.activation,
            "registry": sync,
        }),
        json_output,
    )
}

fn run_repository_doctor(args: &[String], json_output: bool, root: &Path) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&[], &[])?;
    let report = UserConfigStore::open_existing(root)?.doctor_repository_catalog()?;
    let syncable = report.checkouts.iter().all(|checkout| {
        matches!(
            checkout.status,
            CatalogCheckoutStatus::Available | CatalogCheckoutStatus::Missing
        )
    });
    let sync = syncable
        .then(|| sctx_mcp::sync_repository_catalog_at_root(root))
        .transpose()?;
    let legacy_engineering_reference_counts =
        legacy_repository_engineering_reference_counts(root, &report.diagnostics)?;
    let metadata = repository_repair_command_metadata(root)?;
    emit(
        "repository.doctor",
        &metadata,
        json!({
            "catalog": report,
            "registry": sync,
            "legacy_repository_engineering_reference_counts": legacy_engineering_reference_counts,
        }),
        json_output,
    )
}

/// Counts, per legacy (pre-ADR-0001) `RepositoryId`, how many locally indexed
/// `EngineeringReference` events still name it. This is informational only: it
/// never rewrites Git history and a missing/uninitialized Store simply reports
/// zero counts rather than failing `sctx repository doctor`.
fn legacy_repository_engineering_reference_counts(
    root: &Path,
    diagnostics: &[RepositoryCatalogDiagnostic],
) -> Result<BTreeMap<String, usize>> {
    let mut counts = diagnostics
        .iter()
        .map(|diagnostic| {
            let RepositoryCatalogDiagnostic::LegacyRepositoryId { repository_id, .. } = diagnostic;
            (repository_id.to_string(), 0usize)
        })
        .collect::<BTreeMap<_, _>>();
    if counts.is_empty() || !root.join("repository").exists() {
        return Ok(counts);
    }
    let projection =
        ProjectionIndex::new(root.join("repository"), root.join("state")).domain_snapshot()?;
    for reference in projection.projection.engineering_references.values() {
        if let Some(count) = counts.get_mut(reference.reference.repository_id.as_str()) {
            *count += 1;
        }
    }
    Ok(counts)
}

fn run_repository_rename(args: &[String], json_output: bool, root: &Path) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--from", "--to"], &[])?;
    let from = parse_id::<RepositoryId>(options.required("--from")?, "Repository ID")?;
    let to = parse_id::<RepositoryId>(options.required("--to")?, "Repository ID")?;
    let outcome = UserConfigStore::open_existing(root)?.rename_repository(&from, &to)?;
    let sync = sctx_mcp::sync_repository_catalog_at_root(root)?;
    let metadata = repository_repair_command_metadata(root)?;
    emit(
        "repository.rename",
        &metadata,
        json!({"catalog": outcome, "registry": sync}),
        json_output,
    )
}

fn repository_command_metadata(root: &Path) -> Result<IndexMetadata> {
    let store = GitStore::open_existing(root)?;
    Ok(ProjectionIndex::for_store(&store).synchronize()?.metadata)
}

fn repository_repair_command_metadata(root: &Path) -> Result<IndexMetadata> {
    if !root.join("repository").exists() {
        return repository_command_metadata(root);
    }
    Ok(
        ProjectionIndex::new(root.join("repository"), root.join("state"))
            .synchronize()?
            .metadata,
    )
}

fn run_engineering_reference(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid(
            "Usage: sctx engineering-reference record --input <JSON>",
        ));
    };
    if command != "record" {
        return Err(invalid("engineering-reference command must be record"));
    }
    let options = Options::parse(rest, &[])?;
    options.allow_only(&["--input"], &[])?;
    let input: EngineeringReferenceRecordInput =
        read_json(options.required("--input")?, "Engineering Reference record")?;
    let response = sctx_mcp::engineering_reference_record_at_root(installation_root()?, &input)?;
    let data = serde_json::to_value(&response)
        .map_err(json_error("serialize Engineering Reference record"))?;
    emit_raw(
        "engineering-reference.record",
        &response.tree,
        response.generation,
        &data,
        json_output,
    )
}

fn run_association(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "explain" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--reference-id"], &[])?;
            let response = sctx_mcp::association_explain_at_root(
                installation_root()?,
                &AssociationExplainInput {
                    reference_id: options.required("--reference-id")?.to_owned(),
                },
            )?;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Association explanation"))?;
            emit_raw(
                "association.explain",
                &response.tree,
                response.generation,
                &data,
                json_output,
            )
        }
        [command, rest @ ..] if command == "rebuild" => {
            let options = Options::parse(rest, &["--diagnose"])?;
            options.allow_only(&[], &["--diagnose"])?;
            let response = sctx_mcp::association_rebuild_at_root(
                installation_root()?,
                &AssociationRebuildInput {
                    diagnose_only: options.has("--diagnose"),
                },
            )?;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Association rebuild"))?;
            emit_raw(
                if response.diagnose_only {
                    "association.diagnose"
                } else {
                    "association.rebuild"
                },
                &response.tree,
                response.generation,
                &data,
                json_output,
            )
        }
        _ => Err(invalid("Usage: sctx association explain|rebuild [OPTIONS]")),
    }
}

fn run_index(args: &[String], json_output: bool) -> Result<()> {
    let runtime = Runtime::open()?;
    match args {
        [command] if command == "rebuild" => {
            let outcome = runtime.index.rebuild()?;
            let diagnostics = runtime.domain_snapshot()?.diagnostics;
            emit_rebuild("index.rebuild", &outcome, &diagnostics, json_output)
        }
        [command] if command == "status" => {
            let outcome = runtime.index.synchronize()?;
            let diagnostics = runtime.domain_snapshot()?.diagnostics;
            emit_rebuild("index.status", &outcome, &diagnostics, json_output)
        }
        _ => Err(invalid("invalid index command; expected rebuild|status")),
    }
}

fn run_pending(args: &[String], json_output: bool) -> Result<()> {
    let runtime = Runtime::open()?;
    match args {
        [command] if command == "list" => {
            let pending = runtime.store.list_pending()?;
            let metadata = runtime.index.synchronize()?.metadata;
            let batches = pending
                .into_iter()
                .map(|batch| {
                    json!({"batch_id": batch.batch_id, "event_id": batch.event_id,
                           "phase": format!("{:?}", batch.phase).to_lowercase(),
                           "commit_oid": batch.commit_oid, "files": batch.files})
                })
                .collect::<Vec<_>>();
            emit(
                "pending.list",
                &metadata,
                json!({"batches": batches}),
                json_output,
            )
        }
        [command, batch] if command == "commit" => {
            let batch_id = BatchId::from_str(batch)?;
            let append = runtime.store.commit_pending(&batch_id)?;
            let metadata = runtime.index.synchronize()?.metadata;
            emit(
                "pending.commit",
                &metadata,
                append_json(&append),
                json_output,
            )
        }
        [command, batch] if command == "move-aside" => {
            let batch_id = BatchId::from_str(batch)?;
            let destination = runtime.store.move_pending_aside(&batch_id)?;
            let metadata = runtime.index.synchronize()?.metadata;
            emit(
                "pending.move-aside",
                &metadata,
                json!({"batch_id": batch_id, "destination": destination}),
                json_output,
            )
        }
        _ => Err(invalid(
            "invalid pending command; expected list|commit <BATCH_ID>|move-aside <BATCH_ID>",
        )),
    }
}

fn run_validate(args: &[String], json_output: bool) -> Result<()> {
    if args != ["--staged"] {
        return Err(invalid("validate requires exactly --staged"));
    }
    let runtime = Runtime::open()?;
    let validation = runtime.store.validate_staged()?;
    let metadata = runtime.index.synchronize()?.metadata;
    let data = json!({"event_count": validation.event_count, "added_paths": validation.added_paths,
                     "indexed_tree_oid": metadata.indexed_tree_oid});
    emit_raw(
        "validate.staged",
        &validation.tree_oid,
        metadata.projection_generation,
        &data,
        json_output,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextDraftInput {
    kind: ContextKind,
    #[serde(default)]
    topic_key: Option<String>,
    /// Optional restatement of the problem this Context answers (WP-E1's
    /// `ContextRevisionDraft::problem_view`), passed through verbatim when supplied.
    #[serde(default)]
    problem_view: Option<String>,
    statement: String,
    rationale: String,
    #[serde(default)]
    applicability: Applicability,
    #[serde(default)]
    assumptions: Vec<String>,
    #[serde(default)]
    recheck_when: Vec<String>,
    /// Unresolved locator hints (paths, basenames, identifiers) kept as searchable text only.
    #[serde(default)]
    hints: Vec<String>,
    #[serde(default)]
    relations: Vec<sctx_domain::ContextRelation>,
    evidence: Vec<EvidenceInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceInput {
    kind: EvidenceType,
    supports: String,
    content: Value,
    interpretation: String,
    #[serde(default)]
    limitations: Vec<String>,
}

impl From<ContextDraftInput> for ContextRevisionDraft {
    fn from(input: ContextDraftInput) -> Self {
        Self {
            kind: input.kind,
            topic_key: input.topic_key,
            problem_view: input.problem_view,
            hints: input.hints,
            statement: input.statement,
            rationale: input.rationale,
            applicability: input.applicability,
            assumptions: input.assumptions,
            recheck_when: input.recheck_when,
            relations: input.relations,
            evidence: input.evidence.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<EvidenceInput> for EvidenceSnapshotDraft {
    fn from(input: EvidenceInput) -> Self {
        Self {
            kind: input.kind,
            supports: input.supports,
            content: input.content,
            interpretation: input.interpretation,
            limitations: input.limitations,
        }
    }
}

fn intent_from_options(options: &Options) -> Result<IntentSnapshot> {
    if let Some(path) = options.optional("--input")? {
        reject_content_flags_with_input(
            options,
            &[
                "--title",
                "--problem",
                "--desired-outcome",
                "--in-scope",
                "--out-of-scope",
                "--acceptance-condition",
                "--domain-term",
            ],
        )?;
        return read_json(path, "Intent input");
    }
    Ok(IntentSnapshot {
        title: options.required("--title")?.to_owned(),
        problem: options.required("--problem")?.to_owned(),
        desired_outcome: options.required("--desired-outcome")?.to_owned(),
        in_scope: strings(options.many("--in-scope")),
        out_of_scope: strings(options.many("--out-of-scope")),
        acceptance_conditions: strings(options.many("--acceptance-condition")),
        domain_terms: strings(options.many("--domain-term")),
    })
}

fn context_draft(options: &Options) -> Result<ContextRevisionDraft> {
    if let Some(path) = options.optional("--input")? {
        reject_content_flags_with_input(
            options,
            &[
                "--kind",
                "--topic-key",
                "--statement",
                "--rationale",
                "--domain",
                "--platform",
                "--condition",
                "--assumption",
                "--recheck-when",
                "--evidence-json",
            ],
        )?;
        return read_json::<ContextDraftInput>(path, "Context input").map(Into::into);
    }
    let evidence = options
        .many("--evidence-json")
        .into_iter()
        .map(|value| {
            serde_json::from_str::<EvidenceInput>(value)
                .map(Into::into)
                .map_err(|error| invalid(format!("invalid --evidence-json: {error}")))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ContextRevisionDraft {
        // The individual-flag form of `context revise` has no flag for a derived problem
        // framing or unresolved locator hints; use `--input` with `problem_view`/`hints` to set
        // them (WP-E1's `ContextRevisionDraft` fields).
        problem_view: None,
        hints: Vec::new(),
        kind: parse_kind(options.required("--kind")?)?,
        topic_key: options.optional("--topic-key")?.map(ToOwned::to_owned),
        statement: options.required("--statement")?.to_owned(),
        rationale: options.required("--rationale")?.to_owned(),
        applicability: applicability(options),
        assumptions: strings(options.many("--assumption")),
        recheck_when: strings(options.many("--recheck-when")),
        relations: Vec::new(),
        evidence,
    })
}

fn applicability(options: &Options) -> Applicability {
    Applicability {
        domains: strings(options.many("--domain")),
        platforms: strings(options.many("--platform")),
        conditions: strings(options.many("--condition")),
    }
}

fn search_request(options: &Options) -> Result<SearchRequest> {
    Ok(SearchRequest {
        // `--exact` keeps the strict all-token lookup; the default recalls a known fact that the
        // caller phrased differently.
        match_mode: if options.has("--exact") {
            SearchMatchMode::Exact
        } else {
            SearchMatchMode::Ranked
        },
        query: options.optional("--query")?.unwrap_or_default().to_owned(),
        filters: SearchFilters {
            space_ids: parse_many_ids(options, "--space-id", "space ID")?,
            scope: ScopeFilter {
                domains: strings(options.many("--domain")),
                platforms: strings(options.many("--platform")),
                conditions: strings(options.many("--condition")),
            },
            kinds: options
                .many("--kind")
                .into_iter()
                .map(parse_kind)
                .collect::<Result<_>>()?,
            statuses: options
                .many("--status")
                .into_iter()
                .map(parse_status)
                .collect::<Result<_>>()?,
        },
        page_size: parse_usize(
            options.optional("--page-size")?.unwrap_or("20"),
            "page size",
        )?,
        cursor: options.optional("--cursor")?.map(ToOwned::to_owned),
    })
}

fn allow_intent_options(options: &Options, extra: &[&str]) -> Result<()> {
    let mut allowed = vec![
        "--input",
        "--title",
        "--problem",
        "--desired-outcome",
        "--in-scope",
        "--out-of-scope",
        "--acceptance-condition",
        "--domain-term",
    ];
    allowed.extend_from_slice(extra);
    options.allow_only(&allowed, &[])
}

fn allow_context_options(options: &Options, extra: &[&str]) -> Result<()> {
    let mut allowed = vec![
        "--input",
        "--kind",
        "--topic-key",
        "--statement",
        "--rationale",
        "--domain",
        "--platform",
        "--condition",
        "--assumption",
        "--recheck-when",
        "--evidence-json",
    ];
    allowed.extend_from_slice(extra);
    options.allow_only(&allowed, &[])
}

fn allow_search_options(options: &Options, extra: &[&str]) -> Result<()> {
    let mut allowed = vec![
        "--query",
        "--space-id",
        "--domain",
        "--platform",
        "--condition",
        "--kind",
        "--status",
        "--page-size",
        "--cursor",
    ];
    let mut switches = Vec::new();
    for option in extra {
        if matches!(*option, "--automatic" | "--exact") {
            switches.push(*option);
        } else {
            allowed.push(*option);
        }
    }
    options.allow_only(&allowed, &switches)
}

fn reject_content_flags_with_input(options: &Options, names: &[&str]) -> Result<()> {
    if let Some(name) = names.iter().find(|name| !options.many(name).is_empty()) {
        return Err(invalid(format!("{name} cannot be combined with --input")));
    }
    Ok(())
}

fn require_context(
    projection: &DomainProjection,
    space_id: SpaceId,
    context_id: ContextId,
) -> Result<&sctx_domain::ContextProjection> {
    projection
        .spaces
        .get(&space_id)
        .ok_or_else(|| invalid(format!("space does not exist: {space_id}")))?
        .contexts
        .get(&context_id)
        .ok_or_else(|| {
            invalid(format!(
                "Context {context_id} does not belong to Space {space_id}"
            ))
        })
}

fn validate_context_relation_targets(
    projection: &DomainProjection,
    source_context_id: ContextId,
    draft: &ContextRevisionDraft,
) -> Result<()> {
    for relation in &draft.relations {
        if relation.target_context_id == source_context_id {
            return Err(invalid("Context Relation cannot target its source Context"));
        }
        let exists = projection
            .spaces
            .values()
            .any(|space| space.contexts.contains_key(&relation.target_context_id));
        if !exists {
            return Err(invalid(format!(
                "Context Relation target does not exist: {}",
                relation.target_context_id
            )));
        }
    }
    Ok(())
}

fn require_revision(
    context: &sctx_domain::ContextProjection,
    context_id: ContextId,
    revision_id: RevisionId,
) -> Result<&sctx_domain::RevisionProjection> {
    context.revisions.get(&revision_id).ok_or_else(|| {
        invalid(format!(
            "revision {revision_id} does not belong to Context {context_id}"
        ))
    })
}

fn require_exact_heads<T>(name: &str, actual: &BTreeSet<T>, supplied: &BTreeSet<T>) -> Result<()>
where
    T: Ord + ToString,
{
    if actual == supplied {
        return Ok(());
    }
    Err(invariant(format!(
        "{name} Head precondition failed; expected [{}], supplied [{}]. Refresh with the corresponding get command and retry with every current Head",
        display_ids(actual),
        display_ids(supplied)
    )))
}

fn display_ids<T: ToString>(ids: &BTreeSet<T>) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_participant(value: &str) -> Result<ConflictParticipant> {
    let fields = value.split(':').collect::<Vec<_>>();
    let [context, revision, publication] = fields.as_slice() else {
        return Err(invalid(
            "--participant must be CONTEXT_ID:REVISION_ID:PUBLICATION_ID",
        ));
    };
    Ok(ConflictParticipant {
        context_id: parse_id(context, "participant Context ID")?,
        revision_id: parse_id(revision, "participant Revision ID")?,
        publication_id: parse_id(publication, "participant Publication ID")?,
    })
}

fn parse_resolution_result(value: &str) -> Result<ConflictResolutionResult> {
    let fields = value.split(':').collect::<Vec<_>>();
    let [context, revision, outcome] = fields.as_slice() else {
        return Err(invalid(
            "--result must be CONTEXT_ID:REVISION_ID:retained|revised|withdrawn|scope_split",
        ));
    };
    let outcome = match *outcome {
        "retained" => ResolutionOutcome::Retained,
        "revised" => ResolutionOutcome::Revised,
        "withdrawn" => ResolutionOutcome::Withdrawn,
        "scope_split" => ResolutionOutcome::ScopeSplit,
        value => return Err(invalid(format!("invalid resolution outcome: {value}"))),
    };
    Ok(ConflictResolutionResult {
        context_id: parse_id(context, "result Context ID")?,
        revision_id: parse_id(revision, "result Revision ID")?,
        outcome,
    })
}

fn parse_kind(value: &str) -> Result<ContextKind> {
    match value {
        "decision" => Ok(ContextKind::Decision),
        "contract" => Ok(ContextKind::Contract),
        "issue" => Ok(ContextKind::Issue),
        "risk" => Ok(ContextKind::Risk),
        "validation" => Ok(ContextKind::Validation),
        "discovery" => Ok(ContextKind::Discovery),
        "progress" => Ok(ContextKind::Progress),
        _ => Err(invalid(format!("invalid Context kind: {value}"))),
    }
}

fn parse_status(value: &str) -> Result<ContextStatus> {
    match value {
        "candidate" => Ok(ContextStatus::Candidate),
        "accepted" => Ok(ContextStatus::Accepted),
        "deprecated" => Ok(ContextStatus::Deprecated),
        "superseded" => Ok(ContextStatus::Superseded),
        "governance_conflict" => Ok(ContextStatus::GovernanceConflict),
        _ => Err(invalid(format!("invalid Context status: {value}"))),
    }
}

/// Mirrors the MCP `scope` selector: `task` is the Task-local default, `session` widens the
/// listing to every Task of the Session read-only.
fn parse_candidate_review_scope(value: &str) -> Result<sctx_domain::CandidateReviewScope> {
    match value {
        "task" => Ok(sctx_domain::CandidateReviewScope::Task),
        "session" => Ok(sctx_domain::CandidateReviewScope::Session),
        other => Err(invalid(format!(
            "unsupported Candidate Review scope: {other}"
        ))),
    }
}

fn parse_candidate_review_status(value: &str) -> Result<CandidateReviewStatus> {
    match value {
        "pending" => Ok(CandidateReviewStatus::Pending),
        "discarded" => Ok(CandidateReviewStatus::Discarded),
        "expired" => Ok(CandidateReviewStatus::Expired),
        "confirmed" => Ok(CandidateReviewStatus::Confirmed),
        _ => Err(invalid(format!("invalid Candidate Review status: {value}"))),
    }
}

fn parse_many_ids<T>(options: &Options, option: &str, field: &str) -> Result<Vec<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    options
        .many(option)
        .into_iter()
        .map(|value| parse_id(value, field))
        .collect()
}

fn parse_id<T>(value: &str, field: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| invalid(format!("invalid {field}: {error}")))
}

fn parse_usize(value: &str, field: &str) -> Result<usize> {
    value
        .parse()
        .map_err(|error| invalid(format!("invalid {field}: {error}")))
}

fn parse_u64(value: &str, field: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|error| invalid(format!("invalid {field}: {error}")))
}

fn strings(values: Vec<&str>) -> Vec<String> {
    values.into_iter().map(ToOwned::to_owned).collect()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &str, field: &str) -> Result<T> {
    let bytes = fs::read(path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("failed to read {field} {path}: {error}"),
        )
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid {field} JSON in {path}: {error}")))
}

fn context_identity(event: &Event) -> (ContextId, RevisionId) {
    match event.payload() {
        EventPayload::ContextRevisionAdded {
            context_id,
            revision,
            ..
        } => (*context_id, revision.revision_id),
        _ => unreachable!(),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn emit(command: &str, metadata: &IndexMetadata, data: Value, json_output: bool) -> Result<()> {
    emit_raw(
        command,
        &metadata.indexed_tree_oid,
        metadata.projection_generation,
        &data,
        json_output,
    )
}

fn emit_raw(
    command: &str,
    tree: &str,
    generation: u64,
    data: &Value,
    json_output: bool,
) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "command": command,
                "tree": tree,
                "generation": generation,
                "data": data,
            }))
            .map_err(json_error("serialize output"))?
        );
    } else {
        println!("command: {command}");
        println!("tree: {tree}");
        println!("generation: {generation}");
        println!(
            "data: {}",
            serde_json::to_string_pretty(&data).map_err(json_error("serialize output"))?
        );
    }
    Ok(())
}

fn emit_rebuild(
    command: &str,
    outcome: &RebuildOutcome,
    diagnostics: &[ProjectionDiagnosticView],
    json_output: bool,
) -> Result<()> {
    emit(
        command,
        &outcome.metadata,
        json!({
            "rebuilt": outcome.rebuilt,
            "reason": format!("{:?}", outcome.reason).to_lowercase(),
            "update_kind": format!("{:?}", outcome.update_kind).to_lowercase(),
            "incremental_fallback": outcome.incremental_fallback.map(|value| format!("{value:?}").to_lowercase()),
            "source_file_count": outcome.source_file_count,
            "diagnostic_count": outcome.diagnostic_count,
            "diagnostics": diagnostics.iter().map(|diagnostic| json!({
                "key": diagnostic.key,
                "source_path": diagnostic.source_path,
                "code": diagnostic.code,
                "entity_id": diagnostic.entity_id,
                "event_ids": serde_json::from_str::<Value>(&diagnostic.event_ids_json)
                    .unwrap_or_else(|_| Value::String(diagnostic.event_ids_json.clone())),
                "message": diagnostic.message,
            })).collect::<Vec<_>>(),
            "operational_warnings": outcome.operational_warnings.iter().map(|warning| json!({
                "code": warning.code, "paths": warning.paths
            })).collect::<Vec<_>>(),
            "quarantined_database": outcome.quarantined_database,
            "versions": {
                "db_schema": outcome.metadata.db_schema_version,
                "event_parser": outcome.metadata.event_parser_version,
                "reducer": outcome.metadata.reducer_version,
                "conflict_detector": outcome.metadata.conflict_detector_version,
                "normalizer_tokenizer": outcome.metadata.normalizer_tokenizer_version,
                "search_ranking": outcome.metadata.search_ranking_version,
            }
        }),
        json_output,
    )
}

fn append_json(append: &AppendOutcome) -> Value {
    json!({"batch_id": append.batch_id, "event_id": append.event_id,
           "event_path": append.event_path, "commit_oid": append.commit_oid,
           "recovered": append.recovered,
           "objects": append.objects.iter().map(|object| json!({
               "sha256": object.sha256, "path": object.path, "size": object.size
           })).collect::<Vec<_>>()})
}

fn emit_error(error: &Error, json_output: bool) {
    if json_output {
        let encoded = serde_json::to_string(&json!({
            "error": {
                "code": error_code(error.kind()),
                "message": error.message(),
            }
        }))
        .unwrap_or_else(|_| "{\"error\":{\"code\":\"serialization_error\"}}".to_owned());
        eprintln!("{encoded}");
    } else {
        eprintln!("error [{}]: {}", error_code(error.kind()), error);
    }
}

const fn error_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvariantViolation => "invariant_violation",
        ErrorKind::Io => "io_error",
        ErrorKind::External => "external_error",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::RepositoryNotConfigured => "repository_not_configured",
        ErrorKind::Conflict => "conflict",
        ErrorKind::IdempotencyKeyConflict => "idempotency_key_conflict",
        ErrorKind::MaintenanceBusy => "maintenance_busy",
        _ => "unknown_error",
    }
}

fn installation_root() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".shared-context"))
        .ok_or_else(|| invalid("HOME is not set"))
}

fn is_help(args: &[String]) -> bool {
    matches!(args, [arg] if arg == "-h" || arg == "--help")
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn invariant(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvariantViolation, message)
}

fn json_error(operation: &'static str) -> impl FnOnce(serde_json::Error) -> Error {
    move |error| Error::new(ErrorKind::Io, format!("failed to {operation}: {error}"))
}
