//! End-to-end pipeline test: init → checkpoints → diff → rewind → verify.

use std::path::Path;
use std::time::Duration;

use acyclic_engine::config::Config;
use acyclic_engine::diff::{self, ChangeKind};
use acyclic_engine::index::{Attribution, CheckpointKind, Index};
use acyclic_engine::pipeline;
use acyclic_engine::store::{Store, StorePaths};

fn read_only_index(path: &Path) -> Index {
    Index::open(path).expect("open index")
}

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

#[test]
fn checkpoint_rewind_journey() {
    let repo = tempfile::tempdir().expect("repo");
    let stores = tempfile::tempdir().expect("stores");
    std::fs::create_dir(repo.path().join("src")).expect("mkdir");
    std::fs::write(repo.path().join("src/main.rs"), b"fn main() {}\n").expect("seed");
    std::fs::write(repo.path().join(".env"), b"SECRET=1\n").expect("seed env");

    let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("paths");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let store = runtime
        .block_on(Store::init(repo.path(), paths.clone()))
        .expect("init store");
    let index = Index::open(&paths.index_db()).expect("index");
    let (handle, thread) = pipeline::spawn(store, index, fast_config());

    let (pre_generation, post_generation) = runtime.block_on(async {
        // Baseline exists; take the "before" checkpoint.
        let before = handle
            .checkpoint(CheckpointKind::Pre, Attribution::default())
            .await
            .expect("pre checkpoint");

        // Simulate an agent turn: edit + new gitignored artifact + delete.
        let repo_root = repo.path();
        std::fs::write(repo_root.join("src/main.rs"), b"fn main() { changed(); }\n").expect("edit");
        std::fs::write(repo_root.join("generated.bin"), vec![7u8; 4096]).expect("generate");
        std::fs::remove_file(repo_root.join(".env")).expect("delete");
        tokio::time::sleep(Duration::from_millis(120)).await;

        let after = handle
            .checkpoint(
                CheckpointKind::Post,
                Attribution {
                    session_id: Some("s1".into()),
                    tool_name: Some("Bash".into()),
                    ..Attribution::default()
                },
            )
            .await
            .expect("post checkpoint");
        assert_ne!(before.generation, after.generation);
        assert_eq!(after.kind, CheckpointKind::Post);

        // A checkpoint with no changes is recorded as a noop.
        let idle = handle
            .checkpoint(CheckpointKind::Post, Attribution::default())
            .await
            .expect("noop checkpoint");
        assert_eq!(idle.kind, CheckpointKind::Noop);
        assert_eq!(idle.generation, after.generation);

        // Rewind to the pre state.
        let target = {
            let index = read_only_index(&paths.index_db());
            index.by_id(before.row_id).expect("row").expect("some")
        };
        let outcome = handle.rewind(target).await.expect("rewind");
        assert_eq!(outcome.restored, before.generation);
        drop(outcome);

        // The tree is back: edit reverted, artifact gone, .env restored.
        assert_eq!(
            std::fs::read(repo_root.join("src/main.rs")).expect("read"),
            b"fn main() {}\n"
        );
        assert!(!repo_root.join("generated.bin").exists());
        assert_eq!(
            std::fs::read(repo_root.join(".env")).expect("env"),
            b"SECRET=1\n"
        );

        // Diff before → after names exactly the changed paths.
        let status = handle.status().await.expect("status");
        assert_eq!(status.state, pipeline::State::Ready);
        handle.shutdown().await.expect("shutdown");
        (before.generation, after.generation)
    });
    thread.join().expect("pipeline thread");

    // Diff runs against the reopened store (no daemon needed).
    let store = runtime
        .block_on(Store::open(paths.clone()))
        .expect("reopen");
    let changes = runtime
        .block_on(diff::diff(&store, pre_generation, post_generation))
        .expect("diff");
    let by_name: Vec<(String, ChangeKind)> = changes
        .iter()
        .map(|change| (change.path.to_string_lossy().into_owned(), change.change))
        .collect();
    assert!(by_name.contains(&("src/main.rs".into(), ChangeKind::Modified)));
    assert!(by_name.contains(&("generated.bin".into(), ChangeKind::Added)));
    assert!(by_name.contains(&(".env".into(), ChangeKind::Removed)));
}

