//! The speculation scheduler: decides what to compute before it is asked.
//!
//! Two rules shape everything here.
//!
//! **Nothing speculative runs on the pipeline thread.** That thread serves
//! the hook path, and a hook must never wait behind work nobody requested.
//! The scheduler lives on its own thread and reaches the pipeline only
//! through `PipelineHandle::diff_speculative`, which abandons the request
//! rather than queue it when the pipeline is busy.
//!
//! **A speculation is never load-bearing.** Every failure here — a full
//! queue, a wedged cache, a missing database — degrades to "compute it the
//! normal way". Callers claim a result if one happens to be sitting there
//! and otherwise proceed exactly as they did before this module existed.
//!
//! What it precomputes today is the previous-session brief, which is the
//! most expensive thing on the agent's critical path: `SessionStart` blocks
//! on it, and it costs a pipeline diff per abandoned branch plus two more.
//! The session that will read it ends long before it is asked for, so the
//! work lands in dead time.

use std::path::PathBuf;
use std::time::Duration;

use crate::proto;
use acyclic::index::Index;
use acyclic::pipeline::PipelineHandle;
use acyclic::spec::{
    RunId, RunOutcome, SpecEvent as LogEvent, SpecEventRow, SpecHit, SpecKey, SpecKind,
    SpecMetrics, SpecState, SpecStore, SpeculateConfig,
};
use tokio::sync::{mpsc, oneshot};

/// Bound on the claim path's wait for `spec.db`. Short on purpose: the one
/// hook the agent genuinely blocks on is `SessionStart`, so a wedged cache
/// must degrade to computing the brief normally rather than stalling a
/// session start behind a lock.
const CLAIM_BUSY_TIMEOUT: Duration = Duration::from_millis(200);

/// The scheduler's own wait for the cache. It can afford to queue.
const SCHEDULER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the cache is trimmed when nothing else is happening.
const GC_INTERVAL: Duration = Duration::from_secs(600);

/// Depth of the event queue. Shallow on purpose: if the scheduler is so far
/// behind that 64 events piled up, the useful move is to drop speculations,
/// not to accumulate a backlog of stale ones.
const QUEUE_DEPTH: usize = 64;

/// Something worth speculating about.
#[derive(Debug)]
pub enum SpecEvent {
    /// A session ended, so the brief the *next* session will open with can
    /// be computed now.
    SessionEnded,
    /// A turn started, which is the only reliable signal that the PREVIOUS
    /// turn finished — and a finished turn is one that can be summarised
    /// while nobody is waiting.
    TurnStarted { session_id: String, turn: i64 },
    /// Real work happened that a run in flight was racing. Cancelling is a
    /// spend control, never a correctness one: a result computed against a
    /// tree that has moved is already unreachable through its key.
    Invalidate(Cause),
    /// Stop: finish nothing, start nothing, acknowledge.
    Shutdown(oneshot::Sender<()>),
}

/// Why a run in flight is no longer worth paying for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Cause {
    /// The tree was rewound or a path restored under the run.
    TreeMoved,
    /// This session is over, or resolved out from under the run.
    Session(String),
    /// Everything: the daemon is going away.
    All,
}

impl Cause {
    fn covers(&self, session_id: &str) -> bool {
        match self {
            Self::All | Self::TreeMoved => true,
            Self::Session(id) => id == session_id,
        }
    }
}

/// What the scheduler needs to do its work.
pub struct SpecDeps {
    pub index_db: PathBuf,
    pub spec_db: PathBuf,
    /// Scratch and pid files for model runs.
    pub spec_runs: PathBuf,
    pub handle: PipelineHandle,
}

/// The daemon's end of the scheduler.
pub struct SpecHandle {
    events: mpsc::Sender<SpecEvent>,
    spec_db: PathBuf,
    config: SpeculateConfig,
}

impl SpecHandle {
    /// Fire-and-forget. Never awaits and never blocks: call sites are
    /// request handlers, several of them on the hook path.
    pub fn notify(&self, event: SpecEvent) {
        if let Err(error) = self.events.try_send(event) {
            acyclic::trace!("spec", "event dropped: {error}");
        }
    }

