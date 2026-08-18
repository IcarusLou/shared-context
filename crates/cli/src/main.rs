//! `sctx` command-line entry point.

mod args;

use std::{
    collections::BTreeSet, env, ffi::OsString, fs, path::PathBuf, process::ExitCode, str::FromStr,
};

use args::Options;
use sctx_domain::{
    Applicability, ConflictParticipant, ConflictResolutionDraft, ConflictResolutionResult,
    ContextGovernanceStatus, ContextId, ContextKind, ContextRevisionDraft, DomainProjection, Error,
    ErrorKind, EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, PublicationAction,
    PublicationDraft, ResolutionOutcome, Result, ReviewDraft, ReviewSummary, ReviewVerdict,
    RevisionId, SemanticConflictDraft, SpaceId,
};
use sctx_event_schema::{Event, EventPayload};
use sctx_git_store::{AppendOutcome, AppendRequest, BatchId, GitStore};
use sctx_index::{
    DomainSnapshot, IndexMetadata, ProjectionDiagnosticView, ProjectionIndex, RebuildOutcome,
};
use sctx_local_state::UserConfigStore;
use sctx_search::{
    ContextPackMode, ContextPackRequest, ContextStatus, ScopeFilter, SearchEngine, SearchFilters,
    SearchRequest,
};
use serde::Deserialize;
use serde_json::{Value, json};

const HELP: &str = r"Shared Context command-line interface

Usage: sctx [--json] <COMMAND>

Commands:
  space create|intent revise|list|get
  context propose|revise|review|publish|withdraw|get
  semantic conflict open|resolve
  workspace bind|list|unbind
  search
  context-pack
  index rebuild|status
  pending list|commit|move-aside
  validate --staged
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
        [group, rest @ ..] if group == "space" => run_space(rest, json_output),
        [group, command, rest @ ..] if group == "context" && command == "pack" => {
            run_context_pack(rest, json_output)
        }
        [group, rest @ ..] if group == "context" => run_context(rest, json_output),
        [group, rest @ ..] if group == "semantic" => run_semantic(rest, json_output),
        [group, rest @ ..] if group == "workspace" => run_workspace(rest, json_output),
        [command, rest @ ..] if command == "search" => run_search(rest, json_output),
        [command, rest @ ..] if command == "context-pack" => run_context_pack(rest, json_output),
        [group, rest @ ..] if group == "index" => run_index(rest, json_output),
        [group, rest @ ..] if group == "pending" => run_pending(rest, json_output),
        [command, rest @ ..] if command == "validate" => run_validate(rest, json_output),
        [group, rest @ ..] if group == "mcp" => run_mcp(rest),
        _ => Err(invalid(format!("unknown command\n\n{HELP}"))),
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
        let store = GitStore::initialize(installation_root()?)?;
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

fn run_context(args: &[String], json_output: bool) -> Result<()> {
    match args {
        [command, rest @ ..] if command == "propose" => {
            if is_help(rest) {
                print!(
                    "Usage: sctx context propose --space-id <ID> [content options]\n\n{CONTEXT_WRITE_HELP}"
                );
                return Ok(());
            }
            let options = Options::parse(rest, &[])?;
            allow_context_options(&options, &["--space-id"])?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let draft = context_draft(&options)?;
            let runtime = Runtime::open()?;
            let snapshot = runtime.domain_snapshot()?;
            require_space(&snapshot.projection, space_id)?;
            let event = Event::context_proposed(space_id, draft, None)?;
            let (context_id, revision_id) = context_identity(&event);
            let event_id = event.event_id();
            let (append, metadata) = runtime.append(event)?;
            emit(
                "context.propose",
                &metadata,
                json!({"space_id": space_id, "context_id": context_id,
                       "revision_id": revision_id, "event_id": event_id,
                       "batch_id": append.batch_id, "commit_oid": append.commit_oid}),
                json_output,
            )
        }
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
            "invalid context command; expected propose|revise|review|publish|withdraw|get\n\n{CONTEXT_WRITE_HELP}"
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

fn run_workspace(args: &[String], json_output: bool) -> Result<()> {
    let runtime = Runtime::open()?;
    let config = UserConfigStore::initialize(runtime.store.root())?;
    match args {
        [command, rest @ ..] if command == "bind" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--workspace", "--space-id"], &[])?;
            let space_id = parse_id(options.required("--space-id")?, "space ID")?;
            let binding = config.bind(options.required("--workspace")?, space_id)?;
            let snapshot = runtime.domain_snapshot()?;
            emit(
                "workspace.bind",
                &snapshot.metadata,
                json!({"workspace": binding.workspace(), "space_id": binding.space_id()}),
                json_output,
            )
        }
        [command] if command == "list" => {
            let snapshot = runtime.domain_snapshot()?;
            let bindings = config
                .list()?
                .into_iter()
                .map(|binding| {
                    json!({"workspace": binding.workspace(), "space_id": binding.space_id()})
                })
                .collect::<Vec<_>>();
            emit(
                "workspace.list",
                &snapshot.metadata,
                json!({"bindings": bindings}),
                json_output,
            )
        }
        [command, rest @ ..] if command == "unbind" => {
            let options = Options::parse(rest, &[])?;
            options.allow_only(&["--workspace"], &[])?;
            let workspace = options.required("--workspace")?;
            let binding = config.unbind(workspace)?;
            let snapshot = runtime.domain_snapshot()?;
            emit(
                "workspace.unbind",
                &snapshot.metadata,
                json!({"workspace": workspace, "removed": binding.is_some(),
                       "space_id": binding.map(|value| value.space_id())}),
                json_output,
            )
        }
        _ => Err(invalid(
            "invalid workspace command; expected bind|list|unbind",
        )),
    }
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

fn run_context_pack(args: &[String], json_output: bool) -> Result<()> {
    let options = Options::parse(args, &["--automatic"])?;
    allow_search_options(
        &options,
        &["--token-budget", "--candidate-limit", "--automatic"],
    )?;
    let search = search_request(&options)?;
    let token_budget = parse_usize(
        options.optional("--token-budget")?.unwrap_or("2000"),
        "token budget",
    )?;
    let candidate_limit = parse_usize(
        options.optional("--candidate-limit")?.unwrap_or("100"),
        "candidate limit",
    )?;
    let request = ContextPackRequest {
        search,
        token_budget,
        candidate_limit,
        mode: if options.has("--automatic") {
            ContextPackMode::AutomaticInjection
        } else {
            ContextPackMode::Explicit
        },
    };
    let runtime = Runtime::open()?;
    let response = SearchEngine::new(runtime.index).context_pack(&request)?;
    let data = serde_json::to_value(&response).map_err(json_error("serialize Context Pack"))?;
    emit_raw(
        "context-pack",
        &response.indexed_tree_oid,
        response.projection_generation,
        &data,
        json_output,
    )
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
        preferred_space_id: options
            .optional("--preferred-space-id")?
            .map(|id| parse_id(id, "preferred space ID"))
            .transpose()?,
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
        "--preferred-space-id",
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

fn require_space(projection: &DomainProjection, space_id: SpaceId) -> Result<()> {
    if projection.spaces.contains_key(&space_id) {
        Ok(())
    } else {
        Err(invalid(format!("space does not exist: {space_id}")))
    }
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
