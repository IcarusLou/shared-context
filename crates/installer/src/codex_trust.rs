//! Codex hook *trust hashes*: the second half of installing a Codex hook.
//!
//! Writing `~/.codex/hooks.json` is not enough to make Codex run a hook. Codex refuses to execute
//! any hook whose recomputed identity hash does not match the `trusted_hash` recorded in
//! `~/.codex/config.toml` under
//! `[hooks.state."<key source>:<event label>:<group index>:<handler index>"]`. So every time the
//! installer rewrites a hook command -- which it does on every upgrade, because the command embeds
//! `--agent-version` -- the stored hash goes stale and the hook silently stops running. Silently:
//! Codex reports nothing, the hook simply never fires, and the only visible symptom is an absence.
//!
//! This module reproduces Codex's hash so [`super::merge_json_hooks`]'s sibling can re-stamp the
//! six state keys the installer owns in the same transaction that wrote the hooks file.
//!
//! The algorithm is public. Read from openai/codex at commit
//! `c7c824dce4da186e5142af5d9a1587ae553efe46` (2026-09-01, "Treat bundled cleanup hooks as
//! built-ins (#42110)"):
//!
//! - `codex-rs/hooks/src/engine/discovery.rs` -- `hook_hash` builds a
//!   `NormalizedHookIdentity { event_name, #[serde(flatten)] group: MatcherGroup }` whose group
//!   carries the event-adjusted matcher and the single *normalized* handler, serializes it to a
//!   `toml::Value`, and hands that to `version_for_toml`.
//! - `codex-rs/hooks/src/lib.rs` -- `hook_event_key_label` (the `snake_case` label in the state key)
//!   and `hook_key` (`"{key_source}:{label}:{gi}:{hi}"`).
//! - `codex-rs/hooks/src/events/common.rs` -- `matcher_pattern_for_event`: `UserPromptSubmit`,
//!   `Stop` and `Interrupt` drop their matcher entirely.
//! - `codex-rs/hooks/src/events/session_end.rs` -- `SessionEnd`/`Interrupt` timeouts default to 1s
//!   and clamp to `[1, 3]`; every other event defaults to 600s.
//! - `codex-rs/config/src/hook_config.rs` -- the serde shape of `HooksFile` / `MatcherGroup` /
//!   `HookHandlerConfig`, including which fields are skipped when `None`.
//! - `codex-rs/config/src/fingerprint.rs` -- `version_for_toml`: TOML value -> `serde_json::Value`
//!   -> recursively key-sorted -> compact `serde_json` bytes -> sha256 -> `"sha256:<hex>"`.
//!
//! Two Rust behaviours upstream are load-bearing and easy to miss:
//!
//! - `toml::Value::try_from` **drops** map entries whose value is `None`, so an absent matcher or
//!   `statusMessage` contributes no key at all rather than a null.
//! - `timeout` is normalized *before* hashing and is always `Some`, so a handler that states no
//!   timeout still hashes as if it said `600`.
//!
//! The same algorithm exists in Python at `tests/scripts/session_replay/codex_trust.py`, where the
//! replay driver needs it to trust a `hooks.json` it wrote itself. The two are cross-checked
//! bit-for-bit against the shared fixture in `fixtures/codex-trust/` -- see
//! `crates/installer/tests/codex_trust_parity.rs` and that fixture's `expected.json`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, value};

use super::{Result, invalid};

/// The openai/codex commit the rules below were read at. Quoted in the module docs too; kept here
/// so a drift check has something machine-readable to compare against.
pub const CODEX_SOURCE_COMMIT: &str = "c7c824dce4da186e5142af5d9a1587ae553efe46";

