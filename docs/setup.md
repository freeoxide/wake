# Oxiwake — Backend & Setup Design

Oxiwake is a cross-platform "keep awake" CLI/daemon whose job is to hold a wake lock open for as long as the user asks, and to report honestly when the OS will not let it. This document captures the backend strategy, the daemon/toggle model, the dependency stack, the state/IPC layout, and the doctor checklists. It is written for **backend support**, not "distro support": Oxiwake detects capabilities and picks the strongest mechanism available rather than hard-coding distro names.

---

## Think in terms of backends, not distros

For **Oxiwake**, do **not** think in terms of "distro support." Think in terms of **backend support**:

```text
Linux:
  1. systemd-logind inhibitor     ← main backend
  2. XDG Desktop Portal inhibit   ← desktop/session backend
  3. GNOME SessionManager         ← GNOME-specific fallback
  4. KDE PowerDevil / Solid       ← KDE-specific fallback
  5. org.freedesktop.ScreenSaver  ← idle-only fallback
  6. X11 XScreenSaver / DPMS      ← old X11 fallback
  7. Wayland idle-inhibit         ← only useful if you have a surface

Windows:
  1. PowerCreateRequest + PowerSetRequest  ← better modern backend
  2. SetThreadExecutionState               ← simple fallback
  3. powercfg /requests                    ← diagnostics
```

## Big design warning

`ow on` **cannot just acquire a lock and exit**.

On Linux `systemd-logind` returns a file descriptor, and the inhibitor is released when that FD is closed. On Windows `PowerSetRequest` increments a request on a handle and should be cleared/closed when done. So `ow on` needs to start a small background daemon/process that keeps the FD/handle alive. `ow off` kills or IPCs that daemon. ([systemd-logind][1])

So your real model should be:

```bash
ow          # prints state
ow on       # starts oxiwake daemon
ow off      # stops daemon
ow toggle   # starts/stops daemon
ow status   # prints current state
ow doctor   # prints supported backends and edge cases
```

---

# Linux backend plan

## 1. Primary backend: `systemd-logind` D-Bus inhibitor

This is the best Linux backend for Ubuntu, Debian, Pop!_OS, Fedora/Bazzite-style systems, Rocky/RHEL-style systems, and most NixOS desktop installs **when systemd-logind is present**.

Use:

```text
org.freedesktop.login1.Manager.Inhibit(
  what: "sleep:idle:handle-lid-switch",
  who: "Oxiwake",
  why: "Oxiwake wake lock enabled",
  mode: "block"
) -> fd
```

The D-Bus signature is `Inhibit(in s what, in s who, in s why, in s mode, out h fd)` on service `org.freedesktop.login1`, object path `/org/freedesktop/login1`. The `out h` (handle) is a file descriptor; closing it (or process exit) releases the inhibition lock.

Officially, `what` may include `shutdown`, `sleep`, `idle`, `handle-power-key`, `handle-reboot-key`, `handle-suspend-key`, `handle-hibernate-key`, and `handle-lid-switch`; the returned file descriptor is the lifetime of the lock. ([systemd-logind][1])

Good default:

```text
what = "sleep:idle"
mode = "block"
```

Aggressive mode:

```text
what = "sleep:idle:handle-lid-switch"
mode = "block"
```

Diagnostic/aggressive root-ish mode:

```text
what = "shutdown:sleep:idle:handle-power-key:handle-reboot-key:handle-suspend-key:handle-hibernate-key:handle-lid-switch"
mode = "block"
```

Note: taking a `block`-mode inhibitor on `shutdown`, `sleep`, or `handle-*` typically requires privilege (PolicyKit actions `org.freedesktop.login1.inhibit-block-*`), so a normal user may be denied for those tokens; `idle` is the only one routinely allowed unprivileged. The "root-ish" framing above is accurate — expect it to need elevated privileges for the non-`idle` tokens.

