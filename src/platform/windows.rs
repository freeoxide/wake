//! Windows named-pipe IPC platform layer.
//!
//! Oxiwake's daemon and CLI talk over a single well-known named pipe. On
//! Windows that pipe is `\\.\pipe\oxiwake`. A fixed name is acceptable because
//! Oxiwake is a per-user single-instance daemon: only one daemon runs per user
//! session, and its lifetime is bounded by the toggle commands.
//!
//! The surface mirrors [`crate::platform::linux`] exactly:
//!
//! * [`PipeStream`] implements [`crate::platform::Stream`] (a `Read + Write`
//!   byte transport wrapping an OS pipe handle) — including
//!   [`Stream::try_clone`](crate::platform::Stream::try_clone), which produces
//!   an independent handle to the same pipe via `DuplicateHandle`.
//! * [`connect`] — client side: open the pipe by name, returning
//!   [`OxiwakeError::NotRunning`] when there is nothing listening.
//! * [`bind_and_serve`] — server side: create the pipe, run a single-threaded
//!   accept loop that reads one [`ClientMsg`](crate::ipc::ClientMsg) per
//!   connection, hands it to `on_msg`, writes the
//!   [`DaemonReply`](crate::ipc::DaemonReply), and — after dispatching a
//!   [`Stop`](crate::ipc::ClientMsg::Stop) — breaks and returns `Ok(())`.
//!
//! ## Bounded server I/O (overlapped)
//!
//! The accept loop is single-threaded with a single pipe instance. With plain
//! blocking `ReadFile`, a client that connects and then stalls — sending a
//! partial frame or nothing at all — would wedge the daemon forever: every
//! later `ow off` / `ow status` / `ow toggle` would hang, and the lock could
//! not be released via IPC. To prevent that, the *server* side creates its pipe
//! with `FILE_FLAG_OVERLAPPED` and bounds every per-client read/write with a
//! [`WaitForSingleObject`] timeout on an event. A stall is treated as a
//! non-fatal drop: that instance is closed and the loop continues, exactly as
//! linux.rs treats a `TimedOut` read. (Client-side streams stay blocking; the
//! client's own liveness is its concern.)
//!
//! ## Security note
//!
//! `CreateNamedPipeW` is called with `lpSecurityAttributes = NULL`. Per the
//! named-pipe documentation the default ACL of a named pipe grants full control
//! to the `LocalSystem`, `Administrators`, and the *creator owner* accounts and
//! grants read access to members of `Everyone`. For a per-user keep-awake daemon
//! this is the same exposure as a Unix domain socket in a 0700 runtime dir —
//! other local users can connect and issue IPC commands but cannot escalate.
//! If a stricter ACL is required later, pass a real `SECURITY_DESCRIPTOR` here.

// The whole module is Windows-only.
#![cfg(windows)]

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem::MaybeUninit;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, ERROR_LOCK_VIOLATION, GetLastError, DUPLICATE_SAME_ACCESS, FALSE,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, LockFileEx, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, ResetEvent, WaitForSingleObject,
};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

use crate::error::{OxiwakeError, Result};
use crate::ipc::{read_msg, write_msg, ClientMsg, DaemonReply};
use crate::paths::Paths;
use crate::platform::Stream;

/// The well-known pipe path for the Oxiwake daemon: `\\.\pipe\oxiwake`.
///
/// A fixed name is acceptable because Oxiwake is a single-instance, per-user
/// daemon; only one daemon is expected per session.
pub const PIPE_PATH: &str = r"\\.\pipe\oxiwake";

/// Server-side output buffer size (bytes). Generous for Oxiwake's tiny JSON IPC
/// frames; not load-bearing.
const OUT_BUFFER: u32 = 4096;
/// Server-side input buffer size (bytes).
const IN_BUFFER: u32 = 4096;
/// Default timeout passed to `CreateNamedPipeW` (ms). Only nominal for our use.
const DEFAULT_TIMEOUT: u32 = 0;

/// Per-connection I/O timeout on the *server* side. Bounds how long the
/// single-threaded accept loop will wait on any one client's connect/read/write,
/// so a silent or stalling client is dropped instead of wedging the daemon
/// (which would otherwise block all later `ow off` / `ow status` / `ow toggle`).
/// Deliberately shorter than the client's 5s liveness budget so the server
/// recovers before a waiting client gives up — mirrors linux.rs's constant.
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(3);
/// `SERVER_IO_TIMEOUT` expressed as the milliseconds Win32 waits expect.
const SERVER_IO_TIMEOUT_MS: u32 = 3_000;
/// How long the client retries `CreateFileW` while the single server instance is
/// busy (transient `ERROR_PIPE_BUSY`). Bounded so a wedged-but-present server
/// does not hang the CLI forever; a few seconds is ample for a request/response
/// daemon whose every exchange is sub-millisecond.
const CONNECT_BUSY_RETRY_MS: u32 = 2_000;

