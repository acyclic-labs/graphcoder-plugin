//! Merge v2 engine integration: the three-way plan over real generations,
//! the merge generation M, the rebase generation R, and writing R into a
//! fork overlay. Nothing here touches the working tree except to move the
//! mainline; the daemon owns landing.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "test code: a failed expectation should panic with its message"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use acyclic::config::Config;
use acyclic::fork::ForkSeed;
use acyclic::index::{Attribution, CheckpointKind, Index};
use acyclic::merge::{self, ConflictKind, Entry, Reason};
use acyclic::pipeline::{self, PipelineHandle};
use acyclic::store::{Store, StorePaths};
use acyclic::GenerationId;
use acyclic_fs::kernel::NamespacePath;
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
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(repo.path().join("d")).expect("mkdir");
        std::fs::write(repo.path().join("src/shared.txt"), b"1\n2\n3\n").expect("seed");
        std::fs::write(repo.path().join("src/same.txt"), b"same\n").expect("seed");
        std::fs::write(repo.path().join("del.txt"), b"delete me\n").expect("seed");
        std::fs::write(repo.path().join("d/k.txt"), b"k\n").expect("seed");
        std::fs::write(repo.path().join("bin.dat"), b"\x00\x01\x02").expect("seed");
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

    /// Edits the real tree, checkpoints, publishes; returns the new head.
    async fn move_mainline(&self, edits: &[(&str, Option<&[u8]>)]) -> GenerationId {
        for (path, content) in edits {
            let host = self.repo_path().join(path);
            match content {
                Some(bytes) => {
                    if let Some(parent) = host.parent() {
                        std::fs::create_dir_all(parent).expect("mkdir");
                    }
                    std::fs::write(host, bytes).expect("edit");
                }
                None => {
                    if host.is_dir() {
                        std::fs::remove_dir_all(host).expect("rmdir");
                    } else {
                        std::fs::remove_file(host).expect("rm");
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.handle
            .checkpoint(CheckpointKind::Post, Attribution::default())
            .await
            .expect("checkpoint");
        self.handle.publish_head().await.expect("publish")
    }

    fn finish(mut self) {
        self.runtime
            .block_on(self.handle.shutdown())
            .expect("shutdown");
        self.thread.take().expect("thread").join().expect("join");
    }
}

/// Native capture stores names as POSIX bytes; lookups must use the same
/// encoding or they miss (a portable-encoded name is a different key).
fn namespace(path: &str, config: acyclic_fs::model::VolumeConfig) -> NamespacePath {
    acyclic_fs::host_path_to_namespace(
        Path::new(path.trim_start_matches('/')),
        config.profile,
        config.limits,
    )
    .expect("namespace path")
}

async fn fork_write(seed: &ForkSeed, path: &str, bytes: &[u8]) {
    let cancel = CancellationToken::new();
    let mut guard = seed.shared.lock().await;
    let ns = namespace(path, guard.volume_config());
    let existing = guard
        .lookup_no_follow(&ns, WorkCounters::UNBOUNDED, &cancel)
        .await
        .expect("lookup")
        .value;
    if existing.record.is_some() {
        guard
            .remove(ns.clone(), None, WorkCounters::UNBOUNDED, &cancel)
            .await
            .expect("remove");
    }
    guard
        .create_file(
            ns,
            bytes::Bytes::copy_from_slice(bytes),
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .expect("create file in fork overlay");
}

async fn fork_remove(seed: &ForkSeed, path: &str) {
    let cancel = CancellationToken::new();
    let mut guard = seed.shared.lock().await;
    let config = guard.volume_config();
    guard
        .remove(
            namespace(path, config),
            None,
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .expect("remove in fork overlay");
}

async fn snapshot(rig: &Rig, seed: &ForkSeed) -> GenerationId {
    rig.handle
        .snapshot_overlay(Arc::clone(&seed.shared))
        .await
        .expect("snapshot")
}

async fn read(rig: &Rig, generation: GenerationId, path: &str) -> Option<Vec<u8>> {
    rig.handle
        .read_files(generation, vec![PathBuf::from(path)])
        .await
        .expect("read")
        .remove(0)
        .1
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[test]
fn batch_reads_preserve_order_and_non_regular_absence() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let generation = rig.handle.publish_head().await.expect("head");
        let contents = rig
            .handle
            .read_files(
                generation,
                vec![p("src/shared.txt"), p("missing"), p("src"), p("bin.dat")],
            )
            .await
            .expect("batch read");
        assert_eq!(
            contents,
            vec![
                (p("src/shared.txt"), Some(b"1\n2\n3\n".to_vec())),
                (p("missing"), None),
                (p("src"), None),
                (p("bin.dat"), Some(b"\x00\x01\x02".to_vec())),
            ]
        );
    });
    rig.finish();
}

/// The plan over a realistic overlap: disjoint paths on both sides, a file
/// merged by content, an identical change, and independent additions under
/// one directory. Then M = H + entries holds exactly the expected tree.
#[test]
fn plan_and_merge_generation_over_real_generations() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        let base = seed.base;
        // Fork: edit top of shared, add a file, delete a file, add under d/,
        // make the identical change to same.txt.
        fork_write(&seed, "/src/shared.txt", b"1x\n2\n3\n").await;
        fork_write(&seed, "/fork.txt", b"from fork\n").await;
        fork_remove(&seed, "/del.txt").await;
        fork_write(&seed, "/d/ours.txt", b"ours\n").await;
        fork_write(&seed, "/src/same.txt", b"SAME\n").await;
        let ours = snapshot(&rig, &seed).await;
        // Mainline: edit bottom of shared, add a file, add under d/, same change.
        let theirs = rig
            .move_mainline(&[
                ("src/shared.txt", Some(b"1\n2\n3y\n")),
                ("head.txt", Some(b"from head\n")),
                ("d/theirs.txt", Some(b"theirs\n")),
                ("src/same.txt", Some(b"SAME\n")),
            ])
            .await;
        assert_ne!(theirs, base);

        let plan = rig
            .handle
            .merge_plan(base, theirs, ours, "fork t".into())
            .await
            .expect("plan");
        assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
        assert!(plan.conflicted.is_empty(), "{:?}", plan.conflicted);
        assert_eq!(
            plan.take_ours,
            vec![p("d/ours.txt"), p("del.txt"), p("fork.txt")]
        );
        assert_eq!(plan.take_theirs, vec![p("d/theirs.txt"), p("head.txt")]);
        assert_eq!(plan.merged.len(), 1);
        assert_eq!(plan.merged[0].path, p("src/shared.txt"));
        assert_eq!(plan.merged[0].bytes, b"1x\n2\n3y\n");
        assert_eq!(
            plan.landing_paths(),
            vec![
                p("d/ours.txt"),
                p("del.txt"),
                p("fork.txt"),
                p("src/shared.txt")
            ]
        );

        // M = H + take_ours (from F) + merged.
        let mut entries: Vec<(PathBuf, Entry)> = plan
            .take_ours
            .iter()
            .map(|path| (path.clone(), Entry::FromGeneration { generation: ours }))
            .collect();
        entries.extend(plan.merged.iter().map(|file| {
            (
                file.path.clone(),
                Entry::Regular {
                    bytes: file.bytes.clone(),
                    mode: file.mode,
                },
            )
        }));
        let merged = rig
            .handle
            .build_generation(theirs, entries)
            .await
            .expect("M");
        assert_ne!(merged, theirs);
        assert_eq!(
            read(&rig, merged, "src/shared.txt").await.unwrap(),
            b"1x\n2\n3y\n"
        );
        assert_eq!(
            read(&rig, merged, "fork.txt").await.unwrap(),
            b"from fork\n"
        );
        assert_eq!(
            read(&rig, merged, "head.txt").await.unwrap(),
            b"from head\n"
        );
        assert_eq!(read(&rig, merged, "d/ours.txt").await.unwrap(), b"ours\n");
        assert_eq!(
            read(&rig, merged, "d/theirs.txt").await.unwrap(),
            b"theirs\n"
        );
        assert_eq!(read(&rig, merged, "d/k.txt").await.unwrap(), b"k\n");
        assert_eq!(read(&rig, merged, "src/same.txt").await.unwrap(), b"SAME\n");
        assert!(
            read(&rig, merged, "del.txt").await.is_none(),
            "deletion must carry"
        );
        // The head is still H: nothing was published.
        assert_eq!(rig.handle.publish_head().await.expect("head"), theirs);
        // M differs from H only by the fork's landing paths.
        let changed: Vec<PathBuf> = rig
            .handle
            .diff(theirs, merged)
            .await
            .expect("diff")
            .into_iter()
            .filter(|c| c.change != acyclic::diff::ChangeKind::MetadataOnly)
            .map(|c| c.path)
            .collect();
        assert_eq!(changed, plan.landing_paths());
    });
    rig.finish();
}

