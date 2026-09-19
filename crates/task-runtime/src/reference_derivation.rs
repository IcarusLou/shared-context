//! Deterministic server-side derivation of engineering coordinates from Claim text.
//!
//! The public `task_checkpoint` contract stays four fields wide. Agents already write
//! `ProductAnchorAssem.kt:202` or `BottomBarProtocolManager` inside `statement`, `rationale` and
//! `evidence.summary`; this module extracts those spellings, hands each of them to an injected
//! resolver, and turns the resolved ones into [`EngineeringReferenceDraft`] values that flow
//! through the already existing Candidate Build provenance path.
//!
//! Two channels, in strict order. A Claim that spells a path is read by its path. A Claim that
//! spells none falls back to the type names it wrote: `MapSceneRuntime.present()` points at a file
//! as squarely as `MapSceneRuntime.kt:88` does, and both real long Sessions wrote almost entirely
//! in the first form. The fallback is capped, and everything it places says so in its limitations.
//!
//! Two hard rules:
//!
//! * Extraction is a pure function of the Claim text. The only external input is the resolver.
//! * Resolution never fails a Checkpoint. An unavailable checkout, a missing `git` binary, a slow
//!   or failing query, and an ambiguous spelling all leave the Claim unplaced; the spelling is
//!   preserved as an unresolved hint instead. An ambiguity is additionally reported as one, so a
//!   caller can tell "several files answer to this" from "nothing does".

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use sctx_domain::hints::{self, TextScan};
use sctx_domain::{
    AgentCheckpointId, ArtifactKind, ArtifactLocator, CheckpointClaimId, ContextKind,
    EngineeringReferenceDraft, ReferenceRelation, RepoRelativePath, RepositoryId,
};

/// One textual spelling that looks like a repository file path.
///
/// Extraction itself lives in `sctx_index::hints` so the projection reads Claim prose exactly the
/// way this module does; a Context accepted before server-side derivation existed therefore gains
/// the same identifier hints on the next `index rebuild`.
pub use sctx_domain::hints::PathCandidate;

/// Marks every Reference this module authored so review can tell it from Agent-authored facts.
pub const DERIVED_BY_SERVER_LIMITATION: &str = "derived_by_server_from_claim_text";
/// Marks the only sanctioned basename-to-path inference.
pub const UNIQUE_BASENAME_LIMITATION: &str = "derived_from_unique_basename";
/// Marks a coordinate placed by a type name the Claim pointed at, not by a path it wrote.
pub const SYMBOL_MENTION_LIMITATION: &str = "derived_from_symbol_mention";
/// Upper bound on one read-only `git ls-files` lookup for one Candidate Build.
pub const GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on the number of distinct basenames one Candidate Build may ask Git about.
pub const MAX_GIT_QUERY_PATTERNS: usize = 128;
/// Bound on the coordinates one Claim may gain from type names alone.
///
/// A path spelling is a coordinate the Agent wrote down; a type name is one this module inferred,
/// so the second channel gets a ceiling the first does not. Three is what a Claim about one change
/// plausibly names — the type, its collaborator, and the contract between them — and a Claim that
/// name-drops a dozen types is discussing an area, not a file.
pub const MAX_SYMBOL_DERIVED_REFERENCES_PER_CLAIM: usize = 3;
/// Bound on how many type names one Claim contributes to the Build's Git query.
pub const MAX_SYMBOL_MENTIONS_PER_CLAIM: usize = 8;
/// Bound on how many distinct `(checkout, revision, basenames)` answers stay memoized at once.
///
/// The server process is long lived, so the memo is bounded rather than unbounded; the least
/// recently used answer is dropped once the bound is reached.
const MAX_TRACKED_PATH_CACHE_ENTRIES: usize = 32;

/// One resolved repository coordinate for a [`PathCandidate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedReference {
    pub repository_id: RepositoryId,
    pub path: RepoRelativePath,
    /// True when only the unique-basename lookup could place this spelling.
    pub derived_from_unique_basename: bool,
    /// True when the Claim named a type rather than a file, and exactly one tracked file is
    /// named after that type.
    pub derived_from_symbol_mention: bool,
}

/// What one lookup of one spelling found across the checkouts a Session is scoped to.
///
/// The third state is the point of this type. A lookup that found several files used to be
/// indistinguishable from one that found none — both were `None` — so an ambiguity was recorded
/// as "this Claim names nothing", and the once-only derivation marker then made that permanent.
/// Ambiguity is a different answer: the spelling *is* a coordinate, the checkout simply could not
/// say which one yet, and a checkout that gains or loses a file can answer it later.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MentionResolution {
    /// Exactly one tracked file answers to this spelling.
    Placed(ResolvedReference),
    /// Several tracked files answer to it; placing one of them would be a guess.
    Ambiguous { matches: usize },
    /// No tracked file answers to it.
    Unplaced,
}

/// Which reading of the Claim text produced one spelling.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MentionChannel {
    /// The Agent wrote a file path.
    Path,
    /// The Agent wrote a type name.
    Symbol,
}

impl MentionChannel {
    /// The stable spelling this channel is recorded under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Symbol => "symbol",
        }
    }

    /// Reads back a spelling [`MentionChannel::as_str`] wrote.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "path" => Some(Self::Path),
            "symbol" => Some(Self::Symbol),
            _ => None,
        }
    }
}

/// One spelling the checkouts answered with more than one file, and how many they found.
///
/// The count is what makes a later re-derivation decidable without re-reading the Claim: if the
/// checkout now answers the same spelling with a different number of files, the ambiguity that was
/// recorded is not the ambiguity that exists, and the spelling deserves another look.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AmbiguousMention {
    /// The spelling as it was looked up, without any line span.
    pub spelling: String,
    pub channel: MentionChannel,
    pub matches: usize,
}

/// The two channels one Claim's coordinates can arrive through.
///
/// Both are plain functions rather than a resolver object because the caller that has several
/// checkouts has to fold their answers together before this module sees one, and because a caller
/// with no checkout at all must be able to say so in one expression.
pub struct ClaimResolver<'a> {
    path: &'a dyn Fn(&PathCandidate) -> MentionResolution,
    symbol: &'a dyn Fn(&str) -> MentionResolution,
}

static NOTHING_BY_PATH: fn(&PathCandidate) -> MentionResolution = |_| MentionResolution::Unplaced;
static NOTHING_BY_SYMBOL: fn(&str) -> MentionResolution = |_| MentionResolution::Unplaced;

impl<'a> ClaimResolver<'a> {
    /// Both channels live.
    #[must_use]
    pub const fn new(
        path: &'a dyn Fn(&PathCandidate) -> MentionResolution,
        symbol: &'a dyn Fn(&str) -> MentionResolution,
    ) -> Self {
        Self { path, symbol }
    }

