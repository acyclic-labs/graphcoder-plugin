//! Speculation: results computed before anything asked for them.
//!
//! The load-bearing idea is that **correctness comes from the key, not from
//! invalidation**. A speculative result is identified by the generation(s) it
//! describes, and a `GenerationId` is a Merkle id — equal id means a
//! bit-identical tree. So a result computed against a tree that has since
//! moved simply never matches the key a later request builds, and serving a
//! near-miss is structurally impossible. Every cancellation path in the
//! daemon is therefore a way to stop *spending*, never a way to stay correct.
//!
//! This module owns the key, the store and the metrics. It runs nothing and
//! spawns nothing; see `speculate.rs` in the `acyclic` crate for scheduling
//! and child processes.
//!
//! The store is `spec.db`, deliberately separate from `index.db`. The
//! pipeline thread owns the index connection and writes to it synchronously,
//! so a second writer contending for `SQLite`'s single write lock would block
//! the thread that every hook call waits on. `spec.db` is also pure cache:
//! if it is missing, corrupt, or deleted, speculation degrades to off and
//! nothing else notices.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

use crate::{GenerationId, Result};

/// What a speculative result answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecKind {
    /// The previous-session brief the `SessionStart` hook prints.
    Brief,
    /// A blast-radius diff between two generations.
    Diff,
    /// A live fork's diff against its base.
    ForkDiff,
    /// Prose describing one conversation turn (the only kind that costs
    /// tokens).
    Summary,
}

impl SpecKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Brief => "brief",
            Self::Diff => "diff",
            Self::ForkDiff => "fork-diff",
            Self::Summary => "summary",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Brief, Self::Diff, Self::ForkDiff, Self::Summary]
            .into_iter()
            .find(|kind| kind.as_str() == raw)
    }

    /// Whether producing this kind runs a model, and so spends money.
    pub fn spends_tokens(self) -> bool {
        matches!(self, Self::Summary)
    }
}

/// What identifies a speculative result.
///
/// Generations rather than turn numbers: a turn number is monotonic, so
/// turn 7's summary would still "match" long after the tree moved under it.
/// Generations are hex here rather than blobs because nothing ever needs to
/// reconstruct a `GenerationId` from a row — they are only ever compared —
/// and hex keeps `sqlite3 spec.db` readable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpecKey {
    pub kind: SpecKind,
    /// The generation the answer starts from; empty when not applicable.
    pub before: String,
    /// The generation the answer describes.
    pub after: String,
    /// Discriminator within a kind: a fork id, a session id, or empty.
    pub scope: String,
    /// Renderer version, model and prompt template. In the key so that
    /// changing the model stops serving the previous model's prose rather
    /// than silently mixing the two.
    pub recipe: String,
}

impl SpecKey {
    /// A key spanning two generations.
    pub fn range(kind: SpecKind, before: GenerationId, after: GenerationId) -> Self {
        Self {
            kind,
            before: crate::generation_hex(before),
            after: crate::generation_hex(after),
            scope: String::new(),
            recipe: String::new(),
        }
    }

    /// A key describing one generation (a brief, a fork's current state).
    pub fn at(kind: SpecKind, after: GenerationId) -> Self {
        Self {
            kind,
            before: String::new(),
            after: crate::generation_hex(after),
            scope: String::new(),
            recipe: String::new(),
        }
    }

    #[must_use]
    pub fn scoped(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    #[must_use]
    pub fn with_recipe(mut self, recipe: impl Into<String>) -> Self {
        self.recipe = recipe.into();
        self
    }
}

/// Where a speculation got to. Only `Ready` is ever served.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecState {
    Running,
    Ready,
    Failed,
    Timeout,
    Overflow,
    Cancelled,
    Orphaned,
}

impl SpecState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Overflow => "overflow",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        [
            Self::Running,
            Self::Ready,
            Self::Failed,
            Self::Timeout,
            Self::Overflow,
            Self::Cancelled,
            Self::Orphaned,
        ]
        .into_iter()
        .find(|state| state.as_str() == raw)
    }
}

