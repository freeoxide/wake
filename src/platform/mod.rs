//! Platform transport for the daemon IPC.
//!
//! The IPC protocol itself ([`crate::ipc`]) is platform-neutral; this module
//! supplies the byte stream it runs over. On Linux that is a Unix domain
//! socket in the runtime directory; on Windows it is a named pipe.
//!
//! The surface is intentionally small and identical on every platform:
//!
//! - [`Stream`] — a trait combining [`std::io::Read`] and [`std::io::Write`],
//!   so the IPC layer can treat the connection as a single bidirectional
//!   handle.
//! - [`connect`] — client side: open a connection to a running daemon. Maps
//!   "no socket / connection refused" to [`crate::error::OxiwakeError::NotRunning`]
//!   so callers can treat a missing daemon uniformly.
//! - [`bind_and_serve`] — server side: bind the listening socket and run the
//!   single-threaded accept loop. For each accepted connection it reads one
//!   [`ClientMsg`](crate::ipc::ClientMsg), hands it to a closure to produce a
//!   [`DaemonReply`](crate::ipc::DaemonReply), writes the reply, and — after
//!   dispatching a [`Stop`](crate::ipc::ClientMsg::Stop) — breaks the loop and
//!   returns `Ok(())`.
//!
//! Only one platform module is compiled per build: the `cfg` declarations
//! below pull in `linux` on `target_os = "linux"` and `windows` on `windows`,
//! and re-export that module's `connect` / `bind_and_serve` at the crate-root
//! of this module. The other platform's code is simply absent, so the crate
//! links on either host.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{bind_and_serve, connect};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{bind_and_serve, connect};

/// A bidirectional byte stream connecting a client and the daemon.
///
/// This is the conjunction of [`std::io::Read`] and [`std::io::Write`] plus a
/// [`Stream::try_clone`] that produces an independent handle to the same
/// underlying connection: Unix sockets and named pipes are both read/write,
/// and the IPC layer needs to buffer reads and writes against separate halves
/// of the same stream. Concrete platform implementations ([`linux`]'s
/// `UnixStream`, the Windows named-pipe handle) implement this trait, and
/// [`connect`] returns one boxed so the caller stays platform-neutral.
pub trait Stream: std::io::Read + std::io::Write {
    /// Produce an independent boxed handle to the same underlying connection.
    ///
    /// Used so a caller can wrap one half in a [`std::io::BufReader`] and the
    /// other in a [`std::io::BufWriter`] without giving up ownership of the
    /// stream. Implementations mirror their OS primitive's native "duplicate
    /// handle" operation (`dup` / `UnixStream::try_clone`).
    fn try_clone(&self) -> std::io::Result<Box<dyn Stream>>;
}