    /// Path spellings only: the exact reading this module had before type names were read.
    #[must_use]
    pub const fn paths(path: &'a dyn Fn(&PathCandidate) -> MentionResolution) -> Self {
        Self {
            path,
            symbol: &NOTHING_BY_SYMBOL,
        }
    }

    /// Asks the path channel about one spelling.
    #[must_use]
    pub fn resolve_path(&self, candidate: &PathCandidate) -> MentionResolution {
        (self.path)(candidate)
    }

    /// Asks the type-name channel about one spelling.
    #[must_use]
    pub fn resolve_symbol(&self, symbol: &str) -> MentionResolution {
        (self.symbol)(symbol)
    }

    /// Places nothing; the safe default for a Session with no reachable checkout.
    #[must_use]
    pub const fn nothing() -> ClaimResolver<'static> {
        ClaimResolver {
            path: &NOTHING_BY_PATH,
            symbol: &NOTHING_BY_SYMBOL,
        }
    }
}

/// Folds one spelling's answers from several checkouts into the one answer a Claim gets.
///
/// A Session started at the common parent of several checkouts carries several Repositories, and
/// the rule has always been that exactly one resolving checkout wins. What changes here is only
/// the bookkeeping: two checkouts that both place the spelling are now recorded as the ambiguity
/// they are, instead of being dropped as if nothing had been found.
#[must_use]
pub fn combine_resolutions(
    answers: impl IntoIterator<Item = MentionResolution>,
) -> MentionResolution {
    let mut placed = Vec::new();
    let mut ambiguous = 0;
    for answer in answers {
        match answer {
            MentionResolution::Placed(reference) => placed.push(reference),
            MentionResolution::Ambiguous { matches } => ambiguous += matches,
            MentionResolution::Unplaced => {}
        }
    }
    match (placed.len(), ambiguous) {
        (1, 0) => MentionResolution::Placed(placed.remove(0)),
        (0, 0) => MentionResolution::Unplaced,
        (found, ambiguous) => MentionResolution::Ambiguous {
            matches: found + ambiguous,
        },
    }
}

/// One resolved coordinate with the mention count and first mention position that rank it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedMentions {
    reference: ResolvedReference,
    mentions: usize,
    first_order: usize,
}

/// Complete derivation for one Claim.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClaimReferenceDerivation {
    /// Deduplicated, deterministically ordered graph facts.
    pub engineering_references: Vec<EngineeringReferenceDraft>,
    /// `{kind}:{repository}:{path}` for the most frequently mentioned resolved reference.
    pub topic_key_hint: Option<String>,
    /// Path and identifier spellings that stay retrieval text and never become graph facts.
    pub unresolved_hints: Vec<String>,
    /// Spellings several tracked files answered to, deduplicated and deterministically ordered.
    pub ambiguous_mentions: Vec<AmbiguousMention>,
}

/// Bound on how many unplaced spellings one Episode's record keeps by name.
///
/// The counts are complete; the sample is what makes a count legible to whoever reads it later,
/// and eight spellings is enough to recognise a pattern without turning a marker row into a log.
pub const MAX_UNRESOLVED_SAMPLE: usize = 8;

/// What one Episode's recorded derivation attempt found.
///
/// This is the record the once-only marker used to be. A bare marker could only say "derived",
/// which made an ambiguity and a genuine absence the same durable fact; this says which, how
/// many, and — for the ambiguities — exactly how many files answered each spelling, which is the
/// one thing a later Build needs in order to decide whether the question has a new answer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReferenceDerivationRecord {
    /// Coordinates the derivation placed across every Claim of the Episode.
    pub placed: usize,
    /// Spellings several tracked files answered to, with the count that made them ambiguous.
    pub ambiguous: Vec<AmbiguousMention>,
    /// Spellings nothing answered to.
    pub unresolved: usize,
    /// A bounded sample of [`ReferenceDerivationRecord::unresolved`], by name.
    pub unresolved_sample: Vec<String>,
    /// True once an operator asked for this Episode's ambiguities to be looked at again.
    pub reopened: bool,
    /// When the recorded derivation ran; `None` for a row written before this was recorded.
    pub derived_at_unix_seconds: Option<u64>,
}

impl ReferenceDerivationRecord {
    /// Folds one Episode's per-Claim derivations into the record that is persisted for it.
    #[must_use]
    pub fn from_derivations(derivations: &[DerivedClaimReferences]) -> Self {
        let mut ambiguous = BTreeSet::new();
        let mut unresolved_sample = BTreeSet::new();
        let mut placed = 0;
        let mut unresolved = 0;
        for derivation in derivations {
            placed += derivation.engineering_references.len();
            ambiguous.extend(derivation.ambiguous_mentions.iter().cloned());
            // An unresolved hint is every spelling the Claim wrote that no coordinate came out of,
            // identifiers included: that is exactly the set a reader wants to see a sample of.
            unresolved += derivation.unresolved_hints.len();
            unresolved_sample.extend(derivation.unresolved_hints.iter().cloned());
        }
        Self {
            placed,
            ambiguous: ambiguous.into_iter().collect(),
            unresolved,
            unresolved_sample: unresolved_sample
                .into_iter()
                .take(MAX_UNRESOLVED_SAMPLE)
                .collect(),
            reopened: false,
            derived_at_unix_seconds: None,
        }
    }

    /// Rebuilds the lookup input for one recorded ambiguous spelling.
    ///
    /// A path spelling round-trips through the scanner because it is stored the way the Agent
    /// wrote it, leading separator included; a type name is its own lookup key.
    #[must_use]
    pub fn relookup(mention: &AmbiguousMention) -> Option<PathCandidate> {
        match mention.channel {
            MentionChannel::Path => hints::scan_text(&mention.spelling).paths.into_iter().next(),
            MentionChannel::Symbol => None,
        }
    }
}

/// Everything one Episode's Claims name, batched so a single Git query answers the whole Build.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EpisodeClaimMentions {
    /// Every path spelling, deduplicated and deterministically ordered.
    pub paths: Vec<PathCandidate>,
    /// Every type name, deduplicated and deterministically ordered.
    pub symbols: Vec<String>,
}

impl EpisodeClaimMentions {
    /// True when no Claim in this Episode named anything a checkout could be asked about.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.symbols.is_empty()
    }
}

/// Per-Claim derivation record produced by Candidate Build, never by the Checkpoint ACK.
///
/// The durable ACK writes only the receipt and the Candidate Build outbox, so a Claim leaves
/// `task_checkpoint` with no derived coordinates at all. Candidate Build resolves them once,
/// writes them back onto the persisted Claim, and every later Build reports that first answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedClaimReferences {
    /// Checkpoint that persists this Claim.
    pub checkpoint_id: AgentCheckpointId,
    /// Identity of the persisted Claim these coordinates belong to.
    pub claim_id: CheckpointClaimId,
    pub engineering_references: Vec<EngineeringReferenceDraft>,
    pub topic_key_hint: Option<String>,
    pub unresolved_hints: Vec<String>,
    /// Spellings this derivation could not place because several tracked files answered to them.
    ///
    /// Empty on a replay of an already recorded derivation: the ambiguity belongs to the lookup,
    /// and a replay performs none.
    pub ambiguous_mentions: Vec<AmbiguousMention>,
    /// True when `applicability.domains`/`platforms` were inherited from the Working Intent
    /// rather than authored on the Claim itself.
    pub applicability_inherited: bool,
}

