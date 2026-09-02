//! Phase 0 qualification harness for the Rewind milestone.
//!
//! Subcommands:
//!   fixture <dir> [--with-fifo]   build a small fixture repo exercising the tricky cases
//!   roundtrip <src> <work>        capture <src> into a fresh store under <work>, commit,
//!                                 materialize into <work>/restore, compare content+mode
//!
//! Exit code 0 = round-trip verified identical; 1 = mismatches or engine failure.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use acyclic_fs::model::{
    AccessMode, CheckoutMode, ConsistencyMode, FilesystemProfile, GenerationSelector, Lifecycle,
    MutationMode, VolumeConfig,
};
use acyclic_fs::{
    CancellationToken, CheckoutCommitOutcome, GenerationId, LocalFs, LocalOptions, OperationId,
    VolumeId, WorkCounters,
};
use acyclic_fs::{
    capture_baseline, capture_root_identity, materialize_checkout, CaptureOptions,
    MaterializeOptions,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("fixture") => fixture(&args[1..]),
        Some("roundtrip") => roundtrip(&args[1..]),
        Some("corpus") => corpus(&args[1..]),
        Some("bench") => bench(&args[1..]),
        Some("restore-gen") => restore_gen(&args[1..]),
        Some("mount-smoke") => mount_smoke(&args[1..]),
        Some("mount-smoke2") => mount_smoke2(&args[1..]),
        Some("mount-hold") => mount_hold(&args[1..]),
        Some("source-probe") => source_probe(&args[1..]),
        _ => Err(
            "usage: acyclic-qual fixture <dir> [--with-fifo] | roundtrip <src> <work> \
             | corpus <dir> <files> <mb> | bench <src> <work> [rounds]"
                .into(),
        ),
    };
    if let Err(message) = result {
        eprintln!("FAIL: {message}");
        std::process::exit(1);
    }
}

type Failure = Box<dyn std::error::Error>;

fn engine_err<E: std::fmt::Debug>(context: &str) -> impl FnOnce(E) -> Failure + '_ {
    move |error| format!("{context}: {error:?}").into()
}

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

