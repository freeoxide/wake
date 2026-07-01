//! Linux IPC transport: Unix domain sockets.
//!
//! The daemon listens on a Unix domain socket at
//! `$XDG_RUNTIME_DIR/oxiwake/oxiwake.sock`; clients connect to the same path.
//! Because the runtime directory is created mode `0700` by
//! [`crate::paths::Paths`], the socket is reachable only by the owning user,
//! which is exactly the threat model we want for a per-user wake-lock daemon.
//!
//! [`connect`] maps a missing socket or a refused connection to
//! [`OxiwakeError::NotRunning`] so the CLI can say "oxiwake is not running"
//! rather than emitting a raw `ENOENT`.
//!
//! [`bind_and_serve`] removes any stale socket left by a crashed daemon,
//! binds a fresh [`UnixListener`] (mode `0600`), and runs a single-threaded
//! accept loop. Each accepted connection is handled inline: read one
//! [`ClientMsg`](crate::ipc::ClientMsg), call the closure, write the
//! [`DaemonReply`](crate::ipc::DaemonReply), flush, and drop the connection.
//! After dispatching a [`Stop`](crate::ipc::ClientMsg::Stop) the loop breaks
//! and the function returns `Ok(())` so the daemon can tear down.

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use crate::error::{OxiwakeError, Result};
use crate::ipc::{write_msg, ClientMsg, DaemonReply};
use crate::paths::Paths;
use crate::platform::Stream;

/// Per-connection I/O timeout on the *server* side. Bounds how long the
/// single-threaded accept loop will wait on any one client's read/write, so a
/// silent or stalling client is dropped instead of wedging the daemon (which
/// would otherwise block all later `ow off` / `ow status` / `ow toggle`).
/// Deliberately shorter than the *client*'s 5s timeout so the server recovers
/// before a waiting client gives up.
const SERVER_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Concrete impl: a [`UnixStream`] is a [`Stream`].
///
/// `UnixStream` already implements both [`Read`] and [`Write`], so this just
/// closes the trait. It is intentionally a concrete (non-blanket) impl so the
/// compiler can prove `UnixStream: Stream` without a blanket that would also
/// cover unrelated `Read + Write` types.
impl Stream for UnixStream {
    fn try_clone(&self) -> std::io::Result<Box<dyn Stream>> {
        // `UnixStream::try_clone` dups the FD, yielding a fully independent
        // handle to the same socket; closing one half does not close the other.
        Ok(Box::new(UnixStream::try_clone(self)?))
    }
}

/// Client side: connect to a running daemon's Unix socket.
///
/// On any error that indicates "there is nothing listening" — the socket file
/// does not exist, or the connect was refused (daemon exited but left the
/// file, or never started) — this returns [`OxiwakeError::NotRunning`] so the
/// caller can treat "no daemon" uniformly. All other errors (permission
/// denied, I/O failure) propagate as-is.
pub fn connect(paths: &Paths) -> Result<Box<dyn Stream>> {
    match UnixStream::connect(&paths.socket) {
        Ok(stream) => {
            // A short read/write timeout keeps a wedged daemon from hanging
            // the CLI forever; `None` on error means "leave the OS defaults".
            // Best-effort: a failure to set the timeout is not fatal.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
            Ok(Box::new(stream))
        }
        Err(e) => {
            if is_not_running(&e) {
                Err(OxiwakeError::NotRunning)
            } else {
                Err(OxiwakeError::from(e))
            }
        }
    }
}

/// Bind the daemon socket and serve requests until told to stop.
///
/// Steps:
///
/// 1. Remove a stale socket file left by a previous, now-dead daemon
///    (`unlink` is best-effort and ignores "not found").
/// 2. Bind a fresh [`UnixListener`] at `paths.socket` and tighten its
///    permissions to `0600` (best-effort).
/// 3. Loop: accept one connection, read a single [`ClientMsg`], call
///    `on_msg` to produce a [`DaemonReply`], write the reply, flush, and drop
///    the connection. If the message was [`ClientMsg::Stop`], write the reply
///    and then `break` so the function returns `Ok(())`.
///
/// The loop is single-threaded: Oxiwake's protocol is strictly one-message-
/// per-connection, so there is no benefit to concurrency, and staying
/// single-threaded keeps the daemon trivially auditable.
///
/// A client that connects and disconnects without sending a full message, or
/// that sends garbage, is handled gracefully: the connection is dropped and
/// the loop continues to the next accept. Only a failure of the *listener*
/// itself aborts the serve loop.
pub fn bind_and_serve(
    paths: &Paths,
    mut on_msg: impl FnMut(ClientMsg) -> DaemonReply,
) -> Result<()> {
    // 1. Do NOT blindly unlink + bind: if another live daemon is already
    //    listening here, replacing its socket would orphan its lock (it would
    //    keep holding the inhibitor on an unreachable, unlinked socket that no
    //    `ow off` could reach). Probe first:
    //      - connect succeeds  -> a daemon owns it; decline to serve (Ok).
    //      - NotRunning        -> stale/absent socket; safe to (re)create.
    //      - other error       -> propagate.
    match crate::platform::connect(paths) {
        Ok(_) => {
            // Another daemon is live and reachable. Taking over would orphan
            // its lock, so we simply decline. The caller drops its guard.
            return Ok(());
        }
        Err(OxiwakeError::NotRunning) => remove_if_exists(&paths.socket)?,
        Err(e) => return Err(e),
    }

    // 2. Bind. A bind failure (e.g. path is a directory, or permissions) is a
    //    real error and propagates.
    let listener = UnixListener::bind(&paths.socket)?;
    // Tighten the socket to 0600. The parent dir is already 0700, so this is
    // belt-and-braces; best-effort.
    let _ = std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600));

    // 3. Accept loop. `incoming()` yields connections until the listener is
    //    dropped; we break out of it explicitly on Stop.
    let mut stop_requested = false;
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // An error on a single accept (e.g. EMFILE under FD pressure)
            // should not kill a long-running daemon; log-friendly skip.
            Err(_) => continue,
        };

        // Handle this one connection to completion. Errors here are
        // per-connection and must not tear down the serve loop: a failed
        // handler simply reads as "no stop requested" via `unwrap_or_default`.
        stop_requested |= handle_connection(stream, &mut on_msg).unwrap_or_default();

        if stop_requested {
            break;
        }
    }

    // Clean up our own socket file on the way out so a subsequent `connect`
    // sees NotRunning rather than ConnectionRefused against a dead file.
    let _ = std::fs::remove_file(&paths.socket);
    Ok(())
}

