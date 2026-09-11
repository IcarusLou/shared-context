//! Lane A's supply side: the files a Session touched and the Contexts those files anchor.
//!
//! ADR-0007 splits automatic retrieval into two lanes. Lane A is the one with no fuzziness in it
//! at all: the Session opened or rewrote a file, some accepted Context recorded an Engineering
//! Reference to that exact file, and the join of those two facts is the Context's ticket into the
//! Pack. Nothing here ranks, scores, traverses a Relation graph, or compares a vector -- it
//! answers "which knowledge is about the code this Session is standing in" and hands the answer to
//! the assembler with the `repository:path` that justifies it.
//!
//! Three sources contribute anchors, and the first one is the reason this module exists:
//!
//! 1. Every `workspace` and `diff` Task Signal the Session ever recorded, **superseded ones
//!    included**. A superseded Signal leaves *retrieval as a query term* -- that is what the
//!    supersede contract says and this module does not change it -- but Lane A is not querying
//!    with the Signal, it is reading a footprint. The 16-slot active window is a bound on what
//!    the Agent is looking at now, not on where it has been. On the 17-hour Session measured in
//!    Step 0b the active window held 16 files and the full history held 145, so reading only the
//!    window shrinks the true input area by a factor of nine.
//! 2. `working_intent.artifact_hints`, for the spellings that read as a path.
//! 3. The Artifact a `task_artifact_focus` call resolved.
//!
//! The reverse lookup is guarded by [`SAFE_ACCEPTED_CONTEXT_PREDICATE`], the same predicate every
//! other automatic channel uses, which is what keeps a Reference row that is no longer the current
//! accepted knowledge from pulling a retired Context back out of a file.
//!
//! This module also carries the S2-2 non-semantic seed expansion, because it produces the same
//! kind of value -- a seed -- from the same starting point.

// S2-4 wires both halves into `task_context_pack`; until it does, nothing in the crate calls them.
#![allow(dead_code)]

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

use rusqlite::Connection;
use sctx_domain::{
    ContextId, RepoRelativePath, RepositoryId, ResolvedFocus, RevisionId, SpaceId, TaskSignal,
    TaskSignalKind, TaskSignalRecord, WorkingIntentSnapshot, hints,
};
use serde::{Deserialize, Serialize};

use crate::{LocatorPath, Result, SAFE_ACCEPTED_CONTEXT_PREDICATE, from_json, parse_id, sql_error};

/// Where one file anchor came from.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnchorSource {
    /// A `workspace` or `diff` Task Signal of this Session -- active or superseded.
    Signal,
    /// A `working_intent.artifact_hints` spelling that reads as a path.
    ArtifactHint,
    /// The Artifact one `task_artifact_focus` call resolved.
    ArtifactFocus,
}

/// One file this Session demonstrably touched, named.
///
/// `repository_id` is `None` only for an [`AnchorSource::ArtifactHint`]: a hint is free text the
/// Agent wrote into its Intent and carries no Repository coordinate, while a Signal is rendered by
/// the Hook as `<RepositoryId>:<repository-relative path>` and a resolved focus is Repository
/// qualified by construction.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FileAnchor {
    pub repository_id: Option<RepositoryId>,
    pub path: String,
    pub source: AnchorSource,
}

/// How exactly one anchor reached one Engineering Reference.
///
/// The three bases are ordered by how much the match is worth as evidence, which is also the order
/// the assembler should prefer when one Context is reached several ways.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnchorMatchBasis {
    /// Same Repository, same repository-relative path. The only basis a Signal or a resolved
    /// focus can ever produce.
    RepositoryPath,
    /// Same repository-relative path, Repository unknown. Hint-sourced anchors only.
    Path,
    /// Same final path component, Repository and directory unknown. Hint-sourced anchors only,
    /// and the one basis that can name two different files with one spelling.
    Basename,
}

/// One reason a Context is in Lane A's answer, spelled the way a Pack prints a location.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub(crate) struct AnchorEvidence {
    /// `repository:path` of the Engineering Reference that matched -- the same spelling
    /// `load_compact_locations` prints, so a `why` line can render it directly.
    pub location: String,
    pub source: AnchorSource,
    pub basis: AnchorMatchBasis,
}