/// Maps the Context kind onto a `File`-compatible [`ReferenceRelation`].
///
/// `Constrains` and `Validates` are deliberately not used: `sctx_domain` only accepts
/// `Implements` and `DependsOn` for an `ArtifactKind::File` locator, and a derived reference must
/// never invent a `Module`, `Api` or `Test` locator it did not actually resolve.
#[must_use]
pub const fn relation_for_kind(kind: ContextKind) -> ReferenceRelation {
    match kind {
        ContextKind::Progress => ReferenceRelation::Implements,
        ContextKind::Decision
        | ContextKind::Contract
        | ContextKind::Issue
        | ContextKind::Risk
        | ContextKind::Validation
        | ContextKind::Discovery => ReferenceRelation::DependsOn,
    }
}

const fn kind_name(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::Decision => "decision",
        ContextKind::Contract => "contract",
        ContextKind::Issue => "issue",
        ContextKind::Risk => "risk",
        ContextKind::Validation => "validation",
        ContextKind::Discovery => "discovery",
        ContextKind::Progress => "progress",
    }
}

/// Extracts every path-shaped spelling in one Claim without consulting any checkout.
///
/// Callers batch these across a whole Checkpoint so one read-only Git query answers every Claim.
#[must_use]
pub fn claim_path_candidates(
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
) -> Vec<PathCandidate> {
    claim_texts(statement, rationale, evidence_summaries)
        .iter()
        .flat_map(|text| scan_text(text).paths)
        .collect()
}

/// Extracts every type name one Claim points at, bounded, without consulting any checkout.
///
/// The companion of [`claim_path_candidates`] for the second channel: Candidate Build batches
/// these across the whole Checkpoint so the one read-only Git query answers both readings at once.
#[must_use]
pub fn claim_symbol_mentions(
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
) -> Vec<String> {
    let texts = claim_texts(statement, rationale, evidence_summaries);
    let mut symbols = hints::symbol_mentions(texts.iter().map(String::as_str));
    symbols.truncate(MAX_SYMBOL_MENTIONS_PER_CLAIM);
    symbols
}

/// Recomputes the unresolved hints of an already persisted Claim, without any checkout.
///
/// Candidate Build runs long after the Checkpoint transaction and only has the persisted Claim.
/// Passing that Claim's `engineering_references` back in reproduces exactly the hint set the
/// original derivation produced, so the searchable text stays stable across rebuilds.
#[must_use]
pub fn claim_hints(
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
    resolved: &[EngineeringReferenceDraft],
) -> Vec<String> {
    let placed = resolved
        .iter()
        .map(|reference| {
            let path = reference.locator.path().as_str();
            path.rsplit('/').next().unwrap_or(path).to_owned()
        })
        .collect::<BTreeSet<_>>();
    let mut hints = BTreeSet::new();
    for text in claim_texts(statement, rationale, evidence_summaries) {
        let scan = scan_text(&text);
        for candidate in scan.paths {
            if !placed.contains(&candidate.basename) {
                hints.insert(candidate.hint_text());
            }
        }
        hints.extend(scan.identifiers);
    }
    hints.into_iter().collect()
}

/// Derives engineering references, a topic hint, and unresolved hints for one Claim.
///
/// The resolver is the only impure input; with [`ClaimResolver::nothing`] this function is total
/// and pure.
///
/// Two channels, in strict order. Path spellings are read first, exactly as they always were. Only
/// a Claim that placed *no* path at all then falls back to the type names it wrote: a Claim that
/// already named a file has said where it lives, and asking again by type would only add a second,
/// weaker guess about the same change. The fallback is capped at
/// [`MAX_SYMBOL_DERIVED_REFERENCES_PER_CLAIM`] and every coordinate it places carries
/// [`SYMBOL_MENTION_LIMITATION`], so a reviewer can always tell an inferred coordinate from a
/// written one.
#[must_use]
pub fn derive_claim_references(
    kind: ContextKind,
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
    resolver: &ClaimResolver<'_>,
) -> ClaimReferenceDerivation {
    let mut paths = Vec::new();
    let mut identifiers = BTreeSet::new();
    for text in claim_texts(statement, rationale, evidence_summaries) {
        let scan = scan_text(&text);
        paths.extend(scan.paths);
        identifiers.extend(scan.identifiers);
    }

    let mut hints = BTreeSet::new();
    let mut ambiguous = BTreeSet::new();
    let mut placed = BTreeMap::<(String, String), ResolvedMentions>::new();
    for (order, candidate) in paths.iter().enumerate() {
        match resolver.resolve_path(candidate) {
            MentionResolution::Placed(reference) => {
                absorb_reference(&mut placed, reference, order);
            }
            MentionResolution::Ambiguous { matches } => {
                hints.insert(candidate.hint_text());
                ambiguous.insert(AmbiguousMention {
                    // The leading separator is kept so the spelling round-trips through the
                    // scanner: a re-lookup of a host-absolute path must take the absolute branch,
                    // or a recorded ambiguity would be re-checked against the wrong question.
                    spelling: if candidate.absolute {
                        format!("/{}", candidate.path)
                    } else {
                        candidate.path.clone()
                    },
                    channel: MentionChannel::Path,
                    matches,
                });
            }
            MentionResolution::Unplaced => {
                hints.insert(candidate.hint_text());
            }
        }
    }
    // The type-name channel opens only for a Claim no path spelling could place. A symbol spelling
    // is already an identifier hint, so nothing is added to `hints` here: an unplaced type name
    // stays exactly as searchable as it was.
    if placed.is_empty() {
        let symbols = claim_symbol_mentions(statement, rationale, evidence_summaries);
        for (offset, symbol) in symbols.iter().enumerate() {
            if placed.len() == MAX_SYMBOL_DERIVED_REFERENCES_PER_CLAIM {
                break;
            }
            match resolver.resolve_symbol(symbol) {
                MentionResolution::Placed(reference) => {
                    absorb_reference(&mut placed, reference, paths.len() + offset);
                }
                MentionResolution::Ambiguous { matches } => {
                    ambiguous.insert(AmbiguousMention {
                        spelling: symbol.clone(),
                        channel: MentionChannel::Symbol,
                        matches,
                    });
                }
                MentionResolution::Unplaced => {}
            }
        }
    }
    hints.extend(identifiers);

    let engineering_references = placed
        .values()
        .map(|entry| {
            let reference = &entry.reference;
            let mut limitations = vec![DERIVED_BY_SERVER_LIMITATION.to_owned()];
            if reference.derived_from_unique_basename {
                limitations.push(UNIQUE_BASENAME_LIMITATION.to_owned());
            }
            if reference.derived_from_symbol_mention {
                limitations.push(SYMBOL_MENTION_LIMITATION.to_owned());
            }
            EngineeringReferenceDraft {
                repository_id: reference.repository_id.clone(),
                artifact_kind: ArtifactKind::File,
                relation: relation_for_kind(kind),
                locator: ArtifactLocator::File {
                    path: reference.path.clone(),
                },
                supports: statement.to_owned(),
                limitations,
            }
        })
        .collect::<Vec<_>>();

    // Most mentions wins; a tie goes to whichever spelling the Agent wrote down first, which is
    // the one the Claim is actually about. Deterministic because extraction order is deterministic.
    let topic_key_hint = placed
        .iter()
        .map(|((repository, path), entry)| {
            (
                entry.mentions,
                entry.first_order,
                format!("{}:{repository}:{path}", kind_name(kind)),
            )
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)))
        .map(|(_, _, topic)| topic);

    ClaimReferenceDerivation {
        engineering_references,
        topic_key_hint,
        unresolved_hints: hints.into_iter().collect(),
        ambiguous_mentions: ambiguous.into_iter().collect(),
    }
}

