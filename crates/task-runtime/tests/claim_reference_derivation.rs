//! Server-side derivation of engineering references from Checkpoint Claim text (WP-D P1.1/P1.2).
//!
//! Every test drives a real temporary checkout so the only sanctioned basename inference is
//! exercised through the same read-only `git ls-files` path production uses.
//!
//! Derivation belongs to Candidate Build, not to the Checkpoint ACK (ADR-0003): `task_checkpoint`
//! persists a receipt and an outbox entry, and `derive_episode_claim_references` places the
//! spellings once, afterwards, off the latency-bound path.

use std::{fs, path::Path, process::Command};

use sctx_domain::{
    ArtifactKind, ContextKind, EvidenceType, ExternalSessionLocator, ReferenceRelation, TaskId,
    TaskSignal, TaskSignalKind, WorkEpisodeId, WorkingIntentSnapshot,
};
use sctx_task_runtime::{
    AgentCheckpointSubmission, CheckoutReferenceResolver, ClaimResolver,
    DirectCheckpointClaimDraft, DirectEvidenceDraft, TaskRuntime,
    reference_derivation::{
        DERIVED_BY_SERVER_LIMITATION, SYMBOL_MENTION_LIMITATION, UNIQUE_BASENAME_LIMITATION,
        claim_hints, claim_symbol_mentions, derive_claim_references,
    },
};
use tempfile::TempDir;

const FIXTURE_FILES: &[&str] = &[
    "app/src/main/kotlin/comment/CommentBottomBarManager.kt",
    "app/src/main/kotlin/poi/PoiEntranceAssem.kt",
    "app/src/main/kotlin/anchor/ProductAnchorAssem.kt",
    "app/src/main/kotlin/live/ILiveEntryService.kt",
    "app/src/main/kotlin/vertical/DummyVerticalDomainService.kt",
];

fn git(checkout: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(arguments)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates a synthetic checkout whose tracked paths mirror `fixtures/association/probe-v1.json`.
fn probe_checkout(files: &[&str]) -> TempDir {
    let checkout = TempDir::new().unwrap();
    git(checkout.path(), &["init", "--quiet"]);
    git(
        checkout.path(),
        &["config", "user.email", "probe@example.com"],
    );
    git(checkout.path(), &["config", "user.name", "probe"]);
    for file in files {
        let path = checkout.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "// synthetic association probe fixture\n").unwrap();
    }
    git(checkout.path(), &["add", "--all"]);
    git(checkout.path(), &["commit", "--quiet", "-m", "probe"]);
    checkout
}

fn repository_id() -> sctx_domain::RepositoryId {
    "Server".parse().expect("readable RepositoryId")
}

fn resolver_for(checkout: &Path, texts: &[&str]) -> CheckoutReferenceResolver {
    let candidates = texts
        .iter()
        .flat_map(|text| {
            sctx_task_runtime::reference_derivation::claim_path_candidates(text, "", &[])
        })
        .collect::<Vec<_>>();
    let symbols = texts
        .iter()
        .flat_map(|text| claim_symbol_mentions(text, "", &[]))
        .collect::<Vec<_>>();
    CheckoutReferenceResolver::from_checkout(repository_id(), checkout, &candidates, &symbols)
}

/// The read-only `git ls-files` answer is memoized per checkout revision, and the checkout's own
/// Git index is what invalidates it: a second file with the same basename must stop resolving.
#[test]
fn tracked_path_lookup_is_reused_per_revision_and_invalidated_by_a_new_tracked_file() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let texts = ["PoiEntranceAssem.kt registers before the fallback"];
    let candidates =
        sctx_task_runtime::reference_derivation::claim_path_candidates(texts[0], "", &[]);
    let candidate = candidates.first().expect("one path spelling");

    let first = resolver_for(checkout.path(), &texts);
    assert_eq!(
        first,
        resolver_for(checkout.path(), &texts),
        "an unchanged checkout revision reuses one answer"
    );
    assert!(
        first.resolve(candidate).is_some(),
        "a unique basename resolves"
    );

    let duplicate = checkout
        .path()
        .join("app/src/main/kotlin/search/PoiEntranceAssem.kt");
    fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    fs::write(&duplicate, "// a second file with the same basename\n").unwrap();
    git(checkout.path(), &["add", "--all"]);

    let after = resolver_for(checkout.path(), &texts);
    assert_ne!(first, after, "a changed Git index invalidates the memo");
    assert!(
        after.resolve(candidate).is_none(),
        "an ambiguous basename never resolves"
    );
}