    /// Asks the scheduler to stop and waits for it to acknowledge, so no
    /// speculative work is still touching the pipeline when it shuts down.
    pub async fn shutdown(&self) {
        let (reply, receiver) = oneshot::channel();
        if self.events.send(SpecEvent::Shutdown(reply)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), receiver).await;
        }
    }

    pub fn config(&self) -> &SpeculateConfig {
        &self.config
    }

    /// What `acyclic status` reports. `None` when the cache cannot be read,
    /// which is not worth an error: the feature is advisory.
    pub fn metrics(&self) -> Option<SpecMetrics> {
        let store = SpecStore::open(&self.spec_db, CLAIM_BUSY_TIMEOUT).ok()?;
        store.metrics(Duration::from_secs(24 * 3_600)).ok()
    }

    /// Serves a precomputed brief, if one matches this exact request.
    ///
    /// Returns `None` for every unhappy path — no cache, no match, a body
    /// that no longer deserializes — and the caller then computes the brief
    /// as it always did.
    pub fn claim_brief(&self, key: &SpecKey) -> Option<proto::BriefInfo> {
        let mut store = SpecStore::open(&self.spec_db, CLAIM_BUSY_TIMEOUT).ok()?;
        let claimed = store.claim(key).ok()?;
        let event = |event, lead_ms| SpecEventRow {
            event,
            kind: SpecKind::Brief,
            session_id: None,
            turn: None,
            lead_ms,
            wall_ms: None,
            bytes: None,
            detail: None,
        };
        let Some(hit) = claimed else {
            let _ = store.log(&event(LogEvent::ClaimMiss, None));
            return None;
        };
        let info = serde_json::from_str(&hit.body).ok();
        let _ = store.log(&event(LogEvent::ClaimHit, Some(hit.lead_ms)));
        acyclic::trace!("spec", "brief claimed, {}ms ahead", hit.lead_ms);
        info
    }

    /// The key one turn's summary is served under.
    pub fn summary_key(
        &self,
        index: &Index,
        session_id: &str,
        turn: i64,
    ) -> Option<(SpecKey, String)> {
        let (before, after, prompt) = turn_range(index, session_id, turn)?;
        Some((
            SpecKey::range(SpecKind::Summary, before, after)
                .scoped(format!("{session_id}#{turn}"))
                .with_recipe(self.config.recipe(SpecKind::Summary)),
            prompt,
        ))
    }

    /// Serves a turn summary.
    ///
    /// `wait_ms` is the one place a caller may block on speculation, and it
    /// exists for the MCP tool, where a few seconds is acceptable and a
    /// "not ready, ask again" is not. Every hook path passes zero.
    pub async fn claim_summary(&self, key: &SpecKey, wait_ms: u64) -> (Option<SpecHit>, String) {
        let Ok(mut store) = SpecStore::open(&self.spec_db, CLAIM_BUSY_TIMEOUT) else {
            return (None, "unavailable: no speculation cache".to_owned());
        };
        if let Ok(Some(hit)) = store.claim(key) {
            let _ = store.log(&summary_event(LogEvent::ClaimHit, Some(hit.lead_ms)));
            return (Some(hit), "speculated".to_owned());
        }
        // Not ready. If it is being produced right now, the caller may wait
        // for it rather than miss.
        let running = matches!(store.state_of(key), Ok(Some(SpecState::Running)));
        if !running || wait_ms == 0 {
            let _ = store.log(&summary_event(LogEvent::ClaimMiss, None));
            return (
                None,
                if running {
                    "not ready: still being produced".to_owned()
                } else {
                    "not ready: nothing produced for this turn".to_owned()
                },
            );
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(hit)) = store.claim(key) {
                let _ = store.log(&summary_event(LogEvent::ClaimJoin, Some(hit.lead_ms)));
                return (Some(hit), "waited".to_owned());
            }
        }
        let _ = store.log(&summary_event(LogEvent::ClaimMiss, None));
        (None, "not ready: timed out waiting".to_owned())
    }

    /// Caches a brief the request path had to compute itself, so the next
    /// session start over the same tree is a hit even if nothing scheduled
    /// it. Failures are silent by design.
    pub fn store_brief(&self, key: &SpecKey, info: &proto::BriefInfo) {
        let Ok(mut store) = SpecStore::open(&self.spec_db, CLAIM_BUSY_TIMEOUT) else {
            return;
        };
        let Ok(body) = serde_json::to_string(info) else {
            return;
        };
        // `Ok(None)` and `Err` both mean there is nothing to do: the answer
        // is already cached, already in flight, or the cache is unwritable.
        if let Ok(Some(run)) = store.begin(key, None, None) {
            let _ = store.finish(run, &RunOutcome::Ready { body });
        }
    }
}

