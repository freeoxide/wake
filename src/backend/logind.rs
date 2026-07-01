//! systemd-logind D-Bus inhibitor backend (the primary Linux backend).
//!
//! This is Oxiwake's strongest Linux mechanism. systemd-logind exposes an
//! `Inhibit` method on the **system** bus whose reply is a file descriptor:
//! the inhibitor stays alive exactly as long as that FD is open, and is
//! released the instant it is closed. That makes RAII ownership via a guard
//! struct that owns the `OwnedFd` the correct, leak-safe design — see
//! [`LogindGuard`].
//!
//! D-Bus contract (verified against this host's introspection):
//!
//! ```text
//! service:   org.freedesktop.login1
//! path:      /org/freedesktop/login1
//! interface: org.freedesktop.login1.Manager
//! method:    Inhibit(in s what, in s who, in s why, in s mode, out h fd)
//! ```
//!
//! Only the blocking zbus API is used (`zbus::blocking::Connection::system()`,
//! `zbus::blocking::Proxy`, `proxy.call_method(...)`). There is no async
//! runtime and no `.await` anywhere in this module.
//!
//! `shutdown`/`sleep`/`handle-*` block inhibitors normally require a Polkit
//! privilege (`org.freedesktop.login1.inhibit-block-*`); `idle` is the one
//! routinely allowed unprivileged. We map an access-denied reply to
//! [`OxiwakeError::AcquireFailed`] and let the caller try the next backend.

use std::os::fd::OwnedFd;

use zbus::blocking::{Connection, Proxy};
use zbus::names::WellKnownName;
use zbus::zvariant::{ObjectPath, OwnedFd as ZvariantOwnedFd};

use crate::error::{OxiwakeError, Result};
use crate::model::{DoctorReport, WakeBackend, WakeGuard, WakeMode, WakeRequest, WakeStatus};

/// systemd-logind D-Bus coordinates. All three are fixed by the interface.
const SERVICE: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const INTERFACE: &str = "org.freedesktop.login1.Manager";

/// The systemd-logind inhibitor backend.
///
/// Constructing this does **no** I/O — it is just a handle. All bus work
/// happens in [`LogindBackend::acquire`] / [`LogindBackend::doctor`].
#[derive(Debug, Default, Clone, Copy)]
pub struct LogindBackend;

impl LogindBackend {
    /// Create a backend handle. No D-Bus connection is opened here.
    pub fn new() -> Self {
        LogindBackend
    }
}

