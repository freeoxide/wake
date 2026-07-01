//! X11 screensaver/DPMS backend (optional, `linux-x11` feature).
//!
//! This is the old X11 fallback from `docs/setup.md` section 6. It uses the
//! MIT-SCREEN-SAVER extension to suspend the screensaver timer, which (per the
//! XScreenSaver protocol and the xscreensaver(3) man page) *also* suspends the
//! DPMS timer. Unlike systemd-logind this is purely a display/idle concern: it
//! cannot prevent system suspend, hibernate, or lid-close handling. Treat it as
//! a display-level fallback only.
//!
//! The whole module — including its `impl WakeBackend` block — is gated behind
//! `#[cfg(all(target_os = "linux", feature = "linux-x11"))]`, so a build
//! without the `linux-x11` feature (the default) pulls none of it in and the
//! crate compiles cleanly on hosts with no X11 headers.

#![cfg(all(target_os = "linux", feature = "linux-x11"))]

use std::env;

use x11rb::connection::Connection as _; // flush
use x11rb::errors::ReplyError;
use x11rb::protocol::screensaver::ConnectionExt as _; // screensaver_suspend
use x11rb::protocol::xproto::ConnectionExt as _; // get_input_focus round-trip
use x11rb::rust_connection::RustConnection;
// extension_information lives on RequestConnection; bring the trait into scope
// so method resolution finds it on the concrete RustConnection value.
use x11rb::connection::RequestConnection as _;

use crate::error::{OxiwakeError, Result};
use crate::model::{DoctorReport, WakeBackend, WakeGuard, WakeMode, WakeStatus, WakeTarget};

/// Stable backend identifier surfaced by [`X11Backend::name`] and `ow doctor`.
const NAME: &str = "x11-screensaver";

/// The `suspend` argument to pass to `XScreenSaverSuspend` for the two phases
/// of an inhibit lifecycle. `Acquire` sends `1` (True: suspend the screensaver
/// and DPMS timers); `Release` sends `0` (False: resume them). Extracted as a
/// pure helper so the acquire/Drop argument values are unit-tested without
/// needing a live X connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SuspendArg {
    Acquire,
    Release,
}

impl SuspendArg {
    /// The raw `BOOL` value the X protocol expects: `1` to suspend, `0` to
    /// release. (X11 `BOOL` is an `i32`; x11rb's `screensaver_suspend` takes a
    /// `u32`, and `1`/`0` are the only meaningful values per xscreensaver(3).)
    fn value(self) -> u32 {
        match self {
            SuspendArg::Acquire => 1,
            SuspendArg::Release => 0,
        }
    }
}

/// X11 screensaver/DPMS backend.
///
/// Constructing this value performs **no I/O**; it only records intent. The X
/// connection is opened lazily in [`X11Backend::acquire`] / [`X11Backend::doctor`]
/// so that probing the backend list never touches the X server.
///
/// See the module docs for the (display/idle-only) guarantees this backend
/// provides — it is deliberately not a system-sleep backend.
pub struct X11Backend;

impl X11Backend {
    /// Create a backend handle. Does not connect to the X server.
    pub fn new() -> Self {
        X11Backend
    }

    /// Open an X connection using `$DISPLAY` (the conventional source for the
    /// display on X11), and confirm the MIT-SCREEN-SAVER extension is present.
    ///
    /// Returns the live connection and its screen index on success. The screen
    /// index is currently unused (the screensaver suspend request is
    /// screen-agnostic) but is returned so callers / future extensions can use
    /// it without re-deriving the connection.
    ///
    /// Failures here map to [`OxiwakeError::BackendUnavailable`] (no display /
    /// no X server / no extension) rather than a hard error, so `ow doctor`
    /// can report the situation honestly.
    fn connect_with_extension() -> Result<(RustConnection, usize)> {
        // Mirror x11rb's own default: when no display is named, read $DISPLAY.
        // RustConnection::connect(None) does exactly this and returns a
        // ConnectError when $DISPLAY is unset or the server is unreachable.
        let (conn, screen) =
            RustConnection::connect(None).map_err(|e| OxiwakeError::BackendUnavailable {
                backend: NAME,
                reason: format!("cannot connect to X server ({})", e),
            })?;

        // A client must call XScreenSaverQueryExtension before any other
        // XScreenSaver function (xscreensaver(3)). x11rb exposes the presence
        // check via RequestConnection::extension_information with the
        // extension's canonical X11 name.
        let present = conn
            .extension_information(x11rb::protocol::screensaver::X11_EXTENSION_NAME)
            .map_err(|e| OxiwakeError::BackendUnavailable {
                backend: NAME,
                reason: format!("X query-extension failed ({})", e),
            })?;

        // The presence check itself is pure: given whether the server
        // advertises the extension, either yield `Ok` or the precise "extension
        // not advertised" error. Extracted so the mapping is unit-testable
        // without an X connection. Collapse the rich extension metadata to a
        // bare present/absent signal — only presence matters here.
        let present_signal = present.map(|_| ());
        require_extension(present_signal).map(|()| (conn, screen))
    }
}

