//! Team policy text delivered to the model at run time.
//!
//! Shared Context separates protocol from policy. Protocol is what the server enforces and what
//! the Skill states; policy is what one team decided is worth keeping and how it wants progress
//! reported. Protocol ships in the binary. Policy lives in one user-directory file,
//! `~/.shared-context/policy.md`, so a business Repository needs no `AGENTS.md`, no extra Skill,
//! and no router — and so that changing the team's rules never means shipping a release.
//!
//! Delivery is the point: sctx already owns four channels the model reliably reads (the
//! `SessionStart` activation marker, the `task_checkpoint` tool description, the Stop/TurnStop
//! reminder, and the Candidate triage text), and each one carries exactly one section of this
//! file. A section the file omits simply is not delivered.
//!
//! Every failure here is fail-open by construction. An absent file, an unreadable one, a
//! non-UTF-8 one, and an oversize section all resolve to the compiled-in defaults, because a Hook
//! that refuses to activate a Session over a typo in a policy file is a worse outcome than a
//! Session that received last release's wording. The failure is never silent: it is reported by
//! `sctx doctor` and by the `hook_decision` telemetry reason.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// The compiled-in policy, and the file `sctx setup` writes when none exists.
pub const DEFAULT_POLICY_MARKDOWN: &str = include_str!("default_policy.md");

/// File name of the policy document inside the installation root.
pub const POLICY_FILE_NAME: &str = "policy.md";

/// Longest a single policy section may be, per channel.
///
/// Each ceiling is derived from the channel that carries the section, not from a uniform taste:
/// `session` rides inside the activation marker, which is capped against the Codex hook
/// `additionalContext` budget; `checkpoint` and `triage` ride inside MCP tool descriptions, which
/// every host re-renders on every `tools/list`; `stop` is one line appended to a reminder the
/// model may see on every turn. Over the ceiling the section falls back to its default rather
/// than truncating: half a sentence delivered to a model is worse than the wording it replaced.
pub const SESSION_SECTION_MAX_BYTES: usize = 512;
/// See [`SESSION_SECTION_MAX_BYTES`].
pub const CHECKPOINT_SECTION_MAX_BYTES: usize = 1024;
/// See [`SESSION_SECTION_MAX_BYTES`].
pub const STOP_SECTION_MAX_BYTES: usize = 256;
/// See [`SESSION_SECTION_MAX_BYTES`].
pub const TRIAGE_SECTION_MAX_BYTES: usize = 768;

/// The four sections, in the order they appear in the file and in this module.
pub const POLICY_SECTION_NAMES: [&str; 4] = ["session", "checkpoint", "stop", "triage"];

/// One team's runtime policy, already validated and ready to deliver.
///
/// Each field is the section's normalized text: every line trimmed, blank lines dropped, the
/// remaining lines joined with `\n`. An empty field means the section says nothing and its
/// channel delivers protocol alone.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Policy {
    session: String,
    checkpoint: String,
    stop: String,
    triage: String,
}