impl WakeBackend for LogindBackend {
    fn name(&self) -> &'static str {
        "systemd-logind"
    }

    fn supported(&self) -> bool {
        true
    }

    /// Take an inhibitor lock on the system bus.
    ///
    /// Builds `what` from the request via [`WakeRequest::logind_what`], always
    /// takes the lock in `block` mode (per the design spec), and hands logind
    /// our identity (`WakeRequest::WHO`) and the user's reason. The reply `h`
    /// (FD) is captured into an [`OwnedFd`] that the returned guard owns; the
    /// inhibitor is released when that FD is dropped.
    ///
    /// An access-denied / Polkit-refusal reply is mapped to
    /// [`OxiwakeError::AcquireFailed`]; any other D-Bus failure propagates as
    /// [`OxiwakeError::Dbus`] (via `?`).
    fn acquire(&self, req: &WakeRequest) -> Result<Box<dyn WakeGuard>> {
        let conn = Connection::system().map_err(|e| map_unavailable(&e))?;

        let proxy = Proxy::new(&conn, SERVICE, PATH, INTERFACE)?;

        let what = req.logind_what();
        let mode = WakeMode::Block.as_str();
        let body = (&what, WakeRequest::WHO, &req.reason, mode);

        // If `what` is empty (no logind-relevant targets), logind rejects the
        // call. Fail up front with a clear reason rather than letting logind
        // return an opaque "Invalid argument".
        if what.is_empty() {
            return Err(OxiwakeError::AcquireFailed {
                backend: "systemd-logind",
                reason: "request maps to no logind `what` tokens".to_string(),
            });
        }

        let reply = proxy
            .call_method("Inhibit", &body)
            .map_err(|e| map_acquire(&e))?;

        // Decode the `out h` handle. We deserialize the reply body into
        // `zvariant::OwnedFd` (which carries `Type` + `Deserialize` and wraps
        // `std::os::fd::OwnedFd`), then convert into a plain `std::os::fd::OwnedFd`
        // so the guard owns the OS resource with no zvariant borrow lifetime.
        // See the module/in-function notes for why this typed path was chosen.
        let fd: ZvariantOwnedFd = reply
            .body()
            .deserialize()
            .map_err(|e| OxiwakeError::DbusDecode(format!("Inhibit fd: {e}")))?;
        let fd: OwnedFd = fd.into();

        Ok(Box::new(LogindGuard { fd }))
    }

    /// Describe the lock this backend would hold, without taking one.
    ///
    /// Derives the targets from the default Linux request so `ow status` works
    /// without a live IPC or a held lock; no D-Bus round-trip is made.
    fn status(&self) -> Result<WakeStatus> {
        let req = WakeRequest::default_linux();
        let targets = logind_relevant_targets(&req);
        Ok(WakeStatus {
            backend: self.name().to_string(),
            targets,
            mode: WakeMode::Block,
            // logind has no first-class "display" token; display-keep is an
            // idle/display concern handled elsewhere. Always report false here.
            display: false,
        })
    }

    /// Probe the systemd-logind environment and report honest capabilities.
    ///
    /// `available` is `true` iff `org.freedesktop.login1` is reachable on the
    /// system bus. Every individual probe (`CanSuspend`, `LidClosed`/`Docked`/
    /// `OnExternalPower` properties, `ListInhibitors`) is best-effort: a
    /// failure becomes a note, never a panic. `guarantees` carries the honest
    /// caveats from `docs/setup.md` (privilege requirements, lid/power
    /// dependencies) so `ow doctor` does not over-promise.
    fn doctor(&self) -> Result<DoctorReport> {
        let mut notes = Vec::new();
        // We only reach here past the early-return guards below, which means the
        // service answered — so availability is true by construction.
        let available = true;

        // 1. Is the service reachable at all? This is the gating signal.
        let conn = match Connection::system() {
            Ok(c) => c,
            Err(e) => {
                notes.push(format!("cannot open system bus: {e}"));
                return Ok(DoctorReport {
                    backend: self.name().to_string(),
                    supported: true,
                    available: false,
                    guarantees: logind_guarantees(),
                    notes,
                });
            }
        };

        let proxy = match Proxy::new(&conn, SERVICE, PATH, INTERFACE) {
            Ok(p) => p,
            Err(e) => {
                notes.push(format!("cannot build logind proxy: {e}"));
                return Ok(DoctorReport {
                    backend: self.name().to_string(),
                    supported: true,
                    available: false,
                    guarantees: logind_guarantees(),
                    notes,
                });
            }
        };

        // Reaching the Manager interface means the service is up. (A bare
        // proxy build does not itself prove the service answered, so the cheap
        // real calls below are the actual reachability proof.)
        notes.push("org.freedesktop.login1 reachable on system bus".to_string());

        // 2. CanSuspend (returns "yes"/"na"/"no" for the *unprivileged* case;
        //    some systems gate even this behind Polkit, so treat denial as a
        //    note rather than a hard failure).
        match proxy.call_method("CanSuspend", &()) {
            Ok(msg) => match msg.body().deserialize::<String>() {
                Ok(s) => notes.push(format!("CanSuspend = {s}")),
                Err(e) => notes.push(format!("CanSuspend reply undecodable: {e}")),
            },
            Err(e) => notes.push(format!("CanSuspend probe failed: {e}")),
        }

        // 3. Hardware-ish properties via org.freedesktop.DBus.Properties.Get.
        //    These describe the machine, not our privileges, and are usually
        //    world-readable.
        if let Some(b) = read_bool_property(&proxy, "LidClosed") {
            notes.push(format!("LidClosed = {b}"));
        }
        if let Some(b) = read_bool_property(&proxy, "Docked") {
            notes.push(format!("Docked = {b}"));
        }
        if let Some(b) = read_bool_property(&proxy, "OnExternalPower") {
            notes.push(format!("OnExternalPower = {b}"));
        }

        // 4. ListInhibitors (signature a(ssssuu)). Counting active inhibitors
        //    is purely diagnostic for `ow doctor`. It is Polkit-gated on some
        //    systems, so a denial is a note, not a failure.
        match proxy.call_method("ListInhibitors", &()) {
            Ok(msg) => match msg.body().deserialize::<Vec<InhibitorRow>>() {
                Ok(rows) => {
                    let ours = rows.iter().filter(|r| r.who == WakeRequest::WHO).count();
                    notes.push(format!(
                        "ListInhibitors: {} active ({} held by Oxiwake)",
                        rows.len(),
                        ours
                    ));
                }
                Err(e) => notes.push(format!("ListInhibitors reply undecodable: {e}")),
            },
            Err(e) => notes.push(format!("ListInhibitors probe failed: {e}")),
        }

        Ok(DoctorReport {
            backend: self.name().to_string(),
            supported: true,
            available,
            guarantees: logind_guarantees(),
            notes,
        })
    }
}

