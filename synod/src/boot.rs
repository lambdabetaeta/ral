//! The boot media this build of synod ships, and the one-time work of
//! readying it.
//!
//! The kernel and initramfs ship uncompressed and boot in place.  The
//! rootfs — a two-gigabyte read-only Ubuntu userland — ships as a zstd
//! archive so the delivered `.app` stays small, and is inflated once, on
//! the first launch that needs it, into a writable cache beside synod's
//! other state.  A signed bundle is read-only, so the rootfs can never
//! live inside it; the cache is the only writable home it has.
//!
//! # On the filesystem
//!
//! The media are the program's own installed files, looked for and read by
//! name ([`own::kind_named`], [`own::read_named`]): nothing a grant judges.
//! What readying the rootfs keeps is synod's own state, held as an
//! [`OwnDir`]: the cache folder, and the marker that vouches for the inflated
//! image in it.  The image itself reaches `vm_manager` by its path
//! ([`OwnDir::path`]), which resolves it again.

use std::path::{Path, PathBuf};

use ral_core::own::{self, OwnDir};
use ral_core::path::{Kind, Visibility, XdgKind, spelled_parent};
use vm_manager::{Act, Error, HostFile};

use crate::session::SYNOD;
use crate::site::{Silent, site};

/// The cache's folder under synod's own cache directory.
const CACHE: &str = "boot";

/// The inflated rootfs, in the cache.
const IMAGE: &str = "rootfs.img";

/// The marker beside the image, named for it: the checksum the image was
/// verified against.
const MARKER: &str = "rootfs.img.verified";

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
    /// A zstd archive to inflate into the cache, verified against `checksum`
    /// (a `sha256sum` sidecar holding the expected hash of the inflated
    /// image).
    Compressed { archive: PathBuf, checksum: PathBuf },
}

