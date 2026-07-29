//! Disk images Hyper-V will open: raw bytes plus one 512-byte footer.
//!
//! The guest's images are raw ext4 — a filesystem written to a file, nothing
//! more — and Virtualization.framework attaches exactly that.  Hyper-V will
//! not: a `VirtualDisk` attachment must be a VHD or VHDX, and a raw image is
//! refused at machine creation.  Wrapping the same images at install time was
//! always the plan for Windows; what this module does is the cheaper half of
//! that promise, and the honest one.
//!
//! # Why a *fixed* VHD, and why that is not a conversion
//!
//! A fixed VHD is the original format's simplest case: the disk's sectors,
//! verbatim, followed by a 512-byte footer describing them.  So "wrapping" a
//! raw image means appending 512 bytes — the bytes of the filesystem are
//! untouched and identically placed.  Nothing is transcoded, no block map is
//! built, and the same file is still a raw ext4 image with a trailing footer as
//! far as the guest's kernel is concerned (it reads the partition-less device
//! from offset zero and never looks at the tail).
//!
//! VHDX would have been the modern choice and is strictly worse here: it is a
//! log-structured container with metadata regions and a block allocation table,
//! so producing one means writing a real converter — several hundred lines that
//! can be subtly wrong — where this is a header struct and a checksum.  Nothing
//! synod does needs what VHDX adds (no snapshots, no online resize, no 2 TB+
//! disks, no shared VHD).
//!
//! # The two disks, and their different lifetimes
//!
//! - The **rootfs** is the shipped read-only image, wrapped once into synod's
//!   own cache ([`ensure_rootfs_vhd`]) and reused by every session afterwards.
//!   That costs one copy of a two-gigabyte file on first launch, which is why
//!   the marker beside it exists: a second launch finds the wrap already done.
//!   A build that ships a `.vhd` directly is passed through untouched, so this
//!   cost is a development convenience, not a permanent tax.
//! - The **session disk** ([`create_session_vhd`]) is made fresh per session and
//!   deleted with the machine.  It is a *dynamic* disk rather than a fixed one,
//!   for a reason worth knowing before changing it: Hyper-V refuses a virtual
//!   disk whose file is sparse, so growth has to be the format's business and
//!   cannot be the filesystem's.  It is never formatted here — the guest's
//!   initramfs does that on every boot, because a disk that arrives empty is a
//!   disk with nothing of a previous session on it.
//!
//! # On the filesystem calls below
//!
//! Every one of them is host-side machine plumbing performed *before* a guest
//! exists: making a session disk, wrapping the shipped rootfs into the
//! application's own cache.  There is no `Shell` to route it through and no run
//! to raise a card in — the same standing the macOS backend's
//! `create_session_image` has, and for the same reason.  Hence the
//! module-scoped allow: it describes every function here, rather than repeating
//! one reason a dozen times.
#![allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: host-side disk plumbing before any engine exists; no shell, no \
              run, no card. See the module docs."
)]

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A disk's sector size, and so the footer's own length.
const SECTOR: u64 = 512;

/// Seconds between the Unix epoch and the VHD epoch (2000-01-01T00:00:00Z),
/// which is what a footer's timestamp counts from.
const VHD_EPOCH_OFFSET: u64 = 946_684_800;

