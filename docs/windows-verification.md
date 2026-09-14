# Windows support: verification blocker

**Status: BLOCKED — unverified. Do not merge this branch, and do not tag a
release including a `win32` artifact, until every check below passes on real
Windows hardware.**

Everything here was written and reviewed on macOS. No step in this branch has
ever been compiled, linked, or run for a Windows target. CI does not cover
Windows by design — this document is the gate instead.

## What this branch changes

| Area | Change |
| --- | --- |
| `crates/acyclic/src/ipc.rs` | New. The whole platform split: Unix domain socket vs. Windows named pipe. |
| `crates/acyclic/src/client.rs` | Uses `ipc::ClientStream` instead of `std::os::unix::net::UnixStream`. |
| `crates/acyclic/src/server.rs` | Uses `ipc::Listener` / `ipc::ServerStream`; `tokio::io::split` replaces `into_split`. |
| `crates/acyclic-engine/src/fork.rs` | `mount_setup_hint()` gained a Windows branch naming ProjFS. |
| `.github/workflows/release.yml` | `win32/x64` matrix entry, `.exe`-aware smoke, attestation and SBOM subjects. |
| `packaging/npm/*.sh` | `win32` platform package with `.exe`; launcher resolves it. |
| `deny.toml` | `x86_64-pc-windows-msvc` added to the license/advisory target set. |

Unix behaviour is unchanged by construction: every Windows path sits behind
`#[cfg(windows)]`. On macOS after these edits, `cargo clippy --workspace
--all-targets` is clean, 108 workspace tests pass, and `journey.sh` passes.

## Why a named pipe at all

`AF_UNIX` is not reachable from Rust's `std` or from tokio on Windows, and the
daemon protocol needs a local, per-store, connection-oriented channel. A named
pipe is the direct equivalent. `Paths::socket()` still supplies the name on
every platform; on Windows the path is never created on disk, it only seeds
`\\.\pipe\acyclic-<store key>`.

## The checks

Run these in order. Stop at the first failure — later steps assume earlier ones.

### 1. It compiles

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release --locked -p acyclic --target x86_64-pc-windows-msvc
```

Expect zero errors. This is the step most likely to fail first, and the most
likely cause is a `cfg` block in `ipc.rs` that names a tokio or std item
slightly wrong — the Windows arms have never been type-checked.

### 2. Lints and tests

```powershell
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test -p acyclic -p acyclic-engine --target x86_64-pc-windows-msvc
```

`acyclic-qual` is excluded deliberately: it uses `std::os::unix` APIs directly
and is not part of the shipped artifact (`release.yml` builds `-p acyclic`).

### 3. The daemon actually serves

The single most important check, because it exercises the new transport:

```powershell
cd <a scratch git repo>
acyclic init
acyclic checkpoint
acyclic timeline
```

`init` must start a daemon and `timeline` must show the checkpoint. If `init`
hangs, the client is connecting but the server never accepts — look at
`Listener::accept`, where the Windows arm hands out the idle pipe instance and
must immediately create its replacement.

### 4. Two daemons refuse each other

On Unix a second daemon dies on `bind`. On Windows that guarantee comes from
`first_pipe_instance(true)`. Start a daemon, then start a second one against
the same store: **the second must fail to bind, not silently serve a second
pipe instance.** A silent success here means two daemons are writing one store.

### 5. Concurrent clients

Run several hooks at once against one daemon (a few `acyclic checkpoint` calls
in parallel is enough). The client retries 20 times at 10 ms on
`ERROR_PIPE_BUSY`; if you see `pipe busy` errors, that budget is too small for
real concurrency and the retry should become a proper `WaitNamedPipe`.

### 6. Shutdown leaves nothing behind

Stop the daemon, confirm it exits cleanly, and confirm a subsequent `acyclic
init` starts a fresh one. `ipc::cleanup` is a no-op on Windows because a pipe
dies with its process — verify that assumption holds after a `kill -9`
equivalent (`Stop-Process -Force`).

### 7. Mounts, or honest degradation

ProjFS is a separate axis from the transport. With ProjFS **disabled**, verify
the product degrades correctly rather than failing:

- `acyclic init` prints the ProjFS hint from `mount_setup_hint()`
- forks fall back to full copies and still promote correctly
- Safe Mode (`dry_run`) refuses to start

Then enable it and re-check:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart
```

Sign out and back in, then confirm `acyclic init` reports mounts available and
a fork mounts rather than copies. **Note:** `projfs.rs` upstream has never been
exercised by this plugin. Treat a working fork mount as a genuine discovery,
not an expectation.

### 8. Packaging

```powershell
bash packaging/npm/platform-package.sh win32 x64 0.0.1 <path to acyclic.exe> build
bash packaging/npm/launcher-package.sh 0.0.1 build
```

Then install the launcher locally and confirm `require.resolve` finds
`bin/acyclic.exe` — the `EXE` suffix in the launcher is the piece most likely
to be wrong.

### 9. Only then, claim it

`README.md` still says "prebuilt binary, macOS + Linux" and `CHANGELOG.md`
says nothing about Windows. Both are deliberately left alone so this branch
makes no unverified promise. Update them as the last step, once checks 1–8
pass.

## Known gaps, decided deliberately

- **No read/write deadlines.** `ClientStream::set_read_timeout` and
  `set_write_timeout` are no-ops on Windows: a pipe opened as a `File` carries
  no per-handle timeout. The pre-tool hook's exact deadline therefore does not
  bound anything on Windows. Closing this needs either overlapped I/O or a
  watchdog thread. **The latency gate's guarantee does not hold on Windows.**
- **x64 only.** No `aarch64-pc-windows-msvc` target. Windows on ARM users get
  the "unsupported platform" message from the launcher.
- **No Windows CI.** Nothing re-checks any of this after a future change.
  Whoever merges this should decide whether a `windows-2022` job is worth it;
  without one, the next refactor of `client.rs` can break Windows silently.
- **The acceptance suite is bash** and has not been assessed for Windows. Git
  Bash may carry parts of it; nothing here claims it does.
- **`scripts/install.sh` is POSIX-only.** npm is the only supported Windows
  install path.

## If you decide not to finish this

Revert the branch rather than merging it half-verified. A `win32` entry in
`release.yml` that produces an unusable binary is worse than no Windows
support, because it publishes an npm platform package users will install.
