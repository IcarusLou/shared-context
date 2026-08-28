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
        let mut findings = Vec::new();
        scan_prefixed_tokens(bytes, &mut findings);
        scan_private_keys(bytes, &mut findings);
        scan_bearer_tokens(bytes, &mut findings);
        scan_assigned_credentials(bytes, &mut findings);
        scan_emails(bytes, &mut findings);
        scan_phone_numbers(bytes, &mut findings);
        scan_china_national_ids(bytes, &mut findings);
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
}