/// One accepted Context an anchor reached, with every anchor that reached it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaneAHit {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub space_id: SpaceId,
    pub anchors: Vec<AnchorEvidence>,
}

impl LaneAHit {
    /// The `(Context, revision)` pair this hit contributes to the seed set.
    pub fn seed(&self) -> (ContextId, RevisionId) {
        (self.context_id, self.revision_id)
    }
}

impl FileAnchor {
    fn basename(&self) -> &str {
        basename_of(&self.path)
    }

    /// How this anchor matches one Engineering Reference coordinate, or `None` when it does not.
    fn basis_against(&self, repository_id: &str, path: &str) -> Option<AnchorMatchBasis> {
        let Some(repository) = &self.repository_id else {
            if self.path == path {
                return Some(AnchorMatchBasis::Path);
            }
            return (self.basename() == basename_of(path)).then_some(AnchorMatchBasis::Basename);
        };
        (repository.as_str() == repository_id && self.path == path)
            .then_some(AnchorMatchBasis::RepositoryPath)
    }
}

fn basename_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The complete anchor set of one Task, from all three sources, deduplicated and ordered.
///
/// `signals` is the Session's whole Signal history, not its active window: [`TaskSignalRecord`]'s
/// `lifecycle` is deliberately never read here. A caller holding only active Signals may pass
/// them, and will get the smaller footprint that implies.
pub(crate) fn collect_file_anchors(
    signals: &[TaskSignalRecord],
    intent: &WorkingIntentSnapshot,
    resolved_focus: &[ResolvedFocus],
) -> Vec<FileAnchor> {
    let mut anchors = BTreeSet::new();
    for record in signals {
        if let Some(anchor) = signal_anchor(&record.signal) {
            anchors.insert(anchor);
        }
    }
    for hint in &intent.artifact_hints {
        for candidate in hints::scan_text(hint).paths {
            anchors.insert(FileAnchor {
                repository_id: None,
                path: candidate.path,
                source: AnchorSource::ArtifactHint,
            });
        }
    }
    for focus in resolved_focus {
        anchors.insert(FileAnchor {
            repository_id: Some(focus.repository_id.clone()),
            path: focus.locator.path().as_str().to_owned(),
            source: AnchorSource::ArtifactFocus,
        });
    }
    anchors.into_iter().collect()
}

/// The file one `workspace` or `diff` Signal names, or `None` when it names none.
///
/// The Hook renders both kinds as `<RepositoryId>:<repository-relative path>`. Anything else is a
/// bare checkout root, a Workspace root, or Agent-authored prose about what changed; none of those
/// is a coordinate and none of them anchors. `Prompt` and `TestOutcome` never carry one.
fn signal_anchor(signal: &TaskSignal) -> Option<FileAnchor> {
    if !matches!(
        signal.kind,
        TaskSignalKind::Workspace | TaskSignalKind::Diff
    ) {
        return None;
    }
    let (repository_id, path) = signal.content.trim().split_once(':')?;
    let repository_id = repository_id.parse::<RepositoryId>().ok()?;
    let path = RepoRelativePath::new(path.trim()).ok()?;
    Some(FileAnchor {
        repository_id: Some(repository_id),
        path: path.as_str().to_owned(),
        source: AnchorSource::Signal,
    })
}