fn intent(goal: &str) -> WorkingIntentSnapshot {
    WorkingIntentSnapshot {
        goal: goal.to_owned(),
        current_direction: None,
        in_scope: Vec::new(),
        out_of_scope: Vec::new(),
        domains: vec!["comment".to_owned()],
        platforms: vec!["android".to_owned()],
        constraints: Vec::new(),
        acceptance_conditions: Vec::new(),
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn open_runtime(root: &TempDir, session: &str) -> (TaskRuntime, ExternalSessionLocator) {
    let runtime = TaskRuntime::initialize(root.path()).unwrap();
    let locator = ExternalSessionLocator::new("codex", session).unwrap();
    runtime
        .open_or_create(
            locator.clone(),
            TaskId::new(),
            intent("review the POI entrance fallback"),
            vec![TaskSignal {
                kind: TaskSignalKind::Workspace,
                content: "/workspace/probe".to_owned(),
            }],
        )
        .unwrap();
    (runtime, locator)
}

/// Runs the Candidate Build derivation step for one Episode against a real checkout.
fn derive_with_checkout(
    runtime: &TaskRuntime,
    episode_id: WorkEpisodeId,
    checkout: &Path,
) -> Vec<sctx_task_runtime::DerivedClaimReferences> {
    let candidates = runtime
        .pending_claim_reference_candidates(episode_id)
        .unwrap()
        .expect("a freshly acknowledged Episode still needs its spellings placed");
    let resolver = CheckoutReferenceResolver::from_checkout(
        repository_id(),
        checkout,
        &candidates.paths,
        &candidates.symbols,
    );
    runtime
        .derive_episode_claim_references(
            episode_id,
            &ClaimResolver::new(&|candidate| resolver.resolve_path(candidate), &|symbol| {
                resolver.resolve_symbol(symbol)
            }),
        )
        .unwrap()
}

fn claim(
    kind: ContextKind,
    statement: &str,
    rationale: &str,
    summary: &str,
) -> DirectCheckpointClaimDraft {
    DirectCheckpointClaimDraft {
        context_kind: kind,
        statement: statement.to_owned(),
        rationale: rationale.to_owned(),
        conditions: vec!["enterFrom is search_result".to_owned()],
        evidence: vec![DirectEvidenceDraft {
            evidence_type: EvidenceType::SourceSnapshot,
            summary: summary.to_owned(),
            limitations: vec!["synthetic association probe fixture".to_owned()],
        }],
    }
}

/// Derivation fills a gap the Agent left; it never overrules a topic the Claim already named.
#[test]
fn an_agent_authored_topic_hint_survives_derivation() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derive-authored-topic");
    let task = runtime
        .read_snapshot_by_locator(&locator)
        .unwrap()
        .expect("the fixture Session owns one ActiveTask");
    let outcome = runtime
        .write_agent_checkpoint(&sctx_task_runtime::AgentCheckpointWrite {
            locator,
            expected_task_id: task.task_id,
            expected_intent_revision_id: task.current_intent_revision().unwrap().revision_id,
            expected_episode_version: 0,
            boundary: sctx_task_runtime::CheckpointBoundary::Close,
            claims: vec![sctx_task_runtime::CheckpointClaimDraft {
                context_kind_hint: Some(ContextKind::Issue),
                topic_key_hint: Some("comment/fallback-bar".to_owned()),
                statement: "PoiEntranceAssem.kt:118 registers before the null check".to_owned(),
                rationale: "the fallback never runs".to_owned(),
                applicability: sctx_domain::Applicability::default(),
                evidence_refs: Vec::new(),
                inline_validations: vec![sctx_domain::EvidenceSnapshotDraft {
                    kind: EvidenceType::SourceSnapshot,
                    supports: "PoiEntranceAssem.kt:118 registers before the null check".to_owned(),
                    content: serde_json::json!({"summary": "PoiEntranceAssem.kt:118 registers"}),
                    interpretation: "the registration precedes the guard".to_owned(),
                    limitations: vec!["synthetic association probe fixture".to_owned()],
                }],
                engineering_references: Vec::new(),
            }],
            unknowns: Vec::new(),
        })
        .unwrap();

    let episode_id = outcome.episode.episode.episode_id;
    let derivations = derive_with_checkout(&runtime, episode_id, checkout.path());
    let episode = runtime.read_work_episode(episode_id).unwrap().unwrap();
    let persisted = &episode.checkpoints[0].claims[0];
    assert!(
        !persisted.engineering_references.is_empty(),
        "the spelling is still placed as a graph fact"
    );
    assert_eq!(
        persisted.topic_key_hint.as_deref(),
        Some("comment/fallback-bar")
    );
    assert_eq!(
        derivations[0].topic_key_hint.as_deref(),
        Some("comment/fallback-bar"),
        "the reported derivation is the persisted answer, so a replay reports the same topic"
    );
}

#[test]
fn unique_basename_in_checkout_becomes_a_file_reference_and_topic_hint() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derive-unique");
    let submission = AgentCheckpointSubmission {
        locator,
        claims: vec![claim(
            ContextKind::Issue,
            "PoiEntranceAssem 已注册到优先级管理器，随后以空容器抢占 POI_ENTRY。",
            "优先级注册早于 null 判定。",
            "PoiEntranceAssem.kt:118 完成注册；CommentBottomBarManager.kt:96 的兜底被跳过。",
        )],
        unknowns: Vec::new(),
    };
    let outcome = runtime.submit_agent_checkpoint(&submission).unwrap();
    assert!(
        outcome.checkpoint.claims[0]
            .engineering_references
            .is_empty()
            && outcome.checkpoint.claims[0].topic_key_hint.is_none(),
        "the durable ACK is receipt plus outbox and derives nothing"
    );

    let episode_id = outcome.episode.episode.episode_id;
    let derivations = derive_with_checkout(&runtime, episode_id, checkout.path());
    let episode = runtime.read_work_episode(episode_id).unwrap().unwrap();
    let persisted = &episode.checkpoints[0].claims[0];
    let paths = persisted
        .engineering_references
        .iter()
        .map(|reference| reference.locator.path().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "app/src/main/kotlin/comment/CommentBottomBarManager.kt".to_owned(),
            "app/src/main/kotlin/poi/PoiEntranceAssem.kt".to_owned(),
        ]
    );
    for reference in &persisted.engineering_references {
        assert_eq!(reference.artifact_kind, ArtifactKind::File);
        assert_eq!(reference.relation, ReferenceRelation::DependsOn);
        assert_eq!(
            reference.limitations,
            vec![
                DERIVED_BY_SERVER_LIMITATION.to_owned(),
                UNIQUE_BASENAME_LIMITATION.to_owned(),
            ]
        );
        assert_eq!(reference.supports, persisted.statement);
    }
    assert_eq!(
        persisted.topic_key_hint.as_deref(),
        Some("issue:Server:app/src/main/kotlin/poi/PoiEntranceAssem.kt"),
        "an equal mention count is broken by the first spelling the Agent wrote"
    );
    let derivation = &derivations[0];
    assert_eq!(derivation.claim_id, persisted.claim_id);
    assert_eq!(
        derivation.checkpoint_id,
        episode.checkpoints[0].checkpoint_id
    );
    assert!(derivation.applicability_inherited);
    assert_eq!(
        persisted.applicability.domains,
        vec!["comment".to_owned()],
        "domains still inherit the Working Intent"
    );
    assert!(
        derivation
            .unresolved_hints
            .contains(&"PoiEntranceAssem".to_owned()),
        "identifier spellings stay retrieval hints: {:?}",
        derivation.unresolved_hints
    );
}

