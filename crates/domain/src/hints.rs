//! Pure extraction of repository coordinates from free Agent prose.
//!
//! Agents write `ProductAnchorAssem.kt:202` and `ISearchLiveEntryService` inside `statement`,
//! `rationale` and Evidence text regardless of which contract field they are filling. Two
//! independent consumers need exactly the same reading of that prose:
//!
//! * `sctx-task-runtime` turns path spellings into Engineering References at Candidate Build.
//! * this crate's projection turns every spelling into searchable `hint_text` and alias groups,
//!   for the whole accepted history, not just the revisions a Build happened to touch.
//!
//! Keeping one implementation here means an existing Context recorded before server-side
//! derivation existed still gains identifier hints on the next `index rebuild`, and that the
//! Candidate analyzer compares the Candidate and its targets through the same reading.
//!
//! Everything in this module is a pure function of its input text; nothing consults a checkout.

use std::collections::BTreeSet;

use serde_json::Value;

/// File extensions that make a dotted run a repository path rather than prose.
///
/// The second row was added from two real Sessions' own prose (Codex `01a08baf`, Cursor
/// `4b2e9fb5`, cross-checked against `01a08017` and `7d8cbab1`): every one of these spellings was
/// written by an Agent about a file it had opened, and every one of them read as prose before —
/// `live-tag.lepus` did not even survive as an identifier, because none of `live`, `tag`, `lepus`
/// clears the identifier gate. Only extensions with corpus evidence are listed; `log`, `png`,
/// `txt`, `sh` and `jar` also occurred but name build output, assets, scratch files and artifacts
/// rather than the source a Claim is about, and `build.log:512` is pinned as a non-path by
/// [`tests::ignores_unsupported_extension_and_all_caps_identifiers`].
const PATH_EXTENSIONS: &[&str] = &[
    "kt", "java", "rs", "ts", "tsx", "js", "py", "go", "swift", "m", "mm", "kts", "gradle", "yaml",
    "yml", "toml", "json", "proto", "xml", //
    "md", "lepus", "ttml", "ttss", "scss", "cjs",
];

/// Words that look like identifiers but name no repository coordinate.
const IDENTIFIER_STOP_WORDS: &[&str] = &[
    "head",
    "readme",
    "license",
    "todo",
    "fixme",
    "changelog",
    "makefile",
    "dockerfile",
    "gitignore",
];

const MIN_CAMEL_CASE_LENGTH: usize = 4;
const MIN_SNAKE_CASE_LENGTH: usize = 6;

/// One textual spelling that looks like a repository file path.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct PathCandidate {
    /// Exact spelling as written by the Agent, without the trailing line span.
    pub path: String,
    /// Final path component of [`PathCandidate::path`].
    pub basename: String,
    /// True when the spelling already carries at least one directory component.
    pub has_directory: bool,
    /// True when the Agent wrote a host-absolute spelling (`/private/tmp/notes.md`, `~/x/a.md`).
    ///
    /// [`PathCandidate::path`] is always stored without its leading separator, so this flag is the
    /// only surviving record that the spelling named a location on the machine rather than a
    /// coordinate inside a repository. A resolver must not place such a spelling by its basename:
    /// `/private/tmp/notes.md` and a checkout's `docs/notes.md` are different files that happen to
    /// share a name.
    pub absolute: bool,
    /// `:120` or `:120-140` as written, kept for the hint text only.
    pub line_span: Option<String>,
}

impl PathCandidate {
    /// The spelling as it is preserved in retrieval text, line span included.
    #[must_use]
    pub fn hint_text(&self) -> String {
        match &self.line_span {
            Some(span) => format!("{}{span}", self.path),
            None => self.path.clone(),
        }
    }

    /// Basename without its extension, the form a human types when searching.
    #[must_use]
    pub fn stem(&self) -> &str {
        self.basename
            .rsplit_once('.')
            .map_or(self.basename.as_str(), |(stem, _)| stem)
    }
}

/// Complete reading of one text: every path spelling and every identifier spelling.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextScan {
    pub paths: Vec<PathCandidate>,
    pub identifiers: Vec<String>,
}

impl TextScan {
    fn absorb(&mut self, other: Self) {
        self.paths.extend(other.paths);
        self.identifiers.extend(other.identifiers);
    }
}

