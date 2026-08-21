use std::fs;

use sctx_domain::{
    ArtifactKey, ArtifactKeyBasis, ArtifactKind, ContentFingerprint, ContextId,
    EngineeringArtifact, EngineeringReference, LocatorHints, ReferenceId, ReferenceRelation,
    RepositoryId, RepositoryIdentity, RevisionId, SemanticFingerprint,
};
use sctx_engineering_graph::{
    ArtifactObservation, ArtifactSourceState, EngineeringProjectionStore,
    EngineeringReferenceResolver, MatchBasis, ProjectedEngineeringReference, RepositoryScanOutcome,
    RepositorySnapshot, SnapshotArtifact, SnapshotSourcePolicy, SourceLanguage,
};
use tempfile::TempDir;

fn repository(name: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        repository_id: RepositoryId::new(),
        canonical_name: name.to_owned(),
        semantic_fingerprint: SemanticFingerprint::new(format!("repo:{name}")).unwrap(),
    }
}

fn logical_key(
    repository_id: RepositoryId,
    kind: ArtifactKind,
    namespace: &str,
    name: &str,
) -> ArtifactKey {
    ArtifactKey::derive(
        repository_id,
        kind,
        ArtifactKeyBasis::Logical {
            namespace: Some(namespace.to_owned()),
            logical_name: name.to_owned(),
        },
    )
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn artifact(
    repository: &RepositoryIdentity,
    kind: ArtifactKind,
    namespace: &str,
    name: &str,
    path: &str,
    module: Option<&str>,
    content: Option<&str>,
    semantic: Option<&str>,
) -> SnapshotArtifact {
    let artifact = EngineeringArtifact {
        repository: repository.clone(),
        artifact_key: logical_key(repository.repository_id, kind, namespace, name),
        display_name: name.to_owned(),
        locator_hints: LocatorHints {
            module: module.map(ToOwned::to_owned),
            path: Some(path.to_owned()),
            symbol: matches!(kind, ArtifactKind::Symbol | ArtifactKind::Test)
                .then(|| name.to_owned()),
            language: Some("fixture".to_owned()),
            api_or_schema: matches!(kind, ArtifactKind::Api | ArtifactKind::Schema)
                .then(|| name.to_owned()),
            ..LocatorHints::default()
        },
        content_fingerprint: content.map(|value| ContentFingerprint::new(value).unwrap()),
        semantic_fingerprint: semantic.map(|value| SemanticFingerprint::new(value).unwrap()),
    };
    artifact.validate().unwrap();
    SnapshotArtifact {
        artifact,
        snapshot_generation: "snap_fixture".to_owned(),
        source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
        observations: vec![ArtifactObservation {
            path: path.to_owned(),
            line: Some(1),
            language: SourceLanguage::Rust,
            source_state: ArtifactSourceState::TrackedHead,
        }],
    }
}

fn snapshot(
    repository: &RepositoryIdentity,
    generation: &str,
    mut artifacts: Vec<SnapshotArtifact>,
) -> RepositoryScanOutcome {
    artifacts.sort_by(|left, right| {
        left.artifact
            .artifact_key
            .digest()
            .cmp(right.artifact.artifact_key.digest())
    });
    for artifact in &mut artifacts {
        generation.clone_into(&mut artifact.snapshot_generation);
    }
    RepositoryScanOutcome::Available(RepositorySnapshot {
        repository_id: repository.repository_id,
        source_policy: SnapshotSourcePolicy::TrackedHeadWithSafeTrackedModifications,
        policy_version: "tracked-head-plus-safe-tracked-modifications-v1",
        head_tree_oid: format!("tree-{generation}"),
        generation: generation.to_owned(),
        artifacts,
        scanned_files: 1,
        scanned_bytes: 1,
        skipped_files: vec![],
    })
}

fn reference(
    repository_id: RepositoryId,
    kind: ArtifactKind,
    relation: ReferenceRelation,
    locator: LocatorHints,
    content: Option<&str>,
    semantic: Option<&str>,
) -> ProjectedEngineeringReference {
    let reference = EngineeringReference {
        reference_id: ReferenceId::new(),
        repository_id,
        artifact_kind: kind,
        relation,
        locator_hints: Some(locator),
        content_fingerprint: content.map(|value| ContentFingerprint::new(value).unwrap()),
        semantic_fingerprint: semantic.map(|value| SemanticFingerprint::new(value).unwrap()),
        supports: "resolver fixture observation".to_owned(),
        limitations: vec!["lightweight fixture".to_owned()],
    };
    reference.validate().unwrap();
    ProjectedEngineeringReference {
        context_id: ContextId::new(),
        revision_id: RevisionId::new(),
        reference,
    }
}

#[test]
fn matching_precedence_prefers_stable_logical_keys_over_fingerprints() {
    let repository = repository("precedence");
    let logical = artifact(
        &repository,
        ArtifactKind::Api,
        "api",
        "/search/v2",
        "server/api.rs",
        None,
        None,
        Some("semantic:logical"),
    );
    let fingerprint_only = artifact(
        &repository,
        ArtifactKind::Api,
        "api",
        "/different",
        "server/other.rs",
        None,
        None,
        Some("semantic:shared"),
    );
    let projected = reference(
        repository.repository_id,
        ArtifactKind::Api,
        ReferenceRelation::Consumes,
        LocatorHints {
            api_or_schema: Some("/search/v2".to_owned()),
            path: Some("server/other.rs".to_owned()),
            ..LocatorHints::default()
        },
        None,
        Some("semantic:shared"),
    );
    let projection = EngineeringReferenceResolver
        .resolve(
            &[projected],
            &[snapshot(
                &repository,
                "snap-1",
                vec![logical.clone(), fingerprint_only],
            )],
            None,
        )
        .unwrap();
    let resolved = &projection.references[0];

    assert_eq!(
        resolved.resolution.status,
        sctx_domain::ResolutionStatus::Resolved
    );
    assert_eq!(
        resolved.resolution.resolved_artifact,
        Some(logical.artifact.artifact_key)
    );
    assert_eq!(
        resolved.evidence[0].basis,
        MatchBasis::StableApiSchemaLogicalKey
    );
}

#[test]
fn file_move_and_symbol_rename_re_resolve_through_fingerprints() {
    let repository = repository("moves");
    let moved_file = artifact(
        &repository,
        ArtifactKind::File,
        "file",
        "new/search.rs",
        "new/search.rs",
        None,
        Some("content:file"),
        Some("semantic:file"),
    );
    let renamed_symbol = artifact(
        &repository,
        ArtifactKind::Symbol,
        "src",
        "new_name",
        "src/search.rs",
        Some("src"),
        Some("content:new"),
        Some("semantic:body"),
    );
    let file_reference = reference(
        repository.repository_id,
        ArtifactKind::File,
        ReferenceRelation::Implements,
        LocatorHints {
            path: Some("old/search.rs".to_owned()),
            ..LocatorHints::default()
        },
        Some("content:file"),
        None,
    );
    let symbol_reference = reference(
        repository.repository_id,
        ArtifactKind::Symbol,
        ReferenceRelation::Implements,
        LocatorHints {
            module: Some("src".to_owned()),
            path: Some("src/search.rs".to_owned()),
            symbol: Some("old_name".to_owned()),
            ..LocatorHints::default()
        },
        None,
        Some("semantic:body"),
    );
    let file_reference_id = file_reference.reference.reference_id;
    let symbol_reference_id = symbol_reference.reference.reference_id;
    let projection = EngineeringReferenceResolver
        .resolve(
            &[file_reference, symbol_reference],
            &[snapshot(
                &repository,
                "snap-moved",
                vec![moved_file.clone(), renamed_symbol.clone()],
            )],
            None,
        )
        .unwrap();

    let file = projection
        .references
        .iter()
        .find(|reference| reference.reference_id == file_reference_id)
        .unwrap();
    let symbol = projection
        .references
        .iter()
        .find(|reference| reference.reference_id == symbol_reference_id)
        .unwrap();
    assert_eq!(file.evidence[0].basis, MatchBasis::ContentFingerprint);
    assert_eq!(symbol.evidence[0].basis, MatchBasis::SemanticFingerprint);
    assert_eq!(
        symbol.resolution.resolved_artifact,
        Some(renamed_symbol.artifact.artifact_key)
    );
}

#[test]
fn equal_semantic_matches_remain_ambiguous_even_when_one_path_matches() {
    let repository = repository("ambiguous");
    let first = artifact(
        &repository,
        ArtifactKind::Symbol,
        "one",
        "CandidateA",
        "src/exact.rs",
        Some("one"),
        None,
        Some("semantic:equal"),
    );
    let second = artifact(
        &repository,
        ArtifactKind::Symbol,
        "two",
        "CandidateB",
        "src/other.rs",
        Some("two"),
        None,
        Some("semantic:equal"),
    );
    let projected = reference(
        repository.repository_id,
        ArtifactKind::Symbol,
        ReferenceRelation::Implements,
        LocatorHints {
            path: Some("src/exact.rs".to_owned()),
            symbol: Some("old-unqualified".to_owned()),
            ..LocatorHints::default()
        },
        None,
        Some("semantic:equal"),
    );
    let projection = EngineeringReferenceResolver
        .resolve(
            &[projected],
            &[snapshot(&repository, "snap-ambiguous", vec![second, first])],
            None,
        )
        .unwrap();
    let resolved = &projection.references[0];

    assert_eq!(
        resolved.resolution.status,
        sctx_domain::ResolutionStatus::Ambiguous
    );
    assert!(resolved.association.is_none());
    assert_eq!(resolved.evidence.len(), 2);
    assert!(
        resolved
            .evidence
            .iter()
            .all(|evidence| evidence.basis == MatchBasis::SemanticFingerprint)
    );
    assert!(
        resolved
            .resolution
            .candidates
            .windows(2)
            .all(|pair| { pair[0].digest() < pair[1].digest() })
    );
}

#[test]
fn deletion_becomes_stale_and_unavailable_repository_recovers() {
    let repository = repository("lifecycle");
    let current = artifact(
        &repository,
        ArtifactKind::File,
        "file",
        "src/lib.rs",
        "src/lib.rs",
        None,
        Some("content:stable"),
        None,
    );
    let projected = reference(
        repository.repository_id,
        ArtifactKind::File,
        ReferenceRelation::Implements,
        LocatorHints {
            path: Some("src/lib.rs".to_owned()),
            ..LocatorHints::default()
        },
        Some("content:stable"),
        None,
    );
    let resolver = EngineeringReferenceResolver;
    let initial = resolver
        .resolve(
            std::slice::from_ref(&projected),
            &[snapshot(&repository, "snap-present", vec![current.clone()])],
            None,
        )
        .unwrap();
    let stale = resolver
        .resolve_incremental(
            &initial,
            std::slice::from_ref(&projected),
            &[snapshot(&repository, "snap-deleted", vec![])],
        )
        .unwrap();
    assert_eq!(
        stale.references[0].resolution.status,
        sctx_domain::ResolutionStatus::Stale
    );

    let unavailable = resolver
        .resolve(
            std::slice::from_ref(&projected),
            &[RepositoryScanOutcome::Unavailable {
                repository_id: repository.repository_id,
                reason: "checkout missing".to_owned(),
            }],
            Some(&stale),
        )
        .unwrap();
    assert_eq!(
        unavailable.references[0].resolution.status,
        sctx_domain::ResolutionStatus::Unavailable
    );
    assert_eq!(
        unavailable.references[0].evidence[0].basis,
        MatchBasis::RepositoryUnavailable
    );
    let recovered = resolver
        .resolve(
            &[projected],
            &[snapshot(&repository, "snap-recovered", vec![current])],
            Some(&unavailable),
        )
        .unwrap();
    assert_eq!(
        recovered.references[0].resolution.status,
        sctx_domain::ResolutionStatus::Resolved
    );
}

#[test]
fn cross_language_api_and_schema_keys_resolve_logically() {
    let repository = repository("contracts");
    let api = artifact(
        &repository,
        ArtifactKind::Api,
        "api",
        "/search/v2",
        "web/search.ts",
        None,
        None,
        None,
    );
    let schema = artifact(
        &repository,
        ArtifactKind::Schema,
        "schema",
        "SearchResponse",
        "proto/search.proto",
        None,
        None,
        None,
    );
    let references = [
        reference(
            repository.repository_id,
            ArtifactKind::Api,
            ReferenceRelation::Consumes,
            LocatorHints {
                api_or_schema: Some("/search/v2".to_owned()),
                language: Some("swift".to_owned()),
                ..LocatorHints::default()
            },
            None,
            None,
        ),
        reference(
            repository.repository_id,
            ArtifactKind::Schema,
            ReferenceRelation::Consumes,
            LocatorHints {
                api_or_schema: Some("SearchResponse".to_owned()),
                language: Some("kotlin".to_owned()),
                ..LocatorHints::default()
            },
            None,
            None,
        ),
    ];
    let projection = EngineeringReferenceResolver
        .resolve(
            &references,
            &[snapshot(&repository, "snap-contracts", vec![api, schema])],
            None,
        )
        .unwrap();
    assert!(projection.references.iter().all(|reference| {
        reference.resolution.status == sctx_domain::ResolutionStatus::Resolved
            && reference.evidence[0].basis == MatchBasis::StableApiSchemaLogicalKey
    }));
}

#[test]
fn incremental_resolution_equals_scratch_and_projection_rebuild_is_byte_equivalent() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("shared-context");
    let repository = repository("projection");
    let current = artifact(
        &repository,
        ArtifactKind::File,
        "file",
        "src/lib.rs",
        "src/lib.rs",
        None,
        Some("content:projection"),
        None,
    );
    let projected = reference(
        repository.repository_id,
        ArtifactKind::File,
        ReferenceRelation::Implements,
        LocatorHints {
            path: Some("old/lib.rs".to_owned()),
            ..LocatorHints::default()
        },
        Some("content:projection"),
        None,
    );
    let snapshots = [snapshot(&repository, "snap-projection", vec![current])];
    let resolver = EngineeringReferenceResolver;
    let scratch = resolver
        .resolve(std::slice::from_ref(&projected), &snapshots, None)
        .unwrap();
    let incremental = resolver
        .resolve_incremental(&scratch, std::slice::from_ref(&projected), &snapshots)
        .unwrap();
    assert_eq!(incremental, scratch);

    let store = EngineeringProjectionStore::initialize(&root).unwrap();
    store.rebuild(&scratch).unwrap();
    let first_bytes = store.canonical_bytes().unwrap().unwrap();
    let database = store.database_path().to_path_buf();
    drop(store);
    fs::remove_file(database).unwrap();
    let rebuilt = EngineeringProjectionStore::initialize(&root).unwrap();
    rebuilt.rebuild(&scratch).unwrap();
    assert_eq!(rebuilt.canonical_bytes().unwrap().unwrap(), first_bytes);
    assert_eq!(rebuilt.read_projection().unwrap().unwrap(), scratch);

    let empty = resolver.resolve(&[], &snapshots, Some(&scratch)).unwrap();
    rebuilt.rebuild_incremental(&empty).unwrap();
    let current = rebuilt.read_projection().unwrap().unwrap();
    assert_eq!(current.artifact_generation, empty.artifact_generation);
    assert!(current.references.is_empty());
    assert!(
        !rebuilt
            .canonical_bytes()
            .unwrap()
            .unwrap()
            .windows(scratch.artifact_generation.len())
            .any(|window| window == scratch.artifact_generation.as_bytes())
    );
}

#[test]
fn unmatched_reference_without_history_is_unresolved() {
    let repository = repository("unresolved");
    let projected = reference(
        repository.repository_id,
        ArtifactKind::File,
        ReferenceRelation::Implements,
        LocatorHints {
            path: Some("missing.rs".to_owned()),
            ..LocatorHints::default()
        },
        None,
        None,
    );
    let projection = EngineeringReferenceResolver
        .resolve(
            &[projected],
            &[snapshot(&repository, "snap-empty", vec![])],
            None,
        )
        .unwrap();
    assert_eq!(
        projection.references[0].resolution.status,
        sctx_domain::ResolutionStatus::Unresolved
    );
    assert!(projection.references[0].association.is_none());
}
