//! Checkpoint metadata index: SQLite, WAL, owned by the daemon.
//!
//! This is plugin-domain data (sessions, tool calls, labels) — never derived
//! from fs authority replay. The `published` flag records whether an authority
//! commit covers a checkpoint: unpublished generations are invisible to fs GC,
//! and any future retention story must know which is which.

use std::path::Path;

use acyclic_fs::{Digest, GenerationId};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{EngineError, Result};

/// One checkpoint row.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointRow {
    pub id: i64,
    pub generation: GenerationId,
    pub created_at: i64,
    pub kind: CheckpointKind,
    pub published: bool,
    pub session_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub label: Option<String>,
    pub error: Option<String>,
}

impl CheckpointRow {
    /// Whether this row's generation is a state a rewind may restore.
    /// `failed` rows carry the generation from BEFORE the failed capture —
    /// restoring one would claim a state the row does not represent.
    pub fn is_restorable(&self) -> bool {
        self.kind != CheckpointKind::Failed
    }
}

/// Why a checkpoint exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointKind {
    Baseline,
    Pre,
    Post,
    Manual,
    PreRewind,
    Recovered,
    Failed,
    Noop,
}

impl CheckpointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Pre => "pre",
            Self::Post => "post",
            Self::Manual => "manual",
            Self::PreRewind => "pre_rewind",
            Self::Recovered => "recovered",
            Self::Failed => "failed",
            Self::Noop => "noop",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "baseline" => Self::Baseline,
            "pre" => Self::Pre,
            "post" => Self::Post,
            "manual" => Self::Manual,
            "pre_rewind" => Self::PreRewind,
            "recovered" => Self::Recovered,
            "failed" => Self::Failed,
            "noop" => Self::Noop,
            other => {
                return Err(EngineError::Store(format!(
                    "unknown checkpoint kind {other}"
                )))
            }
        })
    }
}

/// Attribution attached to a new checkpoint.
#[derive(Clone, Debug, Default)]
pub struct Attribution {
    pub session_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub label: Option<String>,
}

/// Open handle to the index database.
pub struct Index {
    connection: Connection,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(
                key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions(
                session_id TEXT PRIMARY KEY,
                host TEXT,
                started_at INTEGER NOT NULL,
                ended_at INTEGER);
             CREATE TABLE IF NOT EXISTS checkpoints(
                id INTEGER PRIMARY KEY,
                generation BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN
                  ('baseline','pre','post','manual','pre_rewind','recovered','failed','noop')),
                published INTEGER NOT NULL DEFAULT 0,
                session_id TEXT,
                tool_call_id TEXT,
                tool_name TEXT,
                label TEXT,
                error TEXT);
             CREATE INDEX IF NOT EXISTS checkpoints_by_generation
                ON checkpoints(generation);
             CREATE INDEX IF NOT EXISTS checkpoints_by_session
                ON checkpoints(session_id, id);",
        )?;
        Ok(Self { connection })
    }

    /// Records a successful checkpoint; returns the row id.
    pub fn record(
        &mut self,
        generation: GenerationId,
        kind: CheckpointKind,
        attribution: &Attribution,
    ) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO checkpoints
               (generation, created_at, kind, session_id, tool_call_id, tool_name, label)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                generation.digest().as_bytes().as_slice(),
                now(),
                kind.as_str(),
                attribution.session_id,
                attribution.tool_call_id,
                attribution.tool_name,
                attribution.label,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Records a failed capture attempt (no generation advanced: the previous
    /// generation is stored so the timeline stays navigable).
    pub fn record_failure(
        &mut self,
        last_generation: GenerationId,
        error: &str,
        attribution: &Attribution,
    ) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO checkpoints
               (generation, created_at, kind, session_id, tool_call_id, tool_name, error)
             VALUES (?1, ?2, 'failed', ?3, ?4, ?5, ?6)",
            params![
                last_generation.digest().as_bytes().as_slice(),
                now(),
                attribution.session_id,
                attribution.tool_call_id,
                attribution.tool_name,
                error,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Marks every checkpoint up to `through_id` as covered by an authority
    /// commit.
    pub fn mark_published(&mut self, through_id: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE checkpoints SET published = 1 WHERE id <= ?1 AND published = 0",
            params![through_id],
        )?;
        Ok(())
    }

    /// Number of checkpoints not yet covered by an authority commit.
    pub fn unpublished_count(&self) -> Result<u64> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE published = 0 AND kind != 'failed'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn by_id(&self, id: i64) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                "SELECT id, generation, created_at, kind, published,
                        session_id, tool_call_id, tool_name, label, error
                 FROM checkpoints WHERE id = ?1",
                params![id],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Most recent checkpoint a user would rewind to: real snapshots only,
    /// skipping bookkeeping rows (noop, pre_rewind, recovered, failed).
    pub fn latest_target(&self) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                "SELECT id, generation, created_at, kind, published,
                        session_id, tool_call_id, tool_name, label, error
                 FROM checkpoints WHERE kind IN ('baseline','pre','post','manual')
                 ORDER BY id DESC LIMIT 1",
                [],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Most recent checkpoint of any non-failed kind.
    pub fn latest(&self) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                "SELECT id, generation, created_at, kind, published,
                        session_id, tool_call_id, tool_name, label, error
                 FROM checkpoints WHERE kind != 'failed'
                 ORDER BY id DESC LIMIT 1",
                [],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Newest-first listing, optionally scoped to one session.
    pub fn list(&self, session_id: Option<&str>, limit: u32) -> Result<Vec<CheckpointRow>> {
        let mut statement = self.connection.prepare(
            "SELECT id, generation, created_at, kind, published,
                    session_id, tool_call_id, tool_name, label, error
             FROM checkpoints
             WHERE (?1 IS NULL OR session_id = ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![session_id, limit], row_to_checkpoint)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// First checkpoint of one session (the rewind --session-start target).
    pub fn session_start(&self, session_id: &str) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                "SELECT id, generation, created_at, kind, published,
                        session_id, tool_call_id, tool_name, label, error
                 FROM checkpoints WHERE session_id = ?1 AND kind != 'failed'
                 ORDER BY id ASC LIMIT 1",
                params![session_id],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn session_started(&mut self, session_id: &str, host: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO sessions(session_id, host, started_at) VALUES (?1, ?2, ?3)",
            params![session_id, host, now()],
        )?;
        Ok(())
    }

    pub fn session_ended(&mut self, session_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE session_id = ?1",
            params![session_id, now()],
        )?;
        Ok(())
    }
}

