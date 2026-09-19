//! The team policy document as an installation actually resolves it.
//!
//! The unit tests next to the parser cover the grammar. What only a real root and a real
//! `config.toml` can show is the part the Hook path depends on: that *every* way this file can be
//! wrong resolves to the compiled-in default instead of to an error, and that the failure is still
//! visible afterwards.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use sctx_local_state::{
    DEFAULT_POLICY_MARKDOWN, POLICY_FILE_NAME, Policy, PolicyStatus, STOP_SECTION_MAX_BYTES,
    UserConfigStore, installation_policy, resolve_policy,
};

fn root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write_policy(root: &Path, markdown: &str) {
    fs::write(root.join(POLICY_FILE_NAME), markdown).unwrap();
}

#[test]
fn an_absent_policy_file_resolves_to_the_compiled_in_default() {
    let temporary = root();
    let resolved = resolve_policy(temporary.path(), None);
    assert_eq!(resolved.status, PolicyStatus::Default);
    assert_eq!(resolved.policy, Policy::compiled_default());
    assert_eq!(resolved.path, temporary.path().join(POLICY_FILE_NAME));
    assert!(resolved.oversize_sections.is_empty());
    assert!(!resolved.status.is_degraded());
    assert!(resolved.summary().contains("compiled-in default"));
}

#[test]
fn a_policy_file_replaces_only_the_sections_it_states() {
    let temporary = root();
    write_policy(
        temporary.path(),
        "# our rules\n\nprose that is not a section\n\n## stop\nship it\n\n## notes\nignored\n",
    );
    let resolved = resolve_policy(temporary.path(), None);
    assert_eq!(resolved.status, PolicyStatus::Loaded);
    assert_eq!(resolved.policy.stop(), "ship it");
    assert_eq!(
        resolved.policy.session(),
        "",
        "an omitted section says nothing; it does not inherit the default"
    );
    assert_eq!(resolved.policy.checkpoint(), "");
    assert_eq!(resolved.policy.triage(), "");
}

/// The one substitution that *does* happen: a section the loader refuses falls back to the
/// default, because the alternative is delivering nothing where the operator meant to say
/// something.
#[test]
fn an_oversize_section_falls_back_to_the_default_and_is_reported() {
    let temporary = root();
    write_policy(
        temporary.path(),
        &format!(
            "## stop\n{}\n\n## session\nkept\n",
            "x".repeat(STOP_SECTION_MAX_BYTES + 1)
        ),
    );
    let resolved = resolve_policy(temporary.path(), None);
    assert_eq!(resolved.status, PolicyStatus::SectionOversize);
    assert_eq!(resolved.oversize_sections, vec!["stop"]);
    assert_eq!(resolved.policy.stop(), Policy::compiled_default().stop());
    assert_eq!(resolved.policy.session(), "kept");
    assert!(resolved.status.is_degraded());
    assert!(resolved.summary().contains("stop"));
    assert_eq!(resolved.status.reason(), "policy_section_oversize");
}

#[test]
fn an_unreadable_policy_file_never_fails_and_keeps_the_default() {
    let temporary = root();
    let path = temporary.path().join(POLICY_FILE_NAME);
    write_policy(temporary.path(), "## stop\nunreachable\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let resolved = resolve_policy(temporary.path(), None);
    // A test running as root can read a 0o000 file, so accept either the permission failure or a
    // successful read; what must never happen is an error reaching the caller.
    if resolved.status == PolicyStatus::Unreadable {
        assert_eq!(resolved.policy, Policy::compiled_default());
        assert!(resolved.status.is_degraded());
        assert_eq!(resolved.status.reason(), "policy_unreadable");
        assert!(resolved.summary().contains("unreadable"));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn non_utf8_bytes_are_unreadable_rather_than_lossy() {
    let temporary = root();
    fs::write(temporary.path().join(POLICY_FILE_NAME), [0xff, 0xfe, 0x00]).unwrap();
    let resolved = resolve_policy(temporary.path(), None);
    assert_eq!(resolved.status, PolicyStatus::Unreadable);
    assert_eq!(resolved.policy, Policy::compiled_default());
}

#[test]
fn the_config_policy_table_redirects_the_document_and_round_trips() {
    let temporary = root();
    let installation = temporary.path().join("install");
    let store = UserConfigStore::initialize(&installation).unwrap();
    assert_eq!(store.policy_settings().unwrap().path, None);

    let elsewhere = temporary.path().join("team-policy.md");
    fs::write(&elsewhere, "## triage\nteam grounds\n").unwrap();
    let config = installation.join("config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let text = format!("{text}\n[policy]\npath = {:?}\n", elsewhere.display());
    fs::write(&config, text).unwrap();

    let settings = store.policy_settings().unwrap();
    assert_eq!(settings.path.as_deref(), Some(elsewhere.as_path()));
    let resolved = installation_policy(&installation);
    assert_eq!(resolved.path, elsewhere);
    assert_eq!(resolved.policy.triage(), "team grounds");
    // The redirected document is authoritative, including for what it does not say.
    assert_eq!(resolved.policy.session(), "");
}

/// `[policy] path` is validated on the same terms as every other configured path: a relative one
/// is refused rather than resolved against whichever of the three entry points happens to be
/// running.
#[test]
fn a_relative_policy_path_is_refused_by_the_config_reader() {
    let temporary = root();
    let installation = temporary.path().join("install");
    let store = UserConfigStore::initialize(&installation).unwrap();
    let config = installation.join("config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("\n[policy]\npath = \"policy.md\"\n");
    fs::write(&config, text).unwrap();
    assert!(store.policy_settings().is_err());
    // Fail-open all the way through: an unusable table still activates Sessions on the default.
    assert_eq!(
        installation_policy(&installation).policy,
        Policy::compiled_default()
    );
}

#[test]
fn the_shipped_default_document_is_what_the_compiled_default_parses() {
    let temporary = root();
    write_policy(temporary.path(), DEFAULT_POLICY_MARKDOWN);
    let resolved = resolve_policy(temporary.path(), None);
    assert_eq!(resolved.status, PolicyStatus::Loaded);
    assert_eq!(
        resolved.policy,
        Policy::compiled_default(),
        "an installation that copies the shipped file must get the compiled-in behaviour"
    );
}
