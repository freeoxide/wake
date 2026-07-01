//! XDG Desktop Portal inhibit backend.
//!
//! Talks to [`org.freedesktop.portal.Inhibit`] directly over the **session**
//! D-Bus using `zbus`'s blocking API. No `ashpd` — the Oxiwake stack stays
//! fully synchronous (no async runtime, no `.await`), so we drive the portal
//! by hand.
//!
//! ## What this backend does
//!
//! `Inhibit` takes a flags bitmask; Oxiwake always asks for
//! `LOGOUT_INHIBIT_FLAG_SUSPEND | LOGOUT_INHIBIT_FLAG_IDLE = 4 | 8 = 12`, which
//! maps onto the two `WakeTarget`s this backend honors: `SystemSleep` and
//! `Idle`. The method returns a **handle object path**. That object path *is*
//! the lifetime token: the inhibition stays in force until the client calls
//! [`org.freedesktop.portal.Request.Close`] on it (or disconnects). We hold
//! the handle inside an RAII [`PortalGuard`] and call `Close` from its `Drop`,
//! so leaking the guard can never leak the OS lock.
//!
//! ## The portal response model (and what we do about it)
//!
//! The portal delivers the real success/failure of an inhibit *asynchronously*
//! via the [`org.freedesktop.portal.Request.Response`] signal on the returned
//! handle. For a synchronous client there is no clean way to wait for that
//! signal with a bounded timeout on top of `zbus::blocking` — `SignalIterator`
//! blocks its owning thread indefinitely and there is no `poll`-style timeout.
//! Rather than spawn a thread + timeout machinery for marginal gain, we follow
//! the design's defensive recommendation: **the inhibit is treated as
//! effective the moment `Inhibit` returns a handle**. If the portal raises a
//! synchronous D-Bus error on the call itself (most failures show up there),
//! it is surfaced as [`OxiwakeError::AcquireFailed`]. See the module-level
//! notes in the design doc (`docs/setup.md`, section "2").
//!
//! ## Caveat (surfaced by `doctor`)
//!
//! Portal inhibit is **session/desktop-level**, not a kernel-level lid-close
//! blocker. It is an excellent secondary backend, not a sole backend — and it
//! is only useful at all when a portal backend is actually running on the
//! session bus (none is, on this build host).
//!
//! [`org.freedesktop.portal.Inhibit`]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Inhibit.html
//! [`org.freedesktop.portal.Request.Close`]: https://flatpak.github.io/xdp-dbus/docs/org.freedesktop.portal.Request.html
//! [`org.freedesktop.portal.Request.Response`]: https://flatpak.github.io/xdp-dbus/docs/org.freedesktop.portal.Request.html

use crate::error::{OxiwakeError, Result};
use crate::model::{DoctorReport, WakeBackend, WakeGuard, WakeMode, WakeStatus, WakeTarget};

use zbus::blocking::Connection;
use zbus::blocking::Proxy;
use zbus::zvariant::OwnedObjectPath;

/// D-Bus name of the XDG Desktop Portal root service, reached on the session bus.
const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";

/// Well-known root object path of the portal.
const PORTAL_ROOT_PATH: &str = "/org/freedesktop/portal/desktop";

/// `org.freedesktop.portal.Inhibit` interface.
const PORTAL_INHIBIT_IFACE: &str = "org.freedesktop.portal.Inhibit";

/// `org.freedesktop.portal.Request` interface (the per-call handle object).
const PORTAL_REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

/// `org.freedesktop.DBus` interface, used for the `NameHasOwner` availability probe.
const DBUS_IFACE: &str = "org.freedesktop.DBus";
/// `org.freedesktop.DBus` service name (the bus daemon itself).
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
/// `/org/freedesktop/DBus` object path.
const DBUS_PATH: &str = "/org/freedesktop/DBus";

/// `org.freedesktop.portal.Inhibit` flag: prevent logout. Not requested by Oxiwake.
#[allow(dead_code)]
const FLAG_LOGOUT: u32 = 1;
/// `org.freedesktop.portal.Inhibit` flag: prevent user switch. Not requested by Oxiwake.
#[allow(dead_code)]
const FLAG_USER_SWITCH: u32 = 2;
/// `org.freedesktop.portal.Inhibit` flag: prevent suspend. Requested.
const FLAG_SUSPEND: u32 = 4;
/// `org.freedesktop.portal.Inhibit` flag: prevent idle. Requested.
const FLAG_IDLE: u32 = 8;

/// The fixed flag set Oxiwake asks for: inhibit **suspend and idle** (`4 | 8`).
const INHIBIT_FLAGS: u32 = FLAG_SUSPEND | FLAG_IDLE;