/// The size each session's read-write disk *declares*, in bytes (8 GiB).
///
/// What it costs is what the session writes into it, because the disk is dynamic
/// ([`create_session_vhd`]); what it declares is large because the guest's whole
/// writable world lives on it — the overlay upper for every rootfs write, plus
/// scratch.
pub(super) const SESSION_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// The rootfs as a path Hyper-V can attach, wrapping the shipped image into
/// `cache` if it is not already a VHD.
///
/// Three spellings of "the rootfs" reach here, and the difference between them
/// is one branch each.  A `.vhd` is already what Hyper-V wants and is attached
/// where it lies.  A plain image — what the image pipeline builds, and what a
/// checkout therefore has — is copied and given a footer.  A `.zst` archive is
/// inflated *while* being copied, in the same pass, because that is how the
/// rootfs arrives from an installation: a Windows Installer cabinet cannot hold
/// a file of two gigabytes at all (`light` refuses it outright), so the MSI
/// ships the same compressed image the macOS bundle does.  Inflating here rather
/// than earlier is what keeps that from costing a second copy — the bytes are
/// decompressed straight into the disk that was going to be written anyway.
///
/// # Errors
/// Returns a sentence if the image cannot be read or inflated, the cache cannot
/// be made, or the wrapped copy cannot be written.
pub(super) fn ensure_rootfs_vhd(image: &Path, cache: &Path) -> Result<PathBuf, String> {
    if image
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("vhd"))
    {
        return Ok(image.to_path_buf());
    }
    let compressed = image
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zst"));

    let source = std::fs::metadata(image)
        .map_err(|e| format!("the guest image {} could not be read: {e}", image.display()))?;
    let wrapped = cache.join("rootfs.vhd");
    let marker = cache.join("rootfs.vhd.wrapped");
    // What the marker records is *which* image was wrapped, not that some
    // wrapping happened: a rebuilt rootfs has a new length or a new
    // modification time, and must not be served from a stale wrap.
    let stamp = format!(
        "{}\n{}\n{}\n",
        image.display(),
        source.len(),
        source
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs())
    );
    if wrapped.is_file() && std::fs::read_to_string(&marker).is_ok_and(|seen| seen == stamp) {
        return Ok(wrapped);
    }

    std::fs::create_dir_all(cache).map_err(|e| {
        format!(
            "the guest image cache {} could not be made: {e}",
            cache.display()
        )
    })?;

    // Written to a temporary name and renamed into place, so an interrupted
    // launch never leaves a half-copied disk that a later one would attach.
    let part = cache.join("rootfs.vhd.part");
    let bytes = if compressed {
        inflate(image, &part)?
    } else {
        std::fs::copy(image, &part).map_err(|e| {
            format!(
                "the guest image could not be copied to {}: {e}",
                part.display()
            )
        })?;
        source.len()
    };
    let mut file = OpenOptions::new()
        .write(true)
        .open(&part)
        .map_err(|e| format!("{} could not be opened: {e}", part.display()))?;
    let disk = pad_to_sector(&file, bytes)?;
    append_footer(&mut file, disk)?;
    drop(file);

    std::fs::rename(&part, &wrapped).map_err(|e| {
        format!(
            "the wrapped guest image could not be moved into place at {}: {e}",
            wrapped.display()
        )
    })?;
    std::fs::write(&marker, &stamp).map_err(|e| {
        format!(
            "the guest image was wrapped but its marker {} could not be written: {e}",
            marker.display()
        )
    })?;
    Ok(wrapped)
}

/// Decompress the zstd `archive` into `out`, returning how many bytes came out.
///
/// The inflated length is what the caller needs and what nothing else can
/// supply: a zstd frame's header may carry the decompressed size but is not
/// required to, so the only honest answer is the one counted while writing.
/// That count then becomes the disk's size in the footer, which is why it is
/// returned rather than measured afterwards.
///
/// No checksum is verified here, and the reason is worth stating rather than
/// leaving as an omission.  The archive reaches this code from one of two
/// places: an installation directory under `Program Files`, which only an
/// administrator can write, or a checkout the maintainer built themselves.
/// Windows Installer already validates its own cabinet at install time, so a
/// download corrupted in transit fails the *install* rather than arriving here;
/// and against someone who can rewrite `Program Files`, a hash shipped beside
/// the file in the same directory proves nothing.
///
/// # Errors
/// Returns a sentence naming the archive if it cannot be opened, is not a zstd
/// stream, or cannot be written out.
fn inflate(archive: &Path, out: &Path) -> Result<u64, String> {
    let source = File::open(archive).map_err(|e| {
        format!(
            "the guest image {} could not be opened: {e}",
            archive.display()
        )
    })?;
    let mut decoder = zstd::stream::read::Decoder::new(std::io::BufReader::new(source))
        .map_err(|e| format!("the guest image {} is not readable: {e}", archive.display()))?;
    let file = File::create(out).map_err(|e| {
        format!(
            "the guest image could not be written to {}: {e}",
            out.display()
        )
    })?;
    let mut sink = std::io::BufWriter::new(file);
    let bytes = std::io::copy(&mut decoder, &mut sink).map_err(|e| {
        format!(
            "the guest image {} could not be unpacked: {e}",
            archive.display()
        )
    })?;
    sink.flush().map_err(|e| {
        format!(
            "the guest image {} could not be written: {e}",
            out.display()
        )
    })?;
    Ok(bytes)
}

