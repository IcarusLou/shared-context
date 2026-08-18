use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    str::FromStr,
    sync::Arc,
};

use sctx_domain::{
    Error, ErrorKind, EventId, IntentSnapshot, PublicationAction, PublicationDraft, Result, SpaceId,
};
use sctx_event_schema::Event;
use sctx_git_store::{AppendRequest, CrashInjector, CrashSeam, GitStore};
use serde_json::Value;
use tempfile::{TempDir, tempdir};

const EVIDENCE: &str = r#"{"kind":"experiment_record","supports":"CLI command completed","content":{"command":"contract"},"interpretation":"the contract is executable","limitations":[]}"#;

struct Harness {
    _temporary: TempDir,
    home: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let home = temporary.path().join("用户 home 空格");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            home,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sctx"))
            .arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .output()
            .expect("sctx should start")
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value["tree"].as_str().is_some_and(|tree| !tree.is_empty()));
        assert!(value["generation"].as_u64().is_some());
        value
    }

    fn failure(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "command unexpectedly succeeded: {args:?}"
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn repository(&self) -> PathBuf {
        self.home.join(".shared-context/repository")
    }

    fn root(&self) -> PathBuf {
        self.home.join(".shared-context")
    }

    fn head(&self) -> String {
        git_output(&self.repository(), &["rev-parse", "HEAD"])
    }

    fn event_count(&self) -> usize {
        let output = git_output(
            &self.repository(),
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        output
            .lines()
            .filter(|path| path.starts_with("events/"))
            .count()
    }
}

#[allow(clippy::struct_field_names)]
struct Published {
    context_id: String,
    revision_id: String,
    review_event_id: String,
    publication_id: String,
}

fn create_space(harness: &Harness, title: &str) -> (String, String) {
    let value = harness.success(&[
        "space",
        "create",
        "--title",
        title,
        "--problem",
        "cold start",
        "--desired-outcome",
        "shared knowledge",
        "--in-scope",
        "CLI",
        "--acceptance-condition",
        "works",
    ]);
    (
        text(&value, "space_id").to_owned(),
        text(&value, "revision_id").to_owned(),
    )
}

fn propose(harness: &Harness, space_id: &str, statement: &str) -> (String, String) {
    let value = harness.success(&[
        "context",
        "propose",
        "--space-id",
        space_id,
        "--kind",
        "decision",
        "--topic-key",
        "cli/output",
        "--statement",
        statement,
        "--rationale",
        "stable clients",
        "--domain",
        "cli",
        "--evidence-json",
        EVIDENCE,
    ]);
    (
        text(&value, "context_id").to_owned(),
        text(&value, "revision_id").to_owned(),
    )
}

fn approve_publish(harness: &Harness, space_id: &str, statement: &str) -> Published {
    let (context_id, revision_id) = propose(harness, space_id, statement);
    let review = harness.success(&[
        "context",
        "review",
        "--space-id",
        space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--verdict",
        "approve",
        "--reason",
        "verified",
    ]);
    let review_event_id = text(&review, "event_id").to_owned();
    let publication = harness.success(&[
        "context",
        "publish",
        "--space-id",
        space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--expect-no-publication-head",
        "--review-event-id",
        &review_event_id,
    ]);
    Published {
        context_id,
        revision_id,
        review_event_id,
        publication_id: text(&publication, "publication_id").to_owned(),
    }
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value["data"][field].as_str().unwrap()
}

fn git_output(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn help_and_version_expose_the_complete_lifecycle_surface() {
    let help = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--help")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help.status.success());
    for command in [
        "space create|intent revise|list|get",
        "context propose|revise|review|publish|withdraw|get",
        "semantic conflict open|resolve",
        "search",
        "context-pack",
        "pending list|commit|move-aside",
        "validate --staged",
    ] {
        assert!(stdout.contains(command), "missing help surface: {command}");
    }

    let version = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("sctx {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn lifecycle_commands_share_stable_json_tree_and_generation_envelopes() {
    let harness = Harness::new();
    let (space_id, intent_revision) = create_space(&harness, "Lifecycle");

    let revised_intent = harness.success(&[
        "space",
        "intent",
        "revise",
        "--space-id",
        &space_id,
        "--parent-revision-id",
        &intent_revision,
        "--title",
        "Lifecycle revised",
        "--problem",
        "cold start",
        "--desired-outcome",
        "shared knowledge",
        "--in-scope",
        "CLI",
        "--acceptance-condition",
        "works",
    ]);
    let revised_intent_id = text(&revised_intent, "revision_id");
    assert_ne!(intent_revision, revised_intent_id);
    assert_eq!(
        harness.success(&["space", "list"])["data"]["spaces"][0]["space_id"],
        space_id
    );
    harness.success(&["space", "get", "--space-id", &space_id]);

    let workspace = harness.home.join("业务 workspace 中文");
    fs::create_dir_all(&workspace).unwrap();
    harness.success(&[
        "workspace",
        "bind",
        "--workspace",
        workspace.to_str().unwrap(),
        "--space-id",
        &space_id,
    ]);
    assert_eq!(
        harness.success(&["workspace", "list"])["data"]["bindings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (context_id, first_revision) = propose(&harness, &space_id, "first snapshot");
    let revised = harness.success(&[
        "context",
        "revise",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--parent-revision-id",
        &first_revision,
        "--kind",
        "decision",
        "--topic-key",
        "cli/output",
        "--statement",
        "stable output",
        "--rationale",
        "stable clients",
        "--domain",
        "cli",
        "--evidence-json",
        EVIDENCE,
    ]);
    let revision_id = text(&revised, "revision_id").to_owned();
    let review = harness.success(&[
        "context",
        "review",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--verdict",
        "approve",
        "--reason",
        "verified",
    ]);
    let review_event = text(&review, "event_id");
    let publication = harness.success(&[
        "context",
        "publish",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--expect-no-publication-head",
        "--review-event-id",
        review_event,
    ]);
    let publication_id = text(&publication, "publication_id");

    let search = harness.success(&[
        "search",
        "--query",
        "stable",
        "--space-id",
        &space_id,
        "--status",
        "accepted",
    ]);
    assert_eq!(search["data"]["results"][0]["context_id"], context_id);
    let pack = harness.success(&[
        "context-pack",
        "--query",
        "stable",
        "--space-id",
        &space_id,
        "--automatic",
        "--token-budget",
        "1000",
    ]);
    assert_eq!(pack["data"]["items"][0]["context_id"], context_id);
    harness.success(&[
        "context",
        "get",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
    ]);
    harness.success(&[
        "context",
        "withdraw",
        "--space-id",
        &space_id,
        "--context-id",
        &context_id,
        "--revision-id",
        &revision_id,
        "--previous-publication-id",
        publication_id,
    ]);
    harness.success(&["index", "status"]);
    harness.success(&["index", "rebuild"]);
    harness.success(&["validate", "--staged"]);
    harness.success(&[
        "workspace",
        "unbind",
        "--workspace",
        workspace.to_str().unwrap(),
    ]);

    let human = Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(["space", "list"])
        .env("HOME", &harness.home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("tree:"));
    assert!(stdout.contains("generation:"));
    assert!(stdout.contains(&space_id));
}

#[test]
fn unrelated_unknown_schema_isolated_while_target_and_head_checks_stay_closed() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "Isolation");
    let repository = harness.repository();
    let unknown_path = repository.join("events/aa/evt_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.json");
    fs::create_dir_all(unknown_path.parent().unwrap()).unwrap();
    fs::write(
        &unknown_path,
        r#"{"schema_version":"999","event_id":"evt_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","event_type":"future.event"}"#,
    )
    .unwrap();
    git_output(
        &repository,
        &[
            "add",
            "--",
            "events/aa/evt_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.json",
        ],
    );
    git_output(&repository, &["commit", "-m", "unknown schema fixture"]);

    let published = approve_publish(&harness, &space_id, "legal aggregate remains writable");
    let status = harness.success(&["index", "status"]);
    assert_eq!(status["data"]["diagnostic_count"], 1);
    assert_eq!(
        status["data"]["diagnostics"][0]["code"],
        "UNKNOWN_SCHEMA_VERSION"
    );
    assert!(
        status["data"]["diagnostics"][0]["source_path"]
            .as_str()
            .unwrap()
            .contains("evt_aaaaaaaa")
    );
    harness.success(&["validate", "--staged"]);

    let head_before = harness.head();
    let events_before = harness.event_count();
    let invalid_context = "ctx_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let invalid_revision = "rev_cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let failure = harness.failure(&[
        "context",
        "review",
        "--space-id",
        &space_id,
        "--context-id",
        invalid_context,
        "--revision-id",
        invalid_revision,
        "--verdict",
        "approve",
        "--reason",
        "must fail",
    ]);
    assert_eq!(failure["error"]["code"], "invalid_input");
    assert_eq!(harness.head(), head_before);
    assert_eq!(harness.event_count(), events_before);

    let store = GitStore::initialize(harness.root()).unwrap();
    let concurrent = Event::publication_changed(
        SpaceId::from_str(&space_id).unwrap(),
        published.context_id.parse().unwrap(),
        PublicationDraft {
            previous_publication_ids: Vec::new(),
            action: PublicationAction::Publish,
            revision_id: published.revision_id.parse().unwrap(),
            review_event_ids: vec![EventId::from_str(&published.review_event_id).unwrap()],
        },
        None,
    )
    .unwrap();
    store
        .append_event(AppendRequest::event(concurrent))
        .unwrap();
    let conflict_head = harness.head();
    let failure = harness.failure(&[
        "context",
        "withdraw",
        "--space-id",
        &space_id,
        "--context-id",
        &published.context_id,
        "--revision-id",
        &published.revision_id,
        "--previous-publication-id",
        &published.publication_id,
    ]);
    assert_eq!(failure["error"]["code"], "invariant_violation");
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Head precondition failed")
    );
    assert_eq!(harness.head(), conflict_head);
}

#[test]
#[allow(clippy::too_many_lines)]
fn invalid_semantic_resolution_never_appends_an_event() {
    let harness = Harness::new();
    let (space_id, _) = create_space(&harness, "Conflict");
    let first = approve_publish(&harness, &space_id, "first side");
    let second = approve_publish(&harness, &space_id, "second side");
    let third = approve_publish(&harness, &space_id, "unrelated side");
    let first_participant = format!(
        "{}:{}:{}",
        first.context_id, first.revision_id, first.publication_id
    );
    let second_participant = format!(
        "{}:{}:{}",
        second.context_id, second.revision_id, second.publication_id
    );
    let opened = harness.success(&[
        "semantic",
        "conflict",
        "open",
        "--space-id",
        &space_id,
        "--participant",
        &first_participant,
        "--participant",
        &second_participant,
        "--reason",
        "contradictory decisions",
        "--domain",
        "cli",
    ]);
    let conflict_id = text(&opened, "conflict_id").to_owned();
    let head_before = harness.head();
    let events_before = harness.event_count();
    let first_result = format!("{}:{}:retained", first.context_id, first.revision_id);

    let missing = harness.failure(&[
        "semantic",
        "conflict",
        "resolve",
        "--space-id",
        &space_id,
        "--conflict-id",
        &conflict_id,
        "--expect-no-resolution-head",
        "--related-publication-id",
        &first.publication_id,
        "--related-publication-id",
        &second.publication_id,
        "--result",
        &first_result,
        "--rationale",
        "incomplete",
    ]);
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("every conflict participant exactly once")
    );
    assert_eq!(harness.head(), head_before);
    assert_eq!(harness.event_count(), events_before);

    let second_result = format!("{}:{}:retained", second.context_id, second.revision_id);
    let third_result = format!("{}:{}:retained", third.context_id, third.revision_id);
    let extra = harness.failure(&[
        "semantic",
        "conflict",
        "resolve",
        "--space-id",
        &space_id,
        "--conflict-id",
        &conflict_id,
        "--expect-no-resolution-head",
        "--related-publication-id",
        &first.publication_id,
        "--related-publication-id",
        &second.publication_id,
        "--result",
        &first_result,
        "--result",
        &second_result,
        "--result",
        &third_result,
        "--rationale",
        "contains unrelated Context",
    ]);
    assert!(
        extra["error"]["message"]
            .as_str()
            .unwrap()
            .contains("every conflict participant exactly once")
    );
    assert_eq!(harness.head(), head_before);
    assert_eq!(harness.event_count(), events_before);

    let resolved = harness.success(&[
        "semantic",
        "conflict",
        "resolve",
        "--space-id",
        &space_id,
        "--conflict-id",
        &conflict_id,
        "--expect-no-resolution-head",
        "--related-publication-id",
        &first.publication_id,
        "--related-publication-id",
        &second.publication_id,
        "--result",
        &first_result,
        "--result",
        &second_result,
        "--rationale",
        "explicit complete resolution",
    ]);
    assert!(text(&resolved, "resolution_id").starts_with("rsl_"));
}

#[derive(Debug)]
struct StopAfterJournal;

impl CrashInjector for StopAfterJournal {
    fn check(&self, seam: CrashSeam) -> Result<()> {
        if seam == CrashSeam::AfterJournal {
            Err(Error::new(ErrorKind::Io, "injected stop"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn pending_commit_and_move_aside_are_explicit_and_validate_staged_rejects_modification()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let harness = Harness::new();
    create_space(&harness, "Pending baseline");
    let pending_store =
        GitStore::initialize(harness.root())?.with_crash_injector(Arc::new(StopAfterJournal));
    let pending_event = Event::space_created(intent("Pending commit"), None)?;
    assert!(
        pending_store
            .append_event(AppendRequest::event(pending_event))
            .is_err()
    );
    let listed = harness.success(&["pending", "list"]);
    let batch = listed["data"]["batches"][0]["batch_id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness.success(&["pending", "commit", &batch]);
    assert!(
        harness.success(&["pending", "list"])["data"]["batches"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let pending_store =
        GitStore::initialize(harness.root())?.with_crash_injector(Arc::new(StopAfterJournal));
    let pending_event = Event::space_created(intent("Pending aside"), None)?;
    assert!(
        pending_store
            .append_event(AppendRequest::event(pending_event))
            .is_err()
    );
    let listed = harness.success(&["pending", "list"]);
    let batch = listed["data"]["batches"][0]["batch_id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness.success(&["pending", "move-aside", &batch]);

    let event_path = git_output(
        &harness.repository(),
        &["ls-tree", "-r", "--name-only", "HEAD", "--", "events"],
    )
    .lines()
    .next()
    .unwrap()
    .to_owned();
    let absolute = harness.repository().join(&event_path);
    let mut bytes = fs::read(&absolute)?;
    bytes.push(b' ');
    fs::write(&absolute, bytes)?;
    git_output(&harness.repository(), &["add", "--", &event_path]);
    let failure = harness.failure(&["validate", "--staged"]);
    assert_eq!(failure["error"]["code"], "invariant_violation");
    Ok(())
}

fn intent(title: &str) -> IntentSnapshot {
    IntentSnapshot {
        title: title.to_owned(),
        problem: "pending recovery".to_owned(),
        desired_outcome: "explicit action".to_owned(),
        in_scope: vec!["CLI".to_owned()],
        out_of_scope: Vec::new(),
        acceptance_conditions: vec!["recoverable".to_owned()],
        domain_terms: Vec::new(),
    }
}
