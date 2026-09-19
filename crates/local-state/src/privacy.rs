use std::collections::BTreeSet;

use sctx_domain::{Error, ErrorKind, Result};

/// Maximum text size scanned by the default privacy gate.
const DEFAULT_MAX_SCAN_BYTES: usize = 4 * 1024 * 1024;

/// Stable Secret/PII categories. Diagnostics never contain matched text.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PrivacyFindingKind {
    AwsAccessKey,
    GithubToken,
    GitlabToken,
    GoogleApiKey,
    NpmToken,
    OpenAiKey,
    StripeLiveKey,
    SlackToken,
    Jwt,
    BearerCredential,
    AssignedCredential,
    PrivateKey,
    EmailAddress,
    PhoneNumber,
    ChinaNationalId,
}

impl PrivacyFindingKind {
    /// Stable identifier suitable for safe diagnostics and redaction markers.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AwsAccessKey => "aws_access_key",
            Self::GithubToken => "github_token",
            Self::GitlabToken => "gitlab_token",
            Self::GoogleApiKey => "google_api_key",
            Self::NpmToken => "npm_token",
            Self::OpenAiKey => "openai_key",
            Self::StripeLiveKey => "stripe_live_key",
            Self::SlackToken => "slack_token",
            Self::Jwt => "jwt",
            Self::BearerCredential => "bearer_credential",
            Self::AssignedCredential => "assigned_credential",
            Self::PrivateKey => "private_key",
            Self::EmailAddress => "email_address",
            Self::PhoneNumber => "phone_number",
            Self::ChinaNationalId => "china_national_id",
        }
    }

    /// Whether this finding represents a credential rather than personal data.
    #[must_use]
    pub const fn is_secret(self) -> bool {
        !matches!(
            self,
            Self::EmailAddress | Self::PhoneNumber | Self::ChinaNationalId
        )
    }
}

/// One finding represented only by category and byte offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivacyFinding {
    pub kind: PrivacyFindingKind,
    pub start: usize,
    pub end: usize,
}

/// Result of scanning untrusted UTF-8 text.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrivacyScan {
    findings: Vec<PrivacyFinding>,
}

impl PrivacyScan {
    /// Findings in deterministic source order.
    #[must_use]
    pub fn findings(&self) -> &[PrivacyFinding] {
        &self.findings
    }

    /// True when no supported Secret/PII signature was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Safe, deduplicated finding codes without source snippets.
    #[must_use]
    pub fn diagnostic_codes(&self) -> Vec<&'static str> {
        self.findings
            .iter()
            .map(|finding| finding.kind.code())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Redacted untrusted text plus safe diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactedText {
    pub text: String,
    pub finding_kinds: Vec<PrivacyFindingKind>,
}

/// Deterministic, offline Secret/PII scanner shared by local and Git boundaries.
#[derive(Clone, Debug)]
pub struct PrivacyScanner {
    max_scan_bytes: usize,
}

impl Default for PrivacyScanner {
    fn default() -> Self {
        Self {
            max_scan_bytes: DEFAULT_MAX_SCAN_BYTES,
        }
    }
}

impl PrivacyScanner {
    /// Creates a scanner that rejects unscanned text above `max_scan_bytes`.
    #[must_use]
    pub const fn with_max_scan_bytes(max_scan_bytes: usize) -> Self {
        Self { max_scan_bytes }
    }

