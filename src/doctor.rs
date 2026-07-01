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

/// Report the Windows version by reading the registry.
///
/// `GetVersionExW` is manifest-shimmed: without a compatibility manifest it
/// caps the reported version at 6.2 (Windows 8) / 6.3 (8.1) on Windows 10+,
/// which is exactly the dishonesty `doctor` exists to avoid. The registry is
/// the truthful source — Windows writes its real version under
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`. We read
/// `CurrentMajorVersionNumber` and `CurrentMinorVersionNumber` (REG_DWORD),
/// `CurrentBuild` (REG_SZ), and `UBR` (REG_DWORD, "Update Build Revision"),
/// rendering `major.minor.build.ubr`.
///
/// Note this is the same data the Modern Standby caveat in [`power`] depends
/// on being accurate: S0ix behavior varies across major releases, so reporting
/// a capped 6.2 would mislead the user about their own platform.
#[cfg(windows)]
fn version(out: &mut Vec<(String, String)>) {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY,
    };

    // Open the native view of the registry from either a 32- or 64-bit process
    // so we always read the real (64-bit) key. KEY_READ already grants query
    // access; KEY_WOW64_64KEY forces the 64-bit view.
    let subkey = wide_z("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is a NUL-terminated wide string valid for the call;
    // `key` is an uninitialized out-pointer RegOpenKeyExW writes to on success.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if status != ERROR_SUCCESS {
        out.push(("Windows".to_string(), "version unavailable".to_string()));
        return;
    }

    // Always close the key, even if the value queries below fail.
    let major = query_u32(key, "CurrentMajorVersionNumber");
    let minor = query_u32(key, "CurrentMinorVersionNumber");
    let build = query_string(key, "CurrentBuild");
    let ubr = query_u32(key, "UBR");

    // SAFETY: `key` was successfully opened above; RegCloseKey is the matching
    // release. Safe to call unconditionally on an open handle.
    unsafe { RegCloseKey(key) };

    // Render whatever we managed to read; the REG_DWORD fields are the version,
    // UBR refines build precision, and CurrentBuild is the kernel build string.
    let rendered = match (major, minor, build) {
        (Some(maj), Some(min), Some(bld)) => {
            let base = format!("{}.{}.{}", maj, min, bld);
            match ubr {
                Some(u) => format!("{}.{}", base, u),
                None => base,
            }
        }
        _ => "version unavailable".to_string(),
    };
    out.push(("Windows".to_string(), rendered));
}

/// Query a `REG_DWORD` value from `key`, returning its `u32` value on success.
///
/// `name` is ASCII; it is widened to UTF-16 + NUL on each call (cheap for a
/// handful of doctor probes). Returns `None` on any registry error, on a type
/// mismatch (the value is not a DWORD), or on the wrong data size.
#[cfg(windows)]
fn query_u32(key: windows_sys::Win32::System::Registry::HKEY, name: &str) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegQueryValueExW, REG_DWORD, REG_VALUE_TYPE};

    let name_w = wide_z(name);
    let mut value_type: REG_VALUE_TYPE = 0;
    let mut data: u32 = 0;
    let mut len: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: `key` is an open registry handle; `name_w` is NUL-terminated;
    // `data` and `len` describe a 4-byte buffer exactly the size of a DWORD.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name_w.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            &mut data as *mut u32 as *mut u8,
            &mut len,
        )
    };
    if status != ERROR_SUCCESS || value_type != REG_DWORD || len != 4 {
        return None;
    }
    Some(data)
}

/// Query a `REG_SZ` value from `key`, returning it as a Rust `String`.
///
/// Returns `None` on any registry error, on a type mismatch (the value is not a
/// string), or if the stored bytes are not valid UTF-16.
#[cfg(windows)]
fn query_string(key: windows_sys::Win32::System::Registry::HKEY, name: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegQueryValueExW, REG_SZ, REG_VALUE_TYPE};

    let name_w = wide_z(name);
    let mut value_type: REG_VALUE_TYPE = 0;
    let mut len: u32 = 0;
    // First pass: discover the required buffer size (data pointer is NULL).
    // SAFETY: `key` is an open handle; querying with a NULL data pointer and a
    // zero length is the documented way to read the size.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name_w.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if status != ERROR_SUCCESS || value_type != REG_SZ {
        return None;
    }
    if len == 0 {
        return Some(String::new());
    }

    // `len` is in bytes; allocate a u16 buffer of that many bytes. REG_SZ values
    // may or may not be NUL-terminated; we strip a trailing NUL if present.
    let mut buf = vec![0u16; (len as usize) / 2];
    // SAFETY: same query, now with a buffer sized to the reported length.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name_w.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            buf.as_mut_ptr() as *mut u8,
            &mut len,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    // Strip a single trailing NUL if REG_SZ included one.
    if buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf16(&buf).ok()
}

/// Encode an ASCII string as a NUL-terminated UTF-16 vector for the registry
/// APIs (which take `PCWSTR` = `*const u16`).
#[cfg(windows)]
fn wide_z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
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
