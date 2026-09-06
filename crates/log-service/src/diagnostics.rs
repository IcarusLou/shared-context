use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use sctx_telemetry::{EntryPoint, Event, EventKind, Outcome};
use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode, Result, atomic_write_json, fs::read_bounded, layout};

const WINDOW_MS: i64 = 24 * 60 * 60 * 1_000;
const HOUR_MS: i64 = 60 * 60 * 1_000;
const FUTURE_SKEW_MS: i64 = 5 * 60 * 1_000;
const CHECKPOINT_MS: i64 = 5_000;
const MAX_RECENT: usize = 5_000;
const MAX_KEYS_PER_BUCKET: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookReasonCount {
    pub decision: Outcome,
    pub reason: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookDiagnostics {
    pub schema_version: u32,
    pub observed_from_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub total: u64,
    pub counts: Vec<HookReasonCount>,
    pub recent_events: Vec<Event>,
    #[serde(default)]
    hourly: Vec<HourlyBucket>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct HourlyBucket {
    hour_unix_ms: i64,
    counts: Vec<BucketCount>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BucketCount {
    decision: Outcome,
    reason: String,
    count: u64,
}

impl Default for HookDiagnostics {
    fn default() -> Self {
        let now = now_unix_ms();
        Self {
            schema_version: 1,
            observed_from_unix_ms: now,
            updated_at_unix_ms: 0,
            total: 0,
            counts: Vec::new(),
            recent_events: Vec::new(),
            hourly: Vec::new(),
        }
    }
}

/// Standalone deterministic helper. The collector uses an in-memory accumulator and checkpoints
/// every five seconds, so the hot receive loop never reads and rewrites the view per event.
/// Records one decision into a standalone diagnostics checkpoint.
///
/// # Errors
/// Returns an error for a non-hook event or a failed durable checkpoint.
pub fn record_hook_decision(root: &Path, event: &Event) -> Result<()> {
    let mut accumulator = HookDiagnosticAccumulator::load(root);
    accumulator.record(event)?;
    accumulator.checkpoint()
}

/// Loads the bounded rolling Hook diagnostics view.
///
/// # Errors
/// Returns an error when the view is absent, oversized, malformed, or unreadable.
pub fn load_hook_diagnostics(root: &Path) -> Result<HookDiagnostics> {
    let path = layout(root).hook_diagnostics;
    let bytes = match read_bounded(&path, 16 * 1024 * 1024, ErrorCode::InvalidInput) {
        Ok(bytes) => bytes,
        Err(_)
            if std::fs::symlink_metadata(&path)
                .is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Err(Error::new(
                ErrorCode::NotConfigured,
                "hook diagnostics not observed",
            ));
        }
        Err(error) => return Err(error),
    };
    let mut diagnostics: HookDiagnostics = serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("parse hook diagnostics: {error}"),
        )
    })?;
    rebuild_counts(&mut diagnostics, now_unix_ms());
    Ok(diagnostics)
}

pub(crate) struct HookDiagnosticAccumulator {
    path: PathBuf,
    diagnostics: HookDiagnostics,
    dirty: bool,
    last_checkpoint_unix_ms: i64,
}

impl HookDiagnosticAccumulator {
    pub fn load(root: &Path) -> Self {
        let diagnostics = load_hook_diagnostics(root).unwrap_or_default();
        let last_checkpoint_unix_ms = diagnostics.updated_at_unix_ms;
        Self {
            path: layout(root).hook_diagnostics,
            diagnostics,
            dirty: false,
            last_checkpoint_unix_ms,
        }
    }