/// The hook contract: an acknowledged enqueue is admitted to the FIFO before
/// the ack, so a shutdown issued immediately after still processes it.
#[test]
fn enqueued_checkpoint_survives_immediate_shutdown() {
    let repo = tempfile::tempdir().expect("repo");
    let stores = tempfile::tempdir().expect("stores");
    std::fs::write(repo.path().join("file.txt"), b"before\n").expect("seed");

    let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("paths");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let store = runtime
        .block_on(Store::init(repo.path(), paths.clone()))
        .expect("init store");
    let index = Index::open(&paths.index_db()).expect("index");
    let (handle, thread) = pipeline::spawn(store, index, fast_config());

    runtime.block_on(async {
        // Sync on Ready first: a write issued during the startup baseline is
        // captured by it, and the enqueued checkpoint would be a noop.
        let status = handle.status().await.expect("status");
        assert_eq!(status.state, pipeline::State::Ready);
        std::fs::write(repo.path().join("file.txt"), b"after\n").expect("edit");
        tokio::time::sleep(Duration::from_millis(150)).await;
        handle
            .checkpoint_enqueued(
                CheckpointKind::Post,
                Attribution {
                    tool_name: Some("Edit".into()),
                    ..Attribution::default()
                },
            )
            .await
            .expect("enqueue");
        // No settling sleep: shutdown races the capture on purpose.
        handle.shutdown().await.expect("shutdown");
    });
    thread.join().expect("pipeline thread");

    let index = Index::open(&paths.index_db()).expect("reopen index");
    let latest = index.latest().expect("query").expect("row");
    assert_eq!(latest.kind, CheckpointKind::Post);
    assert!(
        latest.published,
        "shutdown commit must cover the enqueued row"
    );
}

