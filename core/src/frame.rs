//! Length-prefixed JSON frames: the one framing codec on every ral IPC
//! channel — the sealed engine seed and the engine wire in
//! [`crate::protocol::channel`].

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, Read, Write};

/// Checked before the body is allocated, so no peer names a huge buffer.
const MAX_FRAME_LEN: u32 = 256 * 1024 * 1024;

/// The frame fuse, stated once and judged by both doors: the encoder refuses a
/// frame past it, with a sentence, before a byte is written, so the wire stays
/// whole; the reader still severs on an oversize frame, a peer protocol
/// violation.
fn fuse(len: usize) -> io::Result<u32> {
    match u32::try_from(len) {
        Ok(len) if len <= MAX_FRAME_LEN => Ok(len),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame: a frame of {len} bytes exceeds the maximum frame length of \
                 {MAX_FRAME_LEN} bytes"
            ),
        )),
    }
}

/// A body the fuse has passed, and its length prefix.
pub(crate) struct Encoded {
    len: u32,
    body: Vec<u8>,
}

/// Serialise and fuse, with no I/O: an `Err` is the writer's fault and leaves
/// the wire untouched. Encode and decode both grow their stack onto the heap
/// rather than capping depth: a frame nests as deep as the data or IR it
/// carries, and only the fuse bounds it.
pub(crate) fn encode<T: Serialize>(value: &T) -> io::Result<Encoded> {
    let mut body = Vec::new();
    value
        .serialize(serde_stacker::Serializer::new(
            &mut serde_json::Serializer::new(&mut body),
        ))
        .map_err(io::Error::other)?;
    Ok(Encoded {
        len: fuse(body.len())?,
        body,
    })
}

/// Length prefix, body, flush: only I/O can fail here.
pub(crate) fn put<W: Write + ?Sized>(w: &mut W, frame: &Encoded) -> io::Result<()> {
    w.write_all(&frame.len.to_le_bytes())?;
    w.write_all(&frame.body)?;
    w.flush()
}

pub(crate) fn read_frame<R: Read + ?Sized, T: DeserializeOwned>(
    r: &mut R,
) -> io::Result<Option<T>> {
    read_body(r)?.map(|body| decode_body(&body)).transpose()
}

/// `Ok(None)` is a clean EOF at a frame boundary, not a truncated frame.
fn read_body<R: Read + ?Sized>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len_buf[got..]) {
            // A signal cut the read before any byte moved: retry rather than
            // abandon the channel.  Rare — `libc::signal` restarts syscalls.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "frame: partial frame length",
                ));
            }
            Ok(n) => got += n,
        }
    }
    let len = fuse(u32::from_le_bytes(len_buf) as usize)?;
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// A failed decode dumps the raw body and names the dump in the error.
fn decode_body<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    let mut de = serde_json::Deserializer::from_slice(body);
    // Unbounded depth is safe: `serde_stacker` grows the stack onto the heap.
    de.disable_recursion_limit();
    match T::deserialize(serde_stacker::Deserializer::new(&mut de))
        .and_then(|value| de.end().map(|()| value))
    {
        Ok(value) => Ok(value),
        Err(e) => {
            let path = std::env::temp_dir().join(format!(
                "ral-frame-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            ));
            let _ = dump_frame(&path, body);
            Err(io::Error::other(format!(
                "{e} (raw frame written to {})",
                path.display()
            )))
        }
    }
}

/// Owner-only, not the umask default: a frame body is unredacted payload.
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:frame-dump] frame codec: writes a post-mortem frame dump for debugging a frame IPC channel; an IPC diagnostic artifact, not turn-time model data I/O, raises no surface card."
)]
fn dump_frame(path: &std::path::Path, body: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body)
}

