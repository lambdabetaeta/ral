//! What a deny is on Windows: a proof, per launch, that the token reaches
//! nothing it names.
//!
//! Windows cannot take access away from an `AppContainer`. Its access check
//! runs a second, `AppContainer` pass that matches an allow ACE against the
//! token's package and capability SIDs, but a deny ACE against the token's
//! ordinary groups alone (Forshaw, *Windows Security Internals*, ch. 7, whose
//! reference implementation of the check tests a deny ACE's SID without the
//! capability set). A deny ACE naming a capability is therefore inert, and
//! whatever a capability's allow ACE admits stays admitted beneath it.
//!
//! So a deny is enforced by absence. `session` carves it out of the
//! allowances around it and refuses one it cannot carve, so no grant of the
//! projection reaches it; what remains is reach the projection never granted: a descendant whose own DACL admits every sandbox (`ALL
//! APPLICATION PACKAGES`), or a hard link to a file a grant's ACE was
//! propagated into. [`ensure_unreachable`] finds both on the live tree, by
//! asking the access check itself what the launch's token would be granted on
//! each object beneath the deny.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS, LUID};
use windows_sys::Win32::Security::Authorization::{
    AUTHZ_ACCESS_REPLY, AUTHZ_ACCESS_REQUEST, AUTHZ_CLIENT_CONTEXT_HANDLE,
    AUTHZ_RESOURCE_MANAGER_HANDLE, AUTHZ_RM_FLAG_NO_AUDIT, AuthzAccessCheck, AuthzFreeContext,
    AuthzFreeResourceManager, AuthzInitializeContextFromToken, AuthzInitializeResourceManager,
    AuthzSetAppContainerInformation, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSID, SID_AND_ATTRIBUTES, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::SystemServices::MAXIMUM_ALLOWED;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::super::local::Local;
use super::acl::{win32_err, win32_err_str};
use super::{DaclError, cancelled};
use crate::authority::sandbox::TOKEN;
use crate::path::{Kind, LeafMode, Located, RealPath};
use crate::process::cancel::Scope;

/// The access check a launch's token would meet, run in-process: an `AuthZ`
/// client context built from this process's token — the one `CreateProcessW`
/// derives the child's `LowBox` token from — and set to the child's package
/// SID and exactly its capability SIDs.
///
/// It differs from the child's token in one respect, and on the safe side: it
/// keeps this process's integrity level where the child runs at `Low`. The
/// mandatory check only ever removes access, so this context is granted
/// everything the child is, and possibly more; a deny it finds unreachable
/// the child cannot reach.
pub(crate) struct ContainerAccess {
    manager: AUTHZ_RESOURCE_MANAGER_HANDLE,
    client: AUTHZ_CLIENT_CONTEXT_HANDLE,
}

impl ContainerAccess {
    /// The context for a token carrying `package` and `capabilities`, the very
    /// pair `confine` hands to the spawn.
    pub(crate) fn of(
        package: PSID,
        capabilities: &[SID_AND_ATTRIBUTES],
    ) -> Result<Self, DaclError> {
        let failed = |op| DaclError::AccessCheck {
            op,
            error: io::Error::last_os_error(),
        };
        let mut manager = null_mut();
        // SAFETY: no callbacks and no name; `manager` is an out-param.
        if unsafe {
            AuthzInitializeResourceManager(
                AUTHZ_RM_FLAG_NO_AUDIT,
                None,
                None,
                None,
                null(),
                &raw mut manager,
            )
        } == 0
        {
            return Err(failed("AuthzInitializeResourceManager"));
        }
        let mut access = Self {
            manager,
            client: null_mut(),
        };
        let mut token = null_mut();
        // SAFETY: the current-process pseudo-handle needs no closing; `token`
        // is an out-param whose handle we then own.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(failed("OpenProcessToken"));
        }
        // SAFETY: the call succeeded, so `token` is a fresh handle we own.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let count = u32::try_from(capabilities.len())
            .expect("a projection declares a handful of capability SIDs, not four billion");
        // SAFETY: `token` is live with TOKEN_QUERY and `manager` was initialised
        // above; `client` is an out-param, freed by `Drop` once set.
        if unsafe {
            AuthzInitializeContextFromToken(
                0,
                token.as_raw_handle(),
                access.manager,
                null(),
                LUID::default(),
                null(),
                &raw mut access.client,
            )
        } == 0
        {
            return Err(failed("AuthzInitializeContextFromToken"));
        }
        // SAFETY: `package` and every capability SID are live for the call,
        // which copies them into the context.
        if unsafe {
            AuthzSetAppContainerInformation(access.client, package, count, capabilities.as_ptr())
        } == 0
        {
            return Err(failed("AuthzSetAppContainerInformation"));
        }
        Ok(access)
    }

    /// Every right the token would be granted on the object `held`: owner,
    /// group, DACL and mandatory label, checked as the kernel checks them.
    fn granted(&self, held: &File, path: &Path) -> Result<u32, DaclError> {
        let mut sd = null_mut();
        // SAFETY: `held` is live with READ_CONTROL; the descriptor is ours on success.
        let rc = unsafe {
            GetSecurityInfo(
                held.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | GROUP_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | LABEL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &raw mut sd,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(win32_err(path, "GetSecurityInfo", rc));
        }
        // SAFETY: the successful query transferred the descriptor allocation.
        let sd = unsafe { Local::<c_void>::from_raw(sd) };
        let request = AUTHZ_ACCESS_REQUEST {
            DesiredAccess: MAXIMUM_ALLOWED,
            ..AUTHZ_ACCESS_REQUEST::default()
        };
        let (mut granted, mut error) = (0u32, 0u32);
        let mut reply = AUTHZ_ACCESS_REPLY {
            ResultListLength: 1,
            GrantedAccessMask: &raw mut granted,
            SaclEvaluationResults: null_mut(),
            Error: &raw mut error,
        };
        // SAFETY: the context and descriptor are live, and the reply points at
        // one granted mask and one error slot, matching `ResultListLength`.
        if unsafe {
            AuthzAccessCheck(
                0,
                self.client,
                &raw const request,
                null_mut(),
                sd.as_ptr(),
                null(),
                0,
                &raw mut reply,
                null_mut(),
            )
        } == 0
        {
            return Err(DaclError::AccessCheck {
                op: "AuthzAccessCheck",
                error: io::Error::last_os_error(),
            });
        }
        match error {
            ERROR_SUCCESS => Ok(granted),
            ERROR_ACCESS_DENIED => Ok(0),
            rc => Err(win32_err(path, "AuthzAccessCheck", rc)),
        }
    }
}

impl Drop for ContainerAccess {
    fn drop(&mut self) {
        // SAFETY: each handle is null or one this value initialised and alone owns.
        unsafe {
            if !self.client.is_null() {
                AuthzFreeContext(self.client);
            }
            AuthzFreeResourceManager(self.manager);
        }
    }
}

/// Refuse unless `access` is granted nothing — not a single right — on
/// `denied` or on any object beneath it, as the tree stands now. A deny not on
/// disk has nothing to reach and passes. A reparse point is checked as itself
/// and not followed: what it leads to is another object, reached under its own
/// name and judged there.
///
/// The exclusion is held, a file or a directory, and every object beneath it
/// is held in its parent and classified by what the hold acquired, never by
/// what the listing said: a directory found where a file was listed is
/// descended.  Each is read for its descriptor through a handle of its own,
/// derived from the held one: ral must be able to reopen each for
/// `READ_CONTROL` and list each directory; where it cannot, nothing is proven
/// and the launch is refused.
pub(crate) fn ensure_unreachable(
    denied: &RealPath,
    access: &ContainerAccess,
    cancel: &Scope,
) -> Result<(), DaclError> {
    // Gone since it was listed: nothing to reach.
    let check = |held: io::Result<File>, at: &RealPath| match held {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        held => {
            let held = held.map_err(|e| walk_error(at.as_path(), &e))?;
            match access.granted(&held, at.as_path())? {
                0 => Ok(()),
                _ => Err(DaclError::Reachable {
                    denied: denied.as_path().to_path_buf(),
                    at: at.as_path().to_path_buf(),
                }),
            }
        }
    };
    let mut pending = Vec::new();
    match denied
        .open_nofollow(&TOKEN)
        .map(|held| held.map(Located::into_dir))
    {
        Ok(Some(Ok(dir))) => pending.push(dir),
        Ok(Some(Err(file))) => check(file.reopen(READ_CONTROL), file.real())?,
        Ok(None) => {}
        Err(e) => return Err(walk_error(denied.as_path(), &e)),
    }
    while let Some(dir) = pending.pop() {
        cancelled(cancel)?;
        check(dir.reopen(READ_CONTROL), dir.real())?;
        if dir.stat().reparse_tag.is_some() {
            continue;
        }
        let listing = dir
            .read_dir()
            .map_err(|e| walk_error(dir.real().as_path(), &e))?;
        for entry in listing {
            let at = entry.real();
            let kind = match entry.seen() {
                Ok(seen) => seen.stat.kind,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(walk_error(at.as_path(), e)),
            };
            if kind == Kind::Dir {
                match dir.open_dir(entry.name()) {
                    Ok(child) => pending.push(child),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(walk_error(at.as_path(), &e)),
                }
            } else {
                match dir.locate(entry.name(), LeafMode::NoFollow) {
                    Ok(held) => match held.into_dir() {
                        Ok(child) => pending.push(child),
                        Err(file) => check(file.reopen(READ_CONTROL), &at)?,
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(walk_error(at.as_path(), &e)),
                }
            }
        }
    }
    Ok(())
}

fn walk_error(path: &Path, error: &io::Error) -> DaclError {
    win32_err_str(
        path,
        &format!("cannot check what the sandbox could reach beneath an exclusion: {error}"),
    )
}