/// The boot media this build ships, if this computer holds it.
///
/// Three layouts are looked in, mirroring how the window finds its own
/// bundle.  A shipped **macOS** bundle keeps the media under
/// `Contents/Resources/boot/`, beside the binary's own `Contents/MacOS/`; a
/// shipped **Windows** installation keeps it in `boot/` beside the executable
/// itself, because that is where Tauri's resource map puts the same four
/// destinations there.  Either way the rootfs is compressed and inflated into
/// the cache on demand.  A development build reaches the image pipeline's own
/// output under the workspace `target` directory, with the rootfs already
/// inflated.
///
/// The two bundle layouts are tried on every platform rather than `#[cfg]`-ed
/// apart: each is a *place a file might be*, and one that does not exist on
/// this computer answers the question by not being there.  `None` — no media
/// anywhere — is not an error this function raises: [`vm_manager::detect`]
/// turns it into the refusal a synod with nothing to boot must give.
pub(crate) fn boot_media() -> Option<BootPlan> {
    let exe = std::env::current_exe().ok()?;
    let dir = spelled_parent(&exe)?;

    for boot in [
        spelled_parent(dir).map(|contents| contents.join("Resources").join("boot")),
        Some(dir.join("boot")),
    ]
    .into_iter()
    .flatten()
    {
        let kernel = boot.join("kernel");
        let initramfs = boot.join("initramfs.img");
        if !is_file(&kernel) || !is_file(&initramfs) {
            continue;
        }
        // Either spelling of the rootfs is a shipped bundle: both packages carry
        // the archive, and a checkout has the plain image the pipeline built.
        //
        // Both packages, because neither has a choice.  A signed `.app` is
        // read-only and downloaded whole, so it cannot hold a file it means to
        // write; and a Windows Installer cabinet cannot hold a two-gigabyte file
        // at all — `light` refuses to link one, which is how this was found out.
        // What differs is who inflates it.  Here it is inflated into a per-user
        // cache, because that is the only writable place a `.app` has.  On
        // Windows an installed synod never reaches this code: it boots through
        // the machine service, which inflates the same archive into its own
        // machine-wide cache while wrapping it as a VHD, so one copy serves every
        // session on the computer.  This path is then what a *checkout* on
        // Windows uses, where the plain image is already there and the branch
        // below takes it.
        let archive = boot.join("rootfs.img.zst");
        let plain = boot.join("rootfs.img");
        if is_file(&archive) {
            return Some(BootPlan {
                kernel,
                initramfs,
                rootfs: Rootfs::Compressed {
                    archive,
                    checksum: boot.join("rootfs.img.sha256"),
                },
            });
        }
        if is_file(&plain) {
            return Some(BootPlan {
                kernel,
                initramfs,
                rootfs: Rootfs::Ready(plain),
            });
        }
    }

    // The development layout: the image pipeline's output, rootfs plain.
    let mut cursor = dir;
    while let Some(parent) = spelled_parent(cursor) {
        if parent.file_name().is_some_and(|name| name == "target") {
            let out = spelled_parent(parent)?.join("vm-image").join("out");
            let kernel = out.join("boot").join("kernel");
            let initramfs = out.join("boot").join("initramfs.img");
            let rootfs = out.join("rootfs").join("rootfs.img");
            return [&kernel, &initramfs, &rootfs]
                .iter()
                .all(|f| is_file(f))
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

/// Whether `path` names a file, as the boot media do, through any link.
fn is_file(path: &Path) -> bool {
    site!(Silent::InstallTree, own::kind_named(path)) == Some(Kind::File)
}

impl BootPlan {
    /// Ready the media for booting, inflating the rootfs if it ships
    /// compressed, and hand back the artifact [`vm_manager::detect`] wants.
    ///
    /// # Errors
    /// Returns [`Error::HostFile`] if the rootfs's checksum cannot be read, it
    /// cannot be inflated, or its marker cannot be written,
    /// [`Error::HostFileUnplaced`] if its cache cannot be made, and
    /// [`Error::CorruptGuestImage`] if the inflated image does not match its
    /// checksum.
    pub(crate) fn realise(self) -> Result<vm_manager::BootArtifact, Error> {
        let rootfs = match self.rootfs {
            Rootfs::Ready(path) => path,
            Rootfs::Compressed { archive, checksum } => ensure_inflated(&archive, &checksum)?,
        };
        Ok(vm_manager::BootArtifact {
            kernel: self.kernel,
            initramfs: self.initramfs,
            rootfs,
        })
    }
}

/// Synod's boot cache, `$XDG_CACHE_HOME/synod/boot/`, made where absent.
///
/// # Errors
/// [`Error::HostFileUnplaced`] if the cache cannot be made: the cause is the
/// [`OwnError`](ral_core::own::OwnError) itself, which spells the paths it has.
fn cache_boot_dir() -> Result<OwnDir, Error> {
    SYNOD
        .own(XdgKind::Cache)
        .and_then(|cache| cache.make_dir(CACHE))
        .map_err(|cause| Error::HostFileUnplaced {
            file: HostFile::ImageCache,
            act: Act::Make,
            cause: cause.to_string(),
        })
}

/// The rootfs in synod's boot cache, inflating `archive` into it if it is not
/// already present and matching `checksum`.
///
/// A prior launch's inflated image is trusted without re-hashing its two
/// gigabytes: a marker file beside it records the checksum it was verified
/// against, and a marker that still matches means the image did too.  The
/// inflate itself always verifies, and writes to a temporary file renamed
/// into place only on success, so an interrupted launch never leaves a
/// half-written image that a later one would trust.
fn ensure_inflated(archive: &Path, checksum: &Path) -> Result<PathBuf, Error> {
    let expected = read_checksum(checksum)?;
    let boot = cache_boot_dir()?;
    let target = boot.path().join(IMAGE);

    let present = boot
        .stat(IMAGE)
        .is_some_and(|image| image.kind == Kind::File);
    // A marker that cannot be read, or is not a checksum, reads as none: the
    // image is inflated again.
    let seen = boot.load::<String>(MARKER).ok().flatten();
    if present && seen.as_deref() == Some(expected.as_str()) {
        return Ok(target);
    }

    vm_manager::media::inflate(archive, &target, Some(&expected))?;

    boot.store(MARKER, &expected, Visibility::Public)
        .map_err(Error::host_file(
            HostFile::UnpackMarker,
            Act::Write,
            &boot.path().join(MARKER),
        ))?;
    Ok(target)
}

/// The hash from a `sha256sum` sidecar: the first whitespace-delimited token
/// of its one line.
fn read_checksum(path: &Path) -> Result<String, Error> {
    let line = site!(Silent::InstallTree, own::read_named(path)).map_err(Error::host_file(
        HostFile::ImageChecksum,
        Act::Read,
        path,
    ))?;
    line.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| Error::host_file(HostFile::ImageChecksum, Act::Read, path)("it is empty"))
}
