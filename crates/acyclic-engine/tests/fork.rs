//! P-series fork tests (spec: docs/design/spec-forks.md) — promote logic proven
//! WITHOUT mounts: fork overlays are written through the SDK directly, so
//! these run even while the native mount layer is being reworked.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "test code: a failed expectation should panic with its message"
)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acyclic_engine::config::Config;
use acyclic_engine::fork::{PromoteOutcome, SessionResolveOutcome};
use acyclic_engine::index::{Attribution, CheckpointKind, Index};
use acyclic_engine::pipeline::{self, PipelineHandle};
use acyclic_engine::store::{Store, StorePaths};
use acyclic_fs::kernel::{LogicalName, NamespacePath};
use acyclic_fs::model::VolumeLimits;
use acyclic_fs::{CancellationToken, WorkCounters};

fn fast_config() -> Config {
    Config {
        quiesce_ms: 30,
        quiesce_cap_ms: 400,
        commit_every: 100,
        commit_idle_ms: 60_000,
        trash_ttl_days: 1,
        store_dir: None,
        ..Config::default()
    }
}

struct Rig {
    runtime: tokio::runtime::Runtime,
    handle: PipelineHandle,
    thread: Option<std::thread::JoinHandle<()>>,
    repo: tempfile::TempDir,
    _stores: tempfile::TempDir,
    paths: StorePaths,
}

impl Rig {
    fn start() -> Self {
        let repo = tempfile::tempdir().expect("repo");
        let stores = tempfile::tempdir().expect("stores");
        std::fs::create_dir(repo.path().join("src")).expect("mkdir");
        std::fs::write(repo.path().join("src/app.txt"), b"MAINLINE\n").expect("seed");
        std::fs::write(repo.path().join(".env"), b"SECRET=1\n").expect("seed");
        let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("paths");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let store = runtime
            .block_on(Store::init(repo.path(), paths.clone()))
            .expect("init");
        let index = Index::open(&paths.index_db()).expect("index");
        let (handle, thread) = pipeline::spawn(store, index, fast_config());
        Self {
            runtime,
            handle,
            thread: Some(thread),
            repo,
            _stores: stores,
            paths,
        }
    }

    fn repo_path(&self) -> &Path {
        self.repo.path()
    }

    fn finish(mut self) {
        self.runtime
            .block_on(self.handle.shutdown())
            .expect("shutdown");
        self.thread.take().expect("thread").join().expect("join");
    }
}

/// A repo-relative path as the engine names it on this host.
///
/// Built in the host's encoding rather than as a portable path: these names
/// stand in for what the mount layer writes, and a diff decodes them with
/// the same encoding on the way back out.
fn namespace(path: &str) -> NamespacePath {
    let limits = VolumeLimits::default();
    let names = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            LogicalName::new(
                acyclic_engine::names::encoding(),
                acyclic_engine::names::str_to_bytes(part),
                limits.maximum_component_bytes,
            )
            .expect("name")
        })
        .collect();
    NamespacePath::new(names, limits).expect("namespace path")
}

/// Writes into a fork's overlay exactly as a mount callback would: through
/// the shared checkout.
async fn write_in_fork(seed: &acyclic_engine::fork::ForkSeed, path: &str, bytes: &[u8]) {
    let cancel = CancellationToken::new();
    let mut guard = seed.shared.lock().await;
    guard
        .create_file(
            namespace(path),
            bytes::Bytes::copy_from_slice(bytes),
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .expect("create file in fork overlay");
}

/// P1 + P5: promote lands the fork's exact content and records attribution.
#[test]
fn promote_lands_fork_changes() {
    let rig = Rig::start();
    let outcome = rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        write_in_fork(&seed, "/fork-note.txt", b"written in fork\n").await;
        rig.handle
            .promote(
                Arc::clone(&seed.shared),
                seed.base,
                "promote test-fork".into(),
            )
            .await
            .expect("promote")
    });

    let PromoteOutcome::Promoted { old_tree, .. } = outcome else {
        panic!("expected Promoted, got {outcome:?}");
    };
    assert!(old_tree.is_some(), "a real change must swap the tree");
    assert_eq!(
        std::fs::read(rig.repo_path().join("fork-note.txt")).expect("landed file"),
        b"written in fork\n"
    );
    // Mainline files survived the swap untouched.
    assert_eq!(
        std::fs::read(rig.repo_path().join("src/app.txt")).expect("app"),
        b"MAINLINE\n"
    );
    assert_eq!(
        std::fs::read(rig.repo_path().join(".env")).expect("env"),
        b"SECRET=1\n"
    );

    // I6: fork base + promote rows exist.
    let index = Index::open(&rig.paths.index_db()).expect("index");
    let rows = index.list(None, None, 50).expect("rows");
    assert!(rows.iter().any(|row| {
        row.kind == CheckpointKind::Manual && row.label.as_deref() == Some("fork base")
    }));
    assert!(rows.iter().any(|row| {
        row.kind == CheckpointKind::Manual && row.label.as_deref() == Some("promote test-fork")
    }));
    rig.finish();
}

