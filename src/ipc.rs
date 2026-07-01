//! Newline-delimited JSON framing for the daemon IPC protocol.
//!
//! Oxiwake's daemon and CLI talk over a single request/reply stream (a Unix
//! socket on Linux, a named pipe on Windows). Every message is exactly one
//! [`serde_json`] value serialized compactly and terminated by a single `\n`.
//! One line in, one line out — there is no length prefix and no streaming.
//!
//! This module is transport-agnostic: it only knows how to serialize a value
//! onto a writer ([`write_msg`]) and how to read the next line off a reader
//! and deserialize it ([`read_msg`]). The platform layer
//! ([`crate::platform`]) supplies the actual byte stream.
//!
//! Robustness notes:
//!
//! - [`read_msg`] consumes bytes up to and including the next `\n`. Trailing
//!   whitespace inside the line (spaces, tabs, a stray `\r` from a CRLF
//!   transport) is trimmed before parsing, and serde itself ignores leading
//!   whitespace, so a frame is tolerant of incidental padding.
//! - [`write_msg`] serializes compactly (no pretty-printing) and always emits
//!   exactly one trailing `\n`. It does **not** flush — callers wrap the
//!   stream in a [`std::io::BufWriter`] and flush explicitly after sending a
//!   message, so a request and its reply can each be a single buffered write.
//!
//! Because the framing contract is `R: std::io::Read`, [`read_msg`] reads one
//! byte at a time until it sees the framing newline. Oxiwake frames are tiny
//! (a single enum variant or a small status struct), so this is not a
//! performance concern in practice; callers are still encouraged to wrap the
//! stream in a [`std::io::BufReader`] so the per-byte reads hit an in-memory
//! buffer rather than a syscall.

use std::io::{Read, Write};

use crate::error::{OxiwakeError, Result};
use crate::model::WakeStatus;

/// A single client-to-daemon request.
///
/// Deliberately tiny: the daemon either answers a probe ([`Ping`]/[`Status`])
/// or is told to release its lock and exit ([`Stop`]). Everything else about
/// the active lock lives in `state.json`, written by the daemon and read by
/// the CLI without IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ClientMsg {
    /// "Are you there?" The daemon answers with its PID and current status.
    Ping,
    /// "What lock do you hold?" The daemon answers with its current status.
    Status,
    /// "Release the lock and exit." The daemon replies, then shuts down.
    Stop,
}

/// A single daemon-to-client reply.
///
/// Every field except `ok` is optional so a reply can carry just the
/// information relevant to the request that produced it. `ok` is the load-
/// bearing field: when it is `false`, `error` carries a human-readable
/// explanation and the other fields are `None`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DaemonReply {
    /// `true` if the request succeeded.
    pub ok: bool,
    /// A human-readable error message when `ok` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The daemon's current lock status, when the request asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<WakeStatus>,
    /// The daemon's PID, included in `Ping` replies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl DaemonReply {
    /// Build a successful reply carrying a status snapshot.
    pub fn ok_status(status: WakeStatus) -> Self {
        DaemonReply {
            ok: true,
            error: None,
            status: Some(status),
            pid: None,
        }
    }

    /// Build a successful `Ping` reply: status plus the daemon PID.
    pub fn ok_ping(status: WakeStatus, pid: u32) -> Self {
        DaemonReply {
            ok: true,
            error: None,
            status: Some(status),
            pid: Some(pid),
        }
    }

    /// Build a bare successful reply (no payload), e.g. for `Stop`.
    pub fn ok_empty() -> Self {
        DaemonReply {
            ok: true,
            error: None,
            status: None,
            pid: None,
        }
    }

    /// Build a failure reply carrying an error message.
    pub fn err(message: impl Into<String>) -> Self {
        DaemonReply {
            ok: false,
            error: Some(message.into()),
            status: None,
            pid: None,
        }
    }
}

/// Serialize `v` as compact JSON, write it, and append a single newline.
///
/// Does **not** flush the writer — pair it with a [`std::io::BufWriter`] and
/// call [`std::io::BufWriter::flush`] (or drop the writer) to ensure the
/// bytes leave the process. Returning without flushing is intentional: it
/// lets a caller that writes a request then waits for a reply issue the whole
/// request in one buffered chunk.
///
/// # Errors
///
/// Returns [`OxiwakeError::Ipc`] if serialization fails (which, for our enum
/// / struct shapes, is effectively impossible) or [`OxiwakeError::Io`] if the
/// underlying write fails.
pub fn write_msg<W: Write>(w: &mut W, v: &impl serde::Serialize) -> Result<()> {
    // Compact serialization keeps each frame to a single line. `to_vec` lets
    // us side-step lifetime gymnastics with the serializer's borrow of `w`.
    let bytes = serde_json::to_vec(v)
        .map_err(|e| OxiwakeError::Ipc(format!("could not serialize message: {e}")))?;
    w.write_all(&bytes)?;
    w.write_all(b"\n")?;
    Ok(())
}

