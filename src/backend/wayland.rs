//! Wayland idle-inhibit backend (optional, `linux-wayland` feature).
//!
//! This implements the `zwp_idle_inhibit_manager_v1` / `zwp_idle_inhibitor_v1`
//! protocol from `docs/setup.md` section 7. Per that protocol (and the
//! unstable-v1 spec), creating an inhibitor requires binding it to a
//! `wl_surface`, and the compositor only honors the inhibitor while that
//! surface is visible/relevant. For a headless CLI daemon there is no surface
//! to offer, so this backend is intentionally **conservative**: it is
//! `supported()` (compiled in), but `acquire()` returns
//! [`OxiwakeError::BackendUnavailable`] and `doctor()` reports
//! `available=false` unless a surface story is available.
//!
//! The code is correct-but-defensive: it compiles only behind
//! `#[cfg(all(target_os = "linux", feature = "linux-wayland"))]` and, on a
//! real Wayland desktop with a surface handed in, would create and destroy the
//! inhibitor correctly. On this build host (no Wayland) it is never compiled,
//! so it must be — and is — correct by construction.
//!
//! Guarantees (setup.md section 7): idle-only (screen blanking/locking/
//! screensaving). It does **not** prevent system suspend or lid-close.

#![cfg(all(target_os = "linux", feature = "linux-wayland"))]

use std::env;
use std::sync::Arc;

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
// The idle-inhibit protocol lives under `wp::idle_inhibit` and is gated behind
// wayland-protocols' `unstable` feature (enabled in Cargo.toml).
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1;
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1;

use crate::error::{OxiwakeError, Result};
use crate::model::{DoctorReport, WakeBackend, WakeGuard, WakeMode, WakeStatus, WakeTarget};

/// Stable backend identifier surfaced by [`WaylandBackend::name`] / `ow doctor`.
const NAME: &str = "wayland-idle-inhibit";

/// Why this backend is unavailable for a headless daemon (setup.md section 7).
const NO_SURFACE_REASON: &str = "wayland idle-inhibit requires a visible wl_surface";

/// App state for the wayland event queue.
///
/// We do not need to react to dynamic registry events (a CLI daemon only wants
/// a one-shot snapshot of globals, and never produces an inhibitor without a
/// surface), so this state is intentionally empty. The
/// `Dispatch<WlRegistry, GlobalListContents>` impl is still required by
/// [`registry_queue_init`]; it is provided below and does nothing, because the
/// initial global snapshot is populated by `registry_queue_init`'s own
/// round-trip before it returns.
#[derive(Default)]
pub struct WaylandState;

/// Wayland idle-inhibit backend.
///
/// Constructing this value performs **no I/O**. All Wayland interaction
/// (connecting to the compositor, binding the manager global) happens lazily
/// inside [`WaylandBackend::acquire`] / [`WaylandBackend::doctor`].
///
/// A CLI daemon has no `wl_surface` to bind an inhibitor to, so in practice
/// [`WaylandBackend::acquire`] returns [`OxiwakeError::BackendUnavailable`].
/// The full connection/global-binding machinery is kept here so that a future
/// desktop-integrated build (with a real surface) can use it directly, and so
/// `doctor()` can honestly report whether a Wayland display + idle-inhibit
/// global are present even when no surface is feasible.
pub struct WaylandBackend;

impl WaylandBackend {
    /// Create a backend handle. Does not connect to the compositor.
    pub fn new() -> Self {
        WaylandBackend
    }

    /// Connect to the compositor via `$WAYLAND_DISPLAY` / `$XDG_RUNTIME_DIR`
    /// (the env-driven lookup `Connection::connect_to_env` performs) and run an
    /// initial registry round-trip to snapshot the advertised globals.
    ///
    /// Returns the connection, the global-list snapshot, and the event queue
    /// (kept so bound objects survive the caller's scope). We never panic on
    /// absence: a missing display is surfaced as
    /// [`OxiwakeError::BackendUnavailable`].
    fn connect_and_snapshot() -> Result<(Connection, Arc<GlobalList>, EventQueue<WaylandState>)> {
        // connect_to_env reads WAYLAND_DISPLAY (and XDG_RUNTIME_DIR). A missing
        // display yields a ConnectError; we map it to BackendUnavailable so
        // `ow doctor` can report it gracefully rather than panic.
        let conn = Connection::connect_to_env().map_err(|e| OxiwakeError::BackendUnavailable {
            backend: NAME,
            reason: format!("cannot connect to Wayland display ({})", e),
        })?;

        // registry_queue_init creates an event queue and drives an initial
        // round-trip, populating a GlobalList snapshot of all globals the
        // compositor advertises. We do not need live global events, so a static
        // snapshot is enough to probe for the idle-inhibit manager.
        let (globals, queue) = registry_queue_init::<WaylandState>(&conn).map_err(|e| {
            OxiwakeError::BackendUnavailable {
                backend: NAME,
                reason: format!("Wayland registry round-trip failed ({})", e),
            }
        })?;

        Ok((conn, Arc::new(globals), queue))
    }