`systemd-inhibit` exposes the same concept from CLI, including `handle-lid-switch`, `block`, `delay`, and `block-weak`. Its default inhibition set (`--what`, if omitted) is `idle:sleep:shutdown` and its default `--mode` is `block`. ([systemd-inhibit(1)][2])

### Rust crates

Use one of these:

```toml
zbus = "5"
```

Best low-level choice. `zbus` (current: **5.16.0**, as of mid-2026) is the main pure-Rust D-Bus crate — pure Rust, stable, and suitable for calling logind/portal/DE D-Bus APIs directly. ([Docs.rs][3])

```toml
logind-zbus = "5"
```

Convenient wrapper around systemd-logind's D-Bus interface (current: **5.3.2**, as of mid-2026). It may save time, but I would still keep your own small adapter around it, because Oxiwake only needs a tiny subset: `Inhibit`, `ListInhibitors`, and a few properties like `LidClosed`, `OnExternalPower`, `Docked`. ([Crates.io][4])

### C libraries

Use these only if you want C/FFI:

```text
libsystemd / sd-bus
libdbus-1
GIO / GDBus
```

For Rust, I would avoid C D-Bus unless forced. `zbus` is cleaner.

---

## 2. XDG Desktop Portal backend

This is the best "desktop-friendly" Linux backend, especially for Flatpak/Snap/sandbox-ish environments or desktops where portal backends are reliable.

The portal API supports inhibiting:

```text
1 = logout
2 = user switch
4 = suspend
8 = idle
```

So for Oxiwake:

```text
flags = 4 | 8
```

That means "inhibit suspend and idle" (= 12). The full method is `org.freedesktop.portal.Inhibit.Inhibit(IN window s, IN flags u, IN options a{sv}, OUT handle o)`; `flags` is a `uint` bitmask, and the inhibition is removed by calling `org.freedesktop.portal.Request.Close` on the returned handle. `options` may carry `reason` (s) and `handle_token` (s). ([Flatpak][5])

### Rust crate

```toml
ashpd = "0.13"
```

`ashpd` (current: **0.13.12**, as of mid-2026) is the Rust wrapper for XDG portals using `zbus`; it is the Rust alternative to `libportal`. ([Docs.rs][6])

### C library

```text
libportal
```

`libportal` provides GIO-style async APIs for portals, and its session portal maps to `org.freedesktop.portal.Inhibit`. ([LibPortal][7])

### Important caveat

Portal inhibit is session/desktop-level. It is not a magic kernel-level lid-close blocker. It is excellent as a secondary backend, not as your only backend.

---

## 3. `org.freedesktop.ScreenSaver` idle fallback

This is old but still useful as a fallback for idle/session inhibition.

API:

```text
org.freedesktop.ScreenSaver.Inhibit(application_name, reason) -> cookie
org.freedesktop.ScreenSaver.UnInhibit(cookie)
```

The cookie is a `UInt32`, at object path `/org/freedesktop/ScreenSaver`, interface `org.freedesktop.ScreenSaver`.

It only inhibits **idleness**, not full sleep/shutdown/lid handling. The freedesktop spec explicitly says it does not support suspend, hibernation, or user switching, and that user-requested actions still happen. ([Freedesktop Specifications][8])

### Rust crate

Use:

```toml
zbus = "5"
```

No special crate needed.

### Use it for

```text
GNOME-ish fallback
KDE-ish fallback
XFCE/MATE/Cinnamon fallback if available
old Linux desktop fallback
```

### Do not market it as

```text
prevents laptop sleep
```

It is more like:

```text
prevents idle screen blank / idle lock / idle-triggered actions where the session honors it
```

---

## 4. GNOME-specific backend

GNOME has:

```bash
gnome-session-inhibit
```

It calls GNOME SessionManager's D-Bus `Inhibit()` method and the inhibitor is automatically removed when `gnome-session-inhibit` exits. It supports actions:

