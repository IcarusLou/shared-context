use std::collections::{BTreeMap, BTreeSet, VecDeque};

use sctx_domain::{
    Applicability, ArtifactKey, ArtifactRef, AutomaticCandidateStatus, CandidateAnalysis,
    CandidateAnalysisStatus, CandidateAssessmentPath, CandidateAssessmentRelation,
    CandidateConfidence, CandidateRelationAssessment, CandidateSpaceRecommendation,
    CandidateSpaceRecommendationPath, ContextCandidate, ContextRevision, ContextRevisionDraft,
    ContextRevisionRef, EvidenceSnapshotDraft, IntentSnapshot, ProposedSpaceGroupKey,
    RecommendedSpaceRole, RevisionLifecycle, SpaceId, TaskId, TaskIntentRevisionId, TaskSignal,
    TaskSpaceAssociation, WorkingIntentSnapshot, hints,
};
use sctx_engineering_graph::EngineeringProjectionSnapshot;
use sctx_index::{DomainSnapshot, normalize_search_text, search_tokens};

use crate::{
    ContextStatus, Error, ErrorKind, Result, ScopeFilter, SearchEngine, SearchFilters,
    SearchRequest,
};

pub const MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET: usize = 1_024;
pub const MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET: usize = 32_768;
pub const MAX_CANDIDATE_ANALYSIS_TOP_K: usize = 32;
const PRIMARY_SPACE_THRESHOLD: u64 = 90_000;
const RRF_K: u64 = 60;
const PROPOSED_SPACE_GROUP_SCORE: u64 = 1_000_000;
/// Maximum `char` length of a proposed Space title before elision.
///
/// A proposed title is a short handle a reviewer scans in a Candidate list, not a restatement of
/// the goal: the whole goal already reaches the Space as `desired_outcome`.
const PROPOSED_SPACE_TITLE_MAX_CHARS: usize = 40;
/// Normalized statement token Jaccard at or above which two Claims state the same fact.
pub const STATEMENT_SIMILARITY_STRONG_BASIS_POINTS: u64 = 8_000;
/// Lower bound of the band where two Claims are close enough to need human contradiction review.
pub const STATEMENT_SIMILARITY_REVIEW_BASIS_POINTS: u64 = 5_000;
/// Normalized statement overlap at or above which a Claim restates an accepted Context.
///
/// Measured against this repository's own accepted Contexts: pairs that restate one conclusion in
/// different words score `5_400` to `10_000` basis points, while pairs about different conclusions
/// stay at or below `1_100`. The band between this bound and
/// [`STATEMENT_SIMILARITY_STRONG_BASIS_POINTS`] is where a rewrite of one conclusion used to fall
/// through as merely related, because the strongest duplicate path needs a topic key and the topic
/// key is optional.
pub const STATEMENT_NEAR_DUPLICATE_BASIS_POINTS: u64 = 5_000;
/// Shared repository identifiers at or above which two Claims are about the same code.
///
/// One shared identifier is a coincidence of vocabulary; two independently written spellings of
/// the same type, service or symbol are not. This is the only strong-association path that
/// survives a Claim being written in a different natural language from its target.
pub const SHARED_IDENTIFIER_STRONG_OVERLAP: usize = 2;
/// Shared identifiers at or above which the two Claims restate one another regardless of wording.
pub const SHARED_IDENTIFIER_SUPPORT_OVERLAP: usize = 3;

/// Complete, Task-owned input to one rebuildable Candidate analysis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAnalysisRequest {
    pub candidate: ContextCandidate,
    pub source_task_id: TaskId,
    /// The Intent revision the source Episode closed under. Kept as provenance only: the proposed
    /// Space group is derived from `source_task_id` alone, so the recommendation stays confirmable
    /// after later governance turns advance the Intent head.
    pub source_intent_revision_id: TaskIntentRevisionId,
    pub source_working_intent: WorkingIntentSnapshot,
    pub source_task_signals: Vec<TaskSignal>,
    pub explicit_related_contexts: Vec<ContextRevisionRef>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub proposed_space_group_space_id: Option<SpaceId>,
    pub token_budget: usize,
    pub top_k: usize,
}

/// Derived review result; it is never a Context or Space fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAnalysisResult {
    pub analysis: CandidateAnalysis,
    pub space_recommendations: Vec<CandidateSpaceRecommendation>,
    pub confidence: CandidateConfidence,
    pub candidate_status: AutomaticCandidateStatus,
}

// Four independent yes-or-no answers about one retrieval target, not a state machine: each is
// read on its own by the relation rules and none constrains another, so folding them into an enum
// would only hide which question was asked.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone)]
struct TargetState {
    revision: ContextRevision,
    safe: bool,
    /// True when the target revision is accepted, whatever its automatic-injection eligibility.
    ///
    /// `safe` narrows further as Graph and Space checks run; duplicate review asks only whether
    /// the knowledge base already accepted this conclusion.
    accepted: bool,
    space_conflicted: bool,
    channels: BTreeMap<&'static str, usize>,
    paths: Vec<CandidateAssessmentPath>,
    score: u64,
    /// Normalized statement token Jaccard against the Candidate, in basis points.
    statement_similarity: u64,
    /// True when a near-duplicate statement negates the Candidate instead of restating it.
    negation_conflict: bool,
    /// Normalized repository identifiers this accepted, safe target shares with the Candidate.
    shared_identifiers: Vec<String>,
}

#[derive(Default)]
struct SpaceState {
    score: u64,
    paths: Vec<CandidateSpaceRecommendationPath>,
    safe_strong_target: bool,
    intent_conflicted: bool,
}