/// The Claim shape both real long Sessions actually wrote, end to end through a live checkout.
///
/// Codex `01a08baf` spent seventeen hours writing `MapSceneRuntime.present()` and
/// `CameraController.retargetEnvironment` and never once spelled a path, so the path-only reading
/// placed nothing at all for it and the Pack had no seed to start from. The type name is enough:
/// the file is named after the type, and `git ls-files` answers it in the same query.
#[test]
fn a_claim_that_only_points_at_types_places_the_files_named_after_them() {
    let checkout = probe_checkout(&[
        "components/poi/map/engine/MapSceneRuntime.kt",
        "components/poi/map/camera/CameraController.kt",
        "components/poi/map/engine/Unmentioned.kt",
    ]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derive-symbol-mention");
    let outcome = runtime
        .submit_agent_checkpoint(&AgentCheckpointSubmission {
            locator,
            claims: vec![claim(
                ContextKind::Issue,
                "MapSceneRuntime.present() 在布局回调里重入，导致相机被重置。",
                "CameraController.retargetEnvironment 在同一帧内被调用两次。",
                "重入路径上没有任何守卫。",
            )],
            unknowns: Vec::new(),
        })
        .unwrap();

    let episode_id = outcome.episode.episode.episode_id;
    let derivations = derive_with_checkout(&runtime, episode_id, checkout.path());
    let persisted = runtime.read_work_episode(episode_id).unwrap().unwrap();
    let persisted = &persisted.checkpoints[0].claims[0];
    let paths = persisted
        .engineering_references
        .iter()
        .map(|reference| reference.locator.path().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "components/poi/map/camera/CameraController.kt".to_owned(),
            "components/poi/map/engine/MapSceneRuntime.kt".to_owned(),
        ],
        "a type name the checkout can place is a coordinate; `Unmentioned.kt` is not named"
    );
    for reference in &persisted.engineering_references {
        assert_eq!(reference.artifact_kind, ArtifactKind::File);
        assert_eq!(
            reference.limitations,
            vec![
                DERIVED_BY_SERVER_LIMITATION.to_owned(),
                SYMBOL_MENTION_LIMITATION.to_owned(),
            ],
            "an inferred coordinate always says so"
        );
    }
    assert!(derivations[0].ambiguous_mentions.is_empty());
    assert!(
        persisted
            .topic_key_hint
            .as_deref()
            .is_some_and(|topic| topic.ends_with("engine/MapSceneRuntime.kt")),
        "the type the Claim named first wins the tie, exactly as a path spelling would: {:?}",
        persisted.topic_key_hint
    );
}

