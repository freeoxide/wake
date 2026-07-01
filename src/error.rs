//! Error type for Oxiwake.

/// Every fallible Oxiwake operation returns this.
#[derive(Debug, thiserror::Error)]
pub enum OxiwakeError {
    /// The backend is compiled in but cannot be used right now (no D-Bus, no display, ...).
    #[error("backend {backend} unavailable: {reason}")]
    BackendUnavailable {
        backend: &'static str,
        reason: String,
    },

    /// The backend tried to take the lock and the OS refused it (denied by PolicyKit, etc.).
    #[error("backend {backend} could not acquire lock: {reason}")]
    AcquireFailed {
        backend: &'static str,
        reason: String,
    },

    /// A D-Bus call failed. zbus is an optional, Linux-only dependency, so this
    /// variant exists only when we are on Linux *and* at least one zbus-using
    /// feature is enabled (default Linux build). It is absent on Windows, on
    /// `--no-default-features`, and on x11/wayland-only Linux builds.
    #[cfg(all(
        target_os = "linux",
        any(
            feature = "linux-logind",
            feature = "linux-portal",
            feature = "linux-screensaver",
            feature = "linux-gnome",
            feature = "linux-kde"
        )
    ))]
    #[error("D-Bus error: {0}")]
    Dbus(#[from] zbus::Error),

    /// A D-Bus method returned an unexpected type / value.
    #[error("D-Bus reply decode error: {0}")]
    DbusDecode(String),

    /// Filesystem / socket I/O.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The persistent `state.json` was missing, unreadable, or inconsistent.
    #[error("state error: {0}")]
    State(String),

    /// An IPC frame was malformed or the daemon replied with an error.
    #[error("ipc error: {0}")]
    Ipc(String),

    /// `ow off` / `ow status` was asked but no daemon is running.
    #[error("oxiwake is not running")]
    NotRunning,

    /// `ow on` was asked but a daemon is already running.
    #[error("oxiwake is already running (pid {0})")]
    AlreadyRunning(u32),

    /// Catch-all for anything that does not fit a more specific variant.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, OxiwakeError>;

impl OxiwakeError {
    /// Quick constructor for a stringly-typed error.
    pub fn other(msg: impl Into<String>) -> Self {
        OxiwakeError::Other(msg.into())
    }
}
