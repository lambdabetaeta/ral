use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::core::PWSTR;

use crate::error::Error;
use crate::site::{Silent, site};

pub const HYPERVISOR: &str = "Hyper-V";

pub fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// # Safety
/// `text` is null or a NUL-terminated `LocalAlloc` allocation transferred to the caller.
pub unsafe fn take_wide(text: PWSTR) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller transfers a NUL-terminated allocation, read and freed exactly once.
    let rendered = unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    // SAFETY: ownership was transferred by the caller; no references remain.
    unsafe { LocalFree(text.cast()) };
    Some(rendered)
}

pub fn prng16() -> [u8; 16] {
    let mut bytes = [0; 16];
    // SAFETY: ProcessPrng fills the given buffer and always succeeds.
    unsafe {
        windows_sys::Win32::Security::Cryptography::ProcessPrng(bytes.as_mut_ptr(), bytes.len())
    };
    bytes
}

pub fn plain(path: PathBuf) -> PathBuf {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    let prefix = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let rebuilt = |units: &[u16]| PathBuf::from(OsString::from_wide(units));
    if let Some(unc) = units.strip_prefix(prefix(r"\\?\UNC\").as_slice()) {
        return rebuilt(&[prefix(r"\\").as_slice(), unc].concat());
    }
    match units.strip_prefix(prefix(r"\\?\").as_slice()) {
        Some(rest) => rebuilt(rest),
        None => path,
    }
}

/// # Errors
/// Returns [`Error::NotUnicode`] if `path` is not Unicode.
pub fn document_text(path: &Path) -> Result<&str, Error> {
    site!(Silent::DocumentText, path => to_str())
        .ok_or_else(|| Error::NotUnicode(path.to_path_buf()))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn a_documents_paths_are_spelled_plainly() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\C:\synod\boot\kernel")),
            PathBuf::from(r"C:\synod\boot\kernel")
        );
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\dept-files\secretaries\Invoices")),
            PathBuf::from(r"\\dept-files\secretaries\Invoices")
        );
        assert_eq!(
            plain(PathBuf::from(r"C:\already\plain")),
            PathBuf::from(r"C:\already\plain")
        );
    }

    #[test]
    fn a_plain_spelling_keeps_every_unit() {
        let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        let with_surrogate = |prefix: &str| {
            let mut path = units(prefix);
            path.push(0xD800);
            PathBuf::from(OsString::from_wide(&path))
        };
        let stripped = plain(with_surrogate(r"\\?\C:\Invoices"));
        assert_eq!(stripped, with_surrogate(r"C:\Invoices"));
        match document_text(&stripped) {
            Err(Error::NotUnicode(path)) => assert_eq!(path, stripped),
            other => panic!("a name that is not Unicode is refused: {other:?}"),
        }
    }
}