/// A type name several tracked files answer to is written down, not silently dropped.
#[test]
fn an_ambiguous_type_name_is_reported_as_the_ambiguity_it_is() {
    let checkout = probe_checkout(&["android/MapSceneRuntime.kt", "ios/MapSceneRuntime.swift"]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derive-symbol-ambiguous");
    let outcome = runtime
        .submit_agent_checkpoint(&AgentCheckpointSubmission {
            locator,
            claims: vec![claim(
                ContextKind::Issue,
                "MapSceneRuntime 在布局回调里重入。",
                "两端实现同名。",
                "没有守卫。",
            )],
            unknowns: Vec::new(),
        })
        .unwrap();

    let episode_id = outcome.episode.episode.episode_id;
    let derivations = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert!(
        derivations[0].engineering_references.is_empty(),
        "placing one of two files would be a guess"
    );
    assert_eq!(derivations[0].ambiguous_mentions.len(), 1);
    assert_eq!(
        derivations[0].ambiguous_mentions[0].spelling,
        "MapSceneRuntime"
    );
    assert_eq!(derivations[0].ambiguous_mentions[0].matches, 2);
}

#[test]
fn ambiguous_basename_and_untracked_path_never_resolve() {
    let checkout = probe_checkout(&["one/Manager.kt", "two/Manager.kt", "only/Alpha.kt"]);
    let resolver = resolver_for(
        checkout.path(),
        &["Manager.kt:10 and Missing.kt:4 and Alpha.kt:7"],
    );
    let derivation = derive_claim_references(
        ContextKind::Discovery,
        "Manager.kt:10 disagrees with Missing.kt:4 while Alpha.kt:7 holds",
        "ambiguity must never become a graph fact",
        &[],
        &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
    );
    let paths = derivation
        .engineering_references
        .iter()
        .map(|reference| reference.locator.path().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["only/Alpha.kt".to_owned()]);
    assert!(
        derivation
            .unresolved_hints
            .contains(&"Manager.kt:10".to_owned())
    );
    assert!(
        derivation
            .unresolved_hints
            .contains(&"Missing.kt:4".to_owned())
    );
    assert_eq!(
        derivation.topic_key_hint.as_deref(),
        Some("discovery:Server:only/Alpha.kt")
    );
}

#[test]
fn missing_checkout_degrades_to_hints_without_failing_the_checkpoint() {
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derive-no-checkout");
    let submission = AgentCheckpointSubmission {
        locator,
        claims: vec![claim(
            ContextKind::Issue,
            "ProductAnchorAssem.kt:202 returns early",
            "ILiveEntryService has no implementation",
            "ProductAnchorAssem.kt:202 skips navigation",
        )],
        unknowns: Vec::new(),
    };
    let outcome = runtime.submit_agent_checkpoint(&submission).unwrap();
    let episode_id = outcome.episode.episode.episode_id;
    let derivations = derive_with_checkout(
        &runtime,
        episode_id,
        Path::new("/nonexistent-shared-context-probe-checkout"),
    );
    let episode = runtime.read_work_episode(episode_id).unwrap().unwrap();
    assert!(
        episode.checkpoints[0].claims[0]
            .engineering_references
            .is_empty()
    );
    assert!(episode.checkpoints[0].claims[0].topic_key_hint.is_none());
    assert_eq!(
        derivations[0].unresolved_hints,
        vec![
            "ILiveEntryService".to_owned(),
            "ProductAnchorAssem.kt:202".to_owned(),
        ]
    );
}

#[test]
fn same_content_replay_returns_the_first_derivation_even_after_the_checkout_moves() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derive-replay");
    let submission = AgentCheckpointSubmission {
        locator,
        claims: vec![claim(
            ContextKind::Issue,
            "ProductAnchorAssem 在商品锚点点击回调中提前 return。",
            "提前 return 跳过导航。",
            "ProductAnchorAssem.kt:202 提前 return；ILiveEntryService.kt:31 没有真实实现。",
        )],
        unknowns: Vec::new(),
    };
    let first = runtime.submit_agent_checkpoint(&submission).unwrap();
    assert!(!first.replayed);
    let episode_id = first.episode.episode.episode_id;
    let derived = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(derived[0].engineering_references.len(), 2);

    // The checkout moves on. Neither a Checkpoint replay nor a Candidate Build rerun may move the
    // identities or the graph facts the first Build decided.
    let replay = runtime.submit_agent_checkpoint(&submission).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, first.operation_id);
    assert_eq!(
        replay.checkpoint.claims[0].claim_id,
        first.checkpoint.claims[0].claim_id
    );
    assert_eq!(
        replay.checkpoint.claims[0].engineering_references, derived[0].engineering_references,
        "a replayed receipt reports the Build's persisted facts"
    );
    assert!(
        runtime
            .pending_claim_reference_candidates(episode_id)
            .unwrap()
            .is_none(),
        "a derived Episode never asks Git again"
    );
    let rebuilt = runtime
        .derive_episode_claim_references(episode_id, &ClaimResolver::nothing())
        .unwrap();
    assert_eq!(
        rebuilt[0].engineering_references,
        derived[0].engineering_references
    );
    assert_eq!(rebuilt[0].topic_key_hint, derived[0].topic_key_hint);
    assert_eq!(rebuilt[0].unresolved_hints, derived[0].unresolved_hints);
}

