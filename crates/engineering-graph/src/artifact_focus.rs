//! Read-only Artifact focus lookup for the P4.1 Hook reminder experiment.
//!
//! This module is deliberately the narrowest possible reader of
//! `state/engineering.sqlite`: it opens the existing projection read-only, takes
//! no lock, runs no `PRAGMA journal_mode`, and never rebuilds, scans, or invokes
//! Git. It answers exactly one question — "does the current Graph generation
//! already associate accepted Context with this exact File Artifact?" — and gives
//! up on its own deadline instead of waiting.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sctx_domain::{
    ArtifactKey, ArtifactLocator, Error, ErrorKind, RepoRelativePath, RepositoryId, Result,
};

/// Upper bound on Contexts one bounded reminder lookup may return.
pub const MAX_ARTIFACT_FOCUS_HITS: usize = 3;

/// Default wall-clock budget for one read-only lookup. Exceeding it yields no
/// hit, which the caller renders as a neutral Hook result.
pub const ARTIFACT_FOCUS_QUERY_BUDGET: Duration = Duration::from_millis(150);

/// One accepted Graph Context associated with the queried File Artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactFocusHit {
    pub context_id: String,
    /// The accepted revision statement. Callers truncate it for display; this
    /// type performs no presentation policy.
    pub statement: String,
}

/// Why a lookup returned no hit without also returning an error.
///
/// Both variants render identically to a caller's reminder policy — no hit, no prompt — but
/// they are distinguishable diagnostic outcomes so a Hook-path caller can record which one
/// happened instead of treating every empty result the same way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactFocusOutcome {
    /// The lookup ran to completion; `hits` reflects every match found (possibly empty).
    Completed,
    /// The `state/engineering.sqlite` projection file does not exist or is not a plain file.
    ProjectionAbsent,
    /// The wall-clock budget elapsed before the lookup could finish.
    BudgetExceeded,
}

/// Full result of one bounded, read-only lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactFocusLookup {
    pub hits: Vec<ArtifactFocusHit>,
    pub outcome: ArtifactFocusOutcome,
}

/// Bounded read-only reader for one installation's Engineering projection.
#[derive(Clone, Debug)]
pub struct ArtifactFocusReader {
    database: PathBuf,
}