/// Splits one text into maximal ASCII path-shaped runs and classifies each run.
#[must_use]
pub fn scan_text(text: &str) -> TextScan {
    let characters = text.chars().collect::<Vec<_>>();
    let mut scan = TextScan::default();
    let mut index = 0;
    while index < characters.len() {
        if !is_run_character(characters[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < characters.len() && is_run_character(characters[index]) {
            index += 1;
        }
        let run = characters[start..index].iter().collect::<String>();
        let line_span = read_line_span(&characters, index);
        if let Some(span) = &line_span {
            index += span.chars().count();
        }
        classify_run(&run, line_span, &mut scan);
    }
    scan
}

/// Reads several texts as one, preserving the order in which spellings were written.
#[must_use]
pub fn scan_texts<'a>(texts: impl IntoIterator<Item = &'a str>) -> TextScan {
    let mut scan = TextScan::default();
    for text in texts {
        scan.absorb(scan_text(text));
    }
    scan
}

/// Every searchable hint term one body of Claim prose contributes.
///
/// The terms are the identifier spellings plus, for every path spelling, its extension-free
/// basename and — when the Agent wrote a directory — the full path. The line span is deliberately
/// dropped: `:202` is not a retrieval term, and the exact spelling with its span stays available
/// through the Claim's own unresolved hints.
#[must_use]
pub fn derived_hint_terms<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let scan = scan_texts(texts);
    let mut terms = BTreeSet::new();
    for path in &scan.paths {
        terms.insert(path.basename.clone());
        let stem = path.stem();
        if !stem.is_empty() {
            terms.insert(stem.to_owned());
        }
        if path.has_directory {
            terms.insert(path.path.clone());
        }
    }
    terms.extend(scan.identifiers);
    terms.into_iter().collect()
}

/// Extension-free basenames of every path spelling in one body of prose.
///
/// These are the file-backed coordinates: an Agent writing `SearchProductAnchorAssem.kt:202` named
/// a file, whereas an Agent writing `canShow` in a sentence merely used a word that happens to be
/// spelled like code. Only the first kind is a stable enough handle to seed a retrieval alias.
#[must_use]
pub fn derived_path_stems<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let scan = scan_texts(texts);
    scan.paths
        .iter()
        .map(|path| path.stem().to_owned())
        .filter(|stem| !stem.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The identifier-shaped terms of one body of prose, deduplicated and normalized to lower case.
///
/// This is the comparison key the Candidate analyzer intersects: `SearchProductAnchorAssem` in an
/// English Claim and the same spelling inside a Chinese Context revision must compare equal, and
/// case is the only difference the two spellings are allowed to have.
#[must_use]
pub fn normalized_identifiers<'a>(texts: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    let scan = scan_texts(texts);
    let mut identifiers = scan
        .identifiers
        .into_iter()
        .map(|identifier| identifier.to_lowercase())
        .collect::<BTreeSet<_>>();
    for path in &scan.paths {
        let stem = path.stem();
        if identifier_candidate(stem).is_some() {
            identifiers.insert(stem.to_lowercase());
        }
    }
    identifiers
}

/// Collects every string leaf of one JSON document, so structured Evidence content is read as
/// prose without its keys leaking into the identifier set.
pub fn json_string_leaves(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                json_string_leaves(item, out);
            }
        }
        Value::Object(fields) => {
            for field in fields.values() {
                json_string_leaves(field, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

const fn is_run_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '/' | '-')
}

/// Reads a trailing `:120` or `:120-140` immediately after one run.
fn read_line_span(characters: &[char], from: usize) -> Option<String> {
    if characters.get(from) != Some(&':') {
        return None;
    }
    let mut cursor = from + 1;
    let digits_start = cursor;
    while characters.get(cursor).is_some_and(char::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == digits_start {
        return None;
    }
    if characters.get(cursor) == Some(&'-') {
        let mut tail = cursor + 1;
        while characters.get(tail).is_some_and(char::is_ascii_digit) {
            tail += 1;
        }
        if tail > cursor + 1 {
            cursor = tail;
        }
    }
    Some(characters[from..cursor].iter().collect())
}

fn classify_run(run: &str, line_span: Option<String>, scan: &mut TextScan) {
    // `~/x/a.md` reaches here as `/x/a.md`: `~` is not a run character, so a home-relative
    // spelling is host-absolute for this purpose too, which is exactly how it should be read.
    let absolute = run.starts_with('/');
    let trimmed = run.trim_matches(|character| matches!(character, '.' | '-' | '/'));
    if trimmed.is_empty() {
        return;
    }
    if let Some(candidate) = path_candidate(trimmed, absolute, line_span) {
        scan.paths.push(candidate);
        return;
    }
    for segment in trimmed.split(['.', '/', '-']) {
        if let Some(identifier) = identifier_candidate(segment) {
            scan.identifiers.push(identifier);
        }
    }
}

fn path_candidate(value: &str, absolute: bool, line_span: Option<String>) -> Option<PathCandidate> {
    let extension = value.rsplit_once('.')?.1;
    if !PATH_EXTENSIONS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(extension))
    {
        return None;
    }
    let basename = value.rsplit('/').next().unwrap_or(value);
    if basename.is_empty() || basename.starts_with('.') {
        return None;
    }
    Some(PathCandidate {
        path: value.to_owned(),
        basename: basename.to_owned(),
        has_directory: value.contains('/'),
        absolute,
        line_span,
    })
}

fn identifier_candidate(segment: &str) -> Option<String> {
    if segment.is_empty()
        || !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    if segment
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    if IDENTIFIER_STOP_WORDS.contains(&segment.to_ascii_lowercase().as_str()) {
        return None;
    }
    if segment.contains('_') {
        return (segment.len() >= MIN_SNAKE_CASE_LENGTH).then(|| segment.to_owned());
    }
    if segment.len() < MIN_CAMEL_CASE_LENGTH
        || !segment.starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return None;
    }
    (camel_case_segments(segment) >= 2).then(|| segment.to_owned())
}