impl SearchEngine {
    /// Analyzes one persisted Candidate through exact, Graph, Scope and FTS channels.
    ///
    /// # Errors
    ///
    /// Rejects invalid bounds, stale mixed projection generations, malformed Candidate input, or
    /// storage/query failures.
    #[allow(clippy::too_many_lines)]
    pub fn analyze_candidate(
        &self,
        request: &CandidateAnalysisRequest,
    ) -> Result<CandidateAnalysisResult> {
        validate_request(request)?;
        request.candidate.content.validate()?;
        request.source_working_intent.validate()?;
        TaskSignal::validate_collection(&request.source_task_signals)?;
        // The index reuses the snapshot it already reduced for this Tree, so the Builder's own
        // read and every per-Candidate analysis of the same commit share one reduction.
        let snapshot = self.index.shared_domain_snapshot()?;
        if request
            .proposed_space_group_space_id
            .is_some_and(|space_id| !snapshot.projection.spaces.contains_key(&space_id))
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Proposed Space group mapping points to an unavailable Space",
            ));
        }
        let graph = self
            .engineering_graph
            .as_ref()
            .map(sctx_engineering_graph::EngineeringProjectionStore::read_snapshot)
            .transpose()?
            .flatten();
        let query = candidate_query(&request.candidate.content);
        let bm25 = self.search(&SearchRequest {
            query,
            filters: SearchFilters {
                space_ids: Vec::new(),
                scope: ScopeFilter::default(),
                kinds: Vec::new(),
                statuses: vec![
                    ContextStatus::Candidate,
                    ContextStatus::Accepted,
                    ContextStatus::Deprecated,
                    ContextStatus::Superseded,
                    ContextStatus::GovernanceConflict,
                ],
            },
            page_size: request.top_k.saturating_mul(4).clamp(1, 128),
            cursor: None,
            // The Candidate query is a single token, so ranked and exact matching agree; ranked
            // keeps the coverage explanation consistent with every other search caller.
            match_mode: crate::SearchMatchMode::Ranked,
        })?;
        let source_spaces = self.task_space_associations(
            request.source_task_id,
            &request.source_working_intent,
            &request.source_task_signals,
        )?;
        let candidate_intent = candidate_working_intent(&request.candidate.content);
        let candidate_spaces =
            self.task_space_associations(request.source_task_id, &candidate_intent, &[])?;
        for (tree, generation) in [
            (&bm25.indexed_tree_oid, bm25.projection_generation),
            (
                &source_spaces.indexed_tree_oid,
                source_spaces.projection_generation,
            ),
            (
                &candidate_spaces.indexed_tree_oid,
                candidate_spaces.projection_generation,
            ),
        ] {
            if tree != &snapshot.metadata.indexed_tree_oid
                || generation != snapshot.metadata.projection_generation
            {
                return Err(Error::new(
                    ErrorKind::StaleState,
                    "Candidate analysis projection changed during retrieval; retry",
                ));
            }
        }

        let mut targets = collect_targets(&snapshot);
        add_exact_channels(&request.candidate.content, &mut targets);
        add_identifier_channel(&request.candidate.content, &mut targets);
        add_explicit_channel(&request.explicit_related_contexts, &mut targets);
        add_graph_channel(graph.as_ref(), &request.artifact_refs, &mut targets)?;
        add_bm25_channel(&bm25.results, &mut targets);
        score_targets(&mut targets);
        let mut ranked = targets
            .into_iter()
            .filter(|(_, target)| !target.channels.is_empty())
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .1
                .score
                .cmp(&left.1.score)
                .then_with(|| left.0.cmp(&right.0))
        });
        let total_target_count = ranked.len();
        ranked.truncate(request.top_k);
        let mut safe_targets = BTreeMap::new();
        let mut assessments = ranked
            .into_iter()
            .map(|(target, state)| {
                safe_targets.insert(target, state.safe);
                assess_target(&request.candidate.content, target, state)
            })
            .collect::<Vec<_>>();
        if assessments.is_empty() {
            assessments.push(novel_assessment());
        }
        let omitted_target_count = total_target_count.saturating_sub(assessments.len());
        let mut recommendations = recommend_spaces(
            &snapshot,
            &assessments,
            &safe_targets,
            &source_spaces.associations,
            &candidate_spaces.associations,
            &request.source_working_intent,
            ProposedSpaceGroupKey::from_task(request.source_task_id),
            request.proposed_space_group_space_id,
            request.top_k,
        );
        let confidence = aggregate_confidence(&assessments);
        let mut analysis = CandidateAnalysis {
            status: CandidateAnalysisStatus::Complete,
            assessments,
            context_tree_oid: Some(snapshot.metadata.indexed_tree_oid.clone()),
            context_generation: Some(snapshot.metadata.projection_generation),
            graph_context_tree_oid: graph
                .as_ref()
                .and_then(|snapshot| snapshot.context_tree_oid.clone()),
            artifact_generation: graph
                .as_ref()
                .map(|snapshot| snapshot.projection.artifact_generation.clone()),
            token_budget: request.token_budget,
            estimated_tokens: 0,
            omitted_target_count,
            error_code: None,
        };
        enforce_budget(&mut analysis, &mut recommendations)?;
        let candidate_status = review_status(&analysis, &recommendations, &confidence);
        analysis.validate()?;
        Ok(CandidateAnalysisResult {
            analysis,
            space_recommendations: recommendations,
            confidence,
            candidate_status,
        })
    }
}

fn validate_request(request: &CandidateAnalysisRequest) -> Result<()> {
    if request.token_budget < MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET
        || request.token_budget > MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Candidate analysis token_budget must be between {MIN_CANDIDATE_ANALYSIS_TOKEN_BUDGET} and {MAX_CANDIDATE_ANALYSIS_TOKEN_BUDGET}"
            ),
        ));
    }
    if request.top_k == 0 || request.top_k > MAX_CANDIDATE_ANALYSIS_TOP_K {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Candidate analysis top_k must be between 1 and {MAX_CANDIDATE_ANALYSIS_TOP_K}"
            ),
        ));
    }
    Ok(())
}

fn collect_targets(snapshot: &DomainSnapshot) -> BTreeMap<ContextRevisionRef, TargetState> {
    snapshot
        .projection
        .spaces
        .values()
        .flat_map(|space| {
            space.contexts.values().flat_map(move |context| {
                context
                    .revisions
                    .iter()
                    .map(move |(revision_id, revision)| {
                        (
                            ContextRevisionRef {
                                context_id: context.context_id,
                                revision_id: *revision_id,
                            },
                            TargetState {
                                revision: revision.revision.clone(),
                                safe: revision.lifecycle == RevisionLifecycle::Accepted
                                    && context.auto_injection.eligible,
                                accepted: revision.lifecycle == RevisionLifecycle::Accepted,
                                space_conflicted: space.intent.heads.len() > 1,
                                channels: BTreeMap::new(),
                                paths: Vec::new(),
                                score: 0,
                                statement_similarity: 0,
                                negation_conflict: false,
                                shared_identifiers: Vec::new(),
                            },
                        )
                    })
            })
        })
        .collect()
}

