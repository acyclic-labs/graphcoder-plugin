//! Rewind engine: everything below the wire.
//!
//! Imports the snapshot engine from `acyclic-fs`/`acyclic-fs-mount` and adds
//! what the product needs on top: store lifecycle, the per-tool-call capture
//! pipeline, the checkpoint metadata index, rewind, and blast-radius diff.
//!
//! The three rules from Phase 0 (see docs/design/phase0-verdict.md):
//! 1. Per-tool-call snapshots use `checkpoint()`; `commit()` only at coarse
//!    boundaries (it proves closure over the whole tree).
//! 2. All engine state (store, socket, index) lives outside the working tree.
//! 3. Volume limits are raised at creation and the `VolumeId` is persisted.
// The sdk workspace warns on missing docs and lints with -D warnings.
#![allow(
    missing_docs,
    reason = "engine internals consumed only by the acyclic binary; \
              per-item docs are tracked as a follow-up"
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )
)]

/// Seconds since the Unix epoch as the index stores them. Saturates rather
/// than wrapping if the clock is somehow past `i64::MAX` seconds.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The first 12 hex digits of a generation id, as every listing prints it.
/// A shorter (malformed) string is returned whole rather than panicking.
pub fn short_hex(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

pub mod config;
pub mod diff;
pub mod exclude;
pub mod fork;
pub mod guard;
pub mod index;
pub mod merge;
pub mod names;
pub mod pipeline;
pub mod product;
pub mod rewind;
pub mod spec;
pub mod store;
pub mod trace;

use thiserror::Error;

pub use acyclic_fs::{GenerationId, MountId};

/// Canonical hex form of a generation id for display and wire use.
pub fn generation_hex(generation: GenerationId) -> String {
    hex::encode(generation.digest().as_bytes())
}

/// Engine-level failures surfaced to the daemon/CLI layer.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("store: {0}")]
    Store(String),
    #[error("filesystem engine: {0}")]
    Fs(String),
    #[error("capture: {0}")]
    Capture(String),
    #[error("restore: {0}")]
    Restore(String),
    #[error("index: {0}")]
    Index(#[from] rusqlite::Error),
    #[error("config: {0}")]
    Config(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl EngineError {
    /// Wraps any `Debug`-printable fs operation failure.
    pub fn fs<E: std::fmt::Debug>(context: &str) -> impl FnOnce(E) -> Self + '_ {
        move |error| Self::Fs(format!("{context}: {error:?}"))
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
