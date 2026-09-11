//! Rewind engine: everything below the wire.
//!
//! Imports the snapshot engine from `acyclic-fs`/`acyclic-fs-mount` and adds
//! what the product needs on top: store lifecycle, the per-tool-call capture
//! pipeline, the checkpoint metadata index, rewind, and blast-radius diff.
//!
//! The three rules from Phase 0 (see plans/phase0-verdict.md):
//! 1. Per-tool-call snapshots use `checkpoint()`; `commit()` only at coarse
//!    boundaries (it proves closure over the whole tree).
//! 2. All engine state (store, socket, index) lives outside the working tree.
//! 3. Volume limits are raised at creation and the `VolumeId` is persisted.

pub mod config;
pub mod diff;
pub mod exclude;
pub mod fork;
pub mod guard;
pub mod index;
pub mod merge;
pub mod pipeline;
pub mod product;
pub mod rewind;
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