/// How a speculation ended.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    Ready { body: String },
    Failed(String),
    Timeout,
    Overflow,
    Cancelled,
}

impl RunOutcome {
    fn state(&self) -> SpecState {
        match *self {
            Self::Ready { .. } => SpecState::Ready,
            Self::Failed(_) => SpecState::Failed,
            Self::Timeout => SpecState::Timeout,
            Self::Overflow => SpecState::Overflow,
            Self::Cancelled => SpecState::Cancelled,
        }
    }
}

/// A claimed result and how early it was ready.
#[derive(Clone, Debug)]
pub struct SpecHit {
    pub body: String,
    pub bytes: u64,
    /// Milliseconds between the result landing and this claim: how much
    /// waiting the speculation actually saved.
    pub lead_ms: i64,
}

/// One line in the fire/claim log.
#[derive(Clone, Debug)]
pub struct SpecEventRow {
    pub event: SpecEvent,
    pub kind: SpecKind,
    pub session_id: Option<String>,
    pub turn: Option<i64>,
    pub lead_ms: Option<i64>,
    pub wall_ms: Option<i64>,
    pub bytes: Option<u64>,
    pub detail: Option<String>,
}

/// What happened, from the scheduler's point of view. `ClaimMiss` is the
/// reason this log exists at all: a miss leaves no row in `spec_cache`, so
/// without it there is no way to compute a hit rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecEvent {
    Spawn,
    Ready,
    Fail,
    Timeout,
    Cancel,
    /// Scheduler queue was full, so the speculation never started.
    Drop,
    ClaimHit,
    ClaimMiss,
    /// Key matched, but the run was still in flight and the caller waited.
    ClaimJoin,
}

impl SpecEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Ready => "ready",
            Self::Fail => "fail",
            Self::Timeout => "timeout",
            Self::Cancel => "cancel",
            Self::Drop => "drop",
            Self::ClaimHit => "claim_hit",
            Self::ClaimMiss => "claim_miss",
            Self::ClaimJoin => "claim_join",
        }
    }
}

/// What `acyclic status` reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpecMetrics {
    pub runs: u64,
    pub claimed: u64,
    pub missed: u64,
    pub timeouts: u64,
    pub bytes_out: u64,
    pub median_lead_ms: Option<i64>,
}

impl SpecMetrics {
    /// Claims as a percentage of claim attempts, or `None` if nothing has
    /// asked yet. This — not the run count — is the number that says whether
    /// speculation is earning its keep.
    pub fn claim_rate(&self) -> Option<u32> {
        let attempts = self.claimed.checked_add(self.missed)?;
        if attempts == 0 {
            return None;
        }
        let rate = self.claimed.checked_mul(100)?.checked_div(attempts)?;
        u32::try_from(rate).ok()
    }
}

/// The identity of one in-flight speculation (a `spec_cache` row id).
pub type RunId = i64;

/// Open handle to `spec.db`.
pub struct SpecStore {
    connection: Connection,
}

