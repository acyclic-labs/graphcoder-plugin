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
        std::fs::write(repo_root.join("src/main.rs"), b"fn main() { changed(); }\n")
            .expect("edit");
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
    let store = runtime.block_on(Store::open(paths.clone())).expect("reopen");
    let changes = runtime
        .block_on(diff::diff(&store, pre_generation, post_generation))
        .expect("diff");
    let by_name: Vec<(String, ChangeKind)> = changes
        .iter()
        .map(|change| {
            (
                change.path.to_string_lossy().into_owned(),
                change.change,
            )
        })
        .collect();
    assert!(by_name.contains(&("src/main.rs".into(), ChangeKind::Modified)));
    assert!(by_name.contains(&("generated.bin".into(), ChangeKind::Added)));
    assert!(by_name.contains(&(".env".into(), ChangeKind::Removed)));
}