fn row_to_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRow> {
    let corrupt = |message: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            message.to_string().into(),
        )
    };
    let generation_bytes: Vec<u8> = row.get(1)?;
    let bytes: [u8; 32] = generation_bytes
        .as_slice()
        .try_into()
        .map_err(|_| corrupt("generation digest is not 32 bytes"))?;
    let kind_text: String = row.get(3)?;
    Ok(CheckpointRow {
        id: row.get(0)?,
        generation: GenerationId::new(Digest::from_bytes(bytes)),
        created_at: row.get(2)?,
        kind: CheckpointKind::parse(&kind_text).map_err(|error| corrupt(&error.to_string()))?,
        published: row.get::<_, i64>(4)? != 0,
        session_id: row.get(5)?,
        tool_call_id: row.get(6)?,
        tool_name: row.get(7)?,
        label: row.get(8)?,
        error: row.get(9)?,
    })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(byte: u8) -> GenerationId {
        GenerationId::new(Digest::from_bytes([byte; 32]))
    }

    #[test]
    fn record_list_publish_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");

        let attribution = Attribution {
            session_id: Some("s1".into()),
            tool_name: Some("Edit".into()),
            ..Attribution::default()
        };
        index
            .record(
                generation(1),
                CheckpointKind::Baseline,
                &Attribution::default(),
            )
            .expect("baseline");
        let post_id = index
            .record(generation(2), CheckpointKind::Post, &attribution)
            .expect("post");
        index
            .record_failure(generation(2), "boom", &attribution)
            .expect("failure");

        assert_eq!(index.unpublished_count().expect("count"), 2);
        index.mark_published(post_id).expect("publish");
        assert_eq!(index.unpublished_count().expect("count"), 0);

        let latest = index.latest().expect("latest").expect("some");
        assert_eq!(latest.generation, generation(2));
        assert_eq!(latest.kind, CheckpointKind::Post);

        let all = index.list(None, 10).expect("list");
        assert_eq!(all.len(), 3);
        let scoped = index.list(Some("s1"), 10).expect("scoped");
        assert_eq!(scoped.len(), 2);

        let start = index
            .session_start("s1")
            .expect("session start")
            .expect("some");
        assert_eq!(start.id, post_id);
    }

    #[test]
    fn latest_target_skips_bookkeeping_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");
        let a = Attribution::default();
        index
            .record(generation(1), CheckpointKind::Baseline, &a)
            .expect("row");
        let post = index
            .record(generation(2), CheckpointKind::Post, &a)
            .expect("row");
        index
            .record(generation(2), CheckpointKind::Noop, &a)
            .expect("row");
        index
            .record(generation(2), CheckpointKind::PreRewind, &a)
            .expect("row");
        index
            .record(generation(3), CheckpointKind::Recovered, &a)
            .expect("row");
        index
            .record_failure(generation(3), "boom", &a)
            .expect("row");

        let target = index.latest_target().expect("query").expect("some");
        assert_eq!(target.id, post);
        assert_eq!(target.kind, CheckpointKind::Post);
    }

    #[test]
    fn failed_rows_are_not_restorable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");
        let a = Attribution::default();
        let ok = index
            .record(generation(1), CheckpointKind::Post, &a)
            .expect("row");
        let bad = index
            .record_failure(generation(1), "boom", &a)
            .expect("row");
        assert!(index.by_id(ok).expect("q").expect("s").is_restorable());
        assert!(!index.by_id(bad).expect("q").expect("s").is_restorable());
    }
}