    /// Scans untrusted UTF-8 text without returning source snippets.
    ///
    /// # Errors
    ///
    /// Returns an input error rather than allowing oversized, unscanned data.
    pub fn scan(&self, text: &str) -> Result<PrivacyScan> {
        if text.len() > self.max_scan_bytes {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "privacy scan input is {} bytes; limit is {} bytes",
                    text.len(),
                    self.max_scan_bytes
                ),
            ));
        }

        let bytes = text.as_bytes();
        // Mask this system's own opaque identifiers before any rule runs, so their digits and
        // surrounding punctuation can never be mistaken for a phone number, a national id, or
        // any other Secret/PII signature. Masking (rather than filtering findings afterward)
        // also strips an id's digits from feeding a match that starts or ends outside the id
        // itself — e.g. a trailing `)` in prose right after an id, which a naive "finding fully
        // inside the id span" filter would miss because the finding's range extends past it.
        let masked = mask_domain_id_spans(bytes);
        let scan_bytes: &[u8] = masked.as_deref().unwrap_or(bytes);
        let mut findings = Vec::new();
        scan_prefixed_tokens(scan_bytes, &mut findings);
        scan_private_keys(scan_bytes, &mut findings);
        scan_bearer_tokens(scan_bytes, &mut findings);
        scan_assigned_credentials(scan_bytes, &mut findings);
        scan_emails(scan_bytes, &mut findings);
        scan_phone_numbers(scan_bytes, &mut findings);
        scan_china_national_ids(scan_bytes, &mut findings);
        findings.sort_by_key(|finding| (finding.start, finding.end, finding.kind));
        findings.dedup();
        Ok(PrivacyScan { findings })
    }

    /// Redacts every supported match in untrusted text.
    ///
    /// # Errors
    ///
    /// Returns an input error when the scan size limit is exceeded.
    pub fn redact(&self, text: &str) -> Result<RedactedText> {
        let scan = self.scan(text)?;
        if scan.is_clean() {
            return Ok(RedactedText {
                text: text.to_owned(),
                finding_kinds: Vec::new(),
            });
        }

        let mut findings = scan.findings.clone();
        findings.sort_by_key(|finding| (finding.start, usize::MAX - finding.end));
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0;
        let mut kinds = BTreeSet::new();
        for finding in findings {
            kinds.insert(finding.kind);
            if finding.end <= cursor {
                continue;
            }
            let start = finding.start.max(cursor);
            output.push_str(&text[cursor..start]);
            output.push_str("[REDACTED:");
            output.push_str(finding.kind.code());
            output.push(']');
            cursor = finding.end;
        }
        output.push_str(&text[cursor..]);
        Ok(RedactedText {
            text: output,
            finding_kinds: kinds.into_iter().collect(),
        })
    }
}

fn scan_prefixed_tokens(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    let patterns = [
        (b"AKIA".as_slice(), 20, 20, PrivacyFindingKind::AwsAccessKey),
        (b"ASIA".as_slice(), 20, 20, PrivacyFindingKind::AwsAccessKey),
        (b"ghp_".as_slice(), 24, 96, PrivacyFindingKind::GithubToken),
        (b"gho_".as_slice(), 24, 96, PrivacyFindingKind::GithubToken),
        (b"ghu_".as_slice(), 24, 96, PrivacyFindingKind::GithubToken),
        (b"ghs_".as_slice(), 24, 96, PrivacyFindingKind::GithubToken),
        (b"ghr_".as_slice(), 24, 96, PrivacyFindingKind::GithubToken),
        (
            b"github_pat_".as_slice(),
            30,
            128,
            PrivacyFindingKind::GithubToken,
        ),
        (
            b"glpat-".as_slice(),
            26,
            128,
            PrivacyFindingKind::GitlabToken,
        ),
        (b"AIza".as_slice(), 39, 39, PrivacyFindingKind::GoogleApiKey),
        (b"npm_".as_slice(), 24, 128, PrivacyFindingKind::NpmToken),
        (b"sk-".as_slice(), 24, 192, PrivacyFindingKind::OpenAiKey),
        (
            b"sk-proj-".as_slice(),
            24,
            192,
            PrivacyFindingKind::OpenAiKey,
        ),
        (
            b"sk-live_".as_slice(),
            24,
            192,
            PrivacyFindingKind::StripeLiveKey,
        ),
        (b"xoxb-".as_slice(), 20, 192, PrivacyFindingKind::SlackToken),
        (b"xoxp-".as_slice(), 20, 192, PrivacyFindingKind::SlackToken),
        (b"eyJ".as_slice(), 30, 4096, PrivacyFindingKind::Jwt),
    ];
    for (prefix, minimum, maximum, kind) in patterns {
        let mut offset = 0;
        while let Some(relative) = find_bytes(&bytes[offset..], prefix) {
            let start = offset + relative;
            let mut end = start + prefix.len();
            while end < bytes.len() && end - start < maximum && is_token_byte(bytes[end]) {
                end += 1;
            }
            if end - start >= minimum && boundary_before(bytes, start) && boundary_after(bytes, end)
            {
                findings.push(PrivacyFinding { kind, start, end });
            }
            offset = start + prefix.len();
        }
    }
}