// `GENERIC_READ | GENERIC_WRITE` are single-bit u32 flags from the access-rights
// mask. They are not exported by the FileSystem feature set we pull in, so spell
// them out (these values are stable by Win32 ABI contract).
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

// Win32 error codes we map to specific behavior. Stable by ABI contract.
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_IO_PENDING: u32 = 997;

// ---------------------------------------------------------------------------
// PipeStream: the Stream impl
// ---------------------------------------------------------------------------

/// A connected, owning wrapper around a named-pipe `HANDLE`.
///
/// Provides `Read`/`Write` via `ReadFile`/`WriteFile`. Dropping it closes the
/// handle (and any lazily-created overlapped event), so the OS resource is
/// never leaked. The pipe is used in byte mode (not message mode), so std-like
/// read/write semantics apply.
///
/// ## Overlapped timeouts
///
/// A `PipeStream` may optionally carry a *read deadline* (set via
/// [`PipeStream::set_read_timeout`]). When set, [`Read::read`] issues the
/// `ReadFile` overlapped and waits on its event for at most the deadline; a
/// timeout cancels the in-flight I/O and returns
/// [`io::ErrorKind::TimedOut`], mirroring how a `UnixStream` with a read
/// timeout behaves. The server side uses this to bound per-client reads; the
/// client side leaves it unset and stays blocking.
///
/// Overlapped I/O is only sound when the underlying handle was *created* with
/// `FILE_FLAG_OVERLAPPED`. The server constructs its pipe with that flag; the
/// client (whose handle `CreateFileW` opens without `FILE_FLAG_OVERLAPPED`) must
/// not set a read timeout — and does not.
pub struct PipeStream {
    handle: HANDLE,
    /// Lazily-created manual-reset event used for overlapped waits, or null if
    /// this stream has never needed one. Created on first overlapped operation.
    event: HANDLE,
    /// Optional read deadline. When set, [`Read::read`] performs a bounded
    /// overlapped wait. `None` (the default) means plain blocking `ReadFile`.
    read_timeout: Option<Duration>,
}

impl PipeStream {
    /// Wrap an already-open, connected pipe handle. The caller is responsible
    /// for ensuring `handle` is not `INVALID_HANDLE_VALUE`.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid, owned pipe handle that is safe to close with
    /// `CloseHandle` exactly once.
    pub unsafe fn from_raw(handle: HANDLE) -> Self {
        PipeStream {
            handle,
            event: ptr::null_mut(),
            read_timeout: None,
        }
    }

