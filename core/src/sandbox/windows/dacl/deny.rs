//! Reassert a deny wherever a descendant's own DACL outranks inheritance.

use std::fs;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::OwnedHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, READ_CONTROL, WRITE_DAC,
};

use super::acl::{
    AclKind, INHERITED_ACE_FLAG, Security, Target, attribute_tag, null_deny_dacl, win32_err_str,
};
use super::{AceType, DENY_MASK, DaclError, PathMutexGuard, apply_explicit_ace, cancelled};
use crate::process::cancel::Scope;

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-deny-walk] Inspects denied descendants and stamps capability denies where inheritance cannot enforce them. Sandbox setup, not model data I/O."
)]
pub(super) fn descendants(
    root: &Path,
    sid: &str,
    cancel: &Scope,
) -> Result<OwnedHandle, DaclError> {
    let (root_pin, _) =
        pin(root)?.ok_or_else(|| win32_err_str(root, "deny root became a reparse point"))?;
    needs_explicit_deny(&root_pin, root)?;
    let entries = fs::read_dir(root).map_err(|e| walk_error(root, &e))?;
    let mut pending = vec![(root.to_path_buf(), entries)];
    while let Some((dir, entries)) = pending.last_mut() {
        cancelled(cancel)?;
        let Some(entry) = entries.next() else {
            pending.pop();
            continue;
        };
        let path = entry.map_err(|e| walk_error(dir, &e))?.path();
        let Some((held, is_dir)) = pin(&path)? else {
            continue;
        };
        {
            let _guard = PathMutexGuard::acquire(&path)?;
            if needs_explicit_deny(&held, &path)? {
                apply_explicit_ace(
                    Target::Handle(&held, &path),
                    sid,
                    DENY_MASK,
                    is_dir,
                    AceType::Deny,
                )?;
            }
        }
        if is_dir {
            let entries = fs::read_dir(&path).map_err(|e| walk_error(&path, &e))?;
            pending.push((path, entries));
        }
    }
    // The caller keeps the root pinned through its stamp and completion record.
    Ok(root_pin)
}

/// The object at `path`, opened as itself and with `WRITE_DAC` where it grants
/// it, so the DACL inspected is the DACL stamped; a reparse point is skipped.
/// Without the right a stamp fails loudly, as `ERROR_ACCESS_DENIED`.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-deny-pin] Opens each denied object as itself to inspect and stamp its DACL through one handle. Opens reparse points themselves and skips them."
)]
fn pin(path: &Path) -> Result<Option<(OwnedHandle, bool)>, DaclError> {
    let open = |rights| {
        fs::OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES | rights)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    };
    let file = match open(WRITE_DAC) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => open(0),
        held => held,
    }
    .map_err(|e| walk_error(path, &e))?;
    let held = OwnedHandle::from(file);
    let attributes = attribute_tag(&held)
        .map_err(|e| walk_error(path, &e))?
        .FileAttributes;
    Ok((attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0)
        .then_some((held, attributes & FILE_ATTRIBUTE_DIRECTORY != 0)))
}

fn walk_error(path: &Path, error: &io::Error) -> DaclError {
    win32_err_str(path, &format!("cannot inspect denied descendant: {error}"))
}

fn needs_explicit_deny(held: &OwnedHandle, path: &Path) -> Result<bool, DaclError> {
    let security = Security::held(held, path, AclKind::Dacl)?;
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
