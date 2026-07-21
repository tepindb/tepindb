//! Length-prefixed (u32 big-endian) JSON frames — one request, one
//! response, in order, over a local socket.

use std::io::{Read, Write};

use serde_json::Value;

/// Serving moves query results, not bulk data; anything bigger than this
/// is a protocol violation, not a payload.
const MAX_FRAME: u32 = 64 * 1024 * 1024;

pub(crate) fn write_frame(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(v)?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME)
        .ok_or_else(|| std::io::Error::other("frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// `Ok(None)` is a clean end of stream (peer hung up between frames).
pub(crate) fn read_frame(r: &mut impl Read) -> std::io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    if let Err(e) = r.read_exact(&mut len) {
        return match e.kind() {
            std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe => Ok(None),
            _ => Err(e),
        };
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| std::io::Error::other(format!("bad frame: {e}")))
}
