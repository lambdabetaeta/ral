//! Length-prefixed JSON frame codec for the sandbox IPC socket.
//!
//! Each frame is a 4-byte little-endian length followed by that many
//! bytes of `serde_json`.  Reading the length returns `Ok(None)` on a
//! clean EOF at a message boundary; any other partial read is an error.
//!
//! On a deserialise failure the raw bytes are written to a tmpfile and
//! the path is appended to the error — useful for diagnosing wire-format
//! mismatches between parent and child binaries.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{self, Read, Write};

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::other("sandbox: frame exceeds 4 GiB"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len_buf[got..])? {
            0 if got == 0 => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "sandbox: partial frame length",
                ));
            }
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    match serde_json::from_slice(&body) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            // Best-effort dump: write raw bytes to a unique tmpfile and
            // include the path in the error so the caller can inspect
            // the slice that failed.  Silent on dump failure.
            let path = std::env::temp_dir().join(format!(
                "ral-ipc-fail-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            ));
            let _ = std::fs::write(&path, &body);
            Err(io::Error::other(format!(
                "{e} (raw frame written to {})",
                path.display()
            )))
        }
    }
}