/// Records one placed coordinate, merging a repeat mention of a coordinate already seen.
///
/// The weakest reading wins on merge: a coordinate the Claim also wrote as a literal path is no
/// longer a basename inference and no longer a type-name inference, so both marks clear.
fn absorb_reference(
    placed: &mut BTreeMap<(String, String), ResolvedMentions>,
    reference: ResolvedReference,
    order: usize,
) {
    let key = (
        reference.repository_id.to_string(),
        reference.path.as_str().to_owned(),
    );
    placed
        .entry(key)
        .and_modify(|entry| {
            entry.mentions += 1;
            entry.reference.derived_from_unique_basename &= reference.derived_from_unique_basename;
            entry.reference.derived_from_symbol_mention &= reference.derived_from_symbol_mention;
        })
        .or_insert(ResolvedMentions {
            reference,
            mentions: 1,
            first_order: order,
        });
}

/// Marks a `topic_key` derived from the Claim's own prose instead of a placed coordinate.
///
/// It occupies the segment a resolved topic key spends on the Repository, so the two shapes stay
/// distinguishable at a glance and can never collide.
pub const TEXT_TOPIC_SEGMENT: &str = "text";

/// The `topic_key` one Candidate draft carries for a Claim.
///
/// Order of preference: the hint the persisted Claim already holds — either the Agent's own
/// optional `topic_key_hint` or the resolved coordinate [`derive_claim_references`] wrote back
/// onto it — and otherwise the dominant file spelling the Claim's own prose names. `None` only
/// when the Claim names no coordinate at all, which is the one case that still leaves the topic
/// unclassified.
///
/// Pure in its inputs and computed from persisted Claim text alone, so Candidate Build and every
/// later reconstruction of the same Candidate agree on one answer.
#[must_use]
pub fn claim_topic_key(
    kind: ContextKind,
    hint: Option<&str>,
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
) -> Option<String> {
    if let Some(hint) = hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        return Some(hint.to_owned());
    }
    text_topic_key(kind, statement, rationale, evidence_summaries)
}

/// The topic of a Claim no checkout could place, read from the file spellings it wrote down.
///
/// Only path-shaped spellings qualify. An identifier that merely appears in a sentence is not a
/// coordinate — the same reason `token_alias` groups are seeded from path stems alone — and
/// promoting one to a topic key would make every Claim that happens to mention `canShow` share a
/// topic with every other. The spelling is lower-cased because this key is compared, not opened.
fn text_topic_key(
    kind: ContextKind,
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
) -> Option<String> {
    let mut ranked = BTreeMap::<String, (usize, usize)>::new();
    for (order, candidate) in claim_texts(statement, rationale, evidence_summaries)
        .iter()
        .flat_map(|text| scan_text(text).paths)
        .enumerate()
    {
        let stem = candidate.stem().to_lowercase();
        if stem.is_empty() {
            continue;
        }
        let entry = ranked.entry(stem).or_insert((0, order));
        entry.0 += 1;
    }
    // Most mentions wins; a tie goes to the spelling the Agent wrote first, exactly as the
    // resolved topic key above is ranked.
    ranked
        .into_iter()
        .max_by(|left, right| {
            left.1
                .0
                .cmp(&right.1.0)
                .then_with(|| right.1.1.cmp(&left.1.1))
        })
        .map(|(stem, _)| format!("{}:{TEXT_TOPIC_SEGMENT}:{stem}", kind_name(kind)))
}

fn scan_text(text: &str) -> TextScan {
    hints::scan_text(text)
}

fn claim_texts(statement: &str, rationale: &str, evidence_summaries: &[&str]) -> Vec<String> {
    let mut texts = vec![statement.to_owned(), rationale.to_owned()];
    texts.extend(evidence_summaries.iter().map(|value| (*value).to_owned()));
    texts
}

/// One read-only, timeout-bounded view of the tracked paths matching a Build's Claim basenames.
///
/// Construction never fails: an unavailable checkout, a missing or slow `git`, and a non-zero exit
/// all produce an empty index, which resolves every spelling to an unresolved hint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CheckoutReferenceResolver {
    repository_id: Option<RepositoryId>,
    tracked: BTreeSet<String>,
    by_basename: BTreeMap<String, BTreeSet<String>>,
    by_stem: BTreeMap<String, BTreeSet<String>>,
}

impl CheckoutReferenceResolver {
    /// Runs one bounded `git ls-files` for every distinct basename and type name a Build named.
    ///
    /// Candidate Build calls this at most once per Build, and only while the derivation for that
    /// Episode is still pending, so no Checkpoint ACK ever pays for a process spawn.
    ///
    /// Paths hold the whole pattern budget first and type names take what is left, so widening the
    /// query with a second channel can never cost a Build a path it used to place.
    #[must_use]
    pub fn from_checkout(
        repository_id: RepositoryId,
        checkout_path: &Path,
        candidates: &[PathCandidate],
        symbols: &[String],
    ) -> Self {
        let basenames = candidates
            .iter()
            .map(|candidate| candidate.basename.clone())
            .collect::<BTreeSet<_>>();
        if basenames.len() > MAX_GIT_QUERY_PATTERNS {
            return Self::default();
        }
        let symbols = symbols
            .iter()
            .filter(|symbol| hints::is_symbol_mention(symbol))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(MAX_GIT_QUERY_PATTERNS - basenames.len())
            .collect::<Vec<_>>();
        if basenames.is_empty() && symbols.is_empty() {
            return Self::default();
        }
        let patterns = basenames
            .iter()
            .flat_map(|basename| [basename.clone(), format!("*/{basename}")])
            .chain(
                symbols
                    .iter()
                    .flat_map(|symbol| [format!("{symbol}.*"), format!("*/{symbol}.*")]),
            )
            .collect::<Vec<_>>();
        let Some(paths) = tracked_paths(checkout_path, &patterns) else {
            return Self::default();
        };
        Self::from_tracked_paths(repository_id, paths)
    }

