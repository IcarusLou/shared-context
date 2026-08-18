use unicode_casefold::UnicodeCaseFold as _;
use unicode_normalization::UnicodeNormalization as _;

/// Produces the deterministic token stream stored in FTS5 and used for MATCH queries.
///
/// The pipeline applies NFKC, splits code/file identifiers, performs full Unicode case-folding,
/// and emits overlapping bigrams for contiguous Han text.
#[must_use]
pub fn search_tokens(text: &str) -> Vec<String> {
    tokenize(text, false)
}

fn tokenize(text: &str, include_compound_identifiers: bool) -> Vec<String> {
    let normalized = text.nfkc().collect::<String>();
    let characters = normalized.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut identifier = String::new();
    let mut identifier_parts = Vec::new();
    let mut han = Vec::new();

    for (index, character) in characters.iter().copied().enumerate() {
        if is_han(character) {
            flush_identifier(
                &mut identifier,
                &mut word,
                &mut identifier_parts,
                &mut tokens,
                include_compound_identifiers,
            );
            han.push(character);
            continue;
        }
        flush_han(&mut han, &mut tokens);
        if !character.is_alphanumeric() {
            flush_identifier(
                &mut identifier,
                &mut word,
                &mut identifier_parts,
                &mut tokens,
                include_compound_identifiers,
            );
            continue;
        }

        let previous = word.chars().last();
        let next = characters.get(index + 1).copied();
        let camel_boundary = previous.is_some_and(char::is_lowercase) && character.is_uppercase();
        let acronym_boundary = previous.is_some_and(char::is_uppercase)
            && character.is_uppercase()
            && next.is_some_and(char::is_lowercase);
        let digit_boundary =
            previous.is_some_and(char::is_numeric) != character.is_numeric() && previous.is_some();
        if camel_boundary || acronym_boundary || digit_boundary {
            flush_word(&mut word, &mut identifier_parts);
        }
        word.push(character);
        identifier.push(character);
    }
    flush_identifier(
        &mut identifier,
        &mut word,
        &mut identifier_parts,
        &mut tokens,
        include_compound_identifiers,
    );
    flush_han(&mut han, &mut tokens);
    tokens
}

/// Joins [`search_tokens`] into the whitespace-delimited representation indexed by FTS5.
#[must_use]
pub fn normalize_search_text(text: &str) -> String {
    tokenize(text, true).join(" ")
}

fn flush_word(word: &mut String, tokens: &mut Vec<String>) {
    if word.is_empty() {
        return;
    }
    let folded = word.as_str().case_fold().collect::<String>();
    if !folded.is_empty() {
        tokens.push(folded);
    }
    word.clear();
}

fn flush_identifier(
    identifier: &mut String,
    word: &mut String,
    parts: &mut Vec<String>,
    tokens: &mut Vec<String>,
    include_compound_identifiers: bool,
) {
    flush_word(word, parts);
    if identifier.is_empty() {
        return;
    }
    let folded = identifier.as_str().case_fold().collect::<String>();
    if include_compound_identifiers && parts.len() > 1 && !parts.contains(&folded) {
        tokens.push(folded);
    }
    tokens.append(parts);
    identifier.clear();
}

fn flush_han(han: &mut Vec<char>, tokens: &mut Vec<String>) {
    match han.as_slice() {
        [] => {}
        [character] => tokens.push(character.to_string()),
        characters => tokens.extend(
            characters
                .windows(2)
                .map(|pair| pair.iter().collect::<String>()),
        ),
    }
    han.clear();
}

fn is_han(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
            | 0x30000..=0x323AF
    )
}

#[cfg(test)]
mod tests {
    use super::{normalize_search_text, search_tokens};

    #[test]
    fn normalizes_unicode_and_full_case_fold() {
        assert_eq!(search_tokens("ＦＯＯ Straße"), ["foo", "strasse"]);
        assert_eq!(normalize_search_text("Cafe\u{301}"), "café");
    }

    #[test]
    fn splits_code_and_file_identifiers() {
        assert_eq!(
            search_tokens("HTTPServer search_result foo-bar.rs"),
            ["http", "server", "search", "result", "foo", "bar", "rs"]
        );
        assert!(normalize_search_text("HTTPServer").starts_with("httpserver "));
    }

    #[test]
    fn emits_overlapping_han_bigrams() {
        assert_eq!(search_tokens("中文检索"), ["中文", "文检", "检索"]);
        assert_eq!(search_tokens("中"), ["中"]);
    }
}