```text
logout
switch-user
suspend
idle
automount
```

If no action is specified, GNOME assumes `idle`. ([Gnome Pages][9])

### Rust implementation

Use `zbus` directly against:

```text
service:  org.gnome.SessionManager
path:     /org/gnome/SessionManager
method:   org.gnome.SessionManager.Inhibit
```

Note that the raw GNOME D-Bus `Inhibit` signature is distinct from the ScreenSaver one:

```text
Inhibit(app_id: String, toplevel_xid: UInt32, reason: String, flags: UInt32) -> cookie: UInt32
```

where `flags` is a bitmask (`1 = logout`, `2 = switch-user`, `4 = suspend`, `8 = idle`, `16 = automount`). Encode it with those four arguments.

I would not make this the first backend. Use it after:

```text
systemd-logind failed
portal failed
```

### Why

GNOME on Wayland can be weird. Direct compositor protocols are often not enough. GNOME's D-Bus/session APIs and portals are usually safer than trying to hack Wayland directly.

---

## 5. KDE / Plasma backend

KDE/Plasma has PowerDevil/Solid policy inhibition.

The interface is:

```text
service/path: org.kde.Solid.PowerManagement.PolicyAgent
              /org/kde/Solid/PowerManagement/PolicyAgent
AddInhibition(u, s, s) -> u
ReleaseInhibition(u)
```

The first `u` argument is a bitmask of power-management actions to inhibit (e.g. `ChangeProfile = 1`, `ChangeScreenSettings = 2`, etc.); the two `s` arguments are application name and reason, and the returned `u` is the inhibition cookie. KDE's XML introspection shows `AddInhibition` returning a cookie and `ReleaseInhibition` taking that cookie. ([GitHub][10])

### Rust implementation

Again:

```toml
zbus = "5"
```

### Use it as

```text
KDE-specific fallback / doctor signal
```

Not as primary backend.

Reason: systemd-logind is the stronger system-level mechanism. KDE PowerDevil is desktop policy. In some Plasma setups, the desktop layer may handle lid/power behavior itself, so your `doctor` should show both logind inhibitors and KDE/PowerDevil state.

---

## 6. X11 fallback: XScreenSaver + DPMS

For X11, there are two separate things:

```text
XScreenSaverSuspend()  → screensaver/idle behavior (and the DPMS timer)
xset -dpms             → display power management
```

The X Screen Saver extension (libXss, header `X11/extensions/scrnsaver.h`, available since extension v1.1 / X11R7.1) provides:

```c
void XScreenSaverSuspend(Display *dpy, Bool suspend);
```

A client must call `XScreenSaverQueryExtension` before any other XScreenSaver function, and note that `XScreenSaverSuspend(True)` actually suspends **both** the screensaver timer **and** the DPMS timer. ([Linux Documentation][11])

`xset -dpms` disables DPMS power-saving, and `xset s off -dpms` is the classic "don't blank screen / don't DPMS sleep display" combination. ([X.Org][12])

### Rust crates

```toml
x11rb = "0.13"
```

Pure-Rust X11 protocol bindings (current: **0.13.2**, as of mid-2026); good for X11 protocol access. ([Docs.rs][13])

Or (the C-linkage alternatives to avoid unless you need them):

```toml
xcb = "1"
x11-dl = "2.21"
```

`xcb` wraps libxcb (current **1.7.0**), `x11-dl` dynamically loads Xlib at runtime (current **2.21.0**, the newest release but ~3 years old). Avoid X11 C linkage unless you need it.

### C libs

```text
libX11
libXss
libXext
libxcb
```

### Caveat

This is display/idle-level, not system-level sleep/lid protection. Use it as a fallback only.

---

## 7. Wayland idle-inhibit backend

Wayland has:

```text
zwp_idle_inhibit_manager_v1
```