/// `hooks.json` spells events in `PascalCase` (`HookEventsToml`'s serde renames); the persisted
/// state key spells them `snake_case` (`hook_event_key_label`).
const EVENT_KEY_LABELS: [(&str, &str); 12] = [
    ("PreToolUse", "pre_tool_use"),
    ("PermissionRequest", "permission_request"),
    ("PostToolUse", "post_tool_use"),
    ("PreCompact", "pre_compact"),
    ("PostCompact", "post_compact"),
    ("SessionStart", "session_start"),
    ("SessionEnd", "session_end"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("SubagentStart", "subagent_start"),
    ("SubagentStop", "subagent_stop"),
    ("Stop", "stop"),
    ("Interrupt", "interrupt"),
];

/// `matcher_pattern_for_event`: these three ignore whatever matcher the group states, so the hashed
/// identity has to drop it too.
const EVENTS_WITHOUT_MATCHER: [&str; 3] = ["UserPromptSubmit", "Stop", "Interrupt"];

/// `normalize_command_hook`.
const SESSION_END_DEFAULT_TIMEOUT_SEC: i64 = 1;
const SESSION_END_MAX_TIMEOUT_SEC: i64 = 3;
const SHORT_TIMEOUT_EVENTS: [&str; 2] = ["SessionEnd", "Interrupt"];
const DEFAULT_TIMEOUT_SEC: i64 = 600;

/// Only these events can emit `additionalContext`, so only they keep an `additionalContextLimit`;
/// and a limit equal to the default is dropped.
const ADDITIONAL_CONTEXT_EVENTS: [&str; 5] = [
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "UserPromptSubmit",
    "SubagentStart",
];
const DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT: i64 = 2500;

/// Handler kinds Codex refuses to load at all: they `continue` before ever being hashed, so they
/// own a handler index but no state entry.
const UNSUPPORTED_HANDLER_TYPES: [&str; 2] = ["prompt", "agent"];

/// The `snake_case` label Codex writes into a state key for `event`.
#[must_use]
pub fn event_key_label(event: &str) -> Option<&'static str> {
    EVENT_KEY_LABELS
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, label)| *label)
}

/// `hook_key` in `codex-rs/hooks/src/lib.rs`.
///
/// # Errors
///
/// Returns an invalid-input error when `event` is not a Codex hook event.
pub fn hook_key(
    key_source: &str,
    event: &str,
    group_index: usize,
    handler_index: usize,
) -> Result<String> {
    let label =
        event_key_label(event).ok_or_else(|| invalid(format!("unknown hook event {event}")))?;
    Ok(format!(
        "{key_source}:{label}:{group_index}:{handler_index}"
    ))
}

/// Splits a state key back into the key source that produced it.
///
/// A key source may itself contain colons (it is an absolute path, or a `<plugin>@<pack>:...`
/// label), so the split is from the right and the three trailing segments have to look like
/// `<label>:<index>:<index>` for the remainder to count as a source.
#[must_use]
pub fn key_source(key: &str) -> Option<&str> {
    let mut parts = key.rsplitn(4, ':');
    let handler = parts.next()?;
    let group = parts.next()?;
    let label = parts.next()?;
    let source = parts.next()?;
    if source.is_empty()
        || handler.parse::<usize>().is_err()
        || group.parse::<usize>().is_err()
        || !EVENT_KEY_LABELS.iter().any(|(_, known)| *known == label)
    {
        return None;
    }
    Some(source)
}

fn normalize_timeout(event: &str, timeout: Option<&Value>) -> Result<i64> {
    let stated = match timeout {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|_| !value.is_boolean())
                .ok_or_else(|| invalid(format!("non-integer timeout in a {event} hook")))?,
        ),
    };
    if SHORT_TIMEOUT_EVENTS.contains(&event) {
        let value = stated.unwrap_or(SESSION_END_DEFAULT_TIMEOUT_SEC);
        return Ok(value.clamp(1, SESSION_END_MAX_TIMEOUT_SEC));
    }
    Ok(stated.unwrap_or(DEFAULT_TIMEOUT_SEC).max(1))
}