/// Read the next newline-terminated JSON frame from `r` and deserialize it.
///
/// Bytes are consumed up to and including the next `\n`. The captured line is
/// trimmed of trailing whitespace (including a carriage return left by a CRLF
/// transport) before parsing, and serde tolerates any leading whitespace, so
/// incidental padding never breaks a frame.
///
/// If the reader reaches end-of-file before a full line is available, this is
/// reported as [`OxiwakeError::NotRunning`] — for Oxiwake that means the
/// daemon closed the connection (or never opened one), which callers uniformly
/// want to treat as "no daemon". A writer that closes mid-line without a
/// trailing newline is handled gracefully: the partial bytes are treated as
/// the frame.
///
/// Callers are encouraged to wrap raw streams in a [`std::io::BufReader`] so
/// the per-byte reads hit an in-memory buffer.
///
/// # Errors
///
/// [`OxiwakeError::NotRunning`] on a clean EOF with no data,
/// [`OxiwakeError::Ipc`] on a malformed frame or deserialization failure, and
/// [`OxiwakeError::Io`] on a low-level read error.
pub fn read_msg<R: Read, T: serde::de::DeserializeOwned>(r: &mut R) -> Result<T> {
    let line = read_line(r)?;

    let trimmed = line.trim_ascii();
    if trimmed.is_empty() {
        return Err(OxiwakeError::Ipc("received an empty IPC frame".to_string()));
    }

    serde_json::from_slice::<T>(trimmed).map_err(|e| {
        OxiwakeError::Ipc(format!(
            "could not parse IPC frame ({e}): {:?}",
            String::from_utf8_lossy(trimmed)
        ))
    })
}

/// Read one newline-terminated line from `r`, byte by byte.
///
/// Stops after the first `\n` (which is discarded) and returns the
/// accumulated bytes. A `0`-byte read before any `\n` is a clean EOF and maps
/// to [`OxiwakeError::NotRunning`] when nothing has been read yet; if the
/// writer closed after sending a partial line, the partial bytes are returned
/// as the frame. `Interrupted` reads are retried transparently.
fn read_line<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => {
                if buf.is_empty() {
                    return Err(OxiwakeError::NotRunning);
                }
                // The writer closed mid-line without a newline. Treat whatever
                // we have as the frame rather than dropping it on the floor.
                return Ok(buf);
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(buf);
                }
                buf.push(byte[0]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(OxiwakeError::from(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_client_msg() {
        for msg in [ClientMsg::Ping, ClientMsg::Status, ClientMsg::Stop] {
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).unwrap();
            assert_eq!(buf.last(), Some(&b'\n'));
            let mut cursor = std::io::Cursor::new(buf);
            let back: ClientMsg = read_msg(&mut cursor).unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn roundtrip_daemon_reply() {
        let reply = DaemonReply::ok_ping(
            WakeStatus {
                backend: "systemd-logind".to_string(),
                targets: vec![crate::model::WakeTarget::SystemSleep],
                mode: crate::model::WakeMode::Block,
                display: false,
            },
            4242,
        );
        let mut buf = Vec::new();
        write_msg(&mut buf, &reply).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back: DaemonReply = read_msg(&mut cursor).unwrap();
        assert!(back.ok);
        assert_eq!(back.pid, Some(4242));
        assert!(back.status.is_some());
        assert!(back.error.is_none());
    }

    #[test]
    fn tolerates_trailing_whitespace_and_crlf() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &ClientMsg::Ping).unwrap();
        // Clobber the trailing `\n` with a CRLF + spaces to simulate a noisy
        // transport. `read_msg` must still recover the Ping.
        buf.pop();
        buf.extend_from_slice(b" \r\n  ");
        let mut cursor = std::io::Cursor::new(buf);
        let back: ClientMsg = read_msg(&mut cursor).unwrap();
        assert_eq!(back, ClientMsg::Ping);
    }

    #[test]
    fn eof_with_no_data_is_not_running() {
        let mut empty = std::io::Cursor::new(Vec::new());
        let err: Result<ClientMsg> = read_msg(&mut empty);
        assert!(matches!(err, Err(OxiwakeError::NotRunning)));
    }

    #[test]
    fn empty_frame_is_ipc_error() {
        let mut cursor = std::io::Cursor::new(b"\n".to_vec());
        let err: Result<ClientMsg> = read_msg(&mut cursor);
        assert!(matches!(err, Err(OxiwakeError::Ipc(_))));
    }
}