It can inhibit idle behavior such as screen blanking, locking, and screensaving, but it requires a `wl_surface`. The inhibitor is created via `zwp_idle_inhibit_manager_v1_create_inhibitor(wl_surface)` and is bound to that surface; destroying the inhibitor object uninhibits. If the surface is destroyed, unmapped, occluded, or no longer visually relevant, the compositor may ignore the inhibitor. The protocol is also marked experimental/unstable (`unstable-v1`). ([Wayland][14])

### Rust crates

```toml
wayland-client = "0.31"
wayland-protocols = "0.32"
```

`wayland-client` (current: **0.31.14**, as of mid-2026) is a binding to the standard C `libwayland-client` reference implementation (i.e. an FFI binding, not a pure-Rust stack), and `wayland-protocols` (current: **0.32.13**, as of mid-2026) provides protocol definitions beyond core Wayland. ([Docs.rs][15])

### Important for Oxiwake

For a pure CLI daemon, **Wayland idle-inhibit is not your primary backend** because it wants a visible/relevant surface.

So do not build Oxiwake around Wayland directly.

Use:

```text
systemd-logind first
portal second
Wayland protocol only as optional advanced backend
```

---

# Windows backend plan

## 1. Best backend: `PowerCreateRequest` + `PowerSetRequest`

This is the better Windows backend for Oxiwake.

Use:

```text
PowerCreateRequest(REASON_CONTEXT)
PowerSetRequest(handle, PowerRequestSystemRequired)
PowerSetRequest(handle, PowerRequestExecutionRequired)
optional: PowerSetRequest(handle, PowerRequestDisplayRequired)
PowerClearRequest(...)
CloseHandle(...)
```

Microsoft documents these request types:

```text
PowerRequestDisplayRequired   → display remains on
PowerRequestSystemRequired    → system continues running instead of idle sleep
PowerRequestExecutionRequired → process not suspended/terminated by process lifetime management (Windows 8 / Server 2012+; on Traditional Sleep/S3 systems it implies SystemRequired)
```

Microsoft also says `PowerRequestDisplayRequired` alone is not enough; you also need `PowerRequestSystemRequired` if you want the display on and the system awake. ([Microsoft Learn][16])

### Rust crates

Use:

```toml
windows-sys = "0.61"
```

or:

```toml
windows = "0.62"
```

(current: `windows-sys` **0.61.2**, `windows` **0.62.2**, as of mid-2026). I recommend `windows-sys` for a tiny CLI because it is lower-level and generally better for small system utilities.

Required `windows-sys` features (verified against the 0.61.x bindings):

```text
Win32_System_Power       # PowerCreateRequest / PowerSetRequest / PowerClearRequest,
                         # POWER_REQUEST_TYPE (all PowerRequest* variants),
                         # SetThreadExecutionState + all ES_* constants
Win32_System_Threading   # REASON_CONTEXT (the struct PowerCreateRequest takes) — required
Win32_Foundation         # HANDLE, CloseHandle, BOOL
```

`Win32_System_WindowsProgramming` is sometimes listed in older `winapi`-era guides but is **not** needed here — none of these symbols live there, so omit it. `Win32_System_Threading` is mandatory specifically because `REASON_CONTEXT` is defined there. Exact feature names may vary by crate version, so confirm from current `windows-sys` docs when implementing. A target-gated dependency keeps it off non-Windows builds:

```toml
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61", features = [
    "Win32_System_Power",
    "Win32_System_Threading",
    "Win32_Foundation",
] }
```

### Important caveats

Windows will still not let you override everything. Microsoft says on Modern Standby systems on battery/DC, system/execution requests can be terminated five minutes after the system sleep timeout expires. Also, except for Away Mode on old S3 systems, power requests are terminated on user-initiated sleep such as power button, lid close, or selecting Sleep. ([Microsoft Learn][16])

So your doctor output should say:

```text
Windows wake lock: supported
Idle sleep: blocked
Lid-close/manual sleep: not guaranteed
Modern Standby on battery: limited by Windows policy
```