/// Make one session's empty read-write disk in `dir`.
///
/// A **dynamic** VHD, where the rootfs is a fixed one, and the difference is not
/// a preference: Hyper-V refuses to open a virtual disk whose *file* is sparse —
/// `0xC03A001A`, "virtual hard disk files must be uncompressed and unencrypted
/// and must not be sparse" — so the obvious trick of asking NTFS for a sparse
/// eight-gigabyte file and appending a footer produces a machine that will not
/// start. The format has its own answer, which is what this writes: a dynamic
/// disk *declares* its size and allocates blocks as they are first written, so
/// an empty one is eighteen kilobytes on disk and grows only as the session
/// uses it. Growth is then Hyper-V's business rather than the filesystem's,
/// which is the whole reason it is allowed.
///
/// # Errors
/// Returns a sentence if the disk cannot be created or written.
pub(super) fn create_session_vhd(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        format!(
            "the session directory {} could not be made: {e}",
            dir.display()
        )
    })?;
    let path = dir.join(format!(
        "synod-session-{}-{}.vhd",
        std::process::id(),
        unix_seconds()
    ));
    let mut file = File::create(&path).map_err(|e| {
        format!(
            "the session disk {} could not be created: {e}",
            path.display()
        )
    })?;
    let write = |file: &mut File, bytes: &[u8]| {
        file.write_all(bytes).map_err(|e| {
            format!(
                "the session disk {} could not be written: {e}",
                path.display()
            )
        })
    };
    // A dynamic disk is read from both ends: the footer appears at offset zero
    // as well as at the end of the file, so a reader that finds the file
    // truncated can still recover the geometry from the copy at the front.
    write(&mut file, &footer(SESSION_BYTES, Kind::Dynamic))?;
    write(&mut file, &dynamic_header(SESSION_BYTES))?;
    // Every block unallocated: `0xFFFFFFFF` is the block-allocation table's
    // "not present yet", and an empty disk is nothing but a table of them.
    write(&mut file, &vec![0xFF; block_table_bytes(SESSION_BYTES)])?;
    write(&mut file, &footer(SESSION_BYTES, Kind::Dynamic))?;
    file.flush().map_err(|e| {
        format!(
            "the session disk {} could not be flushed: {e}",
            path.display()
        )
    })?;
    Ok(path)
}

/// Which of the two disk layouts a footer describes.
///
/// The rootfs is [`Kind::Fixed`] — its data *is* the file, so the footer only
/// has to say how much of it there is. The session disk is [`Kind::Dynamic`],
/// which means the file holds a header and a table instead, and the disk's
/// blocks appear in it as they are first written.
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

    /// Where the footer says the next structure is. A fixed disk has none, and
    /// says so with an all-ones offset; a dynamic disk's header follows its
    /// leading footer copy immediately.
    const fn data_offset(self) -> u64 {
        match self {
            Self::Fixed => u64::MAX,
            Self::Dynamic => SECTOR,
        }
    }
}

/// How big one of a dynamic disk's blocks is (2 MiB), and so how finely it
/// grows. The format's own conventional value, and what every tool that reads
/// one expects to find.
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

/// The 1024-byte dynamic disk header: what makes the file growable.
///
/// Its checksum follows the same rule as the footer's — the ones' complement of
/// the sum of every other byte — computed over all 1024 of them.
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
    // h[36..40] is the checksum, left zero while it is computed; everything
    // after it — the parent identity, timestamp, name, and locators — stays
    // zero, which is what "no parent" means.
    let sum = h
        .iter()
        .fold(0_u32, |acc, byte| acc.wrapping_add(u32::from(*byte)));
    h[36..40].copy_from_slice(&(!sum).to_be_bytes());
    h
}

/// Round `len` up to a whole sector by extending the file, and answer the disk
/// size the footer should describe.
///
/// An ext4 image is a whole number of blocks and so already sector-aligned;
/// this exists because a footer that describes a non-integral number of sectors
/// is malformed, and "the image was already fine" is a fact worth enforcing
/// rather than assuming.
fn pad_to_sector(file: &File, len: u64) -> Result<u64, String> {
    let disk = len.next_multiple_of(SECTOR);
    if disk != len {
        file.set_len(disk)
            .map_err(|e| format!("the wrapped guest image could not be padded: {e}"))?;
    }
    Ok(disk)
}

/// Write the fixed-disk footer describing `disk` bytes of data at the end of
/// the file.
fn append_footer(file: &mut File, disk: u64) -> Result<(), String> {
    file.seek(SeekFrom::Start(disk))
        .map_err(|e| format!("the disk image could not be positioned for its footer: {e}"))?;
    file.write_all(&footer(disk, Kind::Fixed))
        .map_err(|e| format!("the disk image's footer could not be written: {e}"))?;
    file.flush()
        .map_err(|e| format!("the disk image's footer could not be flushed: {e}"))
}