    /// Builds the same index from an already known tracked-path list.
    #[must_use]
    pub fn from_tracked_paths(
        repository_id: RepositoryId,
        paths: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut tracked = BTreeSet::new();
        let mut by_basename = BTreeMap::<String, BTreeSet<String>>::new();
        let mut by_stem = BTreeMap::<String, BTreeSet<String>>::new();
        for path in paths {
            let Some(basename) = path.rsplit('/').next().map(str::to_owned) else {
                continue;
            };
            if let Some((stem, _)) = basename.rsplit_once('.')
                && !stem.is_empty()
            {
                by_stem
                    .entry(stem.to_owned())
                    .or_default()
                    .insert(path.clone());
            }
            by_basename
                .entry(basename)
                .or_default()
                .insert(path.clone());
            tracked.insert(path);
        }
        Self {
            repository_id: Some(repository_id),
            tracked,
            by_basename,
            by_stem,
        }
    }

    /// Places one spelling, preferring an exact tracked path over a unique basename.
    ///
    /// Kept alongside [`CheckoutReferenceResolver::resolve_path`] for callers that only need to
    /// know whether a spelling landed; an ambiguous spelling reads as `None` here, which is the
    /// answer this method always gave.
    #[must_use]
    pub fn resolve(&self, candidate: &PathCandidate) -> Option<ResolvedReference> {
        match self.resolve_path(candidate) {
            MentionResolution::Placed(reference) => Some(reference),
            MentionResolution::Ambiguous { .. } | MentionResolution::Unplaced => None,
        }
    }

    /// Places one path spelling, distinguishing "several files answer to it" from "none does".
    #[must_use]
    pub fn resolve_path(&self, candidate: &PathCandidate) -> MentionResolution {
        let Some(repository_id) = self.repository_id.clone() else {
            return MentionResolution::Unplaced;
        };
        if candidate.absolute {
            return self.resolve_absolute(repository_id, &candidate.path);
        }
        if self.tracked.contains(&candidate.path)
            && let Ok(path) = RepoRelativePath::new(candidate.path.clone())
        {
            return MentionResolution::Placed(ResolvedReference {
                repository_id,
                path,
                derived_from_unique_basename: false,
                derived_from_symbol_mention: false,
            });
        }
        let Some(matches) = self.by_basename.get(&candidate.basename) else {
            return MentionResolution::Unplaced;
        };
        Self::single(matches, |path| ResolvedReference {
            repository_id,
            path,
            derived_from_unique_basename: true,
            derived_from_symbol_mention: false,
        })
    }

    /// Places one type name by the tracked file that is named after it.
    ///
    /// The comparison is against the extension-free basename and is case sensitive, because a
    /// type and the file it lives in are spelled the same way on purpose. A case-insensitive
    /// filesystem can make `git ls-files` answer `mapsceneruntime.kt` to the pattern
    /// `MapSceneRuntime.*`; comparing case exactly keeps the answer the same on every machine.
    #[must_use]
    pub fn resolve_symbol(&self, symbol: &str) -> MentionResolution {
        let Some(repository_id) = self.repository_id.clone() else {
            return MentionResolution::Unplaced;
        };
        let Some(matches) = self.by_stem.get(symbol) else {
            return MentionResolution::Unplaced;
        };
        Self::single(matches, |path| ResolvedReference {
            repository_id,
            path,
            derived_from_unique_basename: false,
            derived_from_symbol_mention: true,
        })
    }

    /// Turns a match set into one answer: one placeable path, or the ambiguity it really is.
    fn single(
        matches: &BTreeSet<String>,
        place: impl FnOnce(RepoRelativePath) -> ResolvedReference,
    ) -> MentionResolution {
        if matches.len() > 1 {
            return MentionResolution::Ambiguous {
                matches: matches.len(),
            };
        }
        let Some(single) = matches.iter().next() else {
            return MentionResolution::Unplaced;
        };
        // A tracked path Git itself printed is repository-relative by construction; a rejection
        // here would mean Git answered something this module cannot name, which is not ambiguity.
        RepoRelativePath::new(single.clone()).map_or(MentionResolution::Unplaced, |path| {
            MentionResolution::Placed(place(path))
        })
    }

    /// Places a host-absolute spelling only when this checkout actually contains that exact file.
    ///
    /// The unique-basename inference is deliberately unavailable here. A spelling the Agent wrote
    /// as `/private/tmp/notes.md` names a file on the machine; a checkout that happens to track
    /// exactly one `notes.md` is a different file with the same name, and placing one as the other
    /// would fabricate a coordinate. The only sound reading is that the tracked path is the tail
    /// of what the Agent wrote — `/Users/me/work/TikTok/components/x/Foo.kt` ends with
    /// `components/x/Foo.kt` — which makes this an exact match, not an inference.
    ///
    /// This became load-bearing when `md` entered `PATH_EXTENSIONS`: scratch notes and Skill files
    /// under `/private/tmp` and `~/.agents` are the single most common absolute spelling in the
    /// real Sessions, and basenames like `workflow.md` or `protocol.md` are exactly the ones a
    /// repository is also likely to track.
    fn resolve_absolute(&self, repository_id: RepositoryId, spelling: &str) -> MentionResolution {
        let mut matched = self.tracked.iter().filter(|tracked| {
            spelling.len() > tracked.len()
                && spelling.ends_with(tracked.as_str())
                && spelling.as_bytes()[spelling.len() - tracked.len() - 1] == b'/'
        });
        let Some(single) = matched.next() else {
            return MentionResolution::Unplaced;
        };
        if matched.next().is_some() {
            return MentionResolution::Ambiguous { matches: 2 };
        }
        RepoRelativePath::new(single.clone()).map_or(MentionResolution::Unplaced, |path| {
            MentionResolution::Placed(ResolvedReference {
                repository_id,
                path,
                derived_from_unique_basename: false,
                derived_from_symbol_mention: false,
            })
        })
    }
}

/// Identity of one memoized `git ls-files` answer.
///
/// `git ls-files` reads the checkout's Git index, so the index file's identity is what actually
/// decides the answer; the resolved `HEAD` revision is carried alongside it so a memo hit is
/// legible as "this checkout, at this revision, for these basenames".
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TrackedPathKey {
    git_dir: PathBuf,
    head: String,
    index_state: String,
    patterns: Vec<String>,
}

/// Bounded least-recently-used memo of tracked-path answers for this process.
#[derive(Debug, Default)]
struct TrackedPathCache {
    order: VecDeque<TrackedPathKey>,
    entries: HashMap<TrackedPathKey, Vec<String>>,
}

