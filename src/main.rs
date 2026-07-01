//! `ow` — the Oxiwake command-line entry point.
//!
//! This binary is a thin shell: it sets up [`tracing`], parses the CLI
//! ([`oxiwake::cli`]), dispatches to the right subsystem, and renders the
//! result via [`oxiwake::output`]. Every piece of real work — picking a
//! backend, owning the lock, talking to the daemon — lives in the library so
//! it can be reused and tested without spawning a process.
//!
//! # The clock
//!
//! `started_unix` is read from the system clock *here*, once, via
//! [`std::time::SystemTime`], and passed down. Library logic never reads the
//! clock itself, so it stays deterministic.
//!
//! # Exit codes
//!
//! `0` on success (including a clean "off" status). Non-zero on any real
//! [`OxiwakeError`] (paths unresolved, daemon failed to start, etc.). Errors
//! are printed humanably via the `Display` impl in `error.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

use oxiwake::cli::{self, Cli, Commands, RequestFlags};
use oxiwake::daemon;
use oxiwake::doctor;
use oxiwake::error::{OxiwakeError, Result};
use oxiwake::ipc::ClientMsg;
use oxiwake::output;
use oxiwake::paths::Paths;
use oxiwake::state::LockState;

fn main() {
    let cli = cli::parse();

    init_tracing(cli.verbose);

    // Dispatch. Any error becomes a non-zero exit with a human message.
    match run(&cli) {
        Ok(()) => {}
        Err(e) => {
            // Print to stderr so stdout stays clean for machine consumers.
            eprintln!("ow: {e}");
            std::process::exit(1);
        }
    }
}

