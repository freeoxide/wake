//! KDE PowerDevil / Solid inhibition backend (Linux, session D-Bus).
//!
//! KDE-specific fallback documented in `docs/setup.md` section 5. It speaks the
//! **session** bus against the Solid PowerManagement policy agent:
//!
//! ```text
//! service : org.kde.Solid.PowerManagement.PolicyAgent
//! path    : /org/kde/Solid/PowerManagement/PolicyAgent
//! iface   : org.kde.Solid.PowerManagement.PolicyAgent
//! AddInhibition(u actions_bitmask, s app, s reason) -> u cookie
//! ReleaseInhibition(u cookie)
//! ```
//!
//! The first `u` argument is a bitmask of power-management actions to inhibit.
//! Per the KDE `PolicyAgent` introspection (setup.md §5 / ref [10]) the action
//! bits are `ChangeProfile = 1` and `ChangeScreenSettings = 2` (among others).
//! Oxiwake inhibits **both** — `actions_bitmask = 1 | 2 = 3` — so the screen /
//! DPMS settings cannot change and the power profile cannot be swapped while
//! the lock is held, which is what keeps an idle KDE session awake.
//! The cookie is released via `ReleaseInhibition(u)` on guard drop.

use crate::error::{OxiwakeError, Result};
use crate::model::{WakeBackend, WakeGuard, WakeMode, WakeRequest, WakeStatus, WakeTarget};

/// Backend name surfaced everywhere (`backend` field, `doctor`, guard).
const NAME: &str = "kde-powerdevil";

/// D-Bus coordinates for the Solid PowerManagement policy agent.
const DEST: &str = "org.kde.Solid.PowerManagement.PolicyAgent";
const PATH: &str = "/org/kde/Solid/PowerManagement/PolicyAgent";
const IFACE: &str = "org.kde.Solid.PowerManagement.PolicyAgent";

/// KDE `AddInhibition` actions bitmask: `1` (ChangeProfile) `| 2`
/// (ChangeScreenSettings) = `3`.
///
/// From the KDE `PolicyAgent` introspection XML (setup.md §5, ref [10]): the
/// documented action bits are `ChangeProfile = 1` and `ChangeScreenSettings =
/// 2`. Inhibiting both prevents the screen/DPMS settings from changing and the
/// power profile from being swapped, which keeps an idle KDE session awake.
const ACTIONS_CHANGE_PROFILE_AND_SCREEN: u32 = 1 | 2;

/// KDE PowerDevil / Solid policy-agent inhibition backend.
///
/// Holds no OS resource until [`WakeBackend::acquire`] is called; `new()` does
/// no I/O. The returned guard owns the inhibition cookie and releases it on
/// `Drop` via `ReleaseInhibition`.
pub struct KdeBackend;

impl KdeBackend {
    /// Construct the backend. Performs no I/O and never touches D-Bus.
    pub fn new() -> Self {
        KdeBackend
    }
}

impl Default for KdeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeBackend for KdeBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported(&self) -> bool {
        true
    }

    /// Take the KDE PowerDevil inhibition lock.
    ///
    /// Connects to the **session** bus and calls `AddInhibition(actions_bitmask,
    /// app, reason)`, returning a guard that owns the cookie. Reports
    /// `BackendUnavailable` if the session bus or the Solid PowerManagement
    /// policy agent is absent (graceful, never panics).
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
        // AddInhibition(u actions_bitmask, s app, s reason) -> u cookie.
        let cookie: u32 = proxy
            .call_method(
                "AddInhibition",
                &(
                    ACTIONS_CHANGE_PROFILE_AND_SCREEN,
                    WakeRequest::WHO,
                    req.reason.as_str(),
                ),
            )
            .map_err(|e| OxiwakeError::AcquireFailed {
                backend: NAME,
                reason: format!("AddInhibition failed: {e}"),
            })?
            .body()
            .deserialize::<u32>()
            .map_err(|e| OxiwakeError::DbusDecode(format!("cookie u32: {e}")))?;

        Ok(Box::new(KdeGuard { conn, cookie }))
    }

    /// Status reflects the idle/keep-awake intent this backend serves.
    fn status(&self) -> Result<WakeStatus> {
        Ok(WakeStatus {
            backend: NAME.to_string(),
            targets: vec![WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        })
    }

    /// Probe whether the Solid PowerManagement policy agent is owned on the
    /// session bus and document that this is desktop policy, not a system-level
    /// sleep/lid blocker.
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
                "inhibits ChangeProfile + ChangeScreenSettings (KDE bits 1|2 = 3) \
                 to keep the session awake"
                    .to_string(),
                "desktop power policy, not a system-level sleep / lid blocker \
                 (use systemd-logind for system-level guarantees)"
                    .to_string(),
            ],
            notes: vec![format!(
                "session D-Bus service {DEST} at {PATH}; actions={ACTIONS_CHANGE_PROFILE_AND_SCREEN}"
            )],
        })
    }
}

/// RAII guard for a KDE PowerDevil inhibition cookie.
///
/// Owns the session connection plus the cookie; `Drop` calls
/// `ReleaseInhibition(cookie)` best-effort (errors are ignored — releasing is
/// best-effort and must never panic).
struct KdeGuard {
    conn: zbus::blocking::Connection,
    cookie: u32,
}

impl WakeGuard for KdeGuard {
    fn backend(&self) -> &'static str {
        NAME
    }
}

impl Drop for KdeGuard {
    fn drop(&mut self) {
        // Best-effort release; ignore all errors.
        if let Ok(proxy) = zbus::blocking::Proxy::new(&self.conn, DEST, PATH, IFACE) {
            let _ = proxy.call_method("ReleaseInhibition", &(self.cookie));
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
