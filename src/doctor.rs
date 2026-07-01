//! `ow doctor` — environment and backend capability probe.
//!
//! [`run_doctor`] gathers everything `ow doctor` prints:
//!
//! 1. a **platform string** (`"linux"` / `"windows"`), selected at compile time;
//! 2. a flat list of **environment probes** (`(key, value)` pairs) — OS release
//!    info, session type, display server variables on Linux; Windows version,
//!    power source, and a Modern Standby hint on Windows;
//! 3. the **per-backend reports** produced by [`crate::backend::doctor_all`].
//!
//! The per-backend reports already probe their own OS service (D-Bus name
//! reachability, display presence, etc.), so this module deliberately does **not**
//! duplicate that work. It only contributes the environment / file probes that do
//! not belong to any single backend.
//!
//! `run_doctor` never panics and never fails on missing data: a missing
//! `/etc/os-release`, an unset env var, or an unreadable power status simply
//! yields fewer rows. It returns [`DoctorOutput`] by value (infallible) so the
//! CLI can render whatever it managed to collect.

use crate::model::DoctorReport;

/// Output of `ow doctor`.
///
/// `platform` is the short OS family string, `env` is a flat ordered list of
/// `(key, value)` environment facts, and `backends` is one [`DoctorReport`] per
/// backend the integrator compiled in (returned by
/// [`crate::backend::doctor_all`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DoctorOutput {
    /// Short platform family string: `"linux"` or `"windows"`.
    pub platform: String,
    /// Ordered `(key, value)` environment facts (OS release, session vars, ...).
    pub env: Vec<(String, String)>,
    /// One [`DoctorReport`] per compiled-in backend.
    pub backends: Vec<DoctorReport>,
}

impl DoctorOutput {
    /// The platform family this binary was built for: `"linux"` or `"windows"`.
    ///
    /// Selected at compile time so the platform column is always truthful about
    /// the build target, independent of any runtime detection.
    pub fn platform_string() -> &'static str {
        if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(windows) {
            "windows"
        } else {
            "unknown"
        }
    }
}

/// Run the full doctor probe and return the result.
///
/// Infallible by design: missing files, unset env vars, or unreadable power
/// status simply contribute fewer `env` rows rather than erroring. The backend
/// reports come from [`crate::backend::doctor_all`].
pub fn run_doctor() -> DoctorOutput {
    DoctorOutput {
        platform: DoctorOutput::platform_string().to_string(),
        env: collect_env(),
        backends: crate::backend::doctor_all(),
    }
}

/// Collect the environment/file probes for the current platform.
///
/// On unsupported targets this returns an empty vector (only the backend reports
/// remain meaningful).
fn collect_env() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();

    #[cfg(target_os = "linux")]
    collect_linux(&mut out);

    #[cfg(windows)]
    collect_windows(&mut out);

    out
}

/// Push `(key, value)` from the process environment, but only if the variable
/// is set and non-empty. Unset variables are simply omitted from the report.
#[cfg(any(target_os = "linux", windows))]
fn env_kv(out: &mut Vec<(String, String)>, key: &str) {
    if let Ok(val) = std::env::var(key) {
        if !val.is_empty() {
            out.push((key.to_string(), val));
        }
    }
}

// ---------------------------------------------------------------------------
// Linux probes
// ---------------------------------------------------------------------------

/// Append the Linux environment probes to `out`.
///
/// Reads `/etc/os-release` for the distro name (never fatal if absent) and pulls
/// the standard session/display env vars. Per-service D-Bus reachability is
/// intentionally left to each backend's own `doctor()` — preferred over
/// duplicating it here.
#[cfg(target_os = "linux")]
fn collect_linux(out: &mut Vec<(String, String)>) {
    os_release(out);

    // Standard session/display environment variables from setup.md. `env_kv`
    // pushes a row only when the variable is actually set, so an unset variable
    // simply does not appear in the report.
    env_kv(out, "XDG_CURRENT_DESKTOP");
    env_kv(out, "XDG_SESSION_TYPE");
    env_kv(out, "WAYLAND_DISPLAY");
    env_kv(out, "DISPLAY");
}

