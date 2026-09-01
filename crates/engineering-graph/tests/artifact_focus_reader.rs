use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ContextId, ContextKind,
    ContextRevision, EngineeringArtifact, EngineeringReference, EvidenceId, EvidenceSnapshot,
    EvidenceType, ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId,
    RepositoryIdentity, RevisionId, SpaceId,
};
use sctx_engineering_graph::{
    ARTIFACT_FOCUS_QUERY_BUDGET, ArtifactFocusOutcome, ArtifactFocusReader, ArtifactObservation,
    ArtifactSourceState, EngineeringProjectionStore, EngineeringReferenceResolver,
    GraphContextSafety, GraphContextSafetyBlocker, GraphContextSnapshot, GraphContextStatus,
    MAX_ARTIFACT_FOCUS_HITS, ProjectedEngineeringReference, RepositoryScanOutcome,
    RepositorySnapshot, SnapshotArtifact, SnapshotSourcePolicy, SourceLanguage,
};
use tempfile::TempDir;

const STATEMENT: &str =
    "The default comment bottom bar must survive an absent vertical-domain service";

fn repository() -> RepositoryIdentity {
    RepositoryIdentity {
        repository_id: RepositoryId::new(),
        canonical_name: "focus".to_owned(),
    }
}

fn path(value: &str) -> RepoRelativePath {
    RepoRelativePath::new(value).unwrap()
}

fn locator(value: &str) -> ArtifactLocator {
    ArtifactLocator::File { path: path(value) }
}

