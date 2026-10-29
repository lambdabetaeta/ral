//! Guest-media discovery and immutable cache publication.
//! Inflation writes only into unpublished files.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Act, Error, HostFile};
use crate::site::{Silent, site};
use crate::spec::BootArtifact;

/// Matches vm-image/build.sh's --long=27; the default decoder limit is too small.
pub const ROOTFS_WINDOW: u64 = 1 << 27;

/// Inflate into an unpublished file, optionally verifying its SHA-256.
pub(crate) fn inflate(archive: &Path, into: &mut File, verify: Option<&str>) -> Result<u64, Error> {
    let source = site!(Silent::RootfsInflate, File::open(archive)).map_err(Error::host_file(
        HostFile::GuestImage,
        Act::Open,
        archive,
    ))?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
        std::io::BufReader::new(source),
        ROOTFS_WINDOW,
    )
    .map_err(Error::host_file(HostFile::GuestImage, Act::Read, archive))?;
    let mut sink = std::io::BufWriter::new(into);
    let mut hasher = verify.map(|_| Sha256::new());
    let mut bytes = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = decoder.read(&mut buf).map_err(Error::host_file(
            HostFile::GuestImage,
            Act::Unpack,
            archive,
        ))?;
        if n == 0 {
            break;
        }
        if let Some(hasher) = &mut hasher {
            hasher.update(&buf[..n]);
        }
        sink.write_all(&buf[..n]).map_err(Error::host_file(
            HostFile::GuestImage,
            Act::Unpack,
            archive,
        ))?;
        bytes += u64::try_from(n).unwrap_or(u64::MAX);
    }
    sink.flush()
        .map_err(Error::host_file(HostFile::GuestImage, Act::Unpack, archive))?;
    if let (Some(expected), Some(hasher)) = (verify, hasher)
        && hex(&hasher.finalize()) != expected
    {
        return Err(Error::CorruptGuestImage {
            archive: archive.to_path_buf(),
        });
    }
    Ok(bytes)
}