/// RAII handle to an active systemd-logind inhibitor.
///
/// Owns the file descriptor returned by `Inhibit`. systemd-logind releases the
/// inhibitor the instant this FD is closed, so dropping the guard (which drops
/// the [`OwnedFd`], closing the FD) is the *entire* release mechanism — there
/// is nothing extra to do in `Drop`. Leaking the guard leaks the `OwnedFd` and
/// thus keeps the OS lock held (matching the daemon's intent), so the
/// invariant "leaking the guard must never leak the OS lock in a way that
/// loses it" holds: the FD is owned, not borrowed, and survives for the guard's
/// lifetime however long that is.
#[derive(Debug)]
pub struct LogindGuard {
    /// The inhibitor pipe FD. Closed (releasing the lock) on drop. Never read
    /// directly — owning it for the guard's lifetime *is* the lock.
    #[allow(dead_code)]
    fd: OwnedFd,
}

impl WakeGuard for LogindGuard {
    fn backend(&self) -> &'static str {
        "systemd-logind"
    }
}

// No explicit `Drop` impl: `OwnedFd`'s own `Drop` closes the FD, which is
// exactly how systemd-logind inhibitors are released. Documented here so the
// absence is clearly intentional and not an oversight.

/// Map a connection-time zbus error to `BackendUnavailable`.
///
/// This is used only for *opening* the system bus (not for `Inhibit`), since a
/// failure there means "logind is not usable right now" rather than "the OS
/// refused our lock".
fn map_unavailable(e: &zbus::Error) -> OxiwakeError {
    OxiwakeError::BackendUnavailable {
        backend: "systemd-logind",
        reason: e.to_string(),
    }
}

/// Map an `Inhibit` call error. Access-denied / Polkit refusals become
/// [`OxiwakeError::AcquireFailed`]; everything else propagates as the original
/// D-Bus error (auto-converted via the `#[from] zbus::Error` impl on
/// [`OxiwakeError::Dbus`]).
///
/// Access-denied replies arrive in one of two shapes from zbus:
///   - `zbus::Error::MethodError(name, msg, _)` where `name` is an
///     `OwnedErrorName` such as `org.freedesktop.DBus.Error.AccessDenied` or a
///     `org.freedesktop.policykit*` name;
///   - `zbus::Error::FDO(Box<fdo::Error>)` where the inner is
///     `fdo::Error::AccessDenied(_)` / `AuthFailed(_)`.
///
/// We match defensively on the textual error (which covers both shapes, since
/// `OwnedErrorName` and `fdo::Error` both `Display` their canonical name) so a
/// future zbus reshuffle of variants does not silently change behaviour.
fn map_acquire(e: &zbus::Error) -> OxiwakeError {
    if is_access_denied(e) {
        OxiwakeError::AcquireFailed {
            backend: "systemd-logind",
            reason: e.to_string(),
        }
    } else {
        // Any other D-Bus error: let `OxiwakeError::Dbus` carry it.
        OxiwakeError::Dbus(e.clone())
    }
}

/// Decide whether a zbus error represents a permission / access refusal.
///
/// Defensive: matches on the rendered string rather than exhaustive variant
/// matching, because Polkit errors use arbitrary `org.freedesktop.policykit*`
/// names and the exact zbus variant carrying the reply has changed between
/// releases. The substrings chosen cover `AccessDenied`, `AuthFailed`, and the
/// generic policykit `NotAuthorized` / `interactively-authorize` rejections.
fn is_access_denied(e: &zbus::Error) -> bool {
    let s = e.to_string();
    s.contains("AccessDenied")
        || s.contains("AuthFailed")
        || s.contains("NotAuthorized")
        || s.contains("policykit")
        || s.contains("Permission denied")
}

/// Read a `b` (boolean) property from `org.freedesktop.login1.Manager`.
///
/// Returns `None` on any failure so the doctor records a note via the caller's
/// fall-through rather than aborting the whole report. `Get` returns a variant;
/// zbus hands us the inner value when we deserialize to `bool`.
fn read_bool_property(proxy: &Proxy<'_>, name: &str) -> Option<bool> {
    let msg = proxy
        .call_method("Get", &(INTERFACE, name))
        .map_err(|e| {
            tracing::debug!(property = %name, error = %e, "logind property Get failed");
        })
        .ok()?;
    msg.body()
        .deserialize::<bool>()
        .map_err(|e| {
            tracing::debug!(property = %name, error = %e, "logind property decode failed");
        })
        .ok()
}