/// Backend name surfaced by `WakeBackend::name` / `WakeGuard::backend` / `WakeStatus`.
pub const BACKEND_NAME: &str = "xdg-portal";

/// XDG Desktop Portal inhibit backend.
///
/// Constructed cheaply with [`PortalBackend::new`] (no I/O). All D-Bus work
/// happens in [`WakeBackend::acquire`] / [`WakeBackend::doctor`]. The backend
/// is compiled in only when both `target_os = "linux"` and the `linux-portal`
/// feature are enabled.
///
/// The guard it produces ([`PortalGuard`]) owns the session-bus `Connection`
/// and the portal request handle; dropping it calls
/// `org.freedesktop.portal.Request.Close` on the handle (best-effort), which is
/// the documented way to remove the inhibition.
#[derive(Debug, Default, Clone, Copy)]
pub struct PortalBackend;

impl PortalBackend {
    /// Cheap constructor — performs **no** I/O (no D-Bus connection here).
    pub fn new() -> Self {
        PortalBackend
    }
}

impl WakeBackend for PortalBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn supported(&self) -> bool {
        // Compiled into this binary (feature + platform gating below).
        true
    }

    /// Take the inhibit lock.
    ///
    /// Connects to the **session** bus, verifies the portal is actually owned
    /// (returning [`OxiwakeError::BackendUnavailable`] if it is not), then calls
    /// `org.freedesktop.portal.Inhibit.Inhibit("", 12, {"reason": ...})`. The
    /// returned handle object path is the lifetime token; we wrap it in a
    /// [`PortalGuard`] whose `Drop` calls `Request::Close`.
    ///
    /// Per the module docs, we do **not** wait on the asynchronous `Response`
    /// signal: a synchronous D-Bus failure on the `Inhibit` call surfaces here
    /// as [`OxiwakeError::AcquireFailed`], and otherwise the handle is treated
    /// as a live inhibition. `req.reason` is forwarded as the `reason` option;
    /// `req.targets` / `req.display` / `req.aggressive_lid` are not honored (the
    /// portal only offers suspend+idle), which `status()` / `doctor()` report
    /// honestly.
    fn acquire(&self, req: &crate::model::WakeRequest) -> Result<Box<dyn WakeGuard>> {
        let conn = Connection::session().map_err(|e| OxiwakeError::BackendUnavailable {
            backend: BACKEND_NAME,
            reason: format!("cannot connect to session D-Bus: {e}"),
        })?;

        if !portal_owned(&conn)? {
            return Err(OxiwakeError::BackendUnavailable {
                backend: BACKEND_NAME,
                reason: "org.freedesktop.portal.Desktop is not owned on the session bus"
                    .to_string(),
            });
        }

        let proxy = Proxy::new(
            &conn,
            PORTAL_SERVICE,
            PORTAL_ROOT_PATH,
            PORTAL_INHIBIT_IFACE,
        )
        .map_err(|e| OxiwakeError::AcquireFailed {
            backend: BACKEND_NAME,
            reason: format!("cannot build Inhibit proxy: {e}"),
        })?;

        // `options` is a{sv}. Only "reason" is required by the portal spec.
        let options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
            [("reason", zbus::zvariant::Value::new(req.reason.as_str()))]
                .into_iter()
                .collect();

        // IN window s, IN flags u, IN options a{sv}. Empty parent window is allowed.
        let reply = proxy
            .call_method("Inhibit", &("", INHIBIT_FLAGS, options))
            .map_err(|e| OxiwakeError::AcquireFailed {
                backend: BACKEND_NAME,
                reason: format!("portal Inhibit call failed: {e}"),
            })?;

        // OUT handle o — decode the object path of the per-call Request object.
        let handle: OwnedObjectPath = reply.body().deserialize().map_err(|e| {
            OxiwakeError::DbusDecode(format!("portal Inhibit reply was not an object path: {e}"))
        })?;

        Ok(Box::new(PortalGuard {
            conn,
            handle,
            // Already released? Set true by a manual `release()`; the Drop impl
            // checks it to avoid a double-Close (best-effort anyway, but cheap).
            closed: false,
        }))
    }

    /// Describe the lock this backend would hold, without taking one.
    ///
    /// The portal only offers suspend + idle inhibition, in a single (block-like)
    /// mode, with no display control; `status()` reports exactly that.
    fn status(&self) -> Result<WakeStatus> {
        Ok(WakeStatus {
            backend: BACKEND_NAME.to_string(),
            targets: vec![WakeTarget::SystemSleep, WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        })
    }

    /// Probe the session bus and report honest capabilities + caveats.
    ///
    /// `available` is `true` only if `org.freedesktop.portal.Desktop` answers
    /// `NameHasOwner` on the session bus. The guarantees field carries the
    /// design-doc caveat that this is desktop/session-level, **not** a kernel
    /// lid blocker.
    fn doctor(&self) -> Result<DoctorReport> {
        // If we cannot even talk to the session bus, the backend is simply
        // unreachable — surface that rather than panicking.
        let (available, notes) = match Connection::session() {
            Ok(conn) => {
                let owned = portal_owned(&conn).unwrap_or(false);
                let mut n = Vec::new();
                if owned {
                    n.push(
                        "org.freedesktop.portal.Desktop is owned on the session bus".to_string(),
                    );
                } else {
                    n.push(
                        "org.freedesktop.portal.Desktop is NOT owned on the session bus \
                         (no portal backend running)"
                            .to_string(),
                    );
                }
                (owned, n)
            }
            Err(e) => (false, vec![format!("cannot connect to session D-Bus: {e}")]),
        };

        Ok(DoctorReport {
            backend: BACKEND_NAME.to_string(),
            supported: true,
            available,
            guarantees: vec![
                "inhibits suspend and idle only (flags 4 | 8); not logout/user-switch".to_string(),
                // The headline caveat from docs/setup.md section 2.
                "portal inhibit is session/desktop-level, NOT a kernel-level lid-close blocker"
                    .to_string(),
                "useful as a secondary backend, not as the sole wake lock".to_string(),
            ],
            notes,
        })
    }
}

