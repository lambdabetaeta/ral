use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, GetFullPathNameW};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::PWSTR;

use crate::error::{Error, MOVE_TEMP};
use crate::site::{Silent, site};

pub(crate) const HYPERVISOR: &str = "OpenVMM";

pub(super) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// An unguessable version-4 UUID, which names the machine's pipe and logs.
pub(super) fn fresh_id() -> String {
    let mut bytes = [0; 16];
    // SAFETY: ProcessPrng fills the given buffer and always succeeds.
    unsafe {
        windows_sys::Win32::Security::Cryptography::ProcessPrng(bytes.as_mut_ptr(), bytes.len())
    };
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let id = u128::from_be_bytes(bytes);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        id >> 96,
        (id >> 80) & 0xffff,
        (id >> 64) & 0xffff,
        (id >> 48) & 0xffff,
        id & 0xffff_ffff_ffff
    )
}

/// `path` without its `\\?\` head, which is how `OpenVMM` is told a path.
///
/// # Errors
/// Returns [`Error::NoPlainSpelling`] unless Win32 reads that spelling as itself.
pub(crate) fn plain(path: &Path) -> Result<PathBuf, Error> {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    let prefix = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let plain = if let Some(unc) = units.strip_prefix(&prefix(r"\\?\UNC\")[..]) {
        [&prefix(r"\\")[..], unc].concat()
    } else if let Some(rest) = units.strip_prefix(&prefix(r"\\?\")[..]) {
        rest.to_vec()
    } else {
        units
    };
    let read_as_itself = read_by_win32(&plain).is_some_and(|read| read == plain);
    let plain = PathBuf::from(OsString::from_wide(&plain));
    if read_as_itself {
        Ok(plain)
    } else {
        Err(Error::NoPlainSpelling(plain))
    }
}

/// The absolute, normalised path Win32 reads `name` as.
fn read_by_win32(name: &[u16]) -> Option<Vec<u16>> {
    let name: Vec<u16> = name.iter().copied().chain(Some(0)).collect();
    // SAFETY: a NUL-terminated name; a zero-length probe writes nothing.
    let needed =
        unsafe { GetFullPathNameW(name.as_ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    if needed == 0 {
        return None;
    }
    let mut read = vec![0u16; needed as usize];
    // SAFETY: the same name and a writable buffer of exactly `needed` units.
    let written = unsafe {
        GetFullPathNameW(
            name.as_ptr(),
            needed,
            read.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    // Success writes fewer units than `needed`, which counts the NUL.
    (written != 0 && written < needed).then(|| {
        read.truncate(written as usize);
        read
    })
}

/// # Errors
/// Returns [`Error::NotUnicode`] if `path` is not Unicode.
pub(crate) fn document_text(path: &Path) -> Result<&str, Error> {
    site!(Silent::DocumentText, path => to_str())
        .ok_or_else(|| Error::NotUnicode(path.to_path_buf()))
}

/// Marks `file` sparse before sizing it; NTFS otherwise writes every zero at once.
pub(super) fn sparse(file: &File, len: u64) -> io::Result<()> {
    let mut returned = 0;
    // SAFETY: a live synchronous handle; FSCTL_SET_SPARSE without a buffer marks the file sparse.
    let marked = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if marked == 0 {
        let cause = io::Error::last_os_error();
        return Err(io::Error::new(
            cause.kind(),
            format!(
                "its volume cannot mark files sparse ({cause}), so Windows would write all {} GiB \
                 of it at once. {MOVE_TEMP}",
                len >> 30
            ),
        ));
    }
    file.set_len(len)
}

/// Makes `dir` closed, with all it will hold, to everyone but SYSTEM, Administrators and this
/// user from the moment it exists, so no other account can hold a handle to it.
pub(crate) fn private_directory(dir: &Path) -> io::Result<()> {
    let descriptor = Descriptor::new(&format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})",
        user_sid()?
    ))?;
    let attributes = descriptor.attributes();
    let name = wide(dir.as_os_str());
    // SAFETY: a NUL-terminated name and attributes whose descriptor outlives the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A security descriptor parsed from SDDL, freed on drop.
pub(super) struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    pub(super) fn new(sddl: &str) -> io::Result<Self> {
        let sddl = wide(sddl.as_ref());
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated string; the slot receives a LocalAlloc allocation on success.
        let parsed = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if parsed == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    /// Non-inheritable attributes carrying this descriptor, valid while it lives.
    pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("a small struct"),
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the LocalAlloc allocation this value owns, freed exactly once.
        unsafe { LocalFree(self.0) };
    }
}

/// This process's user, as an SDDL SID string.
pub(super) fn user_sid() -> io::Result<String> {
    let unread = |cause: io::Error| {
        io::Error::new(
            cause.kind(),
            format!("synod could not read its own user identity: {cause}"),
        )
    };
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle, a read-only mask, and a writable slot.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(unread(io::Error::last_os_error()));
    }
    // SAFETY: a fresh token handle, owned and closed only here.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let mut needed = 0u32;
    // SAFETY: a zero-length probe that only writes the required length.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    // u64 storage aligns the TOKEN_USER written into it.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` is aligned, writable and at least `needed` bytes.
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if needed == 0 || read == 0 {
        return Err(unread(io::Error::last_os_error()));
    }
    // SAFETY: on success the buffer holds a TOKEN_USER whose SID points into `buffer`.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: PWSTR = std::ptr::null_mut();
    // SAFETY: a valid SID and a slot that receives a LocalAlloc string.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 || text.is_null() {
        return Err(unread(io::Error::last_os_error()));
    }
    // SAFETY: a NUL-terminated LocalAlloc string, read and freed exactly once.
    let rendered = unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let rendered = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        rendered
    };
    Ok(rendered)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_documents_paths_are_spelled_plainly() {
        let plain = |path: &str| plain(Path::new(path)).expect("a plain spelling");
        assert_eq!(
            plain(r"\\?\C:\synod\boot\kernel"),
            PathBuf::from(r"C:\synod\boot\kernel")
        );
        assert_eq!(
            plain(r"\\?\UNC\dept-files\secretaries\Invoices"),
            PathBuf::from(r"\\dept-files\secretaries\Invoices")
        );
        assert_eq!(
            plain(r"C:\already\plain"),
            PathBuf::from(r"C:\already\plain")
        );
    }

    #[test]
    fn a_path_win32_would_read_as_another_is_refused() {
        for (verbatim, spelled) in [
            (r"\\?\C:\data\work.", r"C:\data\work."),
            (r"\\?\C:\data\work ", r"C:\data\work "),
            (r"\\?\C:\data\.\work", r"C:\data\.\work"),
        ] {
            match plain(Path::new(verbatim)) {
                Err(Error::NoPlainSpelling(path)) => assert_eq!(path, PathBuf::from(spelled)),
                other => panic!("{verbatim} is refused: {other:?}"),
            }
        }
    }

    #[test]
    fn a_plain_spelling_keeps_every_unit() {
        let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        let with_surrogate = |prefix: &str| {
            let mut path = units(prefix);
            path.push(0xD800);
            PathBuf::from(OsString::from_wide(&path))
        };
        let stripped = plain(&with_surrogate(r"\\?\C:\Invoices")).expect("a plain spelling");
        assert_eq!(stripped, with_surrogate(r"C:\Invoices"));
        match document_text(&stripped) {
            Err(Error::NotUnicode(path)) => assert_eq!(path, stripped),
            other => panic!("a name that is not Unicode is refused: {other:?}"),
        }
    }

    #[test]
    fn identifiers_are_fresh_version_four_uuids() {
        let (a, b) = (fresh_id(), fresh_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version 4: {a}");
        assert!(
            matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant 1: {a}"
        );
    }

    #[test]
    fn the_user_is_a_resolved_account_sid() {
        let sid = user_sid().expect("this process can read its own user");
        assert!(
            sid.starts_with("S-1-") && sid.split('-').count() >= 4,
            "{sid}"
        );
    }
}