/// Conflicts of every kind are collected in full before anything is
/// written, and the rebase generation R carries the markers.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "every conflict kind is set up in one rig so the all-or-nothing rule is what's tested"
)]
fn conflicts_are_collected_and_r_carries_markers() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        let base = seed.base;
        fork_write(&seed, "/src/shared.txt", b"1\n2\n3 fork\n").await;
        fork_write(&seed, "/del.txt", b"fork kept it\n").await; // theirs deletes
        fork_remove(&seed, "/d/k.txt").await; // theirs modifies
        fork_write(&seed, "/added.txt", b"fork add\n").await; // add/add differs
        fork_write(&seed, "/clean.txt", b"clean fork\n").await; // only ours
        let ours = snapshot(&rig, &seed).await;
        let theirs = rig
            .move_mainline(&[
                ("src/shared.txt", Some(b"1\n2\n3 head\n")),
                ("del.txt", None),
                ("d/k.txt", Some(b"k head\n")),
                ("added.txt", Some(b"head add\n")),
            ])
            .await;

        let plan = rig
            .handle
            .merge_plan(base, theirs, ours, "fork c".into())
            .await
            .expect("plan");
        assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
        assert_eq!(plan.take_ours, vec![p("clean.txt")]);
        let mut kinds: Vec<(PathBuf, ConflictKind)> = plan
            .conflicted
            .iter()
            .map(|c| (c.path.clone(), c.kind))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                (p("added.txt"), ConflictKind::Hunks),
                (p("d/k.txt"), ConflictKind::OursDeleted),
                (p("del.txt"), ConflictKind::TheirsDeleted),
                (p("src/shared.txt"), ConflictKind::Hunks),
            ]
        );
        let shared = plan
            .conflicted
            .iter()
            .find(|c| c.path == p("src/shared.txt"))
            .unwrap();
        let text = String::from_utf8(shared.bytes.clone()).unwrap();
        assert!(
            text.starts_with(
                "1\n2\n<<<<<<< fork c\n3 fork\n||||||| original\n3\n=======\n3 head\n\
                 >>>>>>> mainline\n"
            ),
            "{text}"
        );
        assert_eq!(shared.describe(), "1 conflicting hunk(s)");
        let del = plan
            .conflicted
            .iter()
            .find(|c| c.path == p("del.txt"))
            .unwrap();
        assert!(String::from_utf8_lossy(&del.bytes).starts_with("<<<<<<< fork c (modified)\n"));
        assert_eq!(del.describe(), "mainline deleted, fork modified");

        // R = H + take_ours + conflicted (markers).
        let mut entries = vec![(p("clean.txt"), Entry::FromGeneration { generation: ours })];
        entries.extend(plan.conflicted.iter().map(|c| {
            (
                c.path.clone(),
                Entry::Regular {
                    bytes: c.bytes.clone(),
                    mode: c.mode,
                },
            )
        }));
        let rebased = rig
            .handle
            .build_generation(theirs, entries)
            .await
            .expect("R");
        let markers = read(&rig, rebased, "src/shared.txt").await.unwrap();
        assert!(merge::has_conflict_markers(
            std::str::from_utf8(&markers).unwrap()
        ));
        assert_eq!(
            read(&rig, rebased, "clean.txt").await.unwrap(),
            b"clean fork\n"
        );
        // Head-only content is present in R (a rebase brings the fork up to H).
        let rebased_k = read(&rig, rebased, "d/k.txt")
            .await
            .map(|b| merge::has_conflict_markers(std::str::from_utf8(&b).unwrap()));
        assert_eq!(rebased_k, Some(true));

        // Writing R − F into the fork overlay makes the overlay equal R.
        let changes: Vec<PathBuf> = rig
            .handle
            .diff(ours, rebased)
            .await
            .expect("diff")
            .into_iter()
            .filter(|c| c.change != acyclic::diff::ChangeKind::MetadataOnly)
            .map(|c| c.path)
            .collect();
        let roots = merge::subtree_roots(&changes);
        let entries: Vec<(PathBuf, Entry)> = roots
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    Entry::FromGeneration {
                        generation: rebased,
                    },
                )
            })
            .collect();
        rig.handle
            .apply_to_overlay(Arc::clone(&seed.shared), entries)
            .await
            .expect("apply");
        let after = snapshot(&rig, &seed).await;
        let remaining: Vec<PathBuf> = rig
            .handle
            .diff(rebased, after)
            .await
            .expect("diff")
            .into_iter()
            .filter(|c| c.change != acyclic::diff::ChangeKind::MetadataOnly)
            .map(|c| c.path)
            .collect();
        assert!(remaining.is_empty(), "overlay must equal R: {remaining:?}");
        // And the head still has not moved.
        assert_eq!(rig.handle.publish_head().await.expect("head"), theirs);
    });
    rig.finish();
}