/// Publish a complete, synced file once under an identity-derived name.
pub(crate) fn publish(
    cache: &Path,
    leaf: &str,
    prepare: impl FnOnce(&mut File) -> Result<(), Error>,
) -> Result<PathBuf, Error> {
    let failed = |act, path: &Path| Error::host_file(HostFile::ImageCache, act, path);
    site!(Silent::RootfsInflate, std::fs::create_dir_all(cache))
        .map_err(failed(Act::Make, cache))?;
    let path = cache.join(leaf);
    let lock_path = cache.join(format!("{leaf}.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    let lease = site!(Silent::RootfsInflate, &options => open(&lock_path))
        .map_err(failed(Act::Open, &lock_path))?;
    lease.lock().map_err(failed(Act::Open, &lock_path))?;
    if site!(Silent::RootfsInflate, &path => try_exists()).map_err(failed(Act::Read, &path))? {
        return Ok(path);
    }
    let mut stage = tempfile::NamedTempFile::new_in(cache).map_err(failed(Act::Create, cache))?;
    prepare(stage.as_file_mut())?;
    stage
        .as_file()
        .sync_all()
        .map_err(failed(Act::Flush, stage.path()))?;
    stage
        .persist_noclobber(&path)
        .map_err(|cause| failed(Act::Place, &path)(cause.error))?;
    Ok(path)
}

pub enum Rootfs {
    Plain(PathBuf),
    Packed { archive: PathBuf, checksum: PathBuf },
}

/// Boot files found together in one installation or checkout.
pub struct Layout {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub rootfs: Rootfs,
}

/// # Errors
/// Returns [`Error::NoGuestImage`] with every directory tried.
pub fn locate(exe: &Path) -> Result<Layout, Error> {
    let mut looked_in = Vec::new();
    if let Some(dir) = site!(Silent::CheckoutRoot, exe => parent()) {
        let bundle = site!(Silent::CheckoutRoot, dir => parent())
            .map(|contents| contents.join("Resources").join("boot"));
        for boot in bundle.into_iter().chain([dir.join("boot")]) {
            let kernel = boot.join("kernel");
            let initramfs = boot.join("initramfs.img");
            let archive = boot.join("rootfs.img.zst");
            let checksum = boot.join("rootfs.img.sha256");
            let plain = boot.join("rootfs.img");
            looked_in.push(boot);
            if !is_file(&kernel) || !is_file(&initramfs) {
                continue;
            }
            let rootfs = if is_file(&archive) && is_file(&checksum) {
                Rootfs::Packed { archive, checksum }
            } else if is_file(&plain) {
                Rootfs::Plain(plain)
            } else {
                continue;
            };
            return Ok(Layout {
                kernel,
                initramfs,
                rootfs,
            });
        }
    }
    let checkout = site!(Silent::CheckoutRoot, exe => ancestors())
        .find(|dir| dir.file_name().is_some_and(|name| name == "target"))
        .and_then(|target| site!(Silent::CheckoutRoot, target => parent()));
    if let Some(checkout) = checkout {
        let out = checkout.join("vm-image").join("out");
        let boot = out.join("boot");
        let rootfs_dir = out.join("rootfs");
        let kernel = boot.join("kernel");
        let initramfs = boot.join("initramfs.img");
        let rootfs = rootfs_dir.join("rootfs.img");
        looked_in.extend([boot, rootfs_dir]);
        if [&kernel, &initramfs, &rootfs]
            .into_iter()
            .all(|path| is_file(path))
        {
            return Ok(Layout {
                kernel,
                initramfs,
                rootfs: Rootfs::Plain(rootfs),
            });
        }
    }
    Err(Error::NoGuestImage { looked_in })
}

fn is_file(path: &Path) -> bool {
    site!(Silent::BootMediaProbe, path => is_file())
}

impl Layout {
    /// # Errors
    /// Returns the checksum, inflation or cache-publication refusal.
    pub fn ready(self, cache: &Path) -> Result<BootArtifact, Error> {
        let rootfs = match self.rootfs {
            Rootfs::Plain(path) => path,
            Rootfs::Packed { archive, checksum } => {
                let line =
                    site!(Silent::BootMediaProbe, std::fs::read_to_string(&checksum)).map_err(
                        Error::host_file(HostFile::ImageChecksum, Act::Read, &checksum),
                    )?;
                let expected = line
                    .split_whitespace()
                    .next()
                    .filter(|hash| {
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .ok_or_else(|| {
                        Error::host_file(HostFile::ImageChecksum, Act::Read, &checksum)(
                            "expected a 64-digit SHA-256 checksum as the first token",
                        )
                    })?
                    .to_ascii_lowercase();
                publish(cache, &format!("rootfs-{expected}.img"), |stage| {
                    inflate(&archive, stage, Some(&expected)).map(|_| ())
                })?
            }
        };
        Ok(BootArtifact {
            kernel: self.kernel,
            initramfs: self.initramfs,
            rootfs,
        })
    }

    /// Preserve the shipped spelling so Windows can inflate directly into a VHD.
    pub fn found(self) -> BootArtifact {
        BootArtifact {
            kernel: self.kernel,
            initramfs: self.initramfs,
            rootfs: match self.rootfs {
                Rootfs::Plain(path) => path,
                Rootfs::Packed { archive, .. } => archive,
            },
        }
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn installed(boot: &Path) {
        std::fs::create_dir_all(boot).unwrap();
        for leaf in ["kernel", "initramfs.img", "rootfs.img"] {
            std::fs::write(boot.join(leaf), b"media").unwrap();
        }
    }

    fn packed(boot: &Path, checksum: &str) {
        installed(boot);
        // A zstd frame containing b"rootfs\n".
        let frame = b"\x28\xb5\x2f\xfd\x04\x58\x39\x00\x00rootfs\n\xf4\xd4\xd4\x5c";
        std::fs::write(boot.join("rootfs.img.zst"), frame).unwrap();
        std::fs::write(boot.join("rootfs.img.sha256"), checksum).unwrap();
    }

    #[test]
    fn invalid_checksum_tokens_are_refused_before_publication() {
        for token in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let boot = dir.path().join("boot");
            let cache = dir.path().join("cache");
            packed(&boot, &token);
            let result = locate(&dir.path().join("synod")).unwrap().ready(&cache);
            assert!(matches!(
                result,
                Err(Error::HostFile {
                    file: HostFile::ImageChecksum,
                    act: Act::Read,
                    path,
                    ..
                }) if path == boot.join("rootfs.img.sha256")
            ));
            assert!(!cache.exists());
        }
    }

    #[test]
    fn a_mismatched_checksum_publishes_no_image() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot");
        let cache = dir.path().join("cache");
        let checksum = "0".repeat(64);
        packed(&boot, &checksum);
        let result = locate(&dir.path().join("synod")).unwrap().ready(&cache);
        assert!(matches!(
            result,
            Err(Error::CorruptGuestImage { archive }) if archive == boot.join("rootfs.img.zst")
        ));
        assert!(!cache.join(format!("rootfs-{checksum}.img")).exists());
        let entries: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries, [cache.join(format!("rootfs-{checksum}.img.lock"))]);
    }

    #[test]
    fn uppercase_checksums_publish_and_ready_reuses_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot");
        let cache = dir.path().join("cache");
        let checksum = hex(&Sha256::digest(b"rootfs\n"));
        packed(
            &boot,
            &format!("{}  rootfs.img\n", checksum.to_ascii_uppercase()),
        );
        let ready = || {
            locate(&dir.path().join("synod"))
                .unwrap()
                .ready(&cache)
                .unwrap()
        };
        let first = ready();
        assert_eq!(first.rootfs, cache.join(format!("rootfs-{checksum}.img")));
        assert_eq!(std::fs::read(&first.rootfs).unwrap(), b"rootfs\n");
        std::fs::write(boot.join("rootfs.img.zst"), b"an unreadable archive").unwrap();
        assert_eq!(ready(), first);
        assert_eq!(std::fs::read(&first.rootfs).unwrap(), b"rootfs\n");
    }

    #[test]
    fn installed_media_prefers_a_complete_packed_pair() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot");
        installed(&boot);
        let exe = dir.path().join("synod.exe");
        let archive = boot.join("rootfs.img.zst");
        std::fs::write(&archive, b"archive").unwrap();
        assert!(matches!(locate(&exe).unwrap().rootfs, Rootfs::Plain(_)));
        std::fs::write(boot.join("rootfs.img.sha256"), b"checksum").unwrap();
        let layout = locate(&exe).unwrap();
        assert!(
            matches!(&layout.rootfs, Rootfs::Packed { archive: found, .. } if found == &archive)
        );
        assert_eq!(layout.found().rootfs, archive);
    }

    #[test]
    fn bundle_media_precedes_media_beside_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("Contents");
        let bundle = contents.join("Resources").join("boot");
        let beside = contents.join("MacOS").join("boot");
        installed(&bundle);
        installed(&beside);
        let layout = locate(&contents.join("MacOS").join("synod")).unwrap();
        assert_eq!(layout.kernel, bundle.join("kernel"));
    }

    #[test]
    fn a_checkout_uses_the_rootfs_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("vm-image").join("out");
        std::fs::create_dir_all(out.join("boot")).unwrap();
        std::fs::create_dir_all(out.join("rootfs")).unwrap();
        std::fs::write(out.join("boot").join("kernel"), b"kernel").unwrap();
        std::fs::write(out.join("boot").join("initramfs.img"), b"init").unwrap();
        let rootfs = out.join("rootfs").join("rootfs.img");
        std::fs::write(&rootfs, b"rootfs").unwrap();
        let exe = dir
            .path()
            .join("target")
            .join("debug")
            .join("deps")
            .join("broker");
        let layout = locate(&exe).unwrap();
        assert!(matches!(layout.rootfs, Rootfs::Plain(found) if found == rootfs));
    }

    #[test]
    fn absent_media_reports_every_directory_tried() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("target").join("debug");
        let Err(Error::NoGuestImage { looked_in }) = locate(&bin.join("synod")) else {
            panic!("absent media must name every directory tried");
        };
        assert_eq!(
            looked_in,
            [
                dir.path().join("target").join("Resources").join("boot"),
                bin.join("boot"),
                dir.path().join("vm-image").join("out").join("boot"),
                dir.path().join("vm-image").join("out").join("rootfs"),
            ]
        );
    }

    #[test]
    fn failed_preparation_leaves_no_image_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rootfs-key.img");
        let result = publish(dir.path(), "rootfs-key.img", |stage| {
            stage.write_all(b"partial").unwrap();
            Err(Error::CorruptGuestImage {
                archive: PathBuf::from("rootfs.img.zst"),
            })
        });
        assert!(result.is_err());
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the lock remains"
        );
        publish(dir.path(), "rootfs-key.img", |stage| {
            stage.write_all(b"complete").unwrap();
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"complete");
    }

    #[test]
    fn concurrent_publishers_prepare_one_complete_image() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let barrier = Barrier::new(4);
        let prepared = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    barrier.wait();
                    let path = publish(dir.path(), "rootfs-key.img", |stage| {
                        prepared.fetch_add(1, Ordering::SeqCst);
                        stage.write_all(b"complete").unwrap();
                        Ok(())
                    })
                    .unwrap();
                    assert_eq!(std::fs::read(path).unwrap(), b"complete");
                });
            }
        });
        assert_eq!(prepared.load(Ordering::SeqCst), 1);
        publish(dir.path(), "rootfs-key.img", |_| {
            panic!("published media is immutable")
        })
        .unwrap();
    }
}