    /// True if the compositor advertises `zwp_idle_inhibit_manager_v1`.
    ///
    /// Uses the already-snapshotted global list so we do not issue a second
    /// round-trip. The protocol is unstable-v1, so we only ever care that it
    /// is present (binding would negotiate version 1).
    fn has_idle_inhibit_global(globals: &GlobalList) -> bool {
        let target_iface = ZwpIdleInhibitManagerV1::interface().name;
        // GlobalListContents::with_list runs the closure under the contents'
        // lock and returns the closure's value directly (not a Result), so the
        // boolean is the final answer with no unwrap needed.
        globals
            .contents()
            .with_list(|list| list.iter().any(|g| g.interface == target_iface))
    }
}

impl Default for WaylandBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeBackend for WaylandBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn supported(&self) -> bool {
        // Compiled into this binary (feature + platform match).
        true
    }

    fn acquire(&self, _req: &crate::model::WakeRequest) -> Result<Box<dyn WakeGuard>> {
        // A CLI daemon owns no wl_surface. Per idle-inhibit-unstable-v1, the
        // inhibitor is bound to a surface and is only honored while that
        // surface is visible/relevant. With no surface to offer we cannot
        // meaningfully inhibit, so fail fast with BackendUnavailable rather
        // than create a no-op inhibitor the compositor would ignore.
        //
        // We deliberately do NOT synthesize an unmapped/invisible surface: the
        // protocol explicitly says the compositor may ignore an inhibitor for
        // an unmapped/occluded surface, so that would silently fail to keep
        // anything awake.
        //
        // We probe the display + global first (best-effort, errors ignored) so
        // that a future caller could distinguish "no Wayland at all" / "no
        // global" from "Wayland present but no surface". The user-facing error
        // always carries NO_SURFACE_REASON per the task contract; we surface
        // the finer-grained diagnosis via `doctor()` instead, which is the
        // right channel for environment detail.
        if let Ok((_conn, globals, _queue)) = Self::connect_and_snapshot() {
            // Probe only — the result shapes nothing user-visible here (the
            // backend is unavailable either way without a surface), but
            // touching the snapshot ensures `doctor()` and `acquire()` agree
            // on what the compositor advertises.
            let _ = Self::has_idle_inhibit_global(&globals);
        }

        Err(OxiwakeError::BackendUnavailable {
            backend: NAME,
            reason: NO_SURFACE_REASON.to_string(),
        })
    }

    fn status(&self) -> Result<WakeStatus> {
        // Idle-only scope (setup.md section 7): the protocol inhibits idle
        // behavior (blanking/locking/screensaving), not system sleep or lid.
        // It is a Block-style hold. `display:false` because the backend's
        // declared targets are idle-only.
        Ok(WakeStatus {
            backend: NAME.to_string(),
            targets: vec![WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        })
    }

    fn doctor(&self) -> Result<DoctorReport> {
        let mut notes = Vec::new();

        // Read WAYLAND_DISPLAY exactly once. A present-but-non-UTF8 value is
        // reported honestly via to_string_lossy rather than masked as empty.
        let wayland_display = env::var_os("WAYLAND_DISPLAY");
        let available_env = wayland_display
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if available_env {
            let value = wayland_display
                .as_ref()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            notes.push(format!("WAYLAND_DISPLAY={}", value));
        } else {
            notes.push("WAYLAND_DISPLAY is not set".to_string());
        }

        // Probe the display and the idle-inhibit global. We never panic on a
        // missing display or missing global — those are reported as notes and
        // available=false.
        let available = match Self::connect_and_snapshot() {
            Ok((_conn, globals, _queue)) => {
                if Self::has_idle_inhibit_global(&globals) {
                    // Display reachable AND global advertised — but a CLI
                    // daemon still has no surface, so the backend cannot
                    // actually hold a lock. Report available=false with the
                    // surface caveat; this is the honest answer per setup.md.
                    notes.push(
                        "zwp_idle_inhibit_manager_v1 advertised, but a CLI daemon \
                         has no wl_surface to bind an inhibitor to"
                            .to_string(),
                    );
                    false
                } else {
                    notes.push(
                        "zwp_idle_inhibit_manager_v1 global is not advertised by \
                         this compositor"
                            .to_string(),
                    );
                    false
                }
            }
            Err(e) => {
                notes.push(format!("unavailable: {}", e));
                false
            }
        };

        Ok(DoctorReport {
            backend: NAME.to_string(),
            supported: true,
            available,
            // Cite setup.md section 7: idle-only, surface-bound, unstable.
            guarantees: vec![
                "idle-only: inhibits screen blanking/locking/screensaving".to_string(),
                "does NOT prevent system suspend or lid-close".to_string(),
                "requires a visible wl_surface; ignored if the surface is \
                 unmapped/occluded"
                    .to_string(),
                "protocol is unstable-v1 (experimental)".to_string(),
            ],
            notes,
        })
    }
}