/// A claim/miss line for a summary, which carries no session attribution:
/// the key already identifies the turn.
fn summary_event(event: LogEvent, lead_ms: Option<i64>) -> SpecEventRow {
    SpecEventRow {
        event,
        kind: SpecKind::Summary,
        session_id: None,
        turn: None,
        lead_ms,
        wall_ms: None,
        bytes: None,
        detail: None,
    }
}

/// The key a brief is served under.
///
/// Two things decide a brief's content: which session it describes, and how
/// far the tree has drifted since that session ended. So the key is the
/// subject session plus the current head. If another session records a
/// checkpoint the subject changes; if anything touches the tree the
/// generation changes. Either way the old entry stops matching, which is the
/// whole staleness story — resolving it costs two indexed queries against
/// the N pipeline diffs computing the brief would cost.
///
/// `Ok(None)` means there is nothing to speculate about (no previous session
/// with checkpoints, or no checkpoint at all).
pub fn brief_key(
    index: &Index,
    config: &SpeculateConfig,
    current: Option<&str>,
) -> Result<Option<SpecKey>, String> {
    let subject = index
        .last_session_with_checkpoints(current)
        .map_err(|error| error.to_string())?;
    let Some(subject) = subject else {
        return Ok(None);
    };
    let latest = index.latest_target().map_err(|error| error.to_string())?;
    let Some(latest) = latest else {
        return Ok(None);
    };
    Ok(Some(
        SpecKey::at(SpecKind::Brief, latest.generation)
            .scoped(subject.session_id)
            .with_recipe(config.recipe(SpecKind::Brief)),
    ))
}

/// Starts the scheduler on its own thread, mirroring how the pipeline is
/// spawned. Its own thread rather than a task on the daemon's runtime
/// because it owns a synchronous `SpecStore` connection: single owner,
/// single writer, no lock shared with anything the daemon serves.
///
/// Returns `None` when speculation is disabled, and the daemon then holds
/// `None` — so every call site is a no-op with no branch of its own.
pub fn spawn(
    config: SpeculateConfig,
    deps: SpecDeps,
) -> Option<(SpecHandle, std::thread::JoinHandle<()>)> {
    if !config.enabled {
        return None;
    }
    let (sender, receiver) = mpsc::channel(QUEUE_DEPTH);
    let spec_db = deps.spec_db.clone();
    let thread_config = config.clone();
    let thread = std::thread::Builder::new()
        .name(format!("{}-speculate", acyclic::product::NAME))
        .spawn(move || {
            // `enable_all`, not just timers: a model run is a child
            // process, which needs the IO and signal drivers.
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!(
                        "{}: speculation runtime: {error}; speculation off",
                        acyclic::product::NAME
                    );
                    return;
                }
            };
            runtime.block_on(run(thread_config, deps, receiver));
        })
        .ok()?;
    Some((
        SpecHandle {
            events: sender,
            spec_db,
            config,
        },
        thread,
    ))
}

/// A model run in flight: what it is for, and the handle that stops it.
struct InFlight {
    run: RunId,
    session_id: String,
    turn: i64,
    cancel: oneshot::Sender<()>,
}

/// A finished model run, reported back to the loop so that the single owner
/// of the cache stays its only writer.
struct Finished {
    run: RunId,
    session_id: String,
    turn: i64,
    outcome: RunOutcome,
    wall_ms: i64,
}

/// What the loop keeps between events.
struct Scheduler {
    config: SpeculateConfig,
    store: SpecStore,
    /// The one model run allowed at a time, if any.
    in_flight: Option<InFlight>,
    /// Model runs already spent, per session: the hard ceiling.
    spent: std::collections::HashMap<String, u32>,
    /// When the last model run started, for the rate limit.
    last_run: Option<std::time::Instant>,
}

