//! `org.freedesktop.ScreenSaver` idle-inhibition backend (Linux, session D-Bus).
//!
//! This is the classic freedesktop idle-inhibit fallback documented in
//! `docs/setup.md` section 3. It speaks the **session** bus:
//!
//! ```text
//! service : org.freedesktop.ScreenSaver
//! path    : /org/freedesktop/ScreenSaver
//! iface   : org.freedesktop.ScreenSaver
//! Inhibit(s application_name, s reason) -> u cookie
//! UnInhibit(u cookie)
//! ```
//!
//! # Guarantees (be honest in `doctor`)
//!
//! Per the freedesktop idle-inhibit spec (setup.md §3 / ref [8]), this service
//! **only inhibits idleness** — screen blanking / idle lock / idle-triggered
//! actions where the session honors it. It explicitly does **not** support
//! suspend, hibernation, or user switching, and user-requested actions still
//! happen. So Oxiwake reports this backend as an `Idle`-only fallback, never
//! as a system-sleep / lid blocker.

use crate::error::{OxiwakeError, Result};
use crate::model::{WakeBackend, WakeGuard, WakeMode, WakeRequest, WakeStatus, WakeTarget};

/// Backend name surfaced everywhere (`backend` field, `doctor`, guard).
const NAME: &str = "freedesktop-screensaver";

/// D-Bus coordinates for `org.freedesktop.ScreenSaver`.
const DEST: &str = "org.freedesktop.ScreenSaver";
const PATH: &str = "/org/freedesktop/ScreenSaver";
const IFACE: &str = "org.freedesktop.ScreenSaver";

/// `org.freedesktop.ScreenSaver` idle-inhibit backend.
///
/// Holds no OS resource until [`WakeBackend::acquire`] is called; `new()` does
/// no I/O. The returned guard owns the inhibition cookie and releases it on
/// `Drop` via `UnInhibit`.
pub struct ScreenSaverBackend;

impl ScreenSaverBackend {
    /// Construct the backend. Performs no I/O and never touches D-Bus.
    pub fn new() -> Self {
        ScreenSaverBackend
    }
}

impl Default for ScreenSaverBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeBackend for ScreenSaverBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported(&self) -> bool {
        true
    }

    /// Take the idle-inhibition lock.
    ///
    /// Connects to the **session** bus and calls `Inhibit(application_name,
    /// reason)`, returning a guard that owns the cookie. If no session bus is
    /// reachable or the `org.freedesktop.ScreenSaver` service is not owned, the
    /// call is reported as `BackendUnavailable` (graceful, never panics).
    fn acquire(&self, req: &WakeRequest) -> Result<Box<dyn WakeGuard>> {
        let conn = zbus::blocking::Connection::session().map_err(|e| {
            OxiwakeError::BackendUnavailable {
                backend: NAME,
                reason: format!("no session D-Bus: {e}"),
            }
        })?;

        if !name_has_owner(&conn, DEST)? {
            return Err(OxiwakeError::BackendUnavailable {
                backend: NAME,
                reason: format!("{DEST} not owned on the session bus"),
            });
        }

        let proxy = zbus::blocking::Proxy::new(&conn, DEST, PATH, IFACE)?;
        // Inhibit(s application_name, s reason) -> u cookie.
        let cookie: u32 = proxy
            .call_method("Inhibit", &(WakeRequest::WHO, req.reason.as_str()))
            .map_err(|e| OxiwakeError::AcquireFailed {
                backend: NAME,
                reason: format!("Inhibit failed: {e}"),
            })?
            .body()
            .deserialize::<u32>()
            .map_err(|e| OxiwakeError::DbusDecode(format!("cookie u32: {e}")))?;

        Ok(Box::new(ScreenSaverGuard { conn, cookie }))
    }

    /// Minimal status: this backend only ever inhibits idleness, in block mode.
    fn status(&self) -> Result<WakeStatus> {
        Ok(WakeStatus {
            backend: NAME.to_string(),
            targets: vec![WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        })
    }

    /// Probe whether `org.freedesktop.ScreenSaver` is owned on the session bus
    /// and report the honest idle-only guarantee (setup.md §3).
    fn doctor(&self) -> Result<crate::model::DoctorReport> {
        let available = match zbus::blocking::Connection::session() {
            Ok(conn) => name_has_owner(&conn, DEST).unwrap_or(false),
            Err(_) => false,
        };

        Ok(crate::model::DoctorReport {
            backend: NAME.to_string(),
            supported: true,
            available,
            guarantees: vec![
                "inhibits idleness only (screen blank / idle lock)".to_string(),
                "does NOT prevent suspend, hibernate, lid-close, or shutdown \
                 (freedesktop idle-inhibit spec, setup.md §3)"
                    .to_string(),
            ],
            notes: vec![format!("session D-Bus service {DEST} at {PATH}")],
        })
    }
}

/// RAII guard for a `org.freedesktop.ScreenSaver` inhibition cookie.
///
/// Owns the session connection plus the cookie; `Drop` calls `UnInhibit(cookie)`
/// best-effort (errors are ignored — releasing is best-effort by spec and must
/// never panic). Leaking the guard would leak the OS lock, but Oxiwake never
/// leaks guards: the daemon drops them on `ow off` / shutdown.
struct ScreenSaverGuard {
    conn: zbus::blocking::Connection,
    cookie: u32,
}

impl WakeGuard for ScreenSaverGuard {
    fn backend(&self) -> &'static str {
        NAME
    }
}

impl Drop for ScreenSaverGuard {
    fn drop(&mut self) {
        // Best-effort release; ignore all errors per spec.
        if let Ok(proxy) = zbus::blocking::Proxy::new(&self.conn, DEST, PATH, IFACE) {
            let _ = proxy.call_method("UnInhibit", &(self.cookie));
        }
    }
}

/// Ask `org.freedesktop.DBus` whether `name` currently has an owner on this bus.
///
/// Centralizes the session-bus reachability probe used by both `acquire` and
/// `doctor`. Returns `false` for any D-Bus error so callers can degrade
/// gracefully. (Non-`pub`: an internal helper.)
fn name_has_owner(conn: &zbus::blocking::Connection, name: &str) -> Result<bool> {
    let proxy = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    let owner: bool = proxy
        .call_method("NameHasOwner", &(name))?
        .body()
        .deserialize::<bool>()
        .map_err(|e| OxiwakeError::DbusDecode(format!("NameHasOwner bool: {e}")))?;
    Ok(owner)
}
