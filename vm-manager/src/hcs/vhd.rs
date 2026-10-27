//! Fixed VHD wraps leave filesystem bytes unchanged and append a 512-byte footer.
//! Session VHDs grow through the format: Hyper-V refuses sparse host files.
//! Only the guest formats session disks; rootfs wraps are immutable cached publications.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::error::{Act, Error, HostFile};
use crate::hcs::win;
use crate::lifecycle::unix_seconds;
use crate::media::{hex, inflate, publish};
use crate::site::{Silent, site};
use sha2::{Digest, Sha256};

const SECTOR: u64 = 512;

/// Seconds between the Unix epoch and the VHD epoch (2000-01-01T00:00:00Z),
/// which is what a footer's timestamp counts from.
const VHD_EPOCH_OFFSET: u64 = 946_684_800;

/// The dynamic disk grows on writes; its declared size includes overlay writes and scratch.
pub(super) const SESSION_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// # Errors
/// Returns the image read, inflation, cache or wrapped-image write refusal.
pub(super) fn ensure_rootfs_vhd(image: &Path, cache: &Path) -> Result<PathBuf, Error> {
    if image
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("vhd"))
    {
        return Ok(image.to_path_buf());
    }
    let compressed = image
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zst"));

    let source = site!(Silent::RootfsWrapProbe, std::fs::metadata(image))
        .map_err(Error::host_file(HostFile::GuestImage, Act::Read, image))?;
    let modified =
        source
            .modified()
            .map_err(Error::host_file(HostFile::GuestImage, Act::Read, image))?;
    let mut hasher = Sha256::new();
    hasher.update(image.as_os_str().as_encoded_bytes());
    hasher.update(source.len().to_le_bytes());
    hasher.update(
        modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_secs())
            .to_le_bytes(),
    );
    hasher.update(b"fixed-vhd-1");
    let key = hex(&hasher.finalize()[..8]);
    let leaf = format!("rootfs-{key}.vhd");
    let wrapped = cache.join(&leaf);
    publish(cache, &leaf, |stage| {
        let bytes = if compressed {
            // Program Files is administrator-owned; Installer verifies the cabinet.
            inflate(image, stage, None)?
        } else {
            let mut source = site!(Silent::RootfsWrap, File::open(image))
                .map_err(Error::host_file(HostFile::GuestImage, Act::Open, image))?;
            io::copy(&mut source, stage).map_err(Error::host_file(
                HostFile::WrappedImage,
                Act::Write,
                &wrapped,
            ))?
        };
        let unwritten = |act| Error::host_file(HostFile::WrappedImage, act, &wrapped);
        let disk = pad_to_sector(stage, bytes).map_err(unwritten(Act::Size))?;
        append_footer(stage, disk).map_err(unwritten(Act::Write))
    })
}

/// Write an empty dynamic VHD; sparse host files cannot be attached by Hyper-V.
pub(super) fn write_session_vhd(file: &mut File) -> io::Result<()> {
    // Both footer copies permit recovery from a truncated dynamic disk.
    file.write_all(&footer(SESSION_BYTES, Kind::Dynamic))?;
    file.write_all(&dynamic_header(SESSION_BYTES))?;
    file.write_all(&vec![0xFF; block_table_bytes(SESSION_BYTES)])?;
    file.write_all(&footer(SESSION_BYTES, Kind::Dynamic))?;
    file.flush()
}

/// Fixed disks store sectors directly; dynamic disks allocate blocks through a header and table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fixed,
    Dynamic,
}

impl Kind {
    /// The disk type the format records: 2 for fixed, 3 for dynamic.
    const fn code(self) -> u32 {
        match self {
            Self::Fixed => 2,
            Self::Dynamic => 3,
        }
    }

    /// Fixed disks use an all-ones offset; dynamic headers follow the leading footer.
    const fn data_offset(self) -> u64 {
        match self {
            Self::Fixed => u64::MAX,
            Self::Dynamic => SECTOR,
        }
    }
}