/// Launch 2: restoring one path from a historical checkpoint touches that
/// path only, handles files, directory subtrees, and absence, and is itself
/// recorded (and so undoable) in the timeline.
#[test]
fn single_path_restore_leaves_the_rest_alone() {
    let repo = tempfile::tempdir().expect("repo");
    let stores = tempfile::tempdir().expect("stores");
    let root = repo.path();
    std::fs::create_dir_all(root.join("src/deep")).expect("mkdir");
    std::fs::write(root.join("src/main.rs"), b"v1\n").expect("seed");
    std::fs::write(root.join("src/deep/a.txt"), b"a1\n").expect("seed");
    std::fs::write(root.join("src/deep/b.txt"), b"b1\n").expect("seed");
    std::fs::write(root.join("other.txt"), b"keep\n").expect("seed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(root.join("run.sh"), b"#!/bin/sh\n").expect("seed");
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::os::unix::fs::symlink("src/main.rs", root.join("link")).expect("symlink");
    }

    let paths = StorePaths::for_repo(root, Some(stores.path())).expect("paths");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let store = runtime
        .block_on(Store::init(root, paths.clone()))
        .expect("init store");
    let index = Index::open(&paths.index_db()).expect("index");
    let (handle, thread) = pipeline::spawn(store, index, fast_config());

    runtime.block_on(async {
        let v1 = handle
            .checkpoint(CheckpointKind::Manual, Attribution::default())
            .await
            .expect("v1");

        // Mutate everything.
        std::fs::write(root.join("src/main.rs"), b"v2\n").expect("edit");
        std::fs::write(root.join("src/deep/a.txt"), b"a2\n").expect("edit");
        std::fs::remove_file(root.join("src/deep/b.txt")).expect("rm");
        std::fs::write(root.join("src/deep/c.txt"), b"c2\n").expect("add");
        std::fs::write(root.join("other.txt"), b"changed\n").expect("edit");
        std::fs::write(root.join("new.txt"), b"new\n").expect("add");
        #[cfg(unix)]
        {
            std::fs::write(root.join("run.sh"), b"#!/bin/sh\necho x\n").expect("edit");
            std::fs::remove_file(root.join("link")).expect("rm link");
            std::os::unix::fs::symlink("other.txt", root.join("link")).expect("relink");
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
        let v2 = handle
            .checkpoint(CheckpointKind::Manual, Attribution::default())
            .await
            .expect("v2");
        assert_ne!(v1.generation, v2.generation);

        let target = |row_id| {
            let index = read_only_index(&paths.index_db());
            index.by_id(row_id).expect("row").expect("some")
        };

        // One file back to v1; everything else stays at v2.
        let outcome = handle
            .restore_path(target(v1.row_id), "src/main.rs".into())
            .await
            .expect("restore file");
        assert_eq!(
            outcome.action,
            acyclic_engine::rewind::RestoreAction::Restored
        );
        assert_eq!(
            std::fs::read(root.join("src/main.rs")).expect("read"),
            b"v1\n"
        );
        assert_eq!(
            std::fs::read(root.join("other.txt")).expect("read"),
            b"changed\n"
        );
        assert_eq!(
            std::fs::read(root.join("src/deep/a.txt")).expect("read"),
            b"a2\n"
        );
        assert!(root.join("new.txt").exists());

        // A directory subtree back to v1: a1 restored, b back, c gone.
        handle
            .restore_path(target(v1.row_id), "src/deep".into())
            .await
            .expect("restore dir");
        assert_eq!(
            std::fs::read(root.join("src/deep/a.txt")).expect("read"),
            b"a1\n"
        );
        assert_eq!(
            std::fs::read(root.join("src/deep/b.txt")).expect("read"),
            b"b1\n"
        );
        assert!(!root.join("src/deep/c.txt").exists());
        assert_eq!(
            std::fs::read(root.join("other.txt")).expect("read"),
            b"changed\n"
        );

        // A path absent at the checkpoint is removed.
        let outcome = handle
            .restore_path(target(v1.row_id), "new.txt".into())
            .await
            .expect("restore absent");
        assert_eq!(
            outcome.action,
            acyclic_engine::rewind::RestoreAction::Removed
        );
        assert!(!root.join("new.txt").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            handle
                .restore_path(target(v1.row_id), "run.sh".into())
                .await
                .expect("restore exec");
            let metadata = std::fs::metadata(root.join("run.sh")).expect("meta");
            assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
            assert_eq!(
                std::fs::read(root.join("run.sh")).expect("read"),
                b"#!/bin/sh\n"
            );
            handle
                .restore_path(target(v1.row_id), "link".into())
                .await
                .expect("restore link");
            assert_eq!(
                std::fs::read_link(root.join("link")).expect("readlink"),
                Path::new("src/main.rs")
            );
        }

        // Escapes and the root are refused.
        assert!(handle
            .restore_path(target(v1.row_id), "../etc".into())
            .await
            .is_err());
        assert!(handle
            .restore_path(target(v1.row_id), ".".into())
            .await
            .is_err());

        // Each restore is recorded as a `manual` checkpoint, never as a
        // rewind (nothing is abandoned), and the pre-restore state (v2) is
        // itself restorable: undo is just another restore.
        let index = read_only_index(&paths.index_db());
        let rows = index.list(None, None, 100).expect("rows");
        let restores = rows
            .iter()
            .filter(|row| {
                row.kind == CheckpointKind::Manual
                    && row
                        .label
                        .as_deref()
                        .is_some_and(|label| label.starts_with("restore "))
            })
            .count();
        assert!(restores >= 3, "restores recorded: {restores}");
        assert!(rows.iter().all(|row| row.kind != CheckpointKind::PreRewind));
        drop(index);
        handle
            .restore_path(target(v2.row_id), "src/main.rs".into())
            .await
            .expect("undo via restore");
        assert_eq!(
            std::fs::read(root.join("src/main.rs")).expect("read"),
            b"v2\n"
        );
        handle.shutdown().await.expect("shutdown");
    });
    thread.join().expect("pipeline thread");
}