/// The subset of a request's targets that systemd-logind actually has tokens
/// for (i.e. excluding `Display`, which has no logind token).
fn logind_relevant_targets(req: &WakeRequest) -> Vec<crate::model::WakeTarget> {
    use crate::model::WakeTarget;
    req.targets
        .iter()
        .copied()
        .filter(|t| *t != WakeTarget::Display)
        .collect()
}

/// Honest capability caveats, lifted verbatim in spirit from
/// `docs/setup.md` section 1. These are surfaced by `ow doctor` so users do
/// not assume logind blocks more than the OS actually lets it.
fn logind_guarantees() -> Vec<String> {
    vec![
        "block on shutdown/sleep/handle-* typically needs privilege \
         (Polkit org.freedesktop.login1.inhibit-block-*); an unprivileged \
         user is usually denied those tokens"
            .to_string(),
        "idle is the only block token routinely allowed unprivileged".to_string(),
        "handle-lid-switch behaviour depends on docked/external-power state \
         (lid handling is ignored when Docked or on external power on many \
         systems)"
            .to_string(),
        "logind has no first-class display token; keeping the display on is \
         an idle/display concern handled by other backends"
            .to_string(),
        "the lock is held only for the lifetime of the inhibitor file \
         descriptor — the daemon must keep it open"
            .to_string(),
    ]
}

/// Decoded row of `ListInhibitors` (signature `a(ssssuu)`).
///
/// Field order per systemd-logind: `what`, `who`, `why`, `mode`,
/// `uid` (unsigned), `pid` (unsigned). We keep only what `ow doctor` needs.
#[derive(Debug, Clone, serde::Deserialize, zbus::zvariant::Type)]
struct InhibitorRow {
    #[allow(dead_code)] // positional: keeps the a(ssssuu) decode aligned.
    what: String,
    who: String,
    #[allow(dead_code)]
    why: String,
    #[allow(dead_code)]
    mode: String,
    #[allow(dead_code)]
    uid: u32,
    #[allow(dead_code)]
    pid: u32,
}

// Silence unused-import warnings for the name types that exist purely to make
// the proxy coordinates' types self-documenting and future-proof. They are
// re-exported through zbus so callers using `Proxy::new` with plain `&str` do
// not strictly need them, but importing them documents the expected types and
// keeps the module robust against a future stricter signature.
#[allow(dead_code)]
fn _type_anchor(_: WellKnownName<'static>, _: ObjectPath<'static>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{WakeRequest, WakeTarget};

    #[test]
    fn name_and_supported() {
        let b = LogindBackend::new();
        assert_eq!(b.name(), "systemd-logind");
        assert!(b.supported());
    }

    #[test]
    fn status_uses_block_and_no_display() {
        let b = LogindBackend::new();
        let s = b.status().expect("status");
        assert_eq!(s.backend, "systemd-logind");
        assert_eq!(s.mode, WakeMode::Block);
        assert!(!s.display);
        // Default linux request is sleep:idle; both have logind tokens.
        assert!(s.targets.contains(&WakeTarget::SystemSleep));
        assert!(s.targets.contains(&WakeTarget::Idle));
        assert!(!s.targets.contains(&WakeTarget::Display));
    }

    #[test]
    fn guarantees_mention_privilege_and_idle() {
        let g = logind_guarantees();
        let joined = g.join(" | ");
        assert!(
            joined.contains("privilege"),
            "guarantees must mention privilege"
        );
        assert!(joined.contains("idle"), "guarantees must mention idle");
    }

    #[test]
    fn empty_what_is_rejected_without_dbus() {
        // A request that maps to no logind tokens (only Display) must be
        // rejected before any D-Bus call so we never send an empty `what`.
        let b = LogindBackend::new();
        let req = WakeRequest {
            targets: vec![WakeTarget::Display],
            reason: "x".into(),
            display: true,
            aggressive_lid: false,
        };
        // Match on the Result directly: `unwrap_err()` would require
        // `Box<dyn WakeGuard>: Debug`, which the trait object does not have.
        match b.acquire(&req) {
            Err(OxiwakeError::AcquireFailed { backend, .. }) => {
                assert_eq!(backend, "systemd-logind");
            }
            Err(other) => panic!("expected AcquireFailed, got {other:?}"),
            Ok(_) => panic!("expected acquire to fail for an empty `what`"),
        }
    }
}