/// Counts CamelCase segments the way `ILiveEntryService` splits into `I|Live|Entry|Service`.
#[must_use]
pub fn camel_case_segments(value: &str) -> usize {
    let characters = value.chars().collect::<Vec<_>>();
    let mut segments = 1;
    for index in 1..characters.len() {
        let current = characters[index];
        let previous = characters[index - 1];
        if !current.is_ascii_uppercase() {
            continue;
        }
        let starts_after_lower = !previous.is_ascii_uppercase();
        let starts_acronym_tail = previous.is_ascii_uppercase()
            && characters
                .get(index + 1)
                .is_some_and(char::is_ascii_lowercase);
        if starts_after_lower || starts_acronym_tail {
            segments += 1;
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::{
        PathCandidate, camel_case_segments, derived_hint_terms, normalized_identifiers, scan_text,
    };

    #[test]
    fn extracts_path_with_line_span() {
        let scan = scan_text("see ProductAnchorAssem.kt:202 for the early return");
        assert_eq!(scan.paths.len(), 1);
        assert_eq!(scan.paths[0].path, "ProductAnchorAssem.kt");
        assert_eq!(scan.paths[0].basename, "ProductAnchorAssem.kt");
        assert!(!scan.paths[0].has_directory);
        assert_eq!(scan.paths[0].line_span.as_deref(), Some(":202"));
        assert_eq!(scan.paths[0].stem(), "ProductAnchorAssem");
        assert_eq!(scan.paths[0].hint_text(), "ProductAnchorAssem.kt:202");
    }

    #[test]
    fn extracts_directory_path_and_range_span() {
        let scan = scan_text("app/src/main/kotlin/poi/PoiEntranceAssem.kt:118-140 registers early");
        assert_eq!(
            scan.paths[0].path,
            "app/src/main/kotlin/poi/PoiEntranceAssem.kt"
        );
        assert!(scan.paths[0].has_directory);
        assert_eq!(scan.paths[0].line_span.as_deref(), Some(":118-140"));
    }

    #[test]
    fn ignores_unsupported_extension_and_all_caps_identifiers() {
        let scan = scan_text("build.log:512 reported BUILD SUCCESSFUL for POI_ENTRY and HEAD");
        assert!(scan.paths.is_empty());
        assert!(scan.identifiers.is_empty());
    }

    #[test]
    fn extracts_camel_case_and_snake_case_identifiers() {
        let scan = scan_text("ILiveEntryService.addParamsForLiveAnchor and enter_from_value");
        assert!(scan.identifiers.contains(&"ILiveEntryService".to_owned()));
        assert!(
            scan.identifiers
                .contains(&"addParamsForLiveAnchor".to_owned())
        );
        assert!(scan.identifiers.contains(&"enter_from_value".to_owned()));
    }

    #[test]
    fn camel_case_segment_counting_splits_acronym_prefix() {
        assert_eq!(camel_case_segments("ILiveEntryService"), 4);
        assert_eq!(camel_case_segments("canShow"), 2);
        assert_eq!(camel_case_segments("Dummy"), 1);
    }

    #[test]
    fn hint_terms_keep_the_basename_stem_and_directory_spelling() {
        let terms = derived_hint_terms([
            "app/src/main/kotlin/anchor/SearchProductAnchorAssem.kt:202 returns early",
            "ISearchLiveEntryService has no implementation",
        ]);
        assert!(terms.contains(&"SearchProductAnchorAssem".to_owned()));
        assert!(terms.contains(&"SearchProductAnchorAssem.kt".to_owned()));
        assert!(
            terms.contains(&"app/src/main/kotlin/anchor/SearchProductAnchorAssem.kt".to_owned())
        );
        assert!(terms.contains(&"ISearchLiveEntryService".to_owned()));
        assert!(
            terms.iter().all(|term| !term.contains(':')),
            "line spans are not retrieval terms: {terms:?}"
        );
    }

    #[test]
    fn normalized_identifiers_fold_case_and_include_path_stems() {
        let identifiers = normalized_identifiers([
            "当 ISearchLiveEntryService 无实现时 SearchProductAnchorAssem.kt:202 提前 return",
        ]);
        assert!(identifiers.contains("isearchliveentryservice"));
        assert!(identifiers.contains("searchproductanchorassem"));
    }

    #[test]
    fn path_candidate_defaults_stay_usable() {
        let candidate = PathCandidate {
            path: "Alpha.kt".to_owned(),
            basename: "Alpha.kt".to_owned(),
            has_directory: false,
            absolute: false,
            line_span: None,
        };
        assert_eq!(candidate.hint_text(), "Alpha.kt");
        assert_eq!(candidate.stem(), "Alpha");
    }

    /// Every spelling here is quoted from Codex `01a08baf` or Cursor `4b2e9fb5`; before this
    /// whitelist grew, each one was read as prose and contributed nothing at all.
    #[test]
    fn front_end_and_document_extensions_from_the_real_corpus_read_as_paths() {
        for (text, path, stem) in [
            (
                "subspaces/search/libs/search-components/src/text-badge/index.lepus imports it",
                "subspaces/search/libs/search-components/src/text-badge/index.lepus",
                "index",
            ),
            (
                "living-card/live-card-info.ttml renders the badge",
                "living-card/live-card-info.ttml",
                "live-card-info",
            ),
            (
                "user-avatar/index.ttss sizes it",
                "user-avatar/index.ttss",
                "index",
            ),
            (
                "search-atom-style/style/color.scss holds the token",
                "search-atom-style/style/color.scss",
                "color",
            ),
            (
                "scripts/live-tag.test.cjs covers it",
                "scripts/live-tag.test.cjs",
                "live-tag.test",
            ),
            (
                "poi/docs/x-ttk-map-view/protocol.md states the contract",
                "poi/docs/x-ttk-map-view/protocol.md",
                "protocol",
            ),
        ] {
            let scan = scan_text(text);
            assert_eq!(scan.paths.len(), 1, "{text}");
            assert_eq!(scan.paths[0].path, path);
            assert_eq!(scan.paths[0].stem(), stem);
            assert!(!scan.paths[0].absolute, "{text}");
        }
    }

    /// A bare `live-tag.lepus` used to contribute nothing: it is not a path without the
    /// extension, and none of `live`, `tag`, `lepus` clears the identifier gate on its own.
    #[test]
    fn a_lepus_spelling_now_contributes_the_terms_it_used_to_lose() {
        let terms = derived_hint_terms(["live-tag.lepus went stale"]);
        assert!(terms.contains(&"live-tag.lepus".to_owned()), "{terms:?}");
        assert!(terms.contains(&"live-tag".to_owned()), "{terms:?}");
    }

    #[test]
    fn a_host_absolute_spelling_is_marked_even_though_its_path_is_stored_relative() {
        let scan = scan_text("/private/tmp/x-ttk-map-view-notes.md holds the transcript");
        assert_eq!(scan.paths.len(), 1);
        assert!(scan.paths[0].absolute);
        assert_eq!(scan.paths[0].path, "private/tmp/x-ttk-map-view-notes.md");
        let home = scan_text("~/.agents/skills/shared-context/references/workflow.md says so");
        assert!(home.paths[0].absolute, "{:?}", home.paths);
        let inside = scan_text("components/business/poi/build.yaml lists it");
        assert!(!inside.paths[0].absolute);
    }
}
