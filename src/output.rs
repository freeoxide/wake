//! Pure presentation layer for Oxiwake's human-facing commands.
//!
//! `output.rs` owns *only* formatting. Every function here is pure: it takes a
//! value ([`WakeStatus`], [`LockState`], [`DoctorOutput`], or a plain string)
//! and returns a `String`, with no I/O, no clock reads, and no fallible calls.
//! That keeps the formatting trivially testable and lets `main.rs` decide where
//! the rendered text goes (stdout for normal output, stderr for errors).
//!
//! Two flavors are provided for every shape:
//!
//! - a **human** form: aligned plain text meant for a terminal; and
//! - a **json** form: a compact [`serde_json`] rendering for machine consumers.
//!
//! The JSON forms deliberately use the types' own `serde::Serialize` impls
//! (possibly wrapped in a small envelope) so the on-wire shape is governed by
//! `model.rs` rather than reinvented here.

use crate::doctor::DoctorOutput;
use crate::model::{WakeMode, WakeStatus, WakeTarget};
use crate::state::LockState;

/// The key under which the lock's start time is reported in JSON envelopes.
const SINCE_KEY: &str = "since_unix";

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// Format `started_unix` as a human-friendly relative age plus the raw epoch.
///
/// `now_unix` is supplied by the caller (from `main.rs`, which reads the clock
/// once) so this function stays deterministic and unit-testable: given the same
/// two timestamps it always returns the same string.
///
/// The age is rendered as a compact `Xh Ym Zs` breakdown (each component
/// omitted when zero, with at least the seconds always shown), e.g.
/// `1h 2m 3s`. A negative or zero duration collapses to `0s`.
fn human_age(now_unix: u64, started_unix: u64) -> String {
    let dur = now_unix.saturating_sub(started_unix);
    if dur == 0 {
        return "0s".to_string();
    }
    let secs = dur % 60;
    let mins = (dur / 60) % 60;
    let hours = dur / 3600;

    let mut parts: Vec<String> = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 || hours > 0 {
        parts.push(format!("{mins}m"));
    }
    parts.push(format!("{secs}s"));
    parts.join(" ")
}

/// Render a [`WakeTarget`] set as a compact human label.
///
/// Order follows the model's priority order (as stored in `WakeStatus`);
/// targets are joined with `+` so a status line reads, e.g.,
/// `sleep+idle`. An empty set becomes `none`.
fn human_targets(targets: &[WakeTarget]) -> String {
    if targets.is_empty() {
        return "none".to_string();
    }
    targets
        .iter()
        .map(|t| target_token(*t))
        .collect::<Vec<_>>()
        .join("+")
}

/// The short token used for one [`WakeTarget`] in human output.
///
/// Mirrors the systemd-logind `what` tokens where they exist (so the output
/// matches what `systemd-inhibit --list` would show) and falls back to a
/// readable name for targets without a logind token.
fn target_token(t: WakeTarget) -> &'static str {
    match t {
        WakeTarget::SystemSleep => "sleep",
        WakeTarget::Idle => "idle",
        WakeTarget::Display => "display",
        WakeTarget::LidSwitch => "lid",
        WakeTarget::Shutdown => "shutdown",
    }
}

/// The short token used for one [`WakeMode`] in human output.
fn mode_token(m: WakeMode) -> &'static str {
    m.as_str()
}

// ---------------------------------------------------------------------------
// Status / state formatting
// ---------------------------------------------------------------------------

/// Human form of an *active* lock, drawn from a live status and the persisted
/// [`LockState`].
///
/// Prints three aligned lines: the backend holding the lock, the honored
/// targets + mode, and how long the lock has been held. `now_unix` is supplied
/// by the caller so no clock is read inside this function.
pub fn human_status(status: &WakeStatus, state: &LockState, now_unix: u64) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.push(format!("{:<10} {} (pid {})", "state:", "on", state.pid));
    lines.push(format!("{:<10} {}", "backend:", status.backend));
    lines.push(format!(
        "{:<10} {} [{}]",
        "targets:",
        human_targets(&status.targets),
        mode_token(status.mode)
    ));
    if status.display {
        lines.push(format!("{:<10} {}", "display:", "kept on"));
    }
    lines.push(format!(
        "{:<10} started {} ({} ago)",
        "since:",
        state.started_unix,
        human_age(now_unix, state.started_unix)
    ));

    lines.join("\n")
}