/// Read one [`ClientMsg`] from `stream`, dispatch it through `on_msg`, write
/// the [`DaemonReply`], and flush.
///
/// Returns `Ok(true)` if the message was [`ClientMsg::Stop`] (so the caller
/// can break its loop), `Ok(false)` otherwise. Per-connection errors are
/// swallowed (returned as `Ok(false)`) so a misbehaving client cannot crash
/// the serve loop; the connection is simply dropped.
fn handle_connection(
    stream: UnixStream,
    on_msg: &mut impl FnMut(ClientMsg) -> DaemonReply,
) -> Result<bool> {
    // Split the stream into a reader and writer so we can buffer each side.
    let read_half = stream.try_clone()?;
    let write_half = stream;
    // Bound each half so a client that connects and then stalls (sends a
    // partial frame, or nothing at all) cannot wedge the single-threaded
    // accept loop forever — without this, one silent client would block every
    // later `ow off` / `ow status` / `ow toggle`. A TimedOut/WouldBlock from
    // `read_msg` is swallowed below as a non-fatal drop, exactly like a
    // disconnect. Best-effort: a failure to set the timeout is not fatal.
    let _ = read_half.set_read_timeout(Some(SERVER_IO_TIMEOUT));
    let _ = write_half.set_write_timeout(Some(SERVER_IO_TIMEOUT));
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    // Read exactly one framed message. A short read / disconnect surfaces as
    // NotRunning or Ipc from `read_msg`; swallow it as a non-fatal drop.
    let msg: ClientMsg = match crate::ipc::read_msg(&mut reader) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };

    let is_stop = matches!(msg, ClientMsg::Stop);
    let reply = on_msg(msg);

    // Best-effort write + flush. If the client hung up before reading the
    // reply there is nothing useful to do; treat as a non-fatal drop.
    if write_msg(&mut writer, &reply).is_err() {
        return Ok(is_stop);
    }
    let _ = writer.flush();
    Ok(is_stop)
}

/// `true` if a `connect`/`read` error means "nothing is listening".
///
/// Covers `NotFound` (no socket file at all) and `ConnectionRefused` (the file
/// exists but no process is accepting — the classic "daemon died but left the
/// socket" footprint). `ConnectionReset` during a read is deliberately *not*
/// mapped here: that indicates a peer was briefly present.
fn is_not_running(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Delete `path` if it exists; ignore "not found".
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OxiwakeError::from(e)),
    }
}

// ---------------------------------------------------------------------------
// Singleton lock — the atomic mutex that gates daemon startup
// ---------------------------------------------------------------------------

/// `flock(2)` operation flags, linked from the C library (glibc/musl/bionic all
/// export `flock`) to avoid pulling in the `libc` crate — the same FFI approach
/// used for `setsid` in [`crate::daemon`].
const FLOCK_EXCLUSIVE: i32 = 2;
const FLOCK_NONBLOCK: i32 = 4;

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// An exclusive advisory lock on the singleton lock file, held for the daemon's
/// whole lifetime so two racing `ow on` invocations can never both take an OS
/// wake lock. Dropping it closes the underlying fd, which releases the lock —
/// also true if the daemon crashes, since the kernel reaps the fd.
#[allow(dead_code)] // the File is held purely for its Drop (fd close releases flock)
pub struct SingletonLock(std::fs::File);

/// Atomically claim the singleton lock, or report that another daemon holds it.
///
/// Opens (creating if needed) `paths.lock` and takes a non-blocking exclusive
/// `flock`:
/// - `Ok(None)` — another process already holds the lock (`EWOULDBLOCK`); the
///   caller should decline to serve.
/// - `Ok(Some(_))` — this process now owns the lock for its lifetime.
/// - `Err(_)` — any other I/O failure.
pub fn acquire_singleton_lock(paths: &Paths) -> Result<Option<SingletonLock>> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // The lock file's *content* is irrelevant — only the flock on its fd
        // matters — so never truncate it.
        .truncate(false)
        .open(&paths.lock)?;
    // SAFETY: `fd` is a valid open file descriptor; flock is async-signal-safe
    // and thread-safe. LOCK_EX | LOCK_NB fails immediately (rather than
    // blocking) if the lock is held.
    let rc = unsafe { flock(file.as_raw_fd(), FLOCK_EXCLUSIVE | FLOCK_NONBLOCK) };
    if rc == 0 {
        Ok(Some(SingletonLock(file)))
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            // Another daemon holds the singleton lock; decline to serve.
            Ok(None)
        } else {
            Err(OxiwakeError::from(err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn unixstream_is_a_stream() {
        // Compile-time proof that the concrete impl exists and the blanket
        // requirement (Read + Write) is satisfied.
        fn accepts_stream<S: Stream>(_: &S) {}
        let (a, _b) = UnixStream::pair().unwrap();
        accepts_stream(&a);
    }
}
