use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactKind, ArtifactLocator, ContextId, ContextKind,
    ContextRevision, EngineeringArtifact, EngineeringReference, EvidenceId, EvidenceSnapshot,
    EvidenceType, ReferenceId, ReferenceRelation, RepoRelativePath, RepositoryId,
    RepositoryIdentity, ResolutionStatus, RevisionId, SpaceId,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, GraphContextSafety, GraphContextSnapshot, GraphContextStatus,
    MatchBasis, ProjectedEngineeringReference, RepositoryScanOutcome, RepositorySnapshot,
    SnapshotArtifact, SnapshotSourcePolicy, SourceLanguage,
};
use tempfile::TempDir;

fn repository(name: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        repository_id: RepositoryId::new(),
        canonical_name: name.to_owned(),
    }
}

fn path(value: &str) -> RepoRelativePath {
    RepoRelativePath::new(value).unwrap()
}

fn file(value: &str) -> ArtifactLocator {
    ArtifactLocator::File { path: path(value) }
}

fn module(value: &str) -> ArtifactLocator {
    ArtifactLocator::Module { path: path(value) }
}

fn api(value: &str) -> ArtifactLocator {
    ArtifactLocator::Api {
        path: path("api/search.yaml"),
        protocol: "http".to_owned(),
        operation: "GET".to_owned(),
        normalized_route: value.to_owned(),
    }
}

fn schema(name: &str) -> ArtifactLocator {
    ArtifactLocator::Schema {
        path: path("api/search.yaml"),
        namespace: "search".to_owned(),
        version: "v2".to_owned(),
        qualified_name: format!("search::{name}"),
    }
}

fn symbol(name: &str, signature: &str) -> ArtifactLocator {
    ArtifactLocator::Symbol {
        path: path("src/search.ts"),
        language: "typescript".to_owned(),
        module: "src".to_owned(),
        enclosing_type: Some("SearchController".to_owned()),
        symbol_name: name.to_owned(),
        signature: signature.to_owned(),
    }
}

fn test_locator(name: &str) -> ArtifactLocator {
    ArtifactLocator::Test {
        path: path("tests/search.rs"),
        qualified_test_name: format!("search::{name}"),
    }
}

fn artifact(
    repository: &RepositoryIdentity,
    locator: ArtifactLocator,
    occurrences: usize,
) -> SnapshotArtifact {
    let display_name = locator.path().as_str().to_owned();
    let artifact = EngineeringArtifact {
        repository: repository.clone(),
        artifact_key: ArtifactKey::derive(repository.repository_id.clone(), locator).unwrap(),
        display_name,
    };
    artifact.validate().unwrap();
    SnapshotArtifact {
        artifact,
        snapshot_generation: "snap_fixture".to_owned(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        observations: (0..occurrences)
            .map(|index| ArtifactObservation {
                path: "src/search.ts".to_owned(),
                line: Some(u32::try_from(index + 1).unwrap()),
                language: SourceLanguage::TypeScriptJavaScript,
                source_state: ArtifactSourceState::TrackedHead,
            })
            .collect(),
    }
}

fn snapshot(
    repository: &RepositoryIdentity,
    generation: &str,
    mut artifacts: Vec<SnapshotArtifact>,
) -> RepositoryScanOutcome {
    artifacts.sort_by(|left, right| left.artifact.artifact_key.cmp(&right.artifact.artifact_key));
    for artifact in &mut artifacts {
        generation.clone_into(&mut artifact.snapshot_generation);
    }
    let planned_paths = artifacts
        .iter()
        .map(|artifact| artifact.artifact.artifact_key.locator().path().clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    RepositoryScanOutcome::Available(RepositorySnapshot {
        repository_id: repository.repository_id.clone(),
        source_policy: SnapshotSourcePolicy::PlannedPathsWithSafeTrackedModifications,
        policy_version: "planned-paths-plus-safe-tracked-modifications-v2",
        head_tree_oid: format!("tree-{generation}"),
        generation: generation.to_owned(),
        planned_paths,
        artifacts,
        scanned_files: 1,
        scanned_bytes: 1,
        skipped_files: Vec::new(),
    })
}

fn relation(kind: ArtifactKind) -> ReferenceRelation {
    match kind {
        ArtifactKind::File | ArtifactKind::Module | ArtifactKind::Symbol => {
            ReferenceRelation::Implements
        }
        ArtifactKind::Api | ArtifactKind::Schema => ReferenceRelation::Defines,
        ArtifactKind::Test => ReferenceRelation::Validates,
    }
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
            artifact_kind: locator.kind(),
            relation: relation(locator.kind()),
            locator,
            supports: "the exact locator was directly inspected".to_owned(),
            limitations: vec!["moves and renames are not recovered".to_owned()],
        },
    }
}