fn scan_private_keys(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    let begin = b"-----BEGIN ";
    let mut offset = 0;
    while let Some(relative) = find_bytes(&bytes[offset..], begin) {
        let start = offset + relative;
        let header_end = find_bytes(&bytes[start..], b"PRIVATE KEY-----")
            .map(|value| start + value + b"PRIVATE KEY-----".len());
        if let Some(header_end) = header_end {
            let end = find_bytes(&bytes[header_end..], b"-----END ")
                .and_then(|relative| {
                    let footer = header_end + relative;
                    find_bytes(&bytes[footer..], b"PRIVATE KEY-----")
                        .map(|value| footer + value + b"PRIVATE KEY-----".len())
                })
                .unwrap_or(bytes.len());
            findings.push(PrivacyFinding {
                kind: PrivacyFindingKind::PrivateKey,
                start,
                end,
            });
        }
        offset = start + begin.len();
    }
}

fn scan_bearer_tokens(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    let lower = ascii_lowercase(bytes);
    let needle = b"bearer ";
    let mut offset = 0;
    while let Some(relative) = find_bytes(&lower[offset..], needle) {
        let start = offset + relative;
        let token_start = start + needle.len();
        let mut end = token_start;
        while end < bytes.len() && is_token_byte(bytes[end]) && end - token_start < 4096 {
            end += 1;
        }
        if end - token_start >= 12 && boundary_before(bytes, start) {
            findings.push(PrivacyFinding {
                kind: PrivacyFindingKind::BearerCredential,
                start,
                end,
            });
        }
        offset = token_start;
    }
}

fn scan_assigned_credentials(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    let lower = ascii_lowercase(bytes);
    let names: [&[u8]; 13] = [
        b"api_key",
        b"api-key",
        b"apikey",
        b"access_token",
        b"auth_token",
        b"token",
        b"password",
        b"passwd",
        b"client_secret",
        b"client-secret",
        b"secret_key",
        b"secret",
        b"credential",
    ];
    for name in names {
        let mut offset = 0;
        while let Some(relative) = find_bytes(&lower[offset..], name) {
            let name_start = offset + relative;
            let mut cursor = name_start + name.len();
            if !boundary_before(bytes, name_start) {
                offset = cursor;
                continue;
            }
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_whitespace() || matches!(bytes[cursor], b'\'' | b'"'))
            {
                cursor += 1;
            }
            if cursor >= bytes.len() || !matches!(bytes[cursor], b'=' | b':') {
                offset = name_start + name.len();
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let quote = bytes
                .get(cursor)
                .copied()
                .filter(|byte| matches!(byte, b'\'' | b'"'));
            if quote.is_some() {
                cursor += 1;
            }
            let value_start = cursor;
            while cursor < bytes.len()
                && cursor - value_start < 4096
                && match quote {
                    Some(quote) => bytes[cursor] != quote,
                    None => {
                        !bytes[cursor].is_ascii_whitespace()
                            && !matches!(bytes[cursor], b',' | b';' | b'}' | b']')
                    }
                }
            {
                cursor += 1;
            }
            if cursor - value_start >= 8 && !is_redaction_marker(&bytes[value_start..cursor]) {
                findings.push(PrivacyFinding {
                    kind: PrivacyFindingKind::AssignedCredential,
                    start: value_start,
                    end: cursor,
                });
            }
            offset = name_start + name.len();
        }
    }
}

