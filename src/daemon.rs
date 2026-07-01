//! The keep-awake daemon: lifecycle, IPC dispatch, and lock ownership.
//!
//! `ow on` cannot simply take a lock and exit — on Linux systemd-logind
//! releases the inhibitor the instant its file descriptor is closed, and on
//! Windows `PowerSetRequest` is cleared on `PowerClearRequest` + handle close.
//! So the CLI spawns a small background daemon whose sole job is to keep that
//! OS resource alive for as long as the user wants the lock held. This module
//! is that daemon, plus the CLI-side helpers that start, stop, and talk to it.
//!
//! # Lock-ownership invariant
//!
//! The daemon's [`WakeGuard`](crate::model::WakeGuard) is held by
//! [`run_daemon`] for the **entire** duration of
//! [`platform::bind_and_serve`](crate::platform::bind_and_serve). The guard is
//! dropped only when `run_daemon` returns — on a clean `Stop`, on a fatal
//! serve error, or on process exit. Its `Drop` impl closes the logind FD /
//! clears the power request, so the OS lock is released exactly when the
//! daemon stops, never sooner, and never leaked.
//!
//! # The pending.json hand-off
//!
//! Because the daemon is a freshly-spawned child, it cannot inherit the
//! caller's in-memory `WakeRequest` or the timestamp the caller chose. Instead
//! the CLI writes a small `pending.json` (`{"request": …, "started_unix": …}`)
//! before spawning, and [`daemon_main`] reads it back. `started_unix` is taken
//! from the caller (which may pass `SystemTime::now`) rather than read inside
//! the library, so daemon logic stays deterministic and testable.

use std::io::{BufReader, BufWriter, Write};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::backend;
use crate::error::{OxiwakeError, Result};
use crate::ipc::{read_msg, write_msg, ClientMsg, DaemonReply};
use crate::model::{WakeBackend, WakeRequest, WakeStatus};
use crate::paths::Paths;
use crate::platform;
use crate::state::LockState;

/// How long [`ensure_started`] polls for the freshly-spawned daemon to come
/// up before giving up. Two seconds is generous for a fork + one D-Bus call
/// yet short enough that a wedged start does not feel like a hang.
const STARTUP_POLL_TIMEOUT: Duration = Duration::from_secs(2);
/// Interval between [`ensure_started`] polls of the daemon's status.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The hidden CLI subcommand the daemon process runs as.
///
/// `main.rs` is expected to dispatch `ow __daemon` to [`daemon_main`]; the
/// double underscore makes it unlikely to collide with a user-facing verb.
pub const DAEMON_SUBCOMMAND: &str = "__daemon";

/// `pending.json` shape: the request the CLI wants the daemon to take, plus
/// the timestamp the lock should claim it began.
///
/// Serialized as `{"request": <WakeRequest>, "started_unix": <u64>}` per the
/// cross-module contract.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingRequest {
    request: WakeRequest,
    started_unix: u64,
}

// ---------------------------------------------------------------------------
// Daemon entry points
// ---------------------------------------------------------------------------

/// The `ow __daemon` entry point.
///
/// Resolves the runtime paths, reads `pending.json` to recover the request
/// and timestamp the CLI chose, and hands them to [`run_daemon`]. The pending
/// file is consumed by [`run_daemon`] itself (it removes it once the lock is
/// live), so on success it does not linger.
///
/// # Errors
///
/// [`OxiwakeError::State`] if the runtime directory cannot be resolved or
/// `pending.json` is missing/unparseable; otherwise whatever [`run_daemon`]
/// returns.
pub fn daemon_main() -> Result<()> {
    // Wrap the real work so that, on any startup failure, we publish the error
    // to a `startup_error` file the spawning CLI can read. The detached daemon's
    // stderr is /dev/null, so without this the parent only ever sees a generic
    // timeout; this is what makes `ow on` report the *real* reason (e.g. a
    // PolicyKit denial of the logind inhibitor) instead of "did not come up".
    let result = daemon_main_inner();
    if let Err(ref e) = result {
        if let Ok(paths) = Paths::resolve() {
            let _ = write_startup_error(&paths, &e.to_string());
        }
    }
    result
}