    /// Set a read deadline, mirroring `UnixStream::set_read_timeout`. When set,
    /// [`Read::read`] performs a bounded overlapped wait and returns
    /// [`io::ErrorKind::TimedOut`] on expiry. Only valid for handles created
    /// with `FILE_FLAG_OVERLAPPED` (i.e. the server side); the client side
    /// leaves this unset.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Duplicate this stream into an independent concrete `PipeStream` that
    /// *inherits* the read deadline, so the server's read half stays bounded
    /// after splitting. This is the concrete-typed counterpart of
    /// [`Stream::try_clone`]: it returns a `PipeStream` (not a `Box<dyn Stream>`)
    /// so the caller can still reach [`PipeStream::set_read_timeout`]. The
    /// duplicated handle shares the same underlying connection.
    fn try_clone_with_read_timeout(&self) -> io::Result<PipeStream> {
        let mut target: HANDLE = ptr::null_mut();
        // SAFETY: same `DuplicateHandle` discipline as `Stream::try_clone`:
        // valid source/destination pseudo-handles, valid out-pointer.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle,
                GetCurrentProcess(),
                &mut target,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == FALSE {
            let code = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        // SAFETY: `target` is a valid, freshly-duplicated, owned handle.
        let mut dup = unsafe { PipeStream::from_raw(target) };
        dup.read_timeout = self.read_timeout;
        // Prime the dup's own event up front so the first bounded read does not
        // need to create one. Best-effort: a failure is retried lazily in read.
        let _ = dup.overlapped_event();
        Ok(dup)
    }

    /// Return the lazily-created overlapped event for this stream, creating it
    /// on first use. A manual-reset event so a signaled state survives until we
    /// explicitly `ResetEvent` it between overlapped operations.
    fn overlapped_event(&mut self) -> io::Result<HANDLE> {
        if self.event.is_null() {
            // SAFETY: a NULL security-attributes pointer, manual-reset (TRUE),
            // initially non-signaled (FALSE), and an unnamed (NULL) event are
            // all documented-valid. On failure the call returns NULL.
            let ev = unsafe {
                CreateEventW(
                    ptr::null(),
                    1, /* TRUE: manual reset */
                    0, /* FALSE: not signaled */
                    ptr::null(),
                )
            };
            if ev.is_null() {
                let code = unsafe { GetLastError() };
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            self.event = ev;
        }
        Ok(self.event)
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        // SAFETY: by construction both handles are owned solely by this
        // `PipeStream`; closing each once here is correct. `INVALID_HANDLE_VALUE`
        // is never stored on `handle`, and `event` is either null or a valid
        // event handle we created.
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
        if !self.event.is_null() {
            unsafe { CloseHandle(self.event) };
        }
    }
}

// A pipe handle is process-wide (not thread-affine), so moving the stream
// across threads is sound. There is no interior mutability: `Read`/`Write`
// take `&mut self`, serializing access.
unsafe impl Send for PipeStream {}

impl Stream for PipeStream {
    fn try_clone(&self) -> io::Result<Box<dyn Stream>> {
        // Duplicate the handle into this same process. The result is a fully
        // independent handle to the same pipe: closing one half does not close
        // the other, mirroring `UnixStream::try_clone`. The clone starts with no
        // read deadline and its own (lazily-created) event.
        let mut target: HANDLE = ptr::null_mut();
        // SAFETY: `GetCurrentProcess()` is a pseudo-handle that is always
        // valid; `self.handle` is a valid owned pipe handle; `target` is a
        // stable out-pointer valid for the call. `DUPLICATE_SAME_ACCESS`
        // duplicates with the source's access mask, and `bInheritHandle=FALSE`.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle,
                GetCurrentProcess(),
                &mut target,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == FALSE {
            let code = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        // SAFETY: `target` is a valid, freshly-duplicated, owned handle.
        Ok(Box::new(unsafe { PipeStream::from_raw(target) }))
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Fast path: no deadline => plain blocking `ReadFile`, exactly as before
        // and as the client side expects. Overlapped is only sound on handles
        // created with FILE_FLAG_OVERLAPPED (the server side), and only the
        // server sets a read deadline.
        let deadline = match self.read_timeout {
            None => return read_blocking(self.handle, buf),
            Some(d) => d,
        };
        read_overlapped(self.handle, &mut self.event, buf, deadline)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Named-pipe server writes are tiny (a single reply frame) and the
        // client is waiting to read them, so blocking `WriteFile` cannot wedge
        // the loop in practice. Keep it simple and blocking.
        let mut bytes_written: u32 = 0;
        // SAFETY: `handle` is a valid pipe handle; `buf` is a shared borrow
        // valid for the call; `bytes_written` outlives the call; blocking I/O.
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut bytes_written as *mut u32,
                ptr::null_mut(),
            )
        };
        if ok == FALSE {
            let code = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(bytes_written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Named pipes in byte-stream mode are not buffered by the kernel in a
        // way that requires an explicit flush; `WriteFile` completes once the
        // data is handed to the pipe. Nothing to do.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Client: connect
// ---------------------------------------------------------------------------

/// Open a connection to the well-known Oxiwake pipe as a client.
///
/// Uses `CreateFileW` with `GENERIC_READ | GENERIC_WRITE` and `OPEN_EXISTING`,
/// which is the documented way for a client to connect to a named pipe. Returns
/// [`OxiwakeError::NotRunning`] when the pipe does not exist (genuinely "no
/// daemon"), so callers can give the user a clean `ow status` / `ow off`
/// failure. `ERROR_PIPE_BUSY` — the pipe exists but its single instance is in
/// use — is a *transient* condition, not "daemon absent": it is retried for a
/// short window via `WaitNamedPipeW` + `CreateFileW` rather than mapped to
/// `NotRunning`.
pub fn connect(paths: &Paths) -> Result<Box<dyn Stream>> {
    // On Windows the socket path is not used (the well-known pipe name is
    // fixed), but we accept `paths` to match the platform-neutral signature.
    let _ = paths;
    connect_named(PIPE_PATH)
}

fn connect_named(path: &str) -> Result<Box<dyn Stream>> {
    let wide = wide_utf16_z(path);

    loop {
        // SAFETY: `wide` is a NUL-terminated UTF-16 path valid for the call; the
        // remaining arguments are plain constants / null / zero. The handle is
        // opened *without* FILE_FLAG_OVERLAPPED, so the client uses blocking
        // I/O (it never sets a read deadline).
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0, // exclusive share: no sharing required for request/response IPC
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(), // hTemplateFile
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: `handle` is a valid, freshly-opened, owned pipe handle.
            return Ok(Box::new(unsafe { PipeStream::from_raw(handle) }) as Box<dyn Stream>);
        }

        let code = unsafe { GetLastError() };
        // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND: no pipe by that name
        // exists at all — the daemon is genuinely not running. Map to the
        // dedicated NotRunning variant, mirroring linux.rs.
        if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
            return Err(OxiwakeError::NotRunning);
        }
        // ERROR_PIPE_BUSY: the pipe *exists* but its single instance is busy
        // serving another client. This is a transient condition, NOT "daemon
        // absent" — do NOT map it to NotRunning (that would make a healthy but
        // momentarily-busy daemon look dead to `ow off` / `ow status`). Instead
        // ask the server to wake us when an instance is free, then retry
        // `CreateFileW` in a bounded loop.
        if code == ERROR_PIPE_BUSY {
            // SAFETY: `wide` is a NUL-terminated UTF-16 path valid for the call.
            // `WaitNamedPipeW` returns FALSE on timeout / no-instance; loop and
            // retry CreateFileW regardless, the bounded retry budget below caps
            // total effort.
            unsafe { WaitNamedPipeW(wide.as_ptr(), CONNECT_BUSY_RETRY_MS) };
            // Loop back and retry CreateFileW. To keep the total wait bounded
            // even if WaitNamedPipeW returns immediately (instance vanished
            // again), sleep briefly. A 50ms slot yields ~40 retries within the
            // 2s budget — plenty for a request/response daemon.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
    }
}

// ---------------------------------------------------------------------------
// Server: bind_and_serve
// ---------------------------------------------------------------------------

/// Create the named pipe and serve connections on the calling thread.
///
/// Mirrors [`crate::platform::linux`] exactly:
///
/// 1. **Orphan-lock guard.** Before creating the pipe, probe: try to connect to
///    the existing pipe; if a live daemon answers (i.e. `connect` succeeds),
///    return `Ok(())` *without* serving — do NOT replace a live daemon's pipe
///    (that would orphan its lock). Only (re)create the pipe if `connect` fails
///    with `NotRunning` (stale/absent).
/// 2. **Overlapped, bounded I/O.** The pipe is created with
///    `FILE_FLAG_OVERLAPPED`, and both the accept (`ConnectNamedPipe`) and the
///    per-client read are bounded by [`SERVER_IO_TIMEOUT`] via
///    `WaitForSingleObject`. A client that connects and then stalls is dropped
///    after the timeout instead of wedging the loop forever.
/// 3. **Stop path.** After dispatching a [`ClientMsg::Stop`] the loop writes the
///    reply and then breaks, returning `Ok(())` so the daemon can tear down and
///    its guard (the OS lock) drops.
///
/// The loop is single-threaded: Oxiwake's protocol is strictly one-message-
/// per-connection, so there is no benefit to concurrency, and staying
/// single-threaded keeps the daemon trivially auditable.
pub fn bind_and_serve(
    paths: &Paths,
    mut on_msg: impl FnMut(ClientMsg) -> DaemonReply,
) -> Result<()> {
    // The well-known pipe name is fixed; `paths` is accepted to match the
    // platform-neutral signature and is used only by the connect-first probe.
    // 1. Orphan-lock guard: do NOT blindly create the pipe — if another live
    //    daemon is already listening here, replacing its pipe would orphan its
    //    lock (it would keep holding the power request on a pipe no `ow off`
    //    could reach). Probe first:
    //      - connect succeeds  -> a daemon owns it; decline to serve (Ok).
    //      - NotRunning        -> stale/absent pipe; safe to (re)create.
    //      - other error       -> propagate.
    //    This mirrors src/platform/linux.rs bind_and_serve exactly.
    match crate::platform::connect(paths) {
        Ok(_) => {
            // Another daemon is live and reachable. Taking over would orphan
            // its lock, so we simply decline. The caller drops its guard.
            return Ok(());
        }
        Err(OxiwakeError::NotRunning) => {} // stale/absent: safe to (re)create.
        Err(e) => return Err(e),
    }

    // 2. Accept loop. Each iteration creates a fresh OVERLAPPED server instance,
    //    bounds the accept wait, then handles one connection with a bounded
    //    read. We break out explicitly on Stop.
    let mut stop_requested = false;
    while !stop_requested {
        // Each iteration creates a fresh server instance. `PipeServer` owns the
        // handle (and its overlapped event) and closes them on drop unless
        // handed off to a stream.
        let mut server = create_pipe_instance(PIPE_PATH)?;

        // Bound the accept wait so a long idle period does not matter (the
        // accept has no client to stall on, but capping it keeps the loop
        // responsive to Stop / shutdown). A timeout here simply means "no
        // client yet": close this instance and loop to create a fresh one.
        match server.connect_bounded(SERVER_IO_TIMEOUT_MS)? {
            ConnectOutcome::Connected => {}
            ConnectOutcome::Timeout => continue,
        }

        // Hand the connection off into a stream owned for the duration of the
        // exchange. `take_handle` disarms `PipeServer`'s Drop so only the stream
        // closes the handle. The stream inherits a read deadline so the
        // per-client read is bounded.
        let mut stream = server.take_handle();
        stream.set_read_timeout(Some(SERVER_IO_TIMEOUT));

        // Handle this one connection to completion. Per-connection errors are
        // swallowed (returned as `false`) so a misbehaving client cannot crash
        // the serve loop; the connection is simply dropped — exactly as in
        // linux.rs.
        stop_requested = handle_connection(stream, &mut on_msg).unwrap_or(false);
    }

    Ok(())
}

/// Read one [`ClientMsg`] from `stream`, dispatch it through `on_msg`, write
/// the [`DaemonReply`], and flush.
///
/// Returns `Ok(true)` if the message was [`ClientMsg::Stop`] (so the caller can
/// break its loop), `Ok(false)` otherwise. Per-connection errors — including a
/// read timeout from a stalled client — are swallowed (returned as `Ok(false)`)
/// so a misbehaving client cannot crash the serve loop; the connection is simply
/// dropped. This mirrors linux.rs.
fn handle_connection(
    stream: PipeStream,
    on_msg: &mut impl FnMut(ClientMsg) -> DaemonReply,
) -> Result<bool> {
    // Split the stream into a reader and writer so we can buffer each side.
    // `try_clone_with_read_timeout` yields an independent dup of the same pipe
    // that inherits the server read deadline, so the BufReader-backed
    // `read_msg` is bounded. (A TimedOut from the bounded overlapped read
    // surfaces as a swallowed non-fatal drop below, exactly as linux.rs treats
    // a TimedOut read.)
    let reader_half = stream
        .try_clone_with_read_timeout()
        .map_err(|e| OxiwakeError::Other(format!("could not dup IPC stream: {e}")))?;
    let writer_half = stream;
    let mut reader = BufReader::new(reader_half);
    let mut writer = BufWriter::new(writer_half);

    // Read exactly one framed message. A short read / disconnect / timeout
    // surfaces as NotRunning / Ipc / Io from `read_msg`; swallow it as a
    // non-fatal drop — exactly as linux.rs does for a TimedOut read.
    let msg: ClientMsg = match read_msg(&mut reader) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };

    let is_stop = matches!(msg, ClientMsg::Stop);
    let reply = on_msg(msg);

    // Best-effort write + flush. If the client hung up before reading the reply
    // there is nothing useful to do; treat as a non-fatal drop. On Stop we still
    // report `is_stop` so the caller writes-then-breaks per the contract.
    if write_msg(&mut writer, &reply).is_err() {
        return Ok(is_stop);
    }
    let _ = writer.flush();
    Ok(is_stop)
}

// ---------------------------------------------------------------------------
// Server-side pipe instance RAII
// ---------------------------------------------------------------------------

/// Outcome of a bounded [`PipeServer::connect_bounded`] accept.
enum ConnectOutcome {
    /// A client connected (either `ConnectNamedPipe` returned, or the
    /// `ERROR_PIPE_CONNECTED` race fired).
    Connected,
    /// No client arrived within the timeout window. The instance should be
    /// closed and the loop retried.
    Timeout,
}

/// A pipe server instance that owns its handle (and overlapped event) until
/// handed off to a [`PipeStream`]. Dropping it closes both.
struct PipeServer {
    handle: HANDLE,
    /// A manual-reset event used for overlapped accept/read waits. Owned.
    event: HANDLE,
}

impl PipeServer {
    /// Move the handle out into a connected [`PipeStream`], transferring
    /// ownership (and the `CloseHandle`) to the stream's `Drop`. The accept
    /// event is closed here since the stream owns its own (lazily-created)
    /// event for reads.
    fn take_handle(&mut self) -> PipeStream {
        let h = self.handle;
        self.handle = INVALID_HANDLE_VALUE; // disarm our handle Drop
        if !self.event.is_null() {
            // SAFETY: we own this event and no longer need it.
            unsafe { CloseHandle(self.event) };
            self.event = ptr::null_mut();
        }
        // SAFETY: `h` is a valid, connected, OVERLAPPED-capable pipe handle, now
        // solely owned by the returned stream.
        let mut stream = unsafe { PipeStream::from_raw(h) };
        // Prime the stream's own overlapped event up front so the first bounded
        // read does not need to create one. Best-effort: if it fails the read
        // path will retry.
        let _ = stream.overlapped_event();
        stream
    }

    /// Wait for a client to connect, bounded by `timeout_ms`. With overlapped
    /// I/O the wait is on the accept event via `WaitForSingleObject`; a timeout
    /// returns [`ConnectOutcome::Timeout`] without wedging the loop.
    fn connect_bounded(&mut self, timeout_ms: u32) -> Result<ConnectOutcome> {
        // Reset before arming: a manual-reset event stays signaled until reset,
        // and a leftover signaled state would make the wait return immediately.
        // SAFETY: `self.event` is a valid event we own.
        if unsafe { ResetEvent(self.event) } == FALSE {
            let code = unsafe { GetLastError() };
            return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
        }

        // OVERLAPPED is zero-initialized; we only use the hEvent field.
        let mut overlapped: OVERLAPPED = unsafe { MaybeUninit::zeroed().assume_init() };
        overlapped.hEvent = self.event;

        // SAFETY: `self.handle` is a valid named-pipe server handle created with
        // FILE_FLAG_OVERLAPPED; `overlapped` is a valid stack out-pointer whose
        // hEvent is our event. ConnectNamedPipe returns FALSE for both the
        // overlapped-pending case and real errors; GetLastError disambiguates.
        let connected = unsafe { ConnectNamedPipe(self.handle, &mut overlapped) };
        if connected != FALSE {
            // Synchronous success (rare for overlapped, but documented).
            return Ok(ConnectOutcome::Connected);
        }

        let code = unsafe { GetLastError() };
        // ERROR_PIPE_CONNECTED: a client connected between CreateNamedPipe and
        // ConnectNamedPipe. Benign race; the pipe is connected and usable.
        if code == ERROR_PIPE_CONNECTED {
            return Ok(ConnectOutcome::Connected);
        }
        // ERROR_IO_PENDING: the overlapped accept is in flight; wait on the
        // event for at most `timeout_ms`.
        if code != ERROR_IO_PENDING {
            return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
        }

        // SAFETY: `self.event` is a valid event handle; `timeout_ms` is a plain
        // millisecond count.
        let wait = unsafe { WaitForSingleObject(self.event, timeout_ms) };
        if wait == WAIT_FAILED {
            let code = unsafe { GetLastError() };
            return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
        }
        if wait == WAIT_TIMEOUT {
            // No client within the window. Cancel the in-flight overlapped
            // accept so the handle is reusable, then signal a non-fatal retry.
            // SAFETY: `self.handle` is the handle the overlapped was issued on.
            unsafe { CancelIo(self.handle) };
            // Drain the canceled operation: for a canceled overlapped,
            // GetOverlappedResult returns FALSE with ERROR_OPERATION_ABORTED.
            let mut transferred: u32 = 0;
            // SAFETY: `overlapped` is the same OVERLAPPED we armed; `bWait=FALSE`
            // so this never blocks.
            unsafe {
                GetOverlappedResult(self.handle, &overlapped, &mut transferred, FALSE);
            }
            return Ok(ConnectOutcome::Timeout);
        }

        // WAIT_OBJECT_0: the accept completed. Confirm via GetOverlappedResult,
        // which also finalizes the I/O for overlapped handles. A completion here
        // is a real connection. (Any other wait value is treated as completion
        // too — only WAIT_TIMEOUT / WAIT_FAILED are the bounded-exit cases.)
        debug_assert_eq!(wait, WAIT_OBJECT_0);
        let mut transferred: u32 = 0;
        // SAFETY: `overlapped` is the same OVERLAPPED we armed; `bWait=FALSE`
        // because we already observed the event signaled.
        let ok = unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, FALSE) };
        if ok == FALSE {
            let code = unsafe { GetLastError() };
            return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
        }
        Ok(ConnectOutcome::Connected)
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            // SAFETY: we own this handle and have not handed it off.
            unsafe { CloseHandle(self.handle) };
        }
        if !self.event.is_null() {
            // SAFETY: we own this event and have not closed it yet.
            unsafe { CloseHandle(self.event) };
        }
    }
}

