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
//!
//! And it carries Lane B's second hop ([`lane_b_hits`]), which is where the fuzziness lives and
//! where it is bounded. Lane A produces seeds; the second hop takes each seed's cached document
//! vector, compares it against every other accepted Context's, and *admits* the ones above
//! [`crate::SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`] -- no query text, no encode, no second
//! hop off the Contexts it just admitted. The two lanes live in one file because Lane B's input is
//! Lane A's output and neither is meaningful without the other.

// S2-4 wires all three into `task_context_pack`; until it does, nothing in the crate calls them.
#![allow(dead_code)]

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

use rusqlite::{Connection, params_from_iter};
use sctx_domain::{
    ContextId, RepoRelativePath, RepositoryId, ResolvedFocus, RevisionId, SpaceId, TaskSignal,
    TaskSignalKind, TaskSignalRecord, WorkingIntentSnapshot, hints,
};
use serde::{Deserialize, Serialize};

use crate::{
    LocatorPath, Result, SAFE_ACCEPTED_CONTEXT_PREDICATE,
    embedding::{Hop2AdmissionSample, cosine_similarity, similarity_basis_points},
    from_json, parse_id, sql_error,
};

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

/// Which retrieval field carried a seed to a Context no file anchors.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NonSemanticEdgeKind {
    /// Both Contexts restate the same problem. On the measured installation the map Session's
    /// three Contexts share one `problem_view` and only one of them is file anchored.
    ProblemView,
    /// Both Contexts carry the same non-empty `topic_key`.
    TopicKey,
}

impl NonSemanticEdgeKind {
    /// The `context_revision` column this edge compares.
    const fn column(self) -> &'static str {
        match self {
            Self::ProblemView => "problem_view",
            Self::TopicKey => "topic_key",
        }
    }
}

/// One zero-cost edge that carried a seed one hop.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub(crate) struct NonSemanticEdge {
    pub kind: NonSemanticEdgeKind,
    /// The shared field value, as written. A `why` line renders it truncated.
    pub value: String,
    /// The seed this edge started from.
    pub from_context_id: ContextId,
}

/// One Context the expansion added to the seed set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpandedSeed {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub space_id: SpaceId,
    /// Every edge that reached it, ordered. Never empty.
    pub edges: Vec<NonSemanticEdge>,
}

impl ExpandedSeed {
    /// The `(Context, revision)` pair this seed contributes to the seed set.
    pub fn seed(&self) -> (ContextId, RevisionId) {
        (self.context_id, self.revision_id)
    }
}

/// Adds every accepted Context that shares a `problem_view` or a non-empty `topic_key` with one of
/// `seeds`.
///
/// Step 0b measured 10 of 26 injectable Contexts on a real installation with no Engineering
/// Reference at all -- 38%, and they are exactly the cross-cutting kinds: a contrast-ratio finding,
/// a module-level test-availability discovery, a downgrade risk. No file can reach them, and
/// ADR-0007 names that a signal problem rather than a threshold problem. These two fields are the
/// signal the corpus already carries: two Contexts written against the same restated problem, or
/// filed under the same topic, are related by authorship rather than by a cosine.
///
/// **The expansion is one hop and stops.** The edges of the Contexts it returns are not followed,
/// because a repeated hop over a field this coarse walks the whole corpus in two or three steps.
/// Callers use the result as a seed for Lane B's admission, not as a second round of input here.
///
/// An empty seed set expands to nothing: these edges carry a seed, they do not create one. A
/// Session Lane A could not anchor at all therefore stays empty, which is what the 17-hour Session
/// of Step 0b actually does.
///
/// # Errors
///
/// Returns typed storage errors when the projection cannot be read.
pub(crate) fn expand_seeds_once(
    connection: &Connection,
    seeds: &[(ContextId, RevisionId)],
) -> Result<Vec<ExpandedSeed>> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let seeded = seeds
        .iter()
        .map(|(context_id, _)| *context_id)
        .collect::<BTreeSet<_>>();
    let fields = read_seed_edge_fields(connection, seeds)?;
    let mut reached =
        BTreeMap::<(ContextId, RevisionId, SpaceId), BTreeSet<NonSemanticEdge>>::new();
    for kind in [
        NonSemanticEdgeKind::ProblemView,
        NonSemanticEdgeKind::TopicKey,
    ] {
        let wanted = fields
            .iter()
            .filter(|(edge, _)| edge.0 == kind)
            .map(|(edge, sources)| (edge.1.clone(), sources.clone()))
            .collect::<BTreeMap<_, _>>();
        if wanted.is_empty() {
            continue;
        }
        for (context_id, revision_id, space_id, value) in contexts_sharing(
            connection,
            kind,
            &wanted.keys().cloned().collect::<Vec<_>>(),
        )? {
            if seeded.contains(&context_id) {
                continue;
            }
            let Some(sources) = wanted.get(&value) else {
                continue;
            };
            reached
                .entry((context_id, revision_id, space_id))
                .or_default()
                .extend(sources.iter().map(|from_context_id| NonSemanticEdge {
                    kind,
                    value: value.clone(),
                    from_context_id: *from_context_id,
                }));
        }
    }
    Ok(reached
        .into_iter()
        .map(
            |((context_id, revision_id, space_id), edges)| ExpandedSeed {
                context_id,
                revision_id,
                space_id,
                edges: edges.into_iter().collect(),
            },
        )
        .collect())
}

