//! Length-prefixed JSON frames for ral subprocess helpers.
//!
//! Used by both the grant sandbox IPC path and the pipeline-stage helper
//! path so every re-exec protocol shares one framing codec.

use serde::de::{DeserializeOwned, DeserializeSeed};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::marker::PhantomData;

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

/// Suppress SIGPIPE for the duration of a protocol write.
///
/// Batch mode runs with `SIGPIPE = SIG_DFL` so the bundled coreutils see
/// `EPIPE` and exit cleanly when their downstream closes (`yes | head`).
/// That same disposition would otherwise turn a parent-side protocol
/// write to a helper that has already exited — the gate / report / IPC
/// channels — into a fatal signal (status 141) before the `EPIPE` the
/// error path is written to observe can materialise.
///
/// The fix is per-write, not process-wide: SIGPIPE is synchronous and
/// thread-directed — it is delivered to the very thread whose `write`
/// found a dead peer — so blocking it on this thread for the extent of
/// the write makes the syscall return `EPIPE` instead.  This is the one
/// mechanism portable across Linux/macOS/BSD: `SO_NOSIGPIPE` is
/// BSD-only and `MSG_NOSIGNAL` needs `send(2)` rather than the
/// `write(2)` that `std::io::Write` performs, whereas masking rides the
/// `Write` trait unchanged.  On Windows there is no SIGPIPE; the guard
/// is a no-op and a dead-peer write returns an `io::Error` directly.
#[cfg(unix)]
mod sigpipe {
    pub(super) struct Suppress {
        was_blocked: bool,
    }

    impl Suppress {
        /// Block SIGPIPE on the current thread, remembering whether it
        /// was already blocked so [`Drop`] only unblocks what we blocked.
        pub(super) fn install() -> Self {
            // Safety: `sigemptyset` / `sigaddset` initialise the set
            // before use, and `pthread_sigmask` reads/writes only the
            // local `set` / `old` masks; no signal handler is run here.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGPIPE);
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
                let was_blocked = libc::sigismember(&old, libc::SIGPIPE) == 1;
                Self { was_blocked }
            }
        }
    }

    impl Drop for Suppress {
        fn drop(&mut self) {
            // SIGPIPE was already blocked before us — leave the mask
            // exactly as we found it, and do not consume a signal the
            // surrounding code may be managing itself.
            if self.was_blocked {
                return;
            }
            // Safety: every libc call below reads/writes only locally
            // initialised `sigset_t`s; `sigwait` is invoked only when
            // `sigpending` reports SIGPIPE pending, so it returns
            // immediately rather than blocking.  A dead-peer write while
            // SIGPIPE was blocked leaves it pending; consume it before
            // unblocking so restoring the mask cannot deliver a deferred
            // signal and kill the process under the batch-mode SIG_DFL
            // disposition.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGPIPE);
                let mut pending: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut pending);
                if libc::sigpending(&mut pending) == 0
                    && libc::sigismember(&pending, libc::SIGPIPE) == 1
                {
                    let mut sig: libc::c_int = 0;
                    libc::sigwait(&set, &mut sig);
                }
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            }
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
    decode_body(&body, |b| serde_json::from_slice(b)).map(Some)
}

/// Read one frame and deserialise its body through `seed` rather than a
/// `DeserializeOwned` impl, so the decode itself can carry a constraint
/// the caller supplies (e.g. an expected token).  Shares `read_frame`'s
/// length-prefix read and raw-frame-dump-on-error tail; only the
/// deserialiser differs.
pub fn read_frame_seeded<R: Read + ?Sized, V, S>(r: &mut R, seed: S) -> io::Result<Option<V>>
where
    S: for<'de> DeserializeSeed<'de, Value = V>,
{
    let Some(body) = read_body(r)? else {
        return Ok(None);
    };
    decode_body(&body, |b| {
        seed.deserialize(&mut serde_json::Deserializer::from_slice(b))
    })
    .map(Some)
}

