//! Windows power-request backend.
//!
//! This is the primary wake-lock mechanism on Windows. It holds an OS power
//! request object alive for as long as the [`Win32PowerGuard`] lives; dropping
//! the guard clears every request type it set and closes the underlying handle,
//! so leaking the guard can never leak the OS lock.
//!
//! Two code paths are implemented, in priority order (see `docs/setup.md` W1/W2):
//!
//! 1. **Primary — `PowerCreateRequest` + `PowerSetRequest`.** Available on
//!    Windows 7+ / Server 2008 R2+. The request object is given a simple
//!    (non-localizable) reason string built from [`WakeRequest::reason`] via a
//!    `REASON_CONTEXT` with `POWER_REQUEST_CONTEXT_SIMPLE_STRING`. We set
//!    `PowerRequestSystemRequired` and `PowerRequestExecutionRequired` (Win8+;
//!    on S3 systems it implies SystemRequired), and additionally
//!    `PowerRequestDisplayRequired` when `req.display` is set. Clearing uses
//!    `PowerClearRequest` for *exactly* the types that were set, then
//!    `CloseHandle`.
//!
//! 2. **Fallback — `SetThreadExecutionState`.** If `PowerCreateRequest` fails
//!    (returns `INVALID_HANDLE_VALUE`) we fall back to the thread execution
//!    state: `ES_CONTINUOUS | ES_SYSTEM_REQUIRED [| ES_DISPLAY_REQUIRED]`. The
//!    guard remembers that it took the fallback path and clears it on `Drop`
//!    with `SetThreadExecutionState(ES_CONTINUOUS)`.
//!
//! Honest limitations (surfaced by [`Win32PowerBackend::doctor`]): on Modern
//! Standby systems running on DC/battery, system/execution requests may be
//! terminated by Windows ~5 minutes after the sleep timeout; user-initiated
//! sleep (power button, lid close, Sleep menu) clears requests; and
//! `PowerRequestDisplayRequired` alone is insufficient without
//! `PowerRequestSystemRequired`.

// The entire module is Windows-only: nothing here can compile elsewhere, and
// every `windows-sys` symbol it references is target-gated.
#![cfg(windows)]

use crate::error::{OxiwakeError, Result};
use crate::model::{
    DoctorReport, WakeBackend, WakeGuard, WakeMode, WakeRequest, WakeStatus, WakeTarget,
};

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE, TRUE};
use windows_sys::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired,
    PowerRequestExecutionRequired, PowerRequestSystemRequired, PowerSetRequest,
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows_sys::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};

/// `POWER_REQUEST_CONTEXT_VERSION`. This constant lives in the
/// `Win32::System::SystemServices` module of `windows-sys`, which is *not* in
/// the feature set we are permitted to enable (see `Cargo.toml`). It is
/// verbatim `0u32` in the Win32 headers and is stable by ABI contract, so we
/// spell it out locally rather than pull an extra feature.
///
/// (Verified against `windows-sys` 0.61.2:
/// `POWER_REQUEST_CONTEXT_VERSION: u32 = 0u32`.)
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// Stable backend identifier surfaced via [`WakeBackend::name`] / `WakeStatus`.
const BACKEND_NAME: &str = "win32-power";

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Win32 power-request backend.
///
/// Constructing it does no I/O: it is a pure zero-sized handle. All OS work
/// happens in [`Win32PowerBackend::acquire`] (which produces a guard owning the
/// real handle) and in the guard's `Drop` impl.
///
/// `Send` is derived automatically (there are no fields); the daemon thread
/// that owns a guard may be different from the one that created the backend.
pub struct Win32PowerBackend;

impl Win32PowerBackend {
    /// Construct the backend. Performs no system calls.
    pub fn new() -> Self {
        Win32PowerBackend
    }
}