/// The non-empty `problem_view` and `topic_key` values the seeds carry, each mapped to the seeds
/// that carry it.
///
/// A `NULL` or blank field is not an edge: it is the absence of a classification, and treating
/// every unclassified Context as related to every other unclassified Context would make the
/// expansion a corpus dump.
fn read_seed_edge_fields(
    connection: &Connection,
    seeds: &[(ContextId, RevisionId)],
) -> Result<BTreeMap<(NonSemanticEdgeKind, String), BTreeSet<ContextId>>> {
    let revisions = seeds
        .iter()
        .map(|(_, revision_id)| revision_id.to_string())
        .collect::<Vec<_>>();
    let mut statement = connection
        .prepare(&format!(
            "SELECT context_id, COALESCE(problem_view, ''), COALESCE(topic_key, '')
             FROM context_revision
             WHERE revision_id IN ({})",
            placeholders(revisions.len())
        ))
        .map_err(sql_error("prepare seed edge field read"))?;
    let mut rows = statement
        .query(params_from_iter(revisions))
        .map_err(sql_error("execute seed edge field read"))?;
    let mut fields = BTreeMap::<(NonSemanticEdgeKind, String), BTreeSet<ContextId>>::new();
    while let Some(row) = rows.next().map_err(sql_error("read seed edge field row"))? {
        let context_id: ContextId = parse_id(
            &row.get::<_, String>(0)
                .map_err(sql_error("read seed Context"))?,
        )?;
        for (index, kind) in [
            NonSemanticEdgeKind::ProblemView,
            NonSemanticEdgeKind::TopicKey,
        ]
        .into_iter()
        .enumerate()
        {
            let value: String = row
                .get(index + 1)
                .map_err(sql_error("read seed edge field"))?;
            if !value.trim().is_empty() {
                fields.entry((kind, value)).or_default().insert(context_id);
            }
        }
    }
    Ok(fields)
}

/// Every accepted, injectable Context whose edge field equals one of `values`.
fn contexts_sharing(
    connection: &Connection,
    kind: NonSemanticEdgeKind,
    values: &[String],
) -> Result<Vec<(ContextId, RevisionId, SpaceId, String)>> {
    let column = kind.column();
    let mut statement = connection
        .prepare(&format!(
            "SELECT item.context_id, revision.revision_id, item.space_id, revision.{column}
             FROM context_revision AS revision
             JOIN context_item AS item USING(context_id)
             WHERE {SAFE_ACCEPTED_CONTEXT_PREDICATE}
               AND revision.{column} IN ({})
             ORDER BY item.context_id",
            placeholders(values.len())
        ))
        .map_err(sql_error("prepare non-semantic edge lookup"))?;
    let mut rows = statement
        .query(params_from_iter(values.iter()))
        .map_err(sql_error("execute non-semantic edge lookup"))?;
    let mut shared = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(sql_error("read non-semantic edge row"))?
    {
        shared.push((
            parse_id(
                &row.get::<_, String>(0)
                    .map_err(sql_error("read edge Context"))?,
            )?,
            parse_id(
                &row.get::<_, String>(1)
                    .map_err(sql_error("read edge revision"))?,
            )?,
            parse_id(
                &row.get::<_, String>(2)
                    .map_err(sql_error("read edge Space"))?,
            )?,
            row.get::<_, String>(3)
                .map_err(sql_error("read edge field value"))?,
        ));
    }
    Ok(shared)
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(",")
}

/// What one seed decided about one candidate, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SeedMatch {
    pub seed_context_id: ContextId,
    pub seed_revision_id: RevisionId,
    /// Cosine of the two document vectors, in basis points.
    pub score_basis_points: u16,
    /// Identifiers both Contexts spell out, lower-cased and deduplicated.
    ///
    /// Corroboration, never a ticket: a candidate with identifiers in common is admitted by its
    /// score or not at all, and this only decides which of two equally scored candidates prints
    /// first and what the `why` line has to show for itself.
    pub shared_identifiers: Vec<String>,
}

/// One accepted Context the second hop admitted, and the seed that admitted it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaneBHit {
    pub context_id: ContextId,
    pub revision_id: RevisionId,
    pub space_id: SpaceId,
    /// The strongest seed -- the one a `why` line names, with the score that admitted the Context.
    pub seed: SeedMatch,
    /// Every other seed that also admitted it, strongest first. Usually empty.
    ///
    /// The strongest seed is the one to render, but it is not the only true thing about the hit:
    /// a Context three seeds all reach is better evidence than one a single seed reaches, and the
    /// assembler is the layer that gets to decide whether to say so.
    pub also_admitted_by: Vec<SeedMatch>,
}

impl LaneBHit {
    /// The score that admitted this Context: its strongest seed's.
    pub const fn score_basis_points(&self) -> u16 {
        self.seed.score_basis_points
    }

    /// The identifiers the strongest seed shares with it.
    pub fn shared_identifiers(&self) -> &[String] {
        &self.seed.shared_identifiers
    }

    /// Every seed that admitted this Context, strongest first.
    pub fn admitting_seeds(&self) -> impl Iterator<Item = &SeedMatch> {
        std::iter::once(&self.seed).chain(&self.also_admitted_by)
    }
}

/// Everything one pass of the second hop produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Hop2Admission {
    /// Admitted Contexts, by descending score. Never contains a seed.
    pub hits: Vec<LaneBHit>,
    /// Candidates no seed could be compared against, because the corpus backfill has not reached
    /// them or their cached vector belongs to a generation this one cannot be compared with.
    ///
    /// Counted rather than reported as an omission and never waited for: encoding one on demand
    /// would put a model call on the retrieval path, which is the thing ADR-0004's whole design
    /// refuses. A non-zero count with a large corpus means the backfill is still running.
    pub skipped_missing_vector: usize,
    /// Seeds that carried no usable vector, which is the same fact seen from the other side.
    pub seeds_missing_vector: usize,
    /// One row per pair judged, refusals included, in the order they were judged.
    ///
    /// ADR-0007 pre-registered this: the floor was set from one synthetic corpus cross-checked
    /// against one installation, and it is re-derived from these rows once real traffic has
    /// accumulated. The caller hands them to
    /// [`SemanticVectorCache::record_hop2_admissions`](crate::SemanticVectorCache::record_hop2_admissions);
    /// this function does not write, because it holds a projection connection and the samples
    /// belong in the discardable cache.
    pub samples: Vec<Hop2AdmissionSample>,
}

/// One accepted Context as the second hop sees it: an identity, a vector key, and its prose.
struct Hop2Context {
    context_id: ContextId,
    revision_id: RevisionId,
    space_id: SpaceId,
    /// Identifiers this Context spells out, for the shared-identifier intersection.
    identifiers: BTreeSet<String>,
}

