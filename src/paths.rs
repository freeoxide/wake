//! Filesystem locations for Oxiwake's runtime state.
//!
//! Oxiwake keeps a small amount of per-user runtime state (the active lock
//! snapshot, a pending-request hand-off file, the IPC socket, and a singleton
//! lock file that gates daemon startup) in a single private directory. This
//! module resolves that directory and the well-known files inside it.
//!
//! Locations (see `docs/setup.md`):
//!
//! - **Linux:** `$XDG_RUNTIME_DIR/oxiwake/` — the conventional base for
//!   user-specific non-essential runtime files and sockets. `XDG_RUNTIME_DIR`
//!   has *no* guaranteed default; if it is unset, [`Paths::resolve`] returns an
//!   [`OxiwakeError::State`] error rather than guessing. The directory is
//!   created with mode `0700` so only the owning user can read the socket and
//!   state file.
//! - **Windows:** `%LOCALAPPDATA%\Freeoxide\Oxiwake\`. For v0.1 the
//!   `LOCALAPPDATA` environment variable is read directly; if it is unset,
//!   [`Paths::resolve`] returns an [`OxiwakeError::State`] error. (A future
//!   version should prefer `SHGetKnownFolderPath(FOLDERID_LocalAppData)`.)

use std::path::PathBuf;

use crate::error::{OxiwakeError, Result};

/// The set of well-known files Oxiwake uses at runtime.
///
/// Produced by [`Paths::resolve`], which both locates and (if necessary)
/// creates the owning directory. All paths are absolute.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The runtime directory itself, e.g. `$XDG_RUNTIME_DIR/oxiwake`.
    pub dir: PathBuf,
    /// `dir/state.json` — the snapshot written by the daemon and read by
    /// `ow status` when the daemon is unreachable.
    pub state: PathBuf,
    /// `dir/oxiwake.sock` — the IPC socket (Unix socket on Linux).
    pub socket: PathBuf,
    /// `dir/pending.json` — the request hand-off file the CLI writes so the
    /// freshly-spawned daemon knows what lock to take.
    pub pending: PathBuf,
    /// `dir/oxiwake.lock` — the singleton advisory lock (flock on Linux,
    /// `LockFileEx` on Windows) the daemon holds for its entire lifetime. It is
    /// the atomic mutex that ensures two racing `ow on` invocations can never
    /// both reach OS-lock acquisition; it auto-releases when the daemon's fd /
    /// handle closes (including on crash).
    pub lock: PathBuf,
}

impl Paths {
    /// Resolve the runtime directory and the well-known files inside it.
    ///
    /// Creates the directory (and parents) if it does not yet exist. On Linux
    /// the directory is forced to mode `0700`; an existing directory with
    /// looser permissions is tightened best-effort. Returns
    /// [`OxiwakeError::State`] if the base runtime directory cannot be
    /// determined for this platform.
    pub fn resolve() -> Result<Paths> {
        let dir = Self::runtime_dir_or_none()
            .ok_or_else(|| OxiwakeError::State(base_dir_unset_message().to_string()))?;

        // Create the directory (and any missing parents).
        std::fs::create_dir_all(&dir)?;

        // On Linux, ensure the directory is private to the owning user. The
        // XDG_RUNTIME_DIR base is conventionally 0700, but be defensive: if a
        // previous (or user-created) directory is looser, tighten it. This is
        // best-effort — a failure to chmod is reported but does not mask the
        // successful resolution.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            const PRIVATE_MODE: u32 = 0o700;
            // create_dir_all just succeeded, so a metadata failure here is
            // exotic; ignore it rather than blocking startup.
            if let Ok(meta) = std::fs::metadata(&dir) {
                let cur = meta.permissions().mode();
                if cur & 0o777 != PRIVATE_MODE {
                    let _ = std::fs::set_permissions(
                        &dir,
                        std::fs::Permissions::from_mode(PRIVATE_MODE),
                    );
                }
            }
        }

        let state = dir.join("state.json");
        let pending = dir.join("pending.json");
        // The IPC socket name is platform-neutral; on Linux this is a Unix
        // domain socket.
        let socket = dir.join("oxiwake.sock");
        let lock = dir.join("oxiwake.lock");

        Ok(Paths {
            dir,
            state,
            socket,
            pending,
            lock,
        })
    }

    /// Return the runtime directory if it can be determined, else `None`.
    ///
    /// Never errors — intended for `ow doctor`, which wants to report the
    /// situation rather than abort. Returns the *base* directory that
    /// [`Paths::resolve`] would use (without the trailing `oxiwake` segment
    /// on Linux, or *with* the `Freeoxide\Oxiwake` segments on Windows, so
    /// callers can describe exactly where state would live).
    #[cfg(target_os = "linux")]
    pub fn runtime_dir_or_none() -> Option<PathBuf> {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .map(|base| base.join("oxiwake"))
    }

    /// Return the runtime directory if it can be determined, else `None`.
    ///
    /// Windows variant: reads `%LOCALAPPDATA%` and appends `Freeoxide\Oxiwake`.
    /// Never errors — intended for `ow doctor`.
    #[cfg(windows)]
    pub fn runtime_dir_or_none() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("Freeoxide").join("Oxiwake"))
    }

    /// Fallback for unsupported platforms: no runtime directory is known.
    #[cfg(not(any(target_os = "linux", windows)))]
    pub fn runtime_dir_or_none() -> Option<PathBuf> {
        None
    }
}

/// The human-readable message used when the base runtime directory is unset.
///
/// Factored out so the same wording is used by [`Paths::resolve`] regardless of
/// platform; the platform-specific variable name is selected at compile time.
fn base_dir_unset_message() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "XDG_RUNTIME_DIR is not set; cannot determine oxiwake runtime directory \
         (set it to your user runtime dir, e.g. /run/user/$(id -u), or run \
         inside a systemd/logind session)"
    }
    #[cfg(windows)]
    {
        "LOCALAPPDATA is not set; cannot determine oxiwake runtime directory"
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        "no runtime directory is configured for this platform"
    }
}
