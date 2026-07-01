//! In-process daemon lifecycle test.
//!
//! The live `ow on` path can't be exercised on hosts without a working Polkit
//! (systemd-logind refuses *all* inhibitors when `polkitd` is absent), so this
//! test drives [`oxiwake::daemon::run_daemon`] directly with a mock backend to
//! prove the core invariants of the daemon model:
//!
//!   1. the guard is acquired (the lock is taken);
//!   2. `state.json` is published so `ow status` works without IPC;
//!   3. the IPC protocol answers `Ping` / `Status` while the lock is held;
//!   4. the guard is **still held** mid-serve (not released early);
//!   5. `Stop` ends the serve loop and the daemon returns;
//!   6. the guard is released (its `Drop` ran) — the leak-safe RAII invariant;
//!   7. `state.json` is removed on the way out so a later `ow status` is honest.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use oxiwake::daemon;
use oxiwake::error::Result;
use oxiwake::ipc::ClientMsg;
use oxiwake::model::{
    DoctorReport, WakeBackend, WakeGuard, WakeMode, WakeRequest, WakeStatus, WakeTarget,
};
use oxiwake::paths::Paths;
use oxiwake::state::LockState;

/// Shared mock state observed across the backend, the guard, and the test.
#[derive(Default)]
struct MockState {
    acquired: AtomicBool,
    released: AtomicBool,
}

struct MockBackend {
    st: Arc<MockState>,
}

impl WakeBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn supported(&self) -> bool {
        true
    }
    fn acquire(&self, _req: &WakeRequest) -> Result<Box<dyn WakeGuard>> {
        self.st.acquired.store(true, Ordering::SeqCst);
        Ok(Box::new(MockGuard {
            st: Arc::clone(&self.st),
        }))
    }
    fn status(&self) -> Result<WakeStatus> {
        Ok(WakeStatus {
            backend: "mock".to_string(),
            targets: vec![WakeTarget::Idle],
            mode: WakeMode::Block,
            display: false,
        })
    }
    fn doctor(&self) -> Result<DoctorReport> {
        Ok(DoctorReport {
            backend: "mock".to_string(),
            supported: true,
            available: true,
            guarantees: vec![],
            notes: vec![],
        })
    }
}

struct MockGuard {
    st: Arc<MockState>,
}

impl WakeGuard for MockGuard {
    fn backend(&self) -> &'static str {
        "mock"
    }
}

/// Releasing the lock == the guard's `Drop` ran. This is the whole project's
/// core invariant, so we assert it explicitly.
impl Drop for MockGuard {
    fn drop(&mut self) {
        self.st.released.store(true, Ordering::SeqCst);
    }
}

/// Serializes the tests in this binary: they all repoint the process-global
/// `XDG_RUNTIME_DIR`, so running them in parallel would race on the env var.
static SERIALIZE: Mutex<()> = Mutex::new(());

