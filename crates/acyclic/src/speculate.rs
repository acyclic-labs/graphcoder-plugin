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

use acyclic_engine::index::Index;
use acyclic_engine::pipeline::PipelineHandle;
use acyclic_engine::spec::{
    RunOutcome, SpecEvent as LogEvent, SpecEventRow, SpecKey, SpecKind, SpecMetrics, SpecStore,
    SpeculateConfig,
};
use acyclic_proto as proto;
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
    /// Stop: finish nothing, start nothing, acknowledge.
    Shutdown(oneshot::Sender<()>),
}

/// What the scheduler needs to do its work.
pub struct SpecDeps {
    pub index_db: PathBuf,
    pub spec_db: PathBuf,
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
            acyclic_engine::trace!("spec", "event dropped: {error}");
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
        acyclic_engine::trace!("spec", "brief claimed, {}ms ahead", hit.lead_ms);
        info
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
        .name(format!("{}-speculate", acyclic_engine::product::NAME))
        // The fs futures a diff pulls in are large; match the pipeline's
        // headroom rather than the 2 MiB default.
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!(
                        "{}: speculation runtime: {error}; speculation off",
                        acyclic_engine::product::NAME
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

/// The scheduler loop. Like the pipeline's, the GC deadline is fixed rather
/// than reset per event, so a chatty session cannot starve it.
async fn run(config: SpeculateConfig, deps: SpecDeps, mut receiver: mpsc::Receiver<SpecEvent>) {
    let Ok(mut store) = SpecStore::open(&deps.spec_db, SCHEDULER_BUSY_TIMEOUT) else {
        eprintln!(
            "{}: speculation cache unavailable; speculation off",
            acyclic_engine::product::NAME
        );
        return;
    };
    // A `running` row whose daemon died would hold its key forever, since
    // `running` is the one state that is never retryable.
    if let Ok(swept) = store.sweep_orphans() {
        if swept > 0 {
            acyclic_engine::trace!("spec", "swept {swept} orphaned run(s) from a dead daemon");
        }
    }
    let mut next_gc = tokio::time::Instant::now() + GC_INTERVAL;
    loop {
        match tokio::time::timeout_at(next_gc, receiver.recv()).await {
            Ok(None) => break,
            Ok(Some(SpecEvent::Shutdown(reply))) => {
                let _ = reply.send(());
                break;
            }
            Ok(Some(SpecEvent::SessionEnded)) => {
                speculate_brief(&config, &deps, &mut store).await;
            }
            Err(_) => {
                next_gc = tokio::time::Instant::now() + GC_INTERVAL;
                let _ = store.gc(config.cache_ttl(), config.max_cache_rows);
            }
        }
    }
    let _ = store.gc(config.cache_ttl(), config.max_cache_rows);
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
            acyclic_engine::trace!("spec", "brief precomputed in {wall_ms}ms");
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