For honesty, the Windows doctor should also report whether the power request actually *persists* versus being subject to Windows policy limits (e.g. Modern Standby on DC terminating requests after the sleep timeout, and user-initiated sleep clearing requests).

---

## 2. Simple fallback: `SetThreadExecutionState`

Microsoft explicitly documents `SetThreadExecutionState` for telling Windows your app is busy so the system should not sleep or turn off the display while the app is running. Use:

```text
ES_CONTINUOUS | ES_SYSTEM_REQUIRED
```

Optional display mode:

```text
ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
```

Clear:

```text
ES_CONTINUOUS
```

Microsoft documents that `ES_SYSTEM_REQUIRED` keeps the system in the working state by resetting the system idle timer, and `ES_DISPLAY_REQUIRED` forces the display on by resetting the display idle timer. ([Microsoft Learn][17])

### Use this for

```text
fallback
simple backend
old Windows support
low-code first version
```

### Prefer this?

For v0.1, yes, you can ship with this quickly.

For a polished Oxiwake, prefer:

```text
PowerCreateRequest + PowerSetRequest
```

because it gives better diagnostics and shows up as a proper power request.

---

## 3. Windows diagnostics

Use:

```bash
powercfg /requests
powercfg /availablesleepstates
```

Microsoft documents `/requests` as enumerating application and driver Power Requests, and `/availablesleepstates` as reporting available sleep states. ([Microsoft Learn][18])

For `ow doctor`, shelling out to `powercfg` is perfectly acceptable.

---

# Existing Rust crates/tools worth reading

## `keepawake`

Existing cross-platform Rust crate/CLI (current: **0.6.0**, actively maintained):

```text
keepawake
```

It describes itself as "Keep your computer awake," similar to `caffeinate`, `systemd-inhibit` / `gnome-session-inhibit`, or PowerToys Awake, but cross-platform and written in Rust. Its backends match what Oxiwake plans: Windows `SetThreadExecutionState`, macOS `IOPMAssertionCreateWithName`, Linux `org.freedesktop.ScreenSaver` + systemd-logind inhibitor locks. Read it for prior art, but I would not depend on it for Oxiwake unless its architecture matches your daemon/toggle needs. ([Crates.io][19])

## `donotsleep`

Existing cross-platform CLI (current: **0.1.5**, actively maintained):

```text
donotsleep
```

A 3-platform CLI to keep your system awake — macOS wraps `caffeinate -dimsu`, Linux wraps `systemd-inhibit`, Windows uses Win32 `SetThreadExecutionState`. Again, good prior art, but Oxiwake's differentiator should be cleaner state/toggle UX and stronger diagnostics. ([Lib.rs][20])

## `keep-active`

Another adjacent Rust crate/tool (current: **0.2.0**, actively maintained). It is a **fork of `keepawake-rs`** whose distinguishing feature is *activity/presence simulation* — `--status-active` nudges the mouse or taps F15 to keep status trackers like Skype/MS Teams showing you as "active," on top of the usual keep-awake functionality. Worth reading, not necessarily using. ([Crates.io][21])

## `nosleep-windows`

Windows-only slice of the cross-platform `pevers/nosleep` workspace, to block power-save / sleep. **Unmaintained**: latest version **0.2.1** was published 2022-11-20 (3.5+ years stale as of mid-2026). Maybe useful for ideas, but I would rather directly use `windows-sys` and own the backend. ([Crates.io][22])

---

# Recommended dependency stack

For Oxiwake, I would build your own backends instead of depending on a "keep awake" crate.

## Core

```toml
clap = "4"
serde = "1"
serde_json = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = "0.3"
```

Current latest versions (as of mid-2026): `clap` **4.6.1**, `serde` **1.0.228**, `serde_json` **1.0.150**, `thiserror` **2.0.18** (note the 2.x line, not 1.x), `tracing` **0.1.44**, `tracing-subscriber` **0.3.23**.

