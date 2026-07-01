//! Oxiwake — keep your machine awake where the OS allows it, and report when it cannot.
//!
//! This crate is both a library (used by integration tests and the daemon) and
//! the `ow` binary. The public surface is the [`model`] module: backends,
//! daemon, IPC and CLI are wired together in `main.rs` / their own modules.

pub mod error;
pub mod model;

// Subsystem modules. Each is owned by its own slice of the build; declaring
// them here is the module root wiring that lets the `ow` binary (and tests)
// reach them via `oxiwake::...`.
pub mod backend;
pub mod cli;
pub mod daemon;
pub mod doctor;
pub mod ipc;
pub mod output;
pub mod paths;
pub mod platform;
pub mod state;

pub use error::{OxiwakeError, Result};