fn scan_emails(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    for (at, byte) in bytes.iter().enumerate() {
        if *byte != b'@' {
            continue;
        }
        let mut start = at;
        while start > 0 && is_email_local(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < bytes.len() && is_email_domain(bytes[end]) {
            end += 1;
        }
        let domain = &bytes[at + 1..end];
        if start < at
            && domain.len() >= 3
            && domain.contains(&b'.')
            && !domain.starts_with(b".")
            && !domain.ends_with(b".")
        {
            findings.push(PrivacyFinding {
                kind: PrivacyFindingKind::EmailAddress,
                start,
                end,
            });
        }
    }
}

/// Every stable prefix this system's own opaque identifiers are minted with, taken directly
/// from `sctx_domain`'s canonical id types (`crates/domain/src/ids.rs`) rather than duplicated
/// by hand. Text shaped like `<one of these prefixes><canonical UUIDv4>` is our own generated
/// identity — never third-party PII — so it is excluded from every PII rule below before those
/// rules run. Secret rules never match this shape (their prefixes and alphabets are disjoint),
/// so the exemption only needs to cover the PII rules.
const DOMAIN_ID_PREFIXES: &[&str] = &[
    sctx_domain::SpaceId::PREFIX,
    sctx_domain::ReferenceId::PREFIX,
    sctx_domain::TaskId::PREFIX,
    sctx_domain::TaskSessionId::PREFIX,
    sctx_domain::ExternalSessionId::PREFIX,
    sctx_domain::TaskIntentRevisionId::PREFIX,
    sctx_domain::SignalId::PREFIX,
    sctx_domain::WorkEpisodeId::PREFIX,
    sctx_domain::WorkObservationId::PREFIX,
    sctx_domain::AgentCheckpointId::PREFIX,
    sctx_domain::CheckpointClaimId::PREFIX,
    sctx_domain::CandidateBuildId::PREFIX,
    sctx_domain::SpaceRecommendationId::PREFIX,
    sctx_domain::ProposedSpaceGroupKey::PREFIX,
    sctx_domain::CandidateId::PREFIX,
    sctx_domain::SubmissionId::PREFIX,
    sctx_domain::ConfirmationId::PREFIX,
    sctx_domain::SpaceAssociationId::PREFIX,
    sctx_domain::ContextId::PREFIX,
    sctx_domain::RevisionId::PREFIX,
    sctx_domain::EventId::PREFIX,
    sctx_domain::PublicationId::PREFIX,
    sctx_domain::EvidenceId::PREFIX,
    sctx_domain::ReviewId::PREFIX,
    sctx_domain::ConflictId::PREFIX,
    sctx_domain::ResolutionId::PREFIX,
];

/// Length in bytes of one canonical, lowercase, hyphenated UUID (`8-4-4-4-12` hex digits).
const CANONICAL_UUID_LEN: usize = 36;

/// Byte offsets of the four hyphens inside a canonical UUID string.
const UUID_HYPHEN_OFFSETS: [usize; 4] = [8, 13, 18, 23];

/// Offset of the version nibble (must be `4` for every id this system mints via `Uuid::new_v4`).
const UUID_VERSION_OFFSET: usize = 14;

/// Offset of the RFC 4122 variant nibble (`8`, `9`, `a`, or `b`).
const UUID_VARIANT_OFFSET: usize = 19;

/// Matches a canonical, lowercase-hyphenated `UUIDv4` starting at `start`, returning its end
/// offset. Requires the exact `8-4-4-4-12` hex shape plus the version/variant nibbles that
/// `Uuid::new_v4()` always produces, so a coincidental digit run cannot be mistaken for one of
/// this system's ids.
fn match_canonical_uuid_v4(bytes: &[u8], start: usize) -> Option<usize> {
    let end = start.checked_add(CANONICAL_UUID_LEN)?;
    let candidate = bytes.get(start..end)?;
    for (offset, byte) in candidate.iter().enumerate() {
        if UUID_HYPHEN_OFFSETS.contains(&offset) {
            if *byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_digit() && !matches!(byte, b'a'..=b'f') {
            return None;
        }
    }
    if candidate[UUID_VERSION_OFFSET] != b'4' {
        return None;
    }
    if !matches!(candidate[UUID_VARIANT_OFFSET], b'8' | b'9' | b'a' | b'b') {
        return None;
    }
    Some(end)
}

/// Replaces every occurrence of one of this system's own opaque identifiers (prefix plus a
/// canonical `UUIDv4`, isolated on both sides by a non-alphanumeric boundary) with `#` filler of
/// the same byte length, so every offset downstream still points at the original text.
///
/// `#` is neither a digit nor ASCII-alphanumeric and is not one of the phone-number formatting
/// characters (`+ - ( ) `), so it acts as a hard stop for every scanner: it cannot extend a
/// digit run, satisfy a `boundary_before`/`boundary_after` check as anything but a boundary, or
/// complete a secret prefix.
///
/// Returns `None` when no domain id is present, so callers can scan the original bytes without
/// an allocation.
fn mask_domain_id_spans(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut buffer: Option<Vec<u8>> = None;
    for prefix in DOMAIN_ID_PREFIXES {
        let prefix_bytes = prefix.as_bytes();
        let mut offset = 0;
        while let Some(relative) = find_bytes(&bytes[offset..], prefix_bytes) {
            let start = offset + relative;
            let uuid_start = start + prefix_bytes.len();
            if boundary_before(bytes, start) {
                if let Some(uuid_end) = match_canonical_uuid_v4(bytes, uuid_start) {
                    if boundary_after(bytes, uuid_end) {
                        let buffer = buffer.get_or_insert_with(|| bytes.to_vec());
                        for byte in &mut buffer[start..uuid_end] {
                            *byte = b'#';
                        }
                    }
                }
            }
            offset = start + prefix_bytes.len();
        }
    }
    buffer
}

fn scan_phone_numbers(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    let mut start = 0;
    while start < bytes.len() {
        if !(bytes[start].is_ascii_digit() || bytes[start] == b'+')
            || !boundary_before(bytes, start)
        {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut digits = 0;
        while end < bytes.len()
            && (bytes[end].is_ascii_digit()
                || matches!(bytes[end], b'+' | b'-' | b' ' | b'(' | b')'))
            && end - start < 32
        {
            digits += usize::from(bytes[end].is_ascii_digit());
            end += 1;
        }
        while end > start && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let has_formatting = bytes[start..end]
            .iter()
            .any(|byte| matches!(byte, b'+' | b'-' | b' ' | b'(' | b')'));
        let mainland_mobile = digits == 11
            && bytes[start..end].iter().all(u8::is_ascii_digit)
            && bytes[start] == b'1'
            && matches!(bytes[start + 1], b'3'..=b'9');
        if (10..=15).contains(&digits)
            && (has_formatting || mainland_mobile)
            && boundary_after(bytes, end)
            && !looks_like_iso_date(&bytes[start..end])
        {
            findings.push(PrivacyFinding {
                kind: PrivacyFindingKind::PhoneNumber,
                start,
                end,
            });
        }
        start = end.max(start + 1);
    }
}

fn scan_china_national_ids(bytes: &[u8], findings: &mut Vec<PrivacyFinding>) {
    if bytes.len() < 18 {
        return;
    }
    for start in 0..=bytes.len() - 18 {
        let candidate = &bytes[start..start + 18];
        if boundary_before(bytes, start)
            && boundary_after(bytes, start + 18)
            && candidate[..17].iter().all(u8::is_ascii_digit)
            && (candidate[17].is_ascii_digit() || matches!(candidate[17], b'x' | b'X'))
            && valid_china_id_checksum(candidate)
        {
            findings.push(PrivacyFinding {
                kind: PrivacyFindingKind::ChinaNationalId,
                start,
                end: start + 18,
            });
        }
    }
}

fn valid_china_id_checksum(candidate: &[u8]) -> bool {
    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECKS: &[u8; 11] = b"10X98765432";
    let sum = candidate[..17]
        .iter()
        .zip(WEIGHTS)
        .map(|(digit, weight)| u32::from(*digit - b'0') * weight)
        .sum::<u32>();
    candidate[17].to_ascii_uppercase() == CHECKS[(sum % 11) as usize]
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn ascii_lowercase(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn is_email_local(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

fn boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0 || !bytes[index - 1].is_ascii_alphanumeric()
}

fn boundary_after(bytes: &[u8], index: usize) -> bool {
    index == bytes.len() || !bytes[index].is_ascii_alphanumeric()
}

fn looks_like_iso_date(value: &[u8]) -> bool {
    value.len() == 10 && value[4] == b'-' && value[7] == b'-'
}

fn is_redaction_marker(value: &[u8]) -> bool {
    value.starts_with(b"[REDACTED:") || value == b"[REDACTED]"
}

#[cfg(test)]
mod tests {
    use super::{PrivacyFindingKind, PrivacyScanner};

    #[test]
    fn clean_engineering_text_is_not_changed() {
        let scanner = PrivacyScanner::default();
        let text = "Run cargo test for module auth_token_parser on 2026-08-18";

        assert!(scanner.scan(text).unwrap().is_clean());
        assert_eq!(scanner.redact(text).unwrap().text, text);
    }

    #[test]
    fn diagnostics_do_not_include_source_values() {
        let scanner = PrivacyScanner::default();
        let value = "alice@example.com";
        let scan = scanner.scan(value).unwrap();

        assert_eq!(scan.findings()[0].kind, PrivacyFindingKind::EmailAddress);
        assert!(!format!("{scan:?}").contains(value));
    }

    /// Reproduces the U4 bug directly against `PrivacyScanner`: a real `ContextId` whose UUID
    /// happens to have an all-digit last group (`743887405571`), embedded exactly the way
    /// `crates/mcp/tests/context_usage_signals.rs::submit_checkpoint` writes a Claim rationale
    /// (`"... (building on {context_id})"`). Before the fix this tripped `phone_number`: the
    /// trailing `)` from the prose is one of the phone rule's allowed formatting characters, so
    /// the digit run keeps going past the id and picks up `has_formatting = true`, and the whole
    /// `task_checkpoint` request was rejected outright (`durable_checkpoint` in
    /// `crates/mcp/src/lib.rs` scans the entire serialized request and rejects it wholesale on
    /// any non-clean finding). This id is pinned so the case stays reproduced if the exemption
    /// regresses.
    const KNOWN_TRIGGERING_UUID: &str = "76ce6673-6f2e-4639-86c4-743887405571";

    fn checkpoint_style_rationale(id: &str) -> String {
        format!("The Task confirmed the inherited behavior while extending it (building on {id})")
    }

    #[test]
    fn pinned_context_id_would_have_tripped_the_phone_number_rule_before_the_fix() {
        // This test documents *why* the fix is needed by re-deriving, byte for byte, the
        // fragment the old (pre-fix) phone-number scanner matched: the id's last UUID group
        // plus the immediately following `)`. It does not call the exemption at all, so it
        // keeps passing after the fix and still proves the underlying rule is exploitable by
        // an all-digit UUID group.
        let rationale = checkpoint_style_rationale(&format!("ctx_{KNOWN_TRIGGERING_UUID}"));
        let last_group_and_paren = "743887405571)";
        assert!(rationale.ends_with(last_group_and_paren));
        let digits: usize = last_group_and_paren
            .bytes()
            .filter(u8::is_ascii_digit)
            .count();
        assert_eq!(
            digits, 12,
            "12 digits falls inside the rule's 10..=15 window"
        );
        let has_formatting = last_group_and_paren
            .bytes()
            .any(|byte| matches!(byte, b'+' | b'-' | b' ' | b'(' | b')'));
        assert!(
            has_formatting,
            "the trailing ')' from the surrounding prose supplies the formatting byte"
        );
    }

    #[test]
    fn context_id_embedded_like_a_checkpoint_rationale_is_not_flagged() {
        let scanner = PrivacyScanner::default();
        let id = format!("ctx_{KNOWN_TRIGGERING_UUID}");
        let rationale = checkpoint_style_rationale(&id);

        let scan = scanner.scan(&rationale).unwrap();
        assert!(
            scan.is_clean(),
            "own ContextId must never be treated as PII: {:?}",
            scan.findings()
        );
        assert_eq!(scanner.redact(&rationale).unwrap().text, rationale);
    }

    #[test]
    fn every_domain_id_prefix_with_the_pinned_uuid_is_exempted() {
        // The all-digit last group is what tripped the rule; the prefix in front of it doesn't
        // matter to `scan_phone_numbers`, so sweep every prefix this system actually mints
        // (crates/domain/src/ids.rs) to prove the exemption's prefix list, not just `ctx_`.
        let scanner = PrivacyScanner::default();
        let prefixes = [
            sctx_domain::SpaceId::PREFIX,
            sctx_domain::ReferenceId::PREFIX,
            sctx_domain::TaskId::PREFIX,
            sctx_domain::TaskSessionId::PREFIX,
            sctx_domain::ExternalSessionId::PREFIX,
            sctx_domain::TaskIntentRevisionId::PREFIX,
            sctx_domain::SignalId::PREFIX,
            sctx_domain::WorkEpisodeId::PREFIX,
            sctx_domain::WorkObservationId::PREFIX,
            sctx_domain::AgentCheckpointId::PREFIX,
            sctx_domain::CheckpointClaimId::PREFIX,
            sctx_domain::CandidateBuildId::PREFIX,
            sctx_domain::SpaceRecommendationId::PREFIX,
            sctx_domain::ProposedSpaceGroupKey::PREFIX,
            sctx_domain::CandidateId::PREFIX,
            sctx_domain::SubmissionId::PREFIX,
            sctx_domain::ConfirmationId::PREFIX,
            sctx_domain::SpaceAssociationId::PREFIX,
            sctx_domain::ContextId::PREFIX,
            sctx_domain::RevisionId::PREFIX,
            sctx_domain::EventId::PREFIX,
            sctx_domain::PublicationId::PREFIX,
            sctx_domain::EvidenceId::PREFIX,
            sctx_domain::ReviewId::PREFIX,
            sctx_domain::ConflictId::PREFIX,
            sctx_domain::ResolutionId::PREFIX,
        ];
        for prefix in prefixes {
            let rationale = checkpoint_style_rationale(&format!("{prefix}{KNOWN_TRIGGERING_UUID}"));
            let scan = scanner.scan(&rationale).unwrap();
            assert!(
                scan.is_clean(),
                "prefix {prefix:?} must be exempted: {:?}",
                scan.findings()
            );
        }
    }

    #[test]
    fn a_prefix_outside_the_domain_id_list_is_not_exempted() {
        // The exemption must be an exact allow-list of this system's real prefixes, not "any
        // 3-letter-plus-underscore token in front of a UUID". `xyz_` is not one of ours, so the
        // same digit run must still be rejected.
        let scanner = PrivacyScanner::default();
        let rationale = checkpoint_style_rationale(&format!("xyz_{KNOWN_TRIGGERING_UUID}"));

        let scan = scanner.scan(&rationale).unwrap();
        assert!(
            scan.findings()
                .iter()
                .any(|finding| finding.kind == PrivacyFindingKind::PhoneNumber),
            "an unrecognized prefix must not silently exempt the digits behind it"
        );
    }

    #[test]
    fn a_malformed_uuid_after_a_real_prefix_is_not_exempted() {
        // The exemption requires the *whole* canonical UUIDv4 shape, not just "digits after a
        // known prefix". Flip the version nibble from '4' to '1': this is not a UUID this
        // system's `Uuid::new_v4()` could ever produce, so it must still be scanned.
        let scanner = PrivacyScanner::default();
        let malformed = KNOWN_TRIGGERING_UUID.replacen("4639", "1639", 1);
        assert_ne!(malformed, KNOWN_TRIGGERING_UUID);
        let rationale = checkpoint_style_rationale(&format!("ctx_{malformed}"));

        let scan = scanner.scan(&rationale).unwrap();
        assert!(
            scan.findings()
                .iter()
                .any(|finding| finding.kind == PrivacyFindingKind::PhoneNumber),
            "a non-v4 UUID shape must not be exempted just because it follows 'ctx_'"
        );
    }

    #[test]
    fn an_uppercase_uuid_after_a_real_prefix_is_not_exempted() {
        // `Display` for every domain id type always renders the UUID lowercase
        // (`Uuid::hyphenated()`), so an uppercase rendering is not a shape this system ever
        // produces; the exemption must not recognize it either.
        let scanner = PrivacyScanner::default();
        let uppercase = KNOWN_TRIGGERING_UUID.to_ascii_uppercase();
        let rationale = checkpoint_style_rationale(&format!("ctx_{uppercase}"));

        let scan = scanner.scan(&rationale).unwrap();
        assert!(
            scan.findings()
                .iter()
                .any(|finding| finding.kind == PrivacyFindingKind::PhoneNumber),
            "an uppercase UUID must not be exempted"
        );
    }

    #[test]
    fn real_phone_numbers_are_still_rejected_next_to_an_exempted_context_id() {
        // The exemption must not open a blanket "any parenthesized digit run near an id is
        // fine" loophole: a genuine phone number sitting right next to an exempted ContextId in
        // the same text must still be caught.
        let scanner = PrivacyScanner::default();
        let id = format!("ctx_{KNOWN_TRIGGERING_UUID}");
        let cases = [
            // Note: a phone number immediately followed by "(" hits a separate, pre-existing
            // gap in this same rule (see U4 report's "peripheral findings") where the '(' is
            // swallowed as a formatting continuation and the run then dies on the next letter,
            // so these cases deliberately avoid placing an id-citing "(" right after the
            // number.
            format!("Escalate to +1 415-555-0132, building on {id}"),
            format!("Call the on-call line (415) 555-0132, referencing {id}"),
            format!("On-call mobile 13800001234 tracked {id} in the same report"),
            format!("International contact +86 138-0000-1234 filed against {id}"),
        ];
        for text in cases {
            let scan = scanner.scan(&text).unwrap();
            assert!(
                scan.findings()
                    .iter()
                    .any(|finding| finding.kind == PrivacyFindingKind::PhoneNumber),
                "a real phone number next to an id must still be caught: {text:?} -> {:?}",
                scan.findings()
            );
        }
    }

    #[test]
    fn real_phone_numbers_alone_are_still_rejected() {
        // No domain id involved at all: confirms the fix did not touch the phone rule's own
        // detection of ordinary formatted and mainland-mobile numbers.
        let scanner = PrivacyScanner::default();
        for text in [
            "+1 415-555-0132",
            "(415) 555-0132",
            "call 415-555-0132 back",
            "13800001234",
            "+86 138-0000-1234",
        ] {
            let scan = scanner.scan(text).unwrap();
            assert!(
                scan.findings()
                    .iter()
                    .any(|finding| finding.kind == PrivacyFindingKind::PhoneNumber),
                "expected a phone_number finding for {text:?}, got {:?}",
                scan.findings()
            );
        }
    }
}