    pub fn record(&mut self, source: &Event) -> Result<()> {
        if source.entry_point != EntryPoint::Hook || source.kind != EventKind::HookDecision {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "hook diagnostics accepts only HookDecision events",
            ));
        }
        let now = now_unix_ms();
        let mut event = source.clone().normalized();
        event.occurred_at_unix_ms = event.occurred_at_unix_ms.clamp(
            now.saturating_sub(WINDOW_MS),
            now.saturating_add(FUTURE_SKEW_MS),
        );
        let hour = event.occurred_at_unix_ms.div_euclid(HOUR_MS) * HOUR_MS;
        let bucket_index = self
            .diagnostics
            .hourly
            .iter()
            .position(|bucket| bucket.hour_unix_ms == hour)
            .unwrap_or_else(|| {
                self.diagnostics.hourly.push(HourlyBucket {
                    hour_unix_ms: hour,
                    counts: Vec::new(),
                });
                self.diagnostics.hourly.len() - 1
            });
        let reason = event
            .reason
            .clone()
            .unwrap_or_else(|| "unspecified".to_owned());
        let decision = event.outcome;
        let bucket = &mut self.diagnostics.hourly[bucket_index];
        if let Some(item) = bucket
            .counts
            .iter_mut()
            .find(|item| item.decision == decision && item.reason == reason)
        {
            item.count = item.count.saturating_add(1);
        } else if bucket.counts.len() < MAX_KEYS_PER_BUCKET {
            bucket.counts.push(BucketCount {
                decision,
                reason,
                count: 1,
            });
        } else if let Some(item) = bucket
            .counts
            .iter_mut()
            .find(|item| item.decision == decision && item.reason == "other")
        {
            item.count = item.count.saturating_add(1);
        }
        self.diagnostics.recent_events.push(event);
        let excess = self
            .diagnostics
            .recent_events
            .len()
            .saturating_sub(MAX_RECENT);
        if excess > 0 {
            self.diagnostics.recent_events.drain(..excess);
        }
        rebuild_counts(&mut self.diagnostics, now);
        self.diagnostics.updated_at_unix_ms = now;
        self.dirty = true;
        Ok(())
    }

    pub fn checkpoint_if_due(&mut self) -> Result<()> {
        if self.dirty && now_unix_ms().saturating_sub(self.last_checkpoint_unix_ms) >= CHECKPOINT_MS
        {
            self.checkpoint()?;
        }
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        atomic_write_json(&self.path, &self.diagnostics)?;
        self.last_checkpoint_unix_ms = self.diagnostics.updated_at_unix_ms;
        self.dirty = false;
        Ok(())
    }
}

fn rebuild_counts(diagnostics: &mut HookDiagnostics, now: i64) {
    let cutoff = now.saturating_sub(WINDOW_MS);
    diagnostics.hourly.retain(|bucket| {
        bucket.hour_unix_ms.saturating_add(HOUR_MS) > cutoff
            && bucket.hour_unix_ms <= now.saturating_add(FUTURE_SKEW_MS)
    });
    diagnostics.hourly.sort_by_key(|bucket| bucket.hour_unix_ms);
    diagnostics.hourly.truncate(25);
    let mut counts = BTreeMap::<(Outcome, String), u64>::new();
    for bucket in &diagnostics.hourly {
        for item in &bucket.counts {
            let key = (item.decision, item.reason.clone());
            let current = counts.get(&key).copied().unwrap_or(0);
            counts.insert(key, current.saturating_add(item.count));
        }
    }
    diagnostics.total = counts.values().copied().sum();
    diagnostics.counts = counts
        .into_iter()
        .map(|((decision, reason), count)| HookReasonCount {
            decision,
            reason,
            count,
        })
        .collect();
    diagnostics.observed_from_unix_ms = diagnostics
        .hourly
        .first()
        .map_or(now, |bucket| bucket.hour_unix_ms.max(cutoff));
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn aggregate_counts_are_not_limited_by_recent_event_capacity() {
        let root = TempDir::new().expect("root");
        std::fs::create_dir(root.path().join("state")).expect("state");
        let mut accumulator = HookDiagnosticAccumulator::load(root.path());
        let mut event = Event::finished(
            EntryPoint::Hook,
            EventKind::HookDecision,
            "invocation",
            "session_start",
            Outcome::Success,
        );
        event.reason = Some("enabled".to_owned());
        for sequence in 0..6_001 {
            event.sequence = sequence;
            accumulator.record(&event).expect("record");
        }
        assert_eq!(accumulator.diagnostics.total, 6_001);
        assert_eq!(accumulator.diagnostics.recent_events.len(), MAX_RECENT);
        assert_eq!(
            accumulator.diagnostics.counts,
            vec![HookReasonCount {
                decision: Outcome::Success,
                reason: "enabled".to_owned(),
                count: 6_001,
            }]
        );
        accumulator.checkpoint().expect("checkpoint");
        assert_eq!(
            load_hook_diagnostics(root.path()).expect("load").total,
            6_001
        );
    }
}
