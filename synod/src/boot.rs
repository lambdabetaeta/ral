//! The boot media this build of synod ships, and the one-time work of
//! readying it (`dev/docs/VM/SYNOD.md` §7).
//!
//! The kernel and initramfs ship uncompressed and boot in place.  The
//! rootfs — a two-gigabyte read-only Ubuntu userland — ships as a zstd
//! archive so the delivered `.app` stays small, and is inflated once, on
//! the first launch that needs it, into a writable cache beside synod's
//! other state.  A signed bundle is read-only, so the rootfs can never
//! live inside it; the cache is the only writable home it has.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::session::SYNOD;

/// Boot media located on this computer, with the rootfs in one of two
/// states: already a plain image (a development build, or a bundle whose
/// rootfs an earlier launch inflated), or still the archive a fresh install
/// ships, to be inflated once.
pub(crate) struct BootPlan {
    kernel: PathBuf,
    initramfs: PathBuf,
    rootfs: Rootfs,
}

enum Rootfs {
    /// A plain image, ready to attach.
    Ready(PathBuf),
    /// A zstd archive to inflate to `target`, verified against `checksum`
    /// (a `sha256sum` sidecar holding the expected hash of the inflated
    /// image).
    Compressed {
        archive: PathBuf,
        checksum: PathBuf,
        target: PathBuf,
    },
}

/// The boot media this build ships, if this computer holds it.
///
/// Two layouts are looked in, mirroring how the window finds its own
/// bundle: the shipped bundle keeps the media under
/// `Contents/Resources/boot/`, beside the binary's own `Contents/MacOS/`,
/// with the rootfs compressed; a development build reaches the image
/// pipeline's own output under the workspace `target` directory, with the
/// rootfs already inflated.  `None` — no media anywhere — is not an error
/// this function raises: [`vm_manager::detect`] turns it into the refusal a
/// synod with nothing to boot must give.
pub(crate) fn boot_media() -> Option<BootPlan> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    // The shipped bundle: kernel and initramfs in place, rootfs compressed,
    // inflated into the cache on demand.
    if let Some(contents) = dir.parent() {
        let boot = contents.join("Resources").join("boot");
        let kernel = boot.join("kernel");
        let initramfs = boot.join("initramfs.img");
        let archive = boot.join("rootfs.img.zst");
        if [&kernel, &initramfs, &archive].iter().all(|f| f.is_file()) {
            return Some(BootPlan {
                kernel,
                initramfs,
                rootfs: Rootfs::Compressed {
                    archive,
                    checksum: boot.join("rootfs.img.sha256"),
                    target: cache_boot_dir().join("rootfs.img"),
                },
            });
        }
    }

    // The development layout: the image pipeline's output, rootfs plain.
    let mut cursor = dir;
    while let Some(parent) = cursor.parent() {
        if parent.file_name().is_some_and(|name| name == "target") {
            let out = parent.parent()?.join("vm-image").join("out");
            let kernel = out.join("boot").join("kernel");
            let initramfs = out.join("boot").join("initramfs.img");
            let rootfs = out.join("rootfs.img");
            return [&kernel, &initramfs, &rootfs]
                .iter()
                .all(|f| f.is_file())
                .then_some(BootPlan {
                    kernel,
                    initramfs,
                    rootfs: Rootfs::Ready(rootfs),
                });
        }
        cursor = parent;
    }
    None
}

impl BootPlan {
    /// Ready the media for booting, inflating the rootfs if it ships
    /// compressed, and hand back the artifact [`vm_manager::detect`] wants.
    ///
    /// # Errors
    /// Returns a plain sentence if the rootfs cannot be inflated, its cache
    /// cannot be made, or the inflated image does not match its checksum.
    pub(crate) fn realise(self) -> Result<vm_manager::BootArtifact, String> {
        let rootfs = match self.rootfs {
            Rootfs::Ready(path) => path,
            Rootfs::Compressed {
                archive,
                checksum,
                target,
            } => ensure_inflated(&archive, &checksum, &target)?,
        };
        Ok(vm_manager::BootArtifact {
            kernel: self.kernel,
            initramfs: self.initramfs,
            rootfs,
        })
    }
}

fn cache_boot_dir() -> PathBuf {
    SYNOD
        .xdg_dir(ral_core::path::basedir::XdgKind::Cache)
        .join("boot")
}

/// The rootfs at `target`, inflating `archive` into it if it is not already
/// present and matching `checksum`.
///
/// A prior launch's inflated image is trusted without re-hashing its two
/// gigabytes: a marker file beside it records the checksum it was verified
/// against, and a marker that still matches means the image did too.  The
/// inflate itself always verifies, and writes to a temporary file renamed
/// into place only on success, so an interrupted launch never leaves a
/// half-written image that a later one would trust.
fn ensure_inflated(archive: &Path, checksum: &Path, target: &Path) -> Result<PathBuf, String> {
    let expected = read_checksum(checksum)?;
    let marker = target.with_extension("img.verified");

    if target.is_file()
        && std::fs::read_to_string(&marker).is_ok_and(|seen| seen.trim() == expected)
    {
        return Ok(target.to_path_buf());
    }

    let boot = target
        .parent()
        .expect("the rootfs cache path always has a boot-directory parent");
    std::fs::create_dir_all(boot).map_err(|e| {
        format!(
            "the guest image cache {} could not be made: {e}",
            boot.display()
        )
    })?;

    let tmp = target.with_extension("img.part");
    let actual = inflate(archive, &tmp)?;
    if actual != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "the guest image unpacked from {} did not match its checksum — the download is corrupt",
            archive.display()
        ));
    }

    std::fs::rename(&tmp, target).map_err(|e| {
        format!(
            "the unpacked guest image could not be moved into place at {}: {e}",
            target.display()
        )
    })?;
    std::fs::write(&marker, &expected).map_err(|e| {
        format!(
            "the guest image was unpacked but its checksum marker {} could not be written: {e}",
            marker.display()
        )
    })?;
    Ok(target.to_path_buf())
}

/// Decompress the zstd `archive` into `out`, returning the hex SHA-256 of
/// the inflated bytes computed in the same pass.
fn inflate(archive: &Path, out: &Path) -> Result<String, String> {
    let source = std::fs::File::open(archive).map_err(|e| {
        format!(
            "the guest image {} could not be opened: {e}",
            archive.display()
        )
    })?;
    let mut decoder =
        zstd::stream::read::Decoder::new(std::io::BufReader::new(source)).map_err(|e| {
            format!(
                "the guest image {} could not be read: {e}",
                archive.display()
            )
        })?;
    let mut sink = std::io::BufWriter::new(std::fs::File::create(out).map_err(|e| {
        format!(
            "the guest image could not be written to {}: {e}",
            out.display()
        )
    })?);

    let mut hasher = Sha256::new();
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
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n]).map_err(|e| {
            format!(
                "the guest image could not be written to {}: {e}",
                out.display()
            )
        })?;
    }
    sink.flush().map_err(|e| {
        format!(
            "the guest image could not be written to {}: {e}",
            out.display()
        )
    })?;
    Ok(hex(&hasher.finalize()))
}

/// The hash from a `sha256sum` sidecar: the first whitespace-delimited token
/// of its one line.
fn read_checksum(path: &Path) -> Result<String, String> {
    let line = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "the guest image checksum {} could not be read: {e}",
            path.display()
        )
    })?;
    line.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| format!("the guest image checksum {} is empty", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