/// The scheduler loop.
///
/// A model run is a spawned task rather than an inline await, because the
/// loop has to stay responsive while one is going: an event arriving during
/// a 45-second run is usually the very thing that should cancel it.
async fn run(config: SpeculateConfig, deps: SpecDeps, mut receiver: mpsc::Receiver<SpecEvent>) {
    let Ok(mut store) = SpecStore::open(&deps.spec_db, SCHEDULER_BUSY_TIMEOUT) else {
        eprintln!(
            "{}: speculation cache unavailable; speculation off",
            acyclic::product::NAME
        );
        return;
    };
    // A `running` row whose daemon died would hold its key forever, since
    // `running` is the one state that is never retryable.
    if let Ok(swept) = store.sweep_orphans() {
        if swept > 0 {
            acyclic::trace!("spec", "swept {swept} orphaned run(s) from a dead daemon");
        }
    }
    let mut scheduler = Scheduler {
        config,
        store,
        in_flight: None,
        spent: std::collections::HashMap::new(),
        last_run: None,
    };
    let (done, mut finished) = mpsc::channel::<Finished>(4);
    let mut next_gc = tokio::time::Instant::now() + GC_INTERVAL;
    loop {
        tokio::select! {
            event = receiver.recv() => match event {
                None => break,
                Some(SpecEvent::Shutdown(reply)) => {
                    scheduler.cancel(&Cause::All);
                    let _ = reply.send(());
                    break;
                }
                Some(event) => scheduler.handle(event, &deps, &done).await,
            },
            Some(report) = finished.recv() => scheduler.record(&report),
            () = tokio::time::sleep_until(next_gc) => {
                next_gc = tokio::time::Instant::now() + GC_INTERVAL;
                scheduler.collect();
            }
        }
    }
    scheduler.cancel(&Cause::All);
    scheduler.collect();
}

impl Scheduler {
    async fn handle(&mut self, event: SpecEvent, deps: &SpecDeps, done: &mpsc::Sender<Finished>) {
        match event {
            SpecEvent::SessionEnded => {
                speculate_brief(&self.config, deps, &mut self.store).await;
            }
            SpecEvent::TurnStarted { session_id, turn } => {
                // Turn N starting means turn N-1 is over and will not change
                // again, which is what makes it summarisable.
                if let Some(previous) = turn.checked_sub(1).filter(|turn| *turn >= 1) {
                    self.speculate_summary(deps, done, &session_id, previous)
                        .await;
                }
            }
            SpecEvent::Invalidate(cause) => self.cancel(&cause),
            SpecEvent::Shutdown(reply) => {
                let _ = reply.send(());
            }
        }
    }

    /// Stops a run whose reason for existing just went away, and records it
    /// as cancelled so its key becomes retryable.
    fn cancel(&mut self, cause: &Cause) {
        let Some(run) = self.in_flight.take() else {
            return;
        };
        if !cause.covers(&run.session_id) {
            self.in_flight = Some(run);
            return;
        }
        acyclic::trace!("spec", "cancelling the summary run: {cause:?}");
        let _ = run.cancel.send(());
        let _ = self.store.finish(run.run, &RunOutcome::Cancelled);
        self.log(LogEvent::Cancel, &run.session_id, run.turn, None, None);
    }

    fn record(&mut self, report: &Finished) {
        // A superseded or cancelled run was already finished; only the run
        // the scheduler still considers current may write a result.
        let current = self
            .in_flight
            .as_ref()
            .is_some_and(|run| run.run == report.run);
        if !current {
            return;
        }
        self.in_flight = None;
        let (event, bytes) = match &report.outcome {
            RunOutcome::Ready { body } => {
                acyclic::trace!(
                    "spec",
                    "summary for turn {} ready in {}ms",
                    report.turn,
                    report.wall_ms
                );
                (LogEvent::Ready, Some(body.len() as u64))
            }
            RunOutcome::Timeout => (LogEvent::Timeout, None),
            RunOutcome::Cancelled => (LogEvent::Cancel, None),
            _ => (LogEvent::Fail, None),
        };
        let _ = self.store.finish(report.run, &report.outcome);
        self.log(
            event,
            &report.session_id,
            report.turn,
            Some(report.wall_ms),
            bytes,
        );
    }

    fn collect(&mut self) {
        let _ = self
            .store
            .gc(self.config.cache_ttl(), self.config.max_cache_rows);
    }