/// The format's conventional allocation unit is 2 MiB.
const BLOCK_BYTES: u64 = 2 * 1024 * 1024;

/// Where a dynamic disk's block-allocation table starts: after the leading
/// footer copy (512) and the dynamic header (1024).
const TABLE_OFFSET: u64 = SECTOR + 1024;

/// The block-allocation table's length in bytes, rounded up to a whole sector —
/// one four-byte entry per block the disk could ever hold.
fn block_table_bytes(disk: u64) -> usize {
    let entries = disk.div_ceil(BLOCK_BYTES);
    usize::try_from((entries * 4).next_multiple_of(SECTOR)).expect("a table of a few kilobytes")
}

/// Header checksum is the ones' complement of the byte sum with the checksum field zeroed.
fn dynamic_header(disk: u64) -> [u8; 1024] {
    let mut h = [0u8; 1024];
    h[0..8].copy_from_slice(b"cxsparse");
    // No secondary header, and no parent: this is not a differencing disk.
    h[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
    h[16..24].copy_from_slice(&TABLE_OFFSET.to_be_bytes());
    h[24..28].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    h[28..32].copy_from_slice(
        &u32::try_from(disk.div_ceil(BLOCK_BYTES))
            .expect("a few thousand blocks")
            .to_be_bytes(),
    );
    h[32..36].copy_from_slice(
        &u32::try_from(BLOCK_BYTES)
            .expect("two mebibytes")
            .to_be_bytes(),
    );
    // h[36..40] stays zero until checksummed; parent fields stay zero because there is no parent.
    let sum = h
        .iter()
        .fold(0_u32, |acc, byte| acc.wrapping_add(u32::from(*byte)));
    h[36..40].copy_from_slice(&(!sum).to_be_bytes());
    h
}

/// A footer must describe a whole number of sectors, even if the source was not aligned.
fn pad_to_sector(file: &File, len: u64) -> std::io::Result<u64> {
    let disk = len.next_multiple_of(SECTOR);
    if disk != len {
        file.set_len(disk)?;
    }
    Ok(disk)
}

fn append_footer(file: &mut File, disk: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(disk))?;
    file.write_all(&footer(disk, Kind::Fixed))?;
    file.flush()
}

/// Fields are big-endian; checksum is the ones' complement of the byte sum with its own field zeroed.
fn footer(disk: u64, kind: Kind) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    // Features: reserved bit (0x2) set, as every real footer has it.
    f[8..12].copy_from_slice(&0x0000_0002_u32.to_be_bytes());
    // File format version 1.0.
    f[12..16].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    // Where the next structure is, or all-ones for a fixed disk that has none.
    f[16..24].copy_from_slice(&kind.data_offset().to_be_bytes());
    f[24..28].copy_from_slice(&vhd_timestamp().to_be_bytes());
    // The creator signature identifies a disk found outside its cache.
    f[28..32].copy_from_slice(b"synd");
    f[32..36].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    // Creator host OS: "Wi2k", the format's constant for Windows.
    f[36..40].copy_from_slice(b"Wi2k");
    f[40..48].copy_from_slice(&disk.to_be_bytes());
    f[48..56].copy_from_slice(&disk.to_be_bytes());
    let (cylinders, heads, sectors) = geometry(disk / SECTOR);
    f[56..58].copy_from_slice(&cylinders.to_be_bytes());
    f[58] = heads;
    f[59] = sectors;
    f[60..64].copy_from_slice(&kind.code().to_be_bytes());
    // f[64..68] is the checksum, left zero while it is computed.
    f[68..84].copy_from_slice(&win::prng16());
    // f[84] — saved state — stays 0: this disk is not a suspended machine's.
    let sum = f
        .iter()
        .fold(0_u32, |acc, byte| acc.wrapping_add(u32::from(*byte)));
    f[64..68].copy_from_slice(&(!sum).to_be_bytes());
    f
}

