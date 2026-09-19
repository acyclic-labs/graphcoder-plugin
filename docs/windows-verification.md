# Windows support: what is verified, and what is not

**Status: verified on real Windows hardware.** Checks 1–8 below were run on
Windows 11 (26200) with the MSVC toolchain, against a release build of this
branch. Mounted forks have platform-specific limits recorded under [Known
limits](#known-limits); everything else behaves as it does on POSIX.

`tests/acceptance/windows-smoke.sh` is the executable form of checks 3–7,
plus the pipe-access assertion below, and runs in CI on `windows-2022` (the `windows` job in `.github/workflows/ci.yml`).
That job also builds and clippies the `cfg(windows)` arms, which the Linux
lint job cannot see.

## What this branch changes

| Area | Change |
| --- | --- |
| `crates/acyclic/src/ipc.rs` | New. The transport split: Unix domain socket vs. Windows named pipe, with an owner-only pipe DACL. |
| `crates/acyclic-engine/src/names.rs` | New. The one definition of how a host name becomes engine bytes. |
| `crates/acyclic/src/client.rs` | `ipc::ClientStream`; daemon spawn no longer leaks stdio handles or stands in the repo. |
| `crates/acyclic/src/server.rs` | `ipc::Listener`/`ipc::ServerStream`; fork route names carry the host encoding. |
| `crates/acyclic-engine/src/store.rs` | Volume profile and component byte budget come from `names`. |
| `crates/acyclic-engine/src/rewind.rs` | Windows directory exchange; journal write fixed; staging is retry-safe. |
| `crates/acyclic-engine/src/guard.rs` | Guarded prefixes and `AppleDouble` matching in the host encoding. |
| `crates/acyclic-engine/src/{diff,exclude}.rs` | Name decoding via `names` instead of per-file UTF-8 fallbacks. |
| `crates/acyclic/src/fork.rs` | Windows forks require Projected File System. |
| `crates/acyclic/src/main.rs` | The client steps out of the tree before asking for a whole-tree swap. |
| `.github/workflows/{ci,release}.yml` | A `windows` CI job; `win32/x64` release matrix entry. |
| `packaging/npm/*.sh`, `deny.toml` | `win32` platform package; MSVC target in the license set. |

Unix behaviour is unchanged by construction: every Windows path sits behind
`#[cfg(windows)]`, and the shared paths route through `names`, which is the
identity transform on Unix.

## Why a named pipe

`AF_UNIX` is not reachable from Rust's `std` or from tokio on Windows, and the
daemon protocol needs a local, per-store, connection-oriented channel. A named
pipe is the direct equivalent. `Paths::socket()` still supplies the name on
every platform; on Windows the path is never created on disk, it only seeds
`\\.\pipe\acyclic-<store key>`.

## Why names are encoded per platform

`FilesystemProfile::Posix` was hardcoded for every host. On Windows the sdk
refuses to hand a `PosixBytes` name back (`native_capture.rs`), so *every*
`checkpoint` failed with `UnrepresentablePath` — the first thing anyone would
have hit. The profile is now `names::profile()`, which means names are
UTF-16LE on Windows.

That is one decision, not two, and it reaches further than capture. The
`ProjFS` provider decodes every entry name it is handed as UTF-16LE, so a
name that arrives in any other encoding is *projected as mojibake rather
than rejected* — fork route ids passed as raw ASCII came back as six garbage
characters. Guarded fork paths have the sharper version of the same problem:
they compare configured prefixes byte-for-byte against mount path components,
and a guard that never matches **fails open**. `crates/acyclic-engine/src/names.rs`
exists so there is exactly one place this can be got wrong.

Because the profile is fixed for the life of a volume, a store created on
Windows cannot be moved to a POSIX host or back. Stores are per-machine and
keyed by repo path, so nothing in the product moves them today.

## Who can reach the daemon

On Unix the socket is protected by where it lives: a per-uid directory this
crate chmods to `0o700`, so no other account can see it. A named pipe has no
parent to hide behind — it sits in a global namespace — and one created with
no security descriptor, which is what `ServerOptions::create` does, gets the
system default. That default was verified on this host to be:

```
D:(A;;FR;;;WD)(A;;FR;;;AN)(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;<user>)
```

`WD` is `Everyone` and `AN` is `ANONYMOUS LOGON`, both with read access, on
the endpoint of a daemon that restores files and rewinds trees on request.
No other local account could *drive* it — `FR` carries no write access, so a
request cannot be sent — but any of them could open the endpoint, which is
enough to read from it and to consume the single idle pipe instance the
listener keeps free.

`ipc::create_pipe_instance` now builds an explicit protected DACL for every
instance, listing only the owning user, `SYSTEM` and `Administrators`:

```
D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;<user>)
```

`tests/acceptance/windows-pipe-acl.ps1` asserts this against the live pipe
and runs as part of the smoke, because a weakened DACL does not break the
transport and so would otherwise regress in silence.

## The checks

### 1. It compiles

```powershell
cargo build --release --locked -p acyclic
```

### 2. Lints and tests

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

111 workspace tests pass. `acyclic-qual` builds and tests on Windows too —
an earlier draft of this document claimed it could not, which was wrong.

### 3. The daemon serves, including through a pipe

```powershell
cd <a scratch git repo>
acyclic init
acyclic checkpoint
acyclic timeline
```

Then the same with the output piped (`acyclic init | cat`). That second form
is the one that regresses: Rust spawns with `bInheritHandles = TRUE`, so the
daemon inherits the caller's stdout and the caller never sees EOF even though
the command finished. `client::detach_stdio` clears the inherit flag across
the spawn. A host hook reading our output is exactly this case.

### 4. Two daemons refuse each other

Start a daemon, then start a second against the same store. The second must
fail. It does — but note **it fails on the store lock** (`LocalStream(AlreadyOpen)`),
before the pipe bind is reached, so `first_pipe_instance(true)` is *not*
what is being demonstrated here and remains unexercised.

### 5. Concurrent clients

Eight parallel `acyclic checkpoint` calls against one daemon: all succeed,
no `ERROR_PIPE_BUSY`. The client's 20×10 ms retry is enough at that width;
it would need to become a real `WaitNamedPipe` before anyone leans harder.

### 6. Shutdown leaves nothing behind

`Stop-Process -Force`, then confirm the pipe is gone and a fresh `acyclic init`
starts a new daemon against the same store. `ipc::cleanup` is a no-op on
Windows because a pipe dies with its process; that holds.

### 7. Rewind, forks, promote

Rewind renames the repo root, which is where Windows is least forgiving —
a directory cannot be renamed while any process holds it open, and a
process's current directory *is* such a handle. Two separate offenders had
to go: the daemon inherited the client's cwd (fixed at spawn), and the
client itself stood in the repo it was asking to rewind (`main::step_aside`).
Neither is a problem on Unix, which renames a directory out from under a
cwd without complaint.

The exchange itself is three renames through a scratch name, since NTFS has
no `RENAME_EXCHANGE`. See [Known limits](#known-limits).

### 8. Packaging

```powershell
bash packaging/npm/platform-package.sh win32 x64 0.0.1 target/release/acyclic.exe build
bash packaging/npm/launcher-package.sh 0.0.1 build
```

`require.resolve` finds `bin/acyclic.exe`; the launcher's `EXE` suffix is
correct.

## Known limits

- **Forks are always copies.** `ProjFS` mounts and
  projects a fork correctly — the tree appears and reads back fine — but
  writes into the projection stop at the `ProjFS` local cache and never reach
  the overlay checkout. A mounted fork therefore looked like it worked while
  `fork-diff` reported no changes and `promote` landed nothing: it silently
  ate the work. `mount_capability()` now reports mounts unavailable on
  Windows, so forks take the copy path, which is verified end to end (write,
  `fork-diff`, `promote` all behave). Revisit if the sdk's `ProjFS` provider
  gains write-back.

- **The tree exchange is not atomic.** `RENAME_EXCHANGE` has no Windows
  equivalent, so `atomic_exchange` does three renames through a scratch
  name. Each rename is atomic; the sequence is not. The property rewind
  needs still holds — the repo path is never a mixture of the two trees,
  only ever absent or naming exactly one whole tree — and `recover` resolves
  every intermediate state from the journal and clears the scratch. But a
  crash mid-swap is recovered rather than impossible, which is weaker than
  the APFS/`renameat2` guarantee.

- **Client call deadlines.** Windows named-pipe clients use Tokio's IOCP
  transport. The call timeout covers the whole request and response, including
  partial writes and replies. A timed-out client must reconnect before another
  call so a late daemon response cannot be paired with the wrong request.

- **A speculative run's timeout kills without a grace period, and a
  descendant can escape the job.** `spec_runner` puts each run in a job
  object rather than the Unix process group, and `TerminateJobObject` ends
  the tree. Two differences from Unix follow. There is no graceful signal to
  a job, so `kill_grace` is not honoured: the tree gets the `SIGKILL` half
  without the `SIGTERM` half. And Windows has no `pre_exec`, so the child
  joins the job just *after* `CreateProcess` returns rather than before it
  runs — a grandchild started inside that window is not in the job and
  survives. The window is small and closing it means hand-rolling
  `CreateProcess` with `CREATE_SUSPENDED`. In exchange, the job's
  `KILL_ON_JOB_CLOSE` limit makes Windows *better* than Unix in one respect:
  the daemon holds the job's only handle, so a force-killed daemon reaps
  every speculative descendant, and no equivalent of the Unix stale-run
  sweep is needed.

- **x64 only.** No `aarch64-pc-windows-msvc` target; Windows on ARM gets the
  launcher's "unsupported platform" message.

- **The POSIX acceptance suite is not run on Windows.** It assumes mount
  tooling, symlinks and modes. `windows-smoke.sh` covers the Windows-specific
  behaviour instead; it is not a replacement for the rest of the suite.

- **`scripts/install.sh` is POSIX-only.** npm is the supported Windows
  install path.

- **A closed stdout pipe panics rather than exiting quietly.** `acyclic
  timeline | head -2` ends in a Rust "failed printing to stdout" panic
  instead of the silent `SIGPIPE` exit a POSIX host gives. The exit status is
  still 0 and no state is affected.

## An observation worth a follow-up

`acyclic init` returning does not guarantee that the baseline predates a
write that lands immediately afterwards. A file edited microseconds after
`init` came back had its change folded into checkpoint #1 rather than
appearing as a later one, so the diff between #1 and #2 reported the change
as metadata-only (`m`, not `M`) and the earlier content was never recoverable.
A few seconds' gap captures normally, which is why `windows-smoke.sh` settles
before editing.

Nothing here suggests this is Windows-specific — it looks like a race between
the baseline capture and the caller in the shared pipeline — but it means the
snapshot a user believes `init` took is not always the tree as it stood at
that moment. It deserves its own investigation on all platforms.