#[test]
fn candidate_build_can_recompute_the_same_hints_from_the_persisted_claim() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let statement =
        "ProductAnchorAssem.kt:202 提前 return，VerticalDomainRegistry.kt:44 无法解析。";
    let resolver = resolver_for(checkout.path(), &[statement]);
    let derivation = derive_claim_references(
        ContextKind::Issue,
        statement,
        "早退跳过导航",
        &[],
        &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
    );
    let recomputed = claim_hints(
        statement,
        "早退跳过导航",
        &[],
        &derivation.engineering_references,
    );
    assert_eq!(recomputed, derivation.unresolved_hints);
    assert!(recomputed.contains(&"VerticalDomainRegistry.kt:44".to_owned()));
}

/// Prints the derivation table the work package owes for `fixtures/association/probe-v1.json`.
#[test]
#[allow(clippy::too_many_lines)]
fn probe_fixture_contexts_derive_expected_references() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let contexts: &[(usize, ContextKind, &str, &str, &str)] = &[
        (
            1,
            ContextKind::Issue,
            "POI 底栏的 null 保护发生得过晚：当 DummyVerticalDomainService 返回 null 且 canShow() 为 true 时，PoiEntranceAssem 已注册到优先级管理器，随后以空容器抢占 POI_ENTRY，阻止默认评论栏兜底。",
            "优先级注册早于 null 判定，因此空容器仍然占位并让兜底逻辑无法执行。",
            "PoiEntranceAssem.kt:118 在 DummyVerticalDomainService 返回 null 时仍然完成注册；CommentBottomBarManager.kt:96 的默认输入栏兜底因此被跳过。",
        ),
        (
            2,
            ContextKind::Discovery,
            "When vertical-domain implementation modules are absent, the branch intentionally changes behavior from unresolved-service failure to safe degradation, so strict all-configuration functional equivalence does not hold.",
            "The reviewed branch trades a hard resolution failure for a degraded but survivable entrance, which is a deliberate behavioural change rather than a defect.",
            "VerticalDomainRegistry.kt:44 shows the branch replacing unresolved-service failure with a dummy fallback whenever implementation modules are absent.",
        ),
        (
            3,
            ContextKind::Issue,
            "当 ILiveEntryService 无真实实现或 addParamsForLiveAnchor 返回 null 时，ProductAnchorAssem 会在商品锚点点击回调中提前 return，跳过配置分发与进入直播间导航；基线仍会导航，因此该路径不功能等价。",
            "提前 return 让点击回调既不分发配置也不发起导航，与基线行为出现可观察差异。",
            "ProductAnchorAssem.kt:202 商品锚点点击回调在 addParamsForLiveAnchor 返回 null 时提前 return；ILiveEntryService.kt:31 在该配置下没有真实实现。",
        ),
        (
            4,
            ContextKind::Validation,
            "With the real vertical-domain implementation included, the reviewed branch preserves runtime service resolution and the debug app builds successfully.",
            "Runtime resolution keeps returning the real implementation, so the degraded path is never entered in this configuration.",
            "assembleDebug finished with BUILD SUCCESSFUL after the real vertical-domain implementation module was added back (build.log:512).",
        ),
        (
            5,
            ContextKind::Discovery,
            "无真实实现时，Dummy 与旧动态代理在 primitive、void 和 nullable 返回值上大多等价；music-detail 日志方法由 null 变为空 Map。",
            "逐类型比较返回值后，只有 music-detail 日志方法出现了可观察的返回值差异。",
            "DummyRegistry.kt:57 的 Dummy 实现对 primitive 返回 0、对 void 返回 Unit、对 nullable 返回 null；music-detail 日志方法返回空 Map。",
        ),
        (
            6,
            ContextKind::Validation,
            "All directly changed library modules compile successfully with their Debug Kotlin tasks.",
            "Each changed module was compiled on its own so a failure could be attributed to one module.",
            "gradle :feature-comment:compileDebugKotlin :feature-poi:compileDebugKotlin :feature-anchor:compileDebugKotlin all reported BUILD SUCCESSFUL (build.log:41).",
        ),
        (
            7,
            ContextKind::Progress,
            "The release checklist now verifies that the packaged command line tool reports the expected semantic version before publishing.",
            "A mismatched packaged version had previously shipped, so the checklist gained an explicit assertion step.",
            "release-checklist.md:12 records the packaged version assertion step.",
        ),
    ];

    let all_texts = contexts
        .iter()
        .flat_map(|(_, _, statement, rationale, summary)| [*statement, *rationale, *summary])
        .collect::<Vec<_>>();
    let resolver = resolver_for(checkout.path(), &all_texts);

    let mut derived_paths = Vec::new();
    for (index, kind, statement, rationale, summary) in contexts {
        let derivation = derive_claim_references(
            *kind,
            statement,
            rationale,
            &[summary],
            &ClaimResolver::paths(&|c| resolver.resolve_path(c)),
        );
        let paths = derivation
            .engineering_references
            .iter()
            .map(|reference| reference.locator.path().as_str().to_owned())
            .collect::<Vec<_>>();
        println!(
            "context {index}: references={paths:?} topic={:?} hints={:?}",
            derivation.topic_key_hint, derivation.unresolved_hints
        );
        derived_paths.push(paths);
    }

    assert_eq!(
        derived_paths[0],
        vec![
            "app/src/main/kotlin/comment/CommentBottomBarManager.kt".to_owned(),
            "app/src/main/kotlin/poi/PoiEntranceAssem.kt".to_owned(),
        ]
    );
    assert!(
        derived_paths[1].is_empty(),
        "VerticalDomainRegistry.kt is untracked"
    );
    assert_eq!(
        derived_paths[2],
        vec![
            "app/src/main/kotlin/anchor/ProductAnchorAssem.kt".to_owned(),
            "app/src/main/kotlin/live/ILiveEntryService.kt".to_owned(),
        ]
    );
    assert!(
        derived_paths[3].is_empty(),
        "build.log is not a source extension"
    );
    assert!(derived_paths[4].is_empty(), "DummyRegistry.kt is untracked");
    assert!(
        derived_paths[5].is_empty(),
        "no path spelling survives extraction"
    );
    assert!(
        derived_paths[6].is_empty(),
        "release-checklist.md is not a source extension"
    );
}

