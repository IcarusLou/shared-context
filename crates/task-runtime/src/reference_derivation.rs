//! Deterministic server-side derivation of engineering coordinates from Claim text.
//!
//! The public `task_checkpoint` contract stays four fields wide. Agents already write
//! `ProductAnchorAssem.kt:202` or `BottomBarProtocolManager` inside `statement`, `rationale` and
//! `evidence.summary`; this module extracts those spellings, hands each path candidate to an
//! injected resolver, and turns the resolved ones into [`EngineeringReferenceDraft`] values that
//! flow through the already existing Candidate Build provenance path.
//!
//! Two hard rules:
//!
//! * Extraction is a pure function of the Claim text. The only external input is the resolver.
//! * Resolution never fails a Checkpoint. An unavailable checkout, a missing `git` binary, a slow
//!   or failing query, and an ambiguous basename all resolve to "not derivable"; the spelling is
//!   preserved as an unresolved hint instead.

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
/// Upper bound on one read-only `git ls-files` lookup for one Candidate Build.
pub const GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on the number of distinct basenames one Candidate Build may ask Git about.
pub const MAX_GIT_QUERY_PATTERNS: usize = 128;
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
    /// True when `applicability.domains`/`platforms` were inherited from the Working Intent
    /// rather than authored on the Claim itself.
    pub applicability_inherited: bool,
}

/// Resolver that never places a spelling; the safe default for callers without a checkout.
///
/// Group-scoped and Disabled sessions, and every caller that cannot reach a Repository Catalog,
/// use this and still get unresolved hints.
#[must_use]
pub fn unresolvable(_candidate: &PathCandidate) -> Option<ResolvedReference> {
    None
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
/// The resolver is the only impure input; with [`unresolvable`] this function is total and pure.
#[must_use]
pub fn derive_claim_references(
    kind: ContextKind,
    statement: &str,
    rationale: &str,
    evidence_summaries: &[&str],
    resolve: &dyn Fn(&PathCandidate) -> Option<ResolvedReference>,
) -> ClaimReferenceDerivation {
    let mut paths = Vec::new();
    let mut identifiers = BTreeSet::new();
    for text in claim_texts(statement, rationale, evidence_summaries) {
        let scan = scan_text(&text);
        paths.extend(scan.paths);
        identifiers.extend(scan.identifiers);
    }

    let mut hints = BTreeSet::new();
    let mut resolved = BTreeMap::<(String, String), ResolvedMentions>::new();
    for (order, candidate) in paths.iter().enumerate() {
        match resolve(candidate) {
            Some(reference) => {
                let key = (
                    reference.repository_id.to_string(),
                    reference.path.as_str().to_owned(),
                );
                resolved
                    .entry(key)
                    .and_modify(|entry| {
                        entry.mentions += 1;
                        entry.reference.derived_from_unique_basename &=
                            reference.derived_from_unique_basename;
                    })
                    .or_insert(ResolvedMentions {
                        reference,
                        mentions: 1,
                        first_order: order,
                    });
            }
            None => {
                hints.insert(candidate.hint_text());
            }
        }
    }
    hints.extend(identifiers);

    let engineering_references = resolved
        .values()
        .map(|entry| {
            let reference = &entry.reference;
            let mut limitations = vec![DERIVED_BY_SERVER_LIMITATION.to_owned()];
            if reference.derived_from_unique_basename {
                limitations.push(UNIQUE_BASENAME_LIMITATION.to_owned());
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
    let topic_key_hint = resolved
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
    }
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
}

impl CheckoutReferenceResolver {
    /// Runs one bounded `git ls-files` for every distinct basename in `candidates`.
    ///
    /// Candidate Build calls this at most once per Build, and only while the derivation for that
    /// Episode is still pending, so no Checkpoint ACK ever pays for a process spawn.
    #[must_use]
    pub fn from_checkout(
        repository_id: RepositoryId,
        checkout_path: &Path,
        candidates: &[PathCandidate],
    ) -> Self {
        let basenames = candidates
            .iter()
            .map(|candidate| candidate.basename.clone())
            .collect::<BTreeSet<_>>();
        if basenames.is_empty() || basenames.len() > MAX_GIT_QUERY_PATTERNS {
            return Self::default();
        }
        let patterns = basenames
            .iter()
            .flat_map(|basename| [basename.clone(), format!("*/{basename}")])
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
        for path in paths {
            let Some(basename) = path.rsplit('/').next().map(str::to_owned) else {
                continue;
            };
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
        }
    }

    /// Places one spelling, preferring an exact tracked path over a unique basename.
    #[must_use]
    pub fn resolve(&self, candidate: &PathCandidate) -> Option<ResolvedReference> {
        let repository_id = self.repository_id.clone()?;
        if self.tracked.contains(&candidate.path)
            && let Ok(path) = RepoRelativePath::new(candidate.path.clone())
        {
            return Some(ResolvedReference {
                repository_id,
                path,
                derived_from_unique_basename: false,
            });
        }
        let matches = self.by_basename.get(&candidate.basename)?;
        let mut found = matches.iter();
        let single = found.next()?;
        if found.next().is_some() {
            return None;
        }
        let path = RepoRelativePath::new(single.clone()).ok()?;
        Some(ResolvedReference {
            repository_id,
            path,
            derived_from_unique_basename: true,
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
            &unresolvable,
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
            &|candidate| resolver.resolve(candidate),
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
            &|candidate| resolver.resolve(candidate),
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
            &|candidate| resolver.resolve(candidate),
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
            &|candidate| resolver.resolve(candidate),
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
            &|candidate| resolver.resolve(candidate),
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

    #[test]
    fn missing_checkout_never_panics_and_resolves_nothing() {
        let resolver = CheckoutReferenceResolver::from_checkout(
            RepositoryId::new(),
            Path::new("/nonexistent-shared-context-checkout"),
            &[PathCandidate {
                path: "Alpha.kt".to_owned(),
                basename: "Alpha.kt".to_owned(),
                has_directory: false,
                line_span: None,
            }],
        );
        assert_eq!(
            resolver.resolve(&PathCandidate {
                path: "Alpha.kt".to_owned(),
                basename: "Alpha.kt".to_owned(),
                has_directory: false,
                line_span: None,
            }),
            None
        );
    }
}
