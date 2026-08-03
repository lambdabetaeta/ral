//! Length-prefixed JSON frames: the one framing codec on every ral IPC
//! channel — the pipeline gate / report protocol, the helper stages it
//! re-execs, and the engine wire in `wire`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, Read, Write};

/// Checked before the body is allocated, so no peer names a huge buffer.
const MAX_FRAME_LEN: u32 = 256 * 1024 * 1024;

pub fn write_frame<W: Write + ?Sized, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::other("subprocess: frame exceeds 4 GiB"))?;
    let _no_sigpipe = sigpipe::Suppress::install();
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Block SIGPIPE around a protocol write.
///
/// Batch mode leaves SIGPIPE at `SIG_DFL` so the bundled coreutils die on a
/// closed downstream (`yes | head`) — but it would equally kill the parent
/// mid-write to a helper that has already exited, instead of yielding the
/// `EPIPE` the error path is written to observe.  SIGPIPE is thread-directed,
/// so masking it on the writing thread suffices, and unlike `SO_NOSIGPIPE`
/// (BSD-only) or `MSG_NOSIGNAL` (wants `send(2)`) it rides `Write` unchanged.
#[cfg(unix)]
mod sigpipe {
    pub(super) struct Suppress {
        was_blocked: bool,
    }

    impl Suppress {
        /// Remembers a pre-existing block, so `Drop` unblocks only ours.
        pub(super) fn install() -> Self {
            use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
            let mut set = SigSet::empty();
            set.add(Signal::SIGPIPE);
            let mut old = SigSet::empty();
            let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), Some(&mut old));
            Self {
                was_blocked: old.contains(Signal::SIGPIPE),
            }
        }
    }

    impl Drop for Suppress {
        fn drop(&mut self) {
            // Blocked before us: leave the mask alone, and do not consume a
            // signal the surrounding code may be managing itself.
            if self.was_blocked {
                return;
            }
            use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
            let mut set = SigSet::empty();
            set.add(Signal::SIGPIPE);
            // `nix` wraps no `sigpending(2)`, so this one call drops to libc.
            // Safety: `sigpending` fills `raw` before `assume_init` reads it.
            let pending = unsafe {
                let mut raw = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
                (libc::sigpending(raw.as_mut_ptr()) == 0)
                    .then(|| SigSet::from_sigset_t_unchecked(raw.assume_init()))
            };
            // A dead-peer write left SIGPIPE pending; consume it before
            // unblocking, or restoring the mask delivers it under batch
            // mode's `SIG_DFL`.  `wait` cannot block: it is reached only
            // with SIGPIPE already pending.
            if pending.is_some_and(|p| p.contains(Signal::SIGPIPE)) {
                let _ = set.wait();
            }
            let _ = pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&set), None);
        }
    }
}

#[cfg(not(unix))]
mod sigpipe {
    pub(super) struct Suppress;
    impl Suppress {
        pub(super) fn install() -> Self {
            Self
        }
    }
}

pub fn read_frame<R: Read + ?Sized, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let Some(body) = read_body(r)? else {
        return Ok(None);
    };
    decode_body(&body).map(Some)
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
                    "subprocess: partial frame length",
                ));
            }
            Ok(n) => got += n,
        }
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("subprocess: frame length {len} exceeds max {MAX_FRAME_LEN}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// A failed decode dumps the raw body and names the dump in the error.
fn decode_body<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    match serde_json::from_slice(body) {
        Ok(value) => Ok(value),
        Err(e) => {
            let path = std::env::temp_dir().join(format!(
                "ral-subprocess-frame-{}-{}.json",
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
    reason = "[io-door:silent:frame-dump] subprocess codec (helper IPC): writes a post-mortem frame dump for debugging the helper protocol; an IPC diagnostic artifact, not turn-time model data I/O, raises no surface card."
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
    reason = "[io-door:silent:frame-dump-nonunix] subprocess codec (helper IPC): writes a post-mortem frame dump for debugging the helper protocol; an IPC diagnostic artifact, not turn-time model data I/O, raises no surface card."
)]
fn dump_frame(path: &std::path::Path, body: &[u8]) -> io::Result<()> {
    std::fs::write(path, body)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
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
        write_frame(&mut framed, &"hello").unwrap();
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
            err.to_string().contains("exceeds max"),
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
        assert_eq!(err.to_string(), "subprocess: partial frame length");
    }

    #[cfg(unix)]
    #[test]
    fn decode_failure_dump_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let mut framed = Vec::new();
        write_frame(&mut framed, &"not a number").unwrap();
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