/// Read one length-prefixed frame body off `r`, retrying a signal-cut
/// length read.  `Ok(None)` is a clean EOF at a frame boundary.
fn read_body<R: Read + ?Sized>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len_buf[got..]) {
            // EINTR is not a frame fault: the read was interrupted by a
            // signal before any byte moved, so retry rather than abandon
            // the channel (latent under `libc::signal` BSD restart
            // semantics).
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
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
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Run `decode` over a frame body, turning a deserialise failure into an
/// `io::Error` after dumping the raw bytes for post-mortem.
fn decode_body<T>(
    body: &[u8],
    decode: impl FnOnce(&[u8]) -> serde_json::Result<T>,
) -> io::Result<T> {
    match decode(body) {
        Ok(value) => Ok(value),
        Err(e) => {
            let path = std::env::temp_dir().join(format!(
                "ral-subprocess-frame-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            ));
            let _ = dump_frame(&path, body);
            Err(io::Error::other(format!(
                "{e} (raw frame written to {})",
                path.display()
            )))
        }
    }
}

/// Write a post-mortem frame dump to `path`.  A frame body may carry the
/// per-re-exec sandbox token (see [`Tokened`]) whose secrecy guards the
/// IPC boundary, so on Unix the file is created with owner-only
/// permissions rather than the process umask's default.
#[cfg(unix)]
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
fn dump_frame(path: &std::path::Path, body: &[u8]) -> io::Result<()> {
    std::fs::write(path, body)
}

/// A frame body carrying a capability token alongside its payload.  The
/// child stamps the per-re-exec sandbox token; the parent reads it back
/// through [`ExpectToken`] so a forged response frame fails to decode.
#[derive(Serialize, Deserialize)]
pub struct Tokened<T> {
    pub token: String,
    pub inner: T,
}

/// A [`DeserializeSeed`] that deserialises a [`Tokened<T>`], checks its
/// embedded token against an expected value in constant time, and yields
/// the proven `inner`.  A missing token is serde's "missing field"
/// error; a wrong token is a custom error.  Both route through
/// [`read_frame_seeded`]'s decode-failure path, so neither produces a
/// usable `T` at the call site.
pub struct ExpectToken<'a, T> {
    expected: &'a str,
    _payload: PhantomData<T>,
}

impl<'a, T> ExpectToken<'a, T> {
    pub fn new(expected: &'a str) -> Self {
        Self {
            expected,
            _payload: PhantomData,
        }
    }
}

impl<'de, T: Deserialize<'de>> DeserializeSeed<'de> for ExpectToken<'_, T> {
    type Value = T;

    fn deserialize<D>(self, deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let framed = Tokened::<T>::deserialize(deserializer)?;
        if !crate::sandbox::constant_time_eq(framed.token.as_bytes(), self.expected.as_bytes()) {
            return Err(serde::de::Error::custom("sandbox response: token mismatch"));
        }
        Ok(framed.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that returns `Interrupted` on the first read of each frame
    /// before delivering the buffered bytes — modelling the BSD `signal`
    /// restart semantics under which a length read can be cut short.
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
    fn seeded_read_yields_inner_when_token_matches() {
        let mut framed = Vec::new();
        write_frame(
            &mut framed,
            &Tokened {
                token: "secret".to_string(),
                inner: 42u32,
            },
        )
        .unwrap();
        let mut reader = std::io::Cursor::new(framed);
        let value = read_frame_seeded(&mut reader, ExpectToken::<u32>::new("secret")).unwrap();
        assert_eq!(value, Some(42));
    }

    #[test]
    fn seeded_read_rejects_a_wrong_token() {
        let mut framed = Vec::new();
        write_frame(
            &mut framed,
            &Tokened {
                token: "forged".to_string(),
                inner: 42u32,
            },
        )
        .unwrap();
        let mut reader = std::io::Cursor::new(framed);
        let err = read_frame_seeded(&mut reader, ExpectToken::<u32>::new("secret")).unwrap_err();
        assert!(err.to_string().contains("token mismatch"));
    }

    #[test]
    fn seeded_read_rejects_a_missing_token() {
        // A frame carrying only the payload, with no `token` field.
        let mut framed = Vec::new();
        write_frame(&mut framed, &serde_json::json!({ "inner": 42 })).unwrap();
        let mut reader = std::io::Cursor::new(framed);
        let err = read_frame_seeded(&mut reader, ExpectToken::<u32>::new("secret")).unwrap_err();
        assert!(err.to_string().contains("missing field `token`"));
    }

    #[cfg(unix)]
    #[test]
    fn decode_failure_dump_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        // A token-bearing frame whose payload fails to deserialise as the
        // expected type dumps the raw bytes, including the token.
        let mut framed = Vec::new();
        write_frame(
            &mut framed,
            &Tokened {
                token: "secret".to_string(),
                inner: "not a number",
            },
        )
        .unwrap();
        let mut reader = std::io::Cursor::new(framed);
        let err = read_frame_seeded(&mut reader, ExpectToken::<u32>::new("secret")).unwrap_err();

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