fn tracked_path_cache() -> MutexGuard<'static, TrackedPathCache> {
    static CACHE: OnceLock<Mutex<TrackedPathCache>> = OnceLock::new();
    CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Answers one tracked-path query, reusing the answer while the checkout revision is unchanged.
///
/// A checkout whose revision cannot be read without spawning a process is never memoized: it runs
/// the query exactly as before. A failed or timed-out query is never memoized either, so the next
/// Candidate Build retries it.
fn tracked_paths(checkout_path: &Path, patterns: &[String]) -> Option<Vec<String>> {
    let Some(key) = tracked_path_key(checkout_path, patterns) else {
        return read_tracked_paths(checkout_path, patterns);
    };
    if let Some(memoized) = read_memoized_tracked_paths(&key) {
        return Some(memoized);
    }
    let paths = read_tracked_paths(checkout_path, patterns)?;
    write_memoized_tracked_paths(key, paths.clone());
    Some(paths)
}

fn read_memoized_tracked_paths(key: &TrackedPathKey) -> Option<Vec<String>> {
    let mut cache = tracked_path_cache();
    let paths = cache.entries.get(key).cloned()?;
    if let Some(position) = cache.order.iter().position(|entry| entry == key) {
        cache.order.remove(position);
    }
    cache.order.push_back(key.clone());
    Some(paths)
}

fn write_memoized_tracked_paths(key: TrackedPathKey, paths: Vec<String>) {
    let mut cache = tracked_path_cache();
    if cache.entries.insert(key.clone(), paths).is_none() {
        cache.order.push_back(key);
    }
    while cache.order.len() > MAX_TRACKED_PATH_CACHE_ENTRIES {
        let Some(evicted) = cache.order.pop_front() else {
            break;
        };
        cache.entries.remove(&evicted);
    }
}

/// Reads the checkout revision from Git's own files, never from a subprocess.
fn tracked_path_key(checkout_path: &Path, patterns: &[String]) -> Option<TrackedPathKey> {
    let git_dir = resolve_git_dir(checkout_path)?;
    let head = head_revision(&git_dir)?;
    let index_state = git_index_state(&git_dir)?;
    Some(TrackedPathKey {
        git_dir,
        head,
        index_state,
        patterns: patterns.to_vec(),
    })
}

/// Resolves `<checkout>/.git`, following the `gitdir:` pointer a worktree or submodule leaves.
fn resolve_git_dir(checkout_path: &Path) -> Option<PathBuf> {
    let dot_git = checkout_path.join(".git");
    let metadata = fs::metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    let pointer = fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        checkout_path.join(target)
    })
}

/// Resolves `HEAD` to a commit object ID, falling back to the symbolic name it points at.
fn head_revision(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim().to_owned();
    if head.is_empty() {
        return None;
    }
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return Some(head);
    };
    let mut bases = vec![git_dir.to_path_buf()];
    if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
        let common = common.trim();
        if !common.is_empty() {
            let common = Path::new(common);
            bases.push(if common.is_absolute() {
                common.to_path_buf()
            } else {
                git_dir.join(common)
            });
        }
    }
    for base in &bases {
        if let Ok(loose) = fs::read_to_string(base.join(reference)) {
            let oid = loose.trim();
            if !oid.is_empty() {
                return Some(oid.to_owned());
            }
        }
        if let Some(oid) = packed_reference(&base.join("packed-refs"), reference) {
            return Some(oid);
        }
    }
    // An unborn or unreadable ref is still a deterministic key: the Git index identity below is
    // what makes the memo sound, and that file changes whenever the checkout does.
    Some(head)
}

fn packed_reference(packed_refs: &Path, reference: &str) -> Option<String> {
    let packed = fs::read_to_string(packed_refs).ok()?;
    packed.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('^') {
            return None;
        }
        let (oid, name) = line.split_once(' ')?;
        (name.trim() == reference).then(|| oid.trim().to_owned())
    })
}

/// Identity of the Git index file `git ls-files` reads.
fn git_index_state(git_dir: &Path) -> Option<String> {
    let metadata = fs::metadata(git_dir.join("index")).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(format!("{}:{}", metadata.len(), modified.as_nanos()))
}