fn daemon_main_inner() -> Result<()> {
    let paths = Paths::resolve()?;
    // Clear any startup error left by a previous, failed attempt so the parent
    // does not read a stale reason on this run.
    let _ = clear_startup_error(&paths);
    let pending = read_pending(&paths)?
        .ok_or_else(|| OxiwakeError::State("no pending request found for daemon".to_string()))?;
    let backend = backend::pick(&pending.request)?;
    run_daemon(backend, &pending.request, pending.started_unix)?;
    Ok(())
}

/// Run the daemon loop: take the lock, publish state, serve until `Stop`.
///
/// This is the heart of the daemon and the keeper of the lock-ownership
/// invariant. The guard returned by [`backend::pick`] lives in the closure
/// passed to [`platform::bind_and_serve`](crate::platform::bind_and_serve) and
/// is dropped only when this function returns.
///
/// Steps:
///
/// 1. Pick a backend for `req` and acquire its guard (taking the OS lock).
/// 2. Snapshot the backend's [`WakeStatus`] once — the lock's identity does
///    not change over the daemon's lifetime, so a single `status()` call is
///    both correct and cheap.
/// 3. Write `state.json` so `ow status` works without a live IPC, and remove
///    `pending.json` (the hand-off file has done its job).
/// 4. Serve: [`platform::bind_and_serve`](crate::platform::bind_and_serve) with
///    a closure that owns the guard and status and answers `Ping`, `Status`,
///    and `Stop`.
/// 5. On return (a `Stop` was dispatched, or the serve loop failed), remove
///    `state.json` so a subsequent `ow status` does not report a stale lock —
///    then the guard drops, releasing the OS lock.
pub fn run_daemon(
    backend: Box<dyn WakeBackend>,
    req: &WakeRequest,
    started_unix: u64,
) -> Result<()> {
    let paths = Paths::resolve()?;

    // 0. Defense in depth against a double lock: if a daemon is *already*
    //    serving on this socket, do not take a second inhibitor. A second lock
    //    is pointless, and combined with a bind race it could orphan the first
    //    daemon's lock (it would keep holding an inhibitor on an unreachable
    //    socket). `ensure_started` should have caught this on the client side;
    //    we re-check here so a racing double `ow on` can never wedge things.
    if let Ok(reply) = ipc_request(ClientMsg::Ping) {
        if reply.ok {
            // Another daemon is live. Decline to serve; the guard we were
            // given is dropped on return (releasing any transient resource).
            return Ok(());
        }
    }

    // 1. Take the lock. `acquire` returns the RAII guard that owns the OS
    //    resource; keep it alive for the whole serve loop. (The backend is
    //    injected rather than picked here so the serve/release lifecycle is
    //    unit-testable with a mock backend — the real entry point,
    //    [`daemon_main_inner`], does the `backend::pick`.)
    let guard = backend.acquire(req)?;

    // 2. Snapshot the lock's identity once. Cheap and stable.
    let status = backend.status()?;

    // 3. Publish state for status-without-IPC, then consume the hand-off file.
    let state = LockState {
        pid: std::process::id(),
        backend: guard.backend().to_string(),
        started_unix,
        request: req.clone(),
    };
    LockState::write(&paths, &state)?;
    // pending.json has served its purpose; its absence also signals to a
    // retrying `ensure_started` that the daemon came up.
    let _ = LockState::remove_pending(&paths);

    // 4. Serve. The guard is moved into the closure so it lives exactly as
    //    long as the serve loop; the captured `status` is cheaply cloned per
    //    reply. The closure returns a `DaemonReply` for each message and the
    //    platform layer handles framing and the Stop-triggered exit.
    let pid = std::process::id();
    let serve_result = platform::bind_and_serve(&paths, move |msg: ClientMsg| -> DaemonReply {
        match msg {
            ClientMsg::Ping => DaemonReply::ok_ping(status.clone(), pid),
            ClientMsg::Status => DaemonReply::ok_status(status.clone()),
            ClientMsg::Stop => DaemonReply::ok_empty(),
        }
    });

    // 5. Tear down: clear state so a later `ow status` does not see a stale
    //    lock. Best-effort — a failure here must not mask the real error from
    //    the serve loop. The guard drops at the end of this function, after
    //    state cleanup, releasing the OS resource last.
    let _ = LockState::remove(&paths);

    serve_result?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CLI-side helpers
// ---------------------------------------------------------------------------

/// `ow on`: ensure a daemon is running and holding `req`.
///
/// If a daemon is already up and answers a `Ping`, return its status
/// unchanged (idempotent `ow on`). Otherwise:
///
/// 1. Write `pending.json` carrying `req` and the caller-supplied
///    `started_unix` (the caller — `main.rs` — passes `now().as_secs()` so the
///    timestamp is anchored outside the library).
/// 2. Spawn a detached `ow __daemon` child whose std streams are sent to
///    `Null`. On Linux the child detaches into its own session via a
///    `pre_exec` `setsid`, so it survives the CLI's exit.
/// 3. Poll for up to [`STARTUP_POLL_TIMEOUT`]: once `state.json` appears *and*
///    a `Ping` succeeds, return the daemon's reported status. On timeout,
///    return a clear error.
///
/// `started_unix` is a parameter (not read from the clock here) so this
/// function is deterministic and testable.
pub fn ensure_started(req: &WakeRequest, started_unix: u64) -> Result<WakeStatus> {
    let paths = Paths::resolve()?;

    // Fast path: already running. A live state.json + a successful Ping means
    // a daemon is up; reuse it instead of erroring with AlreadyRunning.
    if LockState::read(&paths)?.is_some() {
        if let Ok(reply) = ipc_request(ClientMsg::Ping) {
            if let Some(status) = reply.status {
                return Ok(status);
            }
        }
    }

    // Write the hand-off file before spawning so the child definitely sees it.
    // Also clear any leftover startup_error from a previous, failed attempt so
    // this run does not get poisoned by a stale reason before the new child
    // has a chance to clear it itself.
    let _ = clear_startup_error(&paths);
    write_pending(&paths, req, started_unix)?;

    // Spawn the detached daemon. Failures here are fatal — without a daemon
    // there is no way to hold the lock.
    spawn_detached_daemon()?;

    // Poll until the daemon publishes state and answers pings, or we time out.
    let deadline = Instant::now() + STARTUP_POLL_TIMEOUT;
    loop {
        if LockState::read(&paths)?.is_some() {
            if let Ok(reply) = ipc_request(ClientMsg::Ping) {
                if reply.ok {
                    if let Some(status) = reply.status {
                        // Up and healthy: drop any stale startup error.
                        let _ = clear_startup_error(&paths);
                        return Ok(status);
                    }
                }
            }
        }

        // Fast-fail: if the daemon already reported why it could not start
        // (e.g. the OS refused the inhibitor), surface that real reason now
        // instead of waiting out the full poll timeout.
        if let Some(reason) = read_and_clear_startup_error(&paths) {
            let _ = LockState::remove_pending(&paths);
            return Err(OxiwakeError::Other(format!(
                "oxiwake daemon failed to start: {reason}"
            )));
        }

        if Instant::now() >= deadline {
            // Clean up the now-stale hand-off file so the next attempt starts
            // clean rather than reusing a request the daemon never consumed.
            let _ = LockState::remove_pending(&paths);
            return Err(OxiwakeError::Other(format!(
                "spawned oxiwake daemon but it did not come up within {:?} \
                 (check `ow doctor` and that a backend can acquire the lock)",
                STARTUP_POLL_TIMEOUT
            )));
        }

        std::thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

/// `ow off`: tell the running daemon to release its lock and exit.
///
/// Sends [`ClientMsg::Stop`]. A [`OxiwakeError::NotRunning`] result is treated
/// as success — "turn it off" is a no-op when it is already off, not an
/// error. Any leftover `state.json` (e.g. from a daemon that crashed without
/// cleaning up) is removed best-effort so `ow status` does not lie.
pub fn ensure_stopped() -> Result<()> {
    match ipc_request(ClientMsg::Stop) {
        Ok(_) => {}
        Err(OxiwakeError::NotRunning) => {
            // Already off — not an error. Fall through to best-effort cleanup.
        }
        Err(e) => return Err(e),
    }

    // Best-effort: clear any stale state file. A failure here is not fatal
    // (the daemon, if it was running, already removed it on its way out).
    if let Ok(paths) = Paths::resolve() {
        let _ = LockState::remove(&paths);
    }
    Ok(())
}

/// Send a single IPC message to the running daemon and read its reply.
///
/// Resolves the runtime paths, connects (returning [`OxiwakeError::NotRunning`]
/// when no daemon is listening), writes one framed message, flushes, and reads
/// one framed reply. The connection is then dropped.
pub fn ipc_request(msg: ClientMsg) -> Result<DaemonReply> {
    let paths = Paths::resolve()?;
    let stream = platform::connect(&paths)?;
    // `try_clone` yields an independent dup of the same connection so we can
    // hand the read half to a BufReader and the write half to a BufWriter
    // without borrowing the trait object.
    let read_half = stream
        .try_clone()
        .map_err(|e| OxiwakeError::Other(format!("could not dup IPC stream: {e}")))?;
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(read_half);

    write_msg(&mut writer, &msg)?;
    writer.flush()?;

    let reply: DaemonReply = read_msg(&mut reader)?;
    Ok(reply)
}

// ---------------------------------------------------------------------------
// pending.json + process spawn helpers
// ---------------------------------------------------------------------------

/// Write the request hand-off file for the about-to-be-spawned daemon.
///
/// Serialized as `{"request": <WakeRequest>, "started_unix": <u64>}`. Written
/// atomically via [`LockState`]'s temp-file-and-rename helper to avoid the
/// child ever reading a half-written file.
fn write_pending(paths: &Paths, req: &WakeRequest, started_unix: u64) -> Result<()> {
    let pending = PendingRequest {
        request: req.clone(),
        started_unix,
    };
    let bytes = serde_json::to_vec_pretty(&pending)
        .map_err(|e| OxiwakeError::State(format!("could not serialize pending request: {e}")))?;
    atomic_write(&paths.pending, &bytes)
}

/// Read the request hand-off file. `Ok(None)` if it is absent.
fn read_pending(paths: &Paths) -> Result<Option<PendingRequest>> {
    match std::fs::read(&paths.pending) {
        Ok(bytes) => {
            let value = serde_json::from_slice::<PendingRequest>(&bytes).map_err(|e| {
                OxiwakeError::State(format!("could not parse {}: {e}", paths.pending.display()))
            })?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OxiwakeError::from(e)),
    }
}

/// Spawn `ow __daemon` as a detached background process.
///
/// All standard streams are wired to `Null` so the daemon never holds the
/// CLI's terminal hostage. On Linux a `pre_exec` hook calls `setsid` so the
/// child escapes the CLI's process group / controlling terminal and survives
/// the CLI's exit (`setsid` is what fully detaches it from signals like SIGHUP).
/// On Windows the child is created `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
/// | CREATE_NO_WINDOW` so it owns no console and survives the CLI's console
/// closing (a plain `spawn` would otherwise be killed with the console group).
fn spawn_detached_daemon() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| OxiwakeError::Other(format!("could not resolve current exe: {e}")))?;

    let mut cmd = Command::new(exe);
    cmd.arg(DAEMON_SUBCOMMAND);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // Detach into a new session so the daemon outlives the CLI and is not
        // killed when the CLI's controlling terminal closes. If `setsid` fails
        // the closure returns the `io::Error`, which aborts the exec attempt and
        // surfaces as an error from `Command::spawn` (a failed `pre_exec` hook
        // makes `spawn` return the error rather than silently running undetached).
        unsafe {
            cmd.pre_exec(|| libc_setsid());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Without these flags the child inherits the CLI's console process
        // group; when that console closes (or the session ends) Windows sends
        // CTRL_CLOSE/CTRL_LOGOFF to the whole group and kills the daemon,
        // releasing the power-request handle even though the user expected the
        // lock to persist until `ow off`. The flags fully detach it:
        //   DETACHED_PROCESS        — no console at all (no group events).
        //   CREATE_NEW_PROCESS_GROUP — isolated from the CLI's CTRL+C/BREAK.
        //   CREATE_NO_WINDOW        — no console window flashes on screen.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    cmd.spawn()
        .map_err(|e| OxiwakeError::Other(format!("could not spawn oxiwake daemon: {e}")))?;
    Ok(())
}

/// `setsid(2)` FFI, kept behind a thin wrapper so the unsafe is isolated.
///
/// On Linux this calls `setsid`; on other platforms it is a no-op so the
/// `pre_exec` closure compiles. (The closure itself is only installed under
/// `cfg(target_os = "linux")`, but this helper is referenced there.)
#[cfg(target_os = "linux")]
unsafe fn libc_setsid() -> std::io::Result<()> {
    // Call setsid via a raw syscall through libc would require a libc dep;
    // instead use the unstable-but-stable-in-practice `nix`-free path: emit
    // the syscall directly. To avoid pulling in `libc`/`nix`, we link the C
    // library symbol `setsid`, which glibc/musl/bionic all export.
    extern "C" {
        fn setsid() -> i32;
    }
    let rc = setsid();
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Atomically write `bytes` to `dest` via a sibling temp file + rename.
///
/// Mirrors [`LockState::write`]'s approach: the temp file is cleaned up on any
/// failure, and the final `rename` is atomic so a reader never sees a torn
/// file. Centralized here so both `state.json` and `pending.json` share the
/// same discipline.
fn atomic_write(dest: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp: std::path::PathBuf = tmp.into();

    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(OxiwakeError::from(e));
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(OxiwakeError::from(e));
    }
    Ok(())
}

impl LockState {
    /// Remove the `pending.json` hand-off file. `Ok(())` if it was absent.
    ///
    /// Lives next to [`LockState::remove`] for discoverability even though
    /// `pending.json` is not a `LockState`; it shares the same
    /// ignore-if-absent discipline.
    pub fn remove_pending(paths: &Paths) -> Result<()> {
        remove_pending(paths)
    }
}

/// Free-function core of [`LockState::remove_pending`]; delete `pending.json`
/// if present, `Ok(())` otherwise.
fn remove_pending(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(&paths.pending) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OxiwakeError::from(e)),
    }
}

