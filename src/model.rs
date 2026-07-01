//! Core domain model for Oxiwake.
//!
//! These types are the **shared contract** every platform backend, the daemon,
//! the IPC layer, and the CLI all agree on. They are deliberately small and
//! backend-agnostic: a backend (`WakeBackend`) turns a `WakeRequest` into an
//! active lock (`WakeGuard`) and reports `WakeStatus` / `DoctorReport`.
//!
//! See `docs/setup.md` for the design rationale — Oxiwake thinks in terms of
//! *backends*, not *distros*.

use crate::error::OxiwakeError;

/// A power state we want to keep from triggering.
///
/// Maps onto systemd-logind's colon-separated `what` string and onto the Win32
/// `POWER_REQUEST_TYPE` set; backends translate the targets they understand and
/// ignore the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WakeTarget {
    /// Prevent system suspend/hibernate (`sleep` on logind, `PowerRequestSystemRequired` on Windows).
    SystemSleep,
    /// Prevent idle blanking/locking (`idle` on logind, idle timer reset on Windows).
    Idle,
    /// Keep the display on (`PowerRequestDisplayRequired` on Windows; mostly honored via idle on Linux).
    Display,
    /// Prevent a lid-close from sleeping (`handle-lid-switch` on logind).
    LidSwitch,
    /// Prevent shutdown (`shutdown` on logind; typically needs privilege).
    Shutdown,
}

impl WakeTarget {
    /// The systemd-logind `what` token for this target, if any.
    pub fn logind_token(self) -> Option<&'static str> {
        match self {
            WakeTarget::SystemSleep => Some("sleep"),
            WakeTarget::Idle => Some("idle"),
            WakeTarget::LidSwitch => Some("handle-lid-switch"),
            WakeTarget::Shutdown => Some("shutdown"),
            // Display has no direct logind token; it is an idle/display concern.
            WakeTarget::Display => None,
        }
    }
}

/// How aggressively to hold the lock. Mirrors systemd-logind's `--mode` values.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum WakeMode {
    /// Take the lock in blocking mode: the action is delayed until the lock is released.
    #[default]
    Block,
    /// Take the delay lock: the action proceeds after a bounded delay even if we hold the lock.
    Delay,
    /// Block-weak: a weaker form of `Block` (overridden by `block` inhibitors).
    BlockWeak,
}

impl WakeMode {
    /// The systemd-logind `mode` string for this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            WakeMode::Block => "block",
            WakeMode::Delay => "delay",
            WakeMode::BlockWeak => "block-weak",
        }
    }
}

/// A user's intent, translated into a backend-agnostic request.
///
/// Backends pick the targets they can honor from `targets`; the daemon keeps
/// the chosen backend's guard alive for the lifetime of the process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WakeRequest {
    /// Which power states to inhibit. Order is priority, not significance.
    pub targets: Vec<WakeTarget>,
    /// Human-readable reason handed to the OS (shown by `systemd-inhibit --list`, etc.).
    pub reason: String,
    /// Additionally keep the display awake where the backend supports it.
    pub display: bool,
    /// Be aggressive about lid-close handling (adds `handle-lid-switch`; often needs privilege).
    pub aggressive_lid: bool,
}

impl WakeRequest {
    /// `who`/`why` identity handed to the OS.
    pub const WHO: &'static str = "Oxiwake";

    /// The good default from the design doc: `sleep:idle`, block, no display, no aggressive lid.
    pub fn default_linux() -> Self {
        WakeRequest {
            targets: vec![WakeTarget::SystemSleep, WakeTarget::Idle],
            reason: "Oxiwake wake lock enabled".to_string(),
            display: false,
            aggressive_lid: false,
        }
    }

    /// Windows default: keep the system required (and execution required), no display.
    pub fn default_windows() -> Self {
        WakeRequest {
            targets: vec![WakeTarget::SystemSleep],
            reason: "Oxiwake wake lock enabled".to_string(),
            display: false,
            aggressive_lid: false,
        }
    }

    /// Build the colon-separated `what` string for systemd-logind from `targets`
    /// (plus `handle-lid-switch` when `aggressive_lid` is set), in priority order
    /// and de-duplicated.
    pub fn logind_what(&self) -> String {
        let mut out: Vec<&str> = Vec::new();
        let push = |out: &mut Vec<&'static str>, t: WakeTarget| {
            if let Some(tok) = t.logind_token() {
                if !out.contains(&tok) {
                    out.push(tok);
                }
            }
        };
        for &t in &self.targets {
            push(&mut out, t);
        }
        if self.aggressive_lid && !out.contains(&"handle-lid-switch") {
            out.push("handle-lid-switch");
        }
        out.join(":")
    }
}

/// Snapshot of the lock a backend currently holds. Returned by `status()` and
/// serialized into `state.json` so `ow status` can report without a live IPC.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WakeStatus {
    /// Name of the backend holding the lock.
    pub backend: String,
    /// The targets the backend actually honored (subset of the request).
    pub targets: Vec<WakeTarget>,
    /// The mode the lock was taken in.
    pub mode: WakeMode,
    /// Whether the display is also being kept on.
    pub display: bool,
}

/// One line of a per-backend doctor report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DoctorReport {
    /// Backend name.
    pub backend: String,
    /// Compiled into this binary (feature flag on / platform on).
    pub supported: bool,
    /// Reachable right now (D-Bus service up, display present, etc.).
    pub available: bool,
    /// Honest caveats: what this backend *cannot* guarantee. Surfaced by `ow doctor`.
    pub guarantees: Vec<String>,
    /// Free-form diagnostic notes.
    pub notes: Vec<String>,
}

impl DoctorReport {
    /// A blank report for a backend that is not compiled in at all.
    pub fn not_compiled(name: &'static str) -> Self {
        DoctorReport {
            backend: name.to_string(),
            supported: false,
            available: false,
            guarantees: vec!["not compiled into this build".to_string()],
            notes: vec![],
        }
    }
}

/// A platform wake-lock backend.
///
/// Implementations: systemd-logind, XDG portal, ScreenSaver, GNOME, KDE
/// (Linux), and Win32 power requests (Windows). `acquire` returns an RAII
/// `WakeGuard`; dropping the guard releases the lock (closes the logind FD on
/// Linux, calls `PowerClearRequest` + `CloseHandle` on Windows).
///
/// Implementations must be `Send` so the daemon can hold them across threads.
pub trait WakeBackend: Send {
    /// Stable identifier, e.g. `"systemd-logind"`.
    fn name(&self) -> &'static str;

    /// `true` if this backend is compiled into the binary (feature/platform).
    fn supported(&self) -> bool;

    /// Take the lock. The returned guard owns the OS resource for its lifetime.
    fn acquire(&self, req: &WakeRequest) -> Result<Box<dyn WakeGuard>, OxiwakeError>;

    /// Describe the lock this backend would / does hold, without necessarily
    /// taking one. Used by `ow doctor` and idle `ow status`.
    fn status(&self) -> Result<WakeStatus, OxiwakeError>;

    /// Probe the backend's environment and report honest capabilities + caveats.
    fn doctor(&self) -> Result<DoctorReport, OxiwakeError>;
}

/// RAII handle to an active wake lock.
///
/// The concrete `Drop` impl releases the OS resource. Because systemd-logind
/// releases its inhibitor the instant the returned FD is closed, tying lock
/// ownership to the guard's lifetime is the correct, leak-safe design.
pub trait WakeGuard: Send {
    /// Name of the backend that produced this guard.
    fn backend(&self) -> &'static str;
}