impl SpecStore {
    /// Opens (creating if absent) the speculation cache.
    ///
    /// `busy_timeout` is a caller decision on purpose: the scheduler can
    /// afford to wait seconds, but the `SessionStart` hook path — the one
    /// the agent genuinely blocks on — must pass a short timeout so a wedged
    /// cache degrades to "compute it normally" instead of stalling a session
    /// start.
    pub fn open(path: &Path, busy_timeout: Duration) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(busy_timeout)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "user_version", 1)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS spec_cache(
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                before_gen TEXT NOT NULL DEFAULT '',
                after_gen TEXT NOT NULL,
                scope TEXT NOT NULL DEFAULT '',
                recipe TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL CHECK(state IN
                  ('running','ready','failed','timeout','overflow','cancelled','orphaned')),
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                claimed_at INTEGER,
                claims INTEGER NOT NULL DEFAULT 0,
                body TEXT,
                bytes INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                session_id TEXT,
                turn INTEGER);
             CREATE UNIQUE INDEX IF NOT EXISTS spec_cache_key
                ON spec_cache(kind, before_gen, after_gen, scope, recipe);
             CREATE TABLE IF NOT EXISTS spec_log(
                id INTEGER PRIMARY KEY,
                at INTEGER NOT NULL,
                event TEXT NOT NULL,
                kind TEXT NOT NULL,
                session_id TEXT,
                turn INTEGER,
                lead_ms INTEGER,
                wall_ms INTEGER,
                bytes INTEGER,
                detail TEXT);
             CREATE INDEX IF NOT EXISTS spec_log_at ON spec_log(at);",
        )?;
        Ok(Self { connection })
    }

    /// Reserves a key for a speculation about to start.
    ///
    /// `Ok(None)` means "someone already has this" — the answer is either
    /// ready or being produced right now. The unique index is what makes
    /// that check atomic, so this doubles as the admission gate: two
    /// schedulers racing the same key cannot both spend.
    pub fn begin(
        &mut self,
        key: &SpecKey,
        session_id: Option<&str>,
        turn: Option<i64>,
    ) -> Result<Option<RunId>> {
        // A previous attempt that failed, timed out or was cancelled must
        // not hold the key forever. Rate limiting retries is the scheduler's
        // job (`min_run_interval_ms`, `max_runs_per_session`), not the
        // store's.
        self.connection.execute(
            "DELETE FROM spec_cache
              WHERE kind = ?1 AND before_gen = ?2 AND after_gen = ?3
                AND scope = ?4 AND recipe = ?5
                AND state NOT IN ('running', 'ready')",
            (
                key.kind.as_str(),
                &key.before,
                &key.after,
                &key.scope,
                &key.recipe,
            ),
        )?;
        let inserted = self.connection.execute(
            "INSERT INTO spec_cache
                (kind, before_gen, after_gen, scope, recipe, state, started_at,
                 session_id, turn)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8)
             ON CONFLICT(kind, before_gen, after_gen, scope, recipe) DO NOTHING",
            (
                key.kind.as_str(),
                &key.before,
                &key.after,
                &key.scope,
                &key.recipe,
                crate::unix_now(),
                session_id,
                turn,
            ),
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        Ok(Some(self.connection.last_insert_rowid()))
    }

    /// Records how a reserved speculation ended. A row moves out of
    /// `running` exactly once, and a body is stored only on success — so a
    /// half-finished run is never visible to a claim.
    pub fn finish(&mut self, run: RunId, outcome: &RunOutcome) -> Result<()> {
        let (body, error) = match outcome {
            RunOutcome::Ready { body } => (Some(body.as_str()), None),
            RunOutcome::Failed(message) => (None, Some(message.as_str())),
            _ => (None, None),
        };
        let bytes = i64::try_from(body.map_or(0, str::len)).unwrap_or(i64::MAX);
        self.connection.execute(
            "UPDATE spec_cache
                SET state = ?2, finished_at = ?3, body = ?4, bytes = ?5, error = ?6
              WHERE id = ?1 AND state = 'running'",
            (
                run,
                outcome.state().as_str(),
                crate::unix_now(),
                body,
                bytes,
                error,
            ),
        )?;
        Ok(())
    }

    /// Serves a result, if and only if the key matches exactly and the
    /// answer is ready. There is no near-miss path by construction.
    pub fn claim(&mut self, key: &SpecKey) -> Result<Option<SpecHit>> {
        let now = crate::unix_now();
        let hit = self
            .connection
            .query_row(
                "UPDATE spec_cache
                    SET claimed_at = ?6, claims = claims + 1
                  WHERE kind = ?1 AND before_gen = ?2 AND after_gen = ?3
                    AND scope = ?4 AND recipe = ?5
                    AND state = 'ready' AND body IS NOT NULL
              RETURNING body, bytes, finished_at",
                (
                    key.kind.as_str(),
                    &key.before,
                    &key.after,
                    &key.scope,
                    &key.recipe,
                    now,
                ),
                |row| {
                    let body: String = row.get(0)?;
                    let bytes: i64 = row.get(1)?;
                    let finished_at: Option<i64> = row.get(2)?;
                    Ok(SpecHit {
                        body,
                        bytes: u64::try_from(bytes).unwrap_or(0),
                        lead_ms: finished_at
                            .map(|at| now.saturating_sub(at).saturating_mul(1_000))
                            .unwrap_or_default(),
                    })
                },
            )
            .optional()?;
        Ok(hit)
    }

    /// The state of a key without claiming it — for the caller that would
    /// rather wait for an in-flight run than miss.
    pub fn state_of(&self, key: &SpecKey) -> Result<Option<SpecState>> {
        let state: Option<String> = self
            .connection
            .query_row(
                "SELECT state FROM spec_cache
                  WHERE kind = ?1 AND before_gen = ?2 AND after_gen = ?3
                    AND scope = ?4 AND recipe = ?5",
                (
                    key.kind.as_str(),
                    &key.before,
                    &key.after,
                    &key.scope,
                    &key.recipe,
                ),
                |row| row.get(0),
            )
            .optional()?;
        Ok(state.as_deref().and_then(SpecState::parse))
    }

    /// Marks every run a dead daemon left mid-flight. Called at open: a
    /// `running` row whose process is gone would otherwise block its key
    /// forever, since `running` is never retryable.
    pub fn sweep_orphans(&mut self) -> Result<u64> {
        let swept = self.connection.execute(
            "UPDATE spec_cache SET state = 'orphaned', finished_at = ?1
              WHERE state = 'running'",
            (crate::unix_now(),),
        )?;
        Ok(u64::try_from(swept).unwrap_or(0))
    }

    /// Drops results past their TTL, then trims to `max_rows` oldest-first.
    /// This is the *only* thing that removes cache entries: a stale result
    /// is already unreachable via its key, so eviction is about disk, not
    /// correctness.
    pub fn gc(&mut self, ttl: Duration, max_rows: u32) -> Result<u64> {
        let ttl_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let cutoff = crate::unix_now().saturating_sub(ttl_secs);
        let mut removed = self.connection.execute(
            "DELETE FROM spec_cache WHERE state != 'running' AND started_at < ?1",
            (cutoff,),
        )?;
        removed = removed.saturating_add(self.connection.execute(
            "DELETE FROM spec_cache WHERE id IN (
                 SELECT id FROM spec_cache WHERE state != 'running'
                  ORDER BY started_at DESC LIMIT -1 OFFSET ?1)",
            (max_rows,),
        )?);
        self.connection
            .execute("DELETE FROM spec_log WHERE at < ?1", (cutoff,))?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    /// Appends one line to the fire/claim log.
    pub fn log(&mut self, row: &SpecEventRow) -> Result<()> {
        self.connection.execute(
            "INSERT INTO spec_log
                (at, event, kind, session_id, turn, lead_ms, wall_ms, bytes, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            (
                crate::unix_now(),
                row.event.as_str(),
                row.kind.as_str(),
                row.session_id.as_deref(),
                row.turn,
                row.lead_ms,
                row.wall_ms,
                row.bytes
                    .map(|bytes| i64::try_from(bytes).unwrap_or(i64::MAX)),
                row.detail.as_deref(),
            ),
        )?;
        Ok(())
    }

    /// Rolls the log up over a recent window.
    pub fn metrics(&self, window: Duration) -> Result<SpecMetrics> {
        let window_secs = i64::try_from(window.as_secs()).unwrap_or(i64::MAX);
        let since = crate::unix_now().saturating_sub(window_secs);
        let mut metrics = SpecMetrics::default();
        let mut counts = self.connection.prepare(
            "SELECT event, COUNT(*), COALESCE(SUM(bytes), 0) FROM spec_log
                       WHERE at >= ?1 GROUP BY event",
        )?;
        let mut rows = counts.query((since,))?;
        while let Some(row) = rows.next()? {
            let event: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let bytes: i64 = row.get(2)?;
            let count = u64::try_from(count).unwrap_or(0);
            match event.as_str() {
                "spawn" => metrics.runs = count,
                "ready" => metrics.bytes_out = u64::try_from(bytes).unwrap_or(0),
                "timeout" => metrics.timeouts = count,
                "claim_hit" => metrics.claimed = count,
                "claim_miss" => metrics.missed = count,
                _ => {}
            }
        }
        metrics.median_lead_ms = self.median_lead(since)?;
        Ok(metrics)
    }

    /// Median over the window's claim leads. Computed here rather than in
    /// SQL because `SQLite` has no median and the row count is bounded by the
    /// log's own TTL.
    fn median_lead(&self, since: i64) -> Result<Option<i64>> {
        let mut statement = self.connection.prepare(
            "SELECT lead_ms FROM spec_log
              WHERE at >= ?1 AND event = 'claim_hit' AND lead_ms IS NOT NULL
              ORDER BY lead_ms",
        )?;
        let leads = statement
            .query_map((since,), |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(leads.get(leads.len() / 2).copied())
    }
}

/// Per-developer speculation settings, read from a file of its own.
///
/// Deliberately NOT a field on [`crate::config::Config`]. That struct is
/// `deny_unknown_fields`, so a new table there would make an older binary
/// fail `Config::load` — and with it every CLI verb and the daemon — on any
/// machine that had opted in. It is also the wrong place politically: the
/// repo config is checked in, and whether to spend tokens is a personal
/// decision, not one a teammate should inherit from a commit.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SpeculateConfig {
    /// Master switch. Off means nothing is scheduled, nothing is cached.
    pub enabled: bool,
    /// Which kinds to speculate. `summary` is the one that spends money, so
    /// enabling speculation and enabling spend are two separate decisions.
    pub kinds: Vec<String>,
    /// Argv of the model command, e.g.
    /// `["claude", "-p", "--model", "claude-haiku-4-5-20251001"]`. Never a
    /// shell string: the prompt goes on stdin, not the command line.
    ///
    /// The program must be on `PATH` or absolute. A relative path would be
    /// resolved against the run's scratch directory, which is empty by
    /// design — the child is given no route into the repository.
    pub command: Vec<String>,
    /// Informational, but it enters the cache key, so changing it stops
    /// serving the previous model's prose.
    pub model: String,
    /// Path to a prompt template; empty uses the built-in one.
    pub prompt_template: String,
    pub timeout_ms: u64,
    /// Grace between `SIGTERM` and `SIGKILL` for a run's process group.
    pub kill_grace_ms: u64,
    pub max_output_bytes: u64,
    pub max_input_bytes: u64,
    pub max_concurrent: u32,
    /// Hard ceiling on model runs in one session.
    pub max_runs_per_session: u32,
    pub min_run_interval_ms: u64,
    /// Only speculate once the watcher has been quiet this long, so
    /// speculation never competes with an agent mid-edit.
    pub idle_before_spawn_ms: u64,
    pub cache_ttl_hours: u64,
    pub max_cache_rows: u32,
}

