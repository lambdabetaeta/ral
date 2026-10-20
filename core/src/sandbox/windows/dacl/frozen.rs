//! A frozen allowance's real path, held open.
//!
//! The invariant: resolution meets no redirecting reparse point (a symlink,
//! a junction, any name surrogate) anywhere on the path, and the grant lands
//! on the very object checked. Components are held root first, each opened
//! relative to its parent's handle with `FILE_OPEN_REPARSE_POINT`: a name is
//! looked up once, in a directory already held, and a component swapped for a
//! link is opened as the link itself and refused, never followed. Only the
//! leaf's handle is kept: it carries the probes, the DACL and the mandatory
//! label, so what was checked is what is stamped.
//!
//! `OBJ_DONT_REPARSE` would refuse every reparse point, cloud placeholders
//! included, which only name surrogates are refused for; so a reparse point is
//! opened and its tag read on the held handle before the walk descends.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, OBJ_CASE_INSENSITIVE, OBJ_KERNEL_HANDLE, RtlNtStatusToDosError,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, SYNCHRONIZE, WRITE_DAC,
    WRITE_OWNER,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::acl::{Target, appcontainer_already_grants, attribute_tag};
use super::{DaclError, GrantKind, ensure_local_canonical_prefix};

/// `IsReparseTagNameSurrogate`: the tag redirects to another name. Symlinks
/// and mount points carry it; cloud-files placeholders do not.
const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

/// A frozen real path, its leaf held open.
pub(crate) struct FrozenPin {
    path: PathBuf,
    leaf: OwnedHandle,
}

impl FrozenPin {
    /// Check each proper ancestor of `path` and then `path`, refusing a reparse
    /// point on any of them; `None` is a path that is not on disk. `kind` picks
    /// the rights the leaf is opened with: a read-write directory also needs
    /// `WRITE_OWNER`, for its label.
    pub(crate) fn open(path: &Path, kind: GrantKind) -> Result<Option<Self>, DaclError> {
        ensure_local_canonical_prefix(path)?;
        let mut chain: Vec<&Path> = path.ancestors().collect();
        chain.reverse();
        let (_, above) = chain.split_last().expect("a path is its own ancestor");
        let mut parent: Option<OwnedHandle> = None;
        for &at in above {
            match open_in(parent.as_ref(), at, 0) {
                Ok(held) => parent = Some(checked(path, at, held)?),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(unheld(at, &e)),
            }
        }
        match open_leaf(parent.as_ref(), path, kind) {
            Ok(held) => Ok(Some(Self {
                path: path.to_path_buf(),
                leaf: checked(path, path, held)?,
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(unheld(path, &e)),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn target(&self) -> Target<'_> {
        Target::Handle(&self.leaf, &self.path)
    }
}

/// Open the volume root without delete sharing and without following it: the
/// one component with no parent handle to look it up in.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-frozen-root] Holds the volume root of a frozen allowance, the one component with no parent handle to open it from. Sandbox setup, not model data I/O."
)]
fn open_root(at: &Path, rights: u32) -> io::Result<OwnedHandle> {
    fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES | rights)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(at)
        .map(Into::into)
}

/// Open `at` inside `parent`, or as the root when there is none.
fn open_in(parent: Option<&OwnedHandle>, at: &Path, rights: u32) -> io::Result<OwnedHandle> {
    match parent {
        None => open_root(at, rights),
        Some(parent) => open_beneath(
            parent,
            at.file_name().ok_or(io::ErrorKind::InvalidInput)?,
            rights,
        ),
    }
}

/// Open `name` inside `parent`, as itself if it is a reparse point. The kernel
/// looks the name up once, in the directory held: no path is walked a second
/// time for a swapped component to redirect.
#[allow(clippy::cast_possible_truncation, reason = "a fixed-size struct")]
fn open_beneath(parent: &OwnedHandle, name: &OsStr, rights: u32) -> io::Result<OwnedHandle> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let bytes = u16::try_from(2 * wide.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: const { size_of::<OBJECT_ATTRIBUTES>() as u32 },
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &raw const name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let (mut handle, mut status) = (null_mut(), IO_STATUS_BLOCK::default());
    // SAFETY: every pointer is to a live local that outlives the call, `name`
    // borrows `wide`, and `parent` is an open handle to a directory.
    let nt = unsafe {
        NtCreateFile(
            &raw mut handle,
            SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES | rights,
            &raw const attributes,
            &raw mut status,
            null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT,
            null(),
            0,
        )
    };
    if nt < 0 {
        // SAFETY: a pure status-to-error table lookup.
        let code = unsafe { RtlNtStatusToDosError(nt) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    // SAFETY: the call succeeded, so `handle` is a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// The leaf, with the most stamping rights `kind` can use that the object
/// grants: a file that refuses `WRITE_DAC` is still held, and only a stamp
/// that needs the right fails, as `ERROR_ACCESS_DENIED` from the write.
fn open_leaf(
    parent: Option<&OwnedHandle>,
    path: &Path,
    kind: GrantKind,
) -> io::Result<OwnedHandle> {
    let ladder: &[u32] = match kind {
        GrantKind::ReadWrite => &[WRITE_DAC | WRITE_OWNER, WRITE_DAC, 0],
        GrantKind::ReadOnly | GrantKind::List => &[WRITE_DAC, 0],
    };
    let (&bare, richer) = ladder.split_last().expect("the ladder is not empty");
    richer
        .iter()
        .find_map(|&rights| match open_in(parent, path, rights) {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => None,
            held => Some(held),
        })
        .unwrap_or_else(|| open_in(parent, path, bare))
}

/// A held component that is a redirecting reparse point is a redirected
/// allowance; a non-redirecting one, such as a cloud placeholder, is not.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-frozen-link] Names the target of the link that redirected a frozen allowance, for the refusal. Sandbox setup, not model data I/O."
)]
fn checked(path: &Path, at: &Path, held: OwnedHandle) -> Result<OwnedHandle, DaclError> {
    let info = attribute_tag(&held).map_err(|e| unheld(at, &e))?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && info.ReparseTag & REPARSE_TAG_NAME_SURROGATE != 0
    {
        return Err(DaclError::Redirected {
            granted: path.to_path_buf(),
            at: at.to_path_buf(),
            target: fs::read_link(at).ok(),
        });
    }
    Ok(held)
}

fn unheld(at: &Path, e: &io::Error) -> DaclError {
    super::acl::win32_err_str(at, &format!("cannot hold it open to check it: {e}"))
}

/// Check every frozen allowance of one kind, before any filter sees it: the
/// pins are what the filters and the stamp then work from, so a path granted
/// at an earlier launch and since replaced by a junction is refused here,
/// memo or no memo. Paths that are not on disk drop out.
pub(crate) fn pin_all(paths: &[PathBuf], kind: GrantKind) -> Result<Vec<FrozenPin>, DaclError> {
    paths
        .iter()
        .filter_map(|p| FrozenPin::open(p, kind).transpose())
        .collect()
}

/// Drop the pins already covered by the well-known SIDs. Without this a
/// non-admin session stamping a system root, `System32`, which
/// `ALL APPLICATION PACKAGES` reads system-wide anyway, would fail closed on
/// `WRITE_DAC` for a grant nobody needed.
///
/// after mxc `dispatcher.rs::filter_paths_needing_grant`
pub(crate) fn needing_grant(pins: Vec<FrozenPin>, mask: u32) -> Vec<FrozenPin> {
    pins.into_iter()
        .filter(|p| !appcontainer_already_grants(p.target(), mask))
        .collect()
}

impl AsRef<Path> for FrozenPin {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests;