fn text_field(handler: &Map<String, Value>, field: &str) -> String {
    handler
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The `HookHandlerConfig` Codex hashes, as the plain JSON object of the fields it serializes.
///
/// Returns `None` for a handler Codex skips -- an empty command, a `prompt`/`agent` hook, an
/// `mcp_tool` hook on `SessionEnd`. Those consume a handler index but get no hash and no state
/// entry.
///
/// # Errors
///
/// Returns an invalid-input error for a handler shape this port cannot reproduce, so a caller
/// re-stamps nothing rather than stamping a wrong hash.
fn normalize_handler(event: &str, handler: &Value) -> Result<Option<Value>> {
    let handler = handler
        .as_object()
        .ok_or_else(|| invalid(format!("a {event} hook handler is not an object")))?;
    let kind = handler.get("type").and_then(Value::as_str).unwrap_or("");
    if UNSUPPORTED_HANDLER_TYPES.contains(&kind) {
        return Ok(None);
    }
    let mut config = Map::new();
    match kind {
        "command" => {
            // `command_windows.unwrap_or(command)` applies only on Windows; this installer is
            // macOS-only, so the plain `command` wins and `commandWindows` normalizes away to
            // `None` (and is therefore dropped).
            let command = text_field(handler, "command");
            if command.trim().is_empty() {
                return Ok(None);
            }
            let asynchronous = match handler.get("async") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(flag)) => *flag,
                Some(_) => {
                    return Err(invalid(format!("non-boolean async in a {event} hook")));
                }
            };
            config.insert("type".to_owned(), Value::from("command"));
            config.insert("command".to_owned(), Value::from(command));
            config.insert(
                "timeout".to_owned(),
                Value::from(normalize_timeout(event, handler.get("timeout"))?),
            );
            config.insert("async".to_owned(), Value::from(asynchronous));
        }
        "mcp_tool" => {
            if event == "SessionEnd" {
                return Ok(None);
            }
            let server = text_field(handler, "server");
            let tool = text_field(handler, "tool");
            if server.trim().is_empty() || tool.trim().is_empty() {
                return Ok(None);
            }
            config.insert("type".to_owned(), Value::from("mcp_tool"));
            config.insert("server".to_owned(), Value::from(server));
            config.insert("tool".to_owned(), Value::from(tool));
            // `input` carries no `skip_serializing_if`, so an empty map is still a key.
            config.insert(
                "input".to_owned(),
                handler
                    .get("input")
                    .filter(|value| value.is_object())
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new())),
            );
            config.insert(
                "timeout".to_owned(),
                Value::from(normalize_timeout(event, handler.get("timeout"))?),
            );
        }
        other => {
            return Err(invalid(format!(
                "unsupported hook handler type {other} in a {event} hook"
            )));
        }
    }
    if let Some(status) = handler
        .get("statusMessage")
        .filter(|value| !value.is_null())
    {
        config.insert("statusMessage".to_owned(), status.clone());
    }
    if kind == "command"
        && ADDITIONAL_CONTEXT_EVENTS.contains(&event)
        && let Some(limit) = handler
            .get("additionalContextLimit")
            .and_then(Value::as_i64)
        && limit != DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT
    {
        config.insert("additionalContextLimit".to_owned(), Value::from(limit));
    }
    Ok(Some(Value::Object(config)))
}

/// `version_for_toml` in `codex-rs/config/src/fingerprint.rs`.
///
/// `serde_json::Map` is a `BTreeMap` here (no `preserve_order` feature anywhere in the workspace),
/// so serializing a `Value` already emits recursively key-sorted, compact JSON -- exactly what
/// upstream produces after its explicit sort. The fixture parity test is what keeps that true.
fn version_for_toml(value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| invalid(format!("failed to serialize a hook identity: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(&bytes)))
}

/// `hook_hash` in `codex-rs/hooks/src/engine/discovery.rs`.
///
/// `matcher` is the *raw* group matcher; this applies `matcher_pattern_for_event` itself. Returns
/// `None` when Codex would skip the handler instead of hashing it.
///
/// # Errors
///
/// Returns an invalid-input error for an unknown event or a handler shape this port cannot
/// reproduce.
pub fn hook_hash(event: &str, matcher: Option<&str>, handler: &Value) -> Result<Option<String>> {
    let label =
        event_key_label(event).ok_or_else(|| invalid(format!("unknown hook event {event}")))?;
    let Some(normalized) = normalize_handler(event, handler)? else {
        return Ok(None);
    };
    let mut identity = Map::new();
    identity.insert("event_name".to_owned(), Value::from(label));
    // `toml::Value::try_from` drops a `None`-valued map entry entirely, so an absent matcher
    // contributes no key rather than a null.
    if let Some(matcher) = matcher.filter(|_| !EVENTS_WITHOUT_MATCHER.contains(&event)) {
        identity.insert("matcher".to_owned(), Value::from(matcher));
    }
    identity.insert("hooks".to_owned(), Value::Array(vec![normalized]));
    version_for_toml(&Value::Object(identity)).map(Some)
}

/// One handler in a `hooks.json`, addressed exactly as Codex addresses it.
pub struct HookHandlerRef<'a> {
    /// `PascalCase`, as written in `hooks.json`.
    pub event: &'static str,
    pub group_index: usize,
    pub handler_index: usize,
    /// Already event-adjusted.
    pub matcher: Option<&'a str>,
    /// The raw entry, before normalization.
    pub handler: &'a Value,
}

/// Walks a parsed `hooks.json` in Codex's own order, yielding addressable handlers.
///
/// Group and handler indices come from position, not from what survives normalization: Codex
/// enumerates first and skips second, so a skipped entry still owns its index.
///
/// # Errors
///
/// Returns an invalid-input error when `hooks` is not an object.
pub fn iter_hook_handlers(hooks_file: &Map<String, Value>) -> Result<Vec<HookHandlerRef<'_>>> {
    let events = match hooks_file.get("hooks") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid("hooks.json's `hooks` is not an object"))?,
    };
    let mut handlers = Vec::new();
    for (event, _) in EVENT_KEY_LABELS {
        let Some(groups) = events.get(event).and_then(Value::as_array) else {
            continue;
        };
        for (group_index, group) in groups.iter().enumerate() {
            let Some(group) = group.as_object() else {
                continue;
            };
            let matcher = if EVENTS_WITHOUT_MATCHER.contains(&event) {
                None
            } else {
                group.get("matcher").and_then(Value::as_str)
            };
            let Some(entries) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for (handler_index, handler) in entries.iter().enumerate() {
                handlers.push(HookHandlerRef {
                    event,
                    group_index,
                    handler_index,
                    matcher,
                    handler,
                });
            }
        }
    }
    Ok(handlers)
}

