//! Writing a secret to disk so only its owner can read it.
//!
//! Two platforms, one promise: the file must never exist, not even for an
//! instant, under permissions a second account could read it through.  On
//! Unix that is `0600` handed to `open` itself rather than `chmod`-ed on
//! afterwards; on Windows it is an owner-only DACL riding in on
//! `CreateFileW`'s `SECURITY_ATTRIBUTES`, protected so the parent
//! directory's inheritable entries are not merged in.
//!
//! Shared by the `ChatGPT` token store ([`super::oauth`]) and
//! [`super::keychain`]'s fallback key file.

#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:secret-write] opens a credential file 0600 for write; credential store infra, not turn-time data I/O"
)]
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

/// The Windows analogue of the Unix arm's `0600` at `open`: the owner-only
/// DACL rides in on `CreateFileW`'s `SECURITY_ATTRIBUTES`, so the file never
/// exists under the parent directory's inherited ACL, not even for an instant.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:secret-write-windows] creates a credential file with an owner-only DACL already in force and writes it; credential store infra, not turn-time data I/O"
)]
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = windows_dacl::create_owner_only(path)?;
    file.write_all(bytes)
}

#[cfg(windows)]
mod windows_dacl {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, FALSE, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        TRUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SetEntriesInAclW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The only revision Win32 defines (`winnt.h`), restated rather than
    /// pulling in another `windows-sys` feature for one ABI constant.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "a fixed Win32 layout of 24 bytes; size_of is a compile-time constant nowhere near u32::MAX"
    )]
    const SECURITY_ATTRIBUTES_LENGTH: u32 = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;

    /// Closes a process token handle exactly once, so the early returns below
    /// need no `CloseHandle` of their own.
    struct OwnedToken(HANDLE);

    impl Drop for OwnedToken {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is a handle this guard owns exclusively,
                // obtained from a successful `OpenProcessToken`.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    /// The current process's owner SID, in the layout `GetTokenInformation`
    /// hands back: a `TOKEN_USER` followed inline by its SID bytes.
    ///
    /// The buffer counts `u64`s though the API counts bytes, because the
    /// caller reads a pointer-leading `TOKEN_USER` back out of it. `Vec<u8>`
    /// is byte-aligned *as a type*; counting in `u64` makes that read sound
    /// by construction rather than by allocator luck.
    fn current_process_owner() -> std::io::Result<Vec<u64>> {
        use windows_sys::Win32::Security::GetTokenInformation;

        let mut raw_token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no
        // close; `raw_token` is an out-param this call fills on success.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw_token) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = OwnedToken(raw_token);

        let mut needed: u32 = 0;
        // SAFETY: a null buffer of zero length is the documented sizing
        // query; `needed` is filled despite the expected failure.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed);
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        // SAFETY: `buf` holds at least the `needed` bytes the sizing call
        // reported, rounded up to whole `u64`s.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// A single-ACE ACL granting the process owner full control and nobody
    /// else anything. The caller owns the allocation and frees it once
    /// `CreateFileW` has copied the descriptor into the new file object.
    fn owner_only_acl() -> std::io::Result<*mut ACL> {
        let owner_buf = current_process_owner()?;
        // SAFETY: `owner_buf` holds a populated `TOKEN_USER` in a `u64`
        // buffer, so aligned for it, and outlives every use of the `PSID`.
        let owner_sid = unsafe { (*owner_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // The trustee-by-SID form reinterprets this field as the PSID;
            // both are raw `*mut _` here, so the cast preserves bits.
            // `trustee_for` in ral-core's `sandbox::windows::dacl` does the same.
            ptstrName: owner_sid.cast::<u16>(),
        };
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee,
        };

        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `ea` outlives the call; a null prior ACL builds a fresh one
        // holding only this entry, merging in nothing inherited.
        let rc = unsafe { SetEntriesInAclW(1, &raw const ea, std::ptr::null(), &raw mut acl) };
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc.cast_signed()));
        }
        Ok(acl)
    }

    /// Create `path`, truncating as the Unix arm does, with the owner-only
    /// DACL and `SE_DACL_PROTECTED` already in force. Returns the open file
    /// for the caller to write through: there is no separate stamping step,
    /// hence no window in which the file carries an inherited ACL.
    pub(super) fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
        let acl = owner_only_acl()?;

        let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let sd_ptr: PSECURITY_DESCRIPTOR = std::ptr::addr_of_mut!(sd).cast();
        let result = (|| -> std::io::Result<std::fs::File> {
            // SAFETY: `sd` is a stack-allocated `SECURITY_DESCRIPTOR` that
            // `InitializeSecurityDescriptor` fills in place.
            if unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `sd` was just initialised; `acl` outlives this call,
            // freed only after `CreateFileW` has copied the descriptor.
            if unsafe { SetSecurityDescriptorDacl(sd_ptr, TRUE, acl, FALSE) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Protecting the DACL is what stops `CreateFileW` merging in the
            // parent directory's inheritable ACEs.
            // SAFETY: `sd` carries the DACL just attached above.
            if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                == 0
            {
                return Err(std::io::Error::last_os_error());
            }

            let sa = SECURITY_ATTRIBUTES {
                nLength: SECURITY_ATTRIBUTES_LENGTH,
                lpSecurityDescriptor: sd_ptr,
                bInheritHandle: FALSE,
            };
            let path_w: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `path_w` is NUL-terminated and alive for the call; `sa`
            // carries the descriptor just built, so the file is created with
            // that DACL atomically.
            let handle = unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    &raw const sa,
                    CREATE_ALWAYS,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `handle` is a just-created file handle opened for
            // `GENERIC_WRITE` with no other owner.
            Ok(
                unsafe {
                    std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle)
                },
            )
        })();

        // SAFETY: `acl` came from `SetEntriesInAclW`, which allocates through
        // `LocalAlloc`; freeing it exactly once on every path is safe because
        // `CreateFileW`, if reached, has already copied the descriptor.
        unsafe {
            if !acl.is_null() {
                LocalFree(acl.cast());
            }
        }
        result
    }
}