#[test]
fn extraction_is_pure_without_a_resolver() {
    let derivation = derive_claim_references(
        ContextKind::Validation,
        "build.log:512 reported BUILD SUCCESSFUL for assembleDebug",
        "no source path is written down",
        &[],
        &ClaimResolver::nothing(),
    );
    assert!(derivation.engineering_references.is_empty());
    assert_eq!(
        derivation.unresolved_hints,
        vec!["assembleDebug".to_owned()]
    );
}

// ---------------------------------------------------------------------------------------------
// The derivation record, and the two ways an ambiguity gets a second look
// ---------------------------------------------------------------------------------------------

/// One Checkpoint whose single Claim names an ambiguous file and an unplaceable one.
fn ambiguous_episode(runtime: &TaskRuntime, locator: ExternalSessionLocator) -> WorkEpisodeId {
    runtime
        .submit_agent_checkpoint(&AgentCheckpointSubmission {
            locator,
            claims: vec![claim(
                ContextKind::Issue,
                "Manager.kt:11 注册顺序有误。",
                "Missing.kt:4 已经不在仓库里。",
                "两端同名实现互相覆盖。",
            )],
            unknowns: Vec::new(),
        })
        .unwrap()
        .episode
        .episode
        .episode_id
}

/// The record is the whole point: what was placed, what was ambiguous with how many files
/// answering, and a sample of what nothing answered to.
#[test]
fn the_derivation_record_says_what_was_placed_ambiguous_and_unplaced() {
    let checkout = probe_checkout(&["one/Manager.kt", "two/Manager.kt", "only/Alpha.kt"]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derivation-record");
    let episode_id = ambiguous_episode(&runtime, locator);
    derive_with_checkout(&runtime, episode_id, checkout.path());

    let record = runtime
        .reference_derivation_record(episode_id)
        .unwrap()
        .expect("the first derivation records what it found");
    assert_eq!(record.placed, 0);
    assert_eq!(record.ambiguous.len(), 1);
    assert_eq!(record.ambiguous[0].spelling, "Manager.kt");
    assert_eq!(record.ambiguous[0].matches, 2);
    assert!(record.unresolved >= 2, "{record:?}");
    assert!(
        record
            .unresolved_sample
            .iter()
            .any(|spelling| spelling == "Missing.kt:4"),
        "{:?}",
        record.unresolved_sample
    );
    assert!(record.derived_at_unix_seconds.is_some());
    assert!(!record.reopened);
}

/// An Episode that left an ambiguity stays open to being asked again, and the answer it gets
/// against an unchanged checkout is the answer it already has: nothing is rewritten.
#[test]
fn an_unchanged_checkout_replays_the_recorded_answer_instead_of_rewriting_it() {
    let checkout = probe_checkout(&["one/Manager.kt", "two/Manager.kt"]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derivation-idempotent");
    let episode_id = ambiguous_episode(&runtime, locator);
    let first = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(first[0].ambiguous_mentions.len(), 1);
    let recorded = runtime.reference_derivation_record(episode_id).unwrap();

    assert!(
        runtime
            .pending_claim_reference_candidates(episode_id)
            .unwrap()
            .is_some(),
        "a recorded ambiguity is a question the checkout may answer differently later"
    );
    let again = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(
        again[0].engineering_references,
        first[0].engineering_references
    );
    assert_eq!(again[0].topic_key_hint, first[0].topic_key_hint);
    assert_eq!(again[0].unresolved_hints, first[0].unresolved_hints);
    assert!(
        again[0].ambiguous_mentions.is_empty(),
        "the same ambiguity is the same answer, so this is a replay, not a derivation"
    );
    assert_eq!(
        runtime.reference_derivation_record(episode_id).unwrap(),
        recorded,
        "a replay leaves the record byte-identical, timestamp included"
    );
}

/// The relaxation itself: the checkout loses the duplicate, so the spelling has one answer now.
#[test]
fn an_ambiguity_the_checkout_has_since_settled_is_derived_on_the_next_build() {
    let checkout = probe_checkout(&["one/Manager.kt", "two/Manager.kt"]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derivation-settled");
    let episode_id = ambiguous_episode(&runtime, locator);
    assert!(
        derive_with_checkout(&runtime, episode_id, checkout.path())[0]
            .engineering_references
            .is_empty()
    );

    git(checkout.path(), &["rm", "--quiet", "two/Manager.kt"]);
    let settled = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(
        settled[0]
            .engineering_references
            .iter()
            .map(|reference| reference.locator.path().as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["one/Manager.kt".to_owned()],
        "an ambiguity is a question, and the checkout now has one answer to it"
    );
    let record = runtime
        .reference_derivation_record(episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.placed, 1);
    assert!(
        record.ambiguous.is_empty(),
        "nothing is ambiguous any more, so the Episode is closed for good"
    );
    assert!(
        runtime
            .pending_claim_reference_candidates(episode_id)
            .unwrap()
            .is_none()
    );

    // The persisted Claim carries the coordinate, and asking again changes nothing.
    let persisted = runtime.read_work_episode(episode_id).unwrap().unwrap();
    assert_eq!(
        persisted.checkpoints[0].claims[0]
            .engineering_references
            .len(),
        1
    );
    assert_eq!(
        runtime
            .derive_episode_claim_references(episode_id, &ClaimResolver::nothing())
            .unwrap()[0]
            .engineering_references,
        settled[0].engineering_references
    );
}

/// A re-derivation adds; it never takes a coordinate back. A checkout that lost the file the
/// first derivation placed does not un-place it.
#[test]
fn a_re_derivation_never_retracts_a_coordinate_a_reviewer_may_have_seen() {
    let checkout = probe_checkout(&["one/Manager.kt", "two/Manager.kt", "only/Alpha.kt"]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derivation-monotone");
    let episode_id = runtime
        .submit_agent_checkpoint(&AgentCheckpointSubmission {
            locator,
            claims: vec![claim(
                ContextKind::Issue,
                "Alpha.kt:7 与 Manager.kt:11 的注册顺序冲突。",
                "两端同名实现互相覆盖。",
                "冲突路径上没有守卫。",
            )],
            unknowns: Vec::new(),
        })
        .unwrap()
        .episode
        .episode
        .episode_id;
    let first = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(
        first[0]
            .engineering_references
            .iter()
            .map(|reference| reference.locator.path().as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["only/Alpha.kt".to_owned()]
    );

    git(checkout.path(), &["rm", "--quiet", "only/Alpha.kt"]);
    git(checkout.path(), &["rm", "--quiet", "two/Manager.kt"]);
    let again = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(
        again[0]
            .engineering_references
            .iter()
            .map(|reference| reference.locator.path().as_str().to_owned())
            .collect::<Vec<_>>(),
        vec!["one/Manager.kt".to_owned(), "only/Alpha.kt".to_owned()],
        "the settled ambiguity is added and the deleted file's coordinate stays"
    );
}

/// The operator's lever: reopening makes the next Build ask again even when nothing changed.
#[test]
fn reopening_asks_the_checkout_again_and_answering_clears_the_request() {
    let checkout = probe_checkout(&["one/Manager.kt", "two/Manager.kt"]);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derivation-reopen");
    let episode_id = ambiguous_episode(&runtime, locator);
    derive_with_checkout(&runtime, episode_id, checkout.path());

    assert_eq!(runtime.reopen_ambiguous_reference_derivations().unwrap(), 1);
    assert_eq!(
        runtime.reopen_ambiguous_reference_derivations().unwrap(),
        0,
        "already reopened is not reopened again"
    );
    assert!(
        runtime
            .reference_derivation_record(episode_id)
            .unwrap()
            .unwrap()
            .reopened
    );

    let reopened = derive_with_checkout(&runtime, episode_id, checkout.path());
    assert_eq!(
        reopened[0].ambiguous_mentions.len(),
        1,
        "a reopened Episode is derived again even though the checkout says the same thing"
    );
    let record = runtime
        .reference_derivation_record(episode_id)
        .unwrap()
        .unwrap();
    assert!(
        !record.reopened,
        "the request is answered, so it is cleared"
    );
    assert_eq!(record.ambiguous.len(), 1);
}

/// A clean derivation is closed for good: no record of a question means no lookup, ever again.
#[test]
fn a_clean_derivation_is_never_reopened_and_never_asks_git_again() {
    let checkout = probe_checkout(FIXTURE_FILES);
    let root = TempDir::new().unwrap();
    let (runtime, locator) = open_runtime(&root, "derivation-clean");
    let episode_id = runtime
        .submit_agent_checkpoint(&AgentCheckpointSubmission {
            locator,
            claims: vec![claim(
                ContextKind::Issue,
                "PoiEntranceAssem.kt:118 提前注册。",
                "兜底被跳过。",
                "CommentBottomBarManager.kt:96 的兜底被跳过。",
            )],
            unknowns: Vec::new(),
        })
        .unwrap()
        .episode
        .episode
        .episode_id;
    derive_with_checkout(&runtime, episode_id, checkout.path());
    let record = runtime
        .reference_derivation_record(episode_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.placed, 2);
    assert!(record.ambiguous.is_empty());
    assert_eq!(runtime.reopen_ambiguous_reference_derivations().unwrap(), 0);
    assert!(
        runtime
            .pending_claim_reference_candidates(episode_id)
            .unwrap()
            .is_none()
    );
}