/// RAII handle to an active XDG portal inhibit.
///
/// Owns the session-bus `Connection` and the per-call portal `Request` handle
/// object path. `Drop` calls `org.freedesktop.portal.Request.Close` on the
/// handle — the documented way to remove the inhibition — best-effort (errors
/// are ignored, matching the spec where a missing/already-closed handle simply
/// does nothing). This makes the guard leak-safe: even if it is never dropped
/// explicitly, the inhibition is released the moment it is.
pub struct PortalGuard {
    /// The session bus connection the handle lives on. Kept alive so the
    /// portal does not see a disconnect (which would also drop the inhibit).
    conn: Connection,
    /// Object path of the `org.freedesktop.portal.Request` for this inhibit call.
    handle: OwnedObjectPath,
    /// Set once `Close` has been issued, so `Drop` does not retry.
    closed: bool,
}

impl PortalGuard {
    /// Release the inhibition immediately, instead of on drop.
    ///
    /// Idempotent. Errors are swallowed — per the portal spec, closing an
    /// already-closed or unknown handle is a no-op on the portal side, and we
    /// never want `Drop`-adjacent cleanup to panic.
    fn release(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Best-effort: a fresh proxy on the stored connection + the stored path.
        // Routing `Close` to the generic portal service works because the
        // Request object is exported by the same portal backend that owns the
        // root service. Ignore any failure.
        if let Ok(proxy) = Proxy::new(
            &self.conn,
            PORTAL_SERVICE,
            self.handle.as_str(),
            PORTAL_REQUEST_IFACE,
        ) {
            let _ = proxy.call_method::<_, ()>("Close", &());
        }
    }
}

impl WakeGuard for PortalGuard {
    fn backend(&self) -> &'static str {
        BACKEND_NAME
    }
}

impl Drop for PortalGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Ask the bus daemon whether `org.freedesktop.portal.Desktop` is currently
/// owned on the session bus.
///
/// This is the availability signal used by both `acquire` (gating the call)
/// and `doctor`. Implemented as a raw `org.freedesktop.DBus.NameHasOwner`
/// method call rather than a generated proxy method, so it does not depend on
/// which members `zbus::fdo::DBusProxy` happens to expose.
fn portal_owned(conn: &Connection) -> Result<bool> {
    let dbus = Proxy::new(conn, DBUS_SERVICE, DBUS_PATH, DBUS_IFACE)?;
    let reply = dbus.call_method("NameHasOwner", &(PORTAL_SERVICE,))?;
    let owned: bool = reply
        .body()
        .deserialize()
        .map_err(|e| OxiwakeError::DbusDecode(format!("NameHasOwner reply was not a bool: {e}")))?;
    Ok(owned)
}

// ---- platform / feature gating -------------------------------------------------
//
// Everything above is plain Rust, but the whole module (its public surface and
// the `WakeBackend` impl) is only meaningful on Linux with the `linux-portal`
// feature on. The crate's `Cargo.toml` only declares `zbus` for
// `target_os = "linux"`, so compiling this module on any other target would
// fail to resolve `zbus`. The integrator wires the cfg gate around the
// `mod backend;` / `pub mod portal;` declaration; we additionally keep the code
// free of any non-portable API so the gate is the only thing needed.