/// Refusals: binary, kind change, ancestry; and they name the path.
#[test]
fn refusals_name_paths_and_reasons() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        let base = seed.base;
        fork_write(&seed, "/bin.dat", b"\x00\x09").await;
        fork_remove(&seed, "/d/k.txt").await;
        fork_remove(&seed, "/d").await; // fork deletes the directory
        {
            let cancel = CancellationToken::new();
            let mut guard = seed.shared.lock().await;
            let config = guard.volume_config();
            guard
                .remove(
                    namespace("/src/same.txt", config),
                    None,
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .expect("rm");
            guard
                .create_symbolic_link(
                    namespace("/src/same.txt", config),
                    bytes::Bytes::from_static(b"shared.txt"),
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .expect("symlink");
        }
        let ours = snapshot(&rig, &seed).await;
        let theirs = rig
            .move_mainline(&[
                ("bin.dat", Some(b"\x00\x0a")),
                ("d/new.txt", Some(b"inside\n")),
                ("src/same.txt", Some(b"edited\n")),
            ])
            .await;
        let plan = rig
            .handle
            .merge_plan(base, theirs, ours, "fork r".into())
            .await
            .expect("plan");
        let mut refusals: Vec<(PathBuf, Reason)> = plan
            .refusals
            .iter()
            .map(|r| (r.path.clone(), r.reason.clone()))
            .collect();
        refusals.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(refusals.len(), 3, "{refusals:?}");
        assert_eq!(refusals[0], (p("bin.dat"), Reason::Binary));
        assert_eq!(
            refusals[1],
            (
                p("d"),
                Reason::Ancestry {
                    inner: vec![p("d/new.txt")]
                }
            )
        );
        assert_eq!(refusals[2], (p("src/same.txt"), Reason::KindChange));
        assert!(plan.conflicted.is_empty());
        // Rendered with the host's separator: `Path` compares component-wise,
        // so the assertion above passes on Windows even though the display
        // form there uses a backslash.
        let named = Path::new("d").join("new.txt");
        assert!(format!("{}", refusals[1].1).contains(&named.display().to_string()));
    });
    rig.finish();
}