fn fixture(args: &[String]) -> Result<(), Failure> {
    let root = PathBuf::from(args.first().ok_or("fixture: missing <dir>")?);
    let with_fifo = args.iter().any(|a| a == "--with-fifo");
    fs::create_dir_all(root.join("src/nested"))?;
    fs::create_dir_all(root.join("node_modules/.bin"))?;
    fs::create_dir_all(root.join(".git/objects"))?;
    fs::create_dir_all(root.join("empty-dir"))?;

    fs::write(root.join("README.md"), b"fixture repo\n")?;
    fs::write(root.join(".gitignore"), b"generated/\n.env\n")?;
    fs::write(root.join(".env"), b"SECRET=hunter2\n")?;
    fs::write(root.join("src/main.rs"), b"fn main() {}\n")?;
    fs::write(root.join("src/nested/mod.rs"), b"// nested\n")?;
    fs::write(root.join(".git/objects/pack-data"), b"\x00\x01\x02git\n")?;
    fs::write(root.join("uni-\u{00e9}\u{4e2d}.txt"), b"unicode name\n")?;

    // Deterministic 8 MiB binary file (multi-chunk content).
    let mut big = Vec::with_capacity(8 * 1024 * 1024);
    let mut state: u32 = 0x9e37_79b9;
    while big.len() < 8 * 1024 * 1024 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        big.extend_from_slice(&state.to_le_bytes());
    }
    fs::write(root.join("assets.bin"), &big)?;

    // Executable script (mode-bit round-trip).
    let script = root.join("tool.sh");
    fs::write(&script, b"#!/bin/sh\necho ok\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    }

    // Symlinks: valid relative (the node_modules/.bin shape) and dangling.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("../../tool.sh", root.join("node_modules/.bin/tool"))?;
        symlink("no-such-target", root.join("dangling-link"))?;
    }

    // Hard-link pair.
    fs::write(root.join("hardlink-a"), b"same inode\n")?;
    fs::hard_link(root.join("hardlink-a"), root.join("hardlink-b"))?;

    if with_fifo {
        let status = std::process::Command::new("mkfifo")
            .arg(root.join("pipe.fifo"))
            .status()?;
        if !status.success() {
            return Err("mkfifo failed".into());
        }
    }

    println!("fixture written to {}", root.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// restore-gen: materialize one generation from an existing store
// ---------------------------------------------------------------------------

fn restore_gen(args: &[String]) -> Result<(), Failure> {
    let store_dir = PathBuf::from(args.first().ok_or("restore-gen: missing <store/store>")?);
    let volume_hex = args.get(1).ok_or("restore-gen: missing <volume-uuid>")?;
    let generation_hex = args.get(2).ok_or("restore-gen: missing <gen-hex>")?;
    let destination = PathBuf::from(args.get(3).ok_or("restore-gen: missing <dest>")?);
    fs::create_dir_all(&destination)?;

    let volume_uuid: [u8; 16] = {
        let clean: String = volume_hex.chars().filter(|c| *c != '-').collect();
        let bytes = hex_decode(&clean)?;
        bytes.as_slice().try_into().map_err(|_| "volume uuid must be 16 bytes")?
    };
    let digest: [u8; 32] = hex_decode(generation_hex)?
        .as_slice()
        .try_into()
        .map_err(|_| "generation must be 32 bytes")?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let fs_engine =
            LocalFs::local(LocalOptions::new(&store_dir)).map_err(engine_err("open store"))?;
        let volume = fs_engine
            .open_volume(
                VolumeId::from_bytes(volume_uuid),
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(engine_err("open volume"))?
            .value;
        let mut checkout = volume
            .checkout(
                GenerationSelector::Exact(GenerationId::new(acyclic_fs::Digest::from_bytes(
                    digest,
                ))),
                CheckoutMode {
                    access: AccessMode::ReadOnly,
                    consistency: ConsistencyMode::Pinned,
                    mutations: MutationMode::None,
                },
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(engine_err("checkout exact"))?
            .value;
        let receipt = materialize_checkout(
            &mut checkout,
            &MaterializeOptions {
                destination: destination.clone(),
                maximum_directory_entries: 1_024,
                maximum_extent_spans: 65_536,
                transfer_bytes: 8 * 1024 * 1024,
            },
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(engine_err("materialize"))?
        .value;
        println!(
            "materialized {} files / {} dirs into {}",
            receipt.files,
            receipt.directories,
            destination.display()
        );
        Ok(())
    })
}

fn hex_decode(text: &str) -> Result<Vec<u8>, Failure> {
    if !text.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| format!("{e}").into()))
        .collect()
}

// ---------------------------------------------------------------------------
// mount-smoke: Launch 3 gate — writable native mount of a captured checkout
// ---------------------------------------------------------------------------

