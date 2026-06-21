//! Wire protocol for the isolate worker.
//!
//! A strictly request/response framing over a pair of byte streams (the child's
//! stdin/stdout in production, a `socketpair` in tests). Each frame is a
//! little-endian `u32` length prefix followed by that many bytes of JSON. The
//! exchange is:
//!
//! 1. parent → child: one [`FromParent::Init`].
//! 2. child runs the agent; for every host op it emits a [`FromChild::Call`] and
//!    blocks for the matching [`FromParent::Reply`].
//! 3. child → parent: a final [`FromChild::Done`] carrying the run's result.
//!
//! There is no pipelining — the child has exactly one outstanding call at a time
//! — so the two sides never deadlock as long as each replies before reading the
//! next frame. JSON (not a binary codec) is deliberate for a first cut: it is
//! trivially debuggable and the per-effect cost is dwarfed by LLM/tool latency.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hard ceiling on a single frame's body (parent-side hardening: a hostile or
/// buggy child must not be able to make the parent allocate without bound).
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Parent → child messages.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromParent {
    /// The one-time handoff that starts a run. The child has no filesystem, so
    /// the entry source and the determinism prelude are shipped inline; sibling
    /// imports are resolved lazily by brokering `__module_load` back to us.
    Init {
        entry_path: String,
        entry_source: String,
        fallback_export: String,
        input: Value,
        /// `Some` for a runtime run (installs captured natives + prelude);
        /// `None` only for backends that wouldn't be isolated in the first place.
        prelude: Option<String>,
    },
    /// The result of one brokered host op.
    Reply(Outcome),
}

/// Child → parent messages.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromChild {
    /// A host op the child needs the parent to perform (`chidori.*` effect,
    /// `__chidori_*` native, `__chidori_dom_render`, or `__module_load`).
    Call { op: String, args: Value },
    /// The run finished; `outcome` is the agent's output or the error.
    Done { outcome: Outcome },
}

/// A fallible JSON value, serialized as a plain tagged enum so both an `Ok`
/// payload and an `Err` message survive the round trip.
#[derive(Debug, Serialize, Deserialize)]
pub enum Outcome {
    Ok(Value),
    Err(String),
}

impl From<Result<Value, String>> for Outcome {
    fn from(r: Result<Value, String>) -> Self {
        match r {
            Ok(v) => Outcome::Ok(v),
            Err(e) => Outcome::Err(e),
        }
    }
}

impl From<Outcome> for Result<Value, String> {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Ok(v) => Ok(v),
            Outcome::Err(e) => Err(e),
        }
    }
}

/// Write one length-prefixed JSON frame and flush it.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame of {} bytes exceeds the {MAX_FRAME_BYTES} cap",
                body.len()
            ),
        ));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame. A clean EOF before the length prefix
/// surfaces as `UnexpectedEof`, which callers treat as "the peer went away".
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_FRAME_BYTES} cap"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let mut buf: Vec<u8> = Vec::new();
        let msg = FromChild::Call {
            op: "log".into(),
            args: serde_json::json!({ "message": "hi" }),
        };
        write_frame(&mut buf, &msg).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back: FromChild = read_frame(&mut cursor).unwrap();
        match back {
            FromChild::Call { op, args } => {
                assert_eq!(op, "log");
                assert_eq!(args, serde_json::json!({ "message": "hi" }));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn oversized_length_prefix_is_rejected() {
        // A length prefix past the cap must error before allocating the body.
        let mut bytes = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(b"ignored");
        let mut cursor = std::io::Cursor::new(bytes);
        let err = read_frame::<_, FromChild>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