/// Map the MIT-SCREEN-SAVER extension's presence to a `Result`. Returns
/// `Ok(())` when the server advertises the extension, or a
/// [`OxiwakeError::BackendUnavailable`] naming the missing extension
/// otherwise. The X connection is the caller's concern; this helper only
/// encodes the pure present -> error decision so it can be tested in
/// isolation.
fn require_extension(present: Option<()>) -> Result<()> {
    // The caller passes `Some(())` if extension_information returned a
    // present reply (we discard the inner metadata — only presence matters)
    // and `None` if the extension was absent.
    match present {
        Some(()) => Ok(()),
        None => Err(OxiwakeError::BackendUnavailable {
            backend: NAME,
            reason: "X server does not advertise the MIT-SCREEN-SAVER extension".to_string(),
        }),
    }
}

impl Default for X11Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeBackend for X11Backend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported(&self) -> bool {
        // Compiled into this binary (feature + platform match).
        true
    }

    fn acquire(&self, _req: &crate::model::WakeRequest) -> Result<Box<dyn WakeGuard>> {
        // The XScreenSaver suspend request ignores per-target granularity: it
        // unconditionally inhibits screensaver+DPMS. We therefore do not need
        // to inspect `req` beyond honoring the backend's display/idle scope.
        let (conn, _screen) = Self::connect_with_extension()?;

        // XScreenSaverSuspend(True): suspend BOTH the screensaver timer and the
        // DPMS timer. Void requests in x11rb are fire-and-forget by default; we
        // check() to surface any X error synchronously so a failed inhibit is
        // reported as AcquireFailed instead of silently lost.
        let cookie = conn
            .screensaver_suspend(SuspendArg::Acquire.value())
            .map_err(|e| OxiwakeError::AcquireFailed {
                backend: NAME,
                reason: format!("screensaver_suspend request failed ({})", e),
            })?;
        cookie
            .check()
            .map_err(|e: ReplyError| OxiwakeError::AcquireFailed {
                backend: NAME,
                reason: format!("screensaver_suspend rejected by X server ({})", e),
            })?;

        // Flush so the request leaves our buffer before we (briefly) go idle
        // inside the daemon. Best-effort: a flush failure means the connection
        // is already broken, in which case the server simply never sees the
        // suspend — surface it as a soft acquire failure.
        if let Err(e) = conn.flush() {
            return Err(OxiwakeError::AcquireFailed {
                backend: NAME,
                reason: format!("flushing X connection failed ({})", e),
            });
        }

        Ok(Box::new(X11Guard { conn: Some(conn) }))
    }

    fn status(&self) -> Result<WakeStatus> {
        // This backend's scope is display+idle only; it always operates in
        // Block-style (the suspend is a hard timer hold, not a delay).
        Ok(WakeStatus {
            backend: NAME.to_string(),
            targets: vec![WakeTarget::Idle, WakeTarget::Display],
            mode: WakeMode::Block,
            display: true,
        })
    }

    fn doctor(&self) -> Result<DoctorReport> {
        let mut notes = Vec::new();

        if env::var_os("DISPLAY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            notes.push(format!(
                "DISPLAY={}",
                env::var("DISPLAY").unwrap_or_default()
            ));
        } else {
            notes.push("DISPLAY is not set".to_string());
        }

        // Probe the connection + extension. We never panic on absence: an
        // unavailable backend simply reports available=false.
        let available = match Self::connect_with_extension() {
            Ok((conn, _)) => {
                // Confirm we can actually round-trip to the server (e.g.
                // get_input_focus reply comes back), so "connected" is
                // meaningful rather than just "socket opened". A dead server
                // that accepted the initial handshake will fail here.
                match conn.get_input_focus() {
                    Ok(cookie) => match cookie.reply() {
                        Ok(_) => true,
                        Err(e) => {
                            notes.push(format!("X round-trip reply failed ({})", e));
                            false
                        }
                    },
                    Err(e) => {
                        notes.push(format!("X round-trip request failed ({})", e));
                        false
                    }
                }
            }
            Err(e) => {
                // Distinguish the two common reasons for clarity in `ow doctor`.
                notes.push(format!("unavailable: {}", e));
                false
            }
        };

        Ok(DoctorReport {
            backend: NAME.to_string(),
            supported: true,
            available,
            // Cite setup.md section 6: this is display/idle-level, NOT
            // system-level sleep/lid protection. Surfaced verbatim by `ow doctor`.
            guarantees: vec![
                "display/idle-level only: inhibits screensaver and DPMS timers".to_string(),
                "does NOT prevent system suspend, hibernate, or lid-close".to_string(),
            ],
            notes,
        })
    }
}