/// Rebased twice: after a rebase the fork's base is H, so a second plan
/// against a further-moved head sees only what the fork changed since H.
#[test]
fn plan_after_rebase_uses_the_new_base() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        fork_write(&seed, "/src/shared.txt", b"1\n2\n3 fork\n").await;
        let ours = snapshot(&rig, &seed).await;
        let h1 = rig
            .move_mainline(&[("src/shared.txt", Some(b"1\n2\n3 head\n"))])
            .await;
        let plan = rig
            .handle
            .merge_plan(seed.base, h1, ours, "fork x".into())
            .await
            .expect("plan");
        assert_eq!(plan.conflicted.len(), 1);
        // Rebase: write R into the fork.
        let entries = vec![(
            p("src/shared.txt"),
            Entry::Regular {
                bytes: plan.conflicted[0].bytes.clone(),
                mode: None,
            },
        )];
        rig.handle
            .apply_to_overlay(Arc::clone(&seed.shared), entries)
            .await
            .expect("apply");
        // The agent resolves the markers in the fork.
        fork_write(&seed, "/src/shared.txt", b"1\n2\n3 resolved\n").await;
        let resolved = snapshot(&rig, &seed).await;
        // Mainline moves again, elsewhere.
        let h2 = rig.move_mainline(&[("other.txt", Some(b"o\n"))]).await;
        let plan = rig
            .handle
            .merge_plan(h1, h2, resolved, "fork x".into())
            .await
            .expect("plan");
        assert!(plan.conflicted.is_empty());
        assert!(plan.refusals.is_empty());
        assert_eq!(plan.take_ours, vec![p("src/shared.txt")]);
        assert_eq!(plan.take_theirs, vec![p("other.txt")]);
    });
    let _ = &rig.paths;
    rig.finish();
}

