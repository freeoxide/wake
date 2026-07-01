//! Persistent lock-state snapshot.
//!
//! The Oxiwake daemon holds a wake lock open for its lifetime. While it runs it
//! also writes a small [`LockState`] snapshot to `state.json` in the runtime
//! directory ([`crate::paths::Paths`]). This lets `ow status` report the active
//! lock even without a live IPC round-trip, and lets a freshly-started CLI
//! detect a stale/crashed daemon whose guard has already gone.
//!
//! All three operations are fault-tolerant in the obvious ways:
//!
//! - [`LockState::read`] returns `Ok(None)` when the file is simply absent.
//! - [`LockState::write`] is **atomic**: it serializes to a sibling `.tmp`
//!   file and then `fs::rename`s it over `state.json`, so a reader never sees
//!   a half-written file.
//! - [`LockState::remove`] returns `Ok(())` whether or not the file existed.

use std::path::Path;

use crate::error::{OxiwakeError, Result};
use crate::model::WakeRequest;
use crate::paths::Paths;

/// Snapshot of the lock a running daemon currently holds.
///
/// Written atomically by [`LockState::write`] and read back by
/// [`LockState::read`]. `started_unix` is supplied by the caller (the daemon)
/// rather than read from the system clock, so library logic stays
/// deterministic and testable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LockState {
    /// PID of the daemon process holding the lock.
    pub pid: u32,
    /// Name of the backend holding the lock (e.g. `"systemd-logind"`).
    pub backend: String,
    /// Unix timestamp (seconds) at which the lock was acquired. Provided by
    /// the caller; never read from the system clock inside this module.
    pub started_unix: u64,
    /// The user request that produced this lock, kept verbatim so `ow status`
    /// can show the original targets/reason.
    pub request: WakeRequest,
}

impl LockState {
    /// Read the lock snapshot, or `Ok(None)` if no `state.json` exists.
    ///
    /// A corrupt/unparseable file is an error ([`OxiwakeError::State`]); a
    /// merely-absent file is not.
    pub fn read(paths: &Paths) -> Result<Option<LockState>> {
        read_json_or_none(&paths.state)
    }

    /// Atomically write the lock snapshot to `state.json`.
    ///
    /// Serializes to `state.json.tmp` in the same directory and then renames
    /// it over `state.json`. POSIX `rename` is atomic, so a concurrent reader
    /// sees either the old file or the new file, never a torn write. The temp
    /// file is removed if serialization fails.
    pub fn write(paths: &Paths, st: &LockState) -> Result<()> {
        let tmp = tmp_path_for(&paths.state);
        let serialized = serde_json::to_vec_pretty(st)
            .map_err(|e| OxiwakeError::State(format!("could not serialize state: {e}")))?;

        // Write the temp file, then rename atomically. If any step fails, try
        // to clean the temp file up so we don't leave litter behind.
        if let Err(e) = std::fs::write(&tmp, &serialized) {
            let _ = std::fs::remove_file(&tmp);
            return Err(OxiwakeError::from(e));
        }
        if let Err(e) = std::fs::rename(&tmp, &paths.state) {
            let _ = std::fs::remove_file(&tmp);
            return Err(OxiwakeError::from(e));
        }
        Ok(())
    }

    /// Remove the `state.json` file. `Ok(())` if it was already absent.
    ///
    /// Called by the daemon on a clean exit, after its guard has dropped.
    pub fn remove(paths: &Paths) -> Result<()> {
        remove_if_exists(&paths.state)
    }
}

/// Read a JSON file into `T`, returning `Ok(None)` when the file is absent and
/// [`OxiwakeError::State`] when it exists but cannot be parsed.
fn read_json_or_none<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value = serde_json::from_slice::<T>(&bytes).map_err(|e| {
                OxiwakeError::State(format!("could not parse {}: {e}", path.display()))
            })?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OxiwakeError::from(e)),
    }
}

/// Delete `path` if it exists; `Ok(())` otherwise.
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OxiwakeError::from(e)),
    }
}

/// The sibling temp path used for atomic writes: `<file>.tmp`.
fn tmp_path_for(file: &Path) -> std::path::PathBuf {
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    tmp.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WakeRequest;

    /// `read` must treat a missing file as `None`, not an error.
    #[test]
    fn missing_file_is_none() {
        let dir = tempdir_state_path();
        let paths = Paths {
            state: dir.join("does-not-exist.json"),
            socket: dir.join("s.sock"),
            pending: dir.join("p.json"),
            lock: dir.join("l.lock"),
            dir: dir.clone(),
        };
        assert!(LockState::read(&paths).expect("read").is_none());
        // remove on a missing file is also fine.
        LockState::remove(&paths).expect("remove");
    }

    /// A round-trip through write/read must preserve all fields.
    #[test]
    fn write_read_roundtrip() {
        let dir = tempdir_state_path();
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            state: dir.join("state.json"),
            socket: dir.join("s.sock"),
            pending: dir.join("p.json"),
            lock: dir.join("l.lock"),
            dir: dir.clone(),
        };
        let st = LockState {
            pid: 4242,
            backend: "systemd-logind".to_string(),
            started_unix: 1_700_000_000,
            request: WakeRequest::default_linux(),
        };
        LockState::write(&paths, &st).expect("write");
        let back = LockState::read(&paths).expect("read").expect("some");
        assert_eq!(back.pid, 4242);
        assert_eq!(back.backend, "systemd-logind");
        assert_eq!(back.started_unix, 1_700_000_000);
        assert_eq!(back.request.targets, st.request.targets);
        // No leftover temp file after a successful write.
        assert!(!tmp_path_for(&paths.state).exists());
    }

    /// Corrupt JSON must surface as a `State` error, not `None`.
    #[test]
    fn corrupt_file_is_error() {
        let dir = tempdir_state_path();
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            state: dir.join("state.json"),
            socket: dir.join("s.sock"),
            pending: dir.join("p.json"),
            lock: dir.join("l.lock"),
            dir: dir.clone(),
        };
        std::fs::write(&paths.state, b"{ not json").unwrap();
        let err = LockState::read(&paths).unwrap_err();
        assert!(matches!(err, OxiwakeError::State(_)));
    }

    /// Build a unique, scratch state path under the system temp dir.
    fn tempdir_state_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oxiwake-state-test-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        p
    }

    // A per-test-call nonce so parallel tests don't collide on the same file.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    fn unique_nonce() -> u64 {
        NONCE.fetch_add(1, Ordering::Relaxed)
    }
}