fn artifact(repository: &RepositoryIdentity, locator: ArtifactLocator) -> SnapshotArtifact {
    let display_name = locator.path().as_str().to_owned();
    let artifact = EngineeringArtifact {
        repository: repository.clone(),
        artifact_key: ArtifactKey::derive(repository.repository_id.clone(), locator).unwrap(),
        display_name,
    };
    artifact.validate().unwrap();
    SnapshotArtifact {
        artifact,
        snapshot_generation: "snap_focus".to_owned(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        observations: vec![ArtifactObservation {
            path: "src/PoiEntranceAssem.kt".to_owned(),
            line: Some(1),
            language: SourceLanguage::Kotlin,
            source_state: ArtifactSourceState::TrackedHead,
        }],
    }
}

fn snapshot(
    repository: &RepositoryIdentity,
    artifacts: Vec<SnapshotArtifact>,
) -> RepositoryScanOutcome {
    let planned_paths = artifacts
        .iter()
        .map(|artifact| artifact.artifact.artifact_key.locator().path().clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    RepositoryScanOutcome::Available(RepositorySnapshot {
        repository_id: repository.repository_id.clone(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        policy_version: "planned-paths-plus-safe-tracked-modifications-v2",
        head_tree_oid: "tree-focus".to_owned(),
        generation: "snap_focus".to_owned(),
        planned_paths,
        artifacts,
        scanned_files: 1,
        scanned_bytes: 1,
        skipped_files: Vec::new(),
    })
}

fn reference(
    repository: &RepositoryIdentity,
    locator: ArtifactLocator,
) -> ProjectedEngineeringReference {
    ProjectedEngineeringReference {
        context_id: ContextId::new(),
        revision_id: RevisionId::new(),
        reference: EngineeringReference {
            reference_id: ReferenceId::new(),
            repository_id: repository.repository_id.clone(),
            artifact_kind: ArtifactKind::File,
            relation: ReferenceRelation::Implements,
            locator,
            supports: "the exact file locator was directly inspected".to_owned(),
            limitations: vec!["moves and renames are not recovered".to_owned()],
        },
    }
}

fn context(reference: &ProjectedEngineeringReference, accepted: bool) -> GraphContextSnapshot {
    let (status, safety) = if accepted {
        (
            GraphContextStatus::Accepted,
            GraphContextSafety {
                automatic_injection_eligible: true,
                blockers: BTreeSet::default(),
            },
        )
    } else {
        (
            GraphContextStatus::Candidate,
            GraphContextSafety {
                automatic_injection_eligible: false,
                blockers: BTreeSet::from([GraphContextSafetyBlocker::NotAccepted]),
            },
        )
    };
    GraphContextSnapshot {
        space_id: SpaceId::new(),
        space_title: "Artifact focus fixture".to_owned(),
        context_id: reference.context_id,
        revision: ContextRevision {
            problem_view: None,
            hints: Vec::new(),
            revision_id: reference.revision_id,
            parent_revision_ids: Vec::new(),
            kind: ContextKind::Decision,
            topic_key: Some("focus/fixture".to_owned()),
            statement: STATEMENT.to_owned(),
            rationale: "The fixture exercises one immutable Context".to_owned(),
            applicability: Applicability::default(),
            assumptions: Vec::new(),
            recheck_when: vec!["the exact locator changes".to_owned()],
            relations: Vec::new(),
            evidence: vec![EvidenceSnapshot {
                evidence_id: EvidenceId::new(),
                kind: EvidenceType::ExperimentRecord,
                supports: "The fixture passed".to_owned(),
                content: serde_json::json!({"result": "passed"}),
                interpretation: "The Context is complete".to_owned(),
                limitations: vec!["synthetic fixture".to_owned()],
            }],
        },
        status,
        evidence_completeness: 1_000,
        safety,
        relations: Vec::new(),
    }
}

struct Fixture {
    _temporary: TempDir,
    reader: ArtifactFocusReader,
    repository: RepositoryIdentity,
    context_id: ContextId,
}

fn build(accepted: bool) -> Fixture {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join(".shared-context");
    let repository = repository();
    let reference = reference(&repository, locator("src/PoiEntranceAssem.kt"));
    let context_id = reference.context_id;
    let contexts = vec![context(&reference, accepted)];
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&reference),
            &[snapshot(
                &repository,
                vec![artifact(&repository, locator("src/PoiEntranceAssem.kt"))],
            )],
            &contexts,
        )
        .unwrap();
    EngineeringProjectionStore::initialize(&root)
        .unwrap()
        .rebuild(&projection)
        .unwrap();
    Fixture {
        reader: ArtifactFocusReader::new(&root),
        _temporary: temporary,
        repository,
        context_id,
    }
}

#[test]
fn exact_file_artifact_returns_only_accepted_injection_eligible_context() {
    let fixture = build(true);
    let lookup = fixture
        .reader
        .accepted_contexts_for_file(
            &fixture.repository.repository_id,
            &path("src/PoiEntranceAssem.kt"),
            MAX_ARTIFACT_FOCUS_HITS,
            ARTIFACT_FOCUS_QUERY_BUDGET,
        )
        .unwrap();
    assert_eq!(lookup.outcome, ArtifactFocusOutcome::Completed);
    assert_eq!(lookup.hits.len(), 1);
    assert_eq!(lookup.hits[0].context_id, fixture.context_id.to_string());
    assert_eq!(lookup.hits[0].statement, STATEMENT);
}

#[test]
fn unaccepted_context_missing_path_and_foreign_repository_all_return_no_hit() {
    let candidate = build(false);
    let candidate_lookup = candidate
        .reader
        .accepted_contexts_for_file(
            &candidate.repository.repository_id,
            &path("src/PoiEntranceAssem.kt"),
            MAX_ARTIFACT_FOCUS_HITS,
            ARTIFACT_FOCUS_QUERY_BUDGET,
        )
        .unwrap();
    assert_eq!(candidate_lookup.outcome, ArtifactFocusOutcome::Completed);
    assert!(candidate_lookup.hits.is_empty());

    let accepted = build(true);
    let missing_path_lookup = accepted
        .reader
        .accepted_contexts_for_file(
            &accepted.repository.repository_id,
            &path("src/Unrelated.kt"),
            MAX_ARTIFACT_FOCUS_HITS,
            ARTIFACT_FOCUS_QUERY_BUDGET,
        )
        .unwrap();
    assert_eq!(missing_path_lookup.outcome, ArtifactFocusOutcome::Completed);
    assert!(missing_path_lookup.hits.is_empty());
    let foreign_repository_lookup = accepted
        .reader
        .accepted_contexts_for_file(
            &RepositoryId::new(),
            &path("src/PoiEntranceAssem.kt"),
            MAX_ARTIFACT_FOCUS_HITS,
            ARTIFACT_FOCUS_QUERY_BUDGET,
        )
        .unwrap();
    assert_eq!(
        foreign_repository_lookup.outcome,
        ArtifactFocusOutcome::Completed
    );
    assert!(foreign_repository_lookup.hits.is_empty());
}

#[test]
fn absent_projection_database_is_no_hit_instead_of_an_error() {
    let temporary = TempDir::new().unwrap();
    let reader = ArtifactFocusReader::new(temporary.path().join(".shared-context"));
    assert!(!reader.database_path().exists());
    let lookup = reader
        .accepted_contexts_for_file(
            &RepositoryId::new(),
            &path("src/PoiEntranceAssem.kt"),
            MAX_ARTIFACT_FOCUS_HITS,
            ARTIFACT_FOCUS_QUERY_BUDGET,
        )
        .unwrap();
    assert_eq!(lookup.outcome, ArtifactFocusOutcome::ProjectionAbsent);
    assert!(lookup.hits.is_empty());
}

#[test]
fn exhausted_query_budget_degrades_to_no_hit() {
    let fixture = build(true);
    let lookup = fixture
        .reader
        .accepted_contexts_for_file(
            &fixture.repository.repository_id,
            &path("src/PoiEntranceAssem.kt"),
            MAX_ARTIFACT_FOCUS_HITS,
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(lookup.outcome, ArtifactFocusOutcome::BudgetExceeded);
    assert!(lookup.hits.is_empty());
}

#[test]
fn read_only_lookup_meets_the_hook_hot_path_budget() {
    let fixture = build(true);
    let mut samples = Vec::with_capacity(100);
    // Latency is measured, not enforced by the deadline: a generous budget keeps
    // this a timing measurement instead of re-testing the abort path, which
    // `exhausted_query_budget_degrades_to_no_hit` already covers.
    for _ in 0..100 {
        let started = Instant::now();
        let lookup = fixture
            .reader
            .accepted_contexts_for_file(
                &fixture.repository.repository_id,
                &path("src/PoiEntranceAssem.kt"),
                MAX_ARTIFACT_FOCUS_HITS,
                Duration::from_secs(30),
            )
            .unwrap();
        samples.push(started.elapsed());
        assert_eq!(lookup.hits.len(), 1);
    }
    samples.sort_unstable();
    let p99 = samples[98];
    println!("read-only Artifact focus lookup p99 = {p99:?}");
    assert!(
        p99 <= Duration::from_millis(200),
        "read-only Artifact focus lookup p99 {p99:?} exceeds the 200ms Hook budget"
    );
}