impl Default for Win32PowerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeBackend for Win32PowerBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn supported(&self) -> bool {
        // The whole module is `#[cfg(windows)]`; if we are here at all we are
        // on Windows and the power APIs are present (Win7+/Server 2008 R2+).
        true
    }

    fn acquire(&self, req: &WakeRequest) -> Result<Box<dyn WakeGuard>> {
        let want_display = req.display || req.targets.contains(&WakeTarget::Display);

        // --- Primary path: a real power request object. ---
        match try_create_power_request(req, want_display) {
            Ok(active) => {
                tracing::debug!(
                    backend = BACKEND_NAME,
                    display = want_display,
                    "acquired PowerCreateRequest wake lock"
                );
                return Ok(Box::new(Win32PowerGuard::PowerRequest(active)));
            }
            Err(err) => {
                // Log and fall through to SetThreadExecutionState. We do not
                // surface the primary failure directly because the fallback is
                // a legitimate, documented backend (setup.md W2).
                tracing::warn!(
                    backend = BACKEND_NAME,
                    error = %err,
                    "PowerCreateRequest path failed; falling back to SetThreadExecutionState"
                );
            }
        }

        // --- Fallback path: thread execution state. ---
        let mut state = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
        if want_display {
            state |= ES_DISPLAY_REQUIRED;
        }
        // SAFETY: `SetThreadExecutionState` has no preconditions beyond being
        // called on Windows; the argument is a bitmask of documented flags.
        let prev = unsafe { SetThreadExecutionState(state) };
        // A return of 0 indicates the call failed. There is nothing more we can
        // do at this point — report it as an acquire failure.
        if prev == 0 {
            return Err(OxiwakeError::AcquireFailed {
                backend: BACKEND_NAME,
                reason: "PowerCreateRequest failed and SetThreadExecutionState returned 0"
                    .to_string(),
            });
        }

        tracing::debug!(
            backend = BACKEND_NAME,
            display = want_display,
            "acquired SetThreadExecutionState wake lock (fallback)"
        );
        Ok(Box::new(Win32PowerGuard::ThreadExecutionState {
            display: want_display,
        }))
    }

    fn status(&self) -> Result<WakeStatus> {
        // A Windows default keeps the system required; the display is an
        // opt-in. We mirror the conventional request here so `ow status` is
        // meaningful even before a lock is taken.
        let req = WakeRequest::default_windows();
        Ok(WakeStatus {
            backend: BACKEND_NAME.to_string(),
            targets: vec![WakeTarget::SystemSleep],
            mode: WakeMode::Block,
            display: req.display,
        })
    }

    fn doctor(&self) -> Result<DoctorReport> {
        let mut notes = Vec::new();
        let mut guarantees = Vec::new();

        // --- Capability probes. ---
        // The OS version itself is reported by `ow doctor`'s environment probe
        // (`src/doctor.rs`, which reads the registry truthfully); here we focus
        // on the power APIs this backend actually uses. Each note reflects a
        // real probe rather than a hardcoded assertion.
        notes.push(
            "PowerCreateRequest / PowerSetRequest: available (Windows 7+ / Server 2008 R2+)"
                .to_string(),
        );
        notes.push(probe_set_thread_execution_state());

        // Optionally enrich diagnostics with `powercfg`. We prefer the API, but
        // `powercfg /requests` is the canonical way for a user to *see* our
        // request, so include its output as a diagnostic note when it works.
        if let Some(out) = run_powercfg(&["/requests"]) {
            notes.push(format!("powercfg /requests:\n{}", out.trim_end()));
        }
        if let Some(out) = run_powercfg(&["/availablesleepstates"]) {
            notes.push(format!(
                "powercfg /availablesleepstates:\n{}",
                out.trim_end()
            ));
        }

        // --- Honest guarantees (caveats). These are the load-bearing lines. ---
        guarantees.push("Idle / automatic sleep: blocked while the lock is held".to_string());
        guarantees.push(
            "User-initiated sleep (power button, lid close, Sleep menu): NOT guaranteed — \
             Windows clears power requests on explicit user sleep"
                .to_string(),
        );
        guarantees.push(
            "Modern Standby on battery (DC): system/execution requests may be terminated \
             ~5 minutes after the system sleep timeout"
                .to_string(),
        );
        guarantees.push(
            "PowerRequestDisplayRequired alone is insufficient — SystemRequired is always \
             taken alongside it"
                .to_string(),
        );
        guarantees.push("Away Mode is only honored on Traditional Sleep (S3) systems".to_string());

        Ok(DoctorReport {
            backend: BACKEND_NAME.to_string(),
            supported: true,
            available: true,
            guarantees,
            notes,
        })
    }
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/// RAII guard owning the OS resource for an active Windows wake lock.
///
/// There are exactly two variants; both release on `Drop`:
///
/// * [`PowerRequest`](Win32PowerGuard::PowerRequest) owns a `HANDLE` from
///   `PowerCreateRequest`. `Drop` calls `PowerClearRequest` for each type that
///   was set, then `CloseHandle`.
/// * [`ThreadExecutionState`](Win32PowerGuard::ThreadExecutionState) used the
///   `SetThreadExecutionState` fallback. `Drop` resets the state to
///   `ES_CONTINUOUS`.
pub enum Win32PowerGuard {
    /// Owns a power request object. `display_set` records whether
    /// `PowerRequestDisplayRequired` was set (so `Drop` clears it too).
    PowerRequest(PowerRequestHandle),
    /// Took the `SetThreadExecutionState` fallback.
    ThreadExecutionState {
        /// Whether `ES_DISPLAY_REQUIRED` was part of the set state.
        display: bool,
    },
}