## Linux

```toml
zbus = "5"
ashpd = "0.13"
logind-zbus = { version = "5", optional = true }
```

Current latest (as of mid-2026): `zbus` **5.16.0**, `ashpd` **0.13.12**, `logind-zbus` **5.3.2**.

> **Implementation note.** The recommendation above lists `ashpd` (portal) and `logind-zbus` (logind) as convenience wrappers, but the actual Oxiwake implementation does **not** use either — it hand-rolls every D-Bus backend (logind, portal, GNOME, KDE, ScreenSaver) directly on raw `zbus`. This is deliberate: the whole stack is fully synchronous (no async runtime, no `.await`, no extra deps), and `zbus`'s blocking API is enough to drive each interface by hand. So in practice the only Linux D-Bus dependency that ships is `zbus` itself; `ashpd` and `logind-zbus` stay listed here as the originally-recommended, but unused, options.

Optional (kept behind feature flags):

```toml
wayland-client = { version = "0.31", optional = true }
wayland-protocols = { version = "0.32", optional = true }
x11rb = { version = "0.13", optional = true }
```

But I would keep Wayland/X11 optional features. In `Cargo.toml` use a proper top-level `[features]` section (inline tables cannot span multiple lines in TOML), and remember every optional dependency must be declared with `optional = true` (as above) and then named under a feature:

```toml
[features]
default = ["linux-logind", "linux-portal"]
linux-logind = ["zbus"]
linux-portal = ["ashpd"]
linux-x11 = ["x11rb"]
linux-wayland = ["wayland-client", "wayland-protocols"]
```

## Windows

```toml
windows-sys = "0.61"
```

Optional if you prefer higher-level APIs:

```toml
windows = { version = "0.62", optional = true }
```

Current latest (as of mid-2026): `windows-sys` **0.61.2**, `windows` **0.62.2**.

---

# Backend priority order

## Linux auto backend

Use this order:

```text
1. systemd-logind D-Bus
2. XDG Desktop Portal Inhibit
3. GNOME SessionManager Inhibit
4. KDE PowerDevil/Solid Inhibit
5. org.freedesktop.ScreenSaver
6. X11 XScreenSaver/DPMS
7. Wayland idle-inhibit only if explicitly enabled and feasible
```

Reason:

```text
systemd-logind = real system sleep/lid/idle backend
portal = desktop/session-safe backend
GNOME/KDE = DE-specific fallback
ScreenSaver/X11/Wayland = mostly idle/display fallback
```

## Windows auto backend

Use this order:

```text
1. PowerCreateRequest + PowerSetRequest
2. SetThreadExecutionState
3. powercfg only for diagnostics
```

---

# Oxiwake state model

Use this:

```rust
pub enum WakeTarget {
    SystemSleep,
    Idle,
    Display,
    LidSwitch,
    Shutdown,
}

pub enum WakeMode {
    Block,
    Delay,
    BlockWeak,
}

pub struct WakeRequest {
    pub targets: Vec<WakeTarget>,
    pub reason: String,
    pub display: bool,
    pub aggressive_lid: bool,
}

pub trait WakeBackend {
    fn name(&self) -> &'static str;
    fn supported(&self) -> bool;
    fn acquire(&self, req: WakeRequest) -> Result<Box<dyn WakeGuard>>;
    fn status(&self) -> Result<WakeStatus>;
    fn doctor(&self) -> Result<DoctorReport>;
}

pub trait WakeGuard {
    fn backend(&self) -> &'static str;
}
```

`WakeGuard` must be RAII. Dropping it releases the lock — a `Drop` impl on the concrete guard closes the systemd-logind inhibitor FD on Linux (or calls `PowerClearRequest` + `CloseHandle` on Windows). Because systemd-logind releases the inhibitor the moment the returned FD is closed, this tie between lock ownership and the guard's lifetime is the correct, leak-safe design.

