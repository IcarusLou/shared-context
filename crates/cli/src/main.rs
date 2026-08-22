//! `sctx` command-line entry point.

mod args;

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

use args::Options;
use sctx_agent_adapter::{
    AgentCapabilities, CanonicalAgentAction, CanonicalBreadcrumbKind, ResolvedAgentAction,
    TaskRuntimeOperation, ToolOutcome, TrustState, plan_action,
};
use sctx_domain::{
    Applicability, ConflictParticipant, ConflictResolutionDraft, ConflictResolutionResult,
    ContextGovernanceStatus, ContextId, ContextKind, ContextRevisionDraft, DomainProjection, Error,
    ErrorKind, EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, PublicationAction,
    PublicationDraft, RepositoryId, ResolutionOutcome, Result, ReviewDraft, ReviewSummary,
    ReviewVerdict, RevisionId, SemanticConflictDraft, SpaceId, TaskSignal, TaskSignalKind,
    WorkEpisodeId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendOutcome, AppendRequest, BatchId, CandidateAppendOutcome, GitStore};
use sctx_index::{
    DomainSnapshot, IndexMetadata, ProjectionDiagnosticView, ProjectionIndex, RebuildOutcome,
};
use sctx_local_state::{
    Breadcrumb, BreadcrumbKind, CaptureDiagnosticKind, CaptureStore, CaptureTaskOwner,
    CatalogCheckoutStatus, RepositoryCatalogSnapshot, UserConfigStore,
};
use sctx_mcp::{
    ArtifactFocusQuery, AssociationExplainInput, AssociationRebuildInput,
    EngineeringReferenceRecordInput, RepositoryScanInput, TaskCheckpointInput,
    TaskContextReadInput, TaskIntentUpdateInput, TaskSignalSupersedeInput,
};
use sctx_search::{ContextStatus, ScopeFilter, SearchEngine, SearchFilters, SearchRequest};
use sctx_task_runtime::TaskRuntime;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const HOOK_TASK_UNAVAILABLE: &str = "Shared Context task retrieval is temporarily unavailable. Coding can continue; retry through MCP or CLI later.";

const HELP: &str = r"Shared Context command-line interface

Usage: sctx [--json] <COMMAND>

Commands:
  setup [--demo] [--agents cursor,codex] [--root PATH] [--runtime-source PATH]
  demo
  doctor [--fix] [--root PATH]
  upgrade [--agents cursor,codex] [--root PATH] [--runtime-source PATH]
  uninstall [--root PATH]
  knowledge delete --confirm-path PATH --confirm DELETE-SHARED-CONTEXT-KNOWLEDGE
  space create|intent revise|list|get
  candidate create
  context revise|review|publish|withdraw|get
  semantic conflict open|resolve
  task context|artifact-focus|checkpoint|intent update|signal supersede
  repository add|list|doctor|scan
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
        [group, rest @ ..] if group == "knowledge" => run_knowledge(rest, json_output),
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
    let options = Options::parse(args, &["--yes", "--demo"])?;
    options.allow_only(
        &[
            "--agents",
            "--root",
            "--runtime-source",
            "--runtime-version",
        ],
        &["--yes", "--demo"],
    )?;
    if command != "setup" && options.has("--demo") {
        return Err(invalid("--demo applies only to setup"));
    }
    let installer = installer_from_options(&options)?;
    let setup = setup_options(&options)?;
    let report = if command == "setup" {
        installer.setup(&setup)?
    } else {
        installer.upgrade(&setup)?
    };
    if options.has("--demo") {
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
    let options = Options::parse(args, &["--fix"])?;
    options.allow_only(
        &[
            "--root",
            "--runtime-source",
            "--runtime-version",
            "--agents",
        ],
        &["--fix"],
    )?;
    let installer = installer_from_options(&options)?;
    let report = if options.has("--fix") {
        installer.doctor_fix(&setup_options(&options)?)?
    } else {
        installer.doctor()
    };
    emit_lifecycle(&report, json_output)
}