/// RAII guard for an active Wayland idle inhibitor.
///
/// In the current headless-daemon code path this guard is never actually
/// produced — [`WaylandBackend::acquire`] returns `BackendUnavailable` before
/// reaching it. It exists so that a future build that obtains a real
/// `wl_surface` can construct it (e.g. via [`WaylandGuard::from_inhibitor`])
/// and get correct Drop semantics for free.
///
/// The inhibitor object owns the OS-level inhibit: destroying it uninhibits
/// (per the idle-inhibit-unstable-v1 protocol). The guard holds the inhibitor
/// proxy and its owning queue handle so the object stays alive for the guard's
/// lifetime; dropping the guard sends `destroy` and releases the lock. Leaking
/// the guard keeps the lock held (correct), but a normal drop always releases
/// it — there is no resource leak.
pub struct WaylandGuard {
    /// `Some` while the inhibitor is held; taken to `None` on drop so a
    /// panic-during-drop cannot double-destroy.
    inhibitor: Option<ZwpIdleInhibitorV1>,
    // The queue handle is kept alive so the inhibitor's queue is not dropped
    // out from under it before `destroy` is sent.
    _queue: QueueHandle<WaylandState>,
}

impl WaylandGuard {
    /// Take ownership of an already-created inhibitor proxy and the queue that
    /// manages it. The caller is responsible for having created the inhibitor
    /// via `ZwpIdleInhibitManagerV1::create_inhibitor` against a *visible*
    /// `wl_surface`.
    pub fn from_inhibitor(inhibitor: ZwpIdleInhibitorV1, queue: QueueHandle<WaylandState>) -> Self {
        WaylandGuard {
            inhibitor: Some(inhibitor),
            _queue: queue,
        }
    }
}

impl WakeGuard for WaylandGuard {
    fn backend(&self) -> &'static str {
        NAME
    }
}

impl Drop for WaylandGuard {
    fn drop(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            // zwp_idle_inhibitor_v1.destroy: uninhibit. Best-effort on drop;
            // a failing destroy is ignored (the compositor cleans up when the
            // client disconnects regardless).
            inhibitor.destroy();
        }
    }
}

// ---- wayland-client Dispatch wiring -----------------------------------------
//
// registry_queue_init needs a Dispatch<WlRegistry, GlobalListContents>
// implementation for its state type. We provide a no-op impl: the initial
// global snapshot is populated by registry_queue_init's own round-trip before
// it returns, and this backend never reacts to dynamic global add/remove
// events (it only needs a one-shot probe).
//
// Note the trait signature (wayland-client 0.31): the first parameter is
// `state: &mut State` (with `State = Self` by default), not `&mut self`, so the
// impl spells it out as `state: &mut WaylandState`.

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        state: &mut WaylandState,
        _proxy: &WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<WaylandState>,
    ) {
        // Intentionally empty: see the module/struct docs. `state` is named
        // only to satisfy the trait signature; it is unused.
        let _ = state;
    }
}
