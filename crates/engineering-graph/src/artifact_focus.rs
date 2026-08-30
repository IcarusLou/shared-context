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
    /// An absent projection returns no hit rather than an error. The lookup is
    /// read-only, non-blocking (`busy_timeout` is zero), and abandoned once
    /// `budget` elapses.
    ///
    /// # Errors
    ///
    /// Returns typed `SQLite` or Artifact identity errors. Every error is safe
    /// for the caller to degrade into a neutral Hook result.
    pub fn accepted_contexts_for_file(
        &self,
        repository_id: &RepositoryId,
        relative_path: &RepoRelativePath,
        limit: usize,
        budget: Duration,
    ) -> Result<Vec<ArtifactFocusHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Ok(metadata) = std::fs::symlink_metadata(&self.database) else {
            return Ok(Vec::new());
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(Vec::new());
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
                return Ok(Vec::new());
            }
            pairs.push(row.map_err(sql_error("read Artifact focus Reference row"))?);
        }
        drop(references);

        let mut hits = Vec::new();
        for (context_id, revision_id) in pairs {
            if Instant::now() >= deadline {
                return Ok(Vec::new());
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
        Ok(hits)
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
    move |error| Error::new(ErrorKind::Io, format!("{context}: {error}"))
}