fn run_uninstall(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &[])?;
    options.allow_only(&["--root", "--runtime-source", "--runtime-version"], &[])?;
    let report = installer_from_options(&options)?.uninstall()?;
    emit_lifecycle(&report, json_output)
}

fn run_knowledge(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid(
            "Usage: sctx knowledge delete --confirm-path <ABSOLUTE_PATH> --confirm DELETE-SHARED-CONTEXT-KNOWLEDGE",
        ));
    };
    if command != "delete" {
        return Err(invalid("knowledge command must be delete"));
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
    let Some(value) = options.optional("--agents")? else {
        return Ok(sctx_installer::SetupOptions::default());
    };
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
    Ok(sctx_installer::SetupOptions { agents })
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
    if tools.len() != 13 || !tools.iter().any(|tool| tool["name"] == "task_checkpoint") {
        return Err(invariant(
            "demo MCP tools/list did not return the thirteen-tool Checkpoint surface",
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
    let mut input = Vec::new();
    io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| Error::new(ErrorKind::Io, format!("read hook stdin: {error}")))?;
    if input.is_empty() {
        return Err(invalid("hook stdin must contain one JSON payload"));
    }

    let (event, version) = if agent == "cursor" {
        let (event, payload_version) = sctx_adapter_cursor::decode_hook_input(&input)?;
        (event, Some(payload_version))
    } else {
        let event = sctx_adapter_codex::decode_hook_input(&input)?;
        let version = options
            .optional("--agent-version")?
            .map(str::to_owned)
            .or_else(|| detect_agent_version(agent));
        (event, version)
    };
    let trust = parse_trust(agent, None, true)?;
    let capabilities = agent_capabilities(agent, version.as_deref(), true, trust);
    let action = plan_action(&event, &capabilities);
    let resolved = resolve_hook_action(action)?;
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
    if !capabilities.hooks_verified() {
        eprintln!("{}", capabilities.diagnostic);
    }
    Ok(())
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

fn resolve_hook_action(action: CanonicalAgentAction) -> Result<ResolvedAgentAction> {
    let CanonicalAgentAction {
        task_operation,
        breadcrumb,
        system_message,
    } = action;
    if let Some(breadcrumb) = breadcrumb {
        let root = installation_root()?;
        if capture_breadcrumb(&root, breadcrumb).is_err() {
            return Ok(ResolvedAgentAction {
                additional_context: None,
                system_message: Some(HOOK_TASK_UNAVAILABLE.to_owned()),
            });
        }
    }
    let additional_context = match task_operation.map(resolve_task_operation).transpose() {
        Ok(context) => context.flatten(),
        Err(_) => {
            return Ok(ResolvedAgentAction {
                additional_context: None,
                system_message: Some(HOOK_TASK_UNAVAILABLE.to_owned()),
            });
        }
    };
    Ok(ResolvedAgentAction {
        additional_context,
        system_message,
    })
}

fn capture_breadcrumb(
    root: &Path,
    breadcrumb: sctx_agent_adapter::CanonicalBreadcrumb,
) -> Result<()> {
    let (task_owner, diagnostics) = match TaskRuntime::initialize(root)
        .and_then(|runtime| runtime.read_snapshot_by_locator(&breadcrumb.external_session_locator))
    {
        Ok(Some(snapshot)) => (
            Some(CaptureTaskOwner {
                task_session_id: snapshot.task_session_id,
                task_id: snapshot.task_id,
                intent_revision_id: snapshot
                    .current_intent_revision()
                    .ok_or_else(|| invariant("ActiveTask has no Intent Head"))?
                    .revision_id,
            }),
            Vec::new(),
        ),
        Ok(None) => (None, vec![CaptureDiagnosticKind::NoActiveTask]),
        Err(_) => (None, vec![CaptureDiagnosticKind::RuntimeUnavailable]),
    };
    CaptureStore::initialize(root)?.capture(&Breadcrumb {
        external_session_locator: breadcrumb.external_session_locator,
        task_owner,
        kind: match breadcrumb.kind {
            CanonicalBreadcrumbKind::ToolOutcome => BreadcrumbKind::ToolOutcome,
            CanonicalBreadcrumbKind::Checkpoint => BreadcrumbKind::Checkpoint,
        },
        summary: breadcrumb.summary,
        workspace_hint: breadcrumb.workspace_hint,
        file_hints: breadcrumb.file_hints,
        diagnostics,
    })?;
    Ok(())
}

fn resolve_task_operation(operation: TaskRuntimeOperation) -> Result<Option<String>> {
    match operation {
        TaskRuntimeOperation::MergeObservations {
            locator,
            cwd,
            workspace_roots,
            file_hints,
            tool_name,
            outcome,
        } => {
            let root = installation_root()?;
            let runtime = TaskRuntime::initialize(&root)?;
            if runtime.read_snapshot_by_locator(&locator)?.is_none() {
                return Ok(None);
            }
            let catalog = UserConfigStore::open_existing(&root)?.repository_catalog()?;
            let signals = normalized_observation_signals(
                &catalog,
                &cwd,
                &workspace_roots,
                &file_hints,
                &tool_name,
                outcome,
            );
            if !signals.is_empty() {
                let _outcome = runtime.merge_signals_by_locator(&locator, signals)?;
            }
            Ok(None)
        }
    }
}

fn normalized_observation_signals(
    _catalog: &RepositoryCatalogSnapshot,
    _cwd: &Path,
    _workspace_roots: &[PathBuf],
    _file_hints: &[PathBuf],
    tool_name: &str,
    outcome: ToolOutcome,
) -> Vec<TaskSignal> {
    let mut signals = Vec::new();
    if is_test_tool(tool_name) {
        let test_outcome = format!(
            "{} {}",
            tool_name.trim(),
            match outcome {
                ToolOutcome::Succeeded => "succeeded",
                ToolOutcome::Failed => "failed",
            }
        );
        push_signal(&mut signals, TaskSignalKind::TestOutcome, &test_outcome);
    }
    signals
}

fn is_test_tool(tool_name: &str) -> bool {
    let name = tool_name.trim().to_ascii_lowercase();
    ["test", "check", "lint"]
        .iter()
        .any(|word| name.contains(word))
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

fn detect_agent_version(agent: &str) -> Option<String> {
    let executable = if agent == "cursor" { "cursor" } else { "codex" };
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
        let store = GitStore::initialize(root)?;
        let index = ProjectionIndex::for_store(&store);
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

    fn append_candidate_once(
        &self,
        event: Event,
    ) -> Result<(CandidateAppendOutcome, IndexMetadata)> {
        let outcome = self
            .store
            .append_candidate_once(AppendRequest::event(event))?;
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
                    json!({"space_id": space.space_id, "intent_heads": space.intent.heads,
                           "titles": titles, "context_count": space.contexts.len()})
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

fn run_candidate(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "create" => {
            if is_help(rest) {
                print!(
                    "Usage: sctx candidate create --source-episode-id <ID> [content options]\n\n{CONTEXT_WRITE_HELP}"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            allow_context_options(&options, &["--source-episode-id"])?;
            let source_episode_id = parse_id::<WorkEpisodeId>(
                options.required("--source-episode-id")?,
                "source episode ID",
            )?;
            let draft = context_draft(&options)?;
            let runtime = Runtime::open()?;
            let event = Event::context_candidate_created(source_episode_id, draft, None)?;
            let (outcome, metadata) = runtime.append_candidate_once(event)?;
            let (candidate_id, source_episode_id) = match outcome.event.payload() {
                EventPayload::ContextCandidateCreated { candidate } => {
                    (candidate.candidate_id, candidate.source_episode_id)
                }
                _ => unreachable!(),
            };
            emit(
                "candidate.create",
                &metadata,
                json!({"candidate_id": candidate_id,
                       "source_episode_id": source_episode_id,
                       "event_id": outcome.event.event_id(), "status": "candidate",
                       "created": outcome.created, "batch_id": outcome.append.batch_id,
                       "commit_oid": outcome.append.commit_oid}),
                json_output,
            )
        }
        _ => Err(invalid(format!(
            "invalid candidate command; expected create\n\n{CONTEXT_WRITE_HELP}"
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
            run_publication(rest, PublicationAction::Withdraw, json_output)
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
    let options = Options::parse(args, &[])?;
    allow_search_options(&options, &[])?;
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
                read_json(options.required("--input")?, "Task Intent update")?;
            let response = sctx_mcp::task_intent_update_at_root(installation_root()?, &input)?;
            let data = serde_json::to_value(&response)
                .map_err(json_error("serialize Task Intent update response"))?;
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
    let options = Options::parse(args, &[])?;
    options.allow_only(
        &[
            "--agent-kind",
            "--external-session-id",
            "--token-budget",
            "--max-spaces",
        ],
        &[],
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
    let response = sctx_mcp::task_context_readonly_at_root(installation_root()?, &input)?;
    let data =
        serde_json::to_value(&response).map_err(json_error("serialize Task Context response"))?;
    emit_raw(
        "task.context",
        &response.tree,
        response.generation,
        &data,
        json_output,
    )
}

fn run_repository(args: &[String], json_output: bool) -> Result<()> {
    let [command, rest @ ..] = args else {
        return Err(invalid(
            "Usage: sctx repository add|list|doctor|scan [OPTIONS]",
        ));
    };
    let root = installation_root()?;
    match command.as_str() {
        "add" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--repository-id", "--path"], &[])?;
            let repository_id = options
                .optional("--repository-id")?
                .map(|value| parse_id::<RepositoryId>(value, "Repository ID"))
                .transpose()?;
            let paths = options
                .many("--path")
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let outcome =
                UserConfigStore::initialize(&root)?.add_repository(repository_id, &paths)?;
            let sync = sctx_mcp::sync_repository_catalog_at_root(&root)?;
            let metadata = repository_command_metadata(&root)?;
            emit(
                "repository.add",
                &metadata,
                json!({"catalog": outcome, "registry": sync}),
                json_output,
            )
        }
        "list" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&[], &[])?;
            let config = UserConfigStore::initialize(&root)?;
            let catalog = config.repository_catalog()?;
            let sync = sctx_mcp::sync_repository_catalog_at_root(&root)?;
            let metadata = repository_command_metadata(&root)?;
            emit(
                "repository.list",
                &metadata,
                json!({"repositories": catalog.repositories, "registry": sync}),
                json_output,
            )
        }
        "doctor" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&[], &[])?;
            let config = UserConfigStore::initialize(&root)?;
            let report = config.doctor_repository_catalog()?;
            let syncable = report.checkouts.iter().all(|checkout| {
                matches!(
                    checkout.status,
                    CatalogCheckoutStatus::Available | CatalogCheckoutStatus::Missing
                )
            });
            let sync = syncable
                .then(|| sctx_mcp::sync_repository_catalog_at_root(&root))
                .transpose()?;
            let metadata = repository_command_metadata(&root)?;
            emit(
                "repository.doctor",
                &metadata,
                json!({"catalog": report, "registry": sync}),
                json_output,
            )
        }
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
            "repository command must be add, list, doctor, or scan",
        )),
    }
}

fn repository_command_metadata(root: &Path) -> Result<IndexMetadata> {
    let store = GitStore::initialize(root)?;
    Ok(ProjectionIndex::for_store(&store).synchronize()?.metadata)
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
    statement: String,
    rationale: String,
    #[serde(default)]
    applicability: Applicability,
    #[serde(default)]
    assumptions: Vec<String>,
    #[serde(default)]
    recheck_when: Vec<String>,
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
            statement: input.statement,
            rationale: input.rationale,
            applicability: input.applicability,
            assumptions: input.assumptions,
            recheck_when: input.recheck_when,
            relations: Vec::new(),
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
        if *option == "--automatic" {
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
