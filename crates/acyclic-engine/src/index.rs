//! Checkpoint metadata index: `SQLite`, WAL, owned by the daemon.
//!
//! This is plugin-domain data (sessions, tool calls, labels) — never derived
//! from fs authority replay. The `published` flag records whether an authority
//! commit covers a checkpoint: unpublished generations are invisible to fs GC,
//! and any future retention story must know which is which.

use std::path::Path;

use acyclic_fs::{Digest, GenerationId};
use rusqlite::{Connection, OptionalExtension, params};

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
    /// Conversation turn (1-based, per session) this checkpoint belongs to.
    pub turn: Option<i64>,
    /// For `pre_rewind` rows: the checkpoint the rewind restored. Everything
    /// recorded between `rewind_target` and this row is an abandoned branch.
    pub rewind_target: Option<i64>,
}

/// One conversation turn: the prompt that caused a run of checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnRow {
    pub session_id: String,
    pub turn: i64,
    pub started_at: i64,
    /// Prompt excerpt (bounded, see [`PROMPT_EXCERPT_BYTES`]).
    pub prompt: String,
    pub first_checkpoint: Option<i64>,
    pub last_checkpoint: Option<i64>,
    pub checkpoints: i64,
}

/// One host session as the index knows it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    pub session_id: String,
    pub host: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub checkpoints: i64,
    pub turns: i64,
}

/// Longest prompt excerpt the index keeps. The timeline is an audit log of
/// *which prompt* caused a change, not a transcript store.
pub const PROMPT_EXCERPT_BYTES: usize = 240;

/// The checkpoint columns every query reads, in `row_to_checkpoint` order.
const CHECKPOINT_COLUMNS: &str = "id, generation, created_at, kind, published,
                        session_id, tool_call_id, tool_name, label, error, turn, rewind_target";

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
    /// Taken by the idle timer, not any request: the safety net for hosts
    /// with no lifecycle-hook API (see `Pipeline::auto_checkpoint`).
    Auto,
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
            Self::Auto => "auto",
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
            "auto" => Self::Auto,
            other => {
                return Err(EngineError::Store(format!(
                    "unknown checkpoint kind {other}"
                )));
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
    /// Explicit turn; when absent the session's latest turn is used.
    pub turn: Option<i64>,
    /// Rewind target, for `pre_rewind` rows.
    pub rewind_target: Option<i64>,
}