For `ow on`, start daemon:

```text
ow on
  → spawn oxiwake daemon
  → daemon acquires WakeGuard
  → daemon writes state file
  → CLI exits
```

For `ow off`:

```text
ow off
  → connect to daemon IPC
  → ask daemon to exit
  → guard drops
  → FD/handle closes
```

State location:

```text
Linux:   $XDG_RUNTIME_DIR/oxiwake/state.json
         $XDG_RUNTIME_DIR/oxiwake/pending.{pid}.json   (per-invocation)
         $XDG_RUNTIME_DIR/oxiwake/oxiwake.lock          (singleton mutex)
Windows: %LOCALAPPDATA%\Freeoxide\Oxiwake\state.json
         %LOCALAPPDATA%\Freeoxide\Oxiwake\pending.{pid}.json   (per-invocation)
         %LOCALAPPDATA%\Freeoxide\Oxiwake\oxiwake.lock          (singleton mutex)
```

`state.json` is the persistent lock snapshot the daemon writes once the guard is live (so `ow status` works without a live IPC) and removes on the way out. `pending.{pid}.json` is the **transient, per-invocation request hand-off** between the CLI and the freshly-spawned daemon: keyed on the CLI's own pid so two concurrent `ow on` invocations never overwrite each other's request, the CLI writes it (`{"request": …, "started_unix": …}`) and passes its path to `ow __daemon`, which reads it back on startup to recover the request and timestamp and then deletes it. `ow on` polls for daemon liveness via `state.json` plus an IPC `Ping`, not via this file, so it should never linger on success. `oxiwake.lock` is the singleton advisory lock (`flock` on Linux, `LockFileEx` on Windows) the daemon holds for its entire lifetime: it is the atomic mutex that guarantees two racing `ow on` invocations can never both reach OS-lock acquisition, and it auto-releases when the daemon's fd/handle closes (including on crash).

`$XDG_RUNTIME_DIR` (typically `/run/user/$UID`, a tmpfs created by `pam_systemd` at login, mode 0700) is the conventional base for user-specific non-essential runtime files and sockets, but it has no guaranteed default — the code should fall back or fail clearly when unset. Since `state.json`, `pending.{pid}.json`, and `oxiwake.lock` all live in a tmpfs they are wiped on logout/reboot, which is fine for transient lock state but the design must not rely on it persisting across reboots. On Windows, prefer resolving `%LOCALAPPDATA%` via `SHGetKnownFolderPath(FOLDERID_LocalAppData)` / the KnownFolders API rather than reading the raw env var, since the env var can be absent or mis-set.

IPC:

```text
Linux: Unix domain socket in $XDG_RUNTIME_DIR/oxiwake/oxiwake.sock
Windows: named pipe
```

---

# What `ow doctor` should check

## Linux

```text
OS/distro info from /etc/os-release
systemd-logind D-Bus reachable?
Can call CanSuspend?
ListInhibitors available?
XDG_CURRENT_DESKTOP
XDG_SESSION_TYPE = wayland/x11
WAYLAND_DISPLAY present?
DISPLAY present?
org.freedesktop.portal.Desktop reachable?
org.freedesktop.ScreenSaver reachable?
org.gnome.SessionManager reachable?
KDE PowerDevil reachable?
LidClosed / Docked / OnExternalPower from logind
```

All of these are real, probeable signals: `CanSuspend`, `ListInhibitors`, `LidClosed`, `Docked`, `OnExternalPower` are genuine `org.freedesktop.login1.Manager` D-Bus members; the `XDG_*` / `WAYLAND_DISPLAY` / `DISPLAY` entries are standard session env vars; and `org.freedesktop.portal.Desktop`, `org.freedesktop.ScreenSaver`, `org.gnome.SessionManager`, and `org.kde.Solid.PowerManagement.PolicyAgent` are real D-Bus names.

## Windows