/// Every `[hooks.state]` key this `hooks.json` *addresses*, whether or not the handler there can be
/// hashed.
///
/// This is the pruning oracle, so it is deliberately positional and never fails on a foreign
/// handler shape: a key the file still addresses must survive even when this port could not
/// reproduce its hash, and a key the file no longer addresses is dead no matter who wrote it.
///
/// # Errors
///
/// Returns an invalid-input error when `hooks` is not an object, or for an event name whose label
/// is unknown (impossible: the walk only yields known events).
pub fn addressed_keys(
    hooks_file: &Map<String, Value>,
    key_source: &str,
) -> Result<BTreeSet<String>> {
    iter_hook_handlers(hooks_file)?
        .into_iter()
        .map(|handler| {
            hook_key(
                key_source,
                handler.event,
                handler.group_index,
                handler.handler_index,
            )
        })
        .collect()
}

/// Every `[hooks.state]` key this `hooks.json` needs, mapped to its trusted hash.
///
/// Used by the doctor check and the parity test; the installer's own re-stamp narrows this to the
/// handlers it wrote itself.
///
/// # Errors
///
/// Returns an invalid-input error for a `hooks.json` shape this port cannot hash.
pub fn hook_state_entries(
    hooks_file: &Map<String, Value>,
    key_source: &str,
) -> Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    for handler in iter_hook_handlers(hooks_file)? {
        let Some(digest) = hook_hash(handler.event, handler.matcher, handler.handler)? else {
            continue;
        };
        entries.insert(
            hook_key(
                key_source,
                handler.event,
                handler.group_index,
                handler.handler_index,
            )?,
            digest,
        );
    }
    Ok(entries)
}

/// What a [`apply_trust`] pass did to a `config.toml`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrustUpdate {
    /// State keys whose `trusted_hash` was written or corrected.
    pub restamped: usize,
    /// Dead state keys removed: keys addressing *our* `hooks.json` at a position it no longer has.
    pub pruned: usize,
}

impl TrustUpdate {
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.restamped > 0 || self.pruned > 0
    }
}

/// Reads `key -> trusted_hash` out of a Codex `config.toml`.
///
/// Entries that are not tables, or whose `trusted_hash` is not a string, are skipped rather than
/// rejected: this is the operator's own hand-written file and a diagnosis must survive its shapes.
#[must_use]
pub fn stored_trust_hashes(document: &DocumentMut) -> BTreeMap<String, String> {
    let Some(state) = document
        .get("hooks")
        .and_then(Item::as_table_like)
        .and_then(|hooks| hooks.get("state"))
        .and_then(Item::as_table_like)
    else {
        return BTreeMap::new();
    };
    state
        .iter()
        .filter_map(|(key, item)| {
            let hash = item
                .as_table_like()?
                .get("trusted_hash")?
                .as_str()?
                .to_owned();
            Some((key.to_owned(), hash))
        })
        .collect()
}