/// Admits the accepted Contexts whose document vector is close enough to a seed's.
///
/// This is Lane B, and it is an admission rather than a ranking. ADR-0007's measurement is the
/// whole reason for the distinction: on a real installation the intent-text path took its top hit
/// from an unrelated topic every single time while *ordering* the corpus essentially perfectly, so
/// a cosine that decides who gets in has to be read against a threshold derived from the highest
/// scoring negative -- which is what [`crate::SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`] is, and what
/// `floor_basis_points` carries here so an operator can move it and a test can pin it.
///
/// Three properties are structural rather than enforced:
///
/// * **No query is encoded.** Both sides of every comparison are document vectors the backfill
///   already wrote, handed in through `vectors`. There is no provider parameter and no text-to-
///   vector call in this function, so the retrieval path cannot acquire a model call by accident --
///   the defect that made the encode budget's mis-calibration invisible for as long as it was.
/// * **It is one hop.** The Contexts admitted here are not re-seeded. `seeds` is what the caller
///   assembled from Lane A and the non-semantic expansion, and the answer is not fed back into it.
/// * **A seed is never its own candidate**, and never another seed's: a seed is already in the
///   Pack by the time this runs, and admitting it a second time would double-count it.
///
/// `vectors` is keyed by revision because that is how [`SemanticVectorCache`](crate::SemanticVectorCache)
/// keys them -- one generation of one model's output, loaded once when the channel is published.
/// A candidate missing from it is skipped and counted, never encoded on demand and never waited on.
///
/// # Errors
///
/// Returns typed storage errors when the projection cannot be read.
pub(crate) fn lane_b_hits(
    connection: &Connection,
    seeds: &[(ContextId, RevisionId)],
    vectors: &BTreeMap<RevisionId, Vec<f32>>,
    floor_basis_points: u16,
) -> Result<Hop2Admission> {
    if seeds.is_empty() {
        return Ok(Hop2Admission::default());
    }
    let seeded = seeds.iter().copied().collect::<BTreeSet<_>>();
    let corpus = read_hop2_corpus(connection)?;
    let (seed_contexts, candidates): (Vec<_>, Vec<_>) = corpus
        .iter()
        .partition(|context| seeded.contains(&(context.context_id, context.revision_id)));

    let mut seeds_missing_vector = 0;
    let scored_seeds = seed_contexts
        .iter()
        .filter(|seed| match vectors.get(&seed.revision_id) {
            Some(vector) if !vector.is_empty() => true,
            _ => {
                seeds_missing_vector += 1;
                false
            }
        })
        .collect::<Vec<_>>();

    let mut admission = Hop2Admission {
        seeds_missing_vector,
        ..Hop2Admission::default()
    };
    for candidate in candidates {
        let Some(candidate_vector) = vectors.get(&candidate.revision_id) else {
            admission.skipped_missing_vector += 1;
            continue;
        };
        let mut matched = Vec::new();
        let mut compared = false;
        for seed in &scored_seeds {
            let seed_vector = &vectors[&seed.revision_id];
            // Two vectors of different length are two generations, not two meanings. The channel
            // drops that pair for the same reason.
            if seed_vector.len() != candidate_vector.len() {
                continue;
            }
            compared = true;
            let score_basis_points =
                similarity_basis_points(cosine_similarity(seed_vector, candidate_vector));
            let admitted = score_basis_points >= floor_basis_points;
            admission.samples.push(Hop2AdmissionSample {
                seed_revision_id: seed.revision_id,
                candidate_revision_id: candidate.revision_id,
                score_basis_points,
                admitted,
            });
            if admitted {
                matched.push(SeedMatch {
                    seed_context_id: seed.context_id,
                    seed_revision_id: seed.revision_id,
                    score_basis_points,
                    shared_identifiers: seed
                        .identifiers
                        .intersection(&candidate.identifiers)
                        .cloned()
                        .collect(),
                });
            }
        }
        if !compared {
            admission.skipped_missing_vector += 1;
            continue;
        }
        // Strongest first, and by seed identity when two seeds reach it equally, so the seed a
        // `why` line names does not depend on the order the projection returned rows in.
        matched.sort_by(|left, right| {
            right
                .score_basis_points
                .cmp(&left.score_basis_points)
                .then_with(|| left.seed_context_id.cmp(&right.seed_context_id))
        });
        let mut matched = matched.into_iter();
        let Some(seed) = matched.next() else {
            continue;
        };
        admission.hits.push(LaneBHit {
            context_id: candidate.context_id,
            revision_id: candidate.revision_id,
            space_id: candidate.space_id,
            seed,
            also_admitted_by: matched.collect(),
        });
    }
    // Score decides the order. Shared identifiers break a tie and nothing more: two Contexts at the
    // same cosine are separated by whether they name the same code, which is the one piece of
    // evidence a cosine does not carry. Context identity closes it so the order is total.
    admission.hits.sort_by(|left, right| {
        right
            .score_basis_points()
            .cmp(&left.score_basis_points())
            .then_with(|| {
                right
                    .shared_identifiers()
                    .len()
                    .cmp(&left.shared_identifiers().len())
            })
            .then_with(|| left.context_id.cmp(&right.context_id))
    });
    Ok(admission)
}