/// Every accepted, injectable Context at least one anchor reaches through an Engineering
/// Reference.
///
/// The join carries [`SAFE_ACCEPTED_CONTEXT_PREDICATE`], whose `accepted_revision_id =
/// revision.revision_id` clause is what makes this a lookup of *current* knowledge: a Reference
/// left behind on a revision that is no longer the accepted one, and every Reference of a Context
/// that has since been superseded or made ineligible, is invisible here. On the installation Step
/// 0b measured, 6 of 73 Reference rows are excluded by exactly this predicate.
///
/// `artifact_kind` is not filtered: every [`ArtifactLocator`](sctx_domain::ArtifactLocator) variant
/// carries the file its coordinate lives in, and a Session that opened that file touched the
/// Symbol's or the Api's file too. All 73 rows on the measured installation are `file` anyway, so
/// this widens nothing that was measured and loses nothing that was not.
///
/// Results are ordered by how many anchors reached each Context, then by Context identity -- a
/// total order that does not depend on row order in the projection.
///
/// # Errors
///
/// Returns typed storage errors when the projection cannot be read.
pub(crate) fn lane_a_hits(
    connection: &Connection,
    anchors: &[FileAnchor],
) -> Result<Vec<LaneAHit>> {
    if anchors.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(&format!(
            "SELECT reference.repository_id, reference.locator_json,
                    item.context_id, revision.revision_id, item.space_id
             FROM engineering_reference AS reference
             JOIN context_revision AS revision ON revision.revision_id = reference.revision_id
             JOIN context_item AS item ON item.context_id = revision.context_id
             WHERE {SAFE_ACCEPTED_CONTEXT_PREDICATE}
             ORDER BY reference.reference_id"
        ))
        .map_err(sql_error("prepare Lane A anchor reverse lookup"))?;
    let mut rows = statement
        .query([])
        .map_err(sql_error("execute Lane A anchor reverse lookup"))?;
    let mut reached = BTreeMap::<(ContextId, RevisionId, SpaceId), BTreeSet<AnchorEvidence>>::new();
    while let Some(row) = rows
        .next()
        .map_err(sql_error("read Lane A anchor reverse lookup row"))?
    {
        let repository_id: String = row
            .get(0)
            .map_err(sql_error("read Lane A Reference Repository"))?;
        let locator: LocatorPath = from_json(
            &row.get::<_, String>(1)
                .map_err(sql_error("read Lane A Reference locator"))?,
        )?;
        let matched = anchors
            .iter()
            .filter_map(|anchor| {
                anchor
                    .basis_against(&repository_id, &locator.path)
                    .map(|basis| AnchorEvidence {
                        location: format!("{repository_id}:{}", locator.path),
                        source: anchor.source,
                        basis,
                    })
            })
            .collect::<BTreeSet<_>>();
        if matched.is_empty() {
            continue;
        }
        let key = (
            parse_id(
                &row.get::<_, String>(2)
                    .map_err(sql_error("read Lane A Context"))?,
            )?,
            parse_id(
                &row.get::<_, String>(3)
                    .map_err(sql_error("read Lane A revision"))?,
            )?,
            parse_id(
                &row.get::<_, String>(4)
                    .map_err(sql_error("read Lane A Space"))?,
            )?,
        );
        reached.entry(key).or_default().extend(matched);
    }
    let mut hits = reached
        .into_iter()
        .map(|((context_id, revision_id, space_id), anchors)| LaneAHit {
            context_id,
            revision_id,
            space_id,
            anchors: anchors.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    hits.sort_by_key(|hit| (Reverse(hit.anchors.len()), hit.context_id));
    Ok(hits)
}

#[cfg(test)]
mod test_support {
    use sctx_domain::{
        Applicability, ArtifactKind, ArtifactLocator, ContextId, ContextKind, ContextRelation,
        ContextRelationKind, ContextRevisionDraft, EngineeringReferenceDraft,
        EvidenceSnapshotDraft, EvidenceType, IntentSnapshot, PublicationAction, PublicationDraft,
        PublicationId, ReferenceRelation, RepoRelativePath, RevisionId, SpaceId, TaskId,
        TaskSessionId, TaskSignal, TaskSignalKind, TaskSignalLifecycle, TaskSignalRecord,
        WorkingIntentSnapshot,
    };
    use sctx_event_schema::{Event, EventPayload};
    use sctx_git_store::{AppendRequest, GitStore};
    use sctx_index::ProjectionIndex;
    use tempfile::TempDir;

    use super::{FileAnchor, LaneAHit, lane_a_hits};
    use crate::lanes::AnchorSource;

    /// One accepted Context revision, as the fixture knows it.
    #[derive(Clone, Copy, Debug)]
    pub struct Accepted {
        pub context: ContextId,
        pub revision: RevisionId,
        pub publication: PublicationId,
    }

    /// A real projected installation with one Space, built through the Event log exactly the way
    /// the product builds one. Hand-written `SQLite` would let a schema drift past these tests.
    pub struct Corpus {
        _temporary: TempDir,
        store: GitStore,
        pub space_id: SpaceId,
    }

    impl Corpus {
        pub fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let store = GitStore::bootstrap_local(temporary.path().join("installation")).unwrap();
            let event = Event::space_created(
                IntentSnapshot {
                    title: "LaneSpace".to_owned(),
                    problem: "the lane fixture needs one Space to hold its Contexts".to_owned(),
                    desired_outcome: "anchored and zero-anchor knowledge live side by side"
                        .to_owned(),
                    in_scope: vec!["lane retrieval".to_owned()],
                    out_of_scope: vec!["LaneExcluded".to_owned()],
                    acceptance_conditions: vec!["LaneAccepted".to_owned()],
                    domain_terms: vec!["LaneTerm".to_owned()],
                },
                None,
            )
            .unwrap();
            let space_id = match event.payload() {
                EventPayload::SpaceCreated { space_id, .. } => *space_id,
                _ => unreachable!(),
            };
            append(&store, event);
            Self {
                _temporary: temporary,
                store,
                space_id,
            }
        }

        pub fn accept(&self, statement: &str) -> Accepted {
            self.accept_draft(draft(statement, None, None, Vec::new()))
        }

        /// An accepted Context carrying the two non-semantic edge fields.
        pub fn accept_grouped(
            &self,
            statement: &str,
            problem_view: Option<&str>,
            topic_key: Option<&str>,
        ) -> Accepted {
            self.accept_draft(draft(statement, problem_view, topic_key, Vec::new()))
        }

        /// A new accepted Context whose `Supersedes` Relation retires `target`.
        pub fn supersede(&self, target: ContextId, statement: &str) -> Accepted {
            self.accept_draft(draft(
                statement,
                None,
                None,
                vec![ContextRelation {
                    target_context_id: target,
                    kind: ContextRelationKind::Supersedes,
                    rationale: "the current reading replaces the retired one".to_owned(),
                    supports: vec!["the retired reading no longer describes the code".to_owned()],
                }],
            ))
        }

        /// A successor revision of `previous`, published so `previous` stops being accepted.
        pub fn revise(&self, previous: Accepted, statement: &str) -> Accepted {
            let event = Event::context_revised(
                self.space_id,
                previous.context,
                vec![previous.revision],
                draft(statement, None, None, Vec::new()),
                None,
            )
            .unwrap();
            let revision_id = match event.payload() {
                EventPayload::ContextRevisionAdded { revision, .. } => revision.revision_id,
                _ => unreachable!(),
            };
            append(&self.store, event);
            let publication_id =
                self.publish(previous.context, revision_id, vec![previous.publication]);
            Accepted {
                context: previous.context,
                revision: revision_id,
                publication: publication_id,
            }
        }

        pub fn reference(&self, accepted: Accepted, repository_id: &str, path: &str) {
            append(
                &self.store,
                Event::engineering_reference_recorded(
                    accepted.context,
                    accepted.revision,
                    EngineeringReferenceDraft {
                        repository_id: repository_id.parse().unwrap(),
                        artifact_kind: ArtifactKind::File,
                        relation: ReferenceRelation::Implements,
                        locator: ArtifactLocator::File {
                            path: RepoRelativePath::new(path).unwrap(),
                        },
                        supports: "the lane fixture anchors this Context to this file".to_owned(),
                        limitations: vec!["synthetic fixture".to_owned()],
                    },
                    None,
                )
                .unwrap(),
            );
        }

        pub fn hits(&self, anchors: &[FileAnchor]) -> Vec<LaneAHit> {
            self.read(|connection| lane_a_hits(connection, anchors))
        }

        pub fn read<T>(&self, query: impl FnOnce(&rusqlite::Connection) -> crate::Result<T>) -> T {
            ProjectionIndex::for_store(&self.store)
                .query_snapshot(query)
                .unwrap()
                .data
        }

        fn accept_draft(&self, draft: ContextRevisionDraft) -> Accepted {
            let event = Event::context_revision_added(self.space_id, draft, None).unwrap();
            let (context_id, revision_id) = match event.payload() {
                EventPayload::ContextRevisionAdded {
                    context_id,
                    revision,
                    ..
                } => (*context_id, revision.revision_id),
                _ => unreachable!(),
            };
            append(&self.store, event);
            let publication_id = self.publish(context_id, revision_id, Vec::new());
            Accepted {
                context: context_id,
                revision: revision_id,
                publication: publication_id,
            }
        }

        fn publish(
            &self,
            context_id: ContextId,
            revision_id: RevisionId,
            previous_publication_ids: Vec<PublicationId>,
        ) -> PublicationId {
            let event = Event::publication_changed(
                self.space_id,
                context_id,
                PublicationDraft {
                    previous_publication_ids,
                    action: PublicationAction::Publish,
                    revision_id,
                    review_event_ids: Vec::new(),
                },
                None,
            )
            .unwrap();
            let publication_id = match event.payload() {
                EventPayload::ContextPublicationChanged { publication, .. } => {
                    publication.publication_id
                }
                _ => unreachable!(),
            };
            append(&self.store, event);
            publication_id
        }
    }

    fn append(store: &GitStore, event: Event) {
        store
            .append_event(AppendRequest::event(event))
            .expect("append lane fixture Event");
    }

    fn draft(
        statement: &str,
        problem_view: Option<&str>,
        topic_key: Option<&str>,
        relations: Vec<ContextRelation>,
    ) -> ContextRevisionDraft {
        ContextRevisionDraft {
            kind: ContextKind::Contract,
            problem_view: problem_view.map(ToOwned::to_owned),
            hints: Vec::new(),
            topic_key: topic_key.map(ToOwned::to_owned),
            statement: statement.to_owned(),
            rationale: "the lane fixture captures durable engineering behavior".to_owned(),
            applicability: Applicability {
                domains: vec!["lanedomain".to_owned()],
                platforms: vec!["laneplatform".to_owned()],
                conditions: vec!["lanecondition".to_owned()],
            },
            assumptions: vec!["fixture inputs remain stable".to_owned()],
            recheck_when: vec!["the fixture contract changes".to_owned()],
            relations,
            evidence: vec![EvidenceSnapshotDraft {
                kind: EvidenceType::ExperimentRecord,
                supports: "the lane fixture is safe for retrieval".to_owned(),
                content: serde_json::json!({
                    "command": "cargo test -p sctx-search lanes",
                    "actual": "passed"
                }),
                interpretation: "the Context has complete local evidence".to_owned(),
                limitations: vec!["synthetic fixture".to_owned()],
            }],
        }
    }

    /// One Repository-qualified Signal anchor, the shape the Hook renders.
    pub fn anchor(repository_id: &str, path: &str) -> FileAnchor {
        FileAnchor {
            repository_id: Some(repository_id.parse().unwrap()),
            path: path.to_owned(),
            source: AnchorSource::Signal,
        }
    }

    /// One Signal record, owned by a single Session, at the given lifecycle.
    pub fn signal(kind: TaskSignalKind, content: &str, active: bool) -> TaskSignalRecord {
        TaskSignalRecord {
            signal_id: sctx_domain::SignalId::new(),
            task_session_id: TaskSessionId::new(),
            task_id: TaskId::new(),
            signal: TaskSignal {
                kind,
                content: content.to_owned(),
            },
            lifecycle: if active {
                TaskSignalLifecycle::Active
            } else {
                TaskSignalLifecycle::Superseded
            },
        }
    }

    /// A Working Intent carrying only the `artifact_hints` under test.
    pub fn intent(artifact_hints: &[&str]) -> WorkingIntentSnapshot {
        WorkingIntentSnapshot {
            goal: "read the lane fixture".to_owned(),
            current_direction: None,
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            domains: Vec::new(),
            platforms: Vec::new(),
            constraints: Vec::new(),
            acceptance_conditions: Vec::new(),
            artifact_hints: artifact_hints
                .iter()
                .map(|hint| (*hint).to_owned())
                .collect(),
            interface_hints: Vec::new(),
            open_questions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Corpus, anchor, intent, signal};
    use super::*;

    #[test]
    fn a_superseded_workspace_signal_anchors_exactly_like_an_active_one() {
        let anchors = collect_file_anchors(
            &[
                signal(TaskSignalKind::Workspace, "Android:app/src/Active.kt", true),
                signal(TaskSignalKind::Workspace, "Android:app/src/Old.kt", false),
                signal(TaskSignalKind::Diff, "Android:app/src/Changed.kt", false),
            ],
            &intent(&[]),
            &[],
        );

        assert_eq!(
            anchors
                .iter()
                .map(|anchor| anchor.path.as_str())
                .collect::<Vec<_>>(),
            ["app/src/Active.kt", "app/src/Changed.kt", "app/src/Old.kt"],
            "Lane A reads a footprint, and a superseded Signal is still a place the Session went"
        );
        assert!(
            anchors
                .iter()
                .all(|anchor| anchor.source == AnchorSource::Signal)
        );
    }

    #[test]
    fn a_signal_that_names_no_repository_qualified_file_anchors_nothing() {
        let anchors = collect_file_anchors(
            &[
                // Prompt and TestOutcome never carry a coordinate.
                signal(TaskSignalKind::Prompt, "Android:app/src/Prompt.kt", true),
                signal(TaskSignalKind::TestOutcome, "test runner failed", true),
                // A bare checkout root says only where the Agent is, never what it opened.
                signal(TaskSignalKind::Workspace, "/Users/dev/checkout", true),
                // An absolute path behind a Repository prefix is not repository relative.
                signal(TaskSignalKind::Workspace, "Android:/etc/passwd", true),
                // Escapes and Windows separators are refused by RepoRelativePath.
                signal(TaskSignalKind::Workspace, "Android:../../secrets", true),
                signal(TaskSignalKind::Workspace, "C:\\Users\\dev\\a.kt", true),
                // A Repository identity may not contain a path separator.
                signal(TaskSignalKind::Workspace, "a/b:app/src/A.kt", true),
            ],
            &intent(&[]),
            &[],
        );

        assert!(anchors.is_empty(), "{anchors:#?}");
    }

    #[test]
    fn an_artifact_hint_anchors_only_the_spellings_that_read_as_a_path() {
        let anchors = collect_file_anchors(
            &[],
            &intent(&[
                "file:search.ts",
                "symbol:SearchResult",
                "components/business/poi/Map.kt",
            ]),
            &[],
        );

        assert_eq!(
            anchors
                .iter()
                .map(|anchor| (anchor.repository_id.clone(), anchor.path.clone()))
                .collect::<Vec<_>>(),
            [
                (None, "components/business/poi/Map.kt".to_owned()),
                (None, "search.ts".to_owned()),
            ],
            "a symbol spelling names no file, and a hint never carries a Repository"
        );
    }

    #[test]
    fn a_resolved_artifact_focus_anchors_the_file_its_locator_lives_in() {
        let focus = ResolvedFocus {
            repository_id: "Android".parse().unwrap(),
            locator: sctx_domain::ArtifactLocator::Symbol {
                path: RepoRelativePath::new("app/src/MapScene.kt").unwrap(),
                language: "kotlin".to_owned(),
                module: "poi".to_owned(),
                enclosing_type: None,
                symbol_name: "measure".to_owned(),
                signature: "fun measure()".to_owned(),
            },
        };

        let anchors = collect_file_anchors(&[], &intent(&[]), &[focus]);

        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].path, "app/src/MapScene.kt");
        assert_eq!(anchors[0].source, AnchorSource::ArtifactFocus);
        assert_eq!(
            anchors[0].repository_id.as_ref().map(RepositoryId::as_str),
            Some("Android")
        );
    }

    #[test]
    fn a_lane_a_hit_carries_the_location_that_justifies_it() {
        let corpus = Corpus::new();
        let anchored = corpus.accept("the map scene measures its viewport before deriving zoom");
        corpus.reference(anchored, "Android", "app/src/MapScene.kt");
        let elsewhere = corpus.accept("the live tag reads its text from the server");
        corpus.reference(elsewhere, "Android", "app/src/LiveTag.kt");

        let hits = corpus.hits(&[anchor("Android", "app/src/MapScene.kt")]);

        assert_eq!(
            hits.iter().map(|hit| hit.context_id).collect::<Vec<_>>(),
            [anchored.context],
            "only the file the Session touched anchors"
        );
        assert_eq!(
            hits[0].anchors,
            [AnchorEvidence {
                location: "Android:app/src/MapScene.kt".to_owned(),
                source: AnchorSource::Signal,
                basis: AnchorMatchBasis::RepositoryPath,
            }]
        );
        assert_eq!(hits[0].revision_id, anchored.revision);
        assert_eq!(hits[0].space_id, corpus.space_id);
    }

    #[test]
    fn a_reference_left_on_a_revision_that_is_no_longer_accepted_never_anchors() {
        let corpus = Corpus::new();
        let first = corpus.accept("the first reading of the map zoom rule");
        corpus.reference(first, "Android", "app/src/Retired.kt");
        let second = corpus.revise(first, "the corrected reading of the map zoom rule");
        corpus.reference(second, "Android", "app/src/Current.kt");

        assert!(
            corpus
                .hits(&[anchor("Android", "app/src/Retired.kt")])
                .is_empty(),
            "a file the retired revision pointed at must not reach the Context it no longer \
             describes"
        );
        assert_eq!(
            corpus
                .hits(&[anchor("Android", "app/src/Current.kt")])
                .iter()
                .map(|hit| (hit.context_id, hit.revision_id))
                .collect::<Vec<_>>(),
            [(second.context, second.revision)],
            "the accepted revision's own Reference still anchors"
        );
    }

    /// The six Reference rows Step 0b found outside the injectable set all belong to one Context
    /// that a later Context supersedes, and every one of them points at a file that is still very
    /// much part of the codebase. Reaching a superseded Context from a file the Session opened is
    /// exactly the failure mode this filter exists to prevent.
    #[test]
    fn the_references_of_a_superseded_context_never_anchor() {
        let corpus = Corpus::new();
        let retired = corpus.accept("the live struct carries the tag text under search_extra");
        corpus.reference(retired, "Android", "app/src/SearchExtraStruct.kt");
        let current = corpus.supersede(
            retired.context,
            "the live struct carries the tag text under new_live_room",
        );
        corpus.reference(current, "Android", "app/src/NewLiveRoomStruct.kt");

        assert!(
            corpus
                .hits(&[anchor("Android", "app/src/SearchExtraStruct.kt")])
                .is_empty(),
            "a file still in the tree must not pull a superseded Context back out of it"
        );
        assert_eq!(
            corpus
                .hits(&[anchor("Android", "app/src/NewLiveRoomStruct.kt")])
                .iter()
                .map(|hit| hit.context_id)
                .collect::<Vec<_>>(),
            [current.context]
        );
    }

    #[test]
    fn a_hint_without_a_directory_reaches_the_reference_by_its_final_component() {
        let corpus = Corpus::new();
        let accepted = corpus.accept("the search result card renders its default value");
        corpus.reference(accepted, "Web", "src/search/result.ts");

        let hits = corpus.hits(&collect_file_anchors(
            &[],
            &intent(&["file:result.ts"]),
            &[],
        ));

        assert_eq!(
            hits.iter()
                .map(|hit| hit.anchors.clone())
                .collect::<Vec<_>>(),
            [vec![AnchorEvidence {
                location: "Web:src/search/result.ts".to_owned(),
                source: AnchorSource::ArtifactHint,
                basis: AnchorMatchBasis::Basename,
            }]],
            "a hint carries no Repository and no directory, and the basis says so"
        );
    }

    #[test]
    fn the_most_anchored_context_is_first_and_the_order_never_depends_on_row_order() {
        let corpus = Corpus::new();
        let one = corpus.accept("one file backs this reading of the viewport");
        corpus.reference(one, "Android", "app/src/Alpha.kt");
        let two = corpus.accept("two files back this reading of the viewport");
        corpus.reference(two, "Android", "app/src/Beta.kt");
        corpus.reference(two, "Android", "app/src/Gamma.kt");

        let hits = corpus.hits(&[
            anchor("Android", "app/src/Alpha.kt"),
            anchor("Android", "app/src/Beta.kt"),
            anchor("Android", "app/src/Gamma.kt"),
        ]);

        assert_eq!(
            hits.iter()
                .map(|hit| (hit.context_id, hit.anchors.len()))
                .collect::<Vec<_>>(),
            [(two.context, 2), (one.context, 1)]
        );
    }
}