/// Re-stamps `desired` into `[hooks.state]` and prunes the keys `addressed` no longer contains.
///
/// Ownership discipline, and the reason the two arguments are separate sets:
///
/// - `desired` holds only the handlers the installer wrote itself, byte for byte. A hook the
///   operator edited is *not* in it, because writing a `trusted_hash` is granting Codex's
///   execution trust -- `sctx setup` may re-grant that for the command it just authored and must
///   never grant it for a command somebody else authored.
/// - `addressed` holds every position our `hooks.json` still has, ours and the operator's alike, so
///   pruning only ever reclaims keys that address *nothing*. A dead key cannot run a hook, so
///   removing it takes no capability away from anyone.
/// - Keys belonging to any other source -- a project `hooks.json`, a `<plugin>@<pack>` label -- are
///   filtered out by `key_source` and are never read, written, or removed.
///
/// Fields other than `trusted_hash` on an existing entry (`enabled`, anything Codex adds later) are
/// preserved. `toml_edit` keeps comments and formatting for every line it does not touch.
///
/// # Errors
///
/// Returns an invalid-input error when `hooks` or `hooks.state` exists but is not table-like.
pub fn apply_trust(
    document: &mut DocumentMut,
    key_source_path: &str,
    desired: &BTreeMap<String, String>,
    addressed: &BTreeSet<String>,
) -> Result<TrustUpdate> {
    let mut update = TrustUpdate::default();
    if desired.is_empty() && !document.contains_key("hooks") {
        // Nothing to trust and no table to prune: leave a config.toml that has never seen a hook
        // exactly as it is.
        return Ok(update);
    }
    // Table-*like* rather than table throughout: the operator may well have written
    // `hooks = { state = { ... } }`, and the whole point of `toml_edit` here is to give their file
    // back in the shape they wrote it.
    let hooks = document
        .entry("hooks")
        .or_insert_with(|| Item::Table(implicit_table()))
        .as_table_like_mut()
        .ok_or_else(|| invalid("Codex hooks must be a table"))?;
    let state_item = hooks
        .entry("state")
        .or_insert_with(|| Item::Table(Table::new()));
    // A new entry has to match its parent's shape or the render is not valid TOML: a standard
    // table header cannot live inside an inline table.
    let inline_parent = !state_item.is_table();
    let state = state_item
        .as_table_like_mut()
        .ok_or_else(|| invalid("Codex hooks.state must be a table"))?;

    for (key, digest) in desired {
        if let Some(existing) = state.get_mut(key).and_then(Item::as_table_like_mut) {
            if existing.get("trusted_hash").and_then(Item::as_str) == Some(digest.as_str()) {
                continue;
            }
            existing.insert("trusted_hash", value(digest.clone()));
            update.restamped += 1;
            continue;
        }
        let mut entry = Table::new();
        entry.insert("trusted_hash", value(digest.clone()));
        let entry = if inline_parent {
            Item::Value(toml_edit::Value::InlineTable(entry.into_inline_table()))
        } else {
            Item::Table(entry)
        };
        state.insert(key, entry);
        update.restamped += 1;
    }

    let dead = state
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| key_source(key) == Some(key_source_path) && !addressed.contains(key))
        .collect::<Vec<_>>();
    for key in &dead {
        state.remove(key);
        update.pruned += 1;
    }
    Ok(update)
}