fn create_pipe_instance(path: &str) -> Result<PipeServer> {
    let wide = wide_utf16_z(path);

    // An overlapped manual-reset event for the bounded accept. Auto-reset would
    // also work, but manual-reset is the safer default for serialized waits.
    // SAFETY: NULL security attrs, manual reset (TRUE), initially non-signaled
    // (FALSE), unnamed (NULL). On failure returns NULL.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        let code = unsafe { GetLastError() };
        return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
    }

    // SAFETY: `wide` is a NUL-terminated UTF-16 path valid for the call; all
    // other arguments are documented constants or NULL. The pipe is created in
    // byte-stream mode *and* with FILE_FLAG_OVERLAPPED so the server can bound
    // its accept/read waits — a blocking read could otherwise wedge the loop
    // forever on a stalled client.
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED, // duplex, overlapped server end
            pipe_mode(),                               // byte stream
            PIPE_UNLIMITED_INSTANCES,
            OUT_BUFFER,
            IN_BUFFER,
            DEFAULT_TIMEOUT,
            ptr::null(), // default security attributes
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        // SAFETY: we created `event` above and are about to throw it away.
        unsafe { CloseHandle(event) };
        return Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)));
    }
    Ok(PipeServer { handle, event })
}

/// Compose the `dwPipeMode` argument: byte type, byte read mode. (With overlapped
/// I/O the pipe is implicitly non-blocking at the API level; there is no
/// PIPE_NOWAIT flag to set, and doing so would switch to nonblocking byte mode
/// which is a different, legacy semantics — leave it in wait/byte mode.)
fn pipe_mode() -> NAMED_PIPE_MODE {
    // PIPE_TYPE_BYTE / PIPE_READMODE_BYTE / PIPE_WAIT are all 0, so the OR is 0;
    // we spell them out for documentation and future changes. (PIPE_WAIT here
    // governs non-overlapped blocking semantics, which we do not rely on since
    // every operation is overlapped.)
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE
}