    fn log(
        &mut self,
        event: LogEvent,
        session_id: &str,
        turn: i64,
        wall_ms: Option<i64>,
        bytes: Option<u64>,
    ) {
        let _ = self.store.log(&SpecEventRow {
            event,
            kind: SpecKind::Summary,
            session_id: Some(session_id.to_owned()),
            turn: Some(turn),
            lead_ms: None,
            wall_ms,
            bytes,
            detail: None,
        });
    }

    /// Whether a model run may start now.
    ///
    /// Three independent limits, because this is the only code path in the
    /// product that spends money: one run at a time, a floor on the gap
    /// between runs, and a per-session ceiling nothing can talk its way past.
    fn may_spend(&self, session_id: &str) -> bool {
        if self.in_flight.is_some() {
            return false;
        }
        if self.spent.get(session_id).copied().unwrap_or(0) >= self.config.max_runs_per_session {
            return false;
        }
        self.last_run.is_none_or(|last| {
            last.elapsed() >= Duration::from_millis(self.config.min_run_interval_ms)
        })
    }
}

/// The instruction sent with every summary unless a template is configured.
/// Short and closed-ended: the answer has a hard byte cap and lands in a
/// brief with a 1 KB budget, so a discursive model wastes the run.
const SUMMARY_PROMPT: &str = "\
You are summarising one turn of a coding-agent session for the developer to \
read later. In at most three sentences of plain past tense, say what changed \
and why it was done. Name files only when a file is the point. Reply with \
the summary and nothing else.";

impl Scheduler {
    /// Pre-fires the summarizer for a turn that just finished.
    ///
    /// This is the one place the daemon spends money, so every gate is
    /// checked before anything is spawned: the kind must be enabled, a
    /// command must be configured, the budget must allow it, and the turn
    /// must actually have changed something. A turn that only asked a
    /// question produces no checkpoints and is skipped — a meaningful share
    /// of turns, and paying a model to summarise an empty diff is pure waste.
    async fn speculate_summary(
        &mut self,
        deps: &SpecDeps,
        done: &mpsc::Sender<Finished>,
        session_id: &str,
        turn: i64,
    ) {
        if !self.config.wants(SpecKind::Summary) || !self.may_spend(session_id) {
            return;
        }
        let Ok(index) = Index::open(&deps.index_db) else {
            return;
        };
        let Some((before, after, prompt)) = turn_range(&index, session_id, turn) else {
            return;
        };
        let key = SpecKey::range(SpecKind::Summary, before, after)
            .scoped(format!("{session_id}#{turn}"))
            .with_recipe(self.config.recipe(SpecKind::Summary));
        // Admission: a cached or in-flight answer for this key means there
        // is nothing to buy.
        let Ok(Some(run)) = self.store.begin(&key, Some(session_id), Some(turn)) else {
            return;
        };

        let changes = match deps.handle.diff_speculative(before, after).await {
            // The pipeline was busy — speculation yields rather than queues.
            Ok(None) => {
                self.log(LogEvent::Drop, session_id, turn, None, None);
                let _ = self
                    .store
                    .finish(run, &RunOutcome::Failed("pipeline busy".to_owned()));
                return;
            }
            Ok(Some(changes)) => changes,
            Err(error) => {
                let _ = self
                    .store
                    .finish(run, &RunOutcome::Failed(error.to_string()));
                return;
            }
        };
        let stdin = render_prompt(&self.config, &prompt, &changes);
        let Ok(space) = crate::spec_runner::RunSpace::create(&deps.spec_runs, run) else {
            let _ = self
                .store
                .finish(run, &RunOutcome::Failed("no scratch dir".to_owned()));
            return;
        };
        let spec = crate::spec_runner::RunSpec {
            command: self.config.command.clone(),
            stdin,
            timeout: self.config.timeout(),
            kill_grace: Duration::from_millis(self.config.kill_grace_ms),
            max_output_bytes: self.config.max_output_bytes,
        };

        let (cancel, cancelled) = oneshot::channel();
        let done = done.clone();
        let owner = session_id.to_owned();
        acyclic::trace!("spec", "pre-firing the summarizer for turn {turn}");
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let outcome = crate::spec_runner::run(spec, &space, cancelled).await;
            let _ = done
                .send(Finished {
                    run,
                    session_id: owner,
                    turn,
                    outcome,
                    wall_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                })
                .await;
        });

        self.in_flight = Some(InFlight {
            run,
            session_id: session_id.to_owned(),
            turn,
            cancel,
        });
        self.last_run = Some(std::time::Instant::now());
        *self.spent.entry(session_id.to_owned()).or_insert(0) += 1;
        self.log(LogEvent::Spawn, session_id, turn, None, None);
    }
}