/// Every accepted, injectable Context with the identifiers its prose spells out.
///
/// The three fields read here are the three [`SearchEngine::embeddable_revisions`](crate::SearchEngine::embeddable_revisions)
/// encodes, which is what makes the identifier intersection a statement about the same text the
/// vectors were built from.
fn read_hop2_corpus(connection: &Connection) -> Result<Vec<Hop2Context>> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT item.context_id, revision.revision_id, item.space_id,
                    revision.statement, revision.rationale,
                    COALESCE(revision.problem_view, '')
             FROM context_revision AS revision
             JOIN context_item AS item USING(context_id)
             WHERE {SAFE_ACCEPTED_CONTEXT_PREDICATE}
             ORDER BY item.context_id"
        ))
        .map_err(sql_error("prepare second-hop corpus read"))?;
    let mut rows = statement
        .query([])
        .map_err(sql_error("execute second-hop corpus read"))?;
    let mut corpus = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(sql_error("read second-hop corpus row"))?
    {
        let statement_text: String = row.get(3).map_err(sql_error("read second-hop statement"))?;
        let rationale: String = row.get(4).map_err(sql_error("read second-hop rationale"))?;
        let problem_view: String = row
            .get(5)
            .map_err(sql_error("read second-hop problem view"))?;
        corpus.push(Hop2Context {
            context_id: parse_id(
                &row.get::<_, String>(0)
                    .map_err(sql_error("read second-hop Context"))?,
            )?,
            revision_id: parse_id(
                &row.get::<_, String>(1)
                    .map_err(sql_error("read second-hop revision"))?,
            )?,
            space_id: parse_id(
                &row.get::<_, String>(2)
                    .map_err(sql_error("read second-hop Space"))?,
            )?,
            // The same reading the Candidate analyzer intersects, and the same one the projection
            // derives hint terms with -- identifier syntax, not word overlap. `hints` refuses a
            // run that is merely short and lower case, so a repository nickname like the `ttk` in
            // `x-ttk-map-view` contributes nothing while `LynxMapController` does. That refusal is
            // what keeps this a corroboration and not a second lexical channel.
            identifiers: hints::normalized_identifiers(
                [&statement_text, &rationale, &problem_view].map(String::as_str),
            ),
        });
    }
    Ok(corpus)
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

        pub fn expand(&self, seeds: &[(ContextId, RevisionId)]) -> Vec<super::ExpandedSeed> {
            self.read(|connection| super::expand_seeds_once(connection, seeds))
        }

        /// An accepted Context whose two embedded fields are both under the test's control.
        pub fn accept_document(&self, statement: &str, rationale: &str) -> Accepted {
            let mut draft = draft(statement, None, None, Vec::new());
            draft.rationale = rationale.to_owned();
            self.accept_draft(draft)
        }

        pub fn hop2(
            &self,
            seeds: &[(ContextId, RevisionId)],
            vectors: &std::collections::BTreeMap<RevisionId, Vec<f32>>,
            floor_basis_points: u16,
        ) -> super::Hop2Admission {
            self.read(|connection| {
                super::lane_b_hits(connection, seeds, vectors, floor_basis_points)
            })
        }

        pub fn read<T>(&self, query: impl FnOnce(&rusqlite::Connection) -> crate::Result<T>) -> T {
            ProjectionIndex::for_store(&self.store)
                .query_snapshot(query)
                .unwrap()
                .data
        }

        /// What the corpus backfill would encode, read through the production rule.
        ///
        /// Not the fixture's text read a second time: the second hop compares vectors the backfill
        /// wrote, and the only way a test can claim its vectors are those is to build them from
        /// the same projection query production builds them from.
        pub fn embeddable(&self) -> Vec<(RevisionId, String)> {
            crate::SearchEngine::new(ProjectionIndex::for_store(&self.store))
                .embeddable_revisions()
                .expect("read the embeddable corpus")
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

    /// The map Session's shape, as Step 0b recorded it: three Contexts share one `problem_view`,
    /// one of them (`fb1f06df`) is anchored to `LynxMapController.kt`, and the other two
    /// (`c1ad461d`, `2ea272dd`) carry no Engineering Reference at all. They are 2 of the 10
    /// zero-anchor Contexts in a 26-Context injectable corpus, and no file reaches either of them.
    #[test]
    fn zero_anchor_knowledge_becomes_reachable_through_a_shared_problem_view() {
        const PROBLEM: &str = "接手 x-ttk-map-view Android 改造，按重构方案推进";
        let corpus = Corpus::new();
        let anchored = corpus.accept_grouped(
            "the POI map engine reads its viewport through LynxMapController",
            Some(PROBLEM),
            Some("issue:Android:LynxMapController.kt"),
        );
        corpus.reference(anchored, "Android", "poi/map/LynxMapController.kt");
        let zoom = corpus.accept_grouped(
            "the effective minimum zoom must be bound to the measured viewport",
            Some(PROBLEM),
            None,
        );
        let tests = corpus.accept_grouped(
            "the POI module's standard unit test task is unavailable in this repository",
            Some(PROBLEM),
            None,
        );
        let elsewhere = corpus.accept_grouped(
            "the live tag reads its text from the server",
            Some("a different problem entirely"),
            None,
        );

        let seeds = corpus.hits(&[anchor("Android", "poi/map/LynxMapController.kt")]);
        assert_eq!(
            seeds.iter().map(LaneAHit::seed).collect::<Vec<_>>(),
            [(anchored.context, anchored.revision)],
            "only one of the three is file anchored"
        );

        let expanded = corpus.expand(&seeds.iter().map(LaneAHit::seed).collect::<Vec<_>>());

        assert_eq!(
            expanded
                .iter()
                .map(|seed| seed.context_id)
                .collect::<BTreeSet<_>>(),
            [zoom.context, tests.context].into_iter().collect(),
            "the zero-anchor siblings go from 0 seeds to 2; {elsewhere:?} stays out"
        );
        assert!(
            expanded.iter().all(|seed| seed.edges
                == [NonSemanticEdge {
                    kind: NonSemanticEdgeKind::ProblemView,
                    value: PROBLEM.to_owned(),
                    from_context_id: anchored.context,
                }]),
            "{expanded:#?}"
        );
    }

    #[test]
    fn a_shared_non_empty_topic_key_is_an_edge_and_an_absent_one_is_not() {
        const TOPIC: &str = "risk:Android:search_live_badge.xml";
        let corpus = Corpus::new();
        let anchored = corpus.accept_grouped(
            "the badge gradient needs its own start and end colour resources",
            None,
            Some(TOPIC),
        );
        corpus.reference(anchored, "Android", "res/layout/search_live_badge.xml");
        let sibling = corpus.accept_grouped(
            "the white label text against the new gradient is a contrast-ratio risk",
            None,
            Some(TOPIC),
        );
        let unclassified_seed = corpus.accept("an unrelated reading with no topic and no problem");
        corpus.reference(unclassified_seed, "Android", "res/layout/other.xml");
        let unclassified_other = corpus.accept("another reading with no topic and no problem");

        let seeds = corpus.hits(&[
            anchor("Android", "res/layout/search_live_badge.xml"),
            anchor("Android", "res/layout/other.xml"),
        ]);
        let expanded = corpus.expand(&seeds.iter().map(LaneAHit::seed).collect::<Vec<_>>());

        assert_eq!(
            expanded
                .iter()
                .map(|seed| (seed.context_id, seed.edges.clone()))
                .collect::<Vec<_>>(),
            [(
                sibling.context,
                vec![NonSemanticEdge {
                    kind: NonSemanticEdgeKind::TopicKey,
                    value: TOPIC.to_owned(),
                    from_context_id: anchored.context,
                }]
            )],
            "two Contexts with no classification at all are not related by having none: \
             {unclassified_other:?} must stay out"
        );
    }

    /// One hop, and then it stops. `problem_view` and `topic_key` are coarse enough that a second
    /// hop would walk a whole installation in a step or two.
    #[test]
    fn the_expansion_never_follows_the_edges_of_what_it_just_reached() {
        let corpus = Corpus::new();
        let seed = corpus.accept_grouped("the anchored reading", Some("shared problem"), None);
        corpus.reference(seed, "Android", "app/src/Seed.kt");
        let one_hop = corpus.accept_grouped(
            "one hop away, by problem view",
            Some("shared problem"),
            Some("shared topic"),
        );
        let two_hops =
            corpus.accept_grouped("two hops away, by topic key", None, Some("shared topic"));

        let seeds = corpus.hits(&[anchor("Android", "app/src/Seed.kt")]);
        let expanded = corpus.expand(&seeds.iter().map(LaneAHit::seed).collect::<Vec<_>>());

        assert_eq!(
            expanded
                .iter()
                .map(|seed| seed.context_id)
                .collect::<Vec<_>>(),
            [one_hop.context],
            "the second hop is reachable and deliberately not taken: {two_hops:?}"
        );
        let twice = seeds
            .iter()
            .map(LaneAHit::seed)
            .chain(expanded.iter().map(ExpandedSeed::seed))
            .collect::<Vec<_>>();
        assert_eq!(
            corpus
                .expand(&twice)
                .iter()
                .map(|seed| seed.context_id)
                .collect::<Vec<_>>(),
            [two_hops.context],
            "a caller that chose to hop again would reach it; this function never does"
        );
    }

    /// The 17-hour Session of Step 0b, in shape: 145 distinct files touched across one Repository,
    /// 129 of them recorded by Signals that have since been superseded, and not one of them is a
    /// file any accepted Context references. The full-history anchor set is nine times the active
    /// window and still reaches nothing, and because these edges carry a seed rather than create
    /// one, the non-semantic expansion of an empty seed set is empty too.
    ///
    /// This is the measurement, not a target: Step 0b's `01a08baf` row reads `145 touched, 0 hit,
    /// 0 path-A Contexts`, and its touched paths share no basename at all with the 56 anchored
    /// coordinates the corpus offers. S2-3's second hop cannot help either, for the same reason --
    /// it queries *from* a seed. A Session whose work is disjoint from every recorded Context gets
    /// an empty Pack, and ADR-0007 makes an empty Pack a first-class outcome.
    #[test]
    fn the_seventeen_hour_session_shape_anchors_nothing_and_the_edges_carry_nothing() {
        let corpus = Corpus::new();
        let accepted = corpus.accept_grouped(
            "the POI map engine reads its viewport through LynxMapController",
            Some("接手 x-ttk-map-view Android 改造"),
            None,
        );
        // The one file the corpus anchors is a sibling of the ones the Session opened, and the
        // Session never opened it.
        corpus.reference(
            accepted,
            "Android",
            "components/business/poi/poi/src/main/java/com/ss/android/ugc/aweme/poi/map/lynxmap/\
             engine/LynxMapController.kt",
        );

        let signals = (0..145)
            .map(|index| {
                signal(
                    TaskSignalKind::Workspace,
                    &format!(
                        "Android:components/business/poi/poi/src/main/java/com/ss/android/ugc/\
                         aweme/poi/map/lynxmap/engine/Touched{index:03}.kt"
                    ),
                    index >= 129,
                )
            })
            .collect::<Vec<_>>();
        let anchors = collect_file_anchors(
            &signals,
            &intent(&["/private/tmp/x-ttk-map-view-refactor-plan.md"]),
            &[],
        );
        assert_eq!(
            anchors.len(),
            145,
            "every touched file anchors, superseded Signals included -- and the Session's one \
             `artifact_hints` entry contributes nothing, because `.md` is not in the extension \
             list that makes a dotted run read as a repository path"
        );

        let seeds = corpus.hits(&anchors);
        assert!(seeds.is_empty(), "{seeds:#?}");
        assert!(corpus.expand(&[]).is_empty());
    }

    // ---------------------------------------------------------------------------------------
    // Lane B: the second hop's admission
    // ---------------------------------------------------------------------------------------

    use std::collections::BTreeMap;

    use crate::SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS;

    /// A unit vector in a three-dimensional toy space, placed at a stated cosine from two axes.
    ///
    /// `against` is the cosine, in basis points, this vector has with the first and the second axis
    /// vector; the third dimension carries whatever is left of the unit length. Two seeds parked on
    /// the two axes therefore score any candidate at exactly the numbers the test names, which is
    /// what lets a test assert that 5199 is refused and 5200 admitted without a language model in
    /// the room. Nothing about the embedding space is being claimed here -- only that the
    /// comparison, the rounding and the threshold behave as written.
    fn at(against: [u16; 2]) -> Vec<f32> {
        let first = f32::from(against[0]) / 10_000.0;
        let second = f32::from(against[1]) / 10_000.0;
        let remainder = (1.0 - first * first - second * second).max(0.0).sqrt();
        vec![first, second, remainder]
    }

    /// The vector a seed sits on: `axis(0)` and `axis(1)` are the two [`at`] refers to.
    fn axis(index: usize) -> Vec<f32> {
        let mut vector = vec![0.0; 3];
        vector[index] = 1.0;
        vector
    }

    #[test]
    fn a_candidate_enters_on_its_score_and_a_basis_point_below_the_floor_does_not() {
        let corpus = Corpus::new();
        let seed = corpus.accept("the seed the Session already touched");
        let admitted = corpus.accept("a Context exactly at the floor");
        let refused = corpus.accept("a Context one basis point under it");

        let vectors = BTreeMap::from([
            (seed.revision, axis(0)),
            (admitted.revision, at([5_200, 0])),
            (refused.revision, at([5_199, 0])),
        ]);
        let admission = corpus.hop2(
            &[(seed.context, seed.revision)],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert_eq!(
            admission
                .hits
                .iter()
                .map(|hit| (hit.context_id, hit.score_basis_points()))
                .collect::<Vec<_>>(),
            [(admitted.context, 5_200)],
            "the floor is a boundary the score has to reach, not one it has to approach"
        );
        assert_eq!(admission.hits[0].revision_id, admitted.revision);
        assert_eq!(admission.hits[0].space_id, corpus.space_id);
        assert_eq!(admission.hits[0].seed.seed_context_id, seed.context);
        assert_eq!(admission.hits[0].seed.seed_revision_id, seed.revision);
        assert_eq!(admission.skipped_missing_vector, 0);
        assert_eq!(admission.seeds_missing_vector, 0);
        assert!(
            admission.hits[0].also_admitted_by.is_empty(),
            "one seed admitted it, so there is no second seed to carry"
        );
    }

    #[test]
    fn a_candidate_the_backfill_has_not_reached_is_skipped_and_counted() {
        let corpus = Corpus::new();
        let seed = corpus.accept("the seed the Session already touched");
        let cached = corpus.accept("a Context the backfill has encoded");
        let uncached = corpus.accept("a Context the backfill has not reached yet");
        let stale_generation = corpus.accept("a Context whose cached vector is another model's");

        let vectors = BTreeMap::from([
            (seed.revision, axis(0)),
            (cached.revision, at([9_000, 0])),
            // A vector of a different width is a different generation, not a different meaning.
            (stale_generation.revision, vec![1.0, 0.0]),
        ]);
        let admission = corpus.hop2(
            &[(seed.context, seed.revision)],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert_eq!(
            admission
                .hits
                .iter()
                .map(|hit| hit.context_id)
                .collect::<Vec<_>>(),
            [cached.context],
            "a missing vector costs one candidate and blocks nothing: {uncached:?}"
        );
        assert_eq!(
            admission.skipped_missing_vector, 2,
            "both the uncached Context and the one no seed could be compared with are counted"
        );
        assert_eq!(
            admission.samples.len(),
            1,
            "a pair that was never scored is not a decision and must not become a sample"
        );
    }

    #[test]
    fn a_seed_with_no_vector_is_counted_and_the_hop_runs_on_the_seeds_that_have_one() {
        let corpus = Corpus::new();
        let scored = corpus.accept("a seed the backfill reached");
        let unscored = corpus.accept("a seed the backfill has not reached");
        let candidate = corpus.accept("the Context under judgement");

        let vectors = BTreeMap::from([
            (scored.revision, axis(0)),
            (candidate.revision, at([7_000, 9_900])),
        ]);
        let admission = corpus.hop2(
            &[
                (scored.context, scored.revision),
                (unscored.context, unscored.revision),
            ],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert_eq!(admission.seeds_missing_vector, 1);
        assert_eq!(
            admission
                .hits
                .iter()
                .map(|hit| (hit.context_id, hit.seed.seed_context_id))
                .collect::<Vec<_>>(),
            [(candidate.context, scored.context)]
        );
    }

    #[test]
    fn every_pair_the_hop_judged_leaves_a_row_and_the_refusals_are_the_point() {
        let corpus = Corpus::new();
        let seed = corpus.accept("the seed the Session already touched");
        let admitted = corpus.accept("a Context above the floor");
        let refused = corpus.accept("a Context well below it");

        let vectors = BTreeMap::from([
            (seed.revision, axis(0)),
            (admitted.revision, at([7_400, 0])),
            (refused.revision, at([2_500, 0])),
        ]);
        let admission = corpus.hop2(
            &[(seed.context, seed.revision)],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        let mut recorded = admission
            .samples
            .iter()
            .map(|sample| {
                (
                    sample.candidate_revision_id,
                    sample.score_basis_points,
                    sample.admitted,
                )
            })
            .collect::<Vec<_>>();
        recorded.sort_by_key(|(_, score, _)| Reverse(*score));
        assert_eq!(
            recorded,
            [
                (admitted.revision, 7_400, true),
                (refused.revision, 2_500, false)
            ],
            "the floor is derived from the highest scoring negative, so a record that dropped the \
             refusals could never re-derive it"
        );
        assert!(
            admission
                .samples
                .iter()
                .all(|sample| sample.seed_revision_id == seed.revision)
        );
    }

    #[test]
    fn the_strongest_seed_names_the_hit_and_the_weaker_ones_survive_beside_it() {
        let corpus = Corpus::new();
        let near = corpus.accept("the seed this Context sits closest to");
        let far = corpus.accept("a second seed that also reaches it");
        let candidate = corpus.accept("the Context two seeds both admit");

        let vectors = BTreeMap::from([
            (near.revision, axis(0)),
            (far.revision, axis(1)),
            (candidate.revision, at([8_000, 5_400])),
        ]);
        let admission = corpus.hop2(
            &[(near.context, near.revision), (far.context, far.revision)],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert_eq!(admission.hits.len(), 1, "one Context, reached twice");
        let hit = &admission.hits[0];
        assert_eq!(hit.context_id, candidate.context);
        assert_eq!(
            (hit.seed.seed_context_id, hit.score_basis_points()),
            (near.context, 8_000),
            "the highest score is the Context's score and its seed is the one a why line names"
        );
        assert_eq!(
            hit.also_admitted_by
                .iter()
                .map(|seed| (seed.seed_context_id, seed.score_basis_points))
                .collect::<Vec<_>>(),
            [(far.context, 5_400)],
            "the weaker seed is still true and the assembler is the layer that decides to say so"
        );
        assert_eq!(
            hit.admitting_seeds()
                .map(|seed| seed.seed_context_id)
                .collect::<Vec<_>>(),
            [near.context, far.context]
        );
        assert_eq!(
            admission.samples.len(),
            2,
            "two seeds judged it, so two decisions were taken"
        );
    }

    /// Identifiers order a tie and nothing more, and only spellings that pass identifier syntax
    /// count as one. The negative here is the real one: `x-ttk-map-view` is a repository nickname
    /// two Contexts in the same product share by the accident of being about the same product, and
    /// letting a slice of it read as a shared identifier would turn corroboration into a second,
    /// much weaker lexical channel.
    #[test]
    fn shared_identifiers_break_a_tie_and_a_repository_nickname_is_not_one() {
        let corpus = Corpus::new();
        let seed = corpus.accept_document(
            "LynxMapController 在 x-ttk-map-view 中持有相机",
            "种子记录的是相机归属",
        );
        let names_the_symbol = corpus.accept_document(
            "LynxMapController 的可见性切换会重置相机",
            "候选与种子谈的是同一个类",
        );
        let names_the_repository = corpus.accept_document(
            "x-ttk-map-view 的构建任务在本仓不可用",
            "候选与种子只共享一个仓库昵称",
        );

        let vectors = BTreeMap::from([
            (seed.revision, axis(0)),
            // Deliberately the same score: the tie is the whole subject of this test.
            (names_the_symbol.revision, at([6_000, 0])),
            (names_the_repository.revision, at([6_000, 0])),
        ]);
        let admission = corpus.hop2(
            &[(seed.context, seed.revision)],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert_eq!(
            admission
                .hits
                .iter()
                .map(|hit| (hit.context_id, hit.shared_identifiers().to_vec()))
                .collect::<Vec<_>>(),
            [
                (
                    names_the_symbol.context,
                    vec!["lynxmapcontroller".to_owned()]
                ),
                (names_the_repository.context, Vec::new()),
            ],
            "`ttk`, `map` and `view` are short lower-case runs, not identifiers, so the Context \
             that names the class sorts above the one that merely names the product"
        );
    }

    #[test]
    fn a_seed_is_never_its_own_candidate_and_never_another_seeds() {
        let corpus = Corpus::new();
        let first = corpus.accept("the first seed");
        let second = corpus.accept("the second seed");

        // Both seeds are as close to each other as two Contexts can be.
        let vectors = BTreeMap::from([(first.revision, axis(0)), (second.revision, axis(0))]);
        let admission = corpus.hop2(
            &[
                (first.context, first.revision),
                (second.context, second.revision),
            ],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert!(
            admission.hits.is_empty(),
            "a seed is already in the Pack; admitting it again would double-count it: {:#?}",
            admission.hits
        );
        assert!(
            admission.samples.is_empty(),
            "no pair was judged, so no decision was taken"
        );
    }

    /// The same predicate Lane A's reverse lookup carries, for the same reason. A cached vector
    /// outlives the revision that produced it -- measured at 5 of 28 rows on one real installation
    /// -- so without this the second hop could reach a Context nothing else in the system will
    /// serve any more.
    #[test]
    fn a_vector_whose_context_left_the_accepted_set_admits_nothing() {
        let corpus = Corpus::new();
        let seed = corpus.accept("the seed the Session already touched");
        let retired = corpus.accept("the reading a later Context replaces");
        let current = corpus.supersede(retired.context, "the reading that replaces it");
        let revised_from = corpus.accept("the first reading of a Context that was then revised");
        let revised_to = corpus.revise(revised_from, "the corrected reading");

        let vectors = BTreeMap::from([
            (seed.revision, axis(0)),
            (retired.revision, axis(0)),
            (revised_from.revision, axis(0)),
            (current.revision, at([9_000, 0])),
            (revised_to.revision, at([9_000, 0])),
        ]);
        let admission = corpus.hop2(
            &[(seed.context, seed.revision)],
            &vectors,
            SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
        );

        assert_eq!(
            admission
                .hits
                .iter()
                .map(|hit| hit.context_id)
                .collect::<BTreeSet<_>>(),
            [current.context, revised_to.context].into_iter().collect(),
            "a superseded Context and a retired revision keep their vectors and must still be \
             invisible here"
        );
    }

    /// The floor is a parameter because ADR-0007 pre-registered it as a configuration key: 5200 is
    /// one synthetic corpus cross-checked against one installation, and the value is due to be
    /// re-derived from the samples above. A lane that read the constant directly could not be moved
    /// without a release.
    #[test]
    fn the_floor_is_the_caller_s_and_moving_it_moves_the_admitted_set() {
        let corpus = Corpus::new();
        let seed = corpus.accept("the seed the Session already touched");
        let borderline = corpus.accept("a Context between the two floors");

        let vectors = BTreeMap::from([
            (seed.revision, axis(0)),
            (borderline.revision, at([4_800, 0])),
        ]);
        let seeds = [(seed.context, seed.revision)];

        assert!(
            corpus
                .hop2(&seeds, &vectors, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS)
                .hits
                .is_empty()
        );
        assert_eq!(
            corpus
                .hop2(&seeds, &vectors, 4_500)
                .hits
                .iter()
                .map(|hit| hit.context_id)
                .collect::<Vec<_>>(),
            [borderline.context]
        );
        assert!(
            corpus
                .hop2(&seeds, &vectors, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS)
                .samples
                .iter()
                .all(|sample| !sample.admitted),
            "the refusal is recorded as the decision that was taken under the floor in force"
        );
    }

    #[test]
    fn an_empty_seed_set_admits_nothing_and_reads_neither_corpus_nor_vectors() {
        let corpus = Corpus::new();
        let orphan = corpus.accept("a Context no seed reaches");
        let vectors = BTreeMap::from([(orphan.revision, axis(0))]);

        assert_eq!(
            corpus.hop2(&[], &vectors, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS),
            Hop2Admission::default(),
            "the second hop queries from a seed; with none it has nothing to ask"
        );
    }
}

/// The second hop, end to end, against the corpus its floor was calibrated on.
///
/// [`embedding_hop2_admission_calibration`](../../tests/embedding_hop2_admission_calibration.rs)
/// measures the *distribution*: it encodes `fixtures/association/hard-negative-v1.json` and scores
/// all 276 unordered pairs to say where a threshold could go. This module asserts that the code
/// which actually decides admissions reproduces that reading through the real path -- Contexts
/// accepted into a real projection, the corpus text read by `embeddable_revisions`, every Context
/// taken as a seed in turn, and [`lane_b_hits`] doing the judging. The two together are the claim
/// the floor rests on: the number is defensible *and* the implementation is the number.
///
/// It is a ratchet, not a target. The two constants below are what the calibration measured; a
/// deliberate recalibration moves them in the same commit as the floor and says why.
///
/// `#[ignore]`d because it needs roughly 2.4 GB of weights this repository deliberately does not
/// ship, and it takes the same two environment variables the calibration does:
///
/// ```text
/// SCTX_PROBE_F2LLM_MODEL=~/.cache/huggingface/hub/models--codefuse-ai--F2LLM-v2-0.6B/snapshots/<sha> \
/// SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
///   cargo test --release -p sctx-search --lib -- --ignored hop2_ratchet --nocapture
/// ```
#[cfg(all(test, feature = "embedding-onnx", unix))]
mod hop2_ratchet {
    use std::collections::{BTreeMap, BTreeSet};

    use sctx_domain::RevisionId;
    use serde_json::Value;

    use super::test_support::Corpus;
    use crate::{EmbeddingProvider, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS, f2llm_snapshot};

    const FIXTURE: &str = include_str!("../../../fixtures/association/hard-negative-v1.json");

    /// Contexts the fixture holds. The two ratchets below are not comparable across a change to it.
    const TOTAL_CONTEXTS: usize = 24;

    /// Cross-repository, same-topic pairs the floor admits, as the calibration measured them.
    ///
    /// The same 13 that file asserts, reached from the other end: it counts unordered pairs above
    /// the floor, this counts the pairs the lane actually produces when each Context is used as a
    /// seed. They agree because a cosine is symmetric and one floor governs both directions, and
    /// the day they stop agreeing is the day the lane stopped implementing the calibration.
    const RETAINED_CROSS_REPO_SAME_TOPIC: usize = 13;

    /// Where one fixture Context ended up in the projection, and what it is about.
    struct Placed {
        label: String,
        repo: String,
        topic: String,
    }

    #[test]
    #[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
    fn no_seed_admits_a_context_from_another_topic() {
        let fixture: Value = serde_json::from_str(FIXTURE).expect("the fixture is valid JSON");
        let corpus = Corpus::new();
        let mut placed = BTreeMap::<RevisionId, Placed>::new();
        let mut seeds = Vec::new();
        for context in fixture["contexts"]
            .as_array()
            .expect("the fixture holds a context array")
        {
            let field = |name: &str| {
                context[name]
                    .as_str()
                    .unwrap_or_else(|| panic!("a context carries {name}"))
                    .to_owned()
            };
            let accepted = corpus.accept_document(&field("statement"), &field("rationale"));
            placed.insert(
                accepted.revision,
                Placed {
                    label: field("label"),
                    repo: field("repo"),
                    topic: field("topic"),
                },
            );
            seeds.push((accepted.context, accepted.revision));
        }
        assert_eq!(placed.len(), TOTAL_CONTEXTS);

        let provider = f2llm_snapshot::provider().as_ref();
        let vectors = corpus
            .embeddable()
            .into_iter()
            .map(|(revision_id, text)| {
                (
                    revision_id,
                    provider
                        .encode_bulk(&text)
                        .expect("every corpus text encodes"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            vectors.len(),
            TOTAL_CONTEXTS,
            "every accepted Context has to be embeddable, or the hop is measuring a smaller corpus"
        );

        let mut cross_topic = Vec::new();
        let mut cross_repo_joins = BTreeSet::new();
        let mut same_repo_joins = BTreeSet::new();
        for seed in &seeds {
            let admission = corpus.hop2(
                std::slice::from_ref(seed),
                &vectors,
                SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS,
            );
            assert_eq!(
                admission.skipped_missing_vector, 0,
                "every candidate has a vector in this corpus"
            );
            assert_eq!(
                admission.samples.len(),
                TOTAL_CONTEXTS - 1,
                "one seed judges every other Context exactly once"
            );
            let from = &placed[&seed.1];
            for hit in &admission.hits {
                let to = &placed[&hit.revision_id];
                let pair = BTreeSet::from([from.label.clone(), to.label.clone()]);
                if to.topic == from.topic {
                    if to.repo == from.repo {
                        same_repo_joins.insert(pair);
                    } else {
                        cross_repo_joins.insert(pair);
                    }
                } else {
                    cross_topic.push((
                        hit.score_basis_points(),
                        from.label.clone(),
                        to.label.clone(),
                    ));
                }
            }
        }

        println!(
            "\nfloor {SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS}: {} cross-repository joins, {} \
             same-repository joins, {} cross-topic admissions",
            cross_repo_joins.len(),
            same_repo_joins.len(),
            cross_topic.len()
        );

        // The constraint that does not bend. A Context admitted here is injected with no further
        // test of relevance, so one cross-topic admission is one off-topic Context in front of an
        // Agent working on something else.
        assert!(
            cross_topic.is_empty(),
            "the second hop admitted {} cross-topic Contexts: {cross_topic:#?}",
            cross_topic.len()
        );
        assert!(
            cross_repo_joins.len() >= RETAINED_CROSS_REPO_SAME_TOPIC,
            "the lane delivers {} cross-repository joins, below the \
             {RETAINED_CROSS_REPO_SAME_TOPIC} the calibration measured over the same corpus",
            cross_repo_joins.len()
        );
    }
}
