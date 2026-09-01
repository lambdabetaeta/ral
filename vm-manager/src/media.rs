//! Inflating the shipped rootfs archive — one procedure, not one per backend.
//!
//! The archive is the same zstd stream wherever it lands: `vm-image/build.sh`
//! writes it once, and every backend that ships it compressed decompresses it
//! the same way. What differs between installs is whether the result is
//! trusted against a shipped checksum, which is why that check is
//! [`inflate`]'s own `verify` parameter rather than a second copy of the loop
//! that only one caller runs.
#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: host-side disk plumbing before any engine exists; no shell, no \
              run, no card. See hcs::vhd's module docs for the fuller argument this shares."
)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Decompress the zstd `archive` into `out`, verifying the result against
/// `verify` — the expected hex SHA-256 of the inflated bytes — when given,
/// and returning how many bytes came out.
///
/// Writes to a `.part` sibling of `out` and renames it into place only on
/// success, so a failure partway through — a full disk, a truncated archive,
/// a checksum mismatch — never leaves a half-written image where `out` says a
/// whole one should be.
///
/// # Errors
/// Returns a sentence naming `archive` or `out` if the archive cannot be
/// opened, is not a zstd stream readable within [`crate::ROOTFS_WINDOW`], if
/// the `.part` file cannot be written or renamed into place, or if `verify`
/// is given and does not match the inflated bytes.
pub fn inflate(archive: &Path, out: &Path, verify: Option<&str>) -> Result<u64, String> {
    let source = std::fs::File::open(archive).map_err(|e| {
        format!(
            "the guest image {} could not be opened: {e}",
            archive.display()
        )
    })?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
        std::io::BufReader::new(source),
        crate::ROOTFS_WINDOW,
    )
    .map_err(|e| format!("the guest image {} could not be read: {e}", archive.display()))?;

    let mut part = out.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);

    let mut land = || -> Result<u64, String> {
        let mut sink = std::io::BufWriter::new(std::fs::File::create(&part).map_err(|e| {
            format!(
                "the guest image could not be written to {}: {e}",
                part.display()
            )
        })?);
        let mut hasher = verify.map(|_| Sha256::new());
        let mut bytes = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = decoder.read(&mut buf).map_err(|e| {
                format!(
                    "the guest image {} could not be unpacked: {e}",
                    archive.display()
                )
            })?;
            if n == 0 {
                break;
            }
            if let Some(hasher) = &mut hasher {
                hasher.update(&buf[..n]);
            }
            sink.write_all(&buf[..n]).map_err(|e| {
                format!(
                    "the guest image could not be written to {}: {e}",
                    part.display()
                )
            })?;
            bytes += u64::try_from(n).unwrap_or(u64::MAX);
        }
        sink.flush().map_err(|e| {
            format!(
                "the guest image could not be written to {}: {e}",
                part.display()
            )
        })?;
        drop(sink);

        if let (Some(expected), Some(hasher)) = (verify, hasher) {
            let actual = hex(&hasher.finalize());
            if actual != expected {
                return Err(format!(
                    "the guest image unpacked from {} did not match its checksum — the download \
                     is corrupt",
                    archive.display()
                ));
            }
        }

        std::fs::rename(&part, out).map_err(|e| {
            format!(
                "the unpacked guest image could not be moved into place at {}: {e}",
                out.display()
            )
        })?;
        Ok(bytes)
    };
    // The part file dies with whatever failure left it there: an inflate cut
    // short by a full disk would otherwise squat gigabytes on the disk that
    // just ran out.
    land().inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })
}

/// The lowercase hex encoding of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