fn graph_contexts(references: &[ProjectedEngineeringReference]) -> Vec<GraphContextSnapshot> {
    references
        .iter()
        .map(|reference| GraphContextSnapshot {
            space_id: SpaceId::new(),
            space_title: "Resolver fixture".to_owned(),
            context_id: reference.context_id,
            revision: ContextRevision {
                revision_id: reference.revision_id,
                parent_revision_ids: Vec::new(),
                kind: ContextKind::Decision,
                topic_key: Some("resolver/fixture".to_owned()),
                statement: "The exact Artifact locator is authoritative".to_owned(),
                rationale: "The resolver fixture exercises one immutable Context".to_owned(),
                applicability: Applicability::default(),
                assumptions: Vec::new(),
                recheck_when: vec!["the exact locator changes".to_owned()],
                relations: Vec::new(),
                evidence: vec![EvidenceSnapshot {
                    evidence_id: EvidenceId::new(),
                    kind: EvidenceType::ExperimentRecord,
                    supports: "The resolver fixture passed".to_owned(),
                    content: serde_json::json!({"result": "passed"}),
                    interpretation: "The Context is complete".to_owned(),
                    limitations: vec!["synthetic fixture".to_owned()],
                }],
            },
            status: GraphContextStatus::Accepted,
            evidence_completeness: 1_000,
            safety: GraphContextSafety {
                automatic_injection_eligible: true,
                blockers: BTreeSet::default(),
            },
            relations: Vec::new(),
        })
        .collect()
}

#[test]
fn all_kind_specific_exact_locators_resolve_with_explainable_basis() {
    let repository = repository("exact");
    let locators = vec![
        (file("src/search.ts"), MatchBasis::ExactFilePath),
        (module("src/search"), MatchBasis::ExactModulePath),
        (api("/v2/search"), MatchBasis::ExactApiLocator),
        (schema("SearchResponse"), MatchBasis::ExactSchemaLocator),
        (
            symbol("search", "search(query: string)"),
            MatchBasis::ExactQualifiedSymbolLocator,
        ),
        (
            test_locator("returns_results"),
            MatchBasis::ExactQualifiedTestLocator,
        ),
    ];
    let references = locators
        .iter()
        .map(|(locator, _)| reference(&repository, locator.clone()))
        .collect::<Vec<_>>();
    let artifacts = locators
        .iter()
        .map(|(locator, _)| artifact(&repository, locator.clone(), 1))
        .collect::<Vec<_>>();
    let expected = references
        .iter()
        .zip(&locators)
        .map(|(reference, (_, basis))| (reference.reference.reference_id, *basis))
        .collect::<BTreeMap<_, _>>();
    let projection = EngineeringReferenceResolver
        .resolve(
            &references,
            &[snapshot(&repository, "snap-exact", artifacts)],
            &graph_contexts(&references),
        )
        .unwrap();
    for resolved in &projection.references {
        assert_eq!(resolved.resolution.status, ResolutionStatus::Resolved);
        assert!(resolved.association.is_some());
        assert_eq!(resolved.evidence[0].basis, expected[&resolved.reference_id]);
        assert!((resolved.evidence[0].confidence - 1.0).abs() < f64::EPSILON);
    }
}

#[test]
fn move_or_rename_is_missing_and_never_guessed_from_similar_content() {
    let repository = repository("move");
    let projected = reference(&repository, file("old/search.ts"));
    let current = artifact(&repository, file("new/search.ts"), 1);
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&projected),
            &[snapshot(&repository, "snap-moved", vec![current])],
            &graph_contexts(std::slice::from_ref(&projected)),
        )
        .unwrap();
    assert_eq!(
        projection.references[0].resolution.status,
        ResolutionStatus::Missing
    );
    assert!(projection.references[0].association.is_none());
    assert!(projection.references[0].evidence.is_empty());
}

#[test]
fn same_display_name_at_different_paths_does_not_affect_exact_selection() {
    let repository = repository("same-content");
    let wanted = file("src/search.ts");
    let projected = reference(&repository, wanted.clone());
    let first = artifact(&repository, wanted, 1);
    let mut second = artifact(&repository, file("legacy/search.ts"), 1);
    second
        .artifact
        .display_name
        .clone_from(&first.artifact.display_name);
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&projected),
            &[snapshot(&repository, "snap-same", vec![second, first])],
            &graph_contexts(std::slice::from_ref(&projected)),
        )
        .unwrap();
    let resolved = &projection.references[0];
    assert_eq!(resolved.resolution.status, ResolutionStatus::Resolved);
    assert_eq!(
        resolved
            .resolution
            .resolved_artifact
            .as_ref()
            .unwrap()
            .locator(),
        &file("src/search.ts")
    );
}