/// Initialize the `tracing_subscriber` once.
///
/// The default filter comes from `RUST_LOG`; each `-v` bumps the level one
/// notch more verbose (`warn` -> `info` -> `debug` -> `trace`) so `-v` / `-vv`
/// are useful out of the box even without setting `RUST_LOG`. An explicit
/// `RUST_LOG` value always wins over the `-v` heuristic so power users keep
/// full control.
///
/// This is best-effort: if installing the subscriber fails (e.g. it was
/// already installed), we carry on rather than aborting the CLI.
fn init_tracing(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};

    // Start from the env, falling back to a verbosity-derived level. `RUST_LOG`
    // takes precedence because it is the more specific intent.
    let default_directive = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let env_set = std::env::var(EnvFilter::DEFAULT_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let filter = if env_set {
        // EnvFilter::from_default_env preserves a caller's RUST_LOG verbatim.
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(default_directive)
    };

    // Ignore the install error: tracing is observability, not correctness. A
    // failure to install the subscriber must not break the CLI.
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Read the wall clock once and return the current Unix timestamp in seconds.
///
/// Centralized here (rather than in library code) so library logic never
/// depends on the clock. A pre-`UNIX_EPOCH` clock is treated as 0 rather than
/// panicking.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Route the parsed CLI to the right subsystem and render its result.
///
/// This is the dispatch table. Each arm either prints to stdout (human or JSON,
/// per `--json`) or delegates to a small helper. Errors bubble up to [`main`].
fn run(cli: &Cli) -> Result<()> {
    // No subcommand -> behave as `ow status`.
    let command = cli.command.clone().unwrap_or(Commands::Status);

    match command {
        Commands::On {
            display,
            aggressive_lid,
            reason,
        } => cmd_on(cli, &RequestFlags::from_on(display, aggressive_lid, reason)),
        Commands::Off => cmd_off(cli),
        Commands::Toggle {
            display,
            aggressive_lid,
            reason,
        } => cmd_toggle(
            cli,
            &RequestFlags::from_toggle(display, aggressive_lid, reason),
        ),
        Commands::Status => cmd_status(cli),
        Commands::Doctor => cmd_doctor(cli),
        Commands::Daemon { pending } => {
            // Hidden internal entry point. Run the daemon loop directly; its
            // own error reporting (and exit) is handled by `main`.
            daemon::daemon_main(&pending)
        }
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

/// `ow on`: build the request from flags and start the daemon.
fn cmd_on(cli: &Cli, flags: &RequestFlags) -> Result<()> {
    let req = cli::build_request(flags);
    // The clock is read here in the binary, never inside library logic.
    let started = now_unix();
    let status = daemon::ensure_started(&req, started)?;

    if cli.json {
        // For a one-shot transition a simple envelope is all a machine
        // consumer needs; it carries the chosen backend + target count.
        println!(
            "{}",
            output::json_simple(
                "on",
                Some(&format!(
                    "backend={} targets={}",
                    status.backend,
                    status.targets.len()
                ))
            )
        );
    } else {
        println!(
            "{}",
            output::human_simple(
                "on",
                Some(&format!(
                    "{} holding {} target(s), display {}",
                    status.backend,
                    status.targets.len(),
                    if status.display { "on" } else { "off" }
                ))
            )
        );
    }
    Ok(())
}

/// `ow off`: stop the daemon if one is running.
fn cmd_off(cli: &Cli) -> Result<()> {
    daemon::ensure_stopped()?;

    if cli.json {
        println!("{}", output::json_simple("off", None));
    } else {
        println!("{}", output::human_simple("off", None));
    }
    Ok(())
}

/// `ow toggle`: flip the daemon's state.
///
/// We detect "is it running" the same way `status` does — a successful `Ping`
/// — so the toggle decision matches what the user would see from `ow status`.
/// If running, stop; otherwise start with the provided flags.
fn cmd_toggle(cli: &Cli, flags: &RequestFlags) -> Result<()> {
    if is_running()? {
        cmd_off(cli)
    } else {
        cmd_on(cli, flags)
    }
}

/// `ow status`: read persisted state, ping the daemon, and print.
///
/// A successful ping *and* a readable state file means the lock is live and we
/// can print the full picture. If the daemon is unreachable we print "off" —
/// this is not an error, it is a valid state.
fn cmd_status(cli: &Cli) -> Result<()> {
    // Try to read state. A missing runtime dir (State error) and a missing
    // state file both mean "off" from the user's perspective; only genuine
    // I/O / parse errors propagate.
    let state = match Paths::resolve() {
        Ok(p) => LockState::read(&p)?,
        // Runtime dir unset (e.g. no XDG_RUNTIME_DIR) -> treat as off.
        Err(OxiwakeError::State(_)) => None,
        Err(e) => return Err(e),
    };

    let reply = daemon::ipc_request(ClientMsg::Ping);

    // Determine whether the daemon is genuinely live. A ping error other than
    // NotRunning is unexpected but should surface, not silently report "off".
    let status = match reply {
        Ok(r) if r.ok => r.status,
        Ok(r) => {
            // The daemon answered but said !ok — surface its message rather than
            // guessing.
            return Err(OxiwakeError::Ipc(
                r.error.unwrap_or_else(|| "daemon replied !ok".to_string()),
            ));
        }
        Err(OxiwakeError::NotRunning) => None,
        Err(e) => return Err(e),
    };

    match (status, state) {
        (Some(status), Some(state)) => {
            let now = now_unix();
            if cli.json {
                println!("{}", output::json_status(&status, &state, now));
            } else {
                println!("{}", output::human_status(&status, &state, now));
            }
        }
        // Daemon unreachable with no state file: genuinely off.
        _ => {
            if cli.json {
                println!("{}", output::json_off());
            } else {
                println!("{}", output::human_off());
            }
        }
    }

    Ok(())
}

/// `ow doctor`: run the probe and print the report.
fn cmd_doctor(cli: &Cli) -> Result<()> {
    let report = doctor::run_doctor();
    if cli.json {
        println!("{}", output::json_doctor(&report));
    } else {
        println!("{}", output::human_doctor(&report));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Is the oxiwake daemon currently reachable?
///
/// Sends a single `Ping`. `NotRunning` (and an unset runtime dir) map to
/// `false`; any other error propagates so we never hide a real failure behind
/// a "not running" guess.
fn is_running() -> Result<bool> {
    match daemon::ipc_request(ClientMsg::Ping) {
        Ok(r) => Ok(r.ok),
        Err(OxiwakeError::NotRunning) => Ok(false),
        Err(OxiwakeError::State(_)) => Ok(false),
        Err(e) => Err(e),
    }
}