/// JSON form of an *active* lock.
///
/// Wraps the daemon's [`WakeStatus`] in a small envelope that also carries the
/// owning pid, the lock's start time, and the elapsed seconds, so a machine
/// reader gets the full picture in one object. Compact (no pretty-printing)
/// for stable, line-oriented consumption.
pub fn json_status(status: &WakeStatus, state: &LockState, now_unix: u64) -> String {
    // Build the envelope by value so we control exactly which fields appear and
    // under what names, independent of how `WakeStatus`/`LockState` happen to
    // serialize on their own.
    let elapsed = now_unix.saturating_sub(state.started_unix);
    let env = serde_json::json!({
        "state": "on",
        "backend": status.backend,
        "targets": status.targets,
        "mode": status.mode,
        "display": status.display,
        "pid": state.pid,
        SINCE_KEY: state.started_unix,
        "elapsed_secs": elapsed,
    });
    serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_string())
}

/// Human form of "no daemon is running".
///
/// `ow status` with nothing to report is *not* an error; this renders the
/// benign off-line so the user sees a clear, consistent answer.
pub fn human_off() -> String {
    "state:     off (oxiwake is not running)".to_string()
}

/// JSON form of "no daemon is running".
pub fn json_off() -> String {
    serde_json::json!({ "state": "off", "running": false }).to_string()
}

/// Human form of a generic status line, given an already-rendered state token
/// and an optional note.
///
/// Used for one-shot transitions such as `ow on` / `ow off` where the result is
/// a short confirmation rather than a full status dump.
pub fn human_simple(state: &str, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("{state}: {n}"),
        None => state.to_string(),
    }
}

/// JSON form of a generic one-shot result.
pub fn json_simple(state: &str, note: Option<&str>) -> String {
    let mut obj = serde_json::json!({ "state": state });
    if let Some(n) = note {
        obj["note"] = serde_json::Value::String(n.to_string());
    }
    serde_json::to_string(&obj).unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// Doctor formatting
// ---------------------------------------------------------------------------

/// Human form of a [`DoctorOutput`].
///
/// Renders a per-backend table (name / supported / available / caveats) and a
/// trailing block of environment `(key, value)` lines. Column widths are
/// computed from the data so the table stays aligned regardless of how many
/// backends were compiled in or how long their names are.
pub fn human_doctor(out: &DoctorOutput) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.push(format!("platform: {}", out.platform));
    lines.push(String::new());

    // ---- Backend table --------------------------------------------------
    // Header columns: backend | supported | available | caveats
    let h_backend = "backend";
    let h_supported = "supported";
    let h_available = "available";

    let w_backend = out
        .backends
        .iter()
        .map(|b| b.backend.chars().count())
        .chain(std::iter::once(h_backend.len()))
        .max()
        .unwrap_or(h_backend.len());
    let w_supported = out
        .backends
        .iter()
        .map(|b| yesno(b.supported).len())
        .chain(std::iter::once(h_supported.len()))
        .max()
        .unwrap_or(h_supported.len());
    let w_available = out
        .backends
        .iter()
        .map(|b| yesno(b.available).len())
        .chain(std::iter::once(h_available.len()))
        .max()
        .unwrap_or(h_available.len());

    lines.push(format!(
        "{:<w_b$}  {:<w_s$}  {:<w_a$}  caveats",
        h_backend,
        h_supported,
        h_available,
        w_b = w_backend,
        w_s = w_supported,
        w_a = w_available,
    ));
    // A simple rule matching the header width.
    lines
        .push("-".repeat(w_backend + w_supported + w_available + "  ".len() * 3 + "caveats".len()));

    for b in &out.backends {
        let caveats = if b.guarantees.is_empty() {
            String::from("-")
        } else {
            b.guarantees.join("; ")
        };
        lines.push(format!(
            "{:<w_b$}  {:<w_s$}  {:<w_a$}  {caveats}",
            b.backend,
            yesno(b.supported),
            yesno(b.available),
            w_b = w_backend,
            w_s = w_supported,
            w_a = w_available,
        ));
        // Notes are secondary detail; show them indented under the row when
        // present so the table stays scannable.
        for note in &b.notes {
            lines.push(format!("{:>w$}  note: {note}", "", w = w_backend));
        }
    }

    // ---- Environment block ---------------------------------------------
    if !out.env.is_empty() {
        lines.push(String::new());
        let w_key = out
            .env
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        lines.push("environment:".to_string());
        for (k, v) in &out.env {
            lines.push(format!("  {:<w$}  {v}", k, w = w_key));
        }
    }

    lines.join("\n")
}