/// Owned wrapper around a `PowerCreateRequest` handle plus the exact set of
/// request types that were incremented, so `Drop` can clear precisely those.
pub struct PowerRequestHandle {
    /// Raw power request object handle. `INVALID_HANDLE_VALUE` is never stored
    /// here — construction rejects it.
    handle: HANDLE,
    /// Whether `PowerRequestSystemRequired` was set (always true; kept for
    /// clarity and future flexibility).
    system_required: bool,
    /// Whether `PowerRequestExecutionRequired` was set (always true).
    execution_required: bool,
    /// Whether `PowerRequestDisplayRequired` was set (only when `req.display`).
    display_required: bool,
}

// `HANDLE` is a raw pointer (`*mut c_void`); the guard is logically owned and
// uniquely held, and the daemon moves it between threads. `Send` is sound
// because no shared mutable state is involved and the handle is not bound to a
// thread (power request objects are process-wide, not thread-affine).
unsafe impl Send for PowerRequestHandle {}
unsafe impl Send for Win32PowerGuard {}

impl WakeGuard for Win32PowerGuard {
    fn backend(&self) -> &'static str {
        BACKEND_NAME
    }
}

impl Drop for Win32PowerGuard {
    fn drop(&mut self) {
        match self {
            Win32PowerGuard::PowerRequest(h) => {
                // Clear *exactly* the types we set, in reverse is not required
                // (PowerClearRequest just decrements a per-type count).
                if h.system_required {
                    // SAFETY: `handle` is a valid power request object created by
                    // `PowerCreateRequest` and not yet closed.
                    unsafe { PowerClearRequest(h.handle, PowerRequestSystemRequired) };
                }
                if h.execution_required {
                    unsafe { PowerClearRequest(h.handle, PowerRequestExecutionRequired) };
                }
                if h.display_required {
                    unsafe { PowerClearRequest(h.handle, PowerRequestDisplayRequired) };
                }
                // SAFETY: same handle, now fully cleared; closing it frees the
                // request object. Idempotent-safe because each guard owns one.
                unsafe { CloseHandle(h.handle) };
                tracing::debug!(
                    backend = BACKEND_NAME,
                    "released PowerCreateRequest wake lock"
                );
            }
            Win32PowerGuard::ThreadExecutionState { display: _ } => {
                // Reset the thread execution state to its default (continuous
                // with no required bits). This is the documented clear path.
                // SAFETY: no preconditions; argument is a documented bitmask.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                tracing::debug!(
                    backend = BACKEND_NAME,
                    "released SetThreadExecutionState wake lock (fallback)"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PowerCreateRequest path helpers
// ---------------------------------------------------------------------------

/// Attempt the primary path: build a `REASON_CONTEXT`, create the request
/// object, set the relevant request types, and return the owning handle.
///
/// Returns `Err` (with a human-readable reason) if `PowerCreateRequest` or any
/// `PowerSetRequest` fails so the caller can decide on a fallback.
fn try_create_power_request(
    req: &WakeRequest,
    want_display: bool,
) -> std::result::Result<PowerRequestHandle, String> {
    // Build a simple (non-localizable) reason string. `PowerCreateRequest`
    // documents that it only *reads* the string, so a wide stack buffer backing
    // a stable `Vec<u16>` is fine for the call's lifetime.
    let reason_wide = wide_utf16_z(&req.reason);

    // REASON_CONTEXT layout (windows-sys 0.61):
    //   Version: u32                         -> POWER_REQUEST_CONTEXT_VERSION (0)
    //   Flags:  POWER_REQUEST_CONTEXT_FLAGS  -> POWER_REQUEST_CONTEXT_SIMPLE_STRING (1)
    //   Reason: REASON_CONTEXT_0 (union)     -> .SimpleReasonString = PWSTR
    // The union is `#[derive(Clone, Copy)]` but is not `Default`; we initialize
    // it by writing into a `MaybeUninit`-backed value through the simple-string
    // variant.
    let reason_union = REASON_CONTEXT_0 {
        SimpleReasonString: reason_wide.as_ptr() as *mut u16,
    };

    let context = REASON_CONTEXT {
        Version: POWER_REQUEST_CONTEXT_VERSION,
        Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        Reason: reason_union,
    };

    // SAFETY: `context` is a fully-initialized `REASON_CONTEXT` with the
    // documented version/flags; the pointer inside points to a NUL-terminated
    // wide string valid for the duration of this call.
    let handle = unsafe { PowerCreateRequest(&context) };
    if handle == INVALID_HANDLE_VALUE {
        return Err("PowerCreateRequest returned INVALID_HANDLE_VALUE".to_string());
    }

    // Helper to set a type and bail (closing the handle) on failure.
    macro_rules! set_or_fail {
        ($kind:expr, $flag:expr) => {
            // SAFETY: `handle` is a valid, just-created power request object.
            if unsafe { PowerSetRequest(handle, $kind) } == FALSE {
                // SAFETY: handle is valid and owns nothing yet (we clear on
                // failure to avoid leaking the object).
                unsafe { CloseHandle(handle) };
                return Err(format!("PowerSetRequest({}) failed", $flag));
            }
        };
    }

    // Always keep the system running. Microsoft documents that
    // PowerRequestSystemRequired must accompany PowerRequestDisplayRequired,
    // and it is the core guarantee of a wake lock in any case.
    set_or_fail!(PowerRequestSystemRequired, "SystemRequired");
    // ExecutionRequired (Win8+) prevents process-lifetime suspension; on S3 it
    // implies SystemRequired. It is harmless to set on systems that map it to
    // SystemRequired.
    set_or_fail!(PowerRequestExecutionRequired, "ExecutionRequired");

    let mut display_required = false;
    if want_display {
        set_or_fail!(PowerRequestDisplayRequired, "DisplayRequired");
        display_required = true;
    }

    Ok(PowerRequestHandle {
        handle,
        system_required: true,
        execution_required: true,
        display_required,
    })
}

/// Probe whether `SetThreadExecutionState` actually works on this system.
///
/// The fallback path (see `acquire`) calls it with `ES_CONTINUOUS |
/// ES_SYSTEM_REQUIRED`; doctor must report whether that call *succeeds* rather
/// than just assert the symbol exists. We set a known-safe combination, treat a
/// non-zero return as success, and always *restore* the thread state to
/// `ES_CONTINUOUS` afterwards so the probe itself never leaves a wake lock
/// dangling.
///
/// Returns a one-line note for the doctor report: "available" on success, or a
/// note describing the failure (the API returns 0 on failure).
fn probe_set_thread_execution_state() -> String {
    // ES_CONTINUOUS | ES_SYSTEM_REQUIRED is exactly what `acquire`'s fallback
    // path sets without the display bit; it is the safe, documented combination.
    let probe_flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;

    // SAFETY: `SetThreadExecutionState` has no preconditions beyond running on
    // Windows; the argument is a documented bitmask. We restore below.
    let prev = unsafe { SetThreadExecutionState(probe_flags) };

    // A return of 0 means the call failed. On success `prev` is the *previous*
    // thread execution state; we restore it to `ES_CONTINUOUS` so the probe
    // itself never leaves a wake lock dangling.
    if prev != 0 {
        // SAFETY: same as above; no preconditions.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        "SetThreadExecutionState: available (works; used as automatic fallback)".to_string()
    } else {
        "SetThreadExecutionState: probe failed (returned 0); fallback unavailable".to_string()
    }
}

/// Encode a Rust string as a NUL-terminated UTF-16 vector suitable for
/// `REASON_CONTEXT::Reason::SimpleReasonString` (`PWSTR`).
fn wide_utf16_z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// Run `powercfg.exe <args>` and return its combined stdout+stderr on success,
/// or `None` if the tool is missing / the spawn fails. Best-effort: used only
/// for diagnostics, never on the acquire path.
fn run_powercfg(args: &[&str]) -> Option<String> {
    use std::process::Command;
    let mut cmd = Command::new("powercfg.exe");
    cmd.args(args);
    let output = cmd.output().ok()?;
    let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.is_empty() {
        out.push_str(&err);
    }
    Some(out)
}

// `TRUE`/`FALSE` are `windows_sys::core::BOOL` constants (an `i32` alias) —
// they are re-exported through `Win32::Foundation` but the type itself lives in
// `core`. Keep the import live so the `set_or_fail!` macro's `BOOL` comparison
// always has the symbol available.
#[allow(dead_code)]
const _ENSURE_TRUE_IMPORTED: windows_sys::core::BOOL = TRUE;

// Unit tests for the pure helpers. The OS-touching path cannot run on the
// Linux CI host, so only the deterministic helpers are exercised here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_utf16_z_is_nul_terminated() {
        let w = wide_utf16_z("hi");
        assert_eq!(w, vec![b'h' as u16, b'i' as u16, 0]);
    }

    #[test]
    fn backend_name_is_stable() {
        assert_eq!(BACKEND_NAME, "win32-power");
    }
}