/// The generations a turn spans, and the prompt that caused it.
///
/// `None` when the turn changed nothing — a question-only turn records no
/// checkpoints — or when its range cannot be resolved.
fn turn_range(
    index: &Index,
    session_id: &str,
    turn: i64,
) -> Option<(acyclic::GenerationId, acyclic::GenerationId, String)> {
    let row = index.turn(session_id, turn).ok()??;
    let (first, last) = (row.first_checkpoint?, row.last_checkpoint?);
    let base = index.latest_target_before(first).ok()??;
    let last = index.by_id(last).ok()??;
    if base.generation == last.generation {
        return None;
    }
    Some((base.generation, last.generation, row.prompt))
}

/// Builds the model's input: the instruction, the prompt that caused the
/// turn, and the list of paths it touched.
///
/// Paths and the prompt excerpt, never file contents. That keeps the run
/// cheap, keeps the input bounded, and keeps what leaves the machine easy to
/// describe — the change list comes from a store-computed diff, which
/// already honours `exclude`.
fn render_prompt(
    config: &SpeculateConfig,
    prompt: &str,
    changes: &[acyclic::diff::FileChange],
) -> String {
    use acyclic::diff::ChangeKind;
    let instruction = if config.prompt_template.is_empty() {
        SUMMARY_PROMPT.to_owned()
    } else {
        std::fs::read_to_string(&config.prompt_template)
            .unwrap_or_else(|_| SUMMARY_PROMPT.to_owned())
    };
    let mut text = format!("{instruction}\n\nThe developer asked: {prompt}\n\nFiles changed:\n");
    for change in changes {
        let tag = match change.change {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "modified",
            // Noise: a rewind rewrites mtimes on everything it materializes.
            ChangeKind::MetadataOnly => continue,
        };
        let line = format!("  {tag} {}\n", change.path.display());
        let cap = usize::try_from(config.max_input_bytes).unwrap_or(usize::MAX);
        if text.len() + line.len() > cap {
            text.push_str("  …\n");
            break;
        }
        text.push_str(&line);
    }
    text
}

/// Computes the brief the next session will open with.
///
/// Every step is allowed to give up: this is work nobody asked for, and the
/// request path computes the same thing correctly if it is not here.
async fn speculate_brief(config: &SpeculateConfig, deps: &SpecDeps, store: &mut SpecStore) {
    if !config.wants(SpecKind::Brief) {
        return;
    }
    let Ok(index) = Index::open(&deps.index_db) else {
        return;
    };
    let Ok(Some(key)) = brief_key(&index, config, None) else {
        return;
    };
    // `begin` is the admission gate: it fails when the answer is already
    // cached or already being produced, so a second trigger cannot double
    // the work.
    let Ok(Some(run)) = store.begin(&key, None, None) else {
        return;
    };
    let started = std::time::Instant::now();
    let log = |store: &mut SpecStore, event, wall_ms, bytes| {
        let _ = store.log(&SpecEventRow {
            event,
            kind: SpecKind::Brief,
            session_id: Some(key.scope.clone()),
            turn: None,
            lead_ms: None,
            wall_ms,
            bytes,
            detail: None,
        });
    };
    log(store, LogEvent::Spawn, None, None);

    let outcome = match crate::server::compute_brief(index, &deps.handle, None).await {
        Ok(info) => match serde_json::to_string(&info) {
            Ok(body) => RunOutcome::Ready { body },
            Err(error) => RunOutcome::Failed(error.to_string()),
        },
        Err(message) => RunOutcome::Failed(message),
    };
    let wall_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    match &outcome {
        RunOutcome::Ready { body } => {
            acyclic::trace!("spec", "brief precomputed in {wall_ms}ms");
            log(
                store,
                LogEvent::Ready,
                Some(wall_ms),
                Some(body.len() as u64),
            );
        }
        _ => log(store, LogEvent::Fail, Some(wall_ms), None),
    }
    let _ = store.finish(run, &outcome);
}