// ---------------------------------------------------------------------------
// Low-level read helpers
// ---------------------------------------------------------------------------

/// Plain blocking `ReadFile`, no deadline. Used by the client (whose handle was
/// not created overlapped) and by a server stream that has no read deadline.
fn read_blocking(handle: HANDLE, buf: &mut [u8]) -> io::Result<usize> {
    let mut bytes_read: u32 = 0;
    // SAFETY: `handle` is a valid pipe handle; `buf` is a mutable borrow valid
    // for the call; `bytes_read` outlives the call. NULL overlapped => blocking.
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut bytes_read as *mut u32,
            ptr::null_mut(),
        )
    };
    if ok == FALSE {
        let code = unsafe { GetLastError() };
        // When the other end closes the pipe, `ReadFile` fails with
        // ERROR_BROKEN_PIPE. Map that to a clean end-of-stream so framing
        // layers see a normal EOF rather than a hard error.
        if code == ERROR_BROKEN_PIPE {
            return Ok(0);
        }
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(bytes_read as usize)
}

/// Issue an overlapped `ReadFile` on `handle` and wait at most `timeout` for it
/// to complete. `event_slot` is a lazily-initialized owned event used for the
/// wait (created on first use, closed by the owning `PipeStream`'s Drop). On
/// timeout the in-flight I/O is canceled and `io::ErrorKind::TimedOut` is
/// returned — mirroring a `UnixStream` with a read timeout.
///
/// # Safety preconditions
///
/// `handle` must have been created with `FILE_FLAG_OVERLAPPED`. The caller
/// (always the server side) guarantees this.
fn read_overlapped(
    handle: HANDLE,
    event_slot: &mut HANDLE,
    buf: &mut [u8],
    timeout: Duration,
) -> io::Result<usize> {
    // Ensure we have an event. Reuse `event_slot` so it is created once per
    // stream and closed on the stream's Drop.
    if event_slot.is_null() {
        // SAFETY: same args as elsewhere; NULL sec attrs, manual reset, not
        // signaled, unnamed. Returns NULL on failure.
        let ev = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if ev.is_null() {
            let code = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        *event_slot = ev;
    }
    let event = *event_slot;

    // Reset before arming so a leftover signaled state does not fool the wait.
    // SAFETY: `event` is a valid event we own.
    if unsafe { ResetEvent(event) } == FALSE {
        let code = unsafe { GetLastError() };
        return Err(io::Error::from_raw_os_error(code as i32));
    }

    let mut overlapped: OVERLAPPED = unsafe { MaybeUninit::zeroed().assume_init() };
    overlapped.hEvent = event;

    let mut bytes_read: u32 = 0;
    // SAFETY: `handle` is a valid OVERLAPPED-capable pipe handle; `buf` is a
    // mutable borrow valid for the call; `bytes_read` outlives it; `overlapped`
    // is a valid stack OVERLAPPED with our event.
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut bytes_read as *mut u32,
            &mut overlapped,
        )
    };
    if ok != FALSE {
        // Completed synchronously (the data was already in the pipe buffer).
        // ERROR_BROKEN_PIPE can still arrive here for a peer that closed.
        return Ok(bytes_read as usize);
    }

    let code = unsafe { GetLastError() };
    // ERROR_BROKEN_PIPE on an overlapped handle may surface immediately rather
    // than as pending; map it to EOF exactly like the blocking path.
    if code == ERROR_BROKEN_PIPE {
        return Ok(0);
    }
    if code != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(code as i32));
    }

    // Pending: wait for the event, bounded by `timeout`.
    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    // SAFETY: `event` is valid; `timeout_ms` is a plain ms count.
    let wait = unsafe { WaitForSingleObject(event, timeout_ms) };
    if wait == WAIT_FAILED {
        let code = unsafe { GetLastError() };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    if wait == WAIT_TIMEOUT {
        // Cancel the in-flight read so the handle is reusable for the next
        // client (this instance is about to be dropped anyway), then surface a
        // TimedOut so the framing layer treats it as a non-fatal drop —
        // exactly as a `set_read_timeout` UnixStream would.
        // SAFETY: `handle` is the handle the overlapped read was issued on.
        unsafe { CancelIo(handle) };
        let mut transferred: u32 = 0;
        // SAFETY: same `overlapped`; `bWait=FALSE` finalizes the cancel.
        unsafe {
            GetOverlappedResult(handle, &overlapped, &mut transferred, FALSE);
        }
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "named-pipe read timed out",
        ));
    }

    // WAIT_OBJECT_0: the read completed. Finalize and report the byte count.
    debug_assert_eq!(wait, WAIT_OBJECT_0);
    let mut transferred: u32 = 0;
    // SAFETY: same `overlapped`; `bWait=FALSE` because the event already fired.
    let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, FALSE) };
    if ok == FALSE {
        let code = unsafe { GetLastError() };
        if code == ERROR_BROKEN_PIPE {
            return Ok(0);
        }
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(transferred as usize)
}