/// Runs `git ls-files` read-only under [`GIT_QUERY_TIMEOUT`]; any failure returns `None`.
fn read_tracked_paths(checkout_path: &Path, patterns: &[String]) -> Option<Vec<String>> {
    if !checkout_path.is_dir() {
        return None;
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(checkout_path)
        .args(["ls-files", "--full-name", "-z", "--"])
        .args(patterns)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).ok().map(|_| buffer)
    });
    let deadline = Instant::now() + GIT_QUERY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let output = reader.join().ok().flatten();
    let (Some(status), Some(output)) = (status, output) else {
        return None;
    };
    if !status.success() {
        return None;
    }
    let text = String::from_utf8(output).ok()?;
    Some(
        text.split('\0')
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Convenience wrapper for callers that hold a Repository identity and checkout path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointReferenceScope {
    pub repository_id: RepositoryId,
    pub checkout_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> TextScan {
        scan_text(text)
    }

    #[test]
    fn an_agent_topic_hint_is_never_overruled_by_the_prose_fallback() {
        assert_eq!(
            claim_topic_key(
                ContextKind::Decision,
                Some("search/result-visibility"),
                "ProductAnchorAssem.kt:202 returns early",
                "",
                &[],
            )
            .as_deref(),
            Some("search/result-visibility")
        );
    }

    #[test]
    fn an_unplaced_claim_takes_its_topic_from_the_file_it_talks_about_most() {
        assert_eq!(
            claim_topic_key(
                ContextKind::Issue,
                None,
                "PoiEntranceAssem.kt:118 registers before the null check",
                "CommentBottomBarManager.kt:96 is skipped",
                &["PoiEntranceAssem.kt:140 preempts the container"],
            )
            .as_deref(),
            Some("issue:text:poientranceassem")
        );
    }

    #[test]
    fn a_tie_goes_to_the_spelling_the_agent_wrote_first() {
        assert_eq!(
            claim_topic_key(
                ContextKind::Discovery,
                None,
                "CommentBottomBarManager.kt:96 falls back",
                "PoiEntranceAssem.kt:118 registers early",
                &[],
            )
            .as_deref(),
            Some("discovery:text:commentbottombarmanager")
        );
    }

    #[test]
    fn a_claim_that_names_no_file_keeps_its_topic_unclassified() {
        assert_eq!(
            claim_topic_key(
                ContextKind::Decision,
                None,
                "the ILiveEntryService has no implementation",
                "nothing here is a file",
                &["BUILD SUCCESSFUL"],
            ),
            None
        );
    }

    #[test]
    fn extracts_path_with_line_span() {
        let scan = scan("see ProductAnchorAssem.kt:202 for the early return");
        assert_eq!(scan.paths.len(), 1);
        assert_eq!(scan.paths[0].path, "ProductAnchorAssem.kt");
        assert_eq!(scan.paths[0].basename, "ProductAnchorAssem.kt");
        assert!(!scan.paths[0].has_directory);
        assert_eq!(scan.paths[0].line_span.as_deref(), Some(":202"));
    }

    #[test]
    fn extracts_directory_path_and_range_span() {
        let scan = scan("app/src/main/kotlin/poi/PoiEntranceAssem.kt:118-140 registers early");
        assert_eq!(
            scan.paths[0].path,
            "app/src/main/kotlin/poi/PoiEntranceAssem.kt"
        );
        assert!(scan.paths[0].has_directory);
        assert_eq!(scan.paths[0].line_span.as_deref(), Some(":118-140"));
    }

    #[test]
    fn ignores_unsupported_extension_and_all_caps_identifiers() {
        let scan = scan("build.log:512 reported BUILD SUCCESSFUL for POI_ENTRY and HEAD");
        assert!(scan.paths.is_empty());
        assert!(scan.identifiers.is_empty());
    }

    #[test]
    fn extracts_camel_case_and_snake_case_identifiers() {
        let scan = scan("ILiveEntryService.addParamsForLiveAnchor and enter_from_value");
        assert!(scan.identifiers.contains(&"ILiveEntryService".to_owned()));
        assert!(
            scan.identifiers
                .contains(&"addParamsForLiveAnchor".to_owned())
        );
        assert!(scan.identifiers.contains(&"enter_from_value".to_owned()));
    }

    #[test]
    fn unresolvable_resolver_keeps_every_spelling_as_hint() {
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "ProductAnchorAssem.kt:202 returns early",
            "ILiveEntryService has no implementation",
            &[],
            &ClaimResolver::nothing(),
        );
        assert!(derivation.engineering_references.is_empty());
        assert!(derivation.topic_key_hint.is_none());
        assert_eq!(
            derivation.unresolved_hints,
            vec![
                "ILiveEntryService".to_owned(),
                "ProductAnchorAssem.kt:202".to_owned(),
            ]
        );
    }

    #[test]
    fn unique_basename_resolves_and_records_the_limitation() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["app/src/main/kotlin/anchor/ProductAnchorAssem.kt".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "ProductAnchorAssem.kt:202 returns early",
            "no implementation",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert_eq!(derivation.engineering_references.len(), 1);
        let reference = &derivation.engineering_references[0];
        assert_eq!(reference.artifact_kind, ArtifactKind::File);
        assert_eq!(reference.relation, ReferenceRelation::DependsOn);
        assert_eq!(
            reference.limitations,
            vec![
                DERIVED_BY_SERVER_LIMITATION.to_owned(),
                UNIQUE_BASENAME_LIMITATION.to_owned(),
            ]
        );
        reference.validate().unwrap();
        assert!(derivation.unresolved_hints.is_empty());
    }

    #[test]
    fn ambiguous_basename_stays_unresolved() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["a/Manager.kt".to_owned(), "b/Manager.kt".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "Manager.kt:11 is wrong",
            "ambiguous",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert!(derivation.engineering_references.is_empty());
        assert_eq!(
            derivation.unresolved_hints,
            vec!["Manager.kt:11".to_owned()]
        );
    }

    #[test]
    fn exact_tracked_path_beats_basename_inference() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["a/Manager.kt".to_owned(), "b/Manager.kt".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Progress,
            "a/Manager.kt now guards the slot",
            "done",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert_eq!(derivation.engineering_references.len(), 1);
        assert_eq!(
            derivation.engineering_references[0].limitations,
            vec![DERIVED_BY_SERVER_LIMITATION.to_owned()]
        );
        assert_eq!(
            derivation.engineering_references[0].relation,
            ReferenceRelation::Implements
        );
    }

    #[test]
    fn topic_hint_prefers_the_most_mentioned_reference() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["one/Alpha.kt".to_owned(), "two/Beta.kt".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Discovery,
            "Alpha.kt:1 and Beta.kt:2 disagree",
            "Alpha.kt:9 is authoritative",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert_eq!(derivation.engineering_references.len(), 2);
        assert!(
            derivation
                .topic_key_hint
                .as_deref()
                .is_some_and(
                    |topic| topic.starts_with("discovery:") && topic.ends_with(":one/Alpha.kt")
                ),
            "unexpected topic {:?}",
            derivation.topic_key_hint
        );
    }

    #[test]
    fn topic_hint_ties_break_on_first_mention_not_alphabetical_order() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["poi/Zeta.kt".to_owned(), "comment/Alpha.kt".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "Zeta.kt:118 registers before Alpha.kt:96 can fall back",
            "the registration order is the defect",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert_eq!(derivation.engineering_references.len(), 2);
        assert!(
            derivation
                .topic_key_hint
                .as_deref()
                .is_some_and(|topic| topic.ends_with(":poi/Zeta.kt")),
            "the first written spelling wins the tie: {:?}",
            derivation.topic_key_hint
        );
    }

    /// The `md` extension makes this shape common: `01a08baf` wrote `/private/tmp/*.md` and
    /// `~/.agents/skills/**/workflow.md` dozens of times while working inside a checkout that also
    /// tracks Markdown. Those are different files that share a name.
    #[test]
    fn an_out_of_repository_absolute_spelling_never_borrows_a_tracked_basename() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["components/poi/docs/protocol.md".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Discovery,
            "/private/tmp/protocol.md captured the transcript",
            "scratch only",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert!(
            derivation.engineering_references.is_empty(),
            "{:?}",
            derivation.engineering_references
        );
        assert_eq!(
            derivation.unresolved_hints,
            vec!["private/tmp/protocol.md".to_owned()]
        );
    }

    #[test]
    fn an_absolute_spelling_of_a_tracked_file_still_places_as_an_exact_match() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["components/poi/docs/protocol.md".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Contract,
            "/Users/me/workspace/TikTok/components/poi/docs/protocol.md states the contract",
            "",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert_eq!(derivation.engineering_references.len(), 1);
        assert_eq!(
            derivation.engineering_references[0].locator.path().as_str(),
            "components/poi/docs/protocol.md"
        );
        assert_eq!(
            derivation.engineering_references[0].limitations,
            vec![DERIVED_BY_SERVER_LIMITATION.to_owned()],
            "a tail match is exact, not a basename inference"
        );
    }

    #[test]
    fn a_suffix_that_is_not_a_whole_path_component_is_not_a_tail_match() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["live/index.scss".to_owned()],
        );
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "/tmp/notlive/index.scss is a copy",
            "",
            &[],
            &ClaimResolver::paths(&|candidate| resolver.resolve_path(candidate)),
        );
        assert!(derivation.engineering_references.is_empty());
    }

    // -------------------------------------------------------------------------------------
    // The type-name channel
    // -------------------------------------------------------------------------------------

    /// Both channels of one `CheckoutReferenceResolver`, as a borrowed [`ClaimResolver`].
    ///
    /// A macro rather than a function because the two closures are temporaries: they must live in
    /// the caller's statement, not in a callee whose frame ends before the resolver is used.
    macro_rules! both_channels {
        ($checkout:expr) => {
            &ClaimResolver::new(&|candidate| $checkout.resolve_path(candidate), &|symbol| {
                $checkout.resolve_symbol(symbol)
            })
        };
    }

    /// The shape both real long Sessions actually wrote: a Claim that points at a type and never
    /// spells a path. Before this channel existed it derived nothing at all.
    #[test]
    fn a_claim_that_points_at_a_type_places_the_file_named_after_it() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            [
                "components/poi/map/engine/MapSceneRuntime.kt".to_owned(),
                "components/poi/map/camera/CameraController.kt".to_owned(),
            ],
        );
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "MapSceneRuntime.present() re-enters the layout pass",
            "CameraController.retargetEnvironment is called from inside it",
            &[],
            both_channels!(resolver),
        );
        let placed = derivation
            .engineering_references
            .iter()
            .map(|reference| reference.locator.path().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            placed,
            vec![
                "components/poi/map/camera/CameraController.kt".to_owned(),
                "components/poi/map/engine/MapSceneRuntime.kt".to_owned(),
            ]
        );
        for reference in &derivation.engineering_references {
            assert_eq!(
                reference.limitations,
                vec![
                    DERIVED_BY_SERVER_LIMITATION.to_owned(),
                    SYMBOL_MENTION_LIMITATION.to_owned(),
                ]
            );
            reference.validate().unwrap();
        }
        assert!(derivation.ambiguous_mentions.is_empty());
    }

    #[test]
    fn a_claim_that_already_placed_a_path_never_asks_by_type_name() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            [
                "poi/PoiEntranceAssem.kt".to_owned(),
                "poi/MapSceneRuntime.kt".to_owned(),
            ],
        );
        let derivation = derive_claim_references(
            ContextKind::Progress,
            "PoiEntranceAssem.kt:118 now guards the slot MapSceneRuntime asks for",
            "",
            &[],
            both_channels!(resolver),
        );
        assert_eq!(derivation.engineering_references.len(), 1);
        assert_eq!(
            derivation.engineering_references[0].locator.path().as_str(),
            "poi/PoiEntranceAssem.kt"
        );
    }

    #[test]
    fn the_type_name_channel_stops_at_its_ceiling() {
        let files = (0..6)
            .map(|index| format!("src/Alpha{index}Runtime.kt"))
            .collect::<Vec<_>>();
        let resolver =
            CheckoutReferenceResolver::from_tracked_paths(RepositoryId::new(), files.clone());
        let statement = files
            .iter()
            .map(|path| {
                path.trim_start_matches("src/")
                    .trim_end_matches(".kt")
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join(" then ");
        let derivation = derive_claim_references(
            ContextKind::Discovery,
            &statement,
            "",
            &[],
            both_channels!(resolver),
        );
        assert_eq!(
            derivation.engineering_references.len(),
            MAX_SYMBOL_DERIVED_REFERENCES_PER_CLAIM
        );
    }

    /// The whole point of the third resolution state: an ambiguity is written down instead of
    /// being silently indistinguishable from "this Claim names nothing".
    #[test]
    fn an_ambiguous_spelling_is_recorded_with_the_number_of_files_that_answered_it() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            [
                "a/Manager.kt".to_owned(),
                "b/Manager.kt".to_owned(),
                "c/MapSceneRuntime.kt".to_owned(),
                "d/MapSceneRuntime.swift".to_owned(),
            ],
        );
        let derivation = derive_claim_references(
            ContextKind::Issue,
            "Manager.kt:11 is wrong and MapSceneRuntime re-enters the layout pass",
            "",
            &[],
            both_channels!(resolver),
        );
        assert!(derivation.engineering_references.is_empty());
        assert_eq!(
            derivation.ambiguous_mentions,
            vec![
                AmbiguousMention {
                    spelling: "Manager.kt".to_owned(),
                    channel: MentionChannel::Path,
                    matches: 2,
                },
                AmbiguousMention {
                    spelling: "MapSceneRuntime".to_owned(),
                    channel: MentionChannel::Symbol,
                    matches: 2,
                },
            ]
        );
        assert!(
            derivation
                .unresolved_hints
                .contains(&"MapSceneRuntime".to_owned()),
            "an unplaced type name stays as searchable as it ever was: {:?}",
            derivation.unresolved_hints
        );
    }

    #[test]
    fn two_checkouts_that_both_place_one_spelling_are_an_ambiguity_not_a_silence() {
        let left = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["a/MapSceneRuntime.kt".to_owned()],
        );
        let right = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["b/MapSceneRuntime.kt".to_owned()],
        );
        assert_eq!(
            combine_resolutions([
                left.resolve_symbol("MapSceneRuntime"),
                right.resolve_symbol("MapSceneRuntime"),
            ]),
            MentionResolution::Ambiguous { matches: 2 }
        );
        assert_eq!(
            combine_resolutions([
                left.resolve_symbol("MapSceneRuntime"),
                right.resolve_symbol("Absent"),
            ]),
            left.resolve_symbol("MapSceneRuntime")
        );
        assert_eq!(
            combine_resolutions([MentionResolution::Unplaced]),
            MentionResolution::Unplaced
        );
    }

    /// A type name is matched against the file stem exactly, so a case-insensitive filesystem
    /// answering `mapsceneruntime.kt` to the pattern `MapSceneRuntime.*` changes nothing.
    #[test]
    fn a_type_name_matches_the_file_stem_case_for_case() {
        let resolver = CheckoutReferenceResolver::from_tracked_paths(
            RepositoryId::new(),
            ["src/mapsceneruntime.kt".to_owned()],
        );
        assert_eq!(
            resolver.resolve_symbol("MapSceneRuntime"),
            MentionResolution::Unplaced
        );
    }

    #[test]
    fn missing_checkout_never_panics_and_resolves_nothing() {
        let resolver = CheckoutReferenceResolver::from_checkout(
            RepositoryId::new(),
            Path::new("/nonexistent-shared-context-checkout"),
            &[PathCandidate {
                path: "Alpha.kt".to_owned(),
                basename: "Alpha.kt".to_owned(),
                has_directory: false,
                absolute: false,
                line_span: None,
            }],
            &["MapSceneRuntime".to_owned()],
        );
        assert_eq!(
            resolver.resolve_symbol("MapSceneRuntime"),
            MentionResolution::Unplaced
        );
        assert_eq!(
            resolver.resolve(&PathCandidate {
                path: "Alpha.kt".to_owned(),
                basename: "Alpha.kt".to_owned(),
                has_directory: false,
                absolute: false,
                line_span: None,
            }),
            None
        );
    }
}