impl Policy {
    /// Parses one policy document. Unknown headings and prose outside a section are ignored.
    ///
    /// Sections over their ceiling are reported in the returned names and left empty here; the
    /// caller decides whether to substitute the default (`load`) or to report the file as
    /// authored (`sctx policy show --file`).
    #[must_use]
    pub fn parse(markdown: &str) -> (Self, Vec<&'static str>) {
        let mut policy = Self::default();
        let mut current: Option<&'static str> = None;
        let mut buffer: Vec<&str> = Vec::new();
        let mut oversize = Vec::new();
        let flush = |section: Option<&'static str>,
                     buffer: &mut Vec<&str>,
                     policy: &mut Self,
                     oversize: &mut Vec<&'static str>| {
            let Some(section) = section else {
                buffer.clear();
                return;
            };
            let text = buffer.join("\n");
            buffer.clear();
            let limit = section_limit(section);
            if text.len() > limit {
                oversize.push(section);
                return;
            }
            *policy.section_mut(section) = text;
        };
        for line in markdown.lines() {
            let trimmed = line.trim();
            if let Some(heading) = heading_name(trimmed) {
                flush(current, &mut buffer, &mut policy, &mut oversize);
                current = match heading {
                    Heading::Section(name) => Some(name),
                    Heading::Other => None,
                };
                continue;
            }
            if current.is_some() && !trimmed.is_empty() {
                buffer.push(trimmed);
            }
        }
        flush(current, &mut buffer, &mut policy, &mut oversize);
        (policy, oversize)
    }

    /// The compiled-in default policy. Parsing it is infallible by construction; a regression is
    /// caught by `default_policy_parses_and_fits_every_section`.
    #[must_use]
    pub fn compiled_default() -> Self {
        Self::parse(DEFAULT_POLICY_MARKDOWN).0
    }

    /// Team summary appended inside the `SessionStart` activation marker.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// "Worth keeping / not worth keeping" text appended to the `task_checkpoint` description.
    #[must_use]
    pub fn checkpoint(&self) -> &str {
        &self.checkpoint
    }

    /// One line appended to a Stop/TurnStop checkpoint reminder.
    #[must_use]
    pub fn stop(&self) -> &str {
        &self.stop
    }

    /// Disposition grounds appended to the shared Candidate triage text.
    #[must_use]
    pub fn triage(&self) -> &str {
        &self.triage
    }

    /// The section's text with newlines collapsed to spaces, for a single-line channel such as an
    /// MCP tool description.
    #[must_use]
    pub fn inline(text: &str) -> String {
        text.replace('\n', " ")
    }

    fn section_mut(&mut self, section: &str) -> &mut String {
        match section {
            "session" => &mut self.session,
            "checkpoint" => &mut self.checkpoint,
            "stop" => &mut self.stop,
            _ => &mut self.triage,
        }
    }

    fn section(&self, section: &str) -> &str {
        match section {
            "session" => &self.session,
            "checkpoint" => &self.checkpoint,
            "stop" => &self.stop,
            _ => &self.triage,
        }
    }
}

const fn section_limit(section: &str) -> usize {
    match section.as_bytes() {
        b"session" => SESSION_SECTION_MAX_BYTES,
        b"checkpoint" => CHECKPOINT_SECTION_MAX_BYTES,
        b"stop" => STOP_SECTION_MAX_BYTES,
        _ => TRIAGE_SECTION_MAX_BYTES,
    }
}

/// What one Markdown heading means to the parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Heading {
    /// `## <name>` for one of the four known sections.
    Section(&'static str),
    /// Any other heading. It still ends the previous section, so prose under an unknown
    /// `## Notes` heading never leaks into the section above it.
    Other,
}

fn heading_name(trimmed: &str) -> Option<Heading> {
    let rest = trimmed.strip_prefix('#')?;
    let name = rest.trim_start_matches('#').trim().to_ascii_lowercase();
    Some(
        POLICY_SECTION_NAMES
            .into_iter()
            .find(|section| *section == name)
            .filter(|_| trimmed.starts_with("## "))
            .map_or(Heading::Other, Heading::Section),
    )
}

/// Why the effective policy is not simply "the file, as authored".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyStatus {
    /// No policy file exists; the compiled-in default is in effect.
    Default,
    /// The file was read and every present section was inside its ceiling.
    Loaded,
    /// The file exists but could not be read or decoded as UTF-8.
    Unreadable,
    /// The file was read, but at least one section was over its ceiling and fell back.
    SectionOversize,
}

impl PolicyStatus {
    /// A stable, closed token for the telemetry `hook_decision` reason and `sctx doctor`.
    ///
    /// The telemetry schema is closed (`deny_unknown_fields`) and carries no attribute map, so
    /// this rides in the existing `reason` field, which is bounded to 64 ASCII bytes. Every token
    /// here is well inside that.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Default => "policy_default",
            Self::Loaded => "policy_loaded",
            Self::Unreadable => "policy_unreadable",
            Self::SectionOversize => "policy_section_oversize",
        }
    }

    /// True when the operator has something to fix.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Unreadable | Self::SectionOversize)
    }
}

/// The effective policy plus everything `sctx doctor` needs to explain it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPolicy {
    /// The policy actually delivered to the model.
    pub policy: Policy,
    /// The file this installation reads, whether or not it exists.
    pub path: PathBuf,
    pub status: PolicyStatus,
    /// Sections that fell back to the default because they were over their ceiling.
    pub oversize_sections: Vec<&'static str>,
}

impl ResolvedPolicy {
    /// One bounded, operator-facing line. Never contains file content.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.status {
            PolicyStatus::Default => format!("no {POLICY_FILE_NAME}; compiled-in default policy"),
            PolicyStatus::Loaded => format!("policy loaded from {}", self.path.display()),
            PolicyStatus::Unreadable => format!(
                "{} is unreadable; compiled-in default policy is in effect",
                self.path.display()
            ),
            PolicyStatus::SectionOversize => format!(
                "{} sections over their byte ceiling fell back to the default: {}",
                self.path.display(),
                self.oversize_sections.join(", ")
            ),
        }
    }
}