```text
Windows version
Modern Standby available?
PowerCreateRequest available?
PowerSetRequest works?
SetThreadExecutionState works?
powercfg /requests output
powercfg /availablesleepstates output
running on AC or battery
does the power request actually persist, or is it subject to Windows policy limits?
```

The last line keeps the doctor honest about guarantees — on Modern Standby/DC the request may be terminated after the sleep timeout, and user-initiated sleep clears requests.

---

# Final recommendation

Build Oxiwake with **two serious backends first**:

```text
Linux:   systemd-logind via zbus
Windows: PowerCreateRequest/PowerSetRequest via windows-sys
```

Then add:

```text
Linux portal backend via ashpd
Linux doctor mode
GNOME/KDE fallbacks
X11 fallback
Wayland optional backend
```

Do **not** make Wayland/X11 the core. Do **not** special-case Ubuntu/Debian/Nix/Bazzite/Pop/Rocky directly. Detect capabilities and choose the backend.

Best v0.1 scope:

```bash
ow
ow on
ow off
ow toggle
ow status
ow doctor
```

Best honest tagline:

```text
Oxiwake keeps your machine awake where the OS allows it — and tells you when it cannot.
```

---

## References

- [1] <https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html> — org.freedesktop.login1(5). (Bot-blocked to programmatic scrapers but resolves fine in a browser; the identical content is also mirrored at <https://manpages.debian.org/testing/systemd/org.freedesktop.login1.5.en.html> and <https://manpages.ubuntu.com/manpages/jammy/man5/org.freedesktop.login1.5.html>.)
- [2] <https://man7.org/linux/man-pages/man1/systemd-inhibit.1.html> — systemd-inhibit(1) - Linux manual page
- [3] <https://docs.rs/zbus/5.16.0/zbus/> — zbus - Rust (version-stable link)
- [4] <https://crates.io/crates/logind-zbus> — logind-zbus - crates.io: Rust Package Registry
- [5] <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Inhibit.html> — Inhibit - XDG Desktop Portal documentation
- [6] <https://docs.rs/crate/ashpd/0.13.12> — ashpd 0.13.12
- [7] <https://libportal.org/libportal.html> — Xdp – 1.0: libportal Reference Manual
- [8] <https://specifications.freedesktop.org/idle-inhibit/0.1/> — Idle Inhibition Service Draft (canonical freedesktop source)
- [9] <https://gnome.pages.gitlab.gnome.org/gnome-session/re02.html> — gnome-session-inhibit
- [10] <https://github.com/KDE/solid-power/blob/master/src/org.kde.Solid.PowerManagement.PolicyAgent.xml> — solid-power PolicyAgent introspection XML
- [11] <https://linux.die.net/man/3/xscreensaversuspend> — xscreensaversuspend(3) - Linux man page
- [12] <https://xorg.freedesktop.org/archive/X11R7.5/doc/man/man1/xset.1.html> — XSET(1) manual page
- [13] <https://docs.rs/x11rb> — x11rb - Rust
- [14] <https://wayland.app/protocols/idle-inhibit-unstable-v1> — Idle inhibit protocol | Wayland Explorer
- [15] <https://docs.rs/wayland-client> — wayland_client - Rust
- [16] <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-powersetrequest> — PowerSetRequest function (winbase.h) - Win32 apps | Microsoft Learn
- [17] <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setthreadexecutionstate> — SetThreadExecutionState function (winbase.h) - Win32 apps
- [18] <https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/powercfg-command-line-options> — Powercfg command-line options | Microsoft Learn
- [19] <https://crates.io/crates/keepawake> — keepawake - crates.io: Rust Package Registry
- [20] <https://lib.rs/crates/donotsleep> — DoNotSleep - Command line utilities
- [21] <https://crates.io/crates/keep-active> — keep-active - crates.io: Rust Package Registry
- [22] <https://crates.io/crates/nosleep-windows> — nosleep-windows - crates.io: Rust Package Registry