fn mount_smoke(args: &[String]) -> Result<(), Failure> {
    use acyclic_fs::{
        mount_native, probe_native_mount, CheckoutMountSource, NativeMountRequest, SharedCheckout,
    };
    use std::sync::Arc;

    let source = PathBuf::from(args.first().ok_or("mount-smoke: missing <src>")?)
        .canonicalize()?;
    let work = PathBuf::from(args.get(1).ok_or("mount-smoke: missing <work>")?);
    let store_dir = work.join("store");
    let mount_dir = work.join("mnt");
    fs::create_dir_all(&store_dir)?;
    fs::create_dir_all(&mount_dir)?;

    let capabilities = probe_native_mount();
    println!(
        "probe: kind={:?} available={} writable={} reason={:?}",
        capabilities.kind, capabilities.available, capabilities.writable,
        capabilities.unavailable_reason
    );
    if !capabilities.available || !capabilities.writable {
        return Err("native mount unavailable or read-only — fork engine gate FAILS".into());
    }

    // Capture the fixture and publish it, exactly like the daemon does.
    let runtime = tokio::runtime::Runtime::new()?;
    let (volume_id, _generation) = runtime.block_on(capture_and_commit(&source, &store_dir))?;

    // Fresh writable Head checkout for the mount (the fork shape).
    let config = volume_config();
    let (checkout, fs_engine) = runtime.block_on(async {
        let cancel = CancellationToken::new();
        let fs_engine = LocalFs::local(LocalOptions::new(&store_dir))
            .map_err(engine_err("reopen store"))?;
        let volume = fs_engine
            .open_volume(volume_id, WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(engine_err("open volume"))?
            .value;
        let checkout = volume
            .checkout(
                GenerationSelector::Head,
                CheckoutMode {
                    access: AccessMode::ReadWrite,
                    consistency: ConsistencyMode::TrackingSafe,
                    mutations: MutationMode::PrivateOverlay,
                },
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(engine_err("checkout head"))?
            .value;
        Ok::<_, Failure>((checkout, fs_engine))
    })?;
    let _keep_engine_alive = fs_engine;

    let shared = Arc::new(SharedCheckout::new(checkout));
    let mount_source = Arc::new(
        CheckoutMountSource::new(Arc::clone(&shared), config)
            .map_err(engine_err("mount source"))?,
    );
    let started = Instant::now();
    let mut session = mount_native(
        NativeMountRequest {
            mount_id: acyclic_fs::MountId::new(),
            volume_id,
            destination: mount_dir.clone(),
            writable: true,
        },
        mount_source,
    )
    .map_err(engine_err("mount_native"))?;
    println!("mounted at {} in {:?}", mount_dir.display(), started.elapsed());

    // Everything below must not leave the mount attached on failure.
    let verdict = (|| -> Result<(), Failure> {
        // Read path: lazy listing + content identical to the fixture.
        let started = Instant::now();
        let mounted_readme = fs::read(mount_dir.join("README.md"))?;
        println!("first small read: {:?}", started.elapsed());
        if mounted_readme != fs::read(source.join("README.md"))? {
            return Err("README content mismatch through mount".into());
        }
        let started = Instant::now();
        let mounted_asset = fs::read(mount_dir.join("assets.bin"))?;
        println!(
            "8MB hydration read: {:?} ({} bytes)",
            started.elapsed(),
            mounted_asset.len()
        );
        if mounted_asset != fs::read(source.join("assets.bin"))? {
            return Err("asset content mismatch through mount".into());
        }
        let mounted_link = fs::read_link(mount_dir.join("node_modules/.bin/tool"))?;
        if mounted_link != PathBuf::from("../../tool.sh") {
            return Err(format!("symlink mismatch through mount: {mounted_link:?}").into());
        }

        // Write path: overlay writes visible in the mount, invisible outside.
        fs::write(mount_dir.join("fork-note.txt"), b"written in the fork\n")?;
        fs::write(mount_dir.join("README.md"), b"edited in the fork\n")?;
        fs::create_dir(mount_dir.join("fork-dir"))?;
        if fs::read(mount_dir.join("fork-note.txt"))? != b"written in the fork\n" {
            return Err("write-then-read mismatch through mount".into());
        }
        if fs::read(mount_dir.join("README.md"))? != b"edited in the fork\n" {
            return Err("edit-then-read mismatch through mount".into());
        }
        if source.join("fork-note.txt").exists() {
            return Err("overlay write leaked into the source tree".into());
        }
        if fs::read(source.join("README.md"))? != mounted_readme {
            return Err("overlay edit leaked into the source tree".into());
        }
        Ok(())
    })();

    let stopped = session.stop();
    println!("unmounted cleanly: {stopped:?}");
    verdict?;
    stopped.map_err(engine_err("unmount"))?;

    println!("MOUNT SMOKE OK: writable native mount serves, isolates, and detaches");
    Ok(())
}

// ---------------------------------------------------------------------------
// mount-hold: attach one mount and hold it for <seconds> so failing
// operations can be probed interactively from a shell.
// ---------------------------------------------------------------------------

fn mount_hold(args: &[String]) -> Result<(), Failure> {
    use acyclic_fs::{
        mount_native, CheckoutMountSource, NativeMountRequest, SharedCheckout,
    };
    use std::sync::Arc;

    let source = PathBuf::from(args.first().ok_or("mount-hold: missing <src>")?)
        .canonicalize()?;
    let work = PathBuf::from(args.get(1).ok_or("mount-hold: missing <work>")?);
    let seconds: u64 = args.get(2).map_or(Ok(60), |value| value.parse())?;
    let store_dir = work.join("store");
    let mount_dir = work.join("mnt");
    fs::create_dir_all(&store_dir)?;
    fs::create_dir_all(&mount_dir)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let (volume_id, _generation) = runtime.block_on(capture_and_commit(&source, &store_dir))?;
    let config = volume_config();
    let checkout = runtime.block_on(async {
        let cancel = CancellationToken::new();
        let fs_engine =
            LocalFs::local(LocalOptions::new(&store_dir)).map_err(engine_err("open store"))?;
        let volume = fs_engine
            .open_volume(volume_id, WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(engine_err("open volume"))?
            .value;
        let checkout = volume
            .checkout(
                GenerationSelector::Head,
                CheckoutMode {
                    access: AccessMode::ReadWrite,
                    consistency: ConsistencyMode::TrackingSafe,
                    mutations: MutationMode::PrivateOverlay,
                },
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(engine_err("checkout"))?
            .value;
        std::mem::forget(fs_engine);
        Ok::<_, Failure>(checkout)
    })?;
    let shared = Arc::new(SharedCheckout::new(checkout));
    let mount_source = Arc::new(
        CheckoutMountSource::new(shared, config).map_err(engine_err("mount source"))?,
    );
    let mut session = mount_native(
        NativeMountRequest {
            mount_id: acyclic_fs::MountId::new(),
            volume_id,
            destination: mount_dir.clone(),
            writable: true,
        },
        mount_source,
    )
    .map_err(engine_err("mount"))?;
    println!("HELD: {} for {seconds}s", mount_dir.display());
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    session.stop().map_err(engine_err("unmount"))?;
    println!("released");
    Ok(())
}

// ---------------------------------------------------------------------------
// source-probe: exercise MountFilesystem directly (no kernel, no NFS) to
// pinpoint which callback fails and with what typed error.
// ---------------------------------------------------------------------------

fn source_probe(args: &[String]) -> Result<(), Failure> {
    use acyclic_fs::{CheckoutMountSource, MountFilesystem, MountPath, SharedCheckout};
    use std::sync::Arc;

    let source = PathBuf::from(args.first().ok_or("source-probe: missing <src>")?)
        .canonicalize()?;
    let work = PathBuf::from(args.get(1).ok_or("source-probe: missing <work>")?);
    let store_dir = work.join("store");
    fs::create_dir_all(&store_dir)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let (volume_id, _generation) = runtime.block_on(capture_and_commit(&source, &store_dir))?;
    let config = volume_config();
    let checkout = runtime.block_on(async {
        let cancel = CancellationToken::new();
        let fs_engine =
            LocalFs::local(LocalOptions::new(&store_dir)).map_err(engine_err("open store"))?;
        let volume = fs_engine
            .open_volume(volume_id, WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(engine_err("open volume"))?
            .value;
        let checkout = volume
            .checkout(
                GenerationSelector::Head,
                CheckoutMode {
                    access: AccessMode::ReadWrite,
                    consistency: ConsistencyMode::TrackingSafe,
                    mutations: MutationMode::PrivateOverlay,
                },
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(engine_err("checkout"))?
            .value;
        std::mem::forget(fs_engine);
        Ok::<_, Failure>(checkout)
    })?;
    let shared = Arc::new(SharedCheckout::new(checkout));
    let mount_source =
        CheckoutMountSource::new(shared, config).map_err(engine_err("mount source"))?;

    let readme = MountPath::root().child(b"README.md".to_vec());
    println!("lookup(/):          {:?}", mount_source.lookup(&MountPath::root()).map(|l| l.map(|l| l.node.kind)));
    println!("lookup(README.md):  {:?}", mount_source.lookup(&readme).map(|l| l.map(|l| (l.node.kind, l.node.logical_bytes))));
    println!(
        "read_directory(/):  {:?}",
        mount_source
            .read_directory(&MountPath::root(), None, 16)
            .map(|p| p.entries.len())
    );
    match mount_source.open_file(&readme) {
        Ok(file) => {
            println!("open_file(README):  Ok");
            println!("  handle.lookup():  {:?}", file.lookup().map(|l| l.node.logical_bytes));
            println!("  read_range(0,13): {:?}", file.read_range(0, 13).map(|b| String::from_utf8_lossy(&b).into_owned()));
        }
        Err(error) => println!("open_file(README):  ERR {error:?}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// mount-smoke2: TWO simultaneous native mounts from ONE process — the
// minimal repro for the fork engine's N>1 requirement.
// ---------------------------------------------------------------------------

fn mount_smoke2(args: &[String]) -> Result<(), Failure> {
    use acyclic_fs::{
        mount_native, CheckoutMountSource, NativeMountRequest, SharedCheckout,
    };
    use std::sync::Arc;

    let source = PathBuf::from(args.first().ok_or("mount-smoke2: missing <src>")?)
        .canonicalize()?;
    let work = PathBuf::from(args.get(1).ok_or("mount-smoke2: missing <work>")?);
    let store_dir = work.join("store");
    fs::create_dir_all(&store_dir)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let (volume_id, _generation) = runtime.block_on(capture_and_commit(&source, &store_dir))?;
    let config = volume_config();

    let mut sessions = Vec::new();
    for index in 0..2u32 {
        let mount_dir = work.join(format!("mnt{index}"));
        fs::create_dir_all(&mount_dir)?;
        let checkout = runtime.block_on(async {
            let cancel = CancellationToken::new();
            let fs_engine = LocalFs::local(LocalOptions::new(&store_dir))
                .map_err(engine_err("open store"))?;
            let volume = fs_engine
                .open_volume(volume_id, WorkCounters::UNBOUNDED, &cancel)
                .await
                .map_err(engine_err("open volume"))?
                .value;
            let checkout = volume
                .checkout(
                    GenerationSelector::Head,
                    CheckoutMode {
                        access: AccessMode::ReadWrite,
                        consistency: ConsistencyMode::TrackingSafe,
                        mutations: MutationMode::PrivateOverlay,
                    },
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .map_err(engine_err("checkout"))?
                .value;
            std::mem::forget(fs_engine); // keep engine alive for the mount
            Ok::<_, Failure>(checkout)
        })?;
        let shared = Arc::new(SharedCheckout::new(checkout));
        let mount_source = Arc::new(
            CheckoutMountSource::new(shared, config).map_err(engine_err("mount source"))?,
        );
        let started = Instant::now();
        let session = mount_native(
            NativeMountRequest {
                mount_id: acyclic_fs::MountId::new(),
                volume_id,
                destination: mount_dir.clone(),
                writable: true,
            },
            mount_source,
        )
        .map_err(engine_err(if index == 0 {
            "FIRST mount"
        } else {
            "SECOND mount"
        }))?;
        println!(
            "mount {index} attached at {} in {:?}",
            mount_dir.display(),
            started.elapsed()
        );
        let listing = fs::read_dir(&mount_dir)?.count();
        println!("mount {index} lists {listing} entries");
        sessions.push(session);
    }

    for (index, mut session) in sessions.into_iter().enumerate() {
        session.stop().map_err(engine_err("unmount"))?;
        println!("mount {index} detached");
    }
    println!("MOUNT SMOKE2 OK: two simultaneous sessions in one process");
    Ok(())
}

// ---------------------------------------------------------------------------
// corpus: synthetic tree of <files> files totalling <mb> MiB, 100 per dir
// ---------------------------------------------------------------------------

fn corpus(args: &[String]) -> Result<(), Failure> {
    let root = PathBuf::from(args.first().ok_or("corpus: missing <dir>")?);
    let files: u64 = args.get(1).ok_or("corpus: missing <files>")?.parse()?;
    let total_mb: u64 = args.get(2).ok_or("corpus: missing <mb>")?.parse()?;
    let bytes_per_file = (total_mb * 1024 * 1024) / files.max(1);
    let mut payload = Vec::with_capacity(bytes_per_file as usize);
    let mut state: u32 = 0x1234_5678;
    while (payload.len() as u64) < bytes_per_file {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        payload.extend_from_slice(&state.to_le_bytes());
    }
    let started = Instant::now();
    for index in 0..files {
        let dir = root.join(format!("dir-{:05}", index / 100));
        if index % 100 == 0 {
            fs::create_dir_all(&dir)?;
        }
        fs::write(dir.join(format!("file-{index:07}.dat")), &payload)?;
    }
    println!(
        "corpus: {files} files x {bytes_per_file} bytes in {:?}",
        started.elapsed()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// bench: baseline capture, then watch-driven incremental capture latency
// ---------------------------------------------------------------------------

fn bench(args: &[String]) -> Result<(), Failure> {
    let source = PathBuf::from(args.first().ok_or("bench: missing <src>")?).canonicalize()?;
    let work = PathBuf::from(args.get(1).ok_or("bench: missing <work>")?);
    let rounds: usize = args.get(2).map_or(Ok(30), |value| value.parse())?;
    let store_dir = work.join("store");
    fs::create_dir_all(&store_dir)?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(bench_inner(&source, &store_dir, rounds))
}

async fn bench_inner(source: &Path, store_dir: &Path, rounds: usize) -> Result<(), Failure> {
    use acyclic_fs::{NativeWatch, NativeWatchOptions, WatchBatch};
    use acyclic_fs::model::VolumeLimits;

    let cancel = CancellationToken::new();
    let fs_engine =
        LocalFs::local(LocalOptions::new(store_dir)).map_err(engine_err("open store"))?;
    let volume = fs_engine
        .create_volume(volume_config(), WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("create volume"))?
        .value;
    let mut checkout = volume
        .checkout(
            GenerationSelector::Head,
            CheckoutMode {
                access: AccessMode::ReadWrite,
                consistency: ConsistencyMode::TrackingSafe,
                mutations: MutationMode::PrivateOverlay,
            },
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(engine_err("checkout"))?
        .value;

    let mut watch = NativeWatch::open(
        source,
        NativeWatchOptions {
            limits: VolumeLimits::default(),
            maximum_queued_changes: 65_536,
            recursive: true,
        },
    )
    .map_err(engine_err("watch open"))?;
    watch.begin_rescan().map_err(engine_err("begin_rescan"))?;

    let options = CaptureOptions {
        source_root: source.to_path_buf(),
        expected_root_identity: capture_root_identity(source).map_err(engine_err("identity"))?,
        maximum_paths: 4_000_000,
        maximum_extent_spans: 65_536,
    };

    let started = Instant::now();
    let receipt = capture_baseline(&mut checkout, &options, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("baseline"))?
        .value;
    let baseline_capture = started.elapsed();
    let started = Instant::now();
    checkout
        .commit(OperationId::new(), WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("baseline commit"))?;
    println!(
        "baseline: capture {:?} + commit {:?} ({} paths, {} bytes)",
        baseline_capture,
        started.elapsed(),
        receipt.examined_paths,
        receipt.staged_file_bytes
    );
    watch.finish_rescan().map_err(engine_err("finish_rescan"))?;

    let mut event_latency = Vec::with_capacity(rounds);
    let mut capture_only = Vec::with_capacity(rounds);
    let mut capture_checkpoint = Vec::with_capacity(rounds);
    let mut capture_commit = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let target = source.join(format!("bench-mutation-{}.txt", round % 5));
        let mutated_at = Instant::now();
        fs::write(&target, format!("round {round} at {mutated_at:?}\n"))?;

        // Poll until the watcher reports the change (event-arrival latency).
        let batch = loop {
            let batch = watch
                .poll(4_096, WorkCounters::UNBOUNDED, &cancel)
                .map_err(engine_err("poll"))?
                .value;
            match &batch {
                WatchBatch::Changes { changes, .. } if !changes.is_empty() => break batch,
                WatchBatch::Changes { .. } => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                WatchBatch::RescanRequired { reason, .. } => {
                    return Err(format!("rescan required mid-bench: {reason:?}").into());
                }
            }
        };
        event_latency.push(mutated_at.elapsed());

        let capture_started = Instant::now();
        acyclic_fs::capture_watch_batch(
            &mut checkout,
            batch,
            &options,
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(engine_err("capture_watch_batch"))?;
        let captured_at = capture_started.elapsed();
        if round % 2 == 0 {
            checkout
                .checkpoint(WorkCounters::UNBOUNDED, &cancel)
                .await
                .map_err(engine_err("incremental checkpoint"))?;
            capture_checkpoint.push(capture_started.elapsed());
        } else {
            checkout
                .commit(OperationId::new(), WorkCounters::UNBOUNDED, &cancel)
                .await
                .map_err(engine_err("incremental commit"))?;
            capture_commit.push(capture_started.elapsed());
        }
        capture_only.push(captured_at);
    }

    report("event-arrival latency", &mut event_latency);
    report("capture (no publish) latency", &mut capture_only);
    report("capture+checkpoint latency", &mut capture_checkpoint);
    report("capture+commit latency", &mut capture_commit);
    Ok(())
}

fn report(label: &str, samples: &mut [std::time::Duration]) {
    samples.sort();
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "{label}: n={} p50={:?} p95={:?} max={:?}",
        samples.len(),
        p(0.50),
        p(0.95),
        samples[samples.len() - 1]
    );
}

// ---------------------------------------------------------------------------
// roundtrip
// ---------------------------------------------------------------------------

fn roundtrip(args: &[String]) -> Result<(), Failure> {
    let source = PathBuf::from(args.first().ok_or("roundtrip: missing <src>")?)
        .canonicalize()
        .map_err(engine_err("canonicalize <src>"))?;
    let work = PathBuf::from(args.get(1).ok_or("roundtrip: missing <work>")?);
    let store_dir = work.join("store");
    let restore_dir = work.join("restore");
    fs::create_dir_all(&store_dir)?;
    fs::create_dir_all(&restore_dir)?;
    if fs::read_dir(&restore_dir)?.next().is_some() {
        return Err("roundtrip: <work>/restore must be empty".into());
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let (volume_id, generation) = runtime.block_on(capture_and_commit(&source, &store_dir))?;
    runtime.block_on(materialize(&store_dir, volume_id, generation, &restore_dir))?;

    let started = Instant::now();
    let mismatches = compare_trees(&source, &restore_dir)?;
    println!("compare: {:?}", started.elapsed());

    if mismatches.is_empty() {
        println!("ROUNDTRIP OK: content + mode identical");
        Ok(())
    } else {
        for m in &mismatches {
            eprintln!("MISMATCH: {m}");
        }
        Err(format!("{} mismatches", mismatches.len()).into())
    }
}

fn volume_config() -> VolumeConfig {
    // QUAL_PROFILE=portable switches the profile for differential debugging.
    let profile = match std::env::var("QUAL_PROFILE").as_deref() {
        Ok("portable") => FilesystemProfile::Portable,
        _ => FilesystemProfile::Posix,
    };
    let mut config = VolumeConfig {
        profile,
        ..VolumeConfig::portable(Lifecycle::Durable)
    };
    // A baseline capture is one atomic authored transaction covering every
    // path in the repo; the default 2,048-mutation batch cap rejects any
    // real tree. Sized for large monorepos.
    config.limits.maximum_mutations_per_batch = 4_194_304;
    config.limits.maximum_paths_per_batch = 4_194_304;
    config
}

async fn capture_and_commit(
    source: &Path,
    store_dir: &Path,
) -> Result<(VolumeId, GenerationId), Failure> {
    let cancel = CancellationToken::new();
    let fs_engine = LocalFs::local(LocalOptions::new(store_dir)).map_err(engine_err("open store"))?;

    let started = Instant::now();
    let volume = fs_engine
        .create_volume(volume_config(), WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("create volume"))?
        .value;
    let mut checkout = volume
        .checkout(
            GenerationSelector::Head,
            CheckoutMode {
                access: AccessMode::ReadWrite,
                consistency: ConsistencyMode::TrackingSafe,
                mutations: MutationMode::PrivateOverlay,
            },
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(engine_err("checkout head"))?
        .value;
    println!("volume + checkout: {:?}", started.elapsed());

    let options = CaptureOptions {
        source_root: source.to_path_buf(),
        expected_root_identity: capture_root_identity(source).map_err(engine_err("root identity"))?,
        maximum_paths: 4_000_000,
        maximum_extent_spans: 65_536,
    };
    let started = Instant::now();
    let receipt = capture_baseline(&mut checkout, &options, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("capture_baseline"))?
        .value;
    println!(
        "capture_baseline: {:?} (examined {} paths, {} changed, {} bytes staged)",
        started.elapsed(),
        receipt.examined_paths,
        receipt.changed_paths,
        receipt.staged_file_bytes
    );

    let started = Instant::now();
    if std::env::var_os("QUAL_CHECKPOINT_ONLY").is_some() {
        let generation = checkout
            .checkpoint(WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(engine_err("checkpoint"))?
            .value;
        println!("checkpoint (no publish): {:?}", started.elapsed());
        return Ok((checkout.volume_id(), generation));
    }
    let outcome = checkout
        .commit(OperationId::new(), WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("commit"))?
        .value;
    println!("commit: {:?}", started.elapsed());
    match outcome {
        CheckoutCommitOutcome::Committed { generation_id, .. }
        | CheckoutCommitOutcome::AlreadyCommitted { generation_id, .. } => {
            Ok((checkout.volume_id(), generation_id))
        }
        other => Err(format!("commit not durable: {other:?}").into()),
    }
}

async fn materialize(
    store_dir: &Path,
    volume_id: VolumeId,
    generation: GenerationId,
    destination: &Path,
) -> Result<(), Failure> {
    let cancel = CancellationToken::new();
    let fs_engine = LocalFs::local(LocalOptions::new(store_dir)).map_err(engine_err("reopen store"))?;
    let volume = fs_engine
        .open_volume(volume_id, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(engine_err("reopen volume"))?
        .value;
    let mut checkout = volume
        .checkout(
            GenerationSelector::Exact(generation),
            CheckoutMode {
                access: AccessMode::ReadOnly,
                consistency: ConsistencyMode::Pinned,
                mutations: MutationMode::None,
            },
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(engine_err("checkout exact"))?
        .value;

    let started = Instant::now();
    let receipt = materialize_checkout(
        &mut checkout,
        &MaterializeOptions {
            destination: destination.to_path_buf(),
            maximum_directory_entries: 1_024,
            maximum_extent_spans: 65_536,
            transfer_bytes: 8 * 1024 * 1024,
        },
        WorkCounters::UNBOUNDED,
        &cancel,
    )
    .await
    .map_err(engine_err("materialize_checkout"))?
    .value;
    println!(
        "materialize: {:?} ({} files, {} dirs, {} symlinks, {} bytes written)",
        started.elapsed(),
        receipt.files,
        receipt.directories,
        receipt.symbolic_links,
        receipt.written_bytes
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// tree comparison: content + kind + symlink target + mode bits (no mtime/uid)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

fn walk(root: &Path) -> Result<BTreeMap<PathBuf, EntryKind>, Failure> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            let relative = path.strip_prefix(root)?.to_path_buf();
            let kind = if meta.file_type().is_symlink() {
                EntryKind::Symlink
            } else if meta.is_dir() {
                stack.push(path.clone());
                EntryKind::Dir
            } else if meta.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            entries.insert(relative, kind);
        }
    }
    Ok(entries)
}

fn compare_trees(source: &Path, restored: &Path) -> Result<Vec<String>, Failure> {
    let left = walk(source)?;
    let right = walk(restored)?;
    let mut mismatches = Vec::new();

    for (path, kind) in &left {
        match right.get(path) {
            None => mismatches.push(format!("{}: missing in restore", path.display())),
            Some(other) if other != kind => mismatches.push(format!(
                "{}: kind {kind:?} vs {other:?}",
                path.display()
            )),
            Some(_) => {
                let a = source.join(path);
                let b = restored.join(path);
                match kind {
                    EntryKind::Symlink => {
                        let ta = fs::read_link(&a)?;
                        let tb = fs::read_link(&b)?;
                        if ta != tb {
                            mismatches.push(format!(
                                "{}: symlink target {:?} vs {:?}",
                                path.display(),
                                ta,
                                tb
                            ));
                        }
                    }
                    EntryKind::File => {
                        if !files_equal(&a, &b)? {
                            mismatches.push(format!("{}: content differs", path.display()));
                        }
                        if let Some(m) = mode_mismatch(&a, &b, path)? {
                            mismatches.push(m);
                        }
                    }
                    EntryKind::Dir => {
                        if let Some(m) = mode_mismatch(&a, &b, path)? {
                            mismatches.push(m);
                        }
                    }
                    EntryKind::Other => {}
                }
            }
        }
    }
    for path in right.keys() {
        if !left.contains_key(path) {
            mismatches.push(format!("{}: extra in restore", path.display()));
        }
    }
    Ok(mismatches)
}

fn files_equal(a: &Path, b: &Path) -> Result<bool, Failure> {
    let (ma, mb) = (fs::metadata(a)?, fs::metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    let (mut fa, mut fb) = (fs::File::open(a)?, fs::File::open(b)?);
    let mut ba = vec![0u8; 1024 * 1024];
    let mut bb = vec![0u8; 1024 * 1024];
    loop {
        let na = fa.read(&mut ba)?;
        let nb = fb.read(&mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

#[cfg(unix)]
fn mode_mismatch(a: &Path, b: &Path, relative: &Path) -> Result<Option<String>, Failure> {
    use std::os::unix::fs::MetadataExt;
    let ma = fs::metadata(a)?.mode() & 0o7777;
    let mb = fs::metadata(b)?.mode() & 0o7777;
    if ma == mb {
        Ok(None)
    } else {
        Ok(Some(format!(
            "{}: mode {ma:o} vs {mb:o}",
            relative.display()
        )))
    }
}

#[cfg(not(unix))]
fn mode_mismatch(_a: &Path, _b: &Path, _relative: &Path) -> Result<Option<String>, Failure> {
    Ok(None)
}
