//! Reassert a deny wherever a descendant's own DACL outranks inheritance.

use std::fs::{self, File};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, READ_CONTROL,
};

use super::acl::{INHERITED_ACE_FLAG, Security, null_deny_dacl, win32_err_str};
use super::{AceType, DENY_MASK, DaclError, PathMutexGuard, apply_explicit_ace, cancelled};
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
    let security = Security::of_file(file, path)?;
    if security.acl.is_null() {
        return Err(null_deny_dacl(path));
    }
    if security.protected().map_err(|e| walk_error(path, &e))? {
        return Ok(true);
    }
    // Every explicit boundary matters, independent of this token's SIDs.
    for ace in security.aces() {
        if ace.map_err(|e| walk_error(path, &e))?.0.AceFlags & INHERITED_ACE_FLAG == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}