impl Default for SpeculateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kinds: vec!["brief".to_owned(), "diff".to_owned()],
            command: Vec::new(),
            model: String::new(),
            prompt_template: String::new(),
            timeout_ms: 45_000,
            kill_grace_ms: 2_000,
            max_output_bytes: 16 * 1024,
            max_input_bytes: 64 * 1024,
            max_concurrent: 1,
            max_runs_per_session: 20,
            min_run_interval_ms: 30_000,
            idle_before_spawn_ms: 1_500,
            cache_ttl_hours: 24,
            max_cache_rows: 500,
        }
    }
}

impl SpeculateConfig {
    /// The file: `$ACYCLIC_SPECULATE_CONFIG`, else
    /// `~/.config/<name>/speculate.toml`.
    pub fn path() -> Option<std::path::PathBuf> {
        if let Some(explicit) = std::env::var_os(crate::product::SPECULATE_CONFIG_ENV) {
            return Some(std::path::PathBuf::from(explicit));
        }
        std::env::var_os("HOME").map(|home| {
            Path::new(&home).join(format!(".config/{}/speculate.toml", crate::product::NAME))
        })
    }

    /// Loads the settings, or returns a disabled config.
    ///
    /// This never fails, which is the opposite of `Config::load`'s contract
    /// and deliberate: a missing file, a typo in a key, or no `HOME` must
    /// leave speculation off and the rest of the product working. A
    /// malformed speculation config must never be able to break
    /// `acyclic status`. The reason is returned so the daemon can log it.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else {
            return (Self::default(), None);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), None)
            }
            Err(error) => {
                return (
                    Self::default(),
                    Some(format!("{}: {error}; speculation off", path.display())),
                )
            }
        };
        match toml::from_str(&text) {
            Ok(config) => (config, None),
            Err(error) => (
                Self::default(),
                Some(format!("{}: {error}; speculation off", path.display())),
            ),
        }
    }

    /// Whether this kind is enabled. A kind that spends tokens additionally
    /// needs a command to run, so a stray `kinds = ["summary"]` with no
    /// `command` cannot start billing anyone.
    pub fn wants(&self, kind: SpecKind) -> bool {
        if !self.enabled {
            return false;
        }
        if !self.kinds.iter().any(|name| name == kind.as_str()) {
            return false;
        }
        !kind.spends_tokens() || !self.command.is_empty()
    }

    /// Whether this configuration can spend money at all — what the daemon
    /// reports and what the compliance note is about.
    pub fn spends_tokens(&self) -> bool {
        [SpecKind::Summary].into_iter().any(|kind| self.wants(kind))
    }

    /// The `recipe` component of every key produced under this config.
    /// Bumping `RENDERER_VERSION` invalidates every cached result by making
    /// the old keys unreachable.
    pub fn recipe(&self, kind: SpecKind) -> String {
        const RENDERER_VERSION: u32 = 1;
        if !kind.spends_tokens() {
            return format!("v{RENDERER_VERSION}");
        }
        let template = blake3::hash(self.prompt_template.as_bytes());
        let digest = template.to_hex();
        format!(
            "v{RENDERER_VERSION}/{}/{}",
            self.model,
            digest.get(..8).unwrap_or("")
        )
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub fn cache_ttl(&self) -> Duration {
        Duration::from_secs(self.cache_ttl_hours.saturating_mul(3_600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SpecStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            SpecStore::open(&dir.path().join("spec.db"), Duration::from_secs(1)).expect("open");
        (dir, store)
    }

    fn key(after: &str) -> SpecKey {
        SpecKey {
            kind: SpecKind::Summary,
            before: "aa".to_owned(),
            after: after.to_owned(),
            scope: "session-1".to_owned(),
            recipe: "v1/haiku/deadbeef".to_owned(),
        }
    }

    #[test]
    fn a_landed_result_is_claimable_once_ready() {
        let (_dir, mut store) = store();
        let key = key("bb");
        let run = store
            .begin(&key, Some("session-1"), Some(2))
            .expect("begin");
        let run = run.expect("first begin reserves the key");
        assert!(store.claim(&key).expect("claim").is_none(), "running");
        store
            .finish(
                run,
                &RunOutcome::Ready {
                    body: "it renamed the thing".to_owned(),
                },
            )
            .expect("finish");
        let hit = store.claim(&key).expect("claim").expect("ready");
        assert_eq!(hit.body, "it renamed the thing");
        assert_eq!(hit.bytes, "it renamed the thing".len() as u64);
    }

    /// The whole correctness argument: a result computed against one tree
    /// must be unreachable once the tree moves, with no staleness check.
    #[test]
    fn a_moved_generation_is_a_miss_not_a_near_miss() {
        let (_dir, mut store) = store();
        let landed = key("bb");
        let run = store
            .begin(&landed, None, None)
            .expect("begin")
            .expect("id");
        store
            .finish(
                run,
                &RunOutcome::Ready {
                    body: "stale prose".to_owned(),
                },
            )
            .expect("finish");

        let moved = key("cc");
        assert!(store.claim(&moved).expect("claim").is_none());
        // Every other component of the key is load-bearing too.
        let mut other_recipe = landed.clone();
        other_recipe.recipe = "v1/opus/deadbeef".to_owned();
        assert!(store.claim(&other_recipe).expect("claim").is_none());
        let mut other_scope = landed.clone();
        other_scope.scope = "session-2".to_owned();
        assert!(store.claim(&other_scope).expect("claim").is_none());
        // The original still is.
        assert!(store.claim(&landed).expect("claim").is_some());
    }

    #[test]
    fn begin_dedupes_so_two_schedulers_cannot_both_spend() {
        let (_dir, mut store) = store();
        let key = key("bb");
        assert!(store.begin(&key, None, None).expect("begin").is_some());
        assert!(
            store.begin(&key, None, None).expect("begin").is_none(),
            "a running key is taken"
        );
        let run = store.state_of(&key).expect("state");
        assert_eq!(run, Some(SpecState::Running));
    }

    #[test]
    fn a_ready_key_is_not_recomputed_but_a_failed_one_retries() {
        let (_dir, mut store) = store();
        let failed = key("bb");
        let run = store
            .begin(&failed, None, None)
            .expect("begin")
            .expect("id");
        store
            .finish(run, &RunOutcome::Failed("boom".to_owned()))
            .expect("finish");
        assert_eq!(
            store.state_of(&failed).expect("state"),
            Some(SpecState::Failed)
        );
        assert!(
            store.begin(&failed, None, None).expect("begin").is_some(),
            "a dead end must not hold the key forever"
        );

        let key2 = key("cc");
        let run = store.begin(&key2, None, None).expect("begin").expect("id");
        store
            .finish(
                run,
                &RunOutcome::Ready {
                    body: "done".to_owned(),
                },
            )
            .expect("finish");
        assert!(
            store.begin(&key2, None, None).expect("begin").is_none(),
            "a ready answer is not recomputed"
        );
    }

    #[test]
    fn a_failed_run_stores_no_body() {
        let (_dir, mut store) = store();
        for outcome in [
            RunOutcome::Failed("boom".to_owned()),
            RunOutcome::Timeout,
            RunOutcome::Overflow,
            RunOutcome::Cancelled,
        ] {
            let key = key(outcome.state().as_str());
            let run = store.begin(&key, None, None).expect("begin").expect("id");
            store.finish(run, &outcome).expect("finish");
            assert_eq!(store.state_of(&key).expect("state"), Some(outcome.state()));
            assert!(store.claim(&key).expect("claim").is_none());
        }
    }

    #[test]
    fn sweep_orphans_reclaims_keys_a_dead_daemon_held() {
        let (_dir, mut store) = store();
        let key = key("bb");
        store.begin(&key, None, None).expect("begin");
        assert_eq!(store.sweep_orphans().expect("sweep"), 1);
        assert_eq!(
            store.state_of(&key).expect("state"),
            Some(SpecState::Orphaned)
        );
        assert!(
            store.begin(&key, None, None).expect("begin").is_some(),
            "an orphaned key is retryable"
        );
        // Idempotent: nothing is running now.
        assert_eq!(store.sweep_orphans().expect("sweep"), 1);
    }

    #[test]
    fn gc_trims_to_the_row_cap() {
        let (_dir, mut store) = store();
        for index in 0..10_u32 {
            let key = key(&format!("gen-{index}"));
            let run = store.begin(&key, None, None).expect("begin").expect("id");
            store
                .finish(
                    run,
                    &RunOutcome::Ready {
                        body: "x".to_owned(),
                    },
                )
                .expect("finish");
        }
        let removed = store.gc(Duration::from_secs(3_600), 4).expect("gc");
        assert_eq!(removed, 6);
        let remaining: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM spec_cache", [], |row| row.get(0))
            .expect("count");
        assert_eq!(remaining, 4);
    }

    #[test]
    fn gc_never_evicts_a_run_still_in_flight() {
        let (_dir, mut store) = store();
        let key = key("bb");
        store.begin(&key, None, None).expect("begin");
        store.gc(Duration::ZERO, 0).expect("gc");
        assert_eq!(
            store.state_of(&key).expect("state"),
            Some(SpecState::Running)
        );
    }

    #[test]
    fn metrics_roll_up_the_log() {
        let (_dir, mut store) = store();
        let row = |event, lead_ms, bytes| SpecEventRow {
            event,
            kind: SpecKind::Summary,
            session_id: None,
            turn: None,
            lead_ms,
            wall_ms: None,
            bytes,
            detail: None,
        };
        store.log(&row(SpecEvent::Spawn, None, None)).expect("log");
        store.log(&row(SpecEvent::Spawn, None, None)).expect("log");
        store
            .log(&row(SpecEvent::Ready, None, Some(300)))
            .expect("log");
        store
            .log(&row(SpecEvent::ClaimHit, Some(1_000), None))
            .expect("log");
        store
            .log(&row(SpecEvent::ClaimHit, Some(5_000), None))
            .expect("log");
        store
            .log(&row(SpecEvent::ClaimHit, Some(9_000), None))
            .expect("log");
        store
            .log(&row(SpecEvent::ClaimMiss, None, None))
            .expect("log");

        let metrics = store.metrics(Duration::from_secs(3_600)).expect("metrics");
        assert_eq!(metrics.runs, 2);
        assert_eq!(metrics.claimed, 3);
        assert_eq!(metrics.missed, 1);
        assert_eq!(metrics.bytes_out, 300);
        assert_eq!(metrics.median_lead_ms, Some(5_000));
        assert_eq!(metrics.claim_rate(), Some(75));
    }

    #[test]
    fn claim_rate_is_none_before_anyone_asks() {
        assert_eq!(SpecMetrics::default().claim_rate(), None);
    }

    #[test]
    fn config_defaults_are_off_and_spend_nothing() {
        let config = SpeculateConfig::default();
        assert!(!config.enabled);
        assert!(!config.spends_tokens());
        for kind in [SpecKind::Brief, SpecKind::Diff, SpecKind::Summary] {
            assert!(!config.wants(kind), "{kind:?} must be off by default");
        }
    }

    /// The two-gate rule: enabling speculation buys the free half only.
    #[test]
    fn enabling_speculation_does_not_by_itself_enable_spend() {
        let mut config = SpeculateConfig {
            enabled: true,
            ..SpeculateConfig::default()
        };
        assert!(config.wants(SpecKind::Brief));
        assert!(!config.spends_tokens());

        // Asking for summaries is still not enough without a command.
        config.kinds.push("summary".to_owned());
        assert!(!config.wants(SpecKind::Summary));
        assert!(!config.spends_tokens());

        config.command = vec!["stub".to_owned()];
        assert!(config.wants(SpecKind::Summary));
        assert!(config.spends_tokens());
    }

    #[test]
    fn a_malformed_config_disables_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("speculate.toml");
        std::fs::write(&path, "enabled = true\nnot_a_key = 1\n").expect("write");
        std::env::set_var(crate::product::SPECULATE_CONFIG_ENV, &path);
        let (config, reason) = SpeculateConfig::load();
        std::env::remove_var(crate::product::SPECULATE_CONFIG_ENV);
        assert_eq!(config, SpeculateConfig::default());
        assert!(!config.enabled, "a typo must not leave speculation on");
        assert!(reason.is_some_and(|reason| reason.contains("speculation off")));
    }

    #[test]
    fn the_model_is_part_of_the_recipe_for_paid_kinds() {
        let haiku = SpeculateConfig {
            model: "haiku".to_owned(),
            ..SpeculateConfig::default()
        };
        let opus = SpeculateConfig {
            model: "opus".to_owned(),
            ..SpeculateConfig::default()
        };
        assert_ne!(
            haiku.recipe(SpecKind::Summary),
            opus.recipe(SpecKind::Summary)
        );
        // Free kinds don't render prose, so the model is irrelevant to them.
        assert_eq!(haiku.recipe(SpecKind::Brief), opus.recipe(SpecKind::Brief));
    }
}