/// Safe Mode: `resolve_session` + `apply_session` together must land a fork's
/// changes identically to promote's single-shot version.
#[test]
fn resolve_then_apply_session_lands_fork_changes() {
    let rig = Rig::start();
    let (diffable_base, outcome) = rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        write_in_fork(&seed, "/fork-note.txt", b"written in fork\n").await;
        let resolved = rig
            .handle
            .resolve_session(
                Arc::clone(&seed.shared),
                seed.base,
                "safe-mode session".into(),
            )
            .await
            .expect("resolve_session");
        let SessionResolveOutcome::Resolved { generation } = resolved else {
            panic!("expected Resolved, got {resolved:?}");
        };
        let diff = rig.handle.diff(seed.base, generation).await.expect("diff");
        let outcome = rig
            .handle
            .apply_session(generation, seed.base, "safe-mode session".into())
            .await
            .expect("apply_session");
        (diff, outcome)
    });

    // The pre-apply diff already shows the new file, before anything landed.
    assert!(
        diffable_base
            .iter()
            .any(|change| change.path == Path::new("fork-note.txt"))
    );

    let PromoteOutcome::Promoted { old_tree, .. } = outcome else {
        panic!("expected Promoted, got {outcome:?}");
    };
    assert!(old_tree.is_some(), "a real change must swap the tree");
    assert_eq!(
        std::fs::read(rig.repo_path().join("fork-note.txt")).expect("landed file"),
        b"written in fork\n"
    );
    rig.finish();
}

/// Safe Mode: discarding a resolved session (never calling `apply_session`)
/// must leave the real tree completely untouched.
#[test]
fn resolved_session_left_unapplied_leaves_zero_trace() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        write_in_fork(&seed, "/fork-note.txt", b"written in fork\n").await;
        let resolved = rig
            .handle
            .resolve_session(
                Arc::clone(&seed.shared),
                seed.base,
                "safe-mode session".into(),
            )
            .await
            .expect("resolve_session");
        assert!(matches!(resolved, SessionResolveOutcome::Resolved { .. }));
    });
    assert!(!rig.repo_path().join("fork-note.txt").exists());
    rig.finish();
}

/// P2: promoting an untouched fork lands nothing and touches nothing.
#[test]
fn promote_of_untouched_fork_is_noop() {
    let rig = Rig::start();
    let outcome = rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        rig.handle
            .promote(Arc::clone(&seed.shared), seed.base, "promote idle".into())
            .await
            .expect("promote")
    });
    match outcome {
        PromoteOutcome::Promoted { old_tree: None, .. } => {}
        other => panic!("expected no-op promote, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(rig.repo_path().join("src/app.txt")).expect("app"),
        b"MAINLINE\n"
    );
    rig.finish();
}

/// P3 + I4: a genuinely moved mainline yields a legible conflict and an
/// untouched tree.
#[test]
fn promote_conflicts_when_mainline_moved() {
    let rig = Rig::start();
    let outcome = rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        write_in_fork(&seed, "/fork-note.txt", b"fork work\n").await;

        // Move the mainline for real: edit, checkpoint, publish.
        std::fs::write(rig.repo_path().join("src/app.txt"), b"MOVED\n").expect("edit");
        tokio::time::sleep(Duration::from_millis(150)).await;
        rig.handle
            .checkpoint(CheckpointKind::Post, Attribution::default())
            .await
            .expect("checkpoint");
        rig.handle.commit().await.expect("publish");

        rig.handle
            .promote(Arc::clone(&seed.shared), seed.base, "promote stale".into())
            .await
            .expect("promote call")
    });
    let PromoteOutcome::Conflict { message } = outcome else {
        panic!("expected Conflict, got {outcome:?}");
    };
    assert!(
        message.contains("moved past the fork's base"),
        "conflict must be legible: {message}"
    );
    assert_eq!(
        std::fs::read(rig.repo_path().join("src/app.txt")).expect("app"),
        b"MOVED\n"
    );
    assert!(!rig.repo_path().join("fork-note.txt").exists());
    rig.finish();
}

/// P4: publishing nothing (noop commits) between fork and promote must not
/// fake a moved mainline.
#[test]
fn noop_commits_do_not_move_mainline() {
    let rig = Rig::start();
    let outcome = rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        write_in_fork(&seed, "/fork-note.txt", b"fork work\n").await;
        // No tree changes: these publishes must be inert.
        rig.handle.commit().await.expect("noop publish");
        rig.handle
            .checkpoint(CheckpointKind::Manual, Attribution::default())
            .await
            .expect("noop checkpoint");
        rig.handle.commit().await.expect("second noop publish");

        rig.handle
            .promote(Arc::clone(&seed.shared), seed.base, "promote calm".into())
            .await
            .expect("promote")
    });
    let PromoteOutcome::Promoted { .. } = outcome else {
        panic!("noop publishes faked a conflict: {outcome:?}");
    };
    assert_eq!(
        std::fs::read(rig.repo_path().join("fork-note.txt")).expect("landed"),
        b"fork work\n"
    );
    rig.finish();
}