#[test]
fn duplicate_qualified_occurrences_are_ambiguous_and_never_form_edge() {
    let repository = repository("ambiguous");
    let locator = symbol("search", "search(query: string)");
    let projected = reference(&repository, locator.clone());
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&projected),
            &[snapshot(
                &repository,
                "snap-ambiguous",
                vec![artifact(&repository, locator, 2)],
            )],
            &graph_contexts(std::slice::from_ref(&projected)),
        )
        .unwrap();
    let ambiguous = &projection.references[0];
    assert_eq!(ambiguous.resolution.status, ResolutionStatus::Ambiguous);
    assert!(ambiguous.association.is_none());
    assert_eq!(ambiguous.resolution.candidates.len(), 1);
}

#[test]
fn unavailable_repository_recovers_only_when_exact_locator_returns() {
    let repository = repository("unavailable");
    let locator = api("/v2/search");
    let projected = reference(&repository, locator.clone());
    let unavailable = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&projected),
            &[RepositoryScanOutcome::Unavailable {
                repository_id: repository.repository_id.clone(),
                reason: "checkout offline".to_owned(),
            }],
            &graph_contexts(std::slice::from_ref(&projected)),
        )
        .unwrap();
    assert_eq!(
        unavailable.references[0].resolution.status,
        ResolutionStatus::Unavailable
    );
    assert!(unavailable.references[0].association.is_none());

    let recovered = EngineeringReferenceResolver
        .resolve_incremental(
            &unavailable,
            std::slice::from_ref(&projected),
            &[snapshot(
                &repository,
                "snap-recovered",
                vec![artifact(&repository, locator, 1)],
            )],
            &graph_contexts(std::slice::from_ref(&projected)),
        )
        .unwrap();
    assert_eq!(
        recovered.references[0].resolution.status,
        ResolutionStatus::Resolved
    );
}

#[test]
fn incremental_resolution_and_tree_provenance_store_equal_scratch() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("shared-context");
    let repository = repository("projection");
    let locator = file("src/lib.rs");
    let projected = reference(&repository, locator.clone());
    let snapshots = [snapshot(
        &repository,
        "snap-projection",
        vec![artifact(&repository, locator, 1)],
    )];
    let resolver = EngineeringReferenceResolver;
    let contexts = graph_contexts(std::slice::from_ref(&projected));
    let scratch = resolver
        .resolve(std::slice::from_ref(&projected), &snapshots, &contexts)
        .unwrap();
    let incremental = resolver
        .resolve_incremental(
            &scratch,
            std::slice::from_ref(&projected),
            &snapshots,
            &contexts,
        )
        .unwrap();
    assert_eq!(incremental, scratch);

    let store = EngineeringProjectionStore::initialize(&root).unwrap();
    store
        .rebuild_for_context_tree(&scratch, Some("context-tree-a"))
        .unwrap();
    let first_bytes = store.canonical_bytes().unwrap().unwrap();
    let pinned = store.read_snapshot().unwrap().unwrap();
    assert_eq!(pinned.context_tree_oid.as_deref(), Some("context-tree-a"));
    assert_eq!(pinned.projection, scratch);
    let database = store.database_path().to_path_buf();
    drop(store);
    fs::remove_file(database).unwrap();
    let rebuilt = EngineeringProjectionStore::initialize(&root).unwrap();
    rebuilt
        .rebuild_for_context_tree(&scratch, Some("context-tree-a"))
        .unwrap();
    assert_eq!(rebuilt.canonical_bytes().unwrap().unwrap(), first_bytes);
}

#[test]
fn empty_snapshot_is_missing_without_diagnostic_edge() {
    let repository = repository("missing");
    let projected = reference(&repository, file("missing.rs"));
    let projection = EngineeringReferenceResolver
        .resolve(
            std::slice::from_ref(&projected),
            &[snapshot(&repository, "snap-empty", Vec::new())],
            &graph_contexts(std::slice::from_ref(&projected)),
        )
        .unwrap();
    assert_eq!(
        projection.references[0].resolution.status,
        ResolutionStatus::Missing
    );
    assert!(projection.references[0].association.is_none());
}