fn add_exact_channels(
    candidate: &ContextRevisionDraft,
    targets: &mut BTreeMap<ContextRevisionRef, TargetState>,
) {
    let candidate_statement = normalize_search_text(&candidate.statement);
    let candidate_tokens = token_set(&candidate_statement);
    let candidate_negations = negation_markers(&candidate.statement);
    let candidate_topic = candidate
        .topic_key
        .as_deref()
        .map(normalize_search_text)
        .filter(|value| !value.is_empty());
    let mut canonical = Vec::new();
    let mut statements = Vec::new();
    let mut topics = Vec::new();
    let mut scopes = Vec::new();
    for (target, state) in targets.iter_mut() {
        let target_statement = normalize_search_text(&state.revision.statement);
        let equal_statement = target_statement == candidate_statement;
        state.statement_similarity = if equal_statement {
            10_000
        } else {
            jaccard_basis_points(&candidate_tokens, &token_set(&target_statement))
        };
        // A near-duplicate statement is the same fact stated differently. Aligning the statement
        // before comparing the rest of the draft keeps "same fact, same evidence" an exact
        // duplicate and "same fact, different evidence" a supporting Claim, instead of letting the
        // wording difference fall through to a contradiction review.
        if state.statement_similarity >= STATEMENT_SIMILARITY_STRONG_BASIS_POINTS {
            // Token overlap cannot see a negation: "is reached" and "is not reached" share every
            // other token. Differing negation markers mean the two Claims disagree about the same
            // fact, which is a contradiction to review, never a duplicate or supporting Claim.
            state.negation_conflict =
                negation_markers(&state.revision.statement) != candidate_negations;
            if state.negation_conflict {
                // Still retrieved and ranked as a near-duplicate, but no equality path is claimed.
                statements.push(*target);
            } else {
                let mut aligned = revision_draft(&state.revision);
                aligned.statement.clone_from(&candidate.statement);
                if &aligned == candidate {
                    canonical.push(*target);
                    add_path(state, CandidateAssessmentPath::CanonicalDraftEquality);
                } else {
                    statements.push(*target);
                    add_path(state, CandidateAssessmentPath::StatementEquality);
                }
            }
        }
        if let (Some(candidate_topic), Some(target_topic)) = (
            candidate_topic.as_ref(),
            state
                .revision
                .topic_key
                .as_deref()
                .map(normalize_search_text),
        ) && &target_topic == candidate_topic
        {
            topics.push(*target);
            add_path(
                state,
                CandidateAssessmentPath::TopicEquality {
                    topic_key: state.revision.topic_key.clone().unwrap_or_default(),
                },
            );
        }
        let overlap = scope_overlap(&candidate.applicability, &state.revision.applicability);
        if !overlap.domains.is_empty() {
            scopes.push(*target);
            add_path(
                state,
                CandidateAssessmentPath::ScopeOverlap {
                    domains: overlap.domains,
                    platforms: overlap.platforms,
                    conditions: overlap.conditions,
                },
            );
        }
    }
    add_ranked_channel(targets, "canonical", canonical);
    add_ranked_channel(targets, "statement", statements);
    add_ranked_channel(targets, "topic", topics);
    add_ranked_channel(targets, "scope", scopes);
}

/// Intersects the repository identifiers the Candidate and each target spell out in their prose.
///
/// Only accepted, automatically injectable revisions are considered: a shared identifier is a
/// strong association path, and a strong path must never be drawn to a fact the Task is not
/// allowed to inherit. The target's identifiers are read from its own text, so a Context recorded
/// long before server-side derivation existed participates without any Engineering Reference.
fn add_identifier_channel(
    candidate: &ContextRevisionDraft,
    targets: &mut BTreeMap<ContextRevisionRef, TargetState>,
) {
    let candidate_identifiers =
        hints::normalized_identifiers(draft_prose(candidate).iter().map(String::as_str));
    if candidate_identifiers.len() < SHARED_IDENTIFIER_STRONG_OVERLAP {
        return;
    }
    let mut ranked = Vec::new();
    for (target, state) in targets.iter_mut() {
        if !state.safe {
            continue;
        }
        let target_identifiers = hints::normalized_identifiers(
            revision_prose(&state.revision).iter().map(String::as_str),
        );
        let shared = candidate_identifiers
            .intersection(&target_identifiers)
            .cloned()
            .collect::<Vec<_>>();
        if shared.len() < SHARED_IDENTIFIER_STRONG_OVERLAP {
            continue;
        }
        state.shared_identifiers.clone_from(&shared);
        ranked.push(*target);
        add_path(
            state,
            CandidateAssessmentPath::SharedIdentifier {
                identifiers: shared,
            },
        );
    }
    add_ranked_channel(targets, "identifier", ranked);
}

/// Free prose of one Candidate draft: statement, rationale and every Evidence text leaf.
fn draft_prose(draft: &ContextRevisionDraft) -> Vec<String> {
    let mut texts = vec![draft.statement.clone(), draft.rationale.clone()];
    for evidence in &draft.evidence {
        texts.push(evidence.supports.clone());
        texts.push(evidence.interpretation.clone());
        hints::json_string_leaves(&evidence.content, &mut texts);
    }
    texts
}

/// Free prose of one immutable revision, read exactly the way [`draft_prose`] reads a draft.
fn revision_prose(revision: &ContextRevision) -> Vec<String> {
    let mut texts = vec![revision.statement.clone(), revision.rationale.clone()];
    for evidence in &revision.evidence {
        texts.push(evidence.supports.clone());
        texts.push(evidence.interpretation.clone());
        hints::json_string_leaves(&evidence.content, &mut texts);
    }
    texts
}

fn add_explicit_channel(
    explicit: &[ContextRevisionRef],
    targets: &mut BTreeMap<ContextRevisionRef, TargetState>,
) {
    let mut values = explicit.to_vec();
    values.sort();
    values.dedup();
    for target in &values {
        if let Some(state) = targets.get_mut(target) {
            add_path(state, CandidateAssessmentPath::ExplicitRelatedContext);
        }
    }
    add_ranked_channel(targets, "explicit", values);
}