/// Parse `/etc/os-release` and push a single `OS` row.
///
/// A missing or unreadable file is silently skipped — doctor must stay usable on
/// minimal/odd systems. The format is `KEY=value` (optionally quoted). We prefer
/// `PRETTY_NAME`, falling back to `ID` + `VERSION_ID`, exactly as `os-release(5)`
/// describes. Some systems ship only `/usr/lib/os-release`, tried as a fallback.
#[cfg(target_os = "linux")]
fn os_release(out: &mut Vec<(String, String)>) {
    let text = match std::fs::read_to_string("/etc/os-release") {
        Ok(t) => t,
        Err(_) => match std::fs::read_to_string("/usr/lib/os-release") {
            Ok(t) => t,
            Err(_) => return, // Not fatal: just no OS row.
        },
    };

    let mut pretty: Option<String> = None;
    let mut id: Option<String> = None;
    let mut version_id: Option<String> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, val) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match key.trim() {
            "PRETTY_NAME" => pretty = Some(unquote(val.trim())),
            "ID" => id = Some(unquote(val.trim())),
            "VERSION_ID" => version_id = Some(unquote(val.trim())),
            _ => {}
        }
    }

    if let Some(name) = pretty {
        out.push(("OS".to_string(), name));
    } else {
        // Synthesize something useful from ID (+ VERSION_ID) when there is no
        // human-readable PRETTY_NAME.
        let mut parts: Vec<String> = Vec::new();
        if let Some(id) = id {
            parts.push(id);
        }
        if let Some(v) = version_id {
            parts.push(v);
        }
        if !parts.is_empty() {
            out.push(("OS".to_string(), parts.join(" ")));
        }
    }
}

/// Strip surrounding quotes (single or double) from an `os-release` value.
#[cfg(target_os = "linux")]
fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 {
        let first = v.chars().next().unwrap();
        let last = v.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

// ---------------------------------------------------------------------------
// Windows probes
// ---------------------------------------------------------------------------

/// Append the Windows environment probes to `out`.
///
/// Reports the Windows version, the AC/battery power source, and a Modern
/// Standby honesty note. None of these are fatal if they cannot be read.
#[cfg(windows)]
fn collect_windows(out: &mut Vec<(String, String)>) {
    version(out);
    power(out);

    // Note: LOCALAPPDATA / runtime dir resolution is owned by `paths`, not
    // doctor; we only report facts that help the user interpret backend output.
    env_kv(out, "LOCALAPPDATA");
}

/// Report the Windows version via `GetVersionExW`.
///
/// `GetVersionExW` may be subject to manifest-based compatibility shimming on
/// Windows 8.1+, but it is the cheapest portable call available through
/// `windows-sys` with the feature set the crate already pulls in. For a doctor
/// probe the reported numbers are good enough.
#[cfg(windows)]
fn version(out: &mut Vec<(String, String)>) {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};

    // SAFETY: `OSVERSIONINFOW` is zero-initialized with its correct
    // `dwOSVersionInfoSize`; `GetVersionExW` only writes into the struct.
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    let ok: BOOL = unsafe { GetVersionExW(&mut info) };
    if ok != 0 {
        out.push((
            "Windows".to_string(),
            format!(
                "{}.{}.{}",
                info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
            ),
        ));
    } else {
        out.push(("Windows".to_string(), "version unavailable".to_string()));
    }
}

/// Report the power source via `GetSystemPowerStatus`.
///
/// Requires the `Win32_System_Power` feature on `windows-sys`, which the crate's
/// `Cargo.toml` enables. If the feature were ever removed, this function would
/// fail to compile — the probe vanishes together with its dependency rather than
/// silently degrading.
#[cfg(windows)]
fn power(out: &mut Vec<(String, String)>) {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    // SAFETY: `SYSTEM_POWER_STATUS` is plain old data; the API writes into it
    // and returns 0 on failure.
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    let ok: BOOL = unsafe { GetSystemPowerStatus(&mut status) };
    if ok == 0 {
        out.push(("Power".to_string(), "status unavailable".to_string()));
        return;
    }

    // ACLineStatus: 0 = offline (battery), 1 = online (AC), 255 = unknown.
    let source = match status.ACLineStatus {
        0 => "battery",
        1 => "AC",
        _ => "unknown",
    };
    out.push(("Power".to_string(), source.to_string()));

    // BatteryFlag: 128 = no system battery. Report battery percent when a
    // battery is present and the life percent is meaningful (<= 100; 255 is
    // "unknown").
    if status.BatteryFlag != 128 {
        if status.BatteryLifePercent <= 100 {
            out.push((
                "Battery".to_string(),
                format!("{}%", status.BatteryLifePercent),
            ));
        } else {
            out.push(("Battery".to_string(), "unknown".to_string()));
        }
    } else {
        out.push(("Battery".to_string(), "no system battery".to_string()));
    }

    // Modern Standby honesty note, per setup.md: on Modern Standby (S0ix)
    // systems on DC, Windows may terminate system/execution power requests
    // after the sleep timeout, and user-initiated sleep clears requests. We
    // cannot cheaply detect S0ix support here, so we emit the caveat as a
    // static reminder to keep the doctor honest about guarantees.
    out.push((
        "ModernStandby".to_string(),
        "on DC, Windows may terminate power requests after the sleep timeout; \
         user-initiated sleep clears requests"
            .to_string(),
    ));
}
