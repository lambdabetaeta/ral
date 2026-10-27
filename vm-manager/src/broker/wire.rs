//! The client supplies only a folder and read-only flag; the service owns media and devices.
//!
//! Socket descriptions target the kernel-reported client PID.
//! The connection leases the machine; Adopted releases the broker's socket copies.

use std::io::{self, Read};

use serde::{Deserialize, Serialize};

use crate::error::{Error, GuestConsole};

/// All client instances share this name; the service's pipe descriptor controls admission.
pub const PIPE: &str = r"\\.\pipe\synod-machine-broker";

/// Checked before boot; `VersionSkew` must remain legible across mismatched versions.
pub const VERSION: u32 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Only the folder and read-only flag are caller-controlled; the service checks the caller's rights.
    Boot {
        version: u32,
        /// The folder in the client's path namespace.
        #[serde(with = "crate::os_path")]
        folder: std::path::PathBuf,
        read_only: bool,
    },
    /// Both sockets are adopted; the broker must close its copies so client EOF reaches the guest.
    Adopted,
    /// The console crosses the pipe because an unprivileged client may not read the service's log.
    Console,
    /// Closing the connection also stops the machine; this request obtains the shutdown result.
    Stop,
}

impl Request {
    /// # Errors
    /// Returns framing or decode errors; the inner refusal names a version skew even if the request changed.
    pub fn read<R: Read>(r: &mut R) -> io::Result<Option<Result<Self, Error>>> {
        let Some(body) = frame::body(r)? else {
            return Ok(None);
        };
        match serde_json::from_slice(&body) {
            Ok(request) => Ok(Some(Ok(request))),
            Err(cause) => {
                #[derive(serde::Deserialize)]
                enum Version {
                    Boot { version: u32 },
                }
                if let Ok(Version::Boot { version }) = serde_json::from_slice(&body)
                    && version != VERSION
                {
                    Ok(Some(Err(Error::VersionSkew {
                        client: version,
                        service: VERSION,
                    })))
                } else {
                    Err(io::Error::other(cause))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    Booted {
        /// Duplicated for this process only; [`super::client::adopt_socket`] adopts it.
        control: Vec<u8>,
        /// Duplicated for the same process as `control`; arrival order does not identify wires.
        net: Vec<u8>,
    },
    Stopped(Result<(), Error>),
    Console(GuestConsole),
    /// The refusal is worded by the client with its own path spelling.
    Refused(Error),
}

/// Framing stays independent of ral-core: the machine layer does not depend on the shell.
pub mod frame {
    use std::io::{self, Read, Write};

    /// Refused before allocation; five paths of 32,767 UTF-16 units take at most
    /// 983,010 bytes of JSON units, leaving room for the rest of a refusal.
    pub const MAX: u32 = 1 << 20;

    /// # Errors
    /// Returns the write or encode error.
    pub fn write<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        let len = u32::try_from(bytes.len())
            .ok()
            .filter(|len| *len <= MAX)
            .ok_or_else(|| io::Error::other("broker: frame too large"))?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(&bytes)?;
        w.flush()
    }

    /// # Errors
    /// Returns the read or decode error; `Ok(None)` means the client and its lease are gone.
    pub fn read<R: Read, T: serde::de::DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
        body(r)?
            .map(|body| serde_json::from_slice(&body).map_err(io::Error::other))
            .transpose()
    }

    pub(super) fn body<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
        let mut prefix = [0u8; 4];
        match r.read_exact(&mut prefix) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(prefix);
        if len > MAX {
            return Err(io::Error::other(format!(
                "broker: frame of {len} bytes exceeds the {MAX}-byte limit"
            )));
        }
        let mut body = vec![0u8; len as usize];
        r.read_exact(&mut body)?;
        Ok(Some(body))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_in_order() {
        let mut buffer = Vec::new();
        frame::write(
            &mut buffer,
            &Request::Boot {
                version: VERSION,
                folder: std::path::PathBuf::from(r"C:\Users\secretary\Invoices"),
                read_only: false,
            },
        )
        .unwrap();
        frame::write(&mut buffer, &Request::Stop).unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        let first: Request = frame::read(&mut cursor).unwrap().unwrap();
        assert!(matches!(first, Request::Boot { version, .. } if version == VERSION));
        assert!(matches!(
            frame::read::<_, Request>(&mut cursor).unwrap(),
            Some(Request::Stop)
        ));
        assert!(
            frame::read::<_, Request>(&mut cursor).unwrap().is_none(),
            "a spent stream reads as gone, not as an error"
        );
    }

    #[test]
    fn an_undecodable_boot_only_yields_skew_for_another_version() {
        for version in [VERSION - 1, VERSION, VERSION + 1] {
            let mut buffer = Vec::new();
            frame::write(
                &mut buffer,
                &serde_json::json!({
                    "Boot": { "version": version, "folder": "old encoding", "read_only": true }
                }),
            )
            .unwrap();
            let result = Request::read(&mut &buffer[..]);
            if version == VERSION {
                assert!(result.is_err(), "a malformed current request is a decode error");
            } else {
                assert!(matches!(
                    result.unwrap(),
                    Some(Err(Error::VersionSkew { client, service: VERSION })) if client == version
                ));
            }
        }
    }

    #[test]
    fn an_oversized_frame_is_refused_before_allocation() {
        let mut stream = std::io::Cursor::new(u32::MAX.to_le_bytes().to_vec());
        let error = frame::read::<_, Request>(&mut stream).expect_err("refused");
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    fn longest_path() -> std::path::PathBuf {
        use std::os::windows::ffi::OsStringExt;

        let mut units = vec![u16::MAX; 32_767];
        units[..3].copy_from_slice(&[u16::from(b'C'), u16::from(b':'), u16::from(b'\\')]);
        units[100] = 0xD800;
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&units))
    }

    #[test]
    fn the_longest_paths_fit_a_frame() {
        let longest = longest_path();
        let mut buffer = Vec::new();
        frame::write(
            &mut buffer,
            &Request::Boot {
                version: VERSION,
                folder: longest.clone(),
                read_only: false,
            },
        )
        .expect("a boot request over the longest path fits a frame");
        let refused = Reply::Refused(Error::NotConstructed {
            hypervisor: crate::hcs::win::HYPERVISOR.into(),
            why: "w".repeat(4096),
            files: Box::new(crate::error::MachineFiles {
                kernel: longest.clone(),
                initramfs: longest.clone(),
                rootfs: longest.clone(),
                session: longest.clone(),
                folder: longest.clone(),
            }),
        });
        frame::write(&mut buffer, &refused)
            .expect("a refusal naming the longest path five times fits a frame");

        let mut cursor = std::io::Cursor::new(buffer);
        assert!(matches!(
            frame::read(&mut cursor).unwrap(),
            Some(Request::Boot { folder, .. }) if folder == longest
        ));
        assert!(matches!(
            frame::read(&mut cursor).unwrap(),
            Some(Reply::Refused(Error::NotConstructed { files, .. }))
                if [&files.kernel, &files.initramfs, &files.rootfs, &files.session, &files.folder]
                    .into_iter().all(|path| path == &longest)
        ));
    }
}