#[test]
fn daemon_holds_lock_serves_ipc_and_releases_on_stop() {
    // These tests all reuse the process-global XDG_RUNTIME_DIR env var, so they
    // must not run concurrently — the mutex serializes them within this binary.
    let _serialize = SERIALIZE.lock().unwrap();
    // Isolate the runtime dir so the test never touches a real session dir.
    let runtime = std::env::temp_dir().join(format!("oxiwake-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);

    let st = Arc::new(MockState::default());
    let backend: Box<dyn WakeBackend> = Box::new(MockBackend {
        st: Arc::clone(&st),
    });
    let req = WakeRequest::default_linux();

    // The daemon serve loop blocks until Stop, so run it on a worker thread.
    let handle = {
        let req = req.clone();
        std::thread::spawn(move || daemon::run_daemon(backend, &req, 1_700_000_000))
    };

    // (1+3) Wait for the daemon to come up, then Ping it.
    let ping = poll_for(Duration::from_secs(5), || {
        daemon::ipc_request(ClientMsg::Ping).ok()
    })
    .expect("daemon never answered Ping");
    assert!(ping.ok, "Ping not ok");
    assert!(ping.status.is_some(), "Ping did not carry status");
    assert!(ping.pid.is_some(), "Ping did not carry pid");

    // (1) The lock was actually acquired.
    assert!(
        st.acquired.load(Ordering::SeqCst),
        "backend.acquire was not called"
    );

    // (4) While serving, the guard must NOT have been released yet.
    assert!(
        !st.released.load(Ordering::SeqCst),
        "guard was released while the daemon is still serving (lock leaked early)"
    );

    // (3) Status round-trips too.
    let status = daemon::ipc_request(ClientMsg::Status).expect("Status IPC failed");
    assert!(status.ok);
    assert_eq!(status.status.as_ref().unwrap().backend, "mock");

    // (2) state.json was published.
    let paths = Paths::resolve().unwrap();
    let persisted = LockState::read(&paths).unwrap();
    let persisted = persisted.expect("state.json was not written");
    assert_eq!(persisted.backend, "mock");
    assert_eq!(persisted.started_unix, 1_700_000_000);
    assert_eq!(persisted.request.targets, req.targets);

    // (5) Stop ends the serve loop and the daemon returns cleanly.
    let stop = daemon::ipc_request(ClientMsg::Stop).expect("Stop IPC failed");
    assert!(stop.ok);
    let join = handle.join().expect("daemon thread panicked");
    assert!(
        join.is_ok(),
        "run_daemon returned an error: {:?}",
        join.err()
    );

    // (6) The guard's Drop ran -> the lock was released. THE invariant.
    assert!(
        st.released.load(Ordering::SeqCst),
        "guard was not released after Stop (lock would leak)"
    );

    // (7) state.json was cleaned up so a later status does not lie.
    assert!(
        LockState::read(&paths).unwrap().is_none(),
        "state.json was not removed on shutdown"
    );

    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn second_run_daemon_declines_when_one_is_already_serving() {
    let _serialize = SERIALIZE.lock().unwrap();
    // Regression for the orphan-lock defect: a second `run_daemon` must detect
    // an already-serving daemon (via the pre-acquire Ping) and return Ok
    // WITHOUT taking a second lock, so a racing double `ow on` can never orphan
    // the first daemon's lock or wedge the socket.
    let runtime = std::env::temp_dir().join(format!("oxiwake-test-2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);

    // Daemon A: takes the lock and serves.
    let st_a = Arc::new(MockState::default());
    let backend_a: Box<dyn WakeBackend> = Box::new(MockBackend {
        st: Arc::clone(&st_a),
    });
    let req = WakeRequest::default_linux();
    let req_for_b = req.clone();
    let handle_a =
        { std::thread::spawn(move || daemon::run_daemon(backend_a, &req, 1_700_000_000)) };
    let ping = poll_for(Duration::from_secs(5), || {
        daemon::ipc_request(ClientMsg::Ping).ok()
    })
    .expect("daemon A never answered Ping");
    assert!(ping.ok);
    assert!(
        st_a.acquired.load(Ordering::SeqCst),
        "A did not acquire the lock"
    );

    // Daemon B: must decline because A is live.
    let st_b = Arc::new(MockState::default());
    let backend_b: Box<dyn WakeBackend> = Box::new(MockBackend {
        st: Arc::clone(&st_b),
    });
    let result_b =
        std::thread::spawn(move || daemon::run_daemon(backend_b, &req_for_b, 1_700_000_001))
            .join()
            .expect("B thread panicked");
    assert!(
        result_b.is_ok(),
        "second run_daemon should decline cleanly, got {:?}",
        result_b.err()
    );
    assert!(
        !st_b.acquired.load(Ordering::SeqCst),
        "B must NOT acquire a second lock"
    );
    assert!(!st_b.released.load(Ordering::SeqCst));

    // A is still the sole owner, still holding its lock.
    assert!(daemon::ipc_request(ClientMsg::Ping).unwrap().ok);
    assert!(st_a.acquired.load(Ordering::SeqCst));
    assert!(
        !st_a.released.load(Ordering::SeqCst),
        "A must still hold the lock"
    );

    // Stop A; it releases.
    assert!(daemon::ipc_request(ClientMsg::Stop).unwrap().ok);
    handle_a.join().unwrap().unwrap();
    assert!(st_a.released.load(Ordering::SeqCst), "A released on stop");

    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn concurrent_double_start_takes_exactly_one_lock() {
    let _serialize = SERIALIZE.lock().unwrap();
    // Regression for the racing-double-`ow on` defect that the singleton lock
    // closes: if two `run_daemon` calls start SIMULTANEOUSLY (so neither is up
    // when the other runs its pre-acquire Ping), the singleton file lock must
    // still ensure exactly one of them acquires the OS lock. Before that lock
    // this was a TOCTOU: both could acquire, clobbering state.json and leaving
    // the winner with no state file.
    let runtime = std::env::temp_dir().join(format!("oxiwake-test-4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);

    let st_a = Arc::new(MockState::default());
    let st_b = Arc::new(MockState::default());
    let req = WakeRequest::default_linux();

    // A barrier so both daemons enter run_daemon as simultaneously as possible,
    // maximizing the chance both pass the pre-acquire Ping before either binds.
    let barrier = Arc::new(Barrier::new(2));
    let backend_a: Box<dyn WakeBackend> = Box::new(MockBackend {
        st: Arc::clone(&st_a),
    });
    let backend_b: Box<dyn WakeBackend> = Box::new(MockBackend {
        st: Arc::clone(&st_b),
    });

    let handle_a = {
        let barrier = Arc::clone(&barrier);
        let req = req.clone();
        std::thread::spawn(move || {
            barrier.wait();
            daemon::run_daemon(backend_a, &req, 1_700_000_000)
        })
    };
    let handle_b = {
        let barrier = Arc::clone(&barrier);
        let req = req.clone();
        std::thread::spawn(move || {
            barrier.wait();
            daemon::run_daemon(backend_b, &req, 1_700_000_001)
        })
    };

    // Exactly one daemon comes up and answers Ping.
    let ping = poll_for(Duration::from_secs(5), || {
        daemon::ipc_request(ClientMsg::Ping).ok()
    })
    .expect("neither daemon answered Ping — both declined?");
    assert!(ping.ok);

    // THE invariant this test exists for: exactly one of the two acquired the
    // OS lock. (The winner acquires before it binds; the loser declined at the
    // singleton lock before acquiring.)
    let acquired_a = st_a.acquired.load(Ordering::SeqCst);
    let acquired_b = st_b.acquired.load(Ordering::SeqCst);
    assert!(
        acquired_a ^ acquired_b,
        "expected exactly one daemon to acquire the lock; got a={acquired_a} b={acquired_b}"
    );

    // Stop whichever daemon won; both threads must then return Ok (the winner
    // served cleanly, the loser declined cleanly).
    assert!(daemon::ipc_request(ClientMsg::Stop).unwrap().ok);
    let result_a = handle_a.join().expect("A thread panicked");
    let result_b = handle_b.join().expect("B thread panicked");
    assert!(result_a.is_ok(), "A should return Ok: {:?}", result_a);
    assert!(result_b.is_ok(), "B should return Ok: {:?}", result_b);

    // The winner released its guard on stop; the loser never took one.
    let released_a = st_a.released.load(Ordering::SeqCst);
    let released_b = st_b.released.load(Ordering::SeqCst);
    assert!(
        released_a ^ released_b,
        "expected exactly one guard to drop; got a={released_a} b={released_b}"
    );

    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn silent_client_does_not_wedge_the_accept_loop() {
    let _serialize = SERIALIZE.lock().unwrap();
    // Regression for the server-side read-timeout defect: a client that
    // connects and sends nothing must NOT wedge the single-threaded accept
    // loop — a subsequent Ping must still get through (within the bounded
    // server timeout), so `ow off`/`status`/`toggle` can always reach the daemon.
    let runtime = std::env::temp_dir().join(format!("oxiwake-test-3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);

    let st = Arc::new(MockState::default());
    let backend: Box<dyn WakeBackend> = Box::new(MockBackend {
        st: Arc::clone(&st),
    });
    let req = WakeRequest::default_linux();
    let handle = std::thread::spawn(move || daemon::run_daemon(backend, &req, 1_700_000_000));
    poll_for(Duration::from_secs(5), || {
        daemon::ipc_request(ClientMsg::Ping).ok()
    })
    .expect("daemon never answered initial Ping");

    // A silent client: connect and send nothing, keep the handle alive.
    let paths = Paths::resolve().unwrap();
    let _silent = std::os::unix::net::UnixStream::connect(&paths.socket).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // A real Ping must still succeed — the silent client is dropped after the
    // server timeout rather than wedging the loop forever.
    let ping = poll_for(Duration::from_secs(8), || {
        daemon::ipc_request(ClientMsg::Ping).ok()
    })
    .expect("accept loop was wedged by the silent client");
    assert!(ping.ok, "Ping did not get through after a silent client");

    assert!(daemon::ipc_request(ClientMsg::Stop).unwrap().ok);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&runtime);
}

/// Try `f` repeatedly until it returns `Some`, or `timeout` elapses.
fn poll_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