/// Geometry follows the VHD specification's appendix even though the guest ignores it.
fn geometry(sectors: u64) -> (u16, u8, u8) {
    let sectors = sectors.min(65_535 * 16 * 255);
    let (mut sectors_per_track, mut heads, mut cylinder_times_heads);
    if sectors > 65_535 * 16 * 63 {
        sectors_per_track = 255;
        heads = 16;
        cylinder_times_heads = sectors / u64::from(sectors_per_track);
    } else {
        sectors_per_track = 17;
        cylinder_times_heads = sectors / u64::from(sectors_per_track);
        heads = u8::try_from(cylinder_times_heads.div_ceil(1024).clamp(4, 255)).unwrap_or(u8::MAX);
        if cylinder_times_heads >= u64::from(heads) * 1024 || heads > 16 {
            sectors_per_track = 31;
            heads = 16;
            cylinder_times_heads = sectors / u64::from(sectors_per_track);
        }
        if cylinder_times_heads >= u64::from(heads) * 1024 {
            sectors_per_track = 63;
            heads = 16;
            cylinder_times_heads = sectors / u64::from(sectors_per_track);
        }
    }
    let cylinders = u16::try_from(cylinder_times_heads / u64::from(heads)).unwrap_or(u16::MAX);
    (cylinders, heads, sectors_per_track)
}