/// A fork-only nested directory copies whole into M (G7's shape).
#[test]
fn nested_directory_subtree_copies_into_merge_generation() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        {
            let cancel = CancellationToken::new();
            let mut guard = seed.shared.lock().await;
            let config = guard.volume_config();
            guard
                .create_directory(namespace("/docs", config), WorkCounters::UNBOUNDED, &cancel)
                .await
                .expect("mkdir");
            guard
                .create_directory(
                    namespace("/docs/deep", config),
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .expect("mkdir");
            guard
                .create_directory(
                    namespace("/docs/deep/er", config),
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .expect("mkdir");
        }
        fork_write(&seed, "/docs/deep/er/file.md", b"deep\n").await;
        fork_remove(&seed, "/del.txt").await;
        let ours = snapshot(&rig, &seed).await;
        let theirs = rig.move_mainline(&[("x.txt", Some(b"x\n"))]).await;
        let plan = rig
            .handle
            .merge_plan(seed.base, theirs, ours, "fork n".into())
            .await
            .expect("plan");
        assert_eq!(plan.take_ours, vec![p("del.txt"), p("docs")]);
        let entries: Vec<(PathBuf, Entry)> = plan
            .take_ours
            .iter()
            .map(|path| (path.clone(), Entry::FromGeneration { generation: ours }))
            .collect();
        let merged = rig
            .handle
            .build_generation(theirs, entries)
            .await
            .expect("M");
        assert_eq!(
            read(&rig, merged, "docs/deep/er/file.md").await.unwrap(),
            b"deep\n"
        );
        assert!(read(&rig, merged, "del.txt").await.is_none());
        assert_eq!(read(&rig, merged, "x.txt").await.unwrap(), b"x\n");
    });
    rig.finish();
}

/// `materialize_paths` writes a generation's paths into a plain directory
/// with ordinary filesystem operations: replaced files, recreated files
/// that were absent, removed paths, and nested subtrees.
#[test]
fn materialize_paths_writes_a_generation_into_a_directory() {
    let rig = Rig::start();
    rig.runtime.block_on(async {
        let seed = rig.handle.fork().await.expect("fork");
        fork_write(&seed, "/src/shared.txt", b"rewritten\n").await;
        fork_remove(&seed, "/del.txt").await;
        {
            let cancel = CancellationToken::new();
            let mut guard = seed.shared.lock().await;
            let config = guard.volume_config();
            guard
                .create_directory(namespace("/docs", config), WorkCounters::UNBOUNDED, &cancel)
                .await
                .expect("mkdir");
            guard
                .create_directory(
                    namespace("/docs/deep", config),
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .expect("mkdir");
        }
        fork_write(&seed, "/docs/deep/file.md", b"deep\n").await;
        let generation = snapshot(&rig, &seed).await;

        // A workspace that currently mirrors the base: shared.txt old,
        // del.txt present, no docs/, plus a stray file that must survive.
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/shared.txt"), b"1\n2\n3\n").unwrap();
        std::fs::write(dir.path().join("del.txt"), b"delete me\n").unwrap();
        std::fs::write(dir.path().join("untouched.txt"), b"keep\n").unwrap();
        rig.handle
            .materialize_paths(
                generation,
                dir.path().to_path_buf(),
                vec![p("src/shared.txt"), p("del.txt"), p("docs")],
            )
            .await
            .expect("materialize");
        assert_eq!(
            std::fs::read(dir.path().join("src/shared.txt")).unwrap(),
            b"rewritten\n"
        );
        assert!(
            !dir.path().join("del.txt").exists(),
            "absent in the generation: removed"
        );
        assert_eq!(
            std::fs::read(dir.path().join("docs/deep/file.md")).unwrap(),
            b"deep\n"
        );
        assert_eq!(
            std::fs::read(dir.path().join("untouched.txt")).unwrap(),
            b"keep\n"
        );
        // Recreating a file that was removed from the directory works too.
        std::fs::remove_file(dir.path().join("src/shared.txt")).unwrap();
        rig.handle
            .materialize_paths(
                generation,
                dir.path().to_path_buf(),
                vec![p("src/shared.txt")],
            )
            .await
            .expect("materialize again");
        assert_eq!(
            std::fs::read(dir.path().join("src/shared.txt")).unwrap(),
            b"rewritten\n"
        );
    });
    rig.finish();
}