/// Resolves the effective policy for one installation root.
///
/// `override_path` is `[policy] path` from `config.toml`, already read by the caller (the Hook hot
/// path reads the Catalog from that same file and must not open it twice). Absent means
/// `<root>/policy.md`.
///
/// This never fails. Every error becomes a [`PolicyStatus`] and the compiled-in default.
#[must_use]
pub fn resolve_policy(root: &Path, override_path: Option<&Path>) -> ResolvedPolicy {
    let path = override_path.map_or_else(|| root.join(POLICY_FILE_NAME), Path::to_path_buf);
    let default = Policy::compiled_default();
    let text = match fs::read(&path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return ResolvedPolicy {
                    policy: default,
                    path,
                    status: PolicyStatus::Unreadable,
                    oversize_sections: Vec::new(),
                };
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ResolvedPolicy {
                policy: default,
                path,
                status: PolicyStatus::Default,
                oversize_sections: Vec::new(),
            };
        }
        Err(_) => {
            return ResolvedPolicy {
                policy: default,
                path,
                status: PolicyStatus::Unreadable,
                oversize_sections: Vec::new(),
            };
        }
    };
    let (mut policy, oversize) = Policy::parse(&text);
    for section in &oversize {
        default
            .section(section)
            .clone_into(policy.section_mut(section));
    }
    let status = if oversize.is_empty() {
        PolicyStatus::Loaded
    } else {
        PolicyStatus::SectionOversize
    };
    ResolvedPolicy {
        policy,
        path,
        status,
        oversize_sections: oversize,
    }
}

/// Resolves the effective policy for one installation, honoring `[policy] path`.
///
/// This is the single entry point for the Hook path, the MCP server, `sctx doctor`, and
/// `sctx policy show`, so that all four agree on which file is in effect. A `config.toml` that
/// cannot be read -- a concurrent writer holds the lock, the installation is half-built -- is the
/// same fail-open as a missing policy file: `<root>/policy.md`, then the compiled-in default.
#[must_use]
pub fn installation_policy(root: &Path) -> ResolvedPolicy {
    let override_path = crate::UserConfigStore::open_existing(root)
        .and_then(|store| store.policy_settings())
        .ok()
        .and_then(|settings| settings.path);
    resolve_policy(root, override_path.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_parses_and_fits_every_section() {
        let (policy, oversize) = Policy::parse(DEFAULT_POLICY_MARKDOWN);
        assert!(oversize.is_empty(), "default policy is over a ceiling");
        for section in POLICY_SECTION_NAMES {
            assert!(
                !policy.section(section).is_empty(),
                "default policy is missing {section}"
            );
        }
        assert_eq!(policy.session().lines().count(), 2);
        assert_eq!(policy.stop().lines().count(), 1);
    }

    #[test]
    fn parse_ignores_title_prose_and_unknown_headings() {
        let (policy, oversize) = Policy::parse(
            "# Title\n\nintro prose\n\n## session\nline one\n\n## notes\nignored\n\n## stop\nend\n",
        );
        assert!(oversize.is_empty());
        assert_eq!(policy.session(), "line one");
        assert_eq!(policy.stop(), "end");
        assert_eq!(policy.checkpoint(), "");
        assert_eq!(policy.triage(), "");
    }

    #[test]
    fn parse_reports_an_oversize_section_and_keeps_the_others() {
        let markdown = format!(
            "## stop\n{}\n\n## triage\nkept\n",
            "x".repeat(STOP_SECTION_MAX_BYTES + 1)
        );
        let (policy, oversize) = Policy::parse(&markdown);
        assert_eq!(oversize, vec!["stop"]);
        assert_eq!(policy.stop(), "");
        assert_eq!(policy.triage(), "kept");
    }

    #[test]
    fn heading_only_matches_a_level_two_section_name() {
        assert_eq!(
            heading_name("## session"),
            Some(Heading::Section("session"))
        );
        assert_eq!(
            heading_name("## Session"),
            Some(Heading::Section("session"))
        );
        assert_eq!(heading_name("### session"), Some(Heading::Other));
        assert_eq!(heading_name("# session"), Some(Heading::Other));
        assert_eq!(heading_name("session"), None);
    }

    #[test]
    fn inline_collapses_newlines_for_single_line_channels() {
        assert_eq!(Policy::inline("a\nb"), "a b");
    }
}