#[cfg(not(unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:frame-dump-nonunix] frame codec: writes a post-mortem frame dump for debugging a frame IPC channel; an IPC diagnostic artifact, not turn-time model data I/O, raises no surface card."
)]
fn dump_frame(path: &std::path::Path, body: &[u8]) -> io::Result<()> {
    std::fs::write(path, body)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    struct InterruptOnce {
        bytes: std::io::Cursor<Vec<u8>>,
        interrupted: bool,
    }

    impl Read for InterruptOnce {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.bytes.read(buf)
        }
    }

    #[test]
    fn read_frame_retries_a_signal_interrupted_length_read() {
        let mut framed = Vec::new();
        put(&mut framed, &encode(&"hello").unwrap()).unwrap();
        let mut reader = InterruptOnce {
            bytes: std::io::Cursor::new(framed),
            interrupted: false,
        };
        let value: Option<String> = read_frame(&mut reader).unwrap();
        assert_eq!(value.as_deref(), Some("hello"));
    }

    #[test]
    fn an_oversized_length_is_refused_before_the_body_is_allocated() {
        let mut reader = std::io::Cursor::new(vec![0xFF; 4]);
        let err = read_frame::<_, String>(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeds the maximum frame length"),
            "unexpected diagnostic: {err}"
        );
    }

    /// Both doors judge a body by the one fuse, so an oversized frame fails
    /// at the writer rather than killing the peer that reads it.
    #[test]
    fn the_fuse_is_the_same_on_both_doors() {
        assert_eq!(fuse(MAX_FRAME_LEN as usize).unwrap(), MAX_FRAME_LEN);
        let err = fuse(MAX_FRAME_LEN as usize + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeds the maximum frame length"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn a_truncated_length_is_not_a_clean_hangup() {
        let mut empty = std::io::Cursor::new(Vec::new());
        let value: Option<String> = read_frame(&mut empty).unwrap();
        assert_eq!(value, None, "EOF at a frame boundary is an orderly close");

        let mut partial = std::io::Cursor::new(vec![0x02, 0x00]);
        let err = read_frame::<_, String>(&mut partial).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(err.to_string(), "frame: partial frame length");
    }

    #[test]
    fn a_deep_nest_round_trips_on_a_default_stack() {
        const DEPTH: usize = 5000;
        let nest = format!("{}{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let mut framed = fuse(nest.len()).unwrap().to_le_bytes().to_vec();
        framed.extend_from_slice(nest.as_bytes());

        let deep: serde_json::Value = read_frame(&mut std::io::Cursor::new(framed.clone()))
            .unwrap()
            .expect("a whole frame decodes");
        let mut again = Vec::new();
        put(&mut again, &encode(&deep).unwrap()).unwrap();
        assert_eq!(again, framed);
    }

    #[test]
    fn a_deep_result_crosses_in_a_report_frame() {
        // Building, comparing and dropping a 5000-deep value recurses past a test thread's stack.
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(deep_result_crosses)
            .unwrap()
            .join()
            .unwrap();
    }

    fn deep_result_crosses() {
        use crate::first_order::FOValue;
        use crate::protocol::{DispatchId, Ending, Event, Frame, Report};

        let lists = (0..5000).fold(FOValue::Unit, |inner, _| FOValue::List {
            items: vec![inner],
        });
        let maps = (0..5000).fold(FOValue::Unit, |inner, _| FOValue::Map {
            entries: vec![("k".into(), inner)],
        });
        for value in [lists, maps] {
            let frame = Frame::Event(
                DispatchId(1),
                Event::Report(Report::Ran {
                    ending: Ending::Settled { value, status: 0 },
                    captured: None,
                    trail: Vec::new(),
                }),
            );
            let mut framed = Vec::new();
            put(&mut framed, &encode(&frame).unwrap()).unwrap();
            let decoded: Option<Frame> = read_frame(&mut std::io::Cursor::new(framed)).unwrap();
            assert!(decoded == Some(frame), "a deep result must cross intact");
        }
    }

    #[cfg(unix)]
    #[test]
    fn decode_failure_dump_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let mut framed = Vec::new();
        put(&mut framed, &encode(&"not a number").unwrap()).unwrap();
        let mut reader = std::io::Cursor::new(framed);
        let err = read_frame::<_, u32>(&mut reader).unwrap_err();

        let message = err.to_string();
        let path = message
            .rsplit_once("written to ")
            .and_then(|(_, tail)| tail.strip_suffix(')'))
            .expect("error names the dump path");
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "dump must not be group/world-readable");
        let _ = std::fs::remove_file(path);
    }
}