/// Open handle to the index database.
pub struct Index {
    connection: Connection,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        // The daemon opens a fresh connection per read while the pipeline
        // thread writes: without a busy timeout, even the WAL pragma below
        // (a brief write lock) fails with SQLITE_BUSY mid-capture.
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
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
                  ('baseline','pre','post','manual','pre_rewind','recovered','failed','noop','auto')),
                published INTEGER NOT NULL DEFAULT 0,
                session_id TEXT,
                tool_call_id TEXT,
                tool_name TEXT,
                label TEXT,
                error TEXT);
             CREATE INDEX IF NOT EXISTS checkpoints_by_generation
                ON checkpoints(generation);
             CREATE INDEX IF NOT EXISTS checkpoints_by_session
                ON checkpoints(session_id, id);
             CREATE TABLE IF NOT EXISTS turns(
                session_id TEXT NOT NULL,
                turn INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                prompt TEXT NOT NULL,
                PRIMARY KEY(session_id, turn));",
        )?;
        // Launch 2 columns on a Launch 1 database: additive migration.
        add_column_if_missing(&connection, "checkpoints", "turn", "INTEGER")?;
        add_column_if_missing(&connection, "checkpoints", "rewind_target", "INTEGER")?;
        // `auto` checkpoints (idle-timer safety net) on a database created
        // before that kind existed: its CHECK constraint predates 'auto'.
        ensure_auto_kind_allowed(&connection)?;
        Ok(Self { connection })
    }

    /// Records a successful checkpoint; returns the row id.
    pub fn record(
        &mut self,
        generation: GenerationId,
        kind: CheckpointKind,
        attribution: &Attribution,
    ) -> Result<i64> {
        let turn = self.effective_turn(attribution)?;
        if let Some(session_id) = attribution.session_id.as_deref() {
            self.ensure_session(session_id)?;
        }
        self.connection.execute(
            "INSERT INTO checkpoints
               (generation, created_at, kind, session_id, tool_call_id, tool_name, label,
                turn, rewind_target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                generation.digest().as_bytes().as_slice(),
                now(),
                kind.as_str(),
                attribution.session_id,
                attribution.tool_call_id,
                attribution.tool_name,
                attribution.label,
                turn,
                attribution.rewind_target,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// The turn a new checkpoint belongs to: the explicit one, else the
    /// session's latest recorded turn (hooks don't know turn numbers; the
    /// `UserPromptSubmit` hook records them and tool hooks inherit).
    fn effective_turn(&self, attribution: &Attribution) -> Result<Option<i64>> {
        if attribution.turn.is_some() {
            return Ok(attribution.turn);
        }
        let Some(session_id) = attribution.session_id.as_deref() else {
            return Ok(None);
        };
        let turn: Option<i64> = self.connection.query_row(
            "SELECT MAX(turn) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(turn)
    }

    /// Records a failed capture attempt (no generation advanced: the previous
    /// generation is stored so the timeline stays navigable).
    pub fn record_failure(
        &mut self,
        last_generation: GenerationId,
        error: &str,
        attribution: &Attribution,
    ) -> Result<i64> {
        let turn = self.effective_turn(attribution)?;
        self.connection.execute(
            "INSERT INTO checkpoints
               (generation, created_at, kind, session_id, tool_call_id, tool_name, error, turn)
             VALUES (?1, ?2, 'failed', ?3, ?4, ?5, ?6, ?7)",
            params![
                last_generation.digest().as_bytes().as_slice(),
                now(),
                attribution.session_id,
                attribution.tool_call_id,
                attribution.tool_name,
                error,
                turn,
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
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn by_id(&self, id: i64) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS}
                 FROM checkpoints WHERE id = ?1"
                ),
                params![id],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Most recent checkpoint a user would rewind to: real snapshots only,
    /// skipping bookkeeping rows (noop, `pre_rewind`, recovered, failed).
    pub fn latest_target(&self) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS}
                 FROM checkpoints WHERE kind IN ('baseline','pre','post','manual','auto')
                 ORDER BY id DESC LIMIT 1"
                ),
                [],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Latest real checkpoint strictly before `id`: the state a turn started
    /// from.
    pub fn latest_target_before(&self, id: i64) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS} FROM checkpoints
                     WHERE id < ?1 AND kind IN ('baseline','pre','post','manual','auto')
                     ORDER BY id DESC LIMIT 1"
                ),
                params![id],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Oldest checkpoint on record.
    pub fn oldest(&self) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS} FROM checkpoints
                     WHERE kind != 'failed' ORDER BY id ASC LIMIT 1"
                ),
                [],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Most recent checkpoint of any non-failed kind.
    /// Latest row whose generation hex starts with `prefix` (case-insensitive).
    pub fn by_generation_prefix(&self, prefix: &str) -> Result<Option<CheckpointRow>> {
        let pattern = format!("{}%", prefix.to_uppercase());
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS} FROM checkpoints
                     WHERE hex(generation) LIKE ?1 AND kind != 'failed'
                     ORDER BY id DESC LIMIT 1"
                ),
                params![pattern],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn latest(&self) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS}
                 FROM checkpoints WHERE kind != 'failed'
                 ORDER BY id DESC LIMIT 1"
                ),
                [],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Newest-first listing, optionally scoped to one session.
    pub fn list(
        &self,
        session_id: Option<&str>,
        turn: Option<i64>,
        limit: u32,
    ) -> Result<Vec<CheckpointRow>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHECKPOINT_COLUMNS}
             FROM checkpoints
             WHERE (?1 IS NULL OR session_id = ?1)
               AND (?3 IS NULL OR turn = ?3)
             ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![session_id, limit, turn], row_to_checkpoint)?;
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
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS}
                 FROM checkpoints WHERE session_id = ?1 AND kind != 'failed'
                 ORDER BY id ASC LIMIT 1"
                ),
                params![session_id],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Last real (restorable, non-bookkeeping) checkpoint of one session:
    /// where the session ended up.
    pub fn session_end(&self, session_id: &str) -> Result<Option<CheckpointRow>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {CHECKPOINT_COLUMNS} FROM checkpoints
                     WHERE session_id = ?1 AND kind IN ('baseline','pre','post','manual','auto')
                     ORDER BY id DESC LIMIT 1"
                ),
                params![session_id],
                row_to_checkpoint,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every rewind taken within a row-id window (its `pre_rewind` rows,
    /// oldest first). Each one marks an abandoned branch: the checkpoints
    /// between `rewind_target` and the row itself. Rewinds carry no session
    /// of their own (the CLI runs them), so a session's rewinds are the ones
    /// between its first and last checkpoint.
    pub fn rewinds_between(&self, lo: i64, hi: i64) -> Result<Vec<CheckpointRow>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM checkpoints
             WHERE id >= ?1 AND id <= ?2 AND kind = 'pre_rewind' AND rewind_target IS NOT NULL
             ORDER BY id ASC"
        ))?;
        let rows = statement.query_map(params![lo, hi], row_to_checkpoint)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    /// Real checkpoints strictly between two row ids (an abandoned branch).
    pub fn between(&self, after_id: i64, before_id: i64) -> Result<Vec<CheckpointRow>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM checkpoints
             WHERE id > ?1 AND id < ?2 AND kind IN ('baseline','pre','post','manual','auto')
             ORDER BY id ASC"
        ))?;
        let rows = statement.query_map(params![after_id, before_id], row_to_checkpoint)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    /// Records the start of a conversation turn; returns its 1-based number.
    /// The prompt is truncated to an excerpt on a char boundary.
    pub fn turn_started(&mut self, session_id: &str, prompt: &str) -> Result<i64> {
        self.ensure_session(session_id)?;
        let next: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(turn), 0) + 1 FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT INTO turns(session_id, turn, started_at, prompt) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, next, now(), excerpt(prompt)],
        )?;
        Ok(next)
    }

    /// One turn with its checkpoint range.
    pub fn turn(&self, session_id: &str, turn: i64) -> Result<Option<TurnRow>> {
        self.connection
            .query_row(
                &format!("{TURN_QUERY} WHERE t.session_id = ?1 AND t.turn = ?2"),
                params![session_id, turn],
                row_to_turn,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Turns of one session (or every session), oldest first.
    pub fn turns(&self, session_id: Option<&str>) -> Result<Vec<TurnRow>> {
        let mut statement = self.connection.prepare(&format!(
            "{TURN_QUERY} WHERE (?1 IS NULL OR t.session_id = ?1)
             ORDER BY t.started_at ASC, t.session_id ASC, t.turn ASC"
        ))?;
        let rows = statement.query_map(params![session_id], row_to_turn)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    /// Sessions newest-first with their checkpoint and turn counts.
    pub fn sessions(&self, limit: u32) -> Result<Vec<SessionRow>> {
        let mut statement = self.connection.prepare(&format!(
            "{SESSION_QUERY} ORDER BY s.started_at DESC, s.rowid DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map(params![limit], row_to_session)?;
        rows.map(|row| row.map_err(Into::into)).collect()
    }

    pub fn session(&self, session_id: &str) -> Result<Option<SessionRow>> {
        self.connection
            .query_row(
                &format!("{SESSION_QUERY} WHERE s.session_id = ?1"),
                params![session_id],
                row_to_session,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The most recent session that produced at least one checkpoint,
    /// skipping `exclude` (the session asking: it has nothing to report yet).
    pub fn last_session_with_checkpoints(
        &self,
        exclude: Option<&str>,
    ) -> Result<Option<SessionRow>> {
        self.connection
            .query_row(
                &format!(
                    "{SESSION_QUERY}
                     WHERE (?1 IS NULL OR s.session_id != ?1)
                       AND EXISTS (SELECT 1 FROM checkpoints c
                                   WHERE c.session_id = s.session_id AND c.kind != 'failed')
                     ORDER BY s.started_at DESC, s.rowid DESC LIMIT 1"
                ),
                params![exclude],
                row_to_session,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Most recently started session, if any.
    pub fn latest_session(&self) -> Result<Option<SessionRow>> {
        self.connection
            .query_row(
                &format!("{SESSION_QUERY} ORDER BY s.started_at DESC, s.rowid DESC LIMIT 1"),
                [],
                row_to_session,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn session_started(&mut self, session_id: &str, host: &str) -> Result<()> {
        // A row backfilled by `ensure_session` has no host yet: fill it in,
        // but never move the start of a session that was already seen.
        self.connection.execute(
            "INSERT INTO sessions(session_id, host, started_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET host = COALESCE(sessions.host, excluded.host)",
            params![session_id, host, now()],
        )?;
        Ok(())
    }

    /// A turn or checkpoint for a session the daemon never saw start (its
    /// `SessionStart` hook fired while the daemon was down) still gets a
    /// session row, so `sessions`, `diff --turn`, and `brief` can find it.
    /// The host is unknown at this point; a later `session_started` is
    /// ignored by the primary key, so the row keeps its earliest start.
    fn ensure_session(&mut self, session_id: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO sessions(session_id, host, started_at) VALUES (?1, NULL, ?2)",
            params![session_id, now()],
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
            message.to_owned().into(),
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
        turn: row.get(10)?,
        rewind_target: row.get(11)?,
    })
}

const TURN_QUERY: &str = "SELECT t.session_id, t.turn, t.started_at, t.prompt,
            (SELECT MIN(c.id) FROM checkpoints c
              WHERE c.session_id = t.session_id AND c.turn = t.turn AND c.kind != 'failed'),
            (SELECT MAX(c.id) FROM checkpoints c
              WHERE c.session_id = t.session_id AND c.turn = t.turn AND c.kind != 'failed'),
            (SELECT COUNT(*) FROM checkpoints c
              WHERE c.session_id = t.session_id AND c.turn = t.turn AND c.kind != 'failed')
     FROM turns t";

const SESSION_QUERY: &str = "SELECT s.session_id, s.host, s.started_at, s.ended_at,
            (SELECT COUNT(*) FROM checkpoints c
              WHERE c.session_id = s.session_id AND c.kind != 'failed'),
            (SELECT COUNT(*) FROM turns t WHERE t.session_id = s.session_id)
     FROM sessions s";

fn row_to_turn(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRow> {
    Ok(TurnRow {
        session_id: row.get(0)?,
        turn: row.get(1)?,
        started_at: row.get(2)?,
        prompt: row.get(3)?,
        first_checkpoint: row.get(4)?,
        last_checkpoint: row.get(5)?,
        checkpoints: row.get(6)?,
    })
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: row.get(0)?,
        host: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        checkpoints: row.get(4)?,
        turns: row.get(5)?,
    })
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    declared_type: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(());
        }
    }
    connection.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {declared_type}"),
        [],
    )?;
    Ok(())
}

/// A database created before `auto` checkpoints existed has a `kind` CHECK
/// constraint that predates that variant; `SQLite` has no `ALTER TABLE ...
/// DROP/ADD CONSTRAINT`, so widening it means rebuilding the table. Detected
/// via the stored `CREATE TABLE` text (idempotent: a no-op once rebuilt, and
/// a no-op on a fresh database, whose `CREATE TABLE` already carries 'auto').
fn ensure_auto_kind_allowed(connection: &Connection) -> Result<()> {
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'checkpoints'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(());
    };
    if sql.contains("'auto'") {
        return Ok(());
    }
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         ALTER TABLE checkpoints RENAME TO checkpoints_pre_auto;
         CREATE TABLE checkpoints(
            id INTEGER PRIMARY KEY,
            generation BLOB NOT NULL,
            created_at INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN
              ('baseline','pre','post','manual','pre_rewind','recovered','failed','noop','auto')),
            published INTEGER NOT NULL DEFAULT 0,
            session_id TEXT,
            tool_call_id TEXT,
            tool_name TEXT,
            label TEXT,
            error TEXT,
            turn INTEGER,
            rewind_target INTEGER);
         INSERT INTO checkpoints
           (id, generation, created_at, kind, published, session_id, tool_call_id,
            tool_name, label, error, turn, rewind_target)
         SELECT id, generation, created_at, kind, published, session_id, tool_call_id,
                tool_name, label, error, turn, rewind_target
         FROM checkpoints_pre_auto;
         DROP TABLE checkpoints_pre_auto;
         CREATE INDEX IF NOT EXISTS checkpoints_by_generation ON checkpoints(generation);
         CREATE INDEX IF NOT EXISTS checkpoints_by_session ON checkpoints(session_id, id);
         COMMIT;",
    )?;
    Ok(())
}

/// Bounded, single-line prompt excerpt: whitespace runs collapsed, cut on a
/// char boundary at [`PROMPT_EXCERPT_BYTES`] with an ellipsis.
pub fn excerpt(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= PROMPT_EXCERPT_BYTES {
        return collapsed;
    }
    let mut cut = PROMPT_EXCERPT_BYTES - 1;
    while !collapsed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", collapsed.get(..cut).unwrap_or(&collapsed))
}

fn now() -> i64 {
    crate::unix_now()
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

        let all = index.list(None, None, 10).expect("list");
        assert_eq!(all.len(), 3);
        let scoped = index.list(Some("s1"), None, 10).expect("scoped");
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

    #[test]
    fn turns_and_checkpoints_backfill_a_missing_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");
        // The SessionStart hook fired while the daemon was down; the first
        // thing the daemon hears about "late" is a turn.
        index.turn_started("late", "fix it").expect("turn");
        let sessions = index.sessions(10).expect("sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "late");
        assert_eq!(sessions[0].host, None);
        assert_eq!(sessions[0].turns, 1);

        // A checkpoint alone is enough too.
        let a = Attribution {
            session_id: Some("orphan".into()),
            ..Attribution::default()
        };
        index
            .record(generation(1), CheckpointKind::Post, &a)
            .expect("row");
        let ids: Vec<_> = index
            .sessions(10)
            .expect("sessions")
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert!(ids.contains(&"orphan".to_owned()));

        // A late session-start fills in the host without resetting the row.
        let started = index.sessions(10).expect("s")[0].started_at;
        index.session_started("late", "claude-code").expect("start");
        let late = index
            .sessions(10)
            .expect("sessions")
            .into_iter()
            .find(|s| s.session_id == "late")
            .expect("late");
        assert_eq!(late.host.as_deref(), Some("claude-code"));
        assert_eq!(late.turns, 1);
        assert!(late.started_at <= started);
    }

    #[test]
    fn turns_link_checkpoints_and_resolve_both_ways() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");
        index.session_started("s1", "test").expect("session");
        let session = Attribution {
            session_id: Some("s1".into()),
            ..Attribution::default()
        };
        // Before any turn: attribution has no turn.
        let untethered = index
            .record(generation(1), CheckpointKind::Baseline, &session)
            .expect("row");
        assert_eq!(index.by_id(untethered).expect("q").expect("s").turn, None);

        let t1 = index
            .turn_started("s1", "  fix   the\nJWT   refactor ")
            .expect("turn");
        assert_eq!(t1, 1);
        let a = index
            .record(generation(2), CheckpointKind::Pre, &session)
            .expect("row");
        let b = index
            .record(generation(3), CheckpointKind::Post, &session)
            .expect("row");
        let t2 = index.turn_started("s1", "now the tests").expect("turn");
        assert_eq!(t2, 2);
        let c = index
            .record(generation(4), CheckpointKind::Post, &session)
            .expect("row");

        // checkpoint -> (session, turn, prompt)
        let row = index.by_id(b).expect("q").expect("s");
        assert_eq!(row.turn, Some(1));
        let turn = index.turn("s1", 1).expect("q").expect("s");
        assert_eq!(turn.prompt, "fix the JWT refactor");
        // turn -> checkpoints
        assert_eq!(turn.first_checkpoint, Some(a));
        assert_eq!(turn.last_checkpoint, Some(b));
        assert_eq!(turn.checkpoints, 2);
        let scoped = index.list(Some("s1"), Some(2), 10).expect("list");
        assert_eq!(scoped.iter().map(|row| row.id).collect::<Vec<_>>(), vec![c]);

        let turns = index.turns(Some("s1")).expect("turns");
        assert_eq!(turns.len(), 2);
        let session_row = index.session("s1").expect("q").expect("s");
        assert_eq!(session_row.turns, 2);
        assert_eq!(session_row.checkpoints, 4);
        assert_eq!(
            index.session_end("s1").expect("q").expect("s").id,
            c,
            "session end is the last real checkpoint"
        );
    }

    #[test]
    fn rewinds_mark_abandoned_branches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");
        let session = Attribution {
            session_id: Some("s1".into()),
            ..Attribution::default()
        };
        let keep = index
            .record(generation(1), CheckpointKind::Post, &session)
            .expect("row");
        let lost_a = index
            .record(generation(2), CheckpointKind::Post, &session)
            .expect("row");
        let lost_b = index
            .record(generation(3), CheckpointKind::Post, &session)
            .expect("row");
        let safety = index
            .record(
                generation(3),
                CheckpointKind::PreRewind,
                &Attribution {
                    rewind_target: Some(keep),
                    ..Attribution::default()
                },
            )
            .expect("row");
        index
            .record(generation(1), CheckpointKind::Recovered, &session)
            .expect("row");

        let rewinds = index.rewinds_between(keep, safety + 1).expect("q");
        assert_eq!(rewinds.len(), 1);
        assert_eq!(rewinds[0].id, safety);
        assert_eq!(rewinds[0].rewind_target, Some(keep));
        let abandoned = index.between(keep, safety).expect("q");
        assert_eq!(
            abandoned.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![lost_a, lost_b]
        );
    }

    #[test]
    fn last_session_with_checkpoints_skips_the_asking_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::open(&dir.path().join("index.db")).expect("open");
        index.session_started("old", "h").expect("s");
        index
            .record(
                generation(1),
                CheckpointKind::Post,
                &Attribution {
                    session_id: Some("old".into()),
                    ..Attribution::default()
                },
            )
            .expect("row");
        index.session_started("empty", "h").expect("s");
        index.session_started("now", "h").expect("s");
        let last = index
            .last_session_with_checkpoints(Some("now"))
            .expect("q")
            .expect("some");
        assert_eq!(last.session_id, "old");
        assert!(
            index
                .last_session_with_checkpoints(Some("old"))
                .expect("q")
                .is_none()
        );
    }

    #[test]
    fn excerpt_is_bounded_on_char_boundaries() {
        let long = "é".repeat(400);
        let cut = excerpt(&long);
        assert!(cut.len() <= PROMPT_EXCERPT_BYTES + "…".len());
        assert!(cut.ends_with('…'));
        assert_eq!(excerpt("a  b\n\tc"), "a b c");
    }

    #[test]
    fn launch1_database_migrates_forward() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.db");
        {
            let connection = Connection::open(&path).expect("open");
            connection
                .execute_batch(
                    "CREATE TABLE checkpoints(
                        id INTEGER PRIMARY KEY, generation BLOB NOT NULL,
                        created_at INTEGER NOT NULL, kind TEXT NOT NULL,
                        published INTEGER NOT NULL DEFAULT 0, session_id TEXT,
                        tool_call_id TEXT, tool_name TEXT, label TEXT, error TEXT);
                     INSERT INTO checkpoints(generation, created_at, kind)
                        VALUES (zeroblob(32), 1, 'post');",
                )
                .expect("seed");
        }
        let index = Index::open(&path).expect("migrate");
        let row = index.latest().expect("q").expect("row");
        assert_eq!(row.turn, None);
        assert_eq!(row.rewind_target, None);
    }

    /// A database from before `auto` checkpoints existed has a `kind` CHECK
    /// constraint that predates that variant. `Index::open` must widen it
    /// (`SQLite` can't `ALTER ... DROP CONSTRAINT`, so this is a table
    /// rebuild) without losing any existing row.
    #[test]
    fn pre_auto_kind_database_is_rebuilt_and_keeps_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.db");
        {
            let connection = Connection::open(&path).expect("open");
            connection
                .execute_batch(
                    "CREATE TABLE checkpoints(
                        id INTEGER PRIMARY KEY, generation BLOB NOT NULL,
                        created_at INTEGER NOT NULL,
                        kind TEXT NOT NULL CHECK(kind IN
                          ('baseline','pre','post','manual','pre_rewind','recovered','failed','noop')),
                        published INTEGER NOT NULL DEFAULT 0, session_id TEXT,
                        tool_call_id TEXT, tool_name TEXT, label TEXT, error TEXT,
                        turn INTEGER, rewind_target INTEGER);
                     INSERT INTO checkpoints(generation, created_at, kind)
                        VALUES (zeroblob(32), 1, 'post');",
                )
                .expect("seed");
        }
        let mut index = Index::open(&path).expect("migrate");

        // The pre-existing row survived the rebuild untouched.
        let existing = index.latest().expect("q").expect("row");
        assert_eq!(existing.kind, CheckpointKind::Post);

        // The widened constraint now accepts 'auto'.
        let row_id = index
            .record(generation(1), CheckpointKind::Auto, &Attribution::default())
            .expect("auto checkpoint should now be a valid kind");
        let row = index.by_id(row_id).expect("q").expect("row");
        assert_eq!(row.kind, CheckpointKind::Auto);
    }
}