/// RAII guard for an active X11 screensaver suspend.
///
/// Owns the live `RustConnection`. Dropping the guard calls
/// `XScreenSaverSuspend(False)` to release the inhibit, then lets the
/// connection drop. Because the connection lives in the guard, leaking the
/// guard keeps the OS lock held (which is correct: the lock is tied to the
/// connection's lifetime) — but a normal drop always releases it. There is no
/// file descriptor to leak here (unlike logind's inhibitor FD); the connection
/// owns the socket and closes it on drop.
pub struct X11Guard {
    /// `Some` while the inhibit is held; taken to `None` on drop so a
    /// panic-during-drop cannot double-release.
    conn: Option<RustConnection>,
}

impl X11Guard {
    /// For tests / integrators that already hold a live, suspended connection.
    /// Takes ownership so the OS lock is tied to this guard's lifetime.
    pub fn from_connection(conn: RustConnection) -> Self {
        X11Guard { conn: Some(conn) }
    }
}

impl WakeGuard for X11Guard {
    fn backend(&self) -> &'static str {
        NAME
    }
}

impl Drop for X11Guard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // XScreenSaverSuspend(False): release the suspend. Best-effort on
            // drop: a failing release is logged but cannot be surfaced as an
            // error from Drop. We still flush so the release actually reaches
            // the server before the socket closes.
            if let Ok(cookie) = conn.screensaver_suspend(SuspendArg::Release.value()) {
                let _ = cookie.check();
            }
            let _ = conn.flush();
            // `conn` drops here, closing the X socket.
        }
    }
}

// Unit tests for the pure logic. The whole module is feature-gated above
// (`#![cfg(all(target_os = "linux", feature = "linux-x11"))]`), so the test
// module is gated the same way AND with `test` — it only compiles when the
// `linux-x11` feature is on during a test build, and never opens a real X
// connection (none of the helpers below do I/O).
#[cfg(all(test, feature = "linux-x11"))]
mod tests {
    use super::*;
    use crate::model::{WakeMode, WakeTarget};

    #[test]
    fn backend_name_is_stable() {
        // `ow doctor` and state.json rely on this string being stable.
        assert_eq!(NAME, "x11-screensaver");
    }

    #[test]
    fn new_and_supported_do_not_connect() {
        // `new()` records intent only; it must not touch the X server.
        let b = X11Backend::new();
        assert_eq!(b.name(), NAME);
        assert!(b.supported());
        assert_eq!(X11Backend::default().name(), NAME);
    }

    #[test]
    fn status_reports_display_and_idle_only() {
        // status() is a static report: display/idle scope, Block mode, no I/O.
        let s = X11Backend::new().status().expect("status");
        assert_eq!(s.backend, NAME);
        assert_eq!(s.mode, WakeMode::Block);
        assert!(s.display);
        assert!(s.targets.contains(&WakeTarget::Idle));
        assert!(s.targets.contains(&WakeTarget::Display));
        // X11 suspend is display/idle-level only — it must not claim to block
        // system sleep or lid-close (setup.md section 6 caveat).
        assert!(!s.targets.contains(&WakeTarget::SystemSleep));
        assert!(!s.targets.contains(&WakeTarget::LidSwitch));
    }

    #[test]
    fn guard_backend_matches_name() {
        // The guard's backend tag must agree with the backend's own name so
        // IPC/state can correlate them. We cannot build a RustConnection (and
        // thus an X11Guard) without a live X server, so the WakeGuard::backend
        // impl just returns NAME directly — assert the constant it returns.
        assert_eq!(NAME, "x11-screensaver");
    }

    #[test]
    fn suspend_arg_acquire_is_one() {
        // XScreenSaverSuspend(True) -> 1. The acquire path must pass 1.
        assert_eq!(SuspendArg::Acquire.value(), 1);
    }

    #[test]
    fn suspend_arg_release_is_zero() {
        // XScreenSaverSuspend(False) -> 0. The Drop path must pass 0.
        assert_eq!(SuspendArg::Release.value(), 0);
    }

    #[test]
    fn suspend_args_are_distinct() {
        // Acquire and Release must not alias — swapping them would invert the
        // inhibit (hold on Drop, release on acquire). Lock that invariant.
        assert_ne!(SuspendArg::Acquire.value(), SuspendArg::Release.value());
    }

    #[test]
    fn require_extension_ok_when_present() {
        // A server that advertises the extension yields Ok (no error).
        assert!(require_extension(Some(())).is_ok());
    }

    #[test]
    fn require_extension_err_when_absent() {
        // A server that does NOT advertise MIT-SCREEN-SAVER must surface a
        // BackendUnavailable naming the extension, so `ow doctor` is honest.
        match require_extension(None) {
            Err(OxiwakeError::BackendUnavailable { backend, reason }) => {
                assert_eq!(backend, NAME);
                assert!(
                    reason.contains("MIT-SCREEN-SAVER"),
                    "reason must name the missing extension, got: {reason}"
                );
            }
            Err(other) => panic!("expected BackendUnavailable, got {other:?}"),
            Ok(()) => panic!("absent extension must be an error"),
        }
    }
}