// ---------------------------------------------------------------------------
// startup-error hand-off (daemon -> spawning CLI)
// ---------------------------------------------------------------------------

/// Path of the one-line file a failing daemon writes so the spawning CLI can
/// report the *real* reason `ow on` did not take the lock. Lives in the runtime
/// dir next to `state.json` / `pending.json`.
fn startup_error_path(paths: &Paths) -> std::path::PathBuf {
    paths.dir.join("startup_error")
}

/// Write the daemon's fatal startup error. Atomic (temp + rename) so the parent
/// never reads a torn message.
fn write_startup_error(paths: &Paths, msg: &str) -> Result<()> {
    atomic_write(&startup_error_path(paths), msg.as_bytes())
}

/// Read and delete the startup-error file. `None` if it is absent or empty.
fn read_and_clear_startup_error(paths: &Paths) -> Option<String> {
    let path = startup_error_path(paths);
    let msg = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    msg.filter(|m| !m.trim().is_empty())
}

/// Delete the startup-error file if present. `Ok(())` if it was already absent.
fn clear_startup_error(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(startup_error_path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OxiwakeError::from(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_roundtrip() {
        // Serialize/deserialize a PendingRequest to lock the on-wire shape.
        let p = PendingRequest {
            request: WakeRequest::default_linux(),
            started_unix: 1_700_000_000,
        };
        let v = serde_json::to_vec(&p).unwrap();
        let back: PendingRequest = serde_json::from_slice(&v).unwrap();
        assert_eq!(back.started_unix, 1_700_000_000);
        assert_eq!(back.request.targets, p.request.targets);
        // The shape must carry both keys.
        let as_value: serde_json::Value = serde_json::from_slice(&v).unwrap();
        assert!(as_value.get("request").is_some());
        assert!(as_value.get("started_unix").is_some());
    }

    #[test]
    fn daemon_subcommand_is_hidden() {
        // A cheap regression guard: the subcommand must start with "__" so it
        // never collides with a user verb and is clearly internal.
        assert!(DAEMON_SUBCOMMAND.starts_with("__"));
    }
}