fn implicit_table() -> Table {
    let mut table = Table::new();
    // `[hooks]` itself carries nothing; rendering a bare header for it would be noise in a file the
    // operator reads.
    table.set_implicit(true);
    table
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::json;
    use toml_edit::DocumentMut;

    use super::{
        addressed_keys, apply_trust, hook_hash, hook_key, hook_state_entries, key_source,
        stored_trust_hashes,
    };

    fn command_hook(command: &str) -> serde_json::Value {
        json!({"type": "command", "command": command, "statusMessage": "Test"})
    }

    #[test]
    fn a_command_handler_hashes_the_normalized_identity() {
        // The literal is the value `tests/scripts/session_replay/codex_trust.py` produces for the
        // same input; `codex_trust_parity.rs` checks the whole fixture, this pins one hash so a
        // change here fails without the fixture too.
        let digest = hook_hash("PostToolUse", None, &command_hook("/bin/true"))
            .expect("hashable")
            .expect("not skipped");
        let expected = {
            use sha2::{Digest, Sha256};
            let body = br#"{"event_name":"post_tool_use","hooks":[{"async":false,"command":"/bin/true","statusMessage":"Test","timeout":600,"type":"command"}]}"#;
            format!("sha256:{:x}", Sha256::digest(body))
        };
        assert_eq!(digest, expected);
    }

    #[test]
    fn an_absent_matcher_contributes_no_key() {
        let with_matcher = hook_hash("PostToolUse", Some(""), &command_hook("/bin/true")).unwrap();
        let without = hook_hash("PostToolUse", None, &command_hook("/bin/true")).unwrap();
        assert_ne!(with_matcher, without);
    }

    #[test]
    fn the_matcher_is_dropped_for_events_that_ignore_it() {
        for event in ["UserPromptSubmit", "Stop", "Interrupt"] {
            assert_eq!(
                hook_hash(event, Some("anything"), &command_hook("/bin/true")).unwrap(),
                hook_hash(event, None, &command_hook("/bin/true")).unwrap(),
                "{event} must ignore its matcher"
            );
        }
    }

    #[test]
    fn session_end_timeouts_default_to_one_second_and_clamp_to_three() {
        let default = hook_hash("SessionEnd", None, &command_hook("/bin/true")).unwrap();
        let explicit_one = hook_hash(
            "SessionEnd",
            None,
            &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "timeout": 1}),
        )
        .unwrap();
        assert_eq!(default, explicit_one);
        let clamped = hook_hash(
            "SessionEnd",
            None,
            &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "timeout": 900}),
        )
        .unwrap();
        let at_three = hook_hash(
            "SessionEnd",
            None,
            &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "timeout": 3}),
        )
        .unwrap();
        assert_eq!(clamped, at_three);
    }

    #[test]
    fn other_events_default_to_ten_minutes() {
        assert_eq!(
            hook_hash("SessionStart", None, &command_hook("/bin/true")).unwrap(),
            hook_hash(
                "SessionStart",
                None,
                &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "timeout": 600}),
            )
            .unwrap()
        );
    }

    #[test]
    fn an_additional_context_limit_is_kept_only_where_it_can_apply() {
        let plain = hook_hash("SessionStart", None, &command_hook("/bin/true")).unwrap();
        let at_default = hook_hash(
            "SessionStart",
            None,
            &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "additionalContextLimit": 2500}),
        )
        .unwrap();
        assert_eq!(plain, at_default, "a limit at the default is dropped");
        let raised = hook_hash(
            "SessionStart",
            None,
            &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "additionalContextLimit": 9000}),
        )
        .unwrap();
        assert_ne!(plain, raised);
        let on_stop = hook_hash(
            "Stop",
            None,
            &json!({"type": "command", "command": "/bin/true", "statusMessage": "Test", "additionalContextLimit": 9000}),
        )
        .unwrap();
        assert_eq!(
            on_stop,
            hook_hash("Stop", None, &command_hook("/bin/true")).unwrap(),
            "Stop cannot emit additionalContext, so the limit is dropped"
        );
    }

    #[test]
    fn retargeting_the_command_changes_the_hash() {
        // The whole reason the installer has to re-stamp: the command embeds `--agent-version`.
        assert_ne!(
            hook_hash(
                "SessionStart",
                None,
                &command_hook("/a/sctx hook --agent-version 1")
            )
            .unwrap(),
            hook_hash(
                "SessionStart",
                None,
                &command_hook("/a/sctx hook --agent-version 2")
            )
            .unwrap()
        );
    }

    #[test]
    fn handlers_codex_skips_have_no_hash() {
        assert!(
            hook_hash("SessionStart", None, &json!({"type": "prompt"}))
                .unwrap()
                .is_none()
        );
        assert!(
            hook_hash("SessionStart", None, &json!({"type": "agent"}))
                .unwrap()
                .is_none()
        );
        assert!(
            hook_hash("SessionStart", None, &command_hook("   "))
                .unwrap()
                .is_none()
        );
        assert!(
            hook_hash(
                "SessionEnd",
                None,
                &json!({"type": "mcp_tool", "server": "s", "tool": "t"})
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn unknown_events_and_handler_types_are_refused() {
        assert!(hook_hash("NoSuchEvent", None, &command_hook("/bin/true")).is_err());
        assert!(hook_hash("Stop", None, &json!({"type": "webhook"})).is_err());
        assert!(hook_key("/a/hooks.json", "NoSuchEvent", 0, 0).is_err());
    }

    #[test]
    fn an_mcp_tool_handler_keeps_an_empty_input_map() {
        let digest = hook_hash(
            "PostToolUse",
            None,
            &json!({"type": "mcp_tool", "server": "s", "tool": "t"}),
        )
        .unwrap()
        .unwrap();
        let expected = {
            use sha2::{Digest, Sha256};
            let body = br#"{"event_name":"post_tool_use","hooks":[{"input":{},"server":"s","timeout":600,"tool":"t","type":"mcp_tool"}]}"#;
            format!("sha256:{:x}", Sha256::digest(body))
        };
        assert_eq!(digest, expected);
    }

    #[test]
    fn keys_are_positional_and_survive_skipped_handlers() {
        let document = json!({
            "hooks": {
                "SessionStart": [
                    {"hooks": [{"type": "prompt"}, command_hook("/bin/true")]},
                    {"matcher": "x", "hooks": [command_hook("/bin/false")]}
                ]
            }
        });
        let document = document.as_object().unwrap();
        let entries = hook_state_entries(document, "/tmp/hooks.json").unwrap();
        assert_eq!(
            entries.keys().cloned().collect::<Vec<_>>(),
            vec![
                "/tmp/hooks.json:session_start:0:1".to_owned(),
                "/tmp/hooks.json:session_start:1:0".to_owned(),
            ]
        );
        // The skipped handler still owns its address, so pruning must not reclaim it.
        assert!(
            addressed_keys(document, "/tmp/hooks.json")
                .unwrap()
                .contains("/tmp/hooks.json:session_start:0:0")
        );
    }

    #[test]
    fn addressing_a_foreign_handler_shape_does_not_fail_the_walk() {
        let document = json!({"hooks": {"Stop": [{"hooks": [{"type": "webhook"}]}]}});
        let document = document.as_object().unwrap();
        assert!(hook_state_entries(document, "/a/hooks.json").is_err());
        assert_eq!(
            addressed_keys(document, "/a/hooks.json").unwrap().len(),
            1,
            "a key we cannot hash is still a key the file addresses"
        );
    }

    #[test]
    fn a_key_source_may_contain_colons_and_a_foreign_key_shape_is_not_one() {
        assert_eq!(
            key_source("/a:b/hooks.json:stop:0:0"),
            Some("/a:b/hooks.json")
        );
        assert_eq!(
            key_source("plugin@pack:hooks/hooks.json:stop:0:0"),
            Some("plugin@pack:hooks/hooks.json")
        );
        assert_eq!(key_source("/a/hooks.json:stop:0"), None);
        assert_eq!(key_source("/a/hooks.json:no_such_event:0:0"), None);
        assert_eq!(key_source("/a/hooks.json:stop:x:0"), None);
    }

    fn desired(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, hash)| ((*key).to_owned(), (*hash).to_owned()))
            .collect()
    }

    fn addressed(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| (*key).to_owned()).collect()
    }

    #[test]
    fn restamping_corrects_our_hash_and_keeps_every_other_field() {
        let mut document = r#"# the operator's own note
[hooks.state."/h/hooks.json:stop:0:0"]
trusted_hash = "sha256:stale"
enabled = true
"#
        .parse::<DocumentMut>()
        .unwrap();
        let update = apply_trust(
            &mut document,
            "/h/hooks.json",
            &desired(&[("/h/hooks.json:stop:0:0", "sha256:fresh")]),
            &addressed(&["/h/hooks.json:stop:0:0"]),
        )
        .unwrap();
        assert_eq!(update.restamped, 1);
        assert_eq!(update.pruned, 0);
        let rendered = document.to_string();
        assert!(rendered.contains("sha256:fresh"), "{rendered}");
        assert!(rendered.contains("enabled = true"), "{rendered}");
        assert!(
            rendered.contains("# the operator's own note"),
            "a comment must survive: {rendered}"
        );
        // Idempotent: a second pass over the same file changes nothing.
        let again = apply_trust(
            &mut document,
            "/h/hooks.json",
            &desired(&[("/h/hooks.json:stop:0:0", "sha256:fresh")]),
            &addressed(&["/h/hooks.json:stop:0:0"]),
        )
        .unwrap();
        assert!(!again.changed());
    }

    #[test]
    fn a_missing_entry_is_created_and_a_quoted_key_round_trips() {
        // The installer's own test harness uses a HOME with a space and non-ASCII in it, so the
        // state key is never a bare TOML key.
        let key = "/用户 Home 空格/.codex/hooks.json:session_start:0:0";
        let mut document = DocumentMut::new();
        let update = apply_trust(
            &mut document,
            "/用户 Home 空格/.codex/hooks.json",
            &desired(&[(key, "sha256:fresh")]),
            &addressed(&[key]),
        )
        .unwrap();
        assert_eq!(update.restamped, 1);
        let rendered = document.to_string();
        assert!(
            !rendered.starts_with("[hooks]"),
            "the empty parent table must stay implicit: {rendered}"
        );
        let reparsed = rendered.parse::<DocumentMut>().expect("valid TOML");
        assert_eq!(
            stored_trust_hashes(&reparsed).get(key).map(String::as_str),
            Some("sha256:fresh")
        );
    }

    #[test]
    fn pruning_reclaims_our_dead_keys_and_touches_nothing_else() {
        let mut document = r#"[hooks.state."/h/hooks.json:session_start:0:0"]
trusted_hash = "sha256:live"

[hooks.state."/h/hooks.json:pre_tool_use:0:0"]
trusted_hash = "sha256:dead"

[hooks.state."/h/hooks.json:post_tool_use:3:0"]
trusted_hash = "sha256:also-dead"

[hooks.state."/project/.codex/hooks.json:stop:0:0"]
trusted_hash = "sha256:not-ours"

[hooks.state."auto-tracking@ai-metrics:hooks/hooks.json:stop:0:0"]
trusted_hash = "sha256:plugin"

[projects."/somewhere"]
trust_level = "trusted"
"#
        .parse::<DocumentMut>()
        .unwrap();
        let update = apply_trust(
            &mut document,
            "/h/hooks.json",
            &desired(&[("/h/hooks.json:session_start:0:0", "sha256:live")]),
            &addressed(&["/h/hooks.json:session_start:0:0"]),
        )
        .unwrap();
        assert_eq!(update.restamped, 0, "the live hash was already correct");
        assert_eq!(update.pruned, 2);
        let stored = stored_trust_hashes(&document);
        assert_eq!(
            stored.keys().cloned().collect::<Vec<_>>(),
            vec![
                "/h/hooks.json:session_start:0:0".to_owned(),
                "/project/.codex/hooks.json:stop:0:0".to_owned(),
                "auto-tracking@ai-metrics:hooks/hooks.json:stop:0:0".to_owned(),
            ]
        );
        assert!(document.to_string().contains(r#"[projects."/somewhere"]"#));
    }

    #[test]
    fn an_operator_owned_position_in_our_file_is_neither_stamped_nor_pruned() {
        // Group 1 of `Stop` is somebody else's hook that still exists in our hooks.json. It is in
        // `addressed` but not in `desired`, so it keeps whatever trust the operator gave it.
        let mut document = r#"[hooks.state."/h/hooks.json:stop:0:0"]
trusted_hash = "sha256:stale"

[hooks.state."/h/hooks.json:stop:1:0"]
trusted_hash = "sha256:theirs"
"#
        .parse::<DocumentMut>()
        .unwrap();
        let update = apply_trust(
            &mut document,
            "/h/hooks.json",
            &desired(&[("/h/hooks.json:stop:0:0", "sha256:fresh")]),
            &addressed(&["/h/hooks.json:stop:0:0", "/h/hooks.json:stop:1:0"]),
        )
        .unwrap();
        assert_eq!((update.restamped, update.pruned), (1, 0));
        assert_eq!(
            stored_trust_hashes(&document)
                .get("/h/hooks.json:stop:1:0")
                .map(String::as_str),
            Some("sha256:theirs")
        );
    }

    #[test]
    fn a_config_that_has_never_seen_a_hook_is_left_alone() {
        let mut document = "[projects.\"/p\"]\ntrust_level = \"trusted\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let update = apply_trust(
            &mut document,
            "/h/hooks.json",
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(!update.changed());
        assert_eq!(
            document.to_string(),
            "[projects.\"/p\"]\ntrust_level = \"trusted\"\n"
        );
    }

    #[test]
    fn an_inline_state_entry_keeps_its_shape() {
        let mut document = r#"[hooks]
state = { "/h/hooks.json:stop:0:0" = { trusted_hash = "sha256:stale", enabled = false } }
"#
        .parse::<DocumentMut>()
        .unwrap();
        apply_trust(
            &mut document,
            "/h/hooks.json",
            &desired(&[("/h/hooks.json:stop:0:0", "sha256:fresh")]),
            &addressed(&["/h/hooks.json:stop:0:0"]),
        )
        .unwrap();
        let rendered = document.to_string();
        assert!(rendered.contains("sha256:fresh"), "{rendered}");
        assert!(rendered.contains("enabled = false"), "{rendered}");
    }

    #[test]
    fn a_non_table_hooks_section_is_refused_rather_than_overwritten() {
        let mut document = "hooks = 1\n".parse::<DocumentMut>().unwrap();
        assert!(
            apply_trust(
                &mut document,
                "/h/hooks.json",
                &desired(&[("/h/hooks.json:stop:0:0", "sha256:fresh")]),
                &addressed(&["/h/hooks.json:stop:0:0"]),
            )
            .is_err()
        );
    }
}
