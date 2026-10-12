//! Reassert a deny wherever a descendant's own DACL outranks inheritance.

use std::fs::{self, File};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACE_HEADER, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, READ_CONTROL,
};

use super::{
    AceType, DENY_MASK, DaclError, INHERITED_ACE_FLAG, PathMutexGuard, apply_explicit_ace,
    cancelled, win32_err, win32_err_str,
};
use crate::process::cancel::CancelScope;

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-deny-walk] Inspects denied descendants and stamps capability denies where inheritance cannot enforce them. Sandbox setup, not model data I/O."
)]
pub(super) fn descendants(root: &Path, sid: &str, cancel: &CancelScope) -> Result<File, DaclError> {
    // Keep ancestors open without write/delete sharing through enumeration.
    let root_pin =
        pin(root)?.ok_or_else(|| win32_err_str(root, "deny root became a reparse point"))?;
    needs_explicit_deny(&root_pin, root)?;
    let entries = fs::read_dir(root).map_err(|e| walk_error(root, &e))?;
    let mut pending = vec![(None, root.to_path_buf(), entries)];
    while let Some((_, dir, entries)) = pending.last_mut() {
        cancelled(cancel)?;
        let Some(entry) = entries.next() else {
            pending.pop();
            continue;
        };
        let path = entry.map_err(|e| walk_error(dir, &e))?.path();
        let Some(pin) = pin(&path)? else {
            continue;
        };
        let is_dir = pin.metadata().map_err(|e| walk_error(&path, &e))?.is_dir();
        {
            let _guard = PathMutexGuard::acquire(&path)?;
            if needs_explicit_deny(&pin, &path)? {
                apply_explicit_ace(&path, sid, DENY_MASK, is_dir, AceType::Deny)?;
            }
        }
        if is_dir {
            let entries = fs::read_dir(&path).map_err(|e| walk_error(&path, &e))?;
            pending.push((Some(pin), path, entries));
        }
    }
    // The caller keeps the root pinned through its stamp and completion record.
    Ok(root_pin)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-deny-pin] Holds each denied object against replacement or reparse conversion while its DACL is inspected. Opens reparse points themselves and skips them."
)]
fn pin(path: &Path) -> Result<Option<File>, DaclError> {
    let file = fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| walk_error(path, &e))?;
    let metadata = file.metadata().map_err(|e| walk_error(path, &e))?;
    Ok((metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0).then_some(file))
}

fn walk_error(path: &Path, error: &io::Error) -> DaclError {
    win32_err_str(path, &format!("cannot inspect denied descendant: {error}"))
}

fn needs_explicit_deny(file: &File, path: &Path) -> Result<bool, DaclError> {
    let mut dacl = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    // SAFETY: the live handle has READ_CONTROL; the outputs live through the scan.
    let rc = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(path, "GetSecurityInfo", rc));
    }
    let result = (|| {
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: GetSecurityInfo returned this descriptor.
        if unsafe { GetSecurityDescriptorControl(sd, &raw mut control, &raw mut revision) } == 0 {
            return Err(walk_error(path, &io::Error::last_os_error()));
        }
        if dacl.is_null() {
            return Err(super::null_deny_dacl(path));
        }
        if control & SE_DACL_PROTECTED != 0 {
            return Ok(true);
        }
        // Conservatively cover every explicit boundary, independent of this token's SIDs.
        // SAFETY: the DACL and its ACEs belong to sd until LocalFree below.
        for index in 0..unsafe { (*dacl).AceCount } {
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(dacl, u32::from(index), &raw mut ace) } == 0 {
                return Err(walk_error(path, &io::Error::last_os_error()));
            }
            if unsafe { (*ace.cast::<ACE_HEADER>()).AceFlags } & INHERITED_ACE_FLAG == 0 {
                return Ok(true);
            }
        }
        Ok(false)
    })();
    // SAFETY: GetSecurityInfo allocated sd; no borrowed ACL pointer escapes.
    unsafe { LocalFree(sd) };
    result
}
