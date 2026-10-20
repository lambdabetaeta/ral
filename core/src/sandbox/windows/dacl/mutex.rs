// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// A port of `filesystem_dacl.rs` from Microsoft's mxc at 0e7c3dd — the
// `DaclManager` apply/restore engine — respelled for ral's error types and
// `windows-sys` bindings, then rekeyed onto path-derived capability SIDs, so
// crash recovery only reclaims profiles. Ported units keep upstream's names
// so each diffs against the one it was taken from.

use super::DaclError;
use crate::path::shown;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// FNV-1a. Enough to keep mutex names apart; not a security boundary.
fn fnv1a64(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The mutex serialising work on `path`, keyed by
/// [`windows_collision_key`](crate::path::identity::windows_collision_key):
/// folded, beyond ASCII and at least as far as NTFS folds a name, where the
/// [stamp witness](super::stamp::stamp_key) and the
/// [capability name](super::fs_capability_name) of the same object are not.
///
/// Each errs in its own safe direction.  A lock that merges two objects
/// costs only a wait, and one that splits an object lets two writers race;
/// a witness that merges two objects fails open, one standing for the
/// other's stamp.  And not every locked path is canonical: the stamp store
/// is locked by the path its sessions spell from the environment, which two
/// sessions may spell in different case or separators, and its
/// read-merge-write must serialise all the same.
fn mutex_name_for(path: &Path) -> String {
    let key = crate::path::identity::windows_collision_key(path.as_os_str());
    let h = fnv1a64(&key);
    format!("Local\\ral.sandbox.dacl.{h:016x}")
}

// A Win32 mutex must be released by the thread that acquired it.
pub(super) struct PathMutexGuard(
    OwnedHandle,
    std::marker::PhantomData<std::sync::MutexGuard<'static, ()>>,
);

/// Two ral sessions stamping the same path should serialize in seconds at
/// worst; the cap exists so a wedged peer surfaces an error instead of
/// hanging us.
const PATH_MUTEX_WAIT_MS: u32 = 30_000;

impl PathMutexGuard {
    pub(super) fn acquire(path: &Path) -> Result<Self, DaclError> {
        let name = mutex_name_for(path);
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; no security
        // attributes, not initially owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(DaclError::Win32 {
                path: path.to_path_buf(),
                reason: format!("CreateMutexW: {}", io::Error::last_os_error()),
            });
        }
        // SAFETY: CreateMutexW returned a fresh, non-null handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let wait = unsafe { WaitForSingleObject(handle.as_raw_handle(), PATH_MUTEX_WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(Self(handle, std::marker::PhantomData));
        }
        if wait == WAIT_ABANDONED {
            // The previous holder died without releasing, but we own the
            // mutex all the same; an incomplete stamp has no witness.
            crate::dbg_trace!(
                "sandbox-win-dacl",
                "acquired abandoned mutex for {}: previous holder terminated without releasing",
                shown(path)
            );
            return Ok(Self(handle, std::marker::PhantomData));
        }
        let err = if wait == WAIT_TIMEOUT {
            DaclError::MutexTimeout {
                path: path.to_path_buf(),
                timeout_ms: PATH_MUTEX_WAIT_MS,
            }
        } else if wait == WAIT_FAILED {
            DaclError::Win32 {
                path: path.to_path_buf(),
                reason: format!("WaitForSingleObject: {}", io::Error::last_os_error()),
            }
        } else {
            DaclError::Win32 {
                path: path.to_path_buf(),
                reason: format!("WaitForSingleObject: unexpected result {wait}"),
            }
        };
        Err(err)
    }
}

impl Drop for PathMutexGuard {
    fn drop(&mut self) {
        // SAFETY: construction requires ownership of this mutex; Drop then closes it.
        unsafe { ReleaseMutex(self.0.as_raw_handle()) };
    }
}

#[cfg(test)]
mod tests;
