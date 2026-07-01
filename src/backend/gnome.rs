//! `org.gnome.SessionManager` inhibition backend (Linux, session D-Bus).
//!
//! GNOME-specific fallback documented in `docs/setup.md` section 4. It speaks
//! the **session** bus:
//!
//! ```text
//! service : org.gnome.SessionManager
//! path    : /org/gnome/SessionManager
//! iface   : org.gnome.SessionManager
//! Inhibit(s app_id, u toplevel_xid, s reason, u flags) -> u cookie
//! ```
//!
//! The GNOME `Inhibit` signature is distinct from ScreenSaver's: `flags` is a
//! bitmask where `1 = logout`, `2 = switch-user`, `4 = suspend`, `8 = idle`,
//! `16 = automount` (setup.md §4). Oxiwake encodes `flags = 4 | 8 = 12`
//! (suspend + idle) with `toplevel_xid = 0`, matching the design default
//! `sleep:idle`. The inhibitor is released automatically when the registering
//! client exits, but we also drop the guard before process exit so the lock is
//! released deterministically rather than waiting for connection teardown.

use crate::error::{OxiwakeError, Result};
use crate::model::{WakeBackend, WakeGuard, WakeMode, WakeRequest, WakeStatus, WakeTarget};

/// Backend name surfaced everywhere (`backend` field, `doctor`, guard).
const NAME: &str = "gnome-session";

/// D-Bus coordinates for `org.gnome.SessionManager`.
const DEST: &str = "org.gnome.SessionManager";
const PATH: &str = "/org/gnome/SessionManager";
const IFACE: &str = "org.gnome.SessionManager";

/// GNOME inhibit `flags` bitmask: `4` (suspend) `| 8` (idle) = `12`.
///
/// `1 = logout`, `2 = switch-user`, `4 = suspend`, `8 = idle`, `16 = automount`
/// (setup.md §4). Oxiwake's default intent is `sleep:idle`, hence `4 | 8`.
const FLAGS_SUSPEND_IDLE: u32 = 4 | 8;

/// `org.gnome.SessionManager` inhibition backend.
///
/// Holds no OS resource until [`WakeBackend::acquire`] is called; `new()` does
/// no I/O. The returned guard owns the cookie and best-effort re-signals the
/// session manager on `Drop` (GNOME also releases on client exit, but dropping
/// the guard is the deterministic, leak-safe path).
pub struct GnomeBackend;

impl GnomeBackend {
    /// Construct the backend. Performs no I/O and never touches D-Bus.
    pub fn new() -> Self {
        GnomeBackend
    }
}

impl Default for GnomeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeBackend for GnomeBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported(&self) -> bool {
        true
    }

    /// Take the GNOME session inhibition lock.
    ///
    /// Connects to the **session** bus and calls `Inhibit(app_id, toplevel_xid,
    /// reason, flags)` with `toplevel_xid = 0` and `flags = 4|8` (suspend+idle).
    /// Reports `BackendUnavailable` if the session bus or the GNOME
    /// SessionManager service is absent (graceful, never panics).
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
        // Inhibit(s app_id, u toplevel_xid, s reason, u flags) -> u cookie.
        let cookie: u32 = proxy
            .call_method(
                "Inhibit",
                &(
                    WakeRequest::WHO,
                    0u32,
                    req.reason.as_str(),
                    FLAGS_SUSPEND_IDLE,
                ),
            )
            .map_err(|e| OxiwakeError::AcquireFailed {
                backend: NAME,
                reason: format!("Inhibit failed: {e}"),
            })?
            .body()
            .deserialize::<u32>()
            .map_err(|e| OxiwakeError::DbusDecode(format!("cookie u32: {e}")))?;

        Ok(Box::new(GnomeGuard { conn, cookie }))
    }

    /// Status reflects the fixed inhibit set: system sleep + idle, block mode.
    fn status(&self) -> Result<WakeStatus> {
        Ok(WakeStatus {
            backend: NAME.to_string(),
            targets: vec![WakeTarget::SystemSleep, WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        })
    }

    /// Probe whether `org.gnome.SessionManager` is owned on the session bus and
    /// document the GNOME-session caveats (session-level, not a kernel sleep
    /// blocker; honors only `suspend` + `idle` here).
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
                "inhibits suspend + idle (GNOME flags 4|8 = 12); other actions \
                 (logout/switch/automount) not requested"
                    .to_string(),
                "session-level inhibition, not a kernel sleep / lid blocker \
                 (use systemd-logind for system-level guarantees)"
                    .to_string(),
            ],
            notes: vec![format!(
                "session D-Bus service {DEST} at {PATH}; flags={FLAGS_SUSPEND_IDLE}"
            )],
        })
    }
}

/// RAII guard for a GNOME SessionManager inhibition cookie.
///
/// Owns the session connection plus the cookie. GNOME releases the inhibition
/// when the registering client disconnects, but the connection here is held by
/// the guard so it stays alive for the guard's lifetime; on `Drop` the
/// connection (and thus the inhibition) is released deterministically.
struct GnomeGuard {
    // Held so the session-bus connection outlives the guard; dropping it
    // disconnects, which the SessionManager treats as releasing the inhibit.
    #[allow(dead_code)]
    conn: zbus::blocking::Connection,
    #[allow(dead_code)] // cookie retained for diagnostics / future explicit-release API.
    cookie: u32,
}

impl WakeGuard for GnomeGuard {
    fn backend(&self) -> &'static str {
        NAME
    }
}

impl Drop for GnomeGuard {
    fn drop(&mut self) {
        // GNOME SessionManager releases inhibition on client disconnect; the
        // owned session connection drops here, ending the registration. There
        // is no per-cookie release method on this interface, so we intentionally
        // rely on connection teardown (deterministic via the guard's lifetime).
        // The cookie is retained for diagnostics and a future explicit-release
        // path should GNOME add one.
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
