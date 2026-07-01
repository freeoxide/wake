//! Backend registry — which wake-lock backends are compiled into this build,
//! and the priority order used to pick one.
//!
//! Oxiwake thinks in *backends*, not *distros* (see `docs/setup.md`). Each
//! backend is a small module behind a platform/feature gate; this module wires
//! them into a single ordered list and exposes the three call sites the rest of
//! the crate needs:
//!
//!   - [`supported_backends`] — every compiled-in backend, in priority order;
//!   - [`pick`] — the strongest backend compiled into this build;
//!   - [`doctor_all`] — [`WakeBackend::doctor`] over every compiled-in backend.
//!
//! Priority order is the "Linux auto backend" / "Windows auto backend" list
//! from `docs/setup.md`: on Linux `systemd-logind` is the real system-level
//! mechanism and comes first; the desktop/session/display backends follow as
//! progressively weaker fallbacks. On Windows the single Win32 power backend is
//! the only entry.

use crate::error::{OxiwakeError, Result};
use crate::model::{DoctorReport, WakeBackend, WakeRequest};

// --- per-platform / per-feature module declarations -------------------------
//
// The D-Bus backends (logind, portal, gnome, kde, screensaver) all `use zbus`,
// which is a Linux-only optional dependency, so they MUST be gated to Linux +
// their feature. x11/wayland additionally gate themselves with an inner
// `#![cfg]`; that is redundant with the gate here but harmless. windows is
// Windows-only.

#[cfg(all(target_os = "linux", feature = "linux-gnome"))]
pub mod gnome;
#[cfg(all(target_os = "linux", feature = "linux-kde"))]
pub mod kde;
#[cfg(all(target_os = "linux", feature = "linux-logind"))]
pub mod logind;
#[cfg(all(target_os = "linux", feature = "linux-portal"))]
pub mod portal;
#[cfg(all(target_os = "linux", feature = "linux-screensaver"))]
pub mod screensaver;
#[cfg(all(target_os = "linux", feature = "linux-wayland"))]
pub mod wayland;
#[cfg(windows)]
pub mod windows;
#[cfg(all(target_os = "linux", feature = "linux-x11"))]
pub mod x11;

/// Every backend compiled into this build, in `docs/setup.md` priority order.
///
/// Constructing each backend is cheap (no I/O — see each `XBackend::new`), so
/// building the full list is fine even for `ow doctor`, which then probes each.
#[allow(clippy::vec_init_then_push)]
pub fn supported_backends() -> Vec<Box<dyn WakeBackend>> {
    // The entries are individually feature-gated, so a `vec![]` literal does
    // not compose with `#[cfg]` — we build the list with conditional pushes.
    // `mut` is itself gated: when no backend feature is enabled (e.g.
    // `--no-default-features`) every push is cfg-ed out and an unconditional
    // `mut` would trip clippy's `unused_mut`.
    #[cfg(any(
        all(target_os = "linux", feature = "linux-logind"),
        all(target_os = "linux", feature = "linux-portal"),
        all(target_os = "linux", feature = "linux-gnome"),
        all(target_os = "linux", feature = "linux-kde"),
        all(target_os = "linux", feature = "linux-screensaver"),
        all(target_os = "linux", feature = "linux-x11"),
        all(target_os = "linux", feature = "linux-wayland"),
        windows
    ))]
    let mut v: Vec<Box<dyn WakeBackend>> = Vec::new();
    #[cfg(not(any(
        all(target_os = "linux", feature = "linux-logind"),
        all(target_os = "linux", feature = "linux-portal"),
        all(target_os = "linux", feature = "linux-gnome"),
        all(target_os = "linux", feature = "linux-kde"),
        all(target_os = "linux", feature = "linux-screensaver"),
        all(target_os = "linux", feature = "linux-x11"),
        all(target_os = "linux", feature = "linux-wayland"),
        windows
    )))]
    let v: Vec<Box<dyn WakeBackend>> = Vec::new();

    #[cfg(all(target_os = "linux", feature = "linux-logind"))]
    v.push(Box::new(logind::LogindBackend::new()));
    #[cfg(all(target_os = "linux", feature = "linux-portal"))]
    v.push(Box::new(portal::PortalBackend::new()));
    #[cfg(all(target_os = "linux", feature = "linux-gnome"))]
    v.push(Box::new(gnome::GnomeBackend::new()));
    #[cfg(all(target_os = "linux", feature = "linux-kde"))]
    v.push(Box::new(kde::KdeBackend::new()));
    #[cfg(all(target_os = "linux", feature = "linux-screensaver"))]
    v.push(Box::new(screensaver::ScreenSaverBackend::new()));
    #[cfg(all(target_os = "linux", feature = "linux-x11"))]
    v.push(Box::new(x11::X11Backend::new()));
    #[cfg(all(target_os = "linux", feature = "linux-wayland"))]
    v.push(Box::new(wayland::WaylandBackend::new()));
    #[cfg(windows)]
    v.push(Box::new(windows::Win32PowerBackend::new()));

    v
}

/// The strongest backend compiled into this build.
///
/// This returns the first entry of [`supported_backends`]; it does *not* probe
/// availability (that happens at [`WakeBackend::acquire`] time, which maps an
/// unreachable service to [`OxiwakeError::BackendUnavailable`]). Single-try
/// selection matches the phased plan in `docs/setup.md` (ship the primary
/// backend first; automatic fallback across the priority list is a later
/// phase).
pub fn pick(_req: &WakeRequest) -> Result<Box<dyn WakeBackend>> {
    supported_backends()
        .into_iter()
        .next()
        .ok_or(OxiwakeError::BackendUnavailable {
            backend: "none",
            reason: "no wake backend is compiled into this build (see `ow doctor`)".to_string(),
        })
}

/// Run [`WakeBackend::doctor`] over every compiled-in backend.
///
/// A backend whose `doctor()` itself errors is dropped (the rest still report),
/// so one broken probe never blanks out the whole table.
pub fn doctor_all() -> Vec<DoctorReport> {
    supported_backends()
        .into_iter()
        .filter_map(|b| b.doctor().ok())
        .collect()
}
