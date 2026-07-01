# Oxiwake

> Oxiwake keeps your machine awake where the OS allows it — and tells you when it cannot.

Oxiwake (`ow`) is a cross-platform "keep awake" CLI/daemon. It holds a wake lock
open for as long as you ask, and reports **honestly** when the OS will not let
it. Think `caffeinate` / `systemd-inhibit` / PowerToys Awake — but
cross-platform, with a daemon you can toggle on and off and a `doctor` that
tells you exactly what is and isn't guaranteed.

```bash
ow            # print state (off / on + which backend)
ow on         # start the keep-awake daemon
ow off        # stop it
ow toggle     # flip it
ow status     # print current lock state
ow doctor     # probe the environment and every backend, with honest caveats
```

## Think in backends, not distros

Oxiwake does **not** hard-code distro names. It detects capabilities and picks
the strongest mechanism available. See [`docs/setup.md`](docs/setup.md) for the
full backend & design rationale.

**Linux** (priority order):

1. `systemd-logind` D-Bus inhibitor — main backend
2. XDG Desktop Portal inhibit
3. GNOME SessionManager
4. KDE PowerDevil / Solid
5. `org.freedesktop.ScreenSaver` (idle-only)
6. X11 XScreenSaver / DPMS *(feature `linux-x11`)*
7. Wayland idle-inhibit *(feature `linux-wayland`)*

**Windows**:

1. `PowerCreateRequest` + `PowerSetRequest`
2. `SetThreadExecutionState` fallback

## Why a daemon?

`ow on` **cannot** just acquire a lock and exit. On Linux, systemd-logind
returns a file descriptor and the inhibitor is released the moment that FD
closes; on Windows, `PowerSetRequest` is held on a handle that should be cleared
when done. So `ow on` spawns a small detached daemon that keeps the FD/handle
alive; `ow off` tells that daemon to stop, which drops the guard and releases
the lock. The lock's lifetime is tied to the guard's lifetime (RAII) — this is
the core, leak-safe design.

State lives in:

```
Linux:   $XDG_RUNTIME_DIR/oxiwake/{state.json, oxiwake.sock, pending.json}
Windows: %LOCALAPPDATA%\Freeoxide\Oxiwake\...
```

## Usage

```
Usage: ow [OPTIONS] [COMMAND]

Commands:
  on       Start the keep-awake daemon (idempotent: a no-op if already running)
  off      Stop the keep-awake daemon (idempotent: a no-op if not running)
  toggle   Start the daemon if off, stop it if on
  status   Print the current lock state
  doctor   Probe the environment and every compiled-in backend, and print a report
  help     Print this message or the help of the given subcommand(s)

Options:
  -j, --json        Emit machine-readable JSON (status, doctor)
  -v, --verbose...  Increase verbosity (repeat for more detail)
  -h, --help        Print help
  -V, --version     Print version
```

`on` / `toggle` flags: `--display` (also keep the display on), `--aggressive-lid`
(add `handle-lid-switch`; usually needs privilege), `--reason <text>`.

### `ow doctor` is the honest part

`doctor` prints each backend's `supported` (compiled in) and `available`
(reachable right now) status, plus **guarantees** — the things that backend
*cannot* promise. For example:

- systemd-logind: `block` on `shutdown`/`sleep`/`handle-*` typically needs a
  PolicyKit privilege; `idle` is the one routinely allowed unprivileged.
- Windows: on Modern Standby / battery, system/execution requests can be
  terminated ~5 minutes after the sleep timeout, and user-initiated sleep
  (power button, lid, Sleep menu) clears requests.

If `ow on` cannot take the lock (e.g. the OS refuses), it reports the **real**
reason rather than hanging.

## Building

```bash
cargo build --release          # the binary is target/release/ow
cargo test                     # unit + the in-process daemon-lifecycle test
cargo clippy --all-targets     # zero warnings
```

Default Linux build includes all D-Bus backends. X11/Wayland are opt-in:

```bash
cargo build --features linux-x11
cargo build --features linux-wayland
```

### Cross-checking Windows from Linux

The Windows backend + named-pipe transport can be type-checked without a
Windows toolchain:

```bash
rustup target add x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-gnu
```

(Final linking needs a mingw-w64 toolchain; `cargo check` exercises the full
windows-sys type-check without it.)

## Project layout

```
src/
  model.rs        WakeTarget / WakeMode / WakeRequest / WakeStatus / DoctorReport
                  + the WakeBackend / WakeGuard traits (the shared contract)
  error.rs        OxiwakeError / Result
  backend/        one module per backend (logind, portal, gnome, kde,
                  screensaver, x11, wayland, windows) + mod.rs registry
  platform/       IPC transport: Unix socket (linux) / named pipe (windows)
  ipc.rs          the ClientMsg/DaemonReply protocol (newline-delimited JSON)
  daemon.rs       daemon lifecycle: acquire -> publish state -> serve -> release
  paths.rs        runtime dir + state/socket/pending path resolution
  state.rs        atomic LockState read/write
  doctor.rs       environment + per-backend probing
  cli.rs/main.rs  the `ow` CLI surface + dispatch
  output.rs       human + JSON formatting
tests/
  model.rs            model-helper unit tests
  daemon_lifecycle.rs in-process RAII lifecycle test (acquire -> serve -> release)
docs/setup.md         the backend & setup design
```

## License

MIT.