fn add_graph_channel(
    graph: Option<&EngineeringProjectionSnapshot>,
    artifacts: &[ArtifactRef],
    targets: &mut BTreeMap<ContextRevisionRef, TargetState>,
) -> Result<()> {
    let Some(graph) = graph else {
        return Ok(());
    };
    let graph_contexts = graph
        .projection
        .contexts
        .iter()
        .map(|context| {
            (
                ContextRevisionRef {
                    context_id: context.context_id,
                    revision_id: context.revision.revision_id,
                },
                context,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut ranked = Vec::new();
    for artifact in artifacts {
        let key = ArtifactKey::derive(artifact.repository_id.clone(), artifact.locator.clone())?;
        for reference in &graph.projection.references {
            if reference.resolution.resolved_artifact.as_ref() != Some(&key)
                || reference.association.is_none()
            {
                continue;
            }
            let root = ContextRevisionRef {
                context_id: reference.context_id,
                revision_id: reference.revision_id,
            };
            let mut queue = VecDeque::from([(root, 0_u8, None)]);
            let mut visited = BTreeSet::new();
            while let Some((target, depth, relation_kind)) = queue.pop_front() {
                if !visited.insert(target) || depth > 2 {
                    continue;
                }
                if let Some(state) = targets.get_mut(&target) {
                    ranked.push(target);
                    add_path(
                        state,
                        CandidateAssessmentPath::ExactArtifactGraph {
                            artifact: artifact.clone(),
                        },
                    );
                    if let Some(relation) = relation_kind {
                        add_path(
                            state,
                            CandidateAssessmentPath::ContextRelationHop { relation, depth },
                        );
                    }
                    if let Some(graph_context) = graph_contexts.get(&target) {
                        state.safe |= graph_context.safety.automatic_injection_eligible;
                    }
                }
                if depth < 2
                    && let Some(context) = graph_contexts.get(&target)
                {
                    for relation in &context.relations {
                        queue.push_back((
                            ContextRevisionRef {
                                context_id: relation.target_context_id,
                                revision_id: relation.target_revision_id,
                            },
                            depth + 1,
                            Some(relation.kind),
                        ));
                    }
                }
            }
        }
    }
    ranked.sort();
    ranked.dedup();
    add_ranked_channel(targets, "graph", ranked);
    Ok(())
}

fn add_bm25_channel(
    results: &[crate::SearchResult],
    targets: &mut BTreeMap<ContextRevisionRef, TargetState>,
) {
    let mut ranked = Vec::new();
    for result in results {
        let target = ContextRevisionRef {
            context_id: result.context_id,
            revision_id: result.revision_id,
        };
        if let Some(state) = targets.get_mut(&target) {
            ranked.push(target);
            if !result.match_reason.matched_tokens.is_empty() {
                add_path(
                    state,
                    CandidateAssessmentPath::ContextFullText {
                        matched_terms: result.match_reason.matched_tokens.clone(),
                    },
                );
            }
            state.safe &= result.auto_injection_eligible;
        }
    }
    add_ranked_channel(targets, "bm25", ranked);
}

fn add_ranked_channel(
    targets: &mut BTreeMap<ContextRevisionRef, TargetState>,
    channel: &'static str,
    mut values: Vec<ContextRevisionRef>,
) {
    values.sort();
    values.dedup();
    for (rank, target) in values.into_iter().enumerate() {
        if let Some(state) = targets.get_mut(&target) {
            state.channels.entry(channel).or_insert(rank + 1);
        }
    }
}

fn score_targets(targets: &mut BTreeMap<ContextRevisionRef, TargetState>) {
    for state in targets.values_mut() {
        state.score = state
            .channels
            .iter()
            .map(|(channel, rank)| rrf(channel_weight(channel), *rank))
            .sum();
    }
}

fn channel_weight(channel: &str) -> u64 {
    match channel {
        "canonical" => 1_000,
        "explicit" | "graph" => 900,
        "statement" => 850,
        "identifier" => 800,
        "topic" => 700,
        "scope" => 400,
        "bm25" => 200,
        _ => 0,
    }
}

fn rrf(weight: u64, rank: usize) -> u64 {
    weight.saturating_mul(10_000) / (RRF_K + u64::try_from(rank).unwrap_or(u64::MAX))
}

#[allow(clippy::too_many_lines)]
fn assess_target(
    candidate: &ContextRevisionDraft,
    target: ContextRevisionRef,
    mut state: TargetState,
) -> CandidateRelationAssessment {
    let canonical = state.channels.contains_key("canonical");
    let statement = state.channels.contains_key("statement");
    let topic = state.channels.contains_key("topic");
    let explicit_or_graph =
        state.channels.contains_key("explicit") || state.channels.contains_key("graph");
    // Sharing at least one exact Artifact is the engineering-strength scope signal. A bare
    // applicability domain overlap is not: every Claim of one Task inherits the same Intent
    // domains, so it used to make unrelated Claims contradict each other.
    let shared_artifact = state
        .paths
        .iter()
        .any(|path| matches!(path, CandidateAssessmentPath::ExactArtifactGraph { .. }));
    let similarity = state.statement_similarity;
    let statement_differs = normalize_search_text(&state.revision.statement)
        != normalize_search_text(&candidate.statement);
    let negation_conflict = state.negation_conflict;
    // A shared identifier set is only strong within one Context kind: an `issue` and the
    // `validation` that exercises the same class are related, not the same fact.
    let shared_identifiers = state.shared_identifiers.len();
    let same_kind = state.revision.kind == candidate.kind;
    let strong_identifier = shared_identifiers >= SHARED_IDENTIFIER_STRONG_OVERLAP && same_kind;
    let identifier_restates = similarity >= STATEMENT_SIMILARITY_REVIEW_BASIS_POINTS
        || shared_identifiers >= SHARED_IDENTIFIER_SUPPORT_OVERLAP;
    // The same statement on the same topic is the same fact, whatever else the two drafts carry:
    // two Tasks recording one finding differ in Evidence identity, `problem_view` and rationale
    // wording, none of which makes the second a new fact. Whole-draft equality still wins first so
    // the stronger path keeps its own trigger text.
    let restates_topic = topic && !statement_differs;
    // One conclusion written a second time against a Context the knowledge base already accepted.
    // The topic key is optional, so without this path a Task that restated an accepted conclusion
    // in its own words and typed no topic reached only `supports` or `unresolved_related`, and the
    // reviewer confirmed the same fact again. It is deliberately confined to a rewritten
    // statement: an identical statement filed under a different topic key stays `supports`, which
    // is the "same statement, new Evidence" case, and a differing statement under a matching topic
    // key stays a contradiction to review. A shared exact Artifact is also left alone: that is
    // proof the two Claims are about the same code, and the existing path sends a differing
    // statement over shared code to contradiction review, which is the stronger, evidence-backed
    // reading. The restatements this catches carry no shared Artifact — they predate server-side
    // Reference derivation, which is exactly why nothing but their wording connects them.
    let near_duplicate = state.accepted
        && !topic
        && !shared_artifact
        && statement_differs
        && !negation_conflict
        && similarity >= STATEMENT_NEAR_DUPLICATE_BASIS_POINTS;
    if near_duplicate {
        add_path(
            &mut state,
            CandidateAssessmentPath::NearDuplicateStatement {
                similarity_basis_points: similarity,
            },
        );
    }
    let relation = if negation_conflict {
        CandidateAssessmentRelation::PotentialContradiction
    } else if canonical || restates_topic || near_duplicate {
        CandidateAssessmentRelation::ExactDuplicate
    } else if statement {
        CandidateAssessmentRelation::Supports
    } else if topic && explicit_or_graph {
        CandidateAssessmentRelation::Revises
    } else if strong_identifier {
        if identifier_restates {
            CandidateAssessmentRelation::Supports
        } else {
            CandidateAssessmentRelation::PotentialContradiction
        }
    } else if statement_differs
        && (topic || (shared_artifact && similarity >= STATEMENT_SIMILARITY_REVIEW_BASIS_POINTS))
    {
        CandidateAssessmentRelation::PotentialContradiction
    } else {
        CandidateAssessmentRelation::UnresolvedRelated
    };
    let trigger = if negation_conflict {
        format!("Path: statement similarity {similarity} basis points but negation markers differ")
    } else if canonical {
        format!("Path: canonical draft equality at statement similarity {similarity} basis points")
    } else if restates_topic {
        "Path: normalized statement equality on one topic key".to_owned()
    } else if near_duplicate {
        format!(
            "Path: statement similarity {similarity} basis points against an accepted Context at or above the {STATEMENT_NEAR_DUPLICATE_BASIS_POINTS} near-duplicate threshold"
        )
    } else if statement {
        if similarity >= 10_000 {
            "Path: normalized statement equality".to_owned()
        } else {
            format!(
                "Path: statement similarity {similarity} basis points at or above the {STATEMENT_SIMILARITY_STRONG_BASIS_POINTS} threshold"
            )
        }
    } else if topic && explicit_or_graph {
        "Path: topic equality with an explicit Context or exact Artifact Graph link".to_owned()
    } else if strong_identifier {
        if identifier_restates {
            format!(
                "Path: {shared_identifiers} shared repository identifiers on the same Context kind, statement similarity {similarity} basis points"
            )
        } else {
            format!(
                "Path: {shared_identifiers} shared repository identifiers on the same Context kind while the statement differs, similarity {similarity} basis points"
            )
        }
    } else if topic && statement_differs {
        "Path: topic equality with a differing statement".to_owned()
    } else if matches!(
        relation,
        CandidateAssessmentRelation::PotentialContradiction
    ) {
        format!("Path: a shared exact Artifact with statement similarity {similarity} basis points")
    } else if shared_artifact {
        format!(
            "Path: a shared exact Artifact with statement similarity {similarity} basis points below the {STATEMENT_SIMILARITY_REVIEW_BASIS_POINTS} review threshold"
        )
    } else {
        format!("Path: retrieval proximity only, statement similarity {similarity} basis points")
    };
    if !state.safe {
        let reason = if state.space_conflicted {
            "Target Space Intent is conflicted; no winner was selected".to_owned()
        } else {
            "Target Context is not safe for automatic factual promotion".to_owned()
        };
        add_path(
            &mut state,
            CandidateAssessmentPath::SafetyDiagnostic { reason },
        );
    }
    let (points, relation_reason) = match relation {
        CandidateAssessmentRelation::ExactDuplicate if canonical => (
            10_000,
            "The complete canonical Candidate draft equals the immutable Context revision",
        ),
        CandidateAssessmentRelation::ExactDuplicate if near_duplicate => (
            9_500,
            "The statement restates a conclusion the knowledge base already accepted; confirming it again needs an explicit supersedes or contradicts decision",
        ),
        CandidateAssessmentRelation::ExactDuplicate => (
            10_000,
            "The statement and the topic key both equal the immutable Context revision; this restates a fact the knowledge base already holds",
        ),
        CandidateAssessmentRelation::Supports => (
            9_000,
            "The statement is equal while rationale or Evidence differs; this is supporting evidence, not a duplicate",
        ),
        CandidateAssessmentRelation::Revises => (
            8_500,
            "The non-empty topic matches and an explicit Context or exact Artifact Graph path links the target",
        ),
        CandidateAssessmentRelation::PotentialContradiction => (
            6_500,
            "Topic or applicability overlaps while the statement differs; human contradiction review is required",
        ),
        CandidateAssessmentRelation::UnresolvedRelated => (
            4_000,
            "Retrieval found related text or engineering proximity, but no factual relation was established",
        ),
        CandidateAssessmentRelation::Novel => unreachable!(),
    };
    CandidateRelationAssessment {
        relation,
        target: Some(target),
        confidence: CandidateConfidence {
            basis_points: points,
            rationale: relation_reason.to_owned(),
        },
        paths: state.paths,
        reasons: identifier_reason(&state.shared_identifiers, same_kind)
            .into_iter()
            .fold(
                vec![relation_reason.to_owned(), trigger],
                |mut reasons, reason| {
                    reasons.push(reason);
                    reasons
                },
            ),
    }
}

/// Names the shared identifiers so a reviewer can see why two differently worded Claims met.
///
/// A cross-kind overlap stays `unresolved_related`, but the reviewer still learns which code the
/// two Claims have in common, which is the whole reason the target was retrieved.
fn identifier_reason(shared: &[String], same_kind: bool) -> Option<String> {
    if shared.len() < SHARED_IDENTIFIER_STRONG_OVERLAP {
        return None;
    }
    let named = shared.join(", ");
    Some(if same_kind {
        format!("Shared repository identifiers: {named}")
    } else {
        format!("Shared repository identifiers on a different Context kind: {named}")
    })
}

/// English negation words compared as whole tokens.
const ENGLISH_NEGATION_MARKERS: &[&str] = &["not", "no", "never", "cannot", "without", "fails"];
/// Chinese negation markers compared as substrings of the raw statement.
const CHINESE_NEGATION_MARKERS: &[&str] = &["没有", "不会", "不能", "不", "无", "未"];

/// Collects the negation markers of one raw statement.
///
/// This reads the raw statement rather than the normalized token stream so a Chinese marker is
/// found regardless of how the tokenizer segments the surrounding characters.
fn negation_markers(statement: &str) -> BTreeSet<&'static str> {
    let lowered = statement.to_lowercase();
    let mut markers = BTreeSet::new();
    for word in lowered.split(|character: char| !character.is_alphanumeric()) {
        if let Some(marker) = ENGLISH_NEGATION_MARKERS
            .iter()
            .find(|marker| **marker == word)
        {
            markers.insert(*marker);
        }
    }
    for marker in CHINESE_NEGATION_MARKERS {
        if statement.contains(marker) {
            markers.insert(*marker);
        }
    }
    markers
}

/// Splits one already normalized statement into its deduplicated search token set.
fn token_set(normalized: &str) -> BTreeSet<&str> {
    normalized.split_whitespace().collect()
}

/// Jaccard overlap of two token sets in basis points; two empty sets never overlap.
fn jaccard_basis_points(left: &BTreeSet<&str>, right: &BTreeSet<&str>) -> u64 {
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let intersection = left.intersection(right).count() as u64;
    let union = left.union(right).count() as u64;
    if union == 0 {
        return 0;
    }
    intersection.saturating_mul(10_000) / union
}

fn novel_assessment() -> CandidateRelationAssessment {
    CandidateRelationAssessment {
        relation: CandidateAssessmentRelation::Novel,
        target: None,
        confidence: CandidateConfidence {
            basis_points: 6_000,
            rationale: "No exact, Graph, topic, scope, or FTS candidate survived retrieval"
                .to_owned(),
        },
        paths: vec![CandidateAssessmentPath::NoSufficientCandidate],
        reasons: vec![
            "No sufficiently related immutable Context revision was retrieved".to_owned(),
        ],
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn recommend_spaces(
    snapshot: &DomainSnapshot,
    assessments: &[CandidateRelationAssessment],
    safe_targets: &BTreeMap<ContextRevisionRef, bool>,
    source_associations: &[TaskSpaceAssociation],
    candidate_associations: &[TaskSpaceAssociation],
    source_working_intent: &WorkingIntentSnapshot,
    proposed_space_group_key: ProposedSpaceGroupKey,
    proposed_space_group_space_id: Option<SpaceId>,
    top_k: usize,
) -> Vec<CandidateSpaceRecommendation> {
    let owners = snapshot
        .projection
        .spaces
        .iter()
        .flat_map(|(space_id, space)| {
            space.contexts.values().map(move |context| {
                (
                    context.context_id,
                    (*space_id, space.intent.heads.len() > 1),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let mut spaces = BTreeMap::<SpaceId, SpaceState>::new();
    for (rank, assessment) in assessments.iter().enumerate() {
        let Some(target) = assessment.target else {
            continue;
        };
        let Some((space_id, conflicted)) = owners.get(&target.context_id).copied() else {
            continue;
        };
        let state = spaces.entry(space_id).or_default();
        state.score = state.score.saturating_add(rrf(
            match assessment.relation {
                CandidateAssessmentRelation::ExactDuplicate => 1_000,
                CandidateAssessmentRelation::Supports => 850,
                CandidateAssessmentRelation::Revises => 800,
                CandidateAssessmentRelation::PotentialContradiction => 400,
                CandidateAssessmentRelation::UnresolvedRelated => 200,
                CandidateAssessmentRelation::Novel => 0,
            },
            rank + 1,
        ));
        state.intent_conflicted |= conflicted;
        state.safe_strong_target |= safe_targets.get(&target).copied().unwrap_or(false)
            && matches!(
                assessment.relation,
                CandidateAssessmentRelation::ExactDuplicate
                    | CandidateAssessmentRelation::Supports
                    | CandidateAssessmentRelation::Revises
            );
        push_space_path(
            state,
            CandidateSpaceRecommendationPath::CandidateTarget {
                relation: assessment.relation,
                target,
            },
        );
    }
    add_association_spaces(&mut spaces, source_associations, 500, false);
    add_association_spaces(&mut spaces, candidate_associations, 600, true);
    if let Some(space_id) = proposed_space_group_space_id {
        let state = spaces.entry(space_id).or_default();
        state.score = state.score.saturating_add(PROPOSED_SPACE_GROUP_SCORE);
        state.safe_strong_target = true;
        push_space_path(
            state,
            CandidateSpaceRecommendationPath::ProposedSpaceGroupResolved {
                proposed_space_group_key,
            },
        );
    }
    for (space_id, state) in &mut spaces {
        if snapshot
            .projection
            .spaces
            .get(space_id)
            .is_some_and(|space| space.intent.heads.len() > 1)
        {
            state.intent_conflicted = true;
            push_space_path(
                state,
                CandidateSpaceRecommendationPath::IntentConflict {
                    head_count: snapshot.projection.spaces[space_id].intent.heads.len(),
                },
            );
        }
    }
    let mut ranked = spaces.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .score
            .cmp(&left.1.score)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.truncate(top_k);
    let primary = ranked.iter().position(|(_, state)| {
        !state.intent_conflicted
            && state.score >= PRIMARY_SPACE_THRESHOLD
            && state.safe_strong_target
    });
    let mut recommendations = ranked
        .into_iter()
        .enumerate()
        .map(|(index, (space_id, state))| {
            let role = if Some(index) == primary {
                RecommendedSpaceRole::Primary
            } else {
                RecommendedSpaceRole::Related
            };
            CandidateSpaceRecommendation::existing_with_paths(
                space_id,
                role,
                if state.intent_conflicted {
                    "Related Space is shown diagnostically; its Intent heads conflict and no winner was selected"
                } else if role == RecommendedSpaceRole::Primary {
                    "Fused Candidate targets, Task association and Space Intent evidence exceed the Primary review threshold"
                } else {
                    "Space remains a non-binding related review candidate"
                },
                CandidateConfidence {
                    basis_points: u16::try_from((state.score / 20).min(10_000)).unwrap_or(10_000),
                    rationale: format!("Deterministic RRF score {}", state.score),
                },
                state.paths,
            )
        })
        .collect::<Vec<_>>();
    if primary.is_none() && proposed_space_group_space_id.is_none() {
        recommendations.push(
            CandidateSpaceRecommendation::proposed_new_for_group_with_paths(
                proposed_space_group_key,
                proposed_intent(source_working_intent),
                "No safe existing Space exceeded the Primary threshold; confirming here opens a provisional Space named from the Task goal",
                CandidateConfidence {
                    basis_points: 6_000,
                    rationale:
                        "No safe fused existing-Space candidate reached the Primary threshold"
                            .to_owned(),
                },
                vec![
                    CandidateSpaceRecommendationPath::ProposedFromTaskIntentRevision {
                        proposed_space_group_key,
                    },
                ],
            ),
        );
    }
    recommendations
}

fn add_association_spaces(
    spaces: &mut BTreeMap<SpaceId, SpaceState>,
    associations: &[TaskSpaceAssociation],
    weight: u64,
    candidate_intent: bool,
) {
    for (rank, association) in associations.iter().enumerate() {
        let state = spaces.entry(association.space_id).or_default();
        state.score = state.score.saturating_add(rrf(weight, rank + 1));
        if candidate_intent && !association.matched_intent_fields.is_empty() {
            push_space_path(
                state,
                CandidateSpaceRecommendationPath::SpaceIntentMatch {
                    matched_terms: association.matched_intent_fields.clone(),
                },
            );
        } else {
            push_space_path(
                state,
                CandidateSpaceRecommendationPath::SourceTaskAssociation {
                    reason: association.reasons.join("; "),
                },
            );
        }
    }
}

fn proposed_intent(source: &WorkingIntentSnapshot) -> IntentSnapshot {
    let mut in_scope = source.in_scope.clone();
    if in_scope.is_empty() {
        in_scope.push(source.goal.clone());
    }
    let mut acceptance_conditions = source.acceptance_conditions.clone();
    if acceptance_conditions.is_empty() {
        acceptance_conditions.push(source.goal.clone());
    }
    IntentSnapshot {
        title: proposed_space_title(&source.goal),
        problem: source
            .current_direction
            .clone()
            .unwrap_or_else(|| source.goal.clone()),
        desired_outcome: source.goal.clone(),
        in_scope,
        out_of_scope: source.out_of_scope.clone(),
        acceptance_conditions,
        domain_terms: source.domains.clone(),
    }
}

fn proposed_space_title(goal: &str) -> String {
    let words = strip_system_suggestion_prefix(goal);
    let normalized = words.join(" ");
    if normalized.is_empty() {
        return FALLBACK_PROPOSED_SPACE_TITLE.to_owned();
    }
    if normalized.chars().count() <= PROPOSED_SPACE_TITLE_MAX_CHARS {
        return normalized;
    }
    let mut title = normalized
        .chars()
        .take(PROPOSED_SPACE_TITLE_MAX_CHARS)
        .collect::<String>();
    // Elide at the exact character bound rather than a word bound: a goal can be written in a
    // script without spaces, where word truncation either keeps everything or nothing.
    while title.ends_with(char::is_whitespace) {
        title.pop();
    }
    title.push('\u{2026}');
    title
}

/// Title used when a Working Intent goal carries nothing but the historical prefix.
const FALLBACK_PROPOSED_SPACE_TITLE: &str = "Task intent";

/// Splits a goal into whitespace-collapsed words with any historical `System suggestion:` prefix
/// removed. Older goals were written with that prefix, and it says nothing about the work.
fn strip_system_suggestion_prefix(goal: &str) -> Vec<&str> {
    let words = goal.split_whitespace().collect::<Vec<_>>();
    let mut start = 0;
    if words.len() >= 2
        && words[0].eq_ignore_ascii_case("system")
        && words[1]
            .trim_end_matches([':', '-', '\u{2014}'])
            .eq_ignore_ascii_case("suggestion")
    {
        start = 2;
        if words.get(start).is_some_and(|word| {
            word.chars()
                .all(|character| matches!(character, ':' | '-' | '\u{2014}'))
        }) {
            start += 1;
        }
    }
    words[start..].to_vec()
}

fn enforce_budget(
    analysis: &mut CandidateAnalysis,
    recommendations: &mut Vec<CandidateSpaceRecommendation>,
) -> Result<()> {
    loop {
        analysis.estimated_tokens = estimate_tokens(&(analysis.clone(), &*recommendations))?;
        if analysis.estimated_tokens <= analysis.token_budget {
            return Ok(());
        }
        if let Some(index) = recommendations.iter().rposition(|recommendation| {
            matches!(
                recommendation,
                CandidateSpaceRecommendation::Existing {
                    role: RecommendedSpaceRole::Related,
                    ..
                }
            )
        }) {
            recommendations.remove(index);
            continue;
        }
        if analysis.assessments.len() > 1 {
            analysis.assessments.pop();
            analysis.omitted_target_count = analysis.omitted_target_count.saturating_add(1);
            continue;
        }
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Candidate analysis token budget cannot hold one assessment and its review metadata",
        ));
    }
}

fn estimate_tokens(value: &impl serde::Serialize) -> Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len().div_ceil(4).max(1))
        .map_err(|error| {
            Error::new(
                ErrorKind::InvariantViolation,
                format!("serialize Candidate analysis token estimate: {error}"),
            )
        })
}

fn review_status(
    analysis: &CandidateAnalysis,
    recommendations: &[CandidateSpaceRecommendation],
    confidence: &CandidateConfidence,
) -> AutomaticCandidateStatus {
    if analysis
        .assessments
        .iter()
        .any(|assessment| assessment.relation == CandidateAssessmentRelation::ExactDuplicate)
    {
        AutomaticCandidateStatus::ExactDuplicateReview
    } else if analysis.assessments.iter().any(|assessment| {
        assessment.relation == CandidateAssessmentRelation::PotentialContradiction
    }) {
        AutomaticCandidateStatus::PotentialContradictionReview
    } else if !recommendations.iter().any(|recommendation| {
        matches!(
            recommendation,
            CandidateSpaceRecommendation::Existing {
                role: RecommendedSpaceRole::Primary,
                ..
            }
        )
    }) {
        AutomaticCandidateStatus::NeedsSpaceReview
    } else if confidence.basis_points < 5_000 {
        AutomaticCandidateStatus::NeedsEvidence
    } else {
        AutomaticCandidateStatus::ReadyForReview
    }
}

fn aggregate_confidence(assessments: &[CandidateRelationAssessment]) -> CandidateConfidence {
    let points = assessments
        .iter()
        .map(|assessment| assessment.confidence.basis_points)
        .max()
        .unwrap_or(0);
    CandidateConfidence {
        basis_points: points,
        rationale: "Highest typed relationship confidence after deterministic fusion".to_owned(),
    }
}

fn candidate_query(candidate: &ContextRevisionDraft) -> String {
    search_tokens(&candidate.statement)
        .into_iter()
        .max_by(|left, right| left.len().cmp(&right.len()).then_with(|| right.cmp(left)))
        .unwrap_or_else(|| candidate.statement.clone())
}

fn candidate_working_intent(candidate: &ContextRevisionDraft) -> WorkingIntentSnapshot {
    let mut seen_acceptance_conditions = BTreeSet::new();
    let acceptance_conditions = candidate
        .evidence
        .iter()
        .filter_map(|evidence| {
            let canonical = evidence
                .supports
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            seen_acceptance_conditions
                .insert(canonical)
                .then(|| evidence.supports.clone())
        })
        .collect();
    WorkingIntentSnapshot {
        goal: candidate.statement.clone(),
        current_direction: Some(candidate.rationale.clone()),
        in_scope: candidate.applicability.conditions.clone(),
        out_of_scope: Vec::new(),
        domains: candidate.applicability.domains.clone(),
        platforms: candidate.applicability.platforms.clone(),
        constraints: candidate.assumptions.clone(),
        acceptance_conditions,
        artifact_hints: Vec::new(),
        interface_hints: Vec::new(),
        open_questions: Vec::new(),
    }
}

fn revision_draft(revision: &ContextRevision) -> ContextRevisionDraft {
    ContextRevisionDraft {
        kind: revision.kind,
        topic_key: revision.topic_key.clone(),
        problem_view: revision.problem_view.clone(),
        statement: revision.statement.clone(),
        rationale: revision.rationale.clone(),
        applicability: revision.applicability.clone(),
        assumptions: revision.assumptions.clone(),
        recheck_when: revision.recheck_when.clone(),
        hints: revision.hints.clone(),
        relations: revision.relations.clone(),
        evidence: revision
            .evidence
            .iter()
            .map(|evidence| EvidenceSnapshotDraft {
                kind: evidence.kind,
                supports: evidence.supports.clone(),
                content: evidence.content.clone(),
                interpretation: evidence.interpretation.clone(),
                limitations: evidence.limitations.clone(),
            })
            .collect(),
    }
}

fn scope_overlap(left: &Applicability, right: &Applicability) -> Applicability {
    Applicability {
        domains: intersection(&left.domains, &right.domains),
        platforms: intersection(&left.platforms, &right.platforms),
        conditions: intersection(&left.conditions, &right.conditions),
    }
}

fn intersection(left: &[String], right: &[String]) -> Vec<String> {
    let right = right
        .iter()
        .map(|value| normalize_search_text(value))
        .collect::<BTreeSet<_>>();
    left.iter()
        .filter(|value| right.contains(&normalize_search_text(value)))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn add_path(state: &mut TargetState, path: CandidateAssessmentPath) {
    if !state.paths.contains(&path) {
        state.paths.push(path);
    }
}

fn push_space_path(state: &mut SpaceState, path: CandidateSpaceRecommendationPath) {
    if !state.paths.contains(&path) {
        state.paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{PROPOSED_SPACE_TITLE_MAX_CHARS, proposed_space_title};

    #[test]
    fn a_short_goal_is_its_own_title_after_normalization() {
        assert_eq!(
            proposed_space_title("  Repair   association   recall  "),
            "Repair association recall"
        );
    }

    #[test]
    fn the_historical_system_suggestion_prefix_is_stripped() {
        assert_eq!(
            proposed_space_title("System suggestion: Repair association recall"),
            "Repair association recall"
        );
        assert_eq!(
            proposed_space_title("System Suggestion — Repair association recall"),
            "Repair association recall"
        );
    }

    #[test]
    fn a_long_goal_is_elided_at_the_character_bound_in_any_script() {
        let latin = proposed_space_title(
            "Build grouped checkout compatibility knowledge for every supported client",
        );
        assert_eq!(latin, "Build grouped checkout compatibility kno\u{2026}");
        assert_eq!(latin.chars().count(), PROPOSED_SPACE_TITLE_MAX_CHARS + 1);

        // A goal written without spaces must still be elided; word truncation could not do this.
        let han = proposed_space_title(&"评论底栏".repeat(20));
        assert_eq!(han.chars().count(), PROPOSED_SPACE_TITLE_MAX_CHARS + 1);
        assert!(han.ends_with('\u{2026}'));
    }

    #[test]
    fn elision_never_leaves_a_dangling_space_before_the_ellipsis() {
        // The 40th `char` of this goal is the space after "or", which must not be kept.
        let title = proposed_space_title("Repair association recall in Chinese or English queries");
        assert_eq!(title, "Repair association recall in Chinese or\u{2026}");
    }

    #[test]
    fn a_goal_carrying_only_the_prefix_falls_back_to_a_named_title() {
        assert_eq!(proposed_space_title("System suggestion:"), "Task intent");
        assert_eq!(proposed_space_title("   "), "Task intent");
    }
}