/// The 512-byte VHD footer for a fixed disk of `disk` bytes.
///
/// Every multi-byte field is big-endian — the format predates the x86
/// convention it is usually read on — and the checksum is the one's complement
/// of the sum of every other byte, computed with its own field zeroed.  Both
/// are the kind of detail a reader should be able to check against the
/// specification without running anything, so the layout is written out field
/// by field rather than through a packed struct.
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
    // Creator application and version: four bytes of identity, so a disk found
    // loose on a disk can be traced back to what made it.
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
    f[68..84].copy_from_slice(&identifier());
    // f[84] — saved state — stays 0: this disk is not a suspended machine's.
    let sum = f
        .iter()
        .fold(0_u32, |acc, byte| acc.wrapping_add(u32::from(*byte)));
    f[64..68].copy_from_slice(&(!sum).to_be_bytes());
    f
}

/// The cylinder/head/sector geometry for `sectors` total sectors.
///
/// This is the algorithm in the VHD specification's own appendix, transcribed:
/// a disk whose real geometry is irrelevant still has to *carry* one, and a
/// reader that recomputes it must get the same answer.  The largest disk the
/// scheme can describe is 65535 × 16 × 255 sectors — a little under 128 GiB —
/// which is far above anything synod attaches.
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

/// A fresh identifier for one disk image.
///
/// Unlike a machine's identifier ([`super::hvsock::fresh_machine_id`]) this one
/// addresses nothing and guards nothing — it exists because the format has the
/// field, and Hyper-V uses it only to tell two disks apart.
fn identifier() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    // SAFETY: `ProcessPrng` fills exactly the buffer it is given.
    unsafe {
        windows_sys::Win32::Security::Cryptography::ProcessPrng(bytes.as_mut_ptr(), bytes.len())
    };
    bytes
}

/// Now, in the footer's own epoch.
fn vhd_timestamp() -> u32 {
    u32::try_from(unix_seconds().saturating_sub(VHD_EPOCH_OFFSET)).unwrap_or(u32::MAX)
}

/// Now, in whole seconds since the Unix epoch.
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test scaffolding writes the images it then inspects; host-side \
              file work with no shell, no run, and no card to raise one in."
)]
mod tests {
    use super::*;

    /// The footer is the format's: the cookie a reader identifies it by, a
    /// fixed disk's all-ones data offset, the size in both size fields, and a
    /// checksum that verifies.  Verifying is the whole test — Hyper-V does
    /// exactly this arithmetic before it will open the file.
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

    /// The geometry is the specification's, checked at the sizes synod
    /// actually attaches: a small rootfs and the 8 GiB session disk.  The
    /// product of the three fields must not exceed the sector count, or a
    /// reader would compute a disk larger than the file.
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

    /// A session disk declares eight gigabytes and costs kilobytes: a leading
    /// footer copy, the dynamic header, a table of unallocated blocks, and the
    /// footer again at the end. This is the test that would have caught the
    /// sparse-file mistake — a disk Hyper-V refuses to open (`0xC03A001A`) —
    /// because it pins *where the growth comes from*.
    #[test]
    fn a_session_disk_declares_its_size_and_costs_a_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = create_session_vhd(dir.path()).expect("a session disk is made");
        let bytes = std::fs::read(&path).unwrap();

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

        // The footer is at both ends, and says "dynamic" with its header's
        // offset rather than the fixed disk's all-ones.
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

        // The dynamic header, its own checksum, and the table it points at.
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

    /// Wrapping leaves the filesystem's own bytes exactly where they were —
    /// the guest reads the device from offset zero, so anything else would be
    /// a corrupted image — and appends one footer.
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

    /// A second launch does not copy two gigabytes again: the marker records
    /// which image was wrapped, so the wrap is reused. Rebuild the image and
    /// it is redone, which is the same marker doing the other half of its job.
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

        // A rootfs rebuilt to a different length must not be served from the
        // old wrap.
        std::fs::write(&raw, vec![7u8; sector() * 3]).unwrap();
        let rewrapped = ensure_rootfs_vhd(&raw, dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&rewrapped).unwrap().len(),
            SECTOR * 3 + SECTOR
        );
    }

    /// An image that is already a VHD is handed straight through: a build that
    /// ships one pays nothing here, which is what makes the copy a development
    /// convenience rather than a permanent cost.
    #[test]
    fn an_image_that_is_already_a_vhd_is_passed_through() {
        let dir = tempfile::tempdir().unwrap();
        let vhd = dir.path().join("rootfs.vhd");
        std::fs::write(&vhd, vec![0u8; 512]).unwrap();
        assert_eq!(ensure_rootfs_vhd(&vhd, dir.path()).unwrap(), vhd);
        assert!(
            !dir.path().join("rootfs.vhd.wrapped").exists(),
            "nothing was wrapped, so nothing was marked"
        );
    }

    /// A sector, as a length: the conversion lives here once rather than at
    /// every use, and is checked rather than assumed.
    fn sector() -> usize {
        usize::try_from(SECTOR).expect("512 fits every pointer width")
    }
}