fn vhd_timestamp() -> u32 {
    u32::try_from(unix_seconds().saturating_sub(VHD_EPOCH_OFFSET)).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn the_footer_is_a_valid_fixed_disk_footer() {
        let disk = 64 * 1024 * 1024;
        let f = footer(disk, Kind::Fixed);
        assert_eq!(&f[0..8], b"conectix");
        assert_eq!(u64::from_be_bytes(f[16..24].try_into().unwrap()), u64::MAX);
        assert_eq!(u64::from_be_bytes(f[40..48].try_into().unwrap()), disk);
        assert_eq!(u64::from_be_bytes(f[48..56].try_into().unwrap()), disk);
        assert_eq!(u32::from_be_bytes(f[60..64].try_into().unwrap()), 2);

        let stated = u32::from_be_bytes(f[64..68].try_into().unwrap());
        let mut zeroed = f;
        zeroed[64..68].fill(0);
        let sum = zeroed
            .iter()
            .fold(0_u32, |acc, byte| acc.wrapping_add(u32::from(*byte)));
        assert_eq!(stated, !sum, "the checksum must verify");
    }

    #[test]
    fn the_geometry_describes_the_disk_it_is_given() {
        for bytes in [64u64 * 1024 * 1024, 2 * 1024 * 1024 * 1024, SESSION_BYTES] {
            let sectors = bytes / SECTOR;
            let (cylinders, heads, per_track) = geometry(sectors);
            let described = u64::from(cylinders) * u64::from(heads) * u64::from(per_track);
            assert!(
                described <= sectors,
                "{bytes} bytes: {described} > {sectors}"
            );
            assert!(
                heads > 0 && per_track > 0,
                "{bytes} bytes: degenerate geometry"
            );
        }
    }

    #[test]
    fn a_session_disk_declares_its_size_and_costs_a_table() {
        let dir = tempfile::tempdir().unwrap();
        let session = crate::lifecycle::SessionDisk::create(
            dir.path(),
            crate::lifecycle::Leaf::Vhd,
            write_session_vhd,
        )
        .expect("a session disk is made");
        let bytes = std::fs::read(session.path()).unwrap();

        let expected = sector() + 1024 + block_table_bytes(SESSION_BYTES) + sector();
        assert_eq!(
            bytes.len(),
            expected,
            "an empty dynamic disk is its metadata"
        );
        assert!(
            u64::try_from(bytes.len()).unwrap() < SESSION_BYTES,
            "the file must not cost what the disk declares"
        );

        assert_eq!(&bytes[0..8], b"conectix");
        assert_eq!(&bytes[bytes.len() - sector()..][..8], b"conectix");
        assert_eq!(u32::from_be_bytes(bytes[60..64].try_into().unwrap()), 3);
        assert_eq!(
            u64::from_be_bytes(bytes[16..24].try_into().unwrap()),
            SECTOR
        );
        assert_eq!(
            u64::from_be_bytes(bytes[40..48].try_into().unwrap()),
            SESSION_BYTES,
            "the declared size is the guest's whole disk"
        );

        let header = &bytes[sector()..sector() + 1024];
        assert_eq!(&header[0..8], b"cxsparse");
        assert_eq!(
            u64::from_be_bytes(header[16..24].try_into().unwrap()),
            TABLE_OFFSET
        );
        assert_eq!(
            u32::from_be_bytes(header[28..32].try_into().unwrap()),
            u32::try_from(SESSION_BYTES / BLOCK_BYTES).unwrap(),
            "one table entry per block of the declared disk"
        );
        let stated = u32::from_be_bytes(header[36..40].try_into().unwrap());
        let mut zeroed = header.to_vec();
        zeroed[36..40].fill(0);
        let sum = zeroed
            .iter()
            .fold(0_u32, |acc, byte| acc.wrapping_add(u32::from(*byte)));
        assert_eq!(stated, !sum, "the header's checksum must verify");

        let table = &bytes[usize::try_from(TABLE_OFFSET).unwrap()..][..4];
        assert_eq!(table, [0xFF; 4], "every block starts unallocated");
    }

    #[test]
    fn wrapping_preserves_the_image_and_appends_a_footer() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("rootfs.img");
        let content: Vec<u8> = (0..SECTOR * 4).map(|i| (i % 251) as u8).collect();
        std::fs::write(&raw, &content).unwrap();

        let wrapped = ensure_rootfs_vhd(&raw, dir.path()).expect("wraps");
        let bytes = std::fs::read(&wrapped).unwrap();
        assert_eq!(&bytes[..content.len()], &content[..], "bytes moved");
        assert_eq!(bytes.len(), content.len() + sector());
        assert_eq!(&bytes[content.len()..content.len() + 8], b"conectix");
    }

    /// Reuse an immutable wrap; a different source identity gets another name.
    #[test]
    fn a_wrap_is_reused_until_the_image_changes() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("rootfs.img");
        std::fs::write(&raw, vec![7u8; sector() * 2]).unwrap();

        let first = ensure_rootfs_vhd(&raw, dir.path()).unwrap();
        let stamp = std::fs::metadata(&first).unwrap().modified().unwrap();
        let again = ensure_rootfs_vhd(&raw, dir.path()).unwrap();
        assert_eq!(first, again);
        assert_eq!(
            std::fs::metadata(&again).unwrap().modified().unwrap(),
            stamp,
            "the wrap was redone for an unchanged image"
        );

        std::fs::write(&raw, vec![7u8; sector() * 3]).unwrap();
        let rewrapped = ensure_rootfs_vhd(&raw, dir.path()).unwrap();
        assert_ne!(first, rewrapped);
        assert_eq!(std::fs::metadata(&first).unwrap().len(), SECTOR * 3);
        assert_eq!(
            std::fs::metadata(&rewrapped).unwrap().len(),
            SECTOR * 3 + SECTOR
        );
    }

    #[test]
    fn an_image_that_is_already_a_vhd_is_passed_through() {
        let dir = tempfile::tempdir().unwrap();
        let vhd = dir.path().join("rootfs.vhd");
        std::fs::write(&vhd, vec![0u8; 512]).unwrap();
        assert_eq!(ensure_rootfs_vhd(&vhd, dir.path()).unwrap(), vhd);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn sector() -> usize {
        usize::try_from(SECTOR).expect("512 fits every pointer width")
    }
}