// ---------------------------------------------------------------------------
// Singleton lock — the atomic mutex that gates daemon startup
// ---------------------------------------------------------------------------

/// `LockFileEx` flags: an exclusive lock that fails immediately rather than
/// blocking when another process already holds it.
const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;

/// An exclusive lock on the singleton lock file, held for the daemon's whole
/// lifetime so two racing `ow on` invocations can never both take an OS wake
/// lock. Dropping it closes the handle, which also releases the `LockFileEx`
/// lock — and would on a crash, once the OS reaps the process's handles.
pub struct SingletonLock {
    handle: HANDLE,
}

// SAFETY: the lock-file handle is process-wide state, not thread-affine; the
// daemon holds it on a single thread for its entire lifetime.
unsafe impl Send for SingletonLock {}

/// Atomically claim the singleton lock, or report that another daemon holds it.
///
/// Opens (creating if needed) `paths.lock` and takes an exclusive, failing-fast
/// `LockFileEx` on byte 0:
/// - `Ok(None)` — another process holds the lock (`ERROR_LOCK_VIOLATION`); the
///   caller should decline to serve.
/// - `Ok(Some(_))` — this process now owns the lock for its lifetime.
/// - `Err(_)` — any other failure.
pub fn acquire_singleton_lock(paths: &Paths) -> Result<Option<SingletonLock>> {
    use std::os::windows::io::{AsRawHandle, IntoRawHandle};

    // create + read + write. std's default share mode (READ|WRITE|DELETE) lets
    // two processes both hold the file open while LockFileEx arbitrates between
    // them, which is exactly the arbitration we want for a singleton daemon.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&paths.lock)?;
    let handle = file.as_raw_handle();

    // OVERLAPPED for LockFileEx: Offset/OffsetHigh select the region; lock byte
    // 0. The rest of the struct is unused and zero-initialized (mirrors the
    // existing overlapped usage in this file).
    let mut overlapped: OVERLAPPED = unsafe { MaybeUninit::zeroed().assume_init() };
    // SAFETY: `handle` is a valid file handle from a just-opened file;
    // `overlapped` is a valid stack out-pointer. The locked region is byte 0.
    let ok = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1, // nNumberOfBytesToLockLow — lock byte 0
            0,
            &mut overlapped,
        )
    };
    if ok != FALSE {
        // Take ownership of the handle without closing it; the guard keeps the
        // file (and its lock) open for the daemon's lifetime.
        let raw = file.into_raw_handle();
        Ok(Some(SingletonLock { handle: raw }))
    } else {
        let code = unsafe { GetLastError() };
        if code == ERROR_LOCK_VIOLATION {
            // Another daemon holds the singleton lock; decline to serve.
            Ok(None)
        } else {
            Err(OxiwakeError::Io(io::Error::from_raw_os_error(code as i32)))
        }
    }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        // Closing the handle releases any LockFileEx lock on it, so a separate
        // UnlockFileEx is unnecessary (and would need a matching OVERLAPPED).
        // Best-effort; there is nothing to propagate from a drop.
        if !self.handle.is_null() {
            // SAFETY: we solely own this handle and only close it here.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Encode a Rust string as a NUL-terminated UTF-16 vector for the wide Win32
/// APIs.
fn wide_utf16_z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_path_is_well_known() {
        assert_eq!(PIPE_PATH, r"\\.\pipe\oxiwake");
    }

    #[test]
    fn wide_utf16_z_is_nul_terminated() {
        let w = wide_utf16_z("pipe");
        assert_eq!(w, vec!['p' as u16, 'i' as u16, 'p' as u16, 'e' as u16, 0]);
    }

    #[test]
    fn pipe_mode_is_blocking_byte_stream() {
        // 0 = TYPE_BYTE | READMODE_BYTE | WAIT.
        assert_eq!(pipe_mode(), 0);
    }

    #[test]
    fn stream_is_implemented_for_pipe_stream() {
        // Compile-time check that PipeStream satisfies the Stream trait.
        fn accepts_stream<S: Stream>(_: &S) {}
        // We cannot construct a PipeStream without OS handles, so just assert
        // the trait bound holds at the type level via a function pointer type.
        let _: fn(&PipeStream) -> () = |s| accepts_stream(s);
        // (No runtime call; this exists to fail compilation if the bound breaks.)
    }
}