impl ArtifactFocusReader {
    /// Binds the reader to `<root>/state/engineering.sqlite` without opening it.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            database: root.as_ref().join("state").join("engineering.sqlite"),
        }
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Returns accepted Graph Contexts associated with one exact File Artifact.
    ///
    /// An absent projection or an exhausted query budget both report no hit through
    /// [`ArtifactFocusLookup::outcome`] rather than as an error — callers that only render a
    /// reminder can keep treating both as "no hit"; callers that want to diagnose the Hook path
    /// can distinguish them. The lookup is read-only and abandoned once `budget` elapses.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::MaintenanceBusy`] when the read-only connection or a query hits
    /// `SQLITE_BUSY`/`SQLITE_LOCKED` (the projection is non-blocking, so this is immediate), and
    /// typed `SQLite` or Artifact identity errors otherwise. Every error is safe for the caller
    /// to degrade into a neutral Hook result.
    pub fn accepted_contexts_for_file(
        &self,
        repository_id: &RepositoryId,
        relative_path: &RepoRelativePath,
        limit: usize,
        budget: Duration,
    ) -> Result<ArtifactFocusLookup> {
        if limit == 0 {
            return Ok(ArtifactFocusLookup {
                hits: Vec::new(),
                outcome: ArtifactFocusOutcome::Completed,
            });
        }
        let Ok(metadata) = std::fs::symlink_metadata(&self.database) else {
            return Ok(ArtifactFocusLookup {
                hits: Vec::new(),
                outcome: ArtifactFocusOutcome::ProjectionAbsent,
            });
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(ArtifactFocusLookup {
                hits: Vec::new(),
                outcome: ArtifactFocusOutcome::ProjectionAbsent,
            });
        }
        let deadline = Instant::now() + budget;
        let digest = ArtifactKey::derive(
            repository_id.clone(),
            ArtifactLocator::File {
                path: relative_path.clone(),
            },
        )?
        .digest()
        .to_owned();
        let connection = self.open_read_only()?;
        let mut references = connection
            .prepare(
                "SELECT DISTINCT
                     json_extract(payload_json, '$.context_id') AS context_id,
                     json_extract(payload_json, '$.revision_id') AS revision_id
                 FROM resolved_reference
                 WHERE json_extract(payload_json, '$.association.artifact_key.digest') = ?1
                 ORDER BY context_id ASC, revision_id ASC
                 LIMIT ?2",
            )
            .map_err(sql_error("prepare Artifact focus Reference lookup"))?;
        let rows = references
            .query_map(
                params![digest, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(sql_error("query Artifact focus References"))?;
        let mut pairs = Vec::new();
        for row in rows {
            if Instant::now() >= deadline {
                return Ok(ArtifactFocusLookup {
                    hits: Vec::new(),
                    outcome: ArtifactFocusOutcome::BudgetExceeded,
                });
            }
            pairs.push(row.map_err(sql_error("read Artifact focus Reference row"))?);
        }
        drop(references);

        let mut hits = Vec::new();
        for (context_id, revision_id) in pairs {
            if Instant::now() >= deadline {
                return Ok(ArtifactFocusLookup {
                    hits: Vec::new(),
                    outcome: ArtifactFocusOutcome::BudgetExceeded,
                });
            }
            let statement = connection
                .query_row(
                    "SELECT json_extract(payload_json, '$.revision.statement')
                     FROM graph_context_snapshot
                     WHERE context_id = ?1
                       AND revision_id = ?2
                       AND json_extract(payload_json, '$.status') = 'accepted'
                       AND json_extract(
                               payload_json,
                               '$.safety.automatic_injection_eligible'
                           ) = 1",
                    params![context_id, revision_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(sql_error("read Artifact focus Context snapshot"))?
                .flatten();
            if let Some(statement) = statement {
                if !hits
                    .iter()
                    .any(|hit: &ArtifactFocusHit| hit.context_id == context_id)
                {
                    hits.push(ArtifactFocusHit {
                        context_id,
                        statement,
                    });
                }
            }
        }
        hits.truncate(limit);
        Ok(ArtifactFocusLookup {
            hits,
            outcome: ArtifactFocusOutcome::Completed,
        })
    }

    fn open_read_only(&self) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_error("open Engineering projection read-only"))?;
        connection
            .busy_timeout(Duration::ZERO)
            .map_err(sql_error("disable Artifact focus busy waiting"))?;
        Ok(connection)
    }
}

fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| {
        if is_busy(&error) {
            return Error::new(
                ErrorKind::MaintenanceBusy,
                format!("{context}: Engineering projection is locked by another process"),
            );
        }
        Error::new(ErrorKind::Io, format!("{context}: {error}"))
    }
}

/// Whether one `rusqlite` error is `SQLITE_BUSY` or `SQLITE_LOCKED`, the two codes a
/// non-blocking (`busy_timeout(Duration::ZERO)`) read-only connection surfaces when another
/// process currently holds the projection.
fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if matches!(
                sqlite_error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_error_classifies_busy_and_locked_as_maintenance_busy() {
        const SQLITE_BUSY: std::ffi::c_int = 5;
        const SQLITE_LOCKED: std::ffi::c_int = 6;
        const SQLITE_IOERR: std::ffi::c_int = 10;

        for code in [SQLITE_BUSY, SQLITE_LOCKED] {
            let raw = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            let mapped = sql_error("probe")(raw);
            assert_eq!(mapped.kind(), ErrorKind::MaintenanceBusy);
        }

        let raw = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(SQLITE_IOERR), None);
        let mapped = sql_error("probe")(raw);
        assert_eq!(mapped.kind(), ErrorKind::Io);
    }
}