/// JSON form of a [`DoctorOutput`].
///
/// Uses the type's own `Serialize` impl verbatim so the shape matches the
/// `model`/`doctor` contract exactly.
pub fn json_doctor(out: &DoctorOutput) -> String {
    serde_json::to_string(out).unwrap_or_else(|_| "{}".to_string())
}

/// Render a boolean as a short human token for the doctor table.
fn yesno(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{WakeMode, WakeRequest, WakeStatus, WakeTarget};

    fn sample_status() -> WakeStatus {
        WakeStatus {
            backend: "systemd-logind".to_string(),
            targets: vec![WakeTarget::SystemSleep, WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        }
    }

    fn sample_state() -> LockState {
        LockState {
            pid: 4242,
            backend: "systemd-logind".to_string(),
            started_unix: 1_000,
            request: WakeRequest::default_linux(),
        }
    }

    #[test]
    fn human_age_formats_hms() {
        assert_eq!(human_age(0, 0), "0s");
        assert_eq!(human_age(1_000, 1_000), "0s");
        // 1h 2m 3s = 3723s
        assert_eq!(human_age(3_723, 0), "1h 2m 3s");
        // 2m 5s = 125s
        assert_eq!(human_age(125, 0), "2m 5s");
        // never negative: started in the future clamps to 0s
        assert_eq!(human_age(100, 200), "0s");
    }

    #[test]
    fn human_status_contains_backend_targets_and_pid() {
        let s = human_status(&sample_status(), &sample_state(), 2_000);
        assert!(s.contains("systemd-logind"), "status must name backend");
        assert!(s.contains("sleep+idle"), "targets must be rendered");
        assert!(s.contains("block"), "mode must be rendered");
        assert!(s.contains("pid 4242"), "pid must be rendered");
        assert!(s.contains("1000"), "started unix must be rendered");
    }

    #[test]
    fn json_status_is_valid_envelope() {
        let raw = json_status(&sample_status(), &sample_state(), 2_000);
        let v: serde_json::Value =
            serde_json::from_str(&raw).expect("json_status must produce valid json");
        assert_eq!(v["state"], "on");
        assert_eq!(v["backend"], "systemd-logind");
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["elapsed_secs"], 1_000);
        assert_eq!(v[SINCE_KEY], 1_000);
    }

    #[test]
    fn off_strings_are_consistent() {
        let h = human_off();
        assert!(h.contains("off"));
        let j = json_off();
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["state"], "off");
        assert_eq!(v["running"], false);
    }

    #[test]
    fn simple_json_roundtrip() {
        let j = json_simple("on", Some("started"));
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["state"], "on");
        assert_eq!(v["note"], "started");
    }

    #[test]
    fn doctor_human_lists_backends_and_env() {
        let out = DoctorOutput {
            platform: "linux".to_string(),
            env: vec![("DISPLAY".to_string(), ":0".to_string())],
            backends: vec![crate::model::DoctorReport {
                backend: "systemd-logind".to_string(),
                supported: true,
                available: true,
                guarantees: vec!["idle only".to_string()],
                notes: vec![],
            }],
        };
        let h = human_doctor(&out);
        assert!(h.contains("platform: linux"));
        assert!(h.contains("systemd-logind"));
        assert!(h.contains("yes"));
        assert!(h.contains("idle only"));
        assert!(h.contains("DISPLAY"));
    }

    #[test]
    fn doctor_json_uses_serde_shape() {
        let out = DoctorOutput {
            platform: "linux".to_string(),
            env: vec![],
            backends: vec![],
        };
        let j = json_doctor(&out);
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["platform"], "linux");
    }
}
